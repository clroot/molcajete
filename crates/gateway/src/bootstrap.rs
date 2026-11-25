//! Startup seeding: users and connections from `gateway.toml`, and a first
//! administrator for an empty database.

use tracing::{info, warn};

use crate::auth;
use crate::config::{Config, ConnectionConfig};
use crate::db::{ConnectionRecord, Database, DbError, NewUser, UserUpdate};

impl ConnectionConfig {
    pub fn to_record(&self) -> ConnectionRecord {
        ConnectionRecord {
            id: self.id.clone(),
            name: self.display_name().to_owned(),
            protocol: self.protocol.clone(),
            guacd_host: self.guacd.as_ref().map(|g| g.host.clone()),
            guacd_port: self.guacd.as_ref().map(|g| g.port),
            parameters: self.parameters.clone(),
        }
    }
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
        db.create_user(NewUser {
            username: &user.username,
            password_hash: &hash,
            is_admin: true,
            disabled: false,
        })
        .await?;
        info!(username = %user.username, "created administrator from the configuration file");
    }

    for connection in &config.connections {
        if db.connection_exists(&connection.id).await? {
            continue;
        }
        db.create_connection(&connection.to_record()).await?;
        info!(connection = %connection.id, "imported connection from the configuration file");
    }

    if db.count_users().await? == 0 {
        let password = auth::random_password();
        let hash = auth::hash_password_blocking(password.clone()).await?;
        db.create_user(NewUser {
            username: "admin",
            password_hash: &hash,
            is_admin: true,
            disabled: false,
        })
        .await?;
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
        Ok((user, _)) => {
            db.update_user(
                &user.id,
                UserUpdate {
                    password_hash: Some(&hash),
                    is_admin: Some(true),
                    disabled: Some(false),
                    ..Default::default()
                },
            )
            .await?;
        }
        Err(DbError::NotFound) => {
            db.create_user(NewUser {
                username,
                password_hash: &hash,
                is_admin: true,
                disabled: false,
            })
            .await?;
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
    }
}
