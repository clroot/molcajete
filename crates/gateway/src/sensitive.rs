//! Which values are secrets: never logged, never shown back in the audit
//! log. One list for the whole gateway; anything that stores, logs or
//! returns connection parameters or user fields asks here.
//!
//! It covers the parameters the web UI edits as passwords or keys
//! (`password`, `passphrase`, `private-key`) and their relatives in
//! Guacamole's protocol plugins (`sftp-password`, `gateway-password`,
//! `client-key`, …), plus the user's own `password`.

/// Names that are secrets wherever they appear.
pub const SECRET_NAMES: &[&str] = &[
    "password",
    "passphrase",
    "private-key",
    "client-key",
    "client-secret",
    "secret",
    "token",
];

/// Parts of a name that make it a secret, for the variants Guacamole's
/// plugins add (`sftp-password`, `gateway-password`, `sftp-private-key`,
/// `sftp-passphrase`, …).
const SECRET_PARTS: &[&str] = &["password", "passphrase", "private-key", "secret"];

/// What a secret value is replaced with.
pub const REDACTED: &str = "[redacted]";

/// Whether a value named `name` (a connection parameter, a user field, a
/// key of a JSON object) is a secret. Case-insensitive; `_` and `-` are
/// the same.
pub fn is_secret(name: &str) -> bool {
    let name = name.trim().to_ascii_lowercase().replace('_', "-");
    SECRET_NAMES.contains(&name.as_str()) || SECRET_PARTS.iter().any(|part| name.contains(part))
}

/// Replaces the value of every secret key in `value`, at any depth, with
/// [`REDACTED`]. `null` stays `null`, so "was it set" still shows.
pub fn redact(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if is_secret(key) {
                    redact_all(value);
                } else {
                    redact(value);
                }
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// Replaces every non-null leaf (keeping the shape of `{from, to}` diffs).
fn redact_all(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Null => {}
        serde_json::Value::Object(map) => map.values_mut().for_each(redact_all),
        _ => *value = serde_json::Value::String(REDACTED.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_secret_names() {
        for name in [
            "password",
            "Password",
            "passphrase",
            "private-key",
            "private_key",
            "sftp-password",
            "sftp-private-key",
            "sftp-passphrase",
            "gateway-password",
            "client-key",
            "client_secret",
        ] {
            assert!(is_secret(name), "{name}");
        }
        for name in ["hostname", "username", "port", "domain", "public-key"] {
            assert!(!is_secret(name), "{name}");
        }
    }

    #[test]
    fn redacts_at_any_depth() {
        let mut value = serde_json::json!({
            "password": { "from": null, "to": "hunter22" },
            "parameters.private-key": { "from": "-----BEGIN", "to": "-----BEGIN 2" },
            "hostname": { "from": "a", "to": "b" },
            "nested": [{ "passphrase": "x" }],
        });
        redact(&mut value);
        assert_eq!(
            value,
            serde_json::json!({
                "password": { "from": null, "to": REDACTED },
                "parameters.private-key": { "from": REDACTED, "to": REDACTED },
                "hostname": { "from": "a", "to": "b" },
                "nested": [{ "passphrase": REDACTED }],
            })
        );
    }
}
