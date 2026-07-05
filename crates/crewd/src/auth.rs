//! L0 token-file identity backend (SPEC §17.1, §17.2). Tokens are generated
//! as 256-bit random secrets and stored on disk as **SHA-256 hashes** (never
//! plaintext at rest). Revocation marks the record and rejects further use;
//! live sessions using a revoked token are rejected at the next call (the
//! session-registry side of live revocation is wired in T10b/T12).
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rand::{RngCore, rngs::OsRng};
use sha2::{Digest, Sha256};

use crewd_core::error::BusError;
use crewd_core::principal::{
    AuthBackend, AuthLevel, CellPrincipal, ClientProof, CredentialIssuer, CredentialScope,
    IssuedCredential, PeerCred,
};
use crewd_core::types::{now_rfc3339, rfc3339_after, SPEC_VERSION};

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn sha256_hex(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    hex(&h.finalize())
}

fn random_hex(nbytes: usize) -> String {
    let mut buf = vec![0u8; nbytes];
    OsRng.fill_bytes(&mut buf);
    hex(&buf)
}

/// Constant-time string compare, to avoid timing oracles on the token hash.
fn ct_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// On-disk credential record: the token **hash** (never the plaintext token),
/// the token_id, the expiry, and a revocation flag.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct TokenRecord {
    token_id: String,
    token_hash: String,
    expires_at: String,
    revoked: bool,
}

/// File-based `CredentialIssuer` (SPEC §17.1). Each cell's record lives at
/// `tokens/<cell_id>.token` (mode 0600, inside a 0700 dir).
pub struct FileIssuer {
    tokens_dir: PathBuf,
    revoked: Mutex<HashSet<String>>,
}

impl FileIssuer {
    pub fn new(tokens_dir: PathBuf) -> std::io::Result<Self> {
        fs::create_dir_all(&tokens_dir)?;
        let mut perms = fs::metadata(&tokens_dir)?.permissions();
        perms.set_mode(0o700);
        fs::set_permissions(&tokens_dir, perms)?;
        Ok(FileIssuer {
            tokens_dir,
            revoked: Mutex::new(HashSet::new()),
        })
    }

    fn record_path(&self, cell_id: &str) -> PathBuf {
        self.tokens_dir.join(format!("{cell_id}.token"))
    }

    fn read_record(&self, cell_id: &str) -> Option<TokenRecord> {
        let p = self.record_path(cell_id);
        fs::read_to_string(&p)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
    }

    fn write_record(&self, cell_id: &str, rec: &TokenRecord) -> std::io::Result<()> {
        let p = self.record_path(cell_id);
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&p)?;
        f.write_all(serde_json::to_vec(rec).expect("serializable").as_slice())?;
        // `.mode(0o600)` only applies on first creation; a pre-existing token
        // file with wider perms keeps them across truncate. Force 0600 after
        // write (SPEC §17.3 / H-09, G3-06).
        let mut perms = f.metadata()?.permissions();
        perms.set_mode(0o600);
        f.set_permissions(perms)?;
        Ok(())
    }

    /// Current `token_id` for `cell_id` (latest record on disk), if any.
    pub fn current_token_id(&self, cell_id: &str) -> Option<String> {
        self.read_record(cell_id).map(|r| r.token_id)
    }

    /// Verify a presented token against the on-disk hash, expiry and revocation
    /// flag. Returns the `token_id` on success; any failure maps to
    /// `E_AUTH_REJECTED`.
    pub fn verify(&self, cell_id: &str, token: &str) -> Result<String, BusError> {
        let rec = self
            .read_record(cell_id)
            .ok_or_else(|| BusError::AuthRejected(format!("no token record for {cell_id}")))?;
        if rec.revoked || self.revoked.lock().expect("poisoned").contains(&rec.token_id) {
            return Err(BusError::AuthRejected(format!("token revoked for {cell_id}")));
        }
        // Same-format RFC 3339 UTC 'Z' strings compare lexicographically == chronologically.
        if rec.expires_at < now_rfc3339() {
            return Err(BusError::AuthRejected(format!("token expired for {cell_id}")));
        }
        if !ct_eq(&rec.token_hash, &sha256_hex(token)) {
            return Err(BusError::AuthRejected(format!("bad token for {cell_id}")));
        }
        Ok(rec.token_id)
    }
}

impl CredentialIssuer for FileIssuer {
    fn issue(&self, cell_id: &str, scope: CredentialScope) -> Result<IssuedCredential, BusError> {
        let token = random_hex(32); // 256-bit secret
        let token_id = random_hex(8);
        let expires_at = rfc3339_after(scope.ttl_secs);
        let rec = TokenRecord {
            token_id: token_id.clone(),
            token_hash: sha256_hex(&token),
            expires_at: expires_at.clone(),
            revoked: false,
        };
        self.write_record(cell_id, &rec)
            .map_err(|e| BusError::Internal(format!("token write: {e}")))?;
        Ok(IssuedCredential {
            token_id,
            token,
            expires_at,
        })
    }

    fn rotate(&self, cell_id: &str) -> Result<IssuedCredential, BusError> {
        // `issue` overwrites the record with a fresh token_id + hash; the old
        // token stops verifying immediately.
        self.issue(cell_id, CredentialScope { ttl_secs: 86_400 })
    }

    fn revoke(&self, cell_id: &str, token_id: &str) -> Result<(), BusError> {
        if let Some(mut rec) = self.read_record(cell_id) {
            rec.revoked = true;
            self.write_record(cell_id, &rec)
                .map_err(|e| BusError::Internal(format!("token write: {e}")))?;
        }
        self.revoked.lock().expect("poisoned").insert(token_id.to_string());
        Ok(())
    }
}

/// `AuthBackend` for L0 (v0): per-cell token file + Unix peer credentials.
pub struct L0TokenBackend {
    issuer: Arc<FileIssuer>,
}

impl L0TokenBackend {
    pub fn new(issuer: Arc<FileIssuer>) -> Self {
        L0TokenBackend { issuer }
    }
}

impl AuthBackend for L0TokenBackend {
    fn authenticate(&self, peer: PeerCred, proof: ClientProof) -> Result<CellPrincipal, BusError> {
        if proof.spec_version != SPEC_VERSION {
            return Err(BusError::UnsupportedSpecVersion(format!(
                "got {}",
                proof.spec_version
            )));
        }
        let token_id = self.issuer.verify(&proof.cell_id, &proof.token)?;
        Ok(CellPrincipal {
            cell_id: proof.cell_id,
            auth_level: AuthLevel::L0Token,
            unix_uid: Some(peer.uid),
            unix_gid: Some(peer.gid),
            pid: peer.pid,
            pidfd_supported: false,
            token_id: Some(token_id),
        })
    }
}
