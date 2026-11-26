//! The audit log: who changed what, who signed in (or failed to), who
//! opened a recording, who ended a session, and what API tokens and
//! one-time session links did.
//!
//! Handlers describe what happened with an [`Entry`] and pass it to
//! [`record`] once the change has been made:
//!
//! ```ignore
//! audit::record(
//!     &state.db,
//!     Entry::new(action::USER_UPDATE)
//!         .by(&admin)
//!         .from(&address)
//!         .target(target::USER, &user.id, &user.username)
//!         .changes(audit::diff(&before, &after)),
//! )
//! .await;
//! ```
//!
//! [`record`] redacts secrets (see [`crate::sensitive`]), writes one
//! structured log line with the target `audit` (for SIEMs) and stores the
//! entry in `molcajete_audit_log`. It never fails the request: when the
//! entry cannot be stored the log line is still written, with an error and
//! `molcajete_gateway_audit_write_failures_total`.
//!
//! Entries are written after the change, not in its transaction: one
//! request (say, a user with new groups and grants) is several repository
//! calls, each with its own transaction.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tracing::{error, info, warn};

use crate::auth::ClientAddress;
use crate::config::AuditConfig;
use crate::db::{AuditEntry, Database, User, now_millis};
use crate::sensitive;

/// Every action the gateway records. The names are stable: filters, SIEM
/// rules and CSV exports depend on them. New actions go here (and into
/// [`action::ALL`] and docs/administration.md).
pub mod action {
    /// Signed in with a username and password.
    pub const LOGIN_SUCCESS: &str = "login.success";
    /// A username and password were refused. The actor is the user with
    /// that name, or only the name when there is none.
    pub const LOGIN_FAILURE: &str = "login.failure";
    /// Too many failed sign-ins: further attempts from the address, or for
    /// the username, are refused for a while. Once per lockout; the actor
    /// is the name typed in the attempt that started it.
    pub const LOGIN_THROTTLED: &str = "login.throttled";
    /// Signed out.
    pub const LOGOUT: &str = "logout";
    /// Changed their own password.
    pub const USER_PASSWORD: &str = "user.password";

    /// Signed in through the OpenID Connect provider.
    pub const OIDC_LOGIN_SUCCESS: &str = "oidc.login.success";
    /// A sign-in through the provider was refused or failed.
    pub const OIDC_LOGIN_FAILURE: &str = "oidc.login.failure";
    /// A user was created on their first single sign-on.
    pub const OIDC_USER_CREATE: &str = "oidc.user.create";
    /// An existing user was linked to their identity at the provider.
    pub const OIDC_USER_LINK: &str = "oidc.user.link";
    /// A user was renamed after the provider.
    pub const OIDC_USER_RENAME: &str = "oidc.user.rename";
    /// A user's groups were set from the provider's (`sync_groups`).
    pub const OIDC_USER_GROUPS: &str = "oidc.user.groups";
    /// A user's role was set from the provider's groups.
    pub const OIDC_USER_ROLE: &str = "oidc.user.role";

    pub const USER_CREATE: &str = "user.create";
    /// Username, role, disabled, password (redacted), groups or grants.
    pub const USER_UPDATE: &str = "user.update";
    pub const USER_DELETE: &str = "user.delete";

    pub const GROUP_CREATE: &str = "group.create";
    /// Name, role, members or grants.
    pub const GROUP_UPDATE: &str = "group.update";
    pub const GROUP_DELETE: &str = "group.delete";

    pub const FOLDER_CREATE: &str = "folder.create";
    pub const FOLDER_RENAME: &str = "folder.rename";
    pub const FOLDER_MOVE: &str = "folder.move";
    pub const FOLDER_DELETE: &str = "folder.delete";
    /// The users and groups granted the folder.
    pub const FOLDER_ACCESS: &str = "folder.access";

    pub const CONNECTION_CREATE: &str = "connection.create";
    pub const CONNECTION_UPDATE: &str = "connection.update";
    pub const CONNECTION_DELETE: &str = "connection.delete";
    pub const CONNECTION_MOVE: &str = "connection.move";
    /// The users and groups granted the connection.
    pub const CONNECTION_ACCESS: &str = "connection.access";

    /// A recording file was downloaded (not played back in the page).
    pub const RECORDING_DOWNLOAD: &str = "recording.download";
    pub const RECORDING_DELETE: &str = "recording.delete";
    /// The owner of a session started recording it from the session menu
    /// (`session`, `connection`); the target is the new recording.
    pub const RECORDING_START: &str = "recording.start";
    /// A recording started from the session menu stopped (`session`,
    /// `reason`: `stopped` by its owner, who is the actor, or
    /// `session_ended`, `daemon_closed`, `error`).
    pub const RECORDING_STOP: &str = "recording.stop";

    /// At start, connection credentials stored in plain text were encrypted
    /// (`count`): the first start after upgrading, or a name newly treated
    /// as a secret.
    pub const SECRETS_ENCRYPT: &str = "secrets.encrypt";
    /// `rotate-key` re-encrypted stored credentials (`count`) with the
    /// current key (`keyId`).
    pub const SECRETS_ROTATE: &str = "secrets.rotate";
    /// A live session was ended by an administrator, or because its user
    /// was disabled, deleted or lost access to the connection (the
    /// `reason` detail).
    pub const SESSION_END: &str = "session.end";
    /// An API token was created (by its owner).
    pub const TOKEN_CREATE: &str = "token.create";
    /// An API token was revoked, by its owner or an administrator.
    pub const TOKEN_REVOKE: &str = "token.revoke";
    /// An API token was used: the first time, from a new address, or for
    /// the first time after an hour without use. Not every request.
    pub const TOKEN_USE: &str = "token.use";
    /// A one-time session link was made for a connection.
    pub const TICKET_CREATE: &str = "ticket.create";
    /// A one-time session link opened its connection.
    pub const TICKET_USE: &str = "ticket.use";
    /// A user created a share link of their live session (`linkId`,
    /// `readOnly`, `expiresAt`).
    pub const SESSION_SHARE: &str = "session.share";
    /// A user revoked a share link of their session (`linkId`); whoever
    /// joined through it was disconnected.
    pub const SESSION_UNSHARE: &str = "session.unshare";
    /// A user joined a live session through a share link (`linkId`,
    /// `readOnly`, `owner`); the target is the session joined.
    pub const SESSION_JOIN: &str = "session.join";
    /// An administrator started watching a live session (`owner`); the
    /// target is the session watched.
    pub const SESSION_SHADOW: &str = "session.shadow";
    /// A tunnel got back a session molcajeted held after the previous
    /// tunnel ended (`connection`, `guacdId`); the target is the new
    /// tunnel's session.
    pub const SESSION_RESUME: &str = "session.resume";

    pub const ALL: &[&str] = &[
        LOGIN_SUCCESS,
        LOGIN_FAILURE,
        LOGIN_THROTTLED,
        LOGOUT,
        USER_PASSWORD,
        OIDC_LOGIN_SUCCESS,
        OIDC_LOGIN_FAILURE,
        OIDC_USER_CREATE,
        OIDC_USER_LINK,
        OIDC_USER_RENAME,
        OIDC_USER_GROUPS,
        OIDC_USER_ROLE,
        USER_CREATE,
        USER_UPDATE,
        USER_DELETE,
        GROUP_CREATE,
        GROUP_UPDATE,
        GROUP_DELETE,
        FOLDER_CREATE,
        FOLDER_RENAME,
        FOLDER_MOVE,
        FOLDER_DELETE,
        FOLDER_ACCESS,
        CONNECTION_CREATE,
        CONNECTION_UPDATE,
        CONNECTION_DELETE,
        CONNECTION_MOVE,
        CONNECTION_ACCESS,
        RECORDING_DOWNLOAD,
        RECORDING_DELETE,
        RECORDING_START,
        RECORDING_STOP,
        SECRETS_ENCRYPT,
        SECRETS_ROTATE,
        SESSION_END,
        TOKEN_CREATE,
        TOKEN_REVOKE,
        TOKEN_USE,
        TICKET_CREATE,
        TICKET_USE,
        SESSION_SHARE,
        SESSION_UNSHARE,
        SESSION_JOIN,
        SESSION_SHADOW,
        SESSION_RESUME,
    ];
}

/// What an entry is about.
pub mod target {
    pub const USER: &str = "user";
    pub const GROUP: &str = "group";
    pub const CONNECTION: &str = "connection";
    pub const FOLDER: &str = "folder";
    pub const RECORDING: &str = "recording";
    /// A remote desktop session; the id is its tunnel id.
    pub const SESSION: &str = "session";
    pub const TOKEN: &str = "token";
}

/// An audit log entry being described; see the module documentation.
#[derive(Debug, Clone)]
#[must_use = "an entry does nothing until it is passed to audit::record"]
pub struct Entry {
    action: &'static str,
    actor_id: Option<String>,
    actor_name: Option<String>,
    source: Option<String>,
    target: Option<(&'static str, String, Option<String>)>,
    changes: Map<String, Value>,
}

impl Entry {
    /// `action` is one of [`action`].
    pub fn new(action: &'static str) -> Self {
        Self {
            action,
            actor_id: None,
            actor_name: None,
            source: None,
            target: None,
            changes: Map::new(),
        }
    }

    /// Done by `user`.
    pub fn by(mut self, user: &User) -> Self {
        self.actor_id = Some(user.id.clone());
        self.actor_name = Some(user.username.clone());
        self
    }

    /// Done by the user `id` called `name`.
    pub fn by_id(mut self, id: &str, name: &str) -> Self {
        self.actor_id = Some(id.to_owned());
        self.actor_name = Some(name.to_owned());
        self
    }

    /// Done by someone who only gave a name (a failed sign-in with an
    /// unknown username). Cut to 255 characters.
    pub fn by_name(mut self, name: &str) -> Self {
        self.actor_id = None;
        self.actor_name = Some(name.chars().take(255).collect());
        self
    }

    /// From the browser at `address`.
    pub fn from(mut self, address: &ClientAddress) -> Self {
        self.source = Some(address.0.chars().take(255).collect());
        self
    }

    /// Not from a browser: `"command line"`, `"configuration file"`, …
    pub fn source(mut self, source: &str) -> Self {
        self.source = Some(source.to_owned());
        self
    }

    /// About `id` of `kind` (one of [`target`]), called `name` at the time.
    pub fn target(mut self, kind: &'static str, id: &str, name: &str) -> Self {
        self.target = Some((kind, id.to_owned(), Some(name.to_owned())));
        self
    }

    /// What changed: an object such as [`diff`] returns. Merged with
    /// earlier changes and details.
    pub fn changes(mut self, changes: Value) -> Self {
        if let Value::Object(map) = changes {
            self.changes.extend(map);
        }
        self
    }

    /// One more detail, such as why a sign-in failed.
    pub fn detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.changes.insert(key.to_owned(), value.into());
        self
    }

    fn into_record(self, occurred_at: i64) -> AuditEntry {
        let (target_type, target_id, target_name) = match self.target {
            Some((kind, id, name)) => (Some(kind.to_owned()), Some(id), name),
            None => (None, None, None),
        };
        let changes = (!self.changes.is_empty()).then(|| {
            let mut changes = Value::Object(self.changes);
            sensitive::redact(&mut changes);
            changes
        });
        AuditEntry {
            // Time-ordered and increasing within the process, so entries of
            // the same millisecond still list in the order they happened.
            id: uuid::Uuid::now_v7().to_string(),
            occurred_at,
            actor_id: self.actor_id,
            actor_name: self.actor_name,
            source_address: self.source,
            action: self.action.to_owned(),
            target_type,
            target_id,
            target_name,
            changes,
        }
    }
}

/// Records `entry`: a log line with the target `audit`, then a row in the
/// audit log. Failing to store it is logged and counted, never returned.
pub async fn record(db: &Database, entry: Entry) {
    let entry = entry.into_record(now_millis());
    log(&entry);
    if let Err(err) = db.insert_audit_entry(&entry).await {
        metrics::counter!("molcajete_gateway_audit_write_failures_total").increment(1);
        error!(
            error = %err,
            action = %entry.action,
            entry = %entry.id,
            "cannot store an audit log entry"
        );
    }
}

fn log(entry: &AuditEntry) {
    let text = |value: &Option<String>| value.clone().unwrap_or_default();
    let changes = entry.changes.as_ref().map(|c| c.to_string());
    info!(
        target: "audit",
        entry = %entry.id,
        action = %entry.action,
        actor = %text(&entry.actor_name),
        actor_id = %text(&entry.actor_id),
        source = %text(&entry.source_address),
        target_type = %text(&entry.target_type),
        target_id = %text(&entry.target_id),
        target_name = %text(&entry.target_name),
        changes = %changes.unwrap_or_default(),
        "{}",
        entry.action
    );
}

/// The fields that differ between `before` and `after` (JSON objects;
/// `null` for a record that did not exist yet or no longer does):
/// `{"field": {"from": …, "to": …}}`. Nested objects are flattened
/// (`parameters.hostname`); lists of strings show what was added and
/// removed (`{"added": […], "removed": […]}`). A missing field and `null`
/// are the same. Secrets are redacted by [`record`].
pub fn diff(before: &Value, after: &Value) -> Value {
    let (before, after) = (flatten(before), flatten(after));
    let keys: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    let mut changes = Map::new();
    let get = |map: &BTreeMap<String, Value>, key: &str| -> Option<Value> {
        map.get(key).filter(|v| !v.is_null()).cloned()
    };
    for key in keys {
        let (old, new) = (get(&before, key), get(&after, key));
        let (old, new) = (old.as_ref(), new.as_ref());
        if old == new {
            continue;
        }
        let change = match (string_set(old), string_set(new)) {
            (Some(old), Some(new)) => {
                let mut change = Map::new();
                let added: Vec<&str> = new.difference(&old).copied().collect();
                let removed: Vec<&str> = old.difference(&new).copied().collect();
                if !added.is_empty() {
                    change.insert("added".into(), json!(added));
                }
                if !removed.is_empty() {
                    change.insert("removed".into(), json!(removed));
                }
                if change.is_empty() {
                    continue; // Same items in another order.
                }
                Value::Object(change)
            }
            _ => json!({ "from": old, "to": new }),
        };
        changes.insert(key.clone(), change);
    }
    Value::Object(changes)
}

/// `{"a": {"b": 1}, "c": 2}` → `{"a.b": 1, "c": 2}`; `null` → nothing.
fn flatten(value: &Value) -> BTreeMap<String, Value> {
    fn walk(prefix: &str, value: &Value, out: &mut BTreeMap<String, Value>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let key = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(&key, value, out);
                }
            }
            Value::Null if prefix.is_empty() => {}
            _ => {
                out.insert(prefix.to_owned(), value.clone());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk("", value, &mut out);
    out
}

/// A list of strings as a set; `None` for anything else. A missing value
/// is the empty list when the other side is a list.
fn string_set(value: Option<&Value>) -> Option<BTreeSet<&str>> {
    match value {
        None => Some(BTreeSet::new()),
        Some(Value::Array(items)) => items.iter().map(Value::as_str).collect(),
        Some(_) => None,
    }
}

/// Unix milliseconds as `2026-11-25T08:30:00.123Z`.
pub fn rfc3339(ms: i64) -> String {
    let (days, ms_of_day) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    let secs = ms_of_day / 1000;
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        ms_of_day % 1000
    )
}

/// One CSV field: quoted when needed, and with a leading `'` when a
/// spreadsheet would take it for a formula (usernames of failed sign-ins
/// are whatever someone typed).
pub fn csv_field(value: &str) -> String {
    let value = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

/// How often expired entries are deleted.
const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Deletes entries older than `[audit] retention_days` now and every hour.
pub fn start_maintenance(db: Database, config: AuditConfig) {
    if config.retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_INTERVAL);
        loop {
            interval.tick().await;
            let cutoff = now_millis().saturating_sub(config.retention_days as i64 * 86_400_000);
            match db.delete_audit_entries_before(cutoff).await {
                Ok(0) => {}
                Ok(deleted) => info!(deleted, "deleted expired audit log entries"),
                Err(err) => warn!(error = %err, "audit log retention failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diffs_fields_lists_and_nested_objects() {
        let before = json!({
            "name": "Desk",
            "parameters": { "hostname": "a", "password": "old" },
            "groups": ["g1", "g2"],
            "same": [1, 2],
        });
        let after = json!({
            "name": "Desk",
            "parameters": { "hostname": "b" },
            "groups": ["g2", "g3"],
            "same": [1, 2],
        });
        assert_eq!(
            diff(&before, &after),
            json!({
                "parameters.hostname": { "from": "a", "to": "b" },
                "parameters.password": { "from": "old", "to": null },
                "groups": { "added": ["g3"], "removed": ["g1"] },
            })
        );
        assert_eq!(
            diff(
                &Value::Null,
                &json!({ "name": "x", "groups": ["g"], "port": null })
            ),
            json!({
                "name": { "from": null, "to": "x" },
                "groups": { "added": ["g"] },
            })
        );
        assert_eq!(
            diff(
                &json!({ "groups": ["a", "b"] }),
                &json!({ "groups": ["b", "a"] })
            ),
            json!({})
        );
    }

    #[test]
    fn entries_redact_secrets() {
        let user = User {
            id: "u1".into(),
            username: "root".into(),
            role: crate::db::Role::Admin,
            effective_role: crate::db::Role::Admin,
            is_admin: true,
            disabled: false,
            created_at: 0,
            last_login_at: None,
        };
        let record = Entry::new(action::CONNECTION_UPDATE)
            .by(&user)
            .from(&ClientAddress("192.0.2.7".into()))
            .target(target::CONNECTION, "desk", "Desk")
            .changes(diff(
                &json!({ "parameters": { "password": "hunter22", "private-key": "KEY" } }),
                &json!({ "parameters": { "password": "hunter23", "passphrase": "pp" } }),
            ))
            .into_record(42);
        assert_eq!(record.occurred_at, 42);
        assert_eq!(record.actor_name.as_deref(), Some("root"));
        assert_eq!(record.source_address.as_deref(), Some("192.0.2.7"));
        assert_eq!(record.target_id.as_deref(), Some("desk"));
        let changes = record.changes.unwrap().to_string();
        for secret in ["hunter22", "hunter23", "KEY", "pp\""] {
            assert!(!changes.contains(secret), "{changes}");
        }
        assert!(changes.contains("parameters.password"), "{changes}");
        assert!(Entry::new(action::LOGOUT).into_record(0).changes.is_none());
    }

    #[test]
    fn formats_times_and_csv_fields() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339(1_795_595_400_123), "2026-11-25T08:30:00.123Z");
        assert_eq!(rfc3339(951_782_400_000), "2000-02-29T00:00:00.000Z");
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=cmd()"), "'=cmd()");
        assert_eq!(csv_field("-1,2"), "\"'-1,2\"");
    }

    #[test]
    fn action_names_are_lowercase_dotted() {
        let mut seen = BTreeSet::new();
        for name in action::ALL {
            assert!(seen.insert(name), "{name} twice");
            assert!(
                name.chars()
                    .all(|c| c.is_ascii_lowercase() || c == '.' || c == '_'),
                "{name}"
            );
        }
    }
}
