//! `molcajete_api_token` and `molcajete_session_ticket`: bearer tokens for
//! automation and the one-time links that open a connection without a
//! login. Both are keyed by the SHA-256 of the secret (see `crate::tokens`).

use serde::Serialize;
use sqlx::Row as _;
use sqlx::any::AnyRow;

use super::{Database, DbError, DbResult, now_millis, query};

/// An API token as listed; the token itself is never stored.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    /// The first characters of the token, to recognise it.
    pub prefix: String,
    pub user_id: String,
    pub username: String,
    /// Addresses and CIDR ranges it may be used from; empty for any.
    pub allowed_ips: Vec<String>,
    /// Unix milliseconds.
    pub created_at: i64,
    pub expires_at: i64,
    pub last_used_at: Option<i64>,
    pub last_used_from: Option<String>,
}

pub struct NewApiToken<'a> {
    pub token_hash: &'a str,
    pub prefix: &'a str,
    pub user_id: &'a str,
    pub name: &'a str,
    pub allowed_ips: &'a [String],
    pub expires_at: i64,
}

/// A ticket taken by [`Database::redeem_ticket`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTicket {
    pub user_id: String,
    pub connection_id: String,
    /// JSON object of display parameters, if any.
    pub parameters: Option<String>,
    pub expires_at: i64,
}

pub struct NewSessionTicket<'a> {
    pub ticket_hash: &'a str,
    pub user_id: &'a str,
    pub connection_id: &'a str,
    pub token_id: Option<&'a str>,
    pub parameters: Option<&'a str>,
    pub expires_at: i64,
}

const COLUMNS: &str = "t.token_id, t.name, t.token_prefix, t.user_id, u.username, \
                       t.allowed_ips, t.created_at, t.expires_at, t.last_used_at, \
                       t.last_used_from";

fn from_row(row: &AnyRow) -> Result<ApiToken, sqlx::Error> {
    let allowed_ips: Option<String> = row.try_get("allowed_ips")?;
    Ok(ApiToken {
        id: row.try_get("token_id")?,
        name: row.try_get("name")?,
        prefix: row.try_get("token_prefix")?,
        user_id: row.try_get("user_id")?,
        username: row.try_get("username")?,
        allowed_ips: allowed_ips
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect(),
        created_at: row.try_get("created_at")?,
        expires_at: row.try_get("expires_at")?,
        last_used_at: row.try_get("last_used_at")?,
        last_used_from: row.try_get("last_used_from")?,
    })
}

impl Database {
    pub async fn create_api_token(&self, new: NewApiToken<'_>) -> DbResult<ApiToken> {
        let id = uuid::Uuid::new_v4().to_string();
        let allowed_ips = (!new.allowed_ips.is_empty()).then(|| new.allowed_ips.join("\n"));
        query!(
            self,
            "INSERT INTO molcajete_api_token (token_id, token_hash, token_prefix, user_id, name, \
             allowed_ips, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(&id)
        .bind(new.token_hash)
        .bind(new.prefix)
        .bind(new.user_id)
        .bind(new.name)
        .bind(allowed_ips)
        .bind(now_millis())
        .bind(new.expires_at)
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        self.api_token(&id).await
    }

    pub async fn api_token(&self, id: &str) -> DbResult<ApiToken> {
        let sql = format!(
            "SELECT {COLUMNS} FROM molcajete_api_token t \
             JOIN molcajete_user u ON u.user_id = t.user_id WHERE t.token_id = ?"
        );
        let row = query!(self, &sql)
            .bind(id)
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok(from_row(&row)?)
    }

    /// The tokens of `user_id`, or everyone's, newest first.
    pub async fn list_api_tokens(&self, user_id: Option<&str>) -> DbResult<Vec<ApiToken>> {
        let mut sql = format!(
            "SELECT {COLUMNS} FROM molcajete_api_token t \
             JOIN molcajete_user u ON u.user_id = t.user_id"
        );
        if user_id.is_some() {
            sql.push_str(" WHERE t.user_id = ?");
        }
        sql.push_str(" ORDER BY t.created_at DESC, t.token_id");
        let mut q = query!(self, &sql);
        if let Some(id) = user_id {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await?;
        Ok(rows.iter().map(from_row).collect::<Result<_, _>>()?)
    }

    /// The unexpired token with this hash whose owner is enabled.
    pub async fn usable_api_token(&self, token_hash: &str) -> DbResult<ApiToken> {
        let sql = format!(
            "SELECT {COLUMNS} FROM molcajete_api_token t \
             JOIN molcajete_user u ON u.user_id = t.user_id \
             WHERE t.token_hash = ? AND t.expires_at > ? AND u.disabled = 0"
        );
        let row = query!(self, &sql)
            .bind(token_hash)
            .bind(now_millis())
            .fetch_one(&self.pool)
            .await
            .map_err(DbError::from_sqlx)?;
        Ok(from_row(&row)?)
    }

    /// Records a use of the token, unless another request already moved
    /// `last_used_at` on from `previous`. Returns whether this call did.
    pub async fn touch_api_token(
        &self,
        id: &str,
        previous: Option<i64>,
        at: i64,
        from: &str,
    ) -> DbResult<bool> {
        let condition = if previous.is_some() {
            "last_used_at = ?"
        } else {
            "last_used_at IS NULL"
        };
        let sql = format!(
            "UPDATE molcajete_api_token SET last_used_at = ?, last_used_from = ? \
             WHERE token_id = ? AND {condition}"
        );
        let mut q = query!(self, &sql).bind(at).bind(from).bind(id);
        if let Some(previous) = previous {
            q = q.bind(previous);
        }
        let result = q.execute(&self.pool).await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deletes a token and the unused tickets it made.
    pub async fn delete_api_token(&self, id: &str) -> DbResult<()> {
        let mut tx = self.pool.begin().await?;
        query!(
            self,
            "DELETE FROM molcajete_session_ticket WHERE token_id = ?"
        )
        .bind(id)
        .execute(&mut *tx)
        .await?;
        let result = query!(self, "DELETE FROM molcajete_api_token WHERE token_id = ?")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound);
        }
        tx.commit().await?;
        Ok(())
    }

    pub async fn create_session_ticket(&self, new: NewSessionTicket<'_>) -> DbResult<()> {
        let now = now_millis();
        query!(
            self,
            "DELETE FROM molcajete_session_ticket WHERE expires_at <= ?"
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        query!(
            self,
            "INSERT INTO molcajete_session_ticket (ticket_hash, user_id, connection_id, token_id, \
             parameters, created_at, expires_at) VALUES (?, ?, ?, ?, ?, ?, ?)"
        )
        .bind(new.ticket_hash)
        .bind(new.user_id)
        .bind(new.connection_id)
        .bind(new.token_id)
        .bind(new.parameters)
        .bind(now)
        .bind(new.expires_at)
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(())
    }

    /// The unused, unexpired ticket with this hash, leaving it in place.
    pub async fn session_ticket(&self, ticket_hash: &str) -> DbResult<SessionTicket> {
        let row = query!(
            self,
            "SELECT user_id, connection_id, parameters, expires_at \
             FROM molcajete_session_ticket WHERE ticket_hash = ? AND expires_at > ?"
        )
        .bind(ticket_hash)
        .bind(now_millis())
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(SessionTicket {
            user_id: row.try_get("user_id")?,
            connection_id: row.try_get("connection_id")?,
            parameters: row.try_get("parameters")?,
            expires_at: row.try_get("expires_at")?,
        })
    }

    /// Takes the ticket: it is deleted, so only one caller (on any
    /// replica) gets it. [`DbError::NotFound`] when it is unknown, used or
    /// expired.
    pub async fn redeem_ticket(&self, ticket_hash: &str) -> DbResult<SessionTicket> {
        let ticket = self.session_ticket(ticket_hash).await?;
        let deleted = query!(
            self,
            "DELETE FROM molcajete_session_ticket WHERE ticket_hash = ?"
        )
        .bind(ticket_hash)
        .execute(&self.pool)
        .await?;
        if deleted.rows_affected() != 1 || ticket.expires_at <= now_millis() {
            return Err(DbError::NotFound);
        }
        Ok(ticket)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::every_backend;
    use super::super::{NewUser, Role, UserUpdate};
    use super::*;

    #[tokio::test]
    async fn tokens_follow_their_owner_and_tickets_are_used_once() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let user = db
                .create_user(NewUser {
                    username: "robot",
                    password_hash: "h",
                    role: Role::User,
                    disabled: false,
                })
                .await
                .unwrap();
            let far = now_millis() + 3_600_000;
            let token = db
                .create_api_token(NewApiToken {
                    token_hash: "hash",
                    prefix: "mlc_1234",
                    user_id: &user.id,
                    name: "ci",
                    allowed_ips: &["10.0.0.0/8".into(), "::1".into()],
                    expires_at: far,
                })
                .await
                .unwrap();
            assert_eq!(token.username, "robot", "{name}");
            assert_eq!(token.allowed_ips, ["10.0.0.0/8", "::1"], "{name}");
            db.create_api_token(NewApiToken {
                token_hash: "old",
                prefix: "mlc_5678",
                user_id: &user.id,
                name: "old",
                allowed_ips: &[],
                expires_at: now_millis() - 1,
            })
            .await
            .unwrap();
            assert_eq!(db.list_api_tokens(Some(&user.id)).await.unwrap().len(), 2);
            assert_eq!(db.list_api_tokens(None).await.unwrap().len(), 2);
            assert_eq!(db.usable_api_token("hash").await.unwrap().id, token.id);
            assert!(
                matches!(db.usable_api_token("old").await, Err(DbError::NotFound)),
                "{name}"
            );

            assert!(db.touch_api_token(&token.id, None, 5, "a").await.unwrap());
            // Another replica got there first.
            assert!(!db.touch_api_token(&token.id, None, 6, "b").await.unwrap());
            assert!(
                db.touch_api_token(&token.id, Some(5), 7, "c")
                    .await
                    .unwrap()
            );
            let touched = db.api_token(&token.id).await.unwrap();
            assert_eq!(touched.last_used_at, Some(7), "{name}");
            assert_eq!(touched.last_used_from.as_deref(), Some("c"), "{name}");

            db.create_connection(&super::super::ConnectionRecord {
                id: "desk".into(),
                name: "Desk".into(),
                protocol: "vnc".into(),
                guacd_host: None,
                guacd_port: None,
                guacd_handshake_timeout: None,
                parameters: Default::default(),
                stored_secrets: Default::default(),
                clear_secrets: Default::default(),
                parent_id: None,
                attributes: Default::default(),
            })
            .await
            .unwrap();
            fn ticket<'a>(
                hash: &'a str,
                user_id: &'a str,
                token_id: Option<&'a str>,
                expires_at: i64,
            ) -> NewSessionTicket<'a> {
                NewSessionTicket {
                    ticket_hash: hash,
                    user_id,
                    connection_id: "desk",
                    token_id,
                    parameters: Some(r#"{"dpi":"96"}"#),
                    expires_at,
                }
            }
            db.create_session_ticket(ticket("once", &user.id, None, far))
                .await
                .unwrap();
            db.create_session_ticket(ticket("stale", &user.id, None, now_millis() - 1))
                .await
                .unwrap();
            db.create_session_ticket(ticket("by-token", &user.id, Some(&token.id), far))
                .await
                .unwrap();
            let taken = db.redeem_ticket("once").await.unwrap();
            assert_eq!(taken.connection_id, "desk", "{name}");
            assert_eq!(taken.parameters.as_deref(), Some(r#"{"dpi":"96"}"#));
            assert!(db.redeem_ticket("once").await.is_err(), "{name}");
            assert!(db.redeem_ticket("stale").await.is_err(), "{name}");
            assert!(db.session_ticket("by-token").await.is_ok(), "{name}");

            // Revoking the token takes its tickets with it.
            db.delete_api_token(&token.id).await.unwrap();
            assert!(db.redeem_ticket("by-token").await.is_err(), "{name}");
            assert!(
                matches!(db.delete_api_token(&token.id).await, Err(DbError::NotFound)),
                "{name}"
            );

            db.update_user(
                &user.id,
                UserUpdate {
                    disabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            let hash = "hash-of-a-disabled-owner";
            db.create_api_token(NewApiToken {
                token_hash: hash,
                prefix: "mlc_9",
                user_id: &user.id,
                name: "x",
                allowed_ips: &[],
                expires_at: far,
            })
            .await
            .unwrap();
            assert!(db.usable_api_token(hash).await.is_err(), "{name}");
            assert_eq!(db.list_api_tokens(None).await.unwrap().len(), 2);
            db.delete_user(&user.id).await.unwrap();
            assert!(db.list_api_tokens(None).await.unwrap().is_empty(), "{name}");
        }
    }
}
