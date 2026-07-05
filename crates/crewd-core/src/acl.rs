//! Capability ACL (SPEC §11): TOML-serialized, capability-based, with
//! protected-cells per-target gating (D2) and broadcast fan-out that respects
//! it (G1-01). Atomic reload: a new file is fully parsed before swap, so an
//! invalid reload leaves the previous ACL authoritative.
use crate::error::BusError;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    Send,
    Ask,
    Reply,
    Broadcast,
    ReadInbox,
    ListCells,
    AttachFiles,
    Wake,
    /// SPEC §20.7: gates `cell_spawn`/`cell_send_task`/`cell_cancel`.
    /// Default-deny (like `broadcast`/`wake`).
    Spawn,
    AdminRegistry,
    ReadAudit,
}

impl Capability {
    /// SPEC §11.1 snake_case names; an unknown string aborts the whole reload.
    pub fn parse(s: &str) -> Result<Self, BusError> {
        Ok(match s {
            "send" => Self::Send,
            "ask" => Self::Ask,
            "reply" => Self::Reply,
            "broadcast" => Self::Broadcast,
            "read_inbox" => Self::ReadInbox,
            "list_cells" => Self::ListCells,
            "attach_files" => Self::AttachFiles,
            "wake" => Self::Wake,
            "spawn" => Self::Spawn,
            "admin_registry" => Self::AdminRegistry,
            "read_audit" => Self::ReadAudit,
            other => return Err(BusError::Internal(format!("unknown capability: {other}"))),
        })
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Send => "send",
            Self::Ask => "ask",
            Self::Reply => "reply",
            Self::Broadcast => "broadcast",
            Self::ReadInbox => "read_inbox",
            Self::ListCells => "list_cells",
            Self::AttachFiles => "attach_files",
            Self::Wake => "wake",
            Self::Spawn => "spawn",
            Self::AdminRegistry => "admin_registry",
            Self::ReadAudit => "read_audit",
        }
    }
}

/// Per-target reach check selector (SPEC §11.5).
pub enum ReachVia {
    Send,
    Ask,
}

#[derive(Debug)]
pub struct CellAcl {
    pub capabilities: BTreeSet<Capability>,
    pub protected: bool,
    pub send_to_protected: BTreeSet<String>,
    pub ask_protected: BTreeSet<String>,
    pub engine: String,
    pub order_preserving: bool,
}

#[derive(Debug)]
pub struct Acl {
    cells: BTreeMap<String, CellAcl>,
}

impl Acl {
    pub fn empty() -> Self {
        Acl {
            cells: BTreeMap::new(),
        }
    }

    /// Parse + validate the ACL TOML. Unknown capability, duplicate capability,
    /// or a per-target grant to an unregistered cell aborts the whole parse.
    pub fn parse(toml_text: &str) -> Result<Self, BusError> {
        #[derive(serde::Deserialize)]
        struct RawCell {
            engine: String,
            capabilities: Vec<String>,
            #[serde(default)]
            protected: bool,
            #[serde(default)]
            send_to_protected: Vec<String>,
            #[serde(default)]
            ask_protected: Vec<String>,
            #[serde(default)]
            order_preserving: bool,
        }
        #[derive(serde::Deserialize)]
        struct RawFile {
            cell: BTreeMap<String, RawCell>,
        }
        let raw: RawFile =
            toml::from_str(toml_text).map_err(|e| BusError::Internal(format!("acl parse: {e}")))?;

        let mut cells: BTreeMap<String, CellAcl> = BTreeMap::new();
        for (name, rc) in raw.cell {
            let mut caps = BTreeSet::new();
            for cs in &rc.capabilities {
                let c = Capability::parse(cs)?; // unknown -> E_INTERNAL, aborts
                if !caps.insert(c) {
                    return Err(BusError::Internal(format!(
                        "duplicate capability {cs} for cell {name}"
                    )));
                }
            }
            cells.insert(
                name,
                CellAcl {
                    capabilities: caps,
                    protected: rc.protected,
                    send_to_protected: rc.send_to_protected.into_iter().collect(),
                    ask_protected: rc.ask_protected.into_iter().collect(),
                    engine: rc.engine,
                    order_preserving: rc.order_preserving,
                },
            );
        }
        // Per-target grants must reference registered cells (SPEC §11.3).
        let registered: BTreeSet<&String> = cells.keys().collect();
        for (name, c) in &cells {
            for t in c.send_to_protected.iter().chain(c.ask_protected.iter()) {
                if !registered.contains(t) {
                    return Err(BusError::Internal(format!(
                        "cell {name} references unregistered grant target {t}"
                    )));
                }
            }
        }
        Ok(Acl { cells })
    }

    /// SPEC §11: caller must hold `cap`. Unregistered caller or missing
    /// capability -> `E_ACL_DENIED`.
    pub fn check(&self, from: &str, cap: Capability) -> Result<(), BusError> {
        match self.cells.get(from) {
            Some(c) if c.capabilities.contains(&cap) => Ok(()),
            _ => Err(BusError::AclDenied(format!(
                "{from} lacks capability {}",
                cap.as_str()
            ))),
        }
    }

    /// SPEC §11.5 protected reach gate. A non-protected `to` is always
    /// reachable; a protected `to` requires the matching per-target grant.
    pub fn check_reach(&self, from: &str, to: &str, via: ReachVia) -> Result<(), BusError> {
        let c = self
            .cells
            .get(from)
            .ok_or_else(|| BusError::AclDenied(format!("unknown cell {from}")))?;
        let protected = self.cells.get(to).map(|t| t.protected).unwrap_or(false);
        if !protected {
            return Ok(());
        }
        let granted = match via {
            ReachVia::Send => c.send_to_protected.contains(to),
            ReachVia::Ask => c.ask_protected.contains(to),
        };
        if granted {
            Ok(())
        } else {
            Err(BusError::AclDenied(format!(
                "{from} lacks per-target grant to protected cell {to}"
            )))
        }
    }

    pub fn is_registered(&self, name: &str) -> bool {
        self.cells.contains_key(name)
    }

    pub fn is_protected(&self, name: &str) -> bool {
        self.cells.get(name).map(|c| c.protected).unwrap_or(false)
    }

    /// Capability strings held by `cell`, for the daemon-derived
    /// `principal_capabilities` envelope field (SPEC §3.1, I3.7).
    pub fn capabilities_of(&self, cell: &str) -> Vec<String> {
        self.cells
            .get(cell)
            .map(|c| {
                c.capabilities
                    .iter()
                    .map(|cap| cap.as_str().to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// `(name, engine)` pairs for `cell_list` (SPEC §5.6).
    pub fn cells(&self) -> Vec<(String, String)> {
        self.cells
            .iter()
            .map(|(n, c)| (n.clone(), c.engine.clone()))
            .collect()
    }

    /// SPEC §5.7 / G1-01 broadcast fan-out: returns `(included, denied)`.
    /// The sender itself is excluded. A protected recipient is included only
    /// with a matching `send_to_protected` grant; otherwise it is denied.
    /// `broadcast` alone never reaches a protected cell.
    pub fn recipients_for_broadcast(&self, from: &str) -> (Vec<String>, Vec<String>) {
        let from_cell = match self.cells.get(from) {
            Some(c) => c,
            None => return (vec![], vec![]),
        };
        let mut included = vec![];
        let mut denied = vec![];
        for (name, c) in &self.cells {
            if name == from {
                continue; // never self-broadcast
            }
            if c.protected {
                if from_cell.send_to_protected.contains(name) {
                    included.push(name.clone());
                } else {
                    denied.push(name.clone());
                }
            } else {
                included.push(name.clone());
            }
        }
        (included, denied)
    }

    fn len(&self) -> usize {
        self.cells.len()
    }
}

/// Summary of a successful reload.
pub struct AclChangeSummary {
    pub cells: usize,
}

/// Holder with atomic swap. The plan suggests an RwLock-free swap; without an
/// external crate this uses a short `Mutex<Arc<Acl>>` (reader = lock+clone of
/// the `Arc`, swap = lock + replace). Parse happens fully before swap, so an
/// invalid reload leaves the previous ACL authoritative.
pub struct AclHolder {
    inner: Mutex<Arc<Acl>>,
}

impl AclHolder {
    pub fn new() -> Self {
        AclHolder {
            inner: Mutex::new(Arc::new(Acl::empty())),
        }
    }

    pub fn current(&self) -> Arc<Acl> {
        self.inner.lock().expect("acl mutex poisoned").clone()
    }

    pub fn reload_from_file(&self, path: &Path) -> Result<AclChangeSummary, BusError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| BusError::Internal(format!("read acl: {e}")))?;
        let new_acl = Acl::parse(&text)?; // full parse before swap
        let count = new_acl.len();
        *self.inner.lock().expect("acl mutex poisoned") = Arc::new(new_acl);
        Ok(AclChangeSummary { cells: count })
    }
}

impl Default for AclHolder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const ACL: &str = r#"
[cell.dev-senior]
engine = "claude"
capabilities = ["send","ask","reply","read_inbox","list_cells","read_audit"]
send_to_protected = ["coordinator"]
ask_protected = ["coordinator"]

[cell.codex-audit]
engine = "codex"
capabilities = ["send","ask","reply","read_inbox","list_cells","attach_files"]

[cell.coordinator]
engine = "claude"
protected = true
capabilities = ["send","ask","reply","broadcast","read_inbox","list_cells","attach_files","admin_registry","read_audit"]
"#;
    #[test]
    fn parse_and_check_capability() {
        let acl = Acl::parse(ACL).unwrap();
        assert!(acl.check("dev-senior", Capability::Send).is_ok());
        assert_eq!(
            acl.check("dev-senior", Capability::Broadcast)
                .unwrap_err()
                .code(),
            "E_ACL_DENIED"
        );
        assert_eq!(acl.check("ghost", Capability::Send).unwrap_err().code(), "E_ACL_DENIED");
    }
    #[test]
    fn unknown_capability_aborts_whole_file() {
        let bad = ACL.replace("\"send\"", "\"sendz\"");
        assert_eq!(Acl::parse(&bad).unwrap_err().code(), "E_INTERNAL");
    }
    #[test]
    fn duplicate_capability_fails() {
        let bad = ACL.replace("\"read_audit\"]", "\"read_audit\",\"send\"]");
        assert!(Acl::parse(&bad).is_err());
    }
    #[test]
    fn protected_reach_requires_per_target_grant() {
        let acl = Acl::parse(ACL).unwrap();
        assert!(acl.check_reach("dev-senior", "coordinator", ReachVia::Ask).is_ok());
        assert_eq!(
            acl.check_reach("codex-audit", "coordinator", ReachVia::Send)
                .unwrap_err()
                .code(),
            "E_ACL_DENIED"
        );
        assert!(
            acl.check_reach("codex-audit", "dev-senior", ReachVia::Send).is_ok(),
            "non-protected: nessun gate"
        );
    }
    #[test]
    fn broadcast_fanout_omits_ungranted_protected() {
        let acl = Acl::parse(ACL).unwrap();
        let (included, denied) = acl.recipients_for_broadcast("codex-audit");
        assert!(included.contains(&"dev-senior".to_string()));
        assert!(
            !included.contains(&"coordinator".to_string()),
            "G1-01: mai per sola capability broadcast"
        );
        assert_eq!(denied, vec!["coordinator".to_string()]);
        let (inc2, den2) = acl.recipients_for_broadcast("dev-senior");
        assert!(inc2.contains(&"coordinator".to_string()), "col grant è incluso");
        assert!(den2.is_empty());
    }
    #[test]
    fn grant_to_unregistered_name_fails_validation() {
        let bad = ACL.replace("[\"coordinator\"]", "[\"nonexistent\"]");
        assert!(Acl::parse(&bad).is_err());
    }
    #[test]
    fn atomic_reload_keeps_previous_on_invalid() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("acl.toml");
        std::fs::write(&p, ACL).unwrap();
        let holder = AclHolder::new();
        holder.reload_from_file(&p).unwrap();
        std::fs::write(&p, "not valid toml [[[").unwrap();
        assert!(holder.reload_from_file(&p).is_err());
        assert!(
            holder.current().check("dev-senior", Capability::Send).is_ok(),
            "ACL precedente ancora attiva"
        );
    }
}
