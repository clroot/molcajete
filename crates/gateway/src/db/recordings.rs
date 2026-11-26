//! `molcajete_recording`: the index of recorded sessions. The files
//! themselves live in `[recordings] directory` (see `crate::recording`).

use serde::Serialize;
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbError, DbResult, query};

/// A recorded session.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Recording {
    pub id: String,
    /// The connection and the user as they were when the session started;
    /// either may have been renamed or deleted since.
    pub connection_id: String,
    pub connection_name: String,
    pub protocol: String,
    pub user_id: String,
    pub username: String,
    /// File name inside the recordings directory.
    pub file_name: String,
    /// The recording contains the key presses of the session.
    pub include_keys: bool,
    /// Unix milliseconds.
    pub started_at: i64,
    /// `None` while the session is still being recorded.
    pub ended_at: Option<i64>,
    pub size_bytes: i64,
    /// The session (tunnel id) recorded; a session can have several
    /// recordings. `None` for recordings older than this field.
    pub session_id: Option<String>,
    /// The user who started the recording from the session menu, who may
    /// play and download it. `None` for recordings of *Record sessions*.
    pub started_by: Option<String>,
}

const COLUMNS: &str = "recording_id, connection_id, connection_name, protocol, user_id, \
                       username, file_name, include_keys, started_at, ended_at, size_bytes, \
                       session_id, started_by";

fn from_row(row: &AnyRow) -> Result<Recording, sqlx::Error> {
    Ok(Recording {
        id: row.try_get("recording_id")?,
        connection_id: row.try_get("connection_id")?,
        connection_name: row.try_get("connection_name")?,
        protocol: row.try_get("protocol")?,
        user_id: row.try_get("user_id")?,
        username: row.try_get("username")?,
        file_name: row.try_get("file_name")?,
        include_keys: row.try_get::<i64, _>("include_keys")? != 0,
        started_at: row.try_get("started_at")?,
        ended_at: row.try_get("ended_at")?,
        size_bytes: row.try_get("size_bytes")?,
        session_id: row.try_get("session_id")?,
        started_by: row.try_get("started_by")?,
    })
}

impl Database {
    pub async fn create_recording(&self, r: &Recording) -> DbResult<()> {
        query!(
            self,
            "INSERT INTO molcajete_recording \
             (recording_id, connection_id, connection_name, protocol, user_id, username, \
              file_name, include_keys, started_at, ended_at, size_bytes, session_id, started_by) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&r.id)
        .bind(&r.connection_id)
        .bind(&r.connection_name)
        .bind(&r.protocol)
        .bind(&r.user_id)
        .bind(&r.username)
        .bind(&r.file_name)
        .bind(i64::from(r.include_keys))
        .bind(r.started_at)
        .bind(r.ended_at)
        .bind(r.size_bytes)
        .bind(r.session_id.as_deref())
        .bind(r.started_by.as_deref())
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(())
    }

    /// Marks a recording as complete.
    pub async fn finish_recording(&self, id: &str, ended_at: i64, size_bytes: i64) -> DbResult<()> {
        query!(
            self,
            "UPDATE molcajete_recording SET ended_at = ?, size_bytes = ? WHERE recording_id = ?"
        )
        .bind(ended_at)
        .bind(size_bytes)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn recording(&self, id: &str) -> DbResult<Recording> {
        let sql = format!("SELECT {COLUMNS} FROM molcajete_recording WHERE recording_id = ?");
        let row = query!(self, &sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok(from_row(&row)?)
    }

    /// Every recording, newest first.
    pub async fn list_recordings(&self) -> DbResult<Vec<Recording>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM molcajete_recording ORDER BY started_at DESC, recording_id"
        );
        let rows = query!(self, &sql).fetch_all(&self.pool).await?;
        rows.iter()
            .map(from_row)
            .collect::<Result<_, _>>()
            .map_err(DbError::from)
    }

    /// The recordings `user_id` started from the session menu, newest
    /// first.
    pub async fn recordings_started_by(&self, user_id: &str) -> DbResult<Vec<Recording>> {
        let sql = format!(
            "SELECT {COLUMNS} FROM molcajete_recording WHERE started_by = ? \
             ORDER BY started_at DESC, recording_id"
        );
        let rows = query!(self, &sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(from_row)
            .collect::<Result<_, _>>()
            .map_err(DbError::from)
    }

    /// The ids of the recordings of each of `sessions` (tunnel ids), oldest
    /// first. Sessions without recordings are left out.
    pub async fn recordings_of_sessions(
        &self,
        sessions: &[String],
    ) -> DbResult<std::collections::HashMap<String, Vec<String>>> {
        let mut found: std::collections::HashMap<String, Vec<String>> = Default::default();
        for chunk in sessions.chunks(500) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT recording_id, session_id FROM molcajete_recording \
                 WHERE session_id IN ({placeholders}) ORDER BY started_at, recording_id"
            );
            let mut q = query!(self, &sql);
            for session in chunk {
                q = q.bind(session);
            }
            for row in q.fetch_all(&self.pool).await? {
                let session: String = row.try_get("session_id")?;
                found
                    .entry(session)
                    .or_default()
                    .push(row.try_get("recording_id")?);
            }
        }
        Ok(found)
    }

    /// Recordings that were still open when the gateway stopped.
    pub async fn unfinished_recordings(&self) -> DbResult<Vec<Recording>> {
        let sql = format!("SELECT {COLUMNS} FROM molcajete_recording WHERE ended_at IS NULL");
        let rows = query!(self, &sql).fetch_all(&self.pool).await?;
        rows.iter()
            .map(from_row)
            .collect::<Result<_, _>>()
            .map_err(DbError::from)
    }

    pub async fn delete_recording(&self, id: &str) -> DbResult<()> {
        let result = query!(
            self,
            "DELETE FROM molcajete_recording WHERE recording_id = ?"
        )
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::every_backend;
    use super::*;

    fn recording(id: &str, started_at: i64) -> Recording {
        Recording {
            id: id.into(),
            connection_id: "desktop".into(),
            connection_name: "Desktop".into(),
            protocol: "vnc".into(),
            user_id: "u1".into(),
            username: "bob".into(),
            file_name: format!("{id}.guac"),
            include_keys: false,
            started_at,
            ended_at: None,
            size_bytes: 0,
            session_id: None,
            started_by: None,
        }
    }

    #[tokio::test]
    async fn recordings_by_session_and_starter() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let mut a = recording("a", 1_000);
            a.session_id = Some("s1".into());
            let mut b = recording("b", 2_000);
            b.session_id = Some("s1".into());
            b.started_by = Some("u1".into());
            let mut c = recording("c", 3_000);
            c.session_id = Some("s2".into());
            c.started_by = Some("u2".into());
            for r in [&a, &b, &c, &recording("old", 500)] {
                db.create_recording(r).await.unwrap();
            }
            assert_eq!(db.recording("b").await.unwrap(), b, "{name}");
            let by_session = db
                .recordings_of_sessions(&["s1".into(), "s2".into(), "s3".into()])
                .await
                .unwrap();
            assert_eq!(by_session.len(), 2, "{name}");
            assert_eq!(by_session["s1"], ["a", "b"], "{name}");
            assert_eq!(by_session["s2"], ["c"], "{name}");
            let mine: Vec<_> = db
                .recordings_started_by("u1")
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            assert_eq!(mine, ["b"], "{name}");
        }
    }

    #[tokio::test]
    async fn recording_index() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            db.create_recording(&recording("a", 1_000)).await.unwrap();
            let mut b = recording("b", 2_000);
            b.include_keys = true;
            db.create_recording(&b).await.unwrap();
            assert!(
                matches!(
                    db.create_recording(&recording("a", 3_000)).await,
                    Err(DbError::Conflict)
                ),
                "{name}"
            );

            let ids: Vec<_> = db
                .list_recordings()
                .await
                .unwrap()
                .into_iter()
                .map(|r| r.id)
                .collect();
            assert_eq!(ids, ["b", "a"], "{name}");
            assert_eq!(db.unfinished_recordings().await.unwrap().len(), 2, "{name}");

            db.finish_recording("a", 1_500, 4_096).await.unwrap();
            let a = db.recording("a").await.unwrap();
            assert_eq!((a.ended_at, a.size_bytes), (Some(1_500), 4_096), "{name}");
            assert!(db.recording("b").await.unwrap().include_keys, "{name}");
            assert_eq!(db.unfinished_recordings().await.unwrap().len(), 1, "{name}");

            db.delete_recording("a").await.unwrap();
            assert!(
                matches!(db.recording("a").await, Err(DbError::NotFound)),
                "{name}"
            );
            assert!(
                matches!(db.delete_recording("a").await, Err(DbError::NotFound)),
                "{name}"
            );
        }
    }
}
