//! Grants: which users and groups may open which connections and folders.
//!
//! A user reaches a connection when it is granted to them or to one of
//! their groups, or when it sits anywhere below a folder granted to them or
//! to one of their groups. One table per (principal, target) pair, as in
//! Guacamole's JDBC schema:
//!
//! | | connection | folder |
//! |---|---|---|
//! | user | `molcajete_connection_permission` | `molcajete_connection_group_permission` |
//! | group | `molcajete_group_connection_permission` | `molcajete_group_connection_group_permission` |

use serde::{Deserialize, Serialize};
use sqlx::Row as _;

use super::{Database, DbResult, query};

/// Who a grant is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Principal {
    User,
    Group,
}

impl Principal {
    fn table(self) -> &'static str {
        match self {
            Self::User => "molcajete_user",
            Self::Group => "molcajete_group",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::User => "user_id",
            Self::Group => "group_id",
        }
    }
}

/// What a grant is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Connection,
    Folder,
}

impl Target {
    fn table(self) -> &'static str {
        match self {
            Self::Connection => "molcajete_connection",
            Self::Folder => "molcajete_connection_group",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Connection => "connection_id",
            Self::Folder => "connection_group_id",
        }
    }
}

fn grant_table(principal: Principal, target: Target) -> &'static str {
    match (principal, target) {
        (Principal::User, Target::Connection) => "molcajete_connection_permission",
        (Principal::User, Target::Folder) => "molcajete_connection_group_permission",
        (Principal::Group, Target::Connection) => "molcajete_group_connection_permission",
        (Principal::Group, Target::Folder) => "molcajete_group_connection_group_permission",
    }
}

/// What a user or a group is granted directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Grants {
    /// Connection ids.
    pub connections: Vec<String>,
    /// Folder ids; each covers everything below it.
    pub folders: Vec<String>,
}

/// Who is granted a connection or a folder directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Access {
    /// User ids.
    pub users: Vec<String>,
    /// Group ids.
    pub groups: Vec<String>,
}

/// `WITH` clause defining `reachable (connection_id)`: the connections the
/// user bound to its three placeholders may open through grants.
pub(super) const REACHABLE: &str = "WITH RECURSIVE \
    my_groups (group_id) AS ( \
      SELECT group_id FROM molcajete_group_member WHERE user_id = ?), \
    granted_folders (id) AS ( \
      SELECT connection_group_id FROM molcajete_connection_group_permission WHERE user_id = ? \
      UNION \
      SELECT connection_group_id FROM molcajete_group_connection_group_permission \
      WHERE group_id IN (SELECT group_id FROM my_groups)), \
    folders (id) AS ( \
      SELECT id FROM granted_folders \
      UNION \
      SELECT f.connection_group_id FROM molcajete_connection_group f \
      JOIN folders d ON f.parent_id = d.id), \
    reachable (connection_id) AS ( \
      SELECT connection_id FROM molcajete_connection_permission WHERE user_id = ? \
      UNION \
      SELECT connection_id FROM molcajete_group_connection_permission \
      WHERE group_id IN (SELECT group_id FROM my_groups) \
      UNION \
      SELECT connection_id FROM molcajete_connection \
      WHERE parent_id IN (SELECT id FROM folders)) ";

fn ids(rows: &[sqlx::any::AnyRow], column: &str) -> DbResult<Vec<String>> {
    Ok(rows
        .iter()
        .map(|r| r.try_get(column))
        .collect::<Result<_, _>>()?)
}

fn unique(ids: &[String]) -> Vec<&str> {
    let mut unique: Vec<&str> = ids.iter().map(String::as_str).collect();
    unique.sort_unstable();
    unique.dedup();
    unique
}

impl Database {
    /// Ids of the connections the user may open through any grant.
    /// Roles are not considered; see [`Database::connection_for_user`].
    pub async fn reachable_connection_ids(&self, user_id: &str) -> DbResult<Vec<String>> {
        let sql = format!("{REACHABLE} SELECT connection_id FROM reachable ORDER BY connection_id");
        let rows = query!(self, &sql)
            .bind(user_id)
            .bind(user_id)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?;
        ids(&rows, "connection_id")
    }

    async fn granted(
        &self,
        principal: Principal,
        id: &str,
        target: Target,
    ) -> DbResult<Vec<String>> {
        let sql = format!(
            "SELECT {t} FROM {table} WHERE {p} = ? ORDER BY {t}",
            t = target.column(),
            p = principal.column(),
            table = grant_table(principal, target),
        );
        let rows = query!(self, &sql).bind(id).fetch_all(&self.pool).await?;
        ids(&rows, target.column())
    }

    /// What the user or group is granted directly.
    pub async fn grants(&self, principal: Principal, id: &str) -> DbResult<Grants> {
        Ok(Grants {
            connections: self.granted(principal, id, Target::Connection).await?,
            folders: self.granted(principal, id, Target::Folder).await?,
        })
    }

    /// Replaces one side of the grants between `principal` and `target`:
    /// with `fixed` = the principal's id, what it is granted; with `fixed`
    /// = the target's id, who is granted it. Unknown ids are ignored.
    async fn replace_grants(
        &self,
        principal: Principal,
        target: Target,
        fixed_is_principal: bool,
        fixed: &str,
        others: &[String],
    ) -> DbResult<()> {
        let table = grant_table(principal, target);
        let (p, t) = (principal.column(), target.column());
        let fixed_column = if fixed_is_principal { p } else { t };
        let mut tx = self.pool.begin().await?;
        let sql = format!("DELETE FROM {table} WHERE {fixed_column} = ?");
        query!(self, &sql).bind(fixed).execute(&mut *tx).await?;
        let insert = format!(
            "INSERT INTO {table} ({p}, {t}) SELECT a.{p}, b.{t} FROM {pt} a, {tt} b \
             WHERE a.{p} = ? AND b.{t} = ?",
            pt = principal.table(),
            tt = target.table(),
        );
        for other in unique(others) {
            let (principal_id, target_id) = if fixed_is_principal {
                (fixed, other)
            } else {
                (other, fixed)
            };
            query!(self, &insert)
                .bind(principal_id)
                .bind(target_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Replaces the connections granted directly to the user or group.
    pub async fn set_granted_connections(
        &self,
        principal: Principal,
        id: &str,
        connection_ids: &[String],
    ) -> DbResult<()> {
        self.replace_grants(principal, Target::Connection, true, id, connection_ids)
            .await
    }

    /// Replaces the folders granted directly to the user or group.
    pub async fn set_granted_folders(
        &self,
        principal: Principal,
        id: &str,
        folder_ids: &[String],
    ) -> DbResult<()> {
        self.replace_grants(principal, Target::Folder, true, id, folder_ids)
            .await
    }

    /// The users and groups granted the connection or folder directly.
    pub async fn access(&self, target: Target, id: &str) -> DbResult<Access> {
        let mut access = Access::default();
        for principal in [Principal::User, Principal::Group] {
            let sql = format!(
                "SELECT {p} FROM {table} WHERE {t} = ? ORDER BY {p}",
                p = principal.column(),
                t = target.column(),
                table = grant_table(principal, target),
            );
            let rows = query!(self, &sql).bind(id).fetch_all(&self.pool).await?;
            let found = ids(&rows, principal.column())?;
            match principal {
                Principal::User => access.users = found,
                Principal::Group => access.groups = found,
            }
        }
        Ok(access)
    }

    /// Replaces who is granted the connection or folder directly.
    pub async fn set_access(&self, target: Target, id: &str, access: &Access) -> DbResult<()> {
        self.replace_grants(Principal::User, target, false, id, &access.users)
            .await?;
        self.replace_grants(Principal::Group, target, false, id, &access.groups)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::tests::every_backend;
    use super::super::{ConnectionRecord, DbError, GroupUpdate, NewUser, Role};
    use super::*;

    async fn connection(db: &Database, id: &str, parent: Option<&str>) {
        db.create_connection(&ConnectionRecord {
            id: id.into(),
            name: id.into(),
            protocol: "vnc".into(),
            guacd_host: None,
            guacd_port: None,
            parameters: BTreeMap::new(),
            parent_id: parent.map(str::to_owned),
        })
        .await
        .unwrap();
    }

    async fn user(db: &Database, name: &str, role: Role) -> String {
        db.create_user(NewUser {
            username: name,
            password_hash: "h",
            role,
            disabled: false,
        })
        .await
        .unwrap()
        .id
    }

    async fn reaches(db: &Database, user_id: &str, connection_id: &str) -> bool {
        db.reachable_connection_ids(user_id)
            .await
            .unwrap()
            .iter()
            .any(|id| id == connection_id)
    }

    #[tokio::test]
    async fn groups_and_folders_grant_access() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let admin = user(&db, "admin", Role::Admin).await;
            let u = user(&db, "u", Role::User).await;

            // Databases (granted to the group) holds pg and Prod/mysql;
            // Desktops holds rdp; own, shared and loose are at the top.
            let databases = db.create_folder("Databases", None).await.unwrap();
            let prod = db.create_folder("Prod", Some(&databases.id)).await.unwrap();
            let desktops = db.create_folder("Desktops", None).await.unwrap();
            connection(&db, "pg", Some(&databases.id)).await;
            connection(&db, "mysql", Some(&prod.id)).await;
            connection(&db, "rdp", Some(&desktops.id)).await;
            connection(&db, "own", None).await;
            connection(&db, "shared", None).await;
            connection(&db, "loose", None).await;

            let team = db.create_group("db-team", Role::User).await.unwrap();
            db.set_group_members(&team.id, &[u.clone(), "missing".into()])
                .await
                .unwrap();
            db.set_granted_folders(
                Principal::Group,
                &team.id,
                std::slice::from_ref(&databases.id),
            )
            .await
            .unwrap();
            db.set_granted_connections(Principal::Group, &team.id, &["shared".into()])
                .await
                .unwrap();
            db.set_user_connections(&u, &["own".into()]).await.unwrap();

            assert_eq!(
                db.reachable_connection_ids(&u).await.unwrap(),
                ["mysql", "own", "pg", "shared"],
                "{name}"
            );
            assert_eq!(db.group(&team.id).await.unwrap().member_count, 1, "{name}");
            assert_eq!(
                db.access(Target::Folder, &databases.id).await.unwrap(),
                Access {
                    users: vec![],
                    groups: vec![team.id.clone()]
                },
                "{name}"
            );
            db.set_access(
                Target::Connection,
                "rdp",
                &Access {
                    users: vec![u.clone(), "missing".into()],
                    groups: vec![team.id.clone()],
                },
            )
            .await
            .unwrap();
            assert_eq!(
                db.access(Target::Connection, "rdp").await.unwrap(),
                Access {
                    users: vec![u.clone()],
                    groups: vec![team.id.clone()]
                },
                "{name}"
            );
            db.set_access(Target::Connection, "rdp", &Access::default())
                .await
                .unwrap();

            // Moving a connection into a granted folder grants it; moving
            // it out takes it away. So does moving a folder.
            db.move_connection("loose", Some(&prod.id)).await.unwrap();
            assert!(reaches(&db, &u, "loose").await, "{name}");
            db.move_connection("loose", None).await.unwrap();
            assert!(!reaches(&db, &u, "loose").await, "{name}");
            db.move_folder(&desktops.id, Some(&databases.id))
                .await
                .unwrap();
            assert!(reaches(&db, &u, "rdp").await, "{name}");
            db.move_folder(&desktops.id, None).await.unwrap();
            assert!(!reaches(&db, &u, "rdp").await, "{name}");
            assert!(
                matches!(
                    db.move_folder(&databases.id, Some(&prod.id)).await,
                    Err(DbError::Invalid(_))
                ),
                "{name}"
            );
            assert!(
                matches!(
                    db.move_connection("pg", Some("missing")).await,
                    Err(DbError::Invalid(_))
                ),
                "{name}"
            );

            // Leaving the group keeps the user's own grants.
            db.set_user_groups(&u, &[]).await.unwrap();
            assert_eq!(
                db.reachable_connection_ids(&u).await.unwrap(),
                ["own"],
                "{name}"
            );

            // Group names from an identity provider.
            let groups = db
                .sync_user_groups_by_name(&u, &["db-team".into(), "idp-only".into()], false)
                .await
                .unwrap();
            assert_eq!(groups.len(), 1, "{name}");
            assert_eq!(
                db.reachable_connection_ids(&u).await.unwrap().len(),
                4,
                "{name}"
            );
            let groups = db
                .sync_user_groups_by_name(&u, &[" idp-only ".into(), String::new()], true)
                .await
                .unwrap();
            assert_eq!(
                groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
                ["idp-only"],
                "{name}"
            );
            assert_eq!(groups[0].role, Role::User, "{name}");
            assert_eq!(
                db.reachable_connection_ids(&u).await.unwrap(),
                ["own"],
                "{name}"
            );

            // A group's role counts for its members, who can be the last
            // administrators.
            let admins = db.create_group("admins", Role::Admin).await.unwrap();
            db.set_group_members(&admins.id, std::slice::from_ref(&u))
                .await
                .unwrap();
            let promoted = db.user(&u).await.unwrap();
            assert_eq!(
                (promoted.role, promoted.effective_role, promoted.is_admin),
                (Role::User, Role::Admin, true),
                "{name}"
            );
            db.delete_user(&admin).await.unwrap();
            let demote = GroupUpdate {
                role: Some(Role::ConnectionManager),
                ..Default::default()
            };
            for result in [
                db.set_group_members(&admins.id, &[]).await,
                db.set_user_groups(&u, &[]).await,
                db.delete_group(&admins.id).await,
                db.sync_user_groups_by_name(&u, &[], false)
                    .await
                    .map(|_| ()),
                db.update_group(&admins.id, demote).await.map(|_| ()),
            ] {
                assert!(
                    matches!(result, Err(DbError::LastAdmin)),
                    "{name} {result:?}"
                );
            }
            assert!(db.user(&u).await.unwrap().is_admin, "{name}");

            // Deleting a folder moves what it holds up and drops its grants.
            db.delete_folder(&databases.id).await.unwrap();
            assert_eq!(db.folder(&prod.id).await.unwrap().parent_id, None, "{name}");
            assert_eq!(db.connection("pg").await.unwrap().parent_id, None, "{name}");
            assert!(
                db.grants(Principal::Group, &team.id)
                    .await
                    .unwrap()
                    .folders
                    .is_empty(),
                "{name}"
            );
        }
    }
}
