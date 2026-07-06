//! Canonical JSON (SPEC §12.1): object keys sorted lexicographically at every
//! depth, no insignificant whitespace, no trailing newline.
use serde_json::Value;

/// Serialize a JSON value to canonical form. Deterministic regardless of the
/// insertion order of the input object.
pub fn to_canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_value(v, &mut out);
    out
}

fn write_value(v: &Value, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => write_json_string(s, out),
        Value::Array(a) => {
            out.push('[');
            for (i, e) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(e, out);
            }
            out.push(']');
        }
        Value::Object(o) => {
            out.push('{');
            let mut keys: Vec<&String> = o.keys().collect();
            keys.sort();
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(k, out);
                out.push(':');
                write_value(&o[*k], out);
            }
            out.push('}');
        }
    }
}

fn write_json_string(s: &str, out: &mut String) {
    // serde_json's string escaping is the canonical form.
    out.push_str(&serde_json::to_string(s).unwrap());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_sorts_keys_recursively() {
        let v: serde_json::Value = serde_json::json!({"b":1,"a":{"z":true,"y":[{"k":2,"a":1}]}});
        assert_eq!(
            to_canonical_json(&v),
            r#"{"a":{"y":[{"a":1,"k":2}],"z":true},"b":1}"#
        );
    }
}
