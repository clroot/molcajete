//! `molcajete_audit_log`: who changed what, signed in or opened a
//! recording. Entries are written through `crate::audit::record`, which
//! redacts secrets first.

use serde::Serialize;
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbResult, query};

/// One audit log entry.
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AuditEntry {
    pub id: String,
    /// Unix milliseconds.
    pub occurred_at: i64,
    /// The user who acted; `None` for someone not signed in (a failed
    /// sign-in with an unknown username) or the gateway itself.
    pub actor_id: Option<String>,
    /// Their username at the time, or the username a failed sign-in gave.
    pub actor_name: Option<String>,
    /// The browser's address, as the gateway logs it.
    pub source_address: Option<String>,
    /// What happened, such as `user.update`; see `crate::audit::action`.
    pub action: String,
    /// `user`, `group`, `connection`, `folder` or `recording`.
    pub target_type: Option<String>,
    pub target_id: Option<String>,
    /// The target's name at the time.
    pub target_name: Option<String>,
    /// What changed, `{"field": {"from": …, "to": …}}`, and other details,
    /// with secrets redacted.
    pub changes: Option<serde_json::Value>,
}

const COLUMNS: &str = "entry_id, occurred_at, actor_id, actor_name, source_address, action, \
                       target_type, target_id, target_name, changes";

impl AuditEntry {
    fn from_row(row: &AnyRow) -> Result<Self, sqlx::Error> {
        let changes: Option<String> = row.try_get("changes")?;
        Ok(Self {
            id: row.try_get("entry_id")?,
            occurred_at: row.try_get("occurred_at")?,
            actor_id: row.try_get("actor_id")?,
            actor_name: row.try_get("actor_name")?,
            source_address: row.try_get("source_address")?,
            action: row.try_get("action")?,
            target_type: row.try_get("target_type")?,
            target_id: row.try_get("target_id")?,
            target_name: row.try_get("target_name")?,
            changes: changes.and_then(|text| serde_json::from_str(&text).ok()),
        })
    }
}

/// Which entries to list; every field narrows the result.
#[derive(Debug, Default, Clone)]
pub struct AuditFilter {
    /// The actor's username or id.
    pub actor: Option<String>,
    /// An action (`user.update`) or a prefix of whole parts (`user`,
    /// `oidc.user`). Only lowercase letters, digits, `_` and `.`.
    pub action: Option<String>,
    pub target_type: Option<String>,
    /// The target's id or name.
    pub target: Option<String>,
    /// From this time on (Unix milliseconds, inclusive).
    pub since: Option<i64>,
    /// Before this time (Unix milliseconds, exclusive).
    pub until: Option<i64>,
}

/// `LIKE` pattern matching `prefix` and everything below it.
fn below(prefix: &str) -> String {
    let mut pattern = String::with_capacity(prefix.len() + 3);
    for c in prefix.chars() {
        if matches!(c, '%' | '_' | '!') {
            pattern.push('!');
        }
        pattern.push(c);
    }
    pattern.push_str(".%");
    pattern
}

impl Database {
    pub async fn insert_audit_entry(&self, entry: &AuditEntry) -> DbResult<()> {
        let sql = format!(
            "INSERT INTO molcajete_audit_log ({COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        );
        query!(self, &sql)
            .bind(&entry.id)
            .bind(entry.occurred_at)
            .bind(entry.actor_id.as_deref())
            .bind(entry.actor_name.as_deref())
            .bind(entry.source_address.as_deref())
            .bind(&entry.action)
            .bind(entry.target_type.as_deref())
            .bind(entry.target_id.as_deref())
            .bind(entry.target_name.as_deref())
            .bind(entry.changes.as_ref().map(|c| c.to_string()))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Matching entries, newest first.
    pub async fn list_audit_entries(
        &self,
        filter: &AuditFilter,
        limit: i64,
        offset: i64,
    ) -> DbResult<Vec<AuditEntry>> {
        let mut conditions = Vec::new();
        let mut binds: Vec<String> = Vec::new();
        let mut times: Vec<i64> = Vec::new();
        if let Some(actor) = &filter.actor {
            conditions.push("(actor_name = ? OR actor_id = ?)");
            binds.extend([actor.clone(), actor.clone()]);
        }
        if let Some(action) = &filter.action {
            let action = action.trim_end_matches(['.', '*']);
            conditions.push("(action = ? OR action LIKE ? ESCAPE '!')");
            binds.extend([action.to_owned(), below(action)]);
        }
        if let Some(target_type) = &filter.target_type {
            conditions.push("target_type = ?");
            binds.push(target_type.clone());
        }
        if let Some(target) = &filter.target {
            conditions.push("(target_id = ? OR target_name = ?)");
            binds.extend([target.clone(), target.clone()]);
        }
        if let Some(since) = filter.since {
            conditions.push("occurred_at >= ?");
            times.push(since);
        }
        if let Some(until) = filter.until {
            conditions.push("occurred_at < ?");
            times.push(until);
        }
        let mut sql = format!("SELECT {COLUMNS} FROM molcajete_audit_log");
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(" ORDER BY occurred_at DESC, entry_id DESC LIMIT ? OFFSET ?");
        // Text conditions come first in `conditions`, then times.
        let mut q = query!(self, &sql);
        for value in binds {
            q = q.bind(value);
        }
        for time in times {
            q = q.bind(time);
        }
        let rows = q.bind(limit).bind(offset).fetch_all(&self.pool).await?;
        Ok(rows
            .iter()
            .map(AuditEntry::from_row)
            .collect::<Result<_, _>>()?)
    }

    /// Deletes entries older than `cutoff` (Unix milliseconds); returns how
    /// many.
    pub async fn delete_audit_entries_before(&self, cutoff: i64) -> DbResult<u64> {
        let result = query!(
            self,
            "DELETE FROM molcajete_audit_log WHERE occurred_at < ?"
        )
        .bind(cutoff)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::every_backend;
    use super::*;

    fn entry(id: &str, at: i64, actor: Option<&str>, action: &str, target: &str) -> AuditEntry {
        AuditEntry {
            id: id.into(),
            occurred_at: at,
            actor_id: actor.map(|a| format!("id-{a}")),
            actor_name: actor.map(str::to_owned),
            source_address: Some("192.0.2.1".into()),
            action: action.into(),
            target_type: Some("user".into()),
            target_id: Some(format!("id-{target}")),
            target_name: Some(target.into()),
            changes: Some(serde_json::json!({ "role": { "from": "user", "to": "admin" } })),
        }
    }

    #[tokio::test]
    async fn stores_filters_and_expires_entries() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            for e in [
                entry("a", 1_000, Some("root"), "user.create", "bob"),
                entry("b", 2_000, Some("root"), "user.update", "bob"),
                entry("c", 3_000, None, "login.failure", "ghost"),
                entry("d", 4_000, Some("bob"), "login.success", "bob"),
                entry("e", 5_000, Some("ada"), "oidc.user.create", "ada"),
                entry("f", 6_000, Some("root"), "user_x.update", "bob"),
            ] {
                db.insert_audit_entry(&e).await.unwrap();
            }
            let ids = |entries: Vec<AuditEntry>| -> Vec<String> {
                entries.into_iter().map(|e| e.id).collect()
            };
            let all = db
                .list_audit_entries(&AuditFilter::default(), 100, 0)
                .await
                .unwrap();
            assert_eq!(ids(all.clone()), ["f", "e", "d", "c", "b", "a"], "{name}");
            assert_eq!(
                all[5],
                entry("a", 1_000, Some("root"), "user.create", "bob")
            );

            let filter = |f: AuditFilter| {
                let db = db.clone();
                async move { ids(db.list_audit_entries(&f, 100, 0).await.unwrap()) }
            };
            let by_action = |action: &str| AuditFilter {
                action: Some(action.into()),
                ..Default::default()
            };
            assert_eq!(filter(by_action("user")).await, ["b", "a"], "{name}");
            assert_eq!(filter(by_action("user.")).await, ["b", "a"], "{name}");
            assert_eq!(filter(by_action("user.update")).await, ["b"], "{name}");
            assert_eq!(filter(by_action("oidc")).await, ["e"], "{name}");
            assert_eq!(filter(by_action("login.fail")).await, [] as [&str; 0]);
            let by_actor = AuditFilter {
                actor: Some("root".into()),
                since: Some(2_000),
                until: Some(6_000),
                ..Default::default()
            };
            assert_eq!(filter(by_actor).await, ["b"], "{name}");
            let by_target = AuditFilter {
                target: Some("id-ghost".into()),
                target_type: Some("user".into()),
                ..Default::default()
            };
            assert_eq!(filter(by_target).await, ["c"], "{name}");
            let page = db
                .list_audit_entries(&AuditFilter::default(), 2, 2)
                .await
                .unwrap();
            assert_eq!(ids(page), ["d", "c"], "{name}");

            assert_eq!(db.delete_audit_entries_before(3_000).await.unwrap(), 2);
            assert_eq!(
                ids(db
                    .list_audit_entries(&AuditFilter::default(), 100, 0)
                    .await
                    .unwrap()),
                ["f", "e", "d", "c"],
                "{name}"
            );
        }
    }
}
