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
}

const COLUMNS: &str = "recording_id, connection_id, connection_name, protocol, user_id, \
                       username, file_name, include_keys, started_at, ended_at, size_bytes";

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
    })
}

impl Database {
    pub async fn create_recording(&self, r: &Recording) -> DbResult<()> {
        query!(
            self,
            "INSERT INTO molcajete_recording \
             (recording_id, connection_id, connection_name, protocol, user_id, username, \
              file_name, include_keys, started_at, ended_at, size_bytes) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
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
