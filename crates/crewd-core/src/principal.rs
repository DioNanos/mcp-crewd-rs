//! `CellPrincipal`, `AuthLevel`, `PeerCred`, `ClientProof`, `AuthBackend`,
//! `CredentialIssuer` — VERBATIM from SPEC §17.1. The identity backend is
//! replaceable; ACL and audit consume only `CellPrincipal`, never raw tokens
//! or UIDs (SPEC §17.1).

use serde::{Deserialize, Serialize};

use crate::error::BusError;

/// How strongly a cell's identity is bound (SPEC §17.1). L0 = per-cell token
/// plus Unix peer credentials (v0). Higher levels bind cells to dedicated Unix
/// users, systemd units, or container identities without changing the envelope,
/// ACL model, or `cell_*` tool contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthLevel {
    L0Token,
    UnixUid,
    IsolatedService,
    Container,
}

/// The daemon-side identity of a connection (SPEC §17.1). The envelope, ACL
/// checks, and audit events consume **only** this principal abstraction, never
/// raw tokens or UIDs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellPrincipal {
    pub cell_id: String,
    pub auth_level: AuthLevel,
    pub unix_uid: Option<u32>,
    pub unix_gid: Option<u32>,
    pub pid: Option<u32>,
    pub pidfd_supported: bool,
    pub token_id: Option<String>,
}

/// Unix peer credentials obtained from the socket (`SO_PEERCRED`,
/// `SO_PEERPIDFD` when available).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCred {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<u32>,
}

/// What a client presents at handshake to prove a cell identity. The payload
/// MUST NOT carry an authoritative `from_cell`; identity comes exclusively from
/// `AuthBackend::authenticate` over the peer credential and this proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientProof {
    pub cell_id: String,
    pub token: String,
    pub spec_version: String,
}

/// Scope of a credential issuance (SPEC §17.1: `CredentialScope { ttl_secs }`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CredentialScope {
    pub ttl_secs: u64,
}

/// A freshly issued/rotated credential (SPEC §17.1). `token` is the secret; it
/// is never stored in plaintext at rest by the bus (hash only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IssuedCredential {
    pub token_id: String,
    pub token: String,
    pub expires_at: String,
}

/// Authenticate a connection into a `CellPrincipal` from peer credentials and
/// a client proof. Upgrading the deployment (L0→L1/L2/L3) changes this
/// implementation and the resulting `AuthLevel`, never the `cell_*` protocol.
pub trait AuthBackend: Send + Sync {
    fn authenticate(
        &self,
        peer: PeerCred,
        proof: ClientProof,
    ) -> Result<CellPrincipal, BusError>;
}

/// Issue, rotate, and revoke per-cell credentials (SPEC §17.1). Part of the
/// identity contract even in v0: tokens are not permanent and not hand-waved
/// into existence. Rotation and revocation MUST be audited when they occur;
/// revocation MUST invalidate live sessions using that token (SPEC §17.2).
pub trait CredentialIssuer: Send + Sync {
    fn issue(
        &self,
        cell_id: &str,
        scope: CredentialScope,
    ) -> Result<IssuedCredential, BusError>;
    fn rotate(&self, cell_id: &str) -> Result<IssuedCredential, BusError>;
    fn revoke(&self, cell_id: &str, token_id: &str) -> Result<(), BusError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_level_serde_roundtrip() {
        for (lvl, expect) in [
            (AuthLevel::L0Token, "L0Token"),
            (AuthLevel::UnixUid, "UnixUid"),
            (AuthLevel::IsolatedService, "IsolatedService"),
            (AuthLevel::Container, "Container"),
        ] {
            assert_eq!(serde_json::to_string(&lvl).unwrap(), format!("\"{expect}\""));
        }
    }

    #[test]
    fn cell_principal_roundtrip() {
        let p = CellPrincipal {
            cell_id: "dev-senior".into(),
            auth_level: AuthLevel::L0Token,
            unix_uid: Some(1000),
            unix_gid: Some(1000),
            pid: Some(42),
            pidfd_supported: true,
            token_id: Some("tok-1".into()),
        };
        let s = serde_json::to_string(&p).unwrap();
        let back: CellPrincipal = serde_json::from_str(&s).unwrap();
        assert_eq!(back.cell_id, "dev-senior");
        assert!(back.pidfd_supported);
        assert_eq!(back.token_id.as_deref(), Some("tok-1"));
    }

    struct DummyBackend;
    impl AuthBackend for DummyBackend {
        fn authenticate(
            &self,
            _peer: PeerCred,
            _proof: ClientProof,
        ) -> Result<CellPrincipal, crate::error::BusError> {
            Ok(CellPrincipal {
                cell_id: "x".into(),
                auth_level: AuthLevel::L0Token,
                unix_uid: None,
                unix_gid: None,
                pid: None,
                pidfd_supported: false,
                token_id: None,
            })
        }
    }
    impl CredentialIssuer for DummyBackend {
        fn issue(
            &self,
            _cell_id: &str,
            _scope: CredentialScope,
        ) -> Result<IssuedCredential, crate::error::BusError> {
            Ok(IssuedCredential {
                token_id: "t".into(),
                token: "secret".into(),
                expires_at: "2999-01-01T00:00:00Z".into(),
            })
        }
        fn rotate(&self, _cell_id: &str) -> Result<IssuedCredential, crate::error::BusError> {
            Ok(IssuedCredential {
                token_id: "t2".into(),
                token: "secret2".into(),
                expires_at: "2999-01-01T00:00:00Z".into(),
            })
        }
        fn revoke(&self, _cell_id: &str, _token_id: &str) -> Result<(), crate::error::BusError> {
            Ok(())
        }
    }

    #[test]
    fn auth_traits_are_object_safe() {
        // Traits must be usable as `dyn` (daemon plugs different backends).
        let _: Box<dyn AuthBackend> = Box::new(DummyBackend);
        let _: Box<dyn CredentialIssuer> = Box::new(DummyBackend);
    }
}
