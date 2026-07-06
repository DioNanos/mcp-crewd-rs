//! Shared input validators for cell-fabric primitives (SPEC §20.1 / §20.2 /
//! §20.7). Enforcing the normative rules here — at the Store primitive — means
//! every entry point (handler, daemon, test) gets the same rejection, instead
//! of trusting each caller to validate.
use crate::error::BusError;

/// SPEC §20.1: reserved prefix for daemon-generated ephemeral cell names.
/// A client MUST NOT supply a name with this prefix: only the
/// daemon mints `~ephemeral-<uuid8>` when `cell` is omitted.
pub const EPHEMERAL_PREFIX: &str = "~ephemeral-";

/// SPEC §20.1/§20.2: a **named** cell name is `[a-z0-9_-]{1,64}`. The reserved
/// `~ephemeral-` prefix is rejected here — it is only valid on the internal,
/// daemon-generated spawn path (`validate_cell_name_allowing_ephemeral`).
pub fn validate_cell_name(name: &str) -> Result<(), BusError> {
    let n = name.len();
    if !(1..=64).contains(&n) {
        return Err(BusError::PolicyDenied(format!(
            "cell name length out of [1,64]: {n}"
        )));
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    {
        return Err(BusError::PolicyDenied(format!(
            "cell name has chars outside [a-z0-9_-]: {name}"
        )));
    }
    Ok(())
}

/// The spawn primitive accepts either a valid **named** cell name
/// OR a daemon-generated ephemeral name `~ephemeral-<suffix>` where `suffix` is
/// `[a-z0-9]{1,53}`. Clients never reach this with a `~`-prefixed name — the
/// handler rejects client-supplied `~*` names with `E_POLICY_DENIED` before
/// generating a fresh one — so allowing the prefix here does not widen the
/// external surface.
pub fn validate_cell_name_allowing_ephemeral(name: &str) -> Result<(), BusError> {
    if let Some(suffix) = name.strip_prefix(EPHEMERAL_PREFIX) {
        let n = suffix.len();
        if !(1..=53).contains(&n) {
            return Err(BusError::PolicyDenied(format!(
                "ephemeral cell suffix length out of [1,53]: {n}"
            )));
        }
        if !suffix
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        {
            return Err(BusError::PolicyDenied(format!(
                "ephemeral cell suffix has chars outside [a-z0-9]: {suffix}"
            )));
        }
        return Ok(());
    }
    validate_cell_name(name)
}

/// SPEC §20.2/§20.7: an idempotency key is `1..=128` bytes.
pub fn validate_idempotency_key(key: &str) -> Result<(), BusError> {
    let n = key.len();
    if !(1..=128).contains(&n) {
        return Err(BusError::PolicyDenied(format!(
            "idempotency key length out of [1,128]: {n}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_name_valid_forms() {
        assert!(validate_cell_name("a").is_ok());
        assert!(validate_cell_name("glm-worker-a").is_ok());
        assert!(validate_cell_name("c1_x-2").is_ok());
        assert!(validate_cell_name(&"a".repeat(64)).is_ok());
    }

    #[test]
    fn cell_name_invalid_forms() {
        // empty / oversize / uppercase / slash / dot / space → E_POLICY_DENIED
        for bad in ["", "Up", "a/b", "a.b", "a b", &"a".repeat(65), "à", "a!b"] {
            assert_eq!(
                validate_cell_name(bad).unwrap_err().code(),
                "E_POLICY_DENIED",
                "expected rejection of {bad:?}"
            );
        }
    }

    #[test]
    fn ephemeral_name_accepted_only_via_dedicated_validator() {
        // The daemon-minted form passes the ephemeral-aware validator…
        assert!(validate_cell_name_allowing_ephemeral("~ephemeral-0190a1b2").is_ok());
        // …but is rejected by the strict named-cell validator.
        assert_eq!(
            validate_cell_name("~ephemeral-0190a1b2")
                .unwrap_err()
                .code(),
            "E_POLICY_DENIED"
        );
        // A malformed ephemeral suffix is rejected even by the lenient path.
        for bad in [
            "~ephemeral-",
            "~ephemeral-UP",
            "~ephemeral-a/b",
            "~ephemeral- x",
        ] {
            assert_eq!(
                validate_cell_name_allowing_ephemeral(bad)
                    .unwrap_err()
                    .code(),
                "E_POLICY_DENIED",
                "expected rejection of {bad:?}"
            );
        }
        // A normal named cell still passes the lenient path.
        assert!(validate_cell_name_allowing_ephemeral("glm-worker-a").is_ok());
    }

    #[test]
    fn idempotency_key_bounds() {
        assert!(validate_idempotency_key("k").is_ok());
        assert!(validate_idempotency_key(&"k".repeat(128)).is_ok());
        assert_eq!(
            validate_idempotency_key("").unwrap_err().code(),
            "E_POLICY_DENIED"
        );
        assert_eq!(
            validate_idempotency_key(&"k".repeat(129))
                .unwrap_err()
                .code(),
            "E_POLICY_DENIED"
        );
    }
}
