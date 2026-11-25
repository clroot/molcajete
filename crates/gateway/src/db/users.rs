//! `molcajete_user` and `molcajete_connection_permission`.

use serde::Serialize;
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbError, DbResult, now_millis, query};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub username: String,
    pub is_admin: bool,
    pub disabled: bool,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

const USER_COLUMNS: &str = "user_id, username, is_admin, disabled, created_at, last_login_at";

impl User {
    fn from_row(row: &AnyRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("user_id")?,
            username: row.try_get("username")?,
            is_admin: row.try_get::<i64, _>("is_admin")? != 0,
            disabled: row.try_get::<i64, _>("disabled")? != 0,
            created_at: row.try_get("created_at")?,
            last_login_at: row.try_get("last_login_at")?,
        })
    }
}

pub struct NewUser<'a> {
    pub username: &'a str,
    pub password_hash: &'a str,
    pub is_admin: bool,
    pub disabled: bool,
}

/// Fields to change; `None` keeps the current value.
#[derive(Default)]
pub struct UserUpdate<'a> {
    pub username: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub is_admin: Option<bool>,
    pub disabled: Option<bool>,
}

impl Database {
    pub async fn count_users(&self) -> DbResult<i64> {
        let row = query!(self, "SELECT COUNT(*) AS n FROM molcajete_user")
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok(row.try_get("n")?)
    }

    pub async fn list_users(&self) -> DbResult<Vec<User>> {
        let sql = format!("SELECT {USER_COLUMNS} FROM molcajete_user ORDER BY username");
        let rows = query!(self, &sql).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(User::from_row).collect::<Result<_, _>>()?)
    }

    pub async fn user(&self, id: &str) -> DbResult<User> {
        let sql = format!("SELECT {USER_COLUMNS} FROM molcajete_user WHERE user_id = ?");
        let row = query!(self, &sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok(User::from_row(&row)?)
    }

    /// The user and their password hash, for logging in.
    pub async fn user_credentials(&self, username: &str) -> DbResult<(User, String)> {
        let sql =
            format!("SELECT {USER_COLUMNS}, password_hash FROM molcajete_user WHERE username = ?");
        let row = query!(self, &sql)
            .bind(username)
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok((User::from_row(&row)?, row.try_get("password_hash")?))
    }

    pub async fn password_hash(&self, user_id: &str) -> DbResult<String> {
        let row = query!(
            self,
            "SELECT password_hash FROM molcajete_user WHERE user_id = ?"
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(row.try_get("password_hash")?)
    }

    pub async fn create_user(&self, user: NewUser<'_>) -> DbResult<User> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_millis();
        query!(
            self,
            "INSERT INTO molcajete_user \
             (user_id, username, password_hash, is_admin, disabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&id)
        .bind(user.username)
        .bind(user.password_hash)
        .bind(i64::from(user.is_admin))
        .bind(i64::from(user.disabled))
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        self.user(&id).await
    }

    pub async fn update_user(&self, id: &str, update: UserUpdate<'_>) -> DbResult<User> {
        let current = self.user(id).await?;
        let mut tx = self.pool.begin().await?;
        let result = query!(
            self,
            "UPDATE molcajete_user SET username = ?, is_admin = ?, disabled = ?, updated_at = ? \
             WHERE user_id = ?"
        )
        .bind(update.username.unwrap_or(&current.username))
        .bind(i64::from(update.is_admin.unwrap_or(current.is_admin)))
        .bind(i64::from(update.disabled.unwrap_or(current.disabled)))
        .bind(now_millis())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from_sqlx)?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        if let Some(hash) = update.password_hash {
            query!(
                self,
                "UPDATE molcajete_user SET password_hash = ? WHERE user_id = ?"
            )
            .bind(hash)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            // A new password signs the user out everywhere.
            query!(self, "DELETE FROM molcajete_session WHERE user_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        if update.disabled == Some(true) {
            query!(self, "DELETE FROM molcajete_session WHERE user_id = ?")
                .bind(id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.user(id).await
    }

    pub async fn record_login(&self, id: &str) -> DbResult<()> {
        query!(
            self,
            "UPDATE molcajete_user SET last_login_at = ? WHERE user_id = ?"
        )
        .bind(now_millis())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn delete_user(&self, id: &str) -> DbResult<()> {
        let result = query!(self, "DELETE FROM molcajete_user WHERE user_id = ?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        Ok(())
    }

    /// Enabled administrators other than `except`.
    pub async fn count_other_admins(&self, except: &str) -> DbResult<i64> {
        let row = query!(
            self,
            "SELECT COUNT(*) AS n FROM molcajete_user \
             WHERE is_admin = 1 AND disabled = 0 AND user_id <> ?"
        )
        .bind(except)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get("n")?)
    }

    /// Ids of the connections granted to the user.
    pub async fn user_connection_ids(&self, user_id: &str) -> DbResult<Vec<String>> {
        let rows = query!(
            self,
            "SELECT connection_id FROM molcajete_connection_permission \
             WHERE user_id = ? ORDER BY connection_id"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get("connection_id"))
            .collect::<Result<_, _>>()?)
    }

    /// Replaces the user's connection permissions. Unknown ids are ignored.
    pub async fn set_user_connections(&self, user_id: &str, ids: &[String]) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        query!(
            self,
            "DELETE FROM molcajete_connection_permission WHERE user_id = ?"
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
        let mut unique: Vec<&String> = ids.iter().collect();
        unique.sort();
        unique.dedup();
        for id in unique {
            query!(
                self,
                "INSERT INTO molcajete_connection_permission (user_id, connection_id) \
                 SELECT ?, connection_id FROM molcajete_connection WHERE connection_id = ?"
            )
            .bind(user_id)
            .bind(id.as_str())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::every_backend;
    use super::*;

    fn new_user<'a>(username: &'a str, is_admin: bool) -> NewUser<'a> {
        NewUser {
            username,
            password_hash: "hash",
            is_admin,
            disabled: false,
        }
    }

    #[tokio::test]
    async fn user_lifecycle() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let admin = db.create_user(new_user("admin", true)).await.unwrap();
            assert!(admin.is_admin && !admin.disabled, "{name}");
            assert!(
                matches!(
                    db.create_user(new_user("admin", false)).await,
                    Err(DbError::Conflict)
                ),
                "{name}"
            );

            let bob = db.create_user(new_user("bob", false)).await.unwrap();
            assert_eq!(db.count_users().await.unwrap(), 2, "{name}");
            assert_eq!(db.count_other_admins(&bob.id).await.unwrap(), 1, "{name}");
            assert_eq!(db.count_other_admins(&admin.id).await.unwrap(), 0, "{name}");

            let bob = db
                .update_user(
                    &bob.id,
                    UserUpdate {
                        username: Some("robert"),
                        password_hash: Some("new-hash"),
                        disabled: Some(true),
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                (bob.username.as_str(), bob.disabled, bob.is_admin),
                ("robert", true, false),
                "{name}"
            );
            let (_, hash) = db.user_credentials("robert").await.unwrap();
            assert_eq!(hash, "new-hash", "{name}");

            db.record_login(&bob.id).await.unwrap();
            assert!(
                db.user(&bob.id).await.unwrap().last_login_at.is_some(),
                "{name}"
            );

            let names: Vec<_> = db
                .list_users()
                .await
                .unwrap()
                .into_iter()
                .map(|u| u.username)
                .collect();
            assert_eq!(names, ["admin", "robert"], "{name}");

            db.delete_user(&bob.id).await.unwrap();
            assert!(
                matches!(db.user(&bob.id).await, Err(DbError::NotFound)),
                "{name}"
            );
            assert!(
                matches!(db.delete_user(&bob.id).await, Err(DbError::NotFound)),
                "{name}"
            );
        }
    }
}
