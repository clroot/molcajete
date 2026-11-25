//! `molcajete_connection` and `molcajete_connection_parameter`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbError, DbResult, now_millis, query};

/// A connection with all of its parameters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionRecord {
    pub id: String,
    pub name: String,
    /// guacd protocol plugin: `vnc`, `rdp`, `ssh`, `telnet`, `kubernetes`.
    pub protocol: String,
    /// guacd (or molcajeted) to use instead of the gateway's default.
    #[serde(default)]
    pub guacd_host: Option<String>,
    #[serde(default)]
    pub guacd_port: Option<u16>,
    /// Protocol parameters sent in the `connect` instruction.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

/// What the connection list shows; no secrets.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConnectionSummary {
    pub id: String,
    pub name: String,
    pub protocol: String,
    pub hostname: Option<String>,
    pub port: Option<String>,
    pub guacd_host: Option<String>,
    pub guacd_port: Option<u16>,
}

fn guacd_port(row: &AnyRow) -> Result<Option<u16>, sqlx::Error> {
    Ok(row
        .try_get::<Option<i64>, _>("guacd_port")?
        .and_then(|p| u16::try_from(p).ok()))
}

impl Database {
    /// Connections visible to a user: all of them for administrators,
    /// otherwise those granted to `user_id`.
    pub async fn connection_summaries(
        &self,
        user_id: &str,
        is_admin: bool,
    ) -> DbResult<Vec<ConnectionSummary>> {
        let select = "SELECT c.connection_id, c.name, c.protocol, c.guacd_host, c.guacd_port, \
                      h.parameter_value AS hostname, p.parameter_value AS port \
                      FROM molcajete_connection c \
                      LEFT JOIN molcajete_connection_parameter h \
                        ON h.connection_id = c.connection_id AND h.parameter_name = 'hostname' \
                      LEFT JOIN molcajete_connection_parameter p \
                        ON p.connection_id = c.connection_id AND p.parameter_name = 'port'";
        let rows = if is_admin {
            let sql = format!("{select} ORDER BY c.name, c.connection_id");
            query!(self, &sql).fetch_all(&self.pool).await?
        } else {
            let sql = format!(
                "{select} JOIN molcajete_connection_permission m \
                   ON m.connection_id = c.connection_id AND m.user_id = ? \
                 ORDER BY c.name, c.connection_id"
            );
            query!(self, &sql)
                .bind(user_id)
                .fetch_all(&self.pool)
                .await?
        };
        rows.iter()
            .map(|row| {
                Ok(ConnectionSummary {
                    id: row.try_get("connection_id")?,
                    name: row.try_get("name")?,
                    protocol: row.try_get("protocol")?,
                    hostname: row.try_get("hostname")?,
                    port: row.try_get("port")?,
                    guacd_host: row.try_get("guacd_host")?,
                    guacd_port: guacd_port(row)?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    pub async fn connection(&self, id: &str) -> DbResult<ConnectionRecord> {
        let row = query!(
            self,
            "SELECT connection_id, name, protocol, guacd_host, guacd_port \
             FROM molcajete_connection WHERE connection_id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        let params = query!(
            self,
            "SELECT parameter_name, parameter_value FROM molcajete_connection_parameter \
             WHERE connection_id = ?"
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        Ok(ConnectionRecord {
            id: row.try_get("connection_id")?,
            name: row.try_get("name")?,
            protocol: row.try_get("protocol")?,
            guacd_host: row.try_get("guacd_host")?,
            guacd_port: guacd_port(&row)?,
            parameters: params
                .iter()
                .map(|r| Ok((r.try_get("parameter_name")?, r.try_get("parameter_value")?)))
                .collect::<Result<_, sqlx::Error>>()?,
        })
    }

    /// The connection, if the user may open it.
    pub async fn connection_for_user(
        &self,
        id: &str,
        user_id: &str,
        is_admin: bool,
    ) -> DbResult<ConnectionRecord> {
        if !is_admin {
            let row = query!(
                self,
                "SELECT COUNT(*) AS n FROM molcajete_connection_permission \
                 WHERE user_id = ? AND connection_id = ?"
            )
            .bind(user_id)
            .bind(id)
            .fetch_one(&self.pool)
            .await?;
            if row.try_get::<i64, _>("n")? == 0 {
                return Err(DbError::NotFound);
            }
        }
        self.connection(id).await
    }

    pub async fn connection_exists(&self, id: &str) -> DbResult<bool> {
        let row = query!(
            self,
            "SELECT COUNT(*) AS n FROM molcajete_connection WHERE connection_id = ?"
        )
        .bind(id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get::<i64, _>("n")? > 0)
    }

    pub async fn create_connection(&self, c: &ConnectionRecord) -> DbResult<ConnectionRecord> {
        let now = now_millis();
        let mut tx = self.pool.begin().await?;
        query!(
            self,
            "INSERT INTO molcajete_connection \
             (connection_id, name, protocol, guacd_host, guacd_port, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&c.id)
        .bind(&c.name)
        .bind(&c.protocol)
        .bind(c.guacd_host.as_deref())
        .bind(c.guacd_port.map(i64::from))
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from_sqlx)?;
        self.insert_parameters(&mut tx, &c.id, &c.parameters)
            .await?;
        tx.commit().await?;
        self.connection(&c.id).await
    }

    /// Replaces the connection's fields and parameters. The id cannot change.
    pub async fn update_connection(&self, c: &ConnectionRecord) -> DbResult<ConnectionRecord> {
        let mut tx = self.pool.begin().await?;
        let result = query!(
            self,
            "UPDATE molcajete_connection \
             SET name = ?, protocol = ?, guacd_host = ?, guacd_port = ?, updated_at = ? \
             WHERE connection_id = ?"
        )
        .bind(&c.name)
        .bind(&c.protocol)
        .bind(c.guacd_host.as_deref())
        .bind(c.guacd_port.map(i64::from))
        .bind(now_millis())
        .bind(&c.id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        query!(
            self,
            "DELETE FROM molcajete_connection_parameter WHERE connection_id = ?"
        )
        .bind(&c.id)
        .execute(&mut *tx)
        .await?;
        self.insert_parameters(&mut tx, &c.id, &c.parameters)
            .await?;
        tx.commit().await?;
        self.connection(&c.id).await
    }

    async fn insert_parameters(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        id: &str,
        parameters: &BTreeMap<String, String>,
    ) -> DbResult<()> {
        for (name, value) in parameters {
            query!(
                self,
                "INSERT INTO molcajete_connection_parameter \
                 (connection_id, parameter_name, parameter_value) VALUES (?, ?, ?)"
            )
            .bind(id)
            .bind(name.as_str())
            .bind(value.as_str())
            .execute(&mut **tx)
            .await?;
        }
        Ok(())
    }

    pub async fn delete_connection(&self, id: &str) -> DbResult<()> {
        let result = query!(
            self,
            "DELETE FROM molcajete_connection WHERE connection_id = ?"
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
    use super::super::NewUser;
    use super::super::tests::every_backend;
    use super::*;

    fn record(id: &str, name: &str) -> ConnectionRecord {
        ConnectionRecord {
            id: id.into(),
            name: name.into(),
            protocol: "vnc".into(),
            guacd_host: None,
            guacd_port: None,
            parameters: BTreeMap::from([
                ("hostname".into(), "10.0.0.5".into()),
                ("port".into(), "5900".into()),
                ("password".into(), "secret".into()),
            ]),
        }
    }

    #[tokio::test]
    async fn connection_lifecycle_and_permissions() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let desktop = db
                .create_connection(&record("desktop", "Desktop"))
                .await
                .unwrap();
            assert_eq!(desktop, record("desktop", "Desktop"), "{name}");
            assert!(
                matches!(
                    db.create_connection(&record("desktop", "Again")).await,
                    Err(DbError::Conflict)
                ),
                "{name}"
            );
            db.create_connection(&record("another", "Another"))
                .await
                .unwrap();

            let mut changed = record("desktop", "Desktop 2");
            changed.guacd_host = Some("127.0.0.1".into());
            changed.guacd_port = Some(4823);
            changed.parameters.remove("password");
            assert_eq!(
                db.update_connection(&changed).await.unwrap(),
                changed,
                "{name}"
            );
            assert!(
                matches!(
                    db.update_connection(&record("missing", "x")).await,
                    Err(DbError::NotFound)
                ),
                "{name}"
            );

            let user = db
                .create_user(NewUser {
                    username: "u",
                    password_hash: "h",
                    is_admin: false,
                    disabled: false,
                })
                .await
                .unwrap();
            assert!(
                db.connection_summaries(&user.id, false)
                    .await
                    .unwrap()
                    .is_empty(),
                "{name}"
            );
            assert_eq!(
                db.connection_summaries(&user.id, true).await.unwrap().len(),
                2,
                "{name}"
            );

            db.set_user_connections(
                &user.id,
                &["desktop".into(), "desktop".into(), "missing".into()],
            )
            .await
            .unwrap();
            assert_eq!(
                db.user_connection_ids(&user.id).await.unwrap(),
                ["desktop"],
                "{name}"
            );
            let list = db.connection_summaries(&user.id, false).await.unwrap();
            assert_eq!(list.len(), 1, "{name}");
            assert_eq!(list[0].hostname.as_deref(), Some("10.0.0.5"), "{name}");
            assert_eq!(list[0].port.as_deref(), Some("5900"), "{name}");
            assert_eq!(list[0].guacd_port, Some(4823), "{name}");
            assert!(
                db.connection_for_user("desktop", &user.id, false)
                    .await
                    .is_ok(),
                "{name}"
            );
            assert!(
                matches!(
                    db.connection_for_user("another", &user.id, false).await,
                    Err(DbError::NotFound)
                ),
                "{name}"
            );
            assert!(
                db.connection_for_user("another", &user.id, true)
                    .await
                    .is_ok(),
                "{name}"
            );

            db.delete_connection("desktop").await.unwrap();
            assert!(
                db.user_connection_ids(&user.id).await.unwrap().is_empty(),
                "{name}"
            );
            assert!(!db.connection_exists("desktop").await.unwrap(), "{name}");
        }
    }
}
