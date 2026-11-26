//! `molcajete_connection`, `molcajete_connection_parameter` and
//! `molcajete_connection_attribute`.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::grants::REACHABLE;
use super::{Database, DbError, DbResult, User, now_millis, query};
use crate::secrets::{self, SecretError};

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
    /// Seconds to wait for the handshake instead of `[guacd]
    /// handshake_timeout`.
    #[serde(default)]
    pub guacd_handshake_timeout: Option<u64>,
    /// Protocol parameters sent in the `connect` instruction.
    ///
    /// Credentials ([`secrets::is_sensitive`]) are write-only: saving
    /// encrypts them, and a connection read back leaves them out and names
    /// them in `stored_secrets` instead ([`Database::connection_secrets`]
    /// decrypts them for the guacd handshake). Saving a connection without
    /// a credential keeps the stored one unless `clear_secrets` names it.
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
    /// Credentials that have a stored value. Ignored when saving.
    #[serde(default, skip_deserializing)]
    pub stored_secrets: BTreeSet<String>,
    /// Stored credentials to remove when saving. Never returned.
    #[serde(default, skip_serializing)]
    pub clear_secrets: BTreeSet<String>,
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
    /// Let the user of a session record it from the session menu.
    pub const RECORD_ON_DEMAND: &str = "record-on-demand";

    /// Every attribute the gateway knows.
    pub const ALL: &[&str] = &[
        RECORD,
        RECORD_ON_DEMAND,
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
    /// The text size a terminal starts with (`font-size`).
    pub font_size: Option<String>,
    /// The `read-only`, `disable-copy` and `disable-paste` parameters, so
    /// the session page can say what is turned off.
    pub read_only: bool,
    pub disable_copy: bool,
    pub disable_paste: bool,
}

/// A boolean guacd parameter: on only when `"true"`, as guacd reads it.
fn flag(row: &AnyRow, column: &str) -> Result<bool, sqlx::Error> {
    Ok(row.try_get::<Option<String>, _>(column)?.as_deref() == Some("true"))
}

/// Whether a stored parameter row holds a credential: a sensitive name, or
/// a value that is encrypted (also under a name no longer on the list).
fn is_secret_row(name: &str, value: &str) -> bool {
    secrets::is_sensitive(name) || secrets::is_sealed(value)
}

fn guacd_port(row: &AnyRow) -> Result<Option<u16>, sqlx::Error> {
    Ok(row
        .try_get::<Option<i64>, _>("guacd_port")?
        .and_then(|p| u16::try_from(p).ok()))
}

fn guacd_handshake_timeout(row: &AnyRow) -> Result<Option<u64>, sqlx::Error> {
    Ok(row
        .try_get::<Option<i64>, _>("guacd_handshake_timeout")?
        .and_then(|t| u64::try_from(t).ok()))
}

fn timeout_column(seconds: u64) -> i64 {
    i64::try_from(seconds).unwrap_or(i64::MAX)
}

impl Database {
    /// Connections visible to a user: all of them for connection managers
    /// and administrators, otherwise those reachable through grants.
    pub async fn connection_summaries(&self, user: &User) -> DbResult<Vec<ConnectionSummary>> {
        let select = "SELECT c.connection_id, c.name, c.protocol, c.guacd_host, c.guacd_port, \
                      c.parent_id, \
                      h.parameter_value AS hostname, p.parameter_value AS port, \
                      d.parameter_value AS dpi, r.parameter_value AS resize_method, \
                      fs.parameter_value AS font_size, \
                      ro.parameter_value AS read_only, dc.parameter_value AS disable_copy, \
                      dp.parameter_value AS disable_paste \
                      FROM molcajete_connection c \
                      LEFT JOIN molcajete_connection_parameter h \
                        ON h.connection_id = c.connection_id AND h.parameter_name = 'hostname' \
                      LEFT JOIN molcajete_connection_parameter p \
                        ON p.connection_id = c.connection_id AND p.parameter_name = 'port' \
                      LEFT JOIN molcajete_connection_parameter d \
                        ON d.connection_id = c.connection_id AND d.parameter_name = 'dpi' \
                      LEFT JOIN molcajete_connection_parameter r \
                        ON r.connection_id = c.connection_id \
                        AND r.parameter_name = 'resize-method' \
                      LEFT JOIN molcajete_connection_parameter fs \
                        ON fs.connection_id = c.connection_id AND fs.parameter_name = 'font-size' \
                      LEFT JOIN molcajete_connection_parameter ro \
                        ON ro.connection_id = c.connection_id AND ro.parameter_name = 'read-only' \
                      LEFT JOIN molcajete_connection_parameter dc \
                        ON dc.connection_id = c.connection_id \
                        AND dc.parameter_name = 'disable-copy' \
                      LEFT JOIN molcajete_connection_parameter dp \
                        ON dp.connection_id = c.connection_id \
                        AND dp.parameter_name = 'disable-paste'";
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
                    font_size: row.try_get("font_size")?,
                    read_only: flag(row, "read_only")?,
                    disable_copy: flag(row, "disable_copy")?,
                    disable_paste: flag(row, "disable_paste")?,
                })
            })
            .collect::<Result<_, sqlx::Error>>()
            .map_err(DbError::from)
    }

    pub async fn connection(&self, id: &str) -> DbResult<ConnectionRecord> {
        let row = query!(
            self,
            "SELECT connection_id, name, protocol, guacd_host, guacd_port, \
             guacd_handshake_timeout, parent_id \
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
        let mut parameters = BTreeMap::new();
        let mut stored_secrets = BTreeSet::new();
        for r in &params {
            let name: String = r.try_get("parameter_name")?;
            let value: String = r.try_get("parameter_value")?;
            if is_secret_row(&name, &value) {
                stored_secrets.insert(name);
            } else {
                parameters.insert(name, value);
            }
        }
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
            guacd_handshake_timeout: guacd_handshake_timeout(&row)?,
            parameters,
            stored_secrets,
            clear_secrets: BTreeSet::new(),
            parent_id: row.try_get("parent_id")?,
            attributes: attributes
                .iter()
                .map(|r| Ok((r.try_get("attribute_name")?, r.try_get("attribute_value")?)))
                .collect::<Result<_, sqlx::Error>>()?,
        })
    }

    /// The decrypted credentials of a connection, for the guacd handshake
    /// only.
    pub async fn connection_secrets(&self, id: &str) -> DbResult<BTreeMap<String, String>> {
        let rows = query!(
            self,
            "SELECT parameter_name, parameter_value FROM molcajete_connection_parameter \
             WHERE connection_id = ?"
        )
        .bind(id)
        .fetch_all(&self.pool)
        .await?;
        let mut values = BTreeMap::new();
        for r in &rows {
            let name: String = r.try_get("parameter_name")?;
            let value: String = r.try_get("parameter_value")?;
            if secrets::is_sealed(&value) {
                let plain = self.secrets()?.open(id, &name, &value)?;
                values.insert(name, plain);
            } else if secrets::is_sensitive(&name) {
                values.insert(name, value);
            }
        }
        Ok(values)
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
             (connection_id, name, protocol, guacd_host, guacd_port, \
              guacd_handshake_timeout, parent_id, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&c.id)
        .bind(&c.name)
        .bind(&c.protocol)
        .bind(c.guacd_host.as_deref())
        .bind(c.guacd_port.map(i64::from))
        .bind(c.guacd_handshake_timeout.map(timeout_column))
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
             SET name = ?, protocol = ?, guacd_host = ?, guacd_port = ?, \
             guacd_handshake_timeout = ?, updated_at = ? \
             WHERE connection_id = ?"
        )
        .bind(&c.name)
        .bind(&c.protocol)
        .bind(c.guacd_host.as_deref())
        .bind(c.guacd_port.map(i64::from))
        .bind(c.guacd_handshake_timeout.map(timeout_column))
        .bind(now_millis())
        .bind(&c.id)
        .execute(&mut *tx)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        // Stored credentials without a new value stay as they are.
        let stored = query!(
            self,
            "SELECT parameter_name, parameter_value FROM molcajete_connection_parameter \
             WHERE connection_id = ?"
        )
        .bind(&c.id)
        .fetch_all(&mut *tx)
        .await?;
        let mut kept = Vec::new();
        for r in &stored {
            let name: String = r.try_get("parameter_name")?;
            let value: String = r.try_get("parameter_value")?;
            if is_secret_row(&name, &value)
                && !c.parameters.contains_key(&name)
                && !c.clear_secrets.contains(&name)
            {
                kept.push((name, value));
            }
        }
        query!(
            self,
            "DELETE FROM molcajete_connection_parameter WHERE connection_id = ?"
        )
        .bind(&c.id)
        .execute(&mut *tx)
        .await?;
        for (name, value) in &kept {
            self.insert_parameter(&mut tx, &c.id, name, value).await?;
        }
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

    /// Inserts parameters given in plain text, encrypting credentials.
    async fn insert_parameters(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        id: &str,
        parameters: &BTreeMap<String, String>,
    ) -> DbResult<()> {
        for (name, value) in parameters {
            if secrets::is_sensitive(name) {
                let sealed = self.secrets()?.seal(id, name, value);
                self.insert_parameter(tx, id, name, &sealed).await?;
            } else {
                self.insert_parameter(tx, id, name, value).await?;
            }
        }
        Ok(())
    }

    /// Inserts one parameter as it is to be stored.
    async fn insert_parameter(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Any>,
        id: &str,
        name: &str,
        stored: &str,
    ) -> DbResult<()> {
        query!(
            self,
            "INSERT INTO molcajete_connection_parameter \
             (connection_id, parameter_name, parameter_value) VALUES (?, ?, ?)"
        )
        .bind(id)
        .bind(name)
        .bind(stored)
        .execute(&mut **tx)
        .await?;
        Ok(())
    }

    /// Whether any stored credential is encrypted (with any key).
    pub async fn has_sealed_secrets(&self) -> DbResult<bool> {
        let row = query!(
            self,
            "SELECT COUNT(*) AS n FROM molcajete_connection_parameter \
             WHERE parameter_value LIKE 'enc:v1:%'"
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get::<i64, _>("n")? > 0)
    }

    /// Run on every start: encrypts credentials stored in plain text (by a
    /// gateway before encryption, or under a name newly added to
    /// [`crate::sensitive`]), all in one transaction, and
    /// checks that every encrypted value is under a configured key.
    /// Returns how many values were encrypted.
    pub async fn seal_plaintext_secrets(&self) -> DbResult<usize> {
        let keys = self.secrets()?;
        self.rewrite_secrets(|id, name, value| match secrets::key_id(value) {
            Some(key_id) if !keys.knows(key_id) => Err(SecretError::UnknownKey {
                key_id: key_id.to_owned(),
            }),
            Some(_) => Ok(None),
            None => Ok(Some(keys.seal(id, name, value))),
        })
        .await
    }

    /// `molcajete-gateway rotate-key`: re-encrypts every credential that is
    /// not under the current key with it, in one transaction. Returns how
    /// many values were re-encrypted.
    pub async fn rotate_secrets(&self) -> DbResult<usize> {
        let keys = self.secrets()?;
        self.rewrite_secrets(|id, name, value| {
            if secrets::key_id(value) == Some(keys.current_id()) {
                return Ok(None);
            }
            let plain = if secrets::is_sealed(value) {
                keys.open(id, name, value)?
            } else {
                value.to_owned()
            };
            Ok(Some(keys.seal(id, name, &plain)))
        })
        .await
    }

    /// Replaces each stored credential for which `rewrite` returns a new
    /// stored value, in one transaction; an error rolls everything back.
    async fn rewrite_secrets(
        &self,
        rewrite: impl Fn(&str, &str, &str) -> Result<Option<String>, SecretError>,
    ) -> DbResult<usize> {
        let mut tx = self.pool.begin().await?;
        // Which names are secrets is decided in Rust (`is_secret_row`), so
        // every row is read; a gateway stores a few thousand at most.
        let rows = query!(
            self,
            "SELECT connection_id, parameter_name, parameter_value \
             FROM molcajete_connection_parameter"
        )
        .fetch_all(&mut *tx)
        .await?;
        let mut changed = 0;
        for r in &rows {
            let id: String = r.try_get("connection_id")?;
            let name: String = r.try_get("parameter_name")?;
            let value: String = r.try_get("parameter_value")?;
            if !is_secret_row(&name, &value) {
                continue;
            }
            if let Some(stored) = rewrite(&id, &name, &value)? {
                query!(
                    self,
                    "UPDATE molcajete_connection_parameter SET parameter_value = ? \
                     WHERE connection_id = ? AND parameter_name = ?"
                )
                .bind(stored)
                .bind(&id)
                .bind(&name)
                .execute(&mut *tx)
                .await?;
                changed += 1;
            }
        }
        tx.commit().await?;
        Ok(changed)
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

    /// A parameter exactly as the database holds it.
    #[cfg(test)]
    pub async fn stored_parameter(&self, id: &str, name: &str) -> Option<String> {
        query!(
            self,
            "SELECT parameter_value FROM molcajete_connection_parameter \
             WHERE connection_id = ? AND parameter_name = ?"
        )
        .bind(id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .unwrap()
        .map(|row| row.try_get("parameter_value").unwrap())
    }

    /// Stores a parameter as it is, as a gateway before encryption did.
    #[cfg(test)]
    pub async fn store_plain_parameter(&self, id: &str, name: &str, value: &str) {
        query!(
            self,
            "INSERT INTO molcajete_connection_parameter \
             (connection_id, parameter_name, parameter_value) VALUES (?, ?, ?)"
        )
        .bind(id)
        .bind(name)
        .bind(value)
        .execute(&self.pool)
        .await
        .unwrap();
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
            guacd_handshake_timeout: None,
            parameters: BTreeMap::from([
                ("hostname".into(), "10.0.0.5".into()),
                ("port".into(), "5900".into()),
                ("password".into(), "secret".into()),
            ]),
            stored_secrets: BTreeSet::new(),
            clear_secrets: BTreeSet::new(),
            parent_id: None,
            attributes: BTreeMap::from([(attributes::RECORD.into(), "true".into())]),
        }
    }

    /// `c` as it reads back: credentials named, not shown.
    fn as_read(mut c: ConnectionRecord) -> ConnectionRecord {
        let (secret, plain) = std::mem::take(&mut c.parameters)
            .into_iter()
            .partition(|(name, _)| secrets::is_sensitive(name));
        c.parameters = plain;
        c.stored_secrets = c
            .stored_secrets
            .into_iter()
            .chain(secret.into_keys())
            .collect();
        c.clear_secrets.clear();
        c
    }

    #[tokio::test]
    async fn connection_lifecycle_and_permissions() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let desktop = db
                .create_connection(&record("desktop", "Desktop"))
                .await
                .unwrap();
            assert_eq!(desktop, as_read(record("desktop", "Desktop")), "{name}");
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
            changed.guacd_handshake_timeout = Some(45);
            changed.parameters.remove("password");
            changed.clear_secrets.insert("password".into());
            changed.attributes = BTreeMap::from([(attributes::RECORD_KEYS.into(), "true".into())]);
            changed.parameters.insert("dpi".into(), "144".into());
            changed.parameters.insert("read-only".into(), "true".into());
            changed
                .parameters
                .insert("disable-paste".into(), "true".into());
            changed
                .parameters
                .insert("resize-method".into(), "display-update".into());
            changed.parameters.insert("font-size".into(), "10".into());
            assert_eq!(
                db.update_connection(&changed).await.unwrap(),
                as_read(changed),
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
                    summaries[1].read_only,
                    summaries[1].disable_copy,
                    summaries[1].disable_paste
                ),
                (true, false, true),
                "{name}"
            );
            assert_eq!(summaries[1].font_size.as_deref(), Some("10"), "{name}");
            assert_eq!(summaries[0].font_size, None, "{name}");
            assert!(!summaries[0].read_only, "{name}");
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

    fn plain(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn credentials_are_encrypted_at_rest() {
        use crate::secrets::SecretKeys;
        use crate::secrets::tests::{KEY_A, KEY_B};

        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let mut ssh = record("ssh", "SSH");
            ssh.parameters
                .insert("private-key".into(), "-----BEGIN KEY-----".into());
            db.create_connection(&ssh).await.unwrap();
            for param in ["password", "private-key"] {
                let stored = db.stored_parameter("ssh", param).await.unwrap();
                assert!(secrets::is_sealed(&stored), "{name} {param}");
                assert!(!stored.contains("secret") && !stored.contains("BEGIN"));
            }
            assert_eq!(
                db.stored_parameter("ssh", "hostname").await.as_deref(),
                Some("10.0.0.5")
            );
            let read = db.connection("ssh").await.unwrap();
            assert!(!read.parameters.contains_key("password"), "{name}");
            assert_eq!(
                db.connection_secrets("ssh").await.unwrap(),
                plain(&[
                    ("password", "secret"),
                    ("private-key", "-----BEGIN KEY-----")
                ]),
                "{name}"
            );

            // Saving what was read keeps the credentials; a new value
            // replaces one, and only an explicit clear removes one.
            db.update_connection(&read).await.unwrap();
            assert_eq!(db.connection_secrets("ssh").await.unwrap().len(), 2);
            let mut edit = read.clone();
            edit.parameters.insert("password".into(), "changed".into());
            edit.clear_secrets.insert("private-key".into());
            let saved = db.update_connection(&edit).await.unwrap();
            assert_eq!(saved.stored_secrets, BTreeSet::from(["password".into()]));
            assert_eq!(
                db.connection_secrets("ssh").await.unwrap(),
                plain(&[("password", "changed")]),
                "{name}"
            );

            // Plain values left by an older gateway are encrypted on start,
            // once.
            let mut old = record("old", "Old");
            old.parameters.clear();
            db.create_connection(&old).await.unwrap();
            db.store_plain_parameter("old", "password", "legacy").await;
            db.store_plain_parameter("old", "sftp-password", "legacy sftp")
                .await;
            db.store_plain_parameter("old", "hostname", "10.0.0.6")
                .await;
            assert_eq!(db.seal_plaintext_secrets().await.unwrap(), 2, "{name}");
            assert_eq!(db.seal_plaintext_secrets().await.unwrap(), 0, "{name}");
            let stored = db.stored_parameter("old", "password").await.unwrap();
            assert!(secrets::is_sealed(&stored), "{name}");
            assert_eq!(
                db.stored_parameter("old", "hostname").await.as_deref(),
                Some("10.0.0.6")
            );
            assert_eq!(
                db.connection_secrets("old").await.unwrap(),
                plain(&[("password", "legacy"), ("sftp-password", "legacy sftp")]),
                "{name}"
            );

            // Rotation: the new key reads the old one's values with it as a
            // previous key, and re-encrypts all of them.
            let rotating = db
                .clone()
                .with_secrets(SecretKeys::from_base64(KEY_B, &[KEY_A]).unwrap());
            assert_eq!(rotating.rotate_secrets().await.unwrap(), 3, "{name}");
            assert_eq!(rotating.rotate_secrets().await.unwrap(), 0, "{name}");
            let new_only = db
                .clone()
                .with_secrets(SecretKeys::from_base64(KEY_B, &[]).unwrap());
            new_only.seal_plaintext_secrets().await.unwrap();
            assert_eq!(
                new_only.connection_secrets("old").await.unwrap()["password"],
                "legacy"
            );
            assert_eq!(
                new_only.connection_secrets("ssh").await.unwrap()["password"],
                "changed"
            );
            // The old key alone no longer opens anything, and says so.
            assert!(
                matches!(
                    db.seal_plaintext_secrets().await,
                    Err(DbError::Secret(SecretError::UnknownKey { .. }))
                ),
                "{name}"
            );
            assert!(
                matches!(
                    db.connection_secrets("ssh").await,
                    Err(DbError::Secret(SecretError::UnknownKey { .. }))
                ),
                "{name}"
            );
        }
    }
}
