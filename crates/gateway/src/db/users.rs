//! `molcajete_user` and roles.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::grants::Principal;
use super::{Database, DbError, DbResult, now_millis, query};

/// What a user may do, from least to most. Stored per user and per group;
/// a user's effective role is the highest of their own and their groups'.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    /// Opens the connections granted to them (directly, through a group or
    /// through a folder).
    #[default]
    User,
    /// Also sees and opens every connection, creates, edits, moves and
    /// deletes connections and folders, and grants them to users and
    /// groups. Cannot manage users, groups or roles.
    ConnectionManager,
    /// Everything.
    Admin,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ConnectionManager => "connection_manager",
            Self::Admin => "admin",
        }
    }

    /// The role stored as `value`; unknown values grant nothing more than
    /// [`Role::User`].
    pub fn from_db(value: &str) -> Self {
        match value {
            "admin" => Self::Admin,
            "connection_manager" => Self::ConnectionManager,
            _ => Self::User,
        }
    }

    pub fn can_manage_connections(self) -> bool {
        self >= Self::ConnectionManager
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct User {
    pub id: String,
    pub username: String,
    /// The user's own role.
    pub role: Role,
    /// The highest of the user's own role and their groups' roles: what
    /// the gateway checks.
    pub effective_role: Role,
    /// `effective_role == Admin`, for clients from before roles.
    pub is_admin: bool,
    pub disabled: bool,
    pub created_at: i64,
    pub last_login_at: Option<i64>,
}

const USER_COLUMNS: &str = "user_id, username, role, disabled, created_at, last_login_at";

impl User {
    /// The user in `row`, given the highest role of their groups.
    fn from_row(row: &AnyRow, group_role: Role) -> Result<Self, sqlx::Error> {
        let role = Role::from_db(&row.try_get::<String, _>("role")?);
        let effective_role = role.max(group_role);
        Ok(Self {
            id: row.try_get("user_id")?,
            username: row.try_get("username")?,
            role,
            effective_role,
            is_admin: effective_role == Role::Admin,
            disabled: row.try_get::<i64, _>("disabled")? != 0,
            created_at: row.try_get("created_at")?,
            last_login_at: row.try_get("last_login_at")?,
        })
    }
}

pub struct NewUser<'a> {
    pub username: &'a str,
    pub password_hash: &'a str,
    pub role: Role,
    pub disabled: bool,
}

/// Fields to change; `None` keeps the current value.
#[derive(Default)]
pub struct UserUpdate<'a> {
    pub username: Option<&'a str>,
    pub password_hash: Option<&'a str>,
    pub role: Option<Role>,
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

    /// The highest group role of each user in a group, or of `user_id`.
    async fn group_roles(&self, user_id: Option<&str>) -> DbResult<HashMap<String, Role>> {
        let select = "SELECT m.user_id, g.role FROM molcajete_group_member m \
                      JOIN molcajete_group g ON g.group_id = m.group_id";
        let rows = match user_id {
            Some(id) => {
                let sql = format!("{select} WHERE m.user_id = ?");
                query!(self, &sql).bind(id).fetch_all(&self.pool).await?
            }
            None => query!(self, select).fetch_all(&self.pool).await?,
        };
        let mut roles = HashMap::new();
        for row in &rows {
            let role = Role::from_db(&row.try_get::<String, _>("role")?);
            let entry = roles
                .entry(row.try_get::<String, _>("user_id")?)
                .or_default();
            *entry = role.max(*entry);
        }
        Ok(roles)
    }

    pub async fn list_users(&self) -> DbResult<Vec<User>> {
        let group_roles = self.group_roles(None).await?;
        let sql = format!("SELECT {USER_COLUMNS} FROM molcajete_user ORDER BY username");
        let rows = query!(self, &sql).fetch_all(&self.pool).await?;
        let users = rows
            .iter()
            .map(|row| {
                let id: String = row.try_get("user_id")?;
                User::from_row(row, group_roles.get(&id).copied().unwrap_or_default())
            })
            .collect::<Result<_, sqlx::Error>>()?;
        Ok(users)
    }

    async fn user_from_row(&self, row: &AnyRow) -> DbResult<User> {
        let id: String = row.try_get("user_id")?;
        let group_role = self
            .group_roles(Some(&id))
            .await?
            .remove(&id)
            .unwrap_or_default();
        Ok(User::from_row(row, group_role)?)
    }

    pub async fn user(&self, id: &str) -> DbResult<User> {
        let sql = format!("SELECT {USER_COLUMNS} FROM molcajete_user WHERE user_id = ?");
        let row = query!(self, &sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        self.user_from_row(&row).await
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
        Ok((
            self.user_from_row(&row).await?,
            row.try_get("password_hash")?,
        ))
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
             (user_id, username, password_hash, role, disabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&id)
        .bind(user.username)
        .bind(user.password_hash)
        .bind(user.role.as_str())
        .bind(i64::from(user.disabled))
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        self.user(&id).await
    }

    /// Fails with [`DbError::LastAdmin`] when the change would leave no
    /// enabled administrator.
    pub async fn update_user(&self, id: &str, update: UserUpdate<'_>) -> DbResult<User> {
        let current = self.user(id).await?;
        let mut tx = self.pool.begin().await?;
        let admins = self.count_admins(&mut tx).await?;
        let result = query!(
            self,
            "UPDATE molcajete_user SET username = ?, role = ?, disabled = ?, updated_at = ? \
             WHERE user_id = ?"
        )
        .bind(update.username.unwrap_or(&current.username))
        .bind(update.role.unwrap_or(current.role).as_str())
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
        self.commit_unless_last_admin(tx, admins).await?;
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

    /// Fails with [`DbError::LastAdmin`] for the last enabled administrator.
    pub async fn delete_user(&self, id: &str) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        let admins = self.count_admins(&mut tx).await?;
        let result = query!(self, "DELETE FROM molcajete_user WHERE user_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            tx.rollback().await?;
            return Err(DbError::NotFound);
        }
        self.commit_unless_last_admin(tx, admins).await?;
        Ok(())
    }

    /// Enabled users whose effective role is administrator.
    pub(super) async fn count_admins(&self, conn: &mut sqlx::AnyConnection) -> DbResult<i64> {
        let row = query!(
            self,
            "SELECT COUNT(*) AS n FROM molcajete_user u WHERE u.disabled = 0 AND \
             (u.role = 'admin' OR EXISTS (SELECT 1 FROM molcajete_group_member m \
              JOIN molcajete_group g ON g.group_id = m.group_id \
              WHERE m.user_id = u.user_id AND g.role = 'admin'))"
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(row.try_get("n")?)
    }

    /// Commits `tx` unless it took away the last enabled administrator
    /// (there were `before`); then it rolls back and fails with
    /// [`DbError::LastAdmin`].
    pub(super) async fn commit_unless_last_admin(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Any>,
        before: i64,
    ) -> DbResult<()> {
        if before > 0 && self.count_admins(&mut tx).await? == 0 {
            // Roll back now rather than on drop, so that the write lock
            // (SQLite) is gone before the caller's next query.
            tx.rollback().await?;
            return Err(DbError::LastAdmin);
        }
        tx.commit().await?;
        Ok(())
    }

    /// Ids of the connections granted to the user directly.
    pub async fn user_connection_ids(&self, user_id: &str) -> DbResult<Vec<String>> {
        Ok(self.grants(Principal::User, user_id).await?.connections)
    }

    /// Replaces the user's direct connection grants. Unknown ids are ignored.
    pub async fn set_user_connections(&self, user_id: &str, ids: &[String]) -> DbResult<()> {
        self.set_granted_connections(Principal::User, user_id, ids)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::every_backend;
    use super::*;

    fn new_user(username: &str, role: Role) -> NewUser<'_> {
        NewUser {
            username,
            password_hash: "hash",
            role,
            disabled: false,
        }
    }

    #[test]
    fn roles_are_ordered() {
        assert!(Role::Admin > Role::ConnectionManager);
        assert!(Role::ConnectionManager > Role::User);
        assert!(Role::ConnectionManager.can_manage_connections());
        assert!(!Role::User.can_manage_connections());
        for role in [Role::User, Role::ConnectionManager, Role::Admin] {
            assert_eq!(Role::from_db(role.as_str()), role);
        }
        assert_eq!(Role::from_db("superuser"), Role::User);
    }

    #[tokio::test]
    async fn user_lifecycle() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let admin = db
                .create_user(new_user("admin", Role::Admin))
                .await
                .unwrap();
            assert!(admin.is_admin && !admin.disabled, "{name}");
            assert!(
                matches!(
                    db.create_user(new_user("admin", Role::User)).await,
                    Err(DbError::Conflict)
                ),
                "{name}"
            );

            let bob = db.create_user(new_user("bob", Role::User)).await.unwrap();
            assert_eq!(db.count_users().await.unwrap(), 2, "{name}");

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

            // The last enabled administrator stays.
            for update in [
                UserUpdate {
                    role: Some(Role::ConnectionManager),
                    ..Default::default()
                },
                UserUpdate {
                    disabled: Some(true),
                    ..Default::default()
                },
            ] {
                assert!(
                    matches!(
                        db.update_user(&admin.id, update).await,
                        Err(DbError::LastAdmin)
                    ),
                    "{name}"
                );
            }
            assert!(
                matches!(db.delete_user(&admin.id).await, Err(DbError::LastAdmin)),
                "{name}"
            );
            assert!(db.user(&admin.id).await.unwrap().is_admin, "{name}");

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
