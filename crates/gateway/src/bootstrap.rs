//! Startup seeding: users and connections from `gateway.toml`, and a first
//! administrator for an empty database.

use serde_json::json;
use tracing::warn;

use crate::audit::{self, Entry, action, target};
use crate::auth;
use crate::config::{Config, ConnectionConfig};
use crate::db::{ConnectionRecord, Database, DbError, NewUser, Role, UserUpdate};

impl ConnectionConfig {
    pub fn to_record(&self) -> ConnectionRecord {
        ConnectionRecord {
            id: self.id.clone(),
            name: self.display_name().to_owned(),
            protocol: self.protocol.clone(),
            guacd_host: self.guacd.as_ref().map(|g| g.host.clone()),
            guacd_port: self.guacd.as_ref().map(|g| g.port),
            parameters: self.parameters.clone(),
            parent_id: None,
            attributes: self.attributes.clone(),
        }
    }
}

/// Where entries of the audit log written at startup come from.
const FROM_CONFIG: &str = "configuration file";
const FROM_COMMAND_LINE: &str = "command line";

/// The audit entry of an administrator created without anyone signed in.
fn admin_created(user: &crate::db::User, source: &str) -> Entry {
    Entry::new(action::USER_CREATE)
        .source(source)
        .target(target::USER, &user.id, &user.username)
        .changes(json!({
            "username": { "from": null, "to": user.username },
            "role": { "from": null, "to": "admin" },
            "password": { "from": null, "to": crate::sensitive::REDACTED },
        }))
}

/// Imports `[[users]]` (as administrators) and `[[connections]]` that are
/// not in the database yet, then makes sure someone can log in.
pub async fn seed(db: &Database, config: &Config) -> anyhow::Result<()> {
    for user in &config.users {
        match db.user_credentials(&user.username).await {
            Ok(_) => continue,
            Err(DbError::NotFound) => {}
            Err(err) => return Err(err.into()),
        }
        let hash = auth::hash_password_blocking(user.password.clone()).await?;
        let created = db
            .create_user(NewUser {
                username: &user.username,
                password_hash: &hash,
                role: Role::Admin,
                disabled: false,
            })
            .await?;
        audit::record(db, admin_created(&created, FROM_CONFIG)).await;
    }

    for connection in &config.connections {
        if db.connection_exists(&connection.id).await? {
            continue;
        }
        let created = db.create_connection(&connection.to_record()).await?;
        let entry = Entry::new(action::CONNECTION_CREATE)
            .source(FROM_CONFIG)
            .target(target::CONNECTION, &created.id, &created.name)
            .changes(audit::diff(&serde_json::Value::Null, &json!(created)));
        audit::record(db, entry).await;
    }

    if db.count_users().await? == 0 {
        let password = auth::random_password();
        let hash = auth::hash_password_blocking(password.clone()).await?;
        let created = db
            .create_user(NewUser {
                username: "admin",
                password_hash: &hash,
                role: Role::Admin,
                disabled: false,
            })
            .await?;
        audit::record(db, admin_created(&created, "first start")).await;
        warn!(
            username = "admin",
            %password,
            "created the first administrator; sign in and change this password"
        );
    }
    Ok(())
}

/// `molcajete-gateway set-password <username>`: sets the password and makes
/// the account an enabled administrator, creating it if needed.
pub async fn set_password(db: &Database, username: &str, password: &str) -> anyhow::Result<()> {
    if password.chars().count() < auth::MIN_PASSWORD_LENGTH {
        anyhow::bail!(
            "passwords need at least {} characters",
            auth::MIN_PASSWORD_LENGTH
        );
    }
    let hash = auth::hash_password_blocking(password.to_owned()).await?;
    match db.user_credentials(username).await {
        Ok((before, _)) => {
            let after = db
                .update_user(
                    &before.id,
                    UserUpdate {
                        password_hash: Some(&hash),
                        role: Some(Role::Admin),
                        disabled: Some(false),
                        ..Default::default()
                    },
                )
                .await?;
            let snapshot = |u: &crate::db::User| json!({ "role": u.role, "disabled": u.disabled });
            let entry = Entry::new(action::USER_UPDATE)
                .source(FROM_COMMAND_LINE)
                .target(target::USER, &after.id, &after.username)
                .changes(audit::diff(&snapshot(&before), &snapshot(&after)))
                .changes(json!({ "password": {
                    "from": crate::sensitive::REDACTED,
                    "to": crate::sensitive::REDACTED,
                } }));
            audit::record(db, entry).await;
        }
        Err(DbError::NotFound) => {
            let created = db
                .create_user(NewUser {
                    username,
                    password_hash: &hash,
                    role: Role::Admin,
                    disabled: false,
                })
                .await?;
            audit::record(db, admin_created(&created, FROM_COMMAND_LINE)).await;
        }
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::tests::sqlite;

    #[tokio::test]
    async fn seeds_from_configuration_once() {
        let (db, _dir) = sqlite().await;
        let config: Config = toml::from_str(
            r#"
            [[users]]
            username = "boss"
            password = "first"

            [[connections]]
            id = "desk"
            protocol = "vnc"
            guacd = { host = "127.0.0.1", port = 4823 }
            [connections.parameters]
            hostname = "10.0.0.5"
            "#,
        )
        .unwrap();
        seed(&db, &config).await.unwrap();
        let (boss, hash) = db.user_credentials("boss").await.unwrap();
        assert!(boss.is_admin);
        assert!(auth::verify_password("first", &hash));
        let desk = db.connection("desk").await.unwrap();
        assert_eq!(desk.name, "desk");
        assert_eq!(desk.guacd_port, Some(4823));

        // Changes made in the database survive a restart with the same file.
        set_password(&db, "boss", "changed-password").await.unwrap();
        let mut renamed = desk.clone();
        renamed.name = "Renamed".into();
        db.update_connection(&renamed).await.unwrap();
        seed(&db, &config).await.unwrap();
        let (_, hash) = db.user_credentials("boss").await.unwrap();
        assert!(auth::verify_password("changed-password", &hash));
        assert_eq!(db.connection("desk").await.unwrap().name, "Renamed");
        assert_eq!(db.count_users().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn creates_a_first_administrator() {
        let (db, _dir) = sqlite().await;
        seed(&db, &toml::from_str("").unwrap()).await.unwrap();
        let (admin, _) = db.user_credentials("admin").await.unwrap();
        assert!(admin.is_admin);
        set_password(&db, "admin", "short").await.unwrap_err();
        set_password(&db, "rescue", "long enough").await.unwrap();
        assert!(db.user_credentials("rescue").await.unwrap().0.is_admin);

        let entries = db
            .list_audit_entries(&Default::default(), 10, 0)
            .await
            .unwrap();
        let seen: Vec<_> = entries
            .iter()
            .rev()
            .map(|e| {
                (
                    e.action.as_str(),
                    e.source_address.as_deref().unwrap(),
                    e.target_name.as_deref().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("user.create", "first start", "admin"),
                ("user.create", "command line", "rescue"),
            ]
        );
        assert!(entries.iter().all(|e| e.actor_id.is_none()));
    }
}
