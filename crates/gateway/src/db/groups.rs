//! `molcajete_group` and `molcajete_group_member`: user groups, each with a
//! role and grants of its own that its members share.

use serde::Serialize;
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbError, DbResult, Role, now_millis, query};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    pub name: String,
    pub role: Role,
    pub member_count: i64,
    pub created_at: i64,
}

const GROUP_SELECT: &str = "SELECT g.group_id, g.name, g.role, g.created_at, \
     (SELECT COUNT(*) FROM molcajete_group_member m WHERE m.group_id = g.group_id) \
       AS member_count \
     FROM molcajete_group g";

impl Group {
    fn from_row(row: &AnyRow) -> Result<Self, sqlx::Error> {
        Ok(Self {
            id: row.try_get("group_id")?,
            name: row.try_get("name")?,
            role: Role::from_db(&row.try_get::<String, _>("role")?),
            member_count: row.try_get("member_count")?,
            created_at: row.try_get("created_at")?,
        })
    }
}

/// Fields to change; `None` keeps the current value.
#[derive(Default)]
pub struct GroupUpdate<'a> {
    pub name: Option<&'a str>,
    pub role: Option<Role>,
}

impl Database {
    pub async fn list_groups(&self) -> DbResult<Vec<Group>> {
        let sql = format!("{GROUP_SELECT} ORDER BY g.name");
        let rows = query!(self, &sql).fetch_all(&self.pool).await?;
        Ok(rows.iter().map(Group::from_row).collect::<Result<_, _>>()?)
    }

    pub async fn group(&self, id: &str) -> DbResult<Group> {
        let sql = format!("{GROUP_SELECT} WHERE g.group_id = ?");
        let row = query!(self, &sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok(Group::from_row(&row)?)
    }

    pub async fn create_group(&self, name: &str, role: Role) -> DbResult<Group> {
        let id = uuid::Uuid::new_v4().to_string();
        self.insert_group(&mut *self.pool.acquire().await?, &id, name, role)
            .await?;
        self.group(&id).await
    }

    async fn insert_group(
        &self,
        conn: &mut sqlx::AnyConnection,
        id: &str,
        name: &str,
        role: Role,
    ) -> DbResult<()> {
        let now = now_millis();
        query!(
            self,
            "INSERT INTO molcajete_group (group_id, name, role, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?)"
        )
        .bind(id)
        .bind(name)
        .bind(role.as_str())
        .bind(now)
        .bind(now)
        .execute(conn)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(())
    }

    /// Fails with [`DbError::LastAdmin`] when the change would leave no
    /// enabled administrator.
    pub async fn update_group(&self, id: &str, update: GroupUpdate<'_>) -> DbResult<Group> {
        let current = self.group(id).await?;
        let mut tx = self.pool.begin().await?;
        let admins = self.count_admins(&mut tx).await?;
        query!(
            self,
            "UPDATE molcajete_group SET name = ?, role = ?, updated_at = ? WHERE group_id = ?"
        )
        .bind(update.name.unwrap_or(&current.name))
        .bind(update.role.unwrap_or(current.role).as_str())
        .bind(now_millis())
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from_sqlx)?;
        self.commit_unless_last_admin(tx, admins).await?;
        self.group(id).await
    }

    /// Deletes the group with its memberships and grants. Fails with
    /// [`DbError::LastAdmin`] when its members were the last administrators.
    pub async fn delete_group(&self, id: &str) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        let admins = self.count_admins(&mut tx).await?;
        let result = query!(self, "DELETE FROM molcajete_group WHERE group_id = ?")
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

    /// Ids of the group's members.
    pub async fn group_member_ids(&self, group_id: &str) -> DbResult<Vec<String>> {
        let rows = query!(
            self,
            "SELECT user_id FROM molcajete_group_member WHERE group_id = ? ORDER BY user_id"
        )
        .bind(group_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get("user_id"))
            .collect::<Result<_, _>>()?)
    }

    /// Ids of the groups the user is a member of.
    pub async fn user_group_ids(&self, user_id: &str) -> DbResult<Vec<String>> {
        let rows = query!(
            self,
            "SELECT group_id FROM molcajete_group_member WHERE user_id = ? ORDER BY group_id"
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| r.try_get("group_id"))
            .collect::<Result<_, _>>()?)
    }

    /// Replaces the group's members. Unknown user ids are ignored. Fails
    /// with [`DbError::LastAdmin`] when that removes the last administrator.
    pub async fn set_group_members(&self, group_id: &str, user_ids: &[String]) -> DbResult<()> {
        self.replace_memberships("group_id", group_id, user_ids)
            .await
    }

    /// Replaces the groups the user is a member of. Unknown group ids are
    /// ignored. Fails with [`DbError::LastAdmin`] when that removes the
    /// last administrator.
    pub async fn set_user_groups(&self, user_id: &str, group_ids: &[String]) -> DbResult<()> {
        self.replace_memberships("user_id", user_id, group_ids)
            .await
    }

    async fn replace_memberships(
        &self,
        fixed_column: &str,
        fixed: &str,
        others: &[String],
    ) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        let admins = self.count_admins(&mut tx).await?;
        let sql = format!("DELETE FROM molcajete_group_member WHERE {fixed_column} = ?");
        query!(self, &sql).bind(fixed).execute(&mut *tx).await?;
        let mut unique: Vec<&String> = others.iter().collect();
        unique.sort();
        unique.dedup();
        for other in unique {
            let (group_id, user_id) = if fixed_column == "group_id" {
                (fixed, other.as_str())
            } else {
                (other.as_str(), fixed)
            };
            query!(
                self,
                "INSERT INTO molcajete_group_member (group_id, user_id) \
                 SELECT g.group_id, u.user_id FROM molcajete_group g, molcajete_user u \
                 WHERE g.group_id = ? AND u.user_id = ?"
            )
            .bind(group_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }
        self.commit_unless_last_admin(tx, admins).await?;
        Ok(())
    }

    /// Makes the user a member of exactly the groups called `names`, for
    /// identity providers that send group names (OIDC `groups` claims).
    ///
    /// Memberships in groups not named are removed. A name without a group
    /// is skipped, or with `create_missing` creates a group of that name
    /// with the role [`Role::User`] and no grants, which an administrator
    /// can then give a role and connections. Names are matched exactly;
    /// blank names are ignored. Returns the user's groups afterwards.
    ///
    /// Fails with [`DbError::LastAdmin`], changing nothing, when this would
    /// remove the last enabled administrator; [`DbError::NotFound`] when
    /// the user does not exist.
    pub async fn sync_user_groups_by_name(
        &self,
        user_id: &str,
        names: &[String],
        create_missing: bool,
    ) -> DbResult<Vec<Group>> {
        self.user(user_id).await?;
        let mut names: Vec<&str> = names
            .iter()
            .map(|n| n.trim())
            .filter(|n| !n.is_empty())
            .collect();
        names.sort_unstable();
        names.dedup();

        let mut tx = self.pool.begin().await?;
        let admins = self.count_admins(&mut tx).await?;
        query!(self, "DELETE FROM molcajete_group_member WHERE user_id = ?")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        for name in names {
            let existing = query!(self, "SELECT group_id FROM molcajete_group WHERE name = ?")
                .bind(name)
                .fetch_optional(&mut *tx)
                .await?;
            let group_id: String = match existing {
                Some(row) => row.try_get("group_id")?,
                None if create_missing => {
                    let id = uuid::Uuid::new_v4().to_string();
                    self.insert_group(&mut tx, &id, name, Role::User).await?;
                    id
                }
                None => continue,
            };
            query!(
                self,
                "INSERT INTO molcajete_group_member (group_id, user_id) VALUES (?, ?)"
            )
            .bind(&group_id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }
        self.commit_unless_last_admin(tx, admins).await?;

        let ids = self.user_group_ids(user_id).await?;
        let mut groups = self.list_groups().await?;
        groups.retain(|g| ids.contains(&g.id));
        Ok(groups)
    }
}
