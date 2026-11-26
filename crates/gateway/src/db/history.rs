//! `molcajete_connection_history`: remote desktop sessions that have
//! closed. Written by `crate::sessions::history` when a tunnel closes.

use serde::Serialize;
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbResult, query};

/// One closed session.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    /// The tunnel id.
    pub id: String,
    /// The user and the connection as they were when the session started;
    /// either may have been renamed or deleted since.
    pub user_id: String,
    pub username: String,
    pub connection_id: String,
    pub connection_name: String,
    pub protocol: String,
    /// The browser's address.
    pub peer_address: Option<String>,
    /// Unix milliseconds.
    pub started_at: i64,
    pub ended_at: i64,
    /// The Guacamole status the tunnel closed with (0: success).
    pub end_status: i64,
    /// Set when someone else ended the session; see
    /// `crate::sessions::EndReason`.
    pub end_reason: Option<String>,
    /// The administrator whose action ended it.
    pub ended_by: Option<String>,
    pub bytes_to_browser: i64,
    pub bytes_to_guacd: i64,
    /// The recording of the session, if *Record sessions* recorded it.
    pub recording_id: Option<String>,
    /// Every recording of the session, oldest first: the one of
    /// `recording_id` and those started from the session menu.
    pub recording_ids: Vec<String>,
}

const COLUMNS: &str = "history_id, user_id, username, connection_id, connection_name, protocol, \
                       peer_address, started_at, ended_at, end_status, end_reason, ended_by, \
                       bytes_to_browser, bytes_to_guacd, recording_id";

impl HistoryEntry {
    fn from_row(row: &AnyRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("history_id")?,
            user_id: row.try_get("user_id")?,
            username: row.try_get("username")?,
            connection_id: row.try_get("connection_id")?,
            connection_name: row.try_get("connection_name")?,
            protocol: row.try_get("protocol")?,
            peer_address: row.try_get("peer_address")?,
            started_at: row.try_get("started_at")?,
            ended_at: row.try_get("ended_at")?,
            end_status: row.try_get("end_status")?,
            end_reason: row.try_get("end_reason")?,
            ended_by: row.try_get("ended_by")?,
            bytes_to_browser: row.try_get("bytes_to_browser")?,
            bytes_to_guacd: row.try_get("bytes_to_guacd")?,
            recording_id: row.try_get("recording_id")?,
            recording_ids: Vec::new(),
        })
    }
}

/// Which sessions to list; every field narrows the result.
#[derive(Debug, Default, Clone)]
pub struct HistoryFilter {
    /// The user's username or id.
    pub user: Option<String>,
    /// The connection's id or name.
    pub connection: Option<String>,
    /// Started at this time or later (Unix milliseconds).
    pub since: Option<i64>,
    /// Started before this time (Unix milliseconds).
    pub until: Option<i64>,
}

impl Database {
    pub async fn insert_history_entry(&self, entry: &HistoryEntry) -> DbResult<()> {
        let sql = format!(
            "INSERT INTO molcajete_connection_history ({COLUMNS}) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        );
        query!(self, &sql)
            .bind(&entry.id)
            .bind(&entry.user_id)
            .bind(&entry.username)
            .bind(&entry.connection_id)
            .bind(&entry.connection_name)
            .bind(&entry.protocol)
            .bind(entry.peer_address.as_deref())
            .bind(entry.started_at)
            .bind(entry.ended_at)
            .bind(entry.end_status)
            .bind(entry.end_reason.as_deref())
            .bind(entry.ended_by.as_deref())
            .bind(entry.bytes_to_browser)
            .bind(entry.bytes_to_guacd)
            .bind(entry.recording_id.as_deref())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Matching sessions, most recently started first.
    pub async fn list_history(
        &self,
        filter: &HistoryFilter,
        limit: i64,
        offset: i64,
    ) -> DbResult<Vec<HistoryEntry>> {
        let mut conditions = Vec::new();
        let mut texts: Vec<String> = Vec::new();
        let mut times: Vec<i64> = Vec::new();
        if let Some(user) = &filter.user {
            conditions.push("(username = ? OR user_id = ?)");
            texts.extend([user.clone(), user.clone()]);
        }
        if let Some(connection) = &filter.connection {
            conditions.push("(connection_id = ? OR connection_name = ?)");
            texts.extend([connection.clone(), connection.clone()]);
        }
        if let Some(since) = filter.since {
            conditions.push("started_at >= ?");
            times.push(since);
        }
        if let Some(until) = filter.until {
            conditions.push("started_at < ?");
            times.push(until);
        }
        let mut sql = format!("SELECT {COLUMNS} FROM molcajete_connection_history");
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(" ORDER BY started_at DESC, history_id DESC LIMIT ? OFFSET ?");
        // Text conditions come first in `conditions`, then times.
        let mut q = query!(self, &sql);
        for value in texts {
            q = q.bind(value);
        }
        for time in times {
            q = q.bind(time);
        }
        let rows = q.bind(limit).bind(offset).fetch_all(&self.pool).await?;
        let mut entries: Vec<HistoryEntry> = rows
            .iter()
            .map(HistoryEntry::from_row)
            .collect::<Result<_, _>>()?;
        let ids: Vec<String> = entries.iter().map(|e| e.id.clone()).collect();
        let mut recordings = self.recordings_of_sessions(&ids).await?;
        for entry in &mut entries {
            let mut all = recordings.remove(&entry.id).unwrap_or_default();
            // Recordings made before they named their session.
            if let Some(id) = &entry.recording_id
                && !all.contains(id)
            {
                all.insert(0, id.clone());
            }
            entry.recording_ids = all;
        }
        Ok(entries)
    }

    /// Deletes sessions that ended before `cutoff` (Unix milliseconds);
    /// returns how many.
    pub async fn delete_history_before(&self, cutoff: i64) -> DbResult<u64> {
        let result = query!(
            self,
            "DELETE FROM molcajete_connection_history WHERE ended_at < ?"
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

    fn entry(id: &str, user: &str, connection: &str, started_at: i64) -> HistoryEntry {
        HistoryEntry {
            id: id.into(),
            user_id: format!("id-{user}"),
            username: user.into(),
            connection_id: connection.into(),
            connection_name: format!("{connection} desktop"),
            protocol: "vnc".into(),
            peer_address: Some("192.0.2.1".into()),
            started_at,
            ended_at: started_at + 60_000,
            end_status: 0,
            end_reason: None,
            ended_by: None,
            bytes_to_browser: 5_000_000_000,
            bytes_to_guacd: 42,
            recording_id: None,
            recording_ids: Vec::new(),
        }
    }

    #[tokio::test]
    async fn stores_filters_and_expires_sessions() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let mut ended = entry("c", "carol", "win", 3_000);
            ended.end_status = 0x020B;
            ended.end_reason = Some("admin".into());
            ended.ended_by = Some("root".into());
            ended.recording_id = Some("rec".into());
            ended.recording_ids = vec!["rec".into()];
            for e in [
                entry("a", "bob", "win", 1_000),
                entry("b", "bob", "linux", 2_000),
                ended.clone(),
            ] {
                db.insert_history_entry(&e).await.unwrap();
            }
            let ids = |entries: Vec<HistoryEntry>| -> Vec<String> {
                entries.into_iter().map(|e| e.id).collect()
            };
            let all = db
                .list_history(&HistoryFilter::default(), 100, 0)
                .await
                .unwrap();
            assert_eq!(ids(all.clone()), ["c", "b", "a"], "{name}");
            assert_eq!(all[0], ended, "{name}");
            assert_eq!(all[2], entry("a", "bob", "win", 1_000), "{name}");

            let list = |f: HistoryFilter| {
                let db = db.clone();
                async move { ids(db.list_history(&f, 100, 0).await.unwrap()) }
            };
            let by_user = HistoryFilter {
                user: Some("id-bob".into()),
                ..Default::default()
            };
            assert_eq!(list(by_user).await, ["b", "a"], "{name}");
            let by_connection = HistoryFilter {
                connection: Some("win desktop".into()),
                since: Some(2_000),
                until: Some(4_000),
                ..Default::default()
            };
            assert_eq!(list(by_connection).await, ["c"], "{name}");
            let page = db
                .list_history(&HistoryFilter::default(), 1, 1)
                .await
                .unwrap();
            assert_eq!(ids(page), ["b"], "{name}");

            assert_eq!(db.delete_history_before(62_001).await.unwrap(), 2);
            assert_eq!(
                ids(db
                    .list_history(&HistoryFilter::default(), 100, 0)
                    .await
                    .unwrap()),
                ["c"],
                "{name}"
            );
        }
    }
}
