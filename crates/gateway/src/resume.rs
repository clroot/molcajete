//! Resume keys: how a tunnel gets back the session molcajeted held after
//! the previous tunnel of the same browser ended
//! (`molcajete_protocol::resume`).
//!
//! The page sends a random id as `resume=<id>`: the one its browser used
//! for the connection before, unless another tab is using it. The gateway
//! keeps nothing: it answers molcajeted's
//! `molcajete-resume` with an HMAC of the user, the connection, that id and
//! the connection's parameters under a key derived from the secret key. The
//! same id therefore finds its session again, also after a restart of the
//! gateway or on another replica, while a changed connection, another user
//! or another id gets a new one, and nobody can make a key without the
//! gateway's.
//!
//! Every resume is an ordinary tunnel request: the session cookie, the
//! user's access to the connection and its stored credentials are checked
//! again.

use std::collections::BTreeMap;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Whether `id` can be a page's resume id: 8 to 64 letters, digits, `-`
/// and `_` (a UUID, for the molcajete page).
pub fn valid_id(id: &str) -> bool {
    (8..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The key molcajeted holds a session under, as 64 hex digits.
pub fn key(
    server_key: &[u8; 32],
    user_id: &str,
    connection_id: &str,
    resume_id: &str,
    parameters: &BTreeMap<String, String>,
) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(server_key).expect("any key length");
    for part in [user_id, connection_id, resume_id] {
        mac.update(part.as_bytes());
        mac.update(b"\0");
    }
    mac.update(&parameters_hash(parameters));
    hex::encode(mac.finalize().into_bytes())
}

/// Changes with any parameter of the connection, its stored credentials
/// included.
fn parameters_hash(parameters: &BTreeMap<String, String>) -> [u8; 32] {
    let mut hash = Sha256::new();
    for (name, value) in parameters {
        // Lengths first, so that no two maps hash alike.
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name.as_bytes());
        hash.update((value.len() as u64).to_le_bytes());
        hash.update(value.as_bytes());
    }
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_differ_by_user_connection_tab_and_parameters() {
        let server = [7u8; 32];
        let params = BTreeMap::from([("hostname".to_owned(), "box".to_owned())]);
        let base = key(&server, "u1", "c1", "tab-0001", &params);
        assert_eq!(base.len(), 64);
        assert_eq!(base, key(&server, "u1", "c1", "tab-0001", &params));
        assert_ne!(base, key(&server, "u2", "c1", "tab-0001", &params));
        assert_ne!(base, key(&server, "u1", "c2", "tab-0001", &params));
        assert_ne!(base, key(&server, "u1", "c1", "tab-0002", &params));
        assert_ne!(base, key(&[8u8; 32], "u1", "c1", "tab-0001", &params));
        let changed = BTreeMap::from([("hostname".to_owned(), "other".to_owned())]);
        assert_ne!(base, key(&server, "u1", "c1", "tab-0001", &changed));
        // Parts cannot be moved from one field to the next.
        assert_ne!(
            key(&server, "u1", "c1x", "tab-0001", &params),
            key(&server, "u1c", "1x", "tab-0001", &params)
        );
    }

    #[test]
    fn only_plain_ids_are_taken() {
        assert!(valid_id("2f1c9a6e-8f0b-4d55-9f0e-6c1d2b3a4f5e"));
        assert!(!valid_id("short"));
        assert!(!valid_id(&"a".repeat(65)));
        assert!(!valid_id("has space in it"));
    }
}
