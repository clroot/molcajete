//! `molcajete_connection_group`: folders of connections, a tree through
//! `parent_id` (NULL at the top). A grant on a folder covers everything
//! below it.

use std::collections::HashMap;

use serde::Serialize;
use sqlx::Row as _;

use super::{Database, DbError, DbResult, now_millis, query};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Folder {
    pub id: String,
    pub name: String,
    /// The folder it is in; `None` at the top level.
    pub parent_id: Option<String>,
}

impl Database {
    /// Every folder, by name.
    pub async fn list_folders(&self) -> DbResult<Vec<Folder>> {
        let rows = query!(
            self,
            "SELECT connection_group_id, name, parent_id FROM molcajete_connection_group \
             ORDER BY name, connection_group_id"
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter()
            .map(|row| {
                Ok(Folder {
                    id: row.try_get("connection_group_id")?,
                    name: row.try_get("name")?,
                    parent_id: row.try_get("parent_id")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    pub async fn folder(&self, id: &str) -> DbResult<Folder> {
        self.list_folders()
            .await?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or(DbError::NotFound)
    }

    /// Fails with [`DbError::Invalid`] when `parent_id` does not exist.
    pub async fn create_folder(&self, name: &str, parent_id: Option<&str>) -> DbResult<Folder> {
        if let Some(parent) = parent_id {
            self.require_folder(parent).await?;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_millis();
        query!(
            self,
            "INSERT INTO molcajete_connection_group \
             (connection_group_id, parent_id, name, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)"
        )
        .bind(&id)
        .bind(parent_id)
        .bind(name)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        self.folder(&id).await
    }

    pub async fn rename_folder(&self, id: &str, name: &str) -> DbResult<Folder> {
        let result = query!(
            self,
            "UPDATE molcajete_connection_group SET name = ?, updated_at = ? \
             WHERE connection_group_id = ?"
        )
        .bind(name)
        .bind(now_millis())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        self.folder(id).await
    }

    /// Moves the folder, with everything in it, into `parent_id` (`None`:
    /// the top level). Fails with [`DbError::Invalid`] when the parent does
    /// not exist or is the folder itself or inside it.
    pub async fn move_folder(&self, id: &str, parent_id: Option<&str>) -> DbResult<Folder> {
        let parents: HashMap<String, Option<String>> = self
            .list_folders()
            .await?
            .into_iter()
            .map(|f| (f.id, f.parent_id))
            .collect();
        if !parents.contains_key(id) {
            return Err(DbError::NotFound);
        }
        let mut ancestor = parent_id;
        while let Some(current) = ancestor {
            if current == id {
                return Err(DbError::Invalid(
                    "A folder cannot be moved into itself or into a folder inside it.".into(),
                ));
            }
            ancestor = parents
                .get(current)
                .ok_or_else(|| DbError::Invalid("That folder does not exist.".into()))?
                .as_deref();
        }
        query!(
            self,
            "UPDATE molcajete_connection_group SET parent_id = ?, updated_at = ? \
             WHERE connection_group_id = ?"
        )
        .bind(parent_id)
        .bind(now_millis())
        .bind(id)
        .execute(&self.pool)
        .await?;
        self.folder(id).await
    }

    /// Deletes the folder and the grants on it. What it holds, folders and
    /// connections, moves up into its parent.
    pub async fn delete_folder(&self, id: &str) -> DbResult<()> {
        let folder = self.folder(id).await?;
        let mut tx = self.pool.begin().await?;
        for table in ["molcajete_connection_group", "molcajete_connection"] {
            let sql = format!("UPDATE {table} SET parent_id = ? WHERE parent_id = ?");
            query!(self, &sql)
                .bind(folder.parent_id.as_deref())
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        query!(
            self,
            "DELETE FROM molcajete_connection_group WHERE connection_group_id = ?"
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Moves the connection into `parent_id` (`None`: the top level).
    pub async fn move_connection(&self, id: &str, parent_id: Option<&str>) -> DbResult<()> {
        if let Some(parent) = parent_id {
            self.require_folder(parent).await?;
        }
        let result = query!(
            self,
            "UPDATE molcajete_connection SET parent_id = ?, updated_at = ? \
             WHERE connection_id = ?"
        )
        .bind(parent_id)
        .bind(now_millis())
        .bind(id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// [`DbError::Invalid`] unless the folder exists.
    pub(super) async fn require_folder(&self, id: &str) -> DbResult<()> {
        match self.folder(id).await {
            Ok(_) => Ok(()),
            Err(DbError::NotFound) => Err(DbError::Invalid("That folder does not exist.".into())),
            Err(err) => Err(err),
        }
    }
}
