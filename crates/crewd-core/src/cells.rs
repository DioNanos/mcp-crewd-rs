//! Cell registry (SPEC §20.1–20.2): `EngineKind`, `CellDef`, `SpawnTarget`,
//! the `cells` table backed by `Store`, and the named-cell immutability rule
//! (`resolve_spawn_target`). Named cells are immutable at launch: any
//! engine/model/profile/cwd override on a spawn targeting a named cell is
//! rejected with `E_POLICY_DENIED` (SPEC §20.1).
use crate::audit::{AuditChain, AuditEventDraft};
use crate::error::BusError;
use crate::store::Store;
use crate::validators::validate_cell_name;
use rusqlite::params;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    Codex,
    Claude,
    Pi,
    Fake,
}

impl EngineKind {
    /// Stable TEXT form stored in the `cells.engine` column; matches the serde
    /// `snake_case` form so the DB value round-trips through serde.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::Pi => "pi",
            Self::Fake => "fake",
        }
    }

    /// Inverse of `as_str`; `None` on an unknown discriminator.
    pub fn from_db_str(s: &str) -> Option<Self> {
        match s {
            "codex" => Some(Self::Codex),
            "claude" => Some(Self::Claude),
            "pi" => Some(Self::Pi),
            "fake" => Some(Self::Fake),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CellDef {
    /// `[a-z0-9_-]{1,64}`, primary key of the `cells` table.
    pub name: String,
    pub engine: EngineKind,
    pub model: Option<String>,
    /// engine-claude: `"max"` (host credentials) or a profile name declared
    /// in `crewd.toml [profile.<name>]`.
    pub profile: Option<String>,
    pub cwd: String,
    pub worktree_default: bool,
    /// Informativo in v0 (e.g. `"dev-agent"`); the worker mounts its own
    /// mcp-memory-rs namespace (SPEC §20.1).
    pub memory_device: Option<String>,
    pub created_at: String,
}

/// Outcome of resolving a spawn target (SPEC §20.1).
pub enum SpawnTarget {
    /// A registered named cell, launched as-defined (no overrides).
    Named(CellDef),
    /// An inline ephemeral cell; `cell_name` is generated `~ephemeral-<uuid8>`.
    Ephemeral {
        engine: EngineKind,
        model: Option<String>,
        profile: Option<String>,
        cwd: String,
    },
}

impl Store {
    /// Insert a new named cell. SPEC §20.10: `cell_registered` is audited
    /// (append + fsync) **before** the SQLite mutation, so an audit failure
    /// leaves no row. `E_POLICY_DENIED` on an invalid name.
    pub fn cell_register(&self, def: &CellDef, audit: &mut AuditChain) -> Result<(), BusError> {
        validate_cell_name(&def.name)?;
        let present = self
            .0
            .query_row(
                "SELECT 1 FROM cells WHERE name = ?1",
                params![&def.name],
                |_| Ok(()),
            )
            .is_ok();
        if present {
            return Err(BusError::Internal(format!(
                "cell already exists: {}",
                def.name
            )));
        }
        let detail = serde_json::json!({
            "engine": def.engine.as_str(),
            "cwd": def.cwd,
            "worktree_default": def.worktree_default,
        });
        audit
            .append(AuditEventDraft::new(
                "cell_registered",
                None,
                Some(&def.name),
                None,
                "ok",
                Some(detail),
            ))
            .map_err(|e| BusError::Internal(format!("audit cell_registered: {e}")))?;
        self.0
            .execute(
                "INSERT INTO cells (name, engine, model, profile, cwd, worktree_default,\
                 memory_device, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    &def.name,
                    def.engine.as_str(),
                    &def.model,
                    &def.profile,
                    &def.cwd,
                    def.worktree_default as i64,
                    &def.memory_device,
                    &def.created_at,
                ],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Update an existing named cell. SPEC §20.10: `cell_updated` is audited
    /// before the SQLite mutation.
    pub fn cell_update(&self, def: &CellDef, audit: &mut AuditChain) -> Result<(), BusError> {
        validate_cell_name(&def.name)?;
        let present = self
            .0
            .query_row(
                "SELECT 1 FROM cells WHERE name = ?1",
                params![&def.name],
                |_| Ok(()),
            )
            .is_ok();
        if !present {
            return Err(BusError::UnknownCell(format!(
                "cell not found: {}",
                def.name
            )));
        }
        let detail = serde_json::json!({
            "engine": def.engine.as_str(),
            "cwd": def.cwd,
            "worktree_default": def.worktree_default,
        });
        audit
            .append(AuditEventDraft::new(
                "cell_updated",
                None,
                Some(&def.name),
                None,
                "ok",
                Some(detail),
            ))
            .map_err(|e| BusError::Internal(format!("audit cell_updated: {e}")))?;
        self.0
            .execute(
                "UPDATE cells SET engine = ?2, model = ?3, profile = ?4, cwd = ?5,\
                 worktree_default = ?6, memory_device = ?7, created_at = ?8 WHERE name = ?1",
                params![
                    &def.name,
                    def.engine.as_str(),
                    &def.model,
                    &def.profile,
                    &def.cwd,
                    def.worktree_default as i64,
                    &def.memory_device,
                    &def.created_at,
                ],
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        Ok(())
    }

    /// Fetch a named cell by name.
    pub fn cell_get(&self, name: &str) -> Result<Option<CellDef>, BusError> {
        let res = self.0.query_row(
            "SELECT name, engine, model, profile, cwd, worktree_default, memory_device,\
             created_at FROM cells WHERE name = ?1",
            params![name],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            },
        );
        let (name, engine_str, model, profile, cwd, wd, memory_device, created_at) = match res {
            Ok(t) => t,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(BusError::Internal(e.to_string())),
        };
        let engine = EngineKind::from_db_str(&engine_str)
            .ok_or_else(|| BusError::Internal(format!("unknown engine kind: {engine_str}")))?;
        Ok(Some(CellDef {
            name,
            engine,
            model,
            profile,
            cwd,
            worktree_default: wd != 0,
            memory_device,
            created_at,
        }))
    }

    /// List all registered cells ordered by name.
    pub fn cell_list_defs(&self) -> Result<Vec<CellDef>, BusError> {
        let mut stmt = self
            .0
            .prepare(
                "SELECT name, engine, model, profile, cwd, worktree_default, memory_device,\
                 created_at FROM cells ORDER BY name",
            )
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, String>(7)?,
                ))
            })
            .map_err(|e| BusError::Internal(e.to_string()))?;
        let mut out = Vec::new();
        for r in rows {
            let (name, engine_str, model, profile, cwd, wd, memory_device, created_at) =
                r.map_err(|e| BusError::Internal(e.to_string()))?;
            let engine = EngineKind::from_db_str(&engine_str)
                .ok_or_else(|| BusError::Internal(format!("unknown engine kind: {engine_str}")))?;
            out.push(CellDef {
                name,
                engine,
                model,
                profile,
                cwd,
                worktree_default: wd != 0,
                memory_device,
                created_at,
            });
        }
        Ok(out)
    }
}

/// SPEC §20.1 immutability rule. A spawn targeting a **named** cell resolves
/// to `SpawnTarget::Named` only when NO override is requested; any `req_*` set
/// on a named cell is `E_POLICY_DENIED`. An **ephemeral** spawn requires
/// `req_engine` (else `E_POLICY_DENIED`).
pub fn resolve_spawn_target(
    named: Option<CellDef>,
    req_engine: Option<EngineKind>,
    req_model: Option<String>,
    req_profile: Option<String>,
    req_cwd: Option<String>,
) -> Result<SpawnTarget, BusError> {
    match named {
        Some(def) => {
            if req_engine.is_some()
                || req_model.is_some()
                || req_profile.is_some()
                || req_cwd.is_some()
            {
                return Err(BusError::PolicyDenied(format!(
                    "named cell '{}' is immutable at launch; \
                     engine/model/profile/cwd overrides are forbidden",
                    def.name
                )));
            }
            Ok(SpawnTarget::Named(def))
        }
        None => {
            let engine = req_engine.ok_or_else(|| {
                BusError::PolicyDenied("ephemeral spawn requires an engine".into())
            })?;
            Ok(SpawnTarget::Ephemeral {
                engine,
                model: req_model,
                profile: req_profile,
                // No cwd requested: the daemon inherits its own. "." denotes
                // "current dir" and is resolved by the spawn path (§20.4).
                cwd: req_cwd.unwrap_or_else(|| ".".into()),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditChain;
    use crate::types::now_rfc3339;

    fn sample_def(name: &str) -> CellDef {
        CellDef {
            name: name.into(),
            engine: EngineKind::Claude,
            model: Some("example-model".into()),
            profile: Some("example-profile".into()),
            cwd: "/tmp/w".into(),
            worktree_default: true,
            memory_device: Some("dev-agent".into()),
            created_at: now_rfc3339(),
        }
    }

    /// A fresh audit chain on a temp file (real fsync path).
    fn fresh_audit() -> (tempfile::TempDir, AuditChain) {
        let dir = tempfile::tempdir().unwrap();
        let a = AuditChain::open(&dir.path().join("a.jsonl")).unwrap();
        (dir, a)
    }

    #[test]
    fn registry_roundtrip_and_immutability() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        let def = sample_def("worker-a");
        s.cell_register(&def, &mut audit).unwrap();
        assert!(s.cell_register(&def, &mut audit).is_err()); // duplicate register = error
        let got = s.cell_get("worker-a").unwrap().unwrap();
        assert_eq!(got.model.as_deref(), Some("example-model"));
        // named + override → E_POLICY_DENIED
        let r = resolve_spawn_target(Some(got), None, Some("other-model".into()), None, None);
        assert!(matches!(r, Err(e) if e.code() == "E_POLICY_DENIED"));
        // ephemeral: engine mandatory
        let e = resolve_spawn_target(
            None,
            Some(EngineKind::Codex),
            None,
            None,
            Some("/tmp/x".into()),
        )
        .unwrap();
        assert!(matches!(
            e,
            SpawnTarget::Ephemeral {
                engine: EngineKind::Codex,
                ..
            }
        ));
    }

    #[test]
    fn cell_update_unknown_rejected() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        let err = s.cell_update(&sample_def("ghost"), &mut audit).unwrap_err();
        assert_eq!(err.code(), "E_UNKNOWN_CELL");
    }

    #[test]
    fn cell_update_existing_succeeds() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        let mut def = sample_def("c1");
        s.cell_register(&def, &mut audit).unwrap();
        def.model = Some("example-model-next".into());
        s.cell_update(&def, &mut audit).unwrap();
        let got = s.cell_get("c1").unwrap().unwrap();
        assert_eq!(got.model.as_deref(), Some("example-model-next"));
    }

    #[test]
    fn cell_list_defs_orders_by_name() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        s.cell_register(&sample_def("zeta"), &mut audit).unwrap();
        s.cell_register(&sample_def("alpha"), &mut audit).unwrap();
        s.cell_register(&sample_def("mid"), &mut audit).unwrap();
        let names: Vec<_> = s
            .cell_list_defs()
            .unwrap()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn named_cell_without_overrides_resolves_named() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        let def = sample_def("named-1");
        s.cell_register(&def, &mut audit).unwrap();
        let got = s.cell_get("named-1").unwrap().unwrap();
        let r = resolve_spawn_target(Some(got), None, None, None, None).unwrap();
        assert!(matches!(r, SpawnTarget::Named(_)));
    }

    #[test]
    fn ephemeral_without_engine_rejected() {
        let r = resolve_spawn_target(None, None, None, None, None);
        assert!(matches!(r, Err(e) if e.code() == "E_POLICY_DENIED"));
    }

    #[test]
    fn engine_kind_roundtrip() {
        for k in [
            EngineKind::Codex,
            EngineKind::Claude,
            EngineKind::Pi,
            EngineKind::Fake,
        ] {
            assert_eq!(EngineKind::from_db_str(k.as_str()), Some(k));
        }
        assert_eq!(EngineKind::from_db_str("nope"), None);
    }

    // --- audit-before-mutation failure + validator coverage ---

    #[test]
    fn cell_register_rejects_invalid_name() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        for bad in ["Up", "a/b", "", &"a".repeat(65)] {
            let mut def = sample_def("placeholder");
            def.name = bad.into();
            assert_eq!(
                s.cell_register(&def, &mut audit).unwrap_err().code(),
                "E_POLICY_DENIED",
                "expected rejection of {bad:?}"
            );
            assert!(s.cell_get(bad).unwrap().is_none(), "no row for {bad:?}");
        }
    }

    #[test]
    fn cell_update_rejects_invalid_name() {
        let s = Store::open_in_memory().unwrap();
        let (_ad, mut audit) = fresh_audit();
        let mut def = sample_def("placeholder");
        def.name = "Bad/Name".into();
        assert_eq!(
            s.cell_update(&def, &mut audit).unwrap_err().code(),
            "E_POLICY_DENIED"
        );
    }

    #[test]
    fn cell_register_audit_failure_writes_no_row() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("store.db")).unwrap();
        // Audit chain whose target parent does not exist: open() succeeds (it
        // does not create), but append() cannot create the file → io::Error.
        let mut audit = AuditChain::open(&dir.path().join("missing-dir").join("a.jsonl")).unwrap();
        let def = sample_def("c1");
        assert!(s.cell_register(&def, &mut audit).is_err());
        // no consumable record written
        assert!(s.cell_get("c1").unwrap().is_none());
    }

    #[test]
    fn cell_update_audit_failure_writes_no_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("store.db")).unwrap();
        // register once with a working audit
        let mut good = AuditChain::open(&dir.path().join("a.jsonl")).unwrap();
        let def = sample_def("c1");
        s.cell_register(&def, &mut good).unwrap();
        let original = s.cell_get("c1").unwrap().unwrap();
        // now try to update with a broken audit → no mutation
        let mut bad = AuditChain::open(&dir.path().join("missing").join("a.jsonl")).unwrap();
        let mut next = def.clone();
        next.model = Some("changed".into());
        assert!(s.cell_update(&next, &mut bad).is_err());
        let after = s.cell_get("c1").unwrap().unwrap();
        assert_eq!(
            after.model, original.model,
            "update must not land on audit failure"
        );
    }
}
