//! `molcajete_connection`, `molcajete_connection_parameter` and
//! `molcajete_connection_attribute`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::grants::REACHABLE;
use super::{Database, DbError, DbResult, User, now_millis, query};

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
    /// The folder it is in; `None` at the top level. Set when creating;
    /// [`Database::update_connection`] leaves it alone (see
    /// [`Database::move_connection`]).
    #[serde(default)]
    pub parent_id: Option<String>,
    /// molcajete's own settings of the connection ([`attributes`]), which
    /// are never sent to guacd.
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
}

/// Names of connection attributes. The flags among them are `"true"` when
/// set and absent otherwise.
pub mod attributes {
    /// Record every session of the connection.
    pub const RECORD: &str = "record-sessions";
    /// Also record key presses, which include every password typed.
    pub const RECORD_KEYS: &str = "record-keys";
    /// Leave the mouse out of recordings.
    pub const RECORD_EXCLUDE_MOUSE: &str = "record-exclude-mouse";
    /// Let molcajeted use its own image and video formats in recorded
    /// sessions, which only molcajete's player can play.
    pub const RECORD_MOLCAJETE_FORMATS: &str = "record-molcajete-formats";

    /// Every attribute the gateway knows.
    pub const ALL: &[&str] = &[
        RECORD,
        RECORD_KEYS,
        RECORD_EXCLUDE_MOUSE,
        RECORD_MOLCAJETE_FORMATS,
    ];
}

impl ConnectionRecord {
    /// Whether the flag attribute `name` is set.
    pub fn flag(&self, name: &str) -> bool {
        self.attributes.get(name).is_some_and(|v| v == "true")
    }
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
    /// The folder it is in; `None` at the top level.
    pub parent_id: Option<String>,
    /// Display parameters the session page starts from (RDP).
    pub dpi: Option<String>,
    pub resize_method: Option<String>,
}

fn guacd_port(row: &AnyRow) -> Result<Option<u16>, sqlx::Error> {
    Ok(row
        .try_get::<Option<i64>, _>("guacd_port")?
        .and_then(|p| u16::try_from(p).ok()))
}

impl Database {
    /// Connections visible to a user: all of them for connection managers
    /// and administrators, otherwise those reachable through grants.
    pub async fn connection_summaries(&self, user: &User) -> DbResult<Vec<ConnectionSummary>> {
        let select = "SELECT c.connection_id, c.name, c.protocol, c.guacd_host, c.guacd_port, \
                      c.parent_id, \
                      h.parameter_value AS hostname, p.parameter_value AS port, \
                      d.parameter_value AS dpi, r.parameter_value AS resize_method \
                      FROM molcajete_connection c \
                      LEFT JOIN molcajete_connection_parameter h \
                        ON h.connection_id = c.connection_id AND h.parameter_name = 'hostname' \
                      LEFT JOIN molcajete_connection_parameter p \
                        ON p.connection_id = c.connection_id AND p.parameter_name = 'port' \
                      LEFT JOIN molcajete_connection_parameter d \
                        ON d.connection_id = c.connection_id AND d.parameter_name = 'dpi' \
                      LEFT JOIN molcajete_connection_parameter r \
                        ON r.connection_id = c.connection_id \
                        AND r.parameter_name = 'resize-method'";
        let rows = if user.effective_role.can_manage_connections() {
            let sql = format!("{select} ORDER BY c.name, c.connection_id");
            query!(self, &sql).fetch_all(&self.pool).await?
        } else {
            let sql = format!(
                "{REACHABLE} {select} \
                 WHERE c.connection_id IN (SELECT connection_id FROM reachable) \
                 ORDER BY c.name, c.connection_id"
            );
            query!(self, &sql)
                .bind(&user.id)
                .bind(&user.id)
                .bind(&user.id)
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
                    parent_id: row.try_get("parent_id")?,
                    dpi: row.try_get("dpi")?,
                    resize_method: row.try_get("resize_method")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    pub async fn connection(&self, id: &str) -> DbResult<ConnectionRecord> {
        let row = query!(
            self,
            "SELECT connection_id, name, protocol, guacd_host, guacd_port, parent_id \
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
        let attributes = query!(
            self,
            "SELECT attribute_name, attribute_value FROM molcajete_connection_attribute \
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
            parent_id: row.try_get("parent_id")?,
            attributes: attributes
                .iter()
                .map(|r| Ok((r.try_get("attribute_name")?, r.try_get("attribute_value")?)))
                .collect::<Result<_, sqlx::Error>>()?,
        })
    }

    /// The connection, if the user may open it: connection managers and
    /// administrators open every connection, other users those reachable
    /// through a grant. [`DbError::NotFound`] otherwise.
    pub async fn connection_for_user(&self, id: &str, user: &User) -> DbResult<ConnectionRecord> {
        if !user.effective_role.can_manage_connections() {
            let sql =
                format!("{REACHABLE} SELECT COUNT(*) AS n FROM reachable WHERE connection_id = ?");
            let row = query!(self, &sql)
                .bind(&user.id)
                .bind(&user.id)
                .bind(&user.id)
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

    /// Fails with [`DbError::Invalid`] when `c.parent_id` does not exist.
    pub async fn create_connection(&self, c: &ConnectionRecord) -> DbResult<ConnectionRecord> {
        if let Some(parent) = &c.parent_id {
            self.require_folder(parent).await?;
        }
        let now = now_millis();
        let mut tx = self.pool.begin().await?;
        query!(
            self,
            "INSERT INTO molcajete_connection \
             (connection_id, name, protocol, guacd_host, guacd_port, parent_id, \
              created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&c.id)
        .bind(&c.name)
        .bind(&c.protocol)
        .bind(c.guacd_host.as_deref())
        .bind(c.guacd_port.map(i64::from))
        .bind(c.parent_id.as_deref())
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(DbError::from_sqlx)?;
        self.insert_parameters(&mut tx, &c.id, &c.parameters)
            .await?;
        self.insert_attributes(&mut tx, &c.id, &c.attributes)
            .await?;
        tx.commit().await?;
        self.connection(&c.id).await
    }

    /// Replaces the connection's fields and parameters. The id and the
    /// folder do not change.
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
        query!(
            self,
            "DELETE FROM molcajete_connection_attribute WHERE connection_id = ?"
        )
        .bind(&c.id)
        .execute(&mut *tx)
        .await?;
        self.insert_attributes(&mut tx, &c.id, &c.attributes)
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

    async fn insert_attributes(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        id: &str,
        attributes: &BTreeMap<String, String>,
    ) -> DbResult<()> {
        for (name, value) in attributes {
            query!(
                self,
                "INSERT INTO molcajete_connection_attribute \
                 (connection_id, attribute_name, attribute_value) VALUES (?, ?, ?)"
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
    use super::super::tests::every_backend;
    use super::super::{NewUser, Role};
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
            parent_id: None,
            attributes: BTreeMap::from([(attributes::RECORD.into(), "true".into())]),
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
            changed.attributes = BTreeMap::from([(attributes::RECORD_KEYS.into(), "true".into())]);
            changed.parameters.insert("dpi".into(), "144".into());
            changed
                .parameters
                .insert("resize-method".into(), "display-update".into());
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
                    role: Role::User,
                    disabled: false,
                })
                .await
                .unwrap();
            let manager = db
                .create_user(NewUser {
                    username: "m",
                    password_hash: "h",
                    role: Role::ConnectionManager,
                    disabled: false,
                })
                .await
                .unwrap();
            assert!(
                db.connection_summaries(&user).await.unwrap().is_empty(),
                "{name}"
            );
            let summaries = db.connection_summaries(&manager).await.unwrap();
            assert_eq!(summaries.len(), 2, "{name}");
            assert_eq!(
                (
                    summaries[1].dpi.as_deref(),
                    summaries[1].resize_method.as_deref()
                ),
                (Some("144"), Some("display-update")),
                "{name}"
            );
            assert_eq!(
                (
                    summaries[0].dpi.as_deref(),
                    summaries[0].resize_method.as_deref()
                ),
                (None, None),
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
            let list = db.connection_summaries(&user).await.unwrap();
            assert_eq!(list.len(), 1, "{name}");
            assert_eq!(list[0].hostname.as_deref(), Some("10.0.0.5"), "{name}");
            assert_eq!(list[0].port.as_deref(), Some("5900"), "{name}");
            assert_eq!(list[0].guacd_port, Some(4823), "{name}");
            assert!(
                db.connection_for_user("desktop", &user).await.is_ok(),
                "{name}"
            );
            assert!(
                matches!(
                    db.connection_for_user("another", &user).await,
                    Err(DbError::NotFound)
                ),
                "{name}"
            );
            assert!(
                db.connection_for_user("another", &manager).await.is_ok(),
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
