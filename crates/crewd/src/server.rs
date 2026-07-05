//! Daemon bootstrap (§17.3 hardening) + per-connection handshake producing a
//! `CellPrincipal`, followed by the tool dispatch loop. Live-revocation
//! registry for sessions.
use std::collections::HashSet;
use std::fs;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use crewd_core::error::BusError;
use crewd_core::principal::{AuthBackend, ClientProof, PeerCred};
use crewd_core::wire::{WireRequest, WireResponse};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::oneshot;

use crate::config::CrewdConfig;
use crate::handlers::{dispatch, DaemonState};

/// Maximum bytes for a handshake frame (small, bounded).
pub const MAX_HANDSHAKE_BYTES: usize = 8 * 1024;
/// Maximum bytes for a tool frame (body ≤ 64 KiB + envelope overhead).
pub const MAX_FRAME_BYTES: usize = 192 * 1024;

#[derive(Default)]
pub struct SessionRegistry {
    revoked: Mutex<HashSet<String>>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn revoke(&self, token_id: &str) {
        self.revoked
            .lock()
            .expect("poisoned")
            .insert(token_id.to_string());
    }
    pub fn is_revoked(&self, token_id: &str) -> bool {
        self.revoked.lock().expect("poisoned").contains(token_id)
    }
}

/// §17.3 hardening bootstrap: runtime dir 0700; refuse to bind if the socket
/// path is a symlink; unlink a stale socket file; bind; chmod the socket 0600.
pub fn bootstrap_hardening(cfg: &CrewdConfig) -> io::Result<UnixListener> {
    fs::create_dir_all(&cfg.runtime_dir)?;
    let mut dir_perms = fs::metadata(&cfg.runtime_dir)?.permissions();
    dir_perms.set_mode(0o700);
    fs::set_permissions(&cfg.runtime_dir, dir_perms)?;

    let sock = cfg.socket_path();
    if let Ok(md) = fs::symlink_metadata(&sock) {
        use std::os::unix::fs::FileTypeExt;
        let ft = md.file_type();
        if ft.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refuse to bind: socket path is a symlink",
            ));
        }
        // Only a genuine (stale) Unix socket may be unlinked and rebound. Any
        // other file type at the socket path — regular file, dir, fifo, device
        // — is foreign and MUST be refused, not silently removed (SPEC §17.3,
        // G3-07).
        if !ft.is_socket() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "refuse to bind: non-socket file at socket path",
            ));
        }
        fs::remove_file(&sock)?;
    }
    let listener = UnixListener::bind(&sock)?;
    let mut sp = fs::metadata(&sock)?.permissions();
    sp.set_mode(0o600);
    fs::set_permissions(&sock, sp)?;
    Ok(listener)
}

/// Accept loop. Each connection is authenticated, then runs the tool dispatch
/// loop until the peer disconnects or the session is revoked.
pub async fn serve(
    listener: UnixListener,
    backend: Arc<dyn AuthBackend>,
    state: Arc<DaemonState>,
    mut shutdown: oneshot::Receiver<()>,
) -> io::Result<()> {
    loop {
        tokio::select! {
            biased;
            _ = &mut shutdown => break,
            res = listener.accept() => {
                let (stream, _addr) = res?;
                let b = backend.clone();
                let s = state.clone();
                tokio::spawn(async move {
                    let _ = handle_connection(stream, b, s).await;
                });
            }
        }
    }
    Ok(())
}

async fn handle_connection(
    mut stream: UnixStream,
    backend: Arc<dyn AuthBackend>,
    state: Arc<DaemonState>,
) {
    // 1. SO_PEERCRED (SPEC §17.3 MUST). Failure → E_AUTH_REJECTED, audited.
    let peer = match peercred(&stream) {
        Ok(p) => p,
        Err(_) => {
            state.audit_best_effort(
                "auth_rejected",
                None,
                None,
                None,
                "rejected",
                Some(serde_json::json!({"reason": "peercred_unavailable"})),
            );
            let _ = send_err(
                &mut stream,
                0,
                BusError::AuthRejected("peercred unavailable".into()),
            )
            .await;
            return;
        }
    };
    let peer_detail = serde_json::json!({"uid": peer.uid, "gid": peer.gid, "pid": peer.pid});
    // 2. Bounded handshake frame. Il FrameReader vive per l'intera connessione
    // così i byte pipelined oltre il primo `\n` non vengono persi.
    let mut frames = FrameReader::new();
    let raw = match frames.read_bounded_line(&mut stream, MAX_HANDSHAKE_BYTES).await {
        Ok(b) => b,
        Err(_) => return,
    };
    let req: WireRequest = match serde_json::from_slice(&raw) {
        Ok(r) => r,
        Err(_) => {
            let _ = send_err(&mut stream, 0, BusError::Internal("bad handshake frame".into())).await;
            return;
        }
    };
    let proof: ClientProof = match serde_json::from_value(req.params.clone()) {
        Ok(p) => p,
        Err(_) => {
            let _ = send_err(
                &mut stream,
                req.id,
                BusError::Internal("bad handshake params".into()),
            )
            .await;
            return;
        }
    };
    // Retain the claimed identity for the audit trail before `proof` is moved.
    let claimed_cell = proof.cell_id.clone();
    let principal = match backend.authenticate(peer, proof) {
        Ok(p) => p,
        Err(e) => {
            // Audit every rejected handshake — bad/unknown token, expired,
            // revoked, or spec-version mismatch (SPEC §13 / §19.1, G3-02).
            let kind = if e.code() == "E_AUTH_REJECTED" {
                "auth_rejected"
            } else {
                "spec_version_rejected"
            };
            state.audit_best_effort(
                kind,
                None,
                Some(&claimed_cell),
                None,
                "rejected",
                Some(peer_detail.clone()),
            );
            let _ = send_err(&mut stream, req.id, e).await;
            return;
        }
    };
    if let Some(tid) = principal.token_id.as_ref() {
        if state.registry.is_revoked(tid) {
            state.audit_best_effort(
                "auth_rejected",
                None,
                Some(&principal.cell_id),
                None,
                "rejected",
                Some(serde_json::json!({"reason": "revoked_at_handshake", "token_id": tid})),
            );
            let _ = send_err(&mut stream, req.id, BusError::AuthRejected("revoked".into())).await;
            return;
        }
    }
    let _ = send_ok(
        &mut stream,
        req.id,
        serde_json::json!({"cell": principal.cell_id, "auth_level": "L0Token"}),
    )
    .await;

    // 3. Tool dispatch loop.
    loop {
        let frame = match frames.read_bounded_line(&mut stream, MAX_FRAME_BYTES).await {
            Ok(b) => b,
            Err(_) => return,
        };
        if frame.is_empty() {
            return; // clean EOF
        }
        let req: WireRequest = match serde_json::from_slice(&frame) {
            Ok(r) => r,
            Err(_) => {
                let _ = send_err(
                    &mut stream,
                    0,
                    BusError::Internal("bad request frame".into()),
                )
                .await;
                continue;
            }
        };
        // Live-revocation check on every frame (SPEC §17.2 MUST): a revoked
        // token closes the live session and every subsequent call returns
        // E_AUTH_REJECTED + an audited `auth_rejected` event (G3-02).
        if let Some(tid) = principal.token_id.as_ref() {
            if state.registry.is_revoked(tid) {
                state.audit_best_effort(
                    "auth_rejected",
                    None,
                    Some(&principal.cell_id),
                    None,
                    "rejected",
                    Some(serde_json::json!({"reason": "revoked_live_session", "token_id": tid})),
                );
                let _ =
                    send_err(&mut stream, req.id, BusError::AuthRejected("revoked".into())).await;
                return;
            }
        }
        let resp = dispatch(state.clone(), &principal, req).await;
        if send_response(&mut stream, &resp).await.is_err() {
            return;
        }
    }
}

fn peercred(stream: &UnixStream) -> io::Result<PeerCred> {
    use rustix::fd::BorrowedFd;
    use rustix::net::sockopt::get_socket_peercred;
    use std::os::unix::io::AsRawFd;
    let fd = unsafe { BorrowedFd::borrow_raw(stream.as_raw_fd()) };
    let cred = get_socket_peercred(fd)?;
    let pid_raw = rustix::process::Pid::as_raw(Some(cred.pid));
    Ok(PeerCred {
        uid: cred.uid.as_raw(),
        gid: cred.gid.as_raw(),
        pid: if pid_raw > 0 {
            Some(pid_raw as u32)
        } else {
            None
        },
    })
}

/// Per-connection NDJSON frame reader with carry-over. The pre-fix free
/// function discarded the bytes AFTER the `\n` within the same `read()`, so
/// two coalesced/pipelined frames in one chunk lost the second one (client
/// hung waiting for a reply — 2026-07-05 pre-publish audit finding). The
/// remainder now stays in `carry` and feeds the next call.
struct FrameReader {
    carry: Vec<u8>,
}

impl FrameReader {
    fn new() -> Self {
        Self { carry: Vec::new() }
    }

    async fn read_bounded_line(
        &mut self,
        stream: &mut UnixStream,
        max: usize,
    ) -> io::Result<Vec<u8>> {
        loop {
            if let Some(pos) = self.carry.iter().position(|&b| b == b'\n') {
                let mut line: Vec<u8> = self.carry.drain(..=pos).collect();
                line.pop(); // drop '\n'
                return Ok(line);
            }
            if self.carry.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
            }
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                // EOF senza newline: restituisce il residuo (semantica pre-fix).
                return Ok(std::mem::take(&mut self.carry));
            }
            self.carry.extend_from_slice(&chunk[..n]);
        }
    }
}

async fn send_response(stream: &mut UnixStream, resp: &WireResponse) -> io::Result<()> {
    let mut bytes =
        serde_json::to_vec(resp).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await
}

#[allow(dead_code)]
async fn send_ok(stream: &mut UnixStream, id: u64, result: serde_json::Value) -> io::Result<()> {
    send_response(stream, &WireResponse::ok(id, result)).await
}

#[allow(dead_code)]
async fn send_err(stream: &mut UnixStream, id: u64, e: BusError) -> io::Result<()> {
    send_response(stream, &WireResponse::err(id, &e)).await
}
