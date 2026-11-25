//! `molcajete_session`: login sessions, keyed by the SHA-256 of the cookie
//! token so a leaked database does not hand out live sessions.

use super::{Database, DbError, DbResult, User, now_millis, query};

impl Database {
    pub async fn create_session(
        &self,
        token_hash: &str,
        user_id: &str,
        ttl_ms: i64,
        id_token: Option<&str>,
    ) -> DbResult<()> {
        let now = now_millis();
        query!(self, "DELETE FROM molcajete_session WHERE expires_at <= ?")
            .bind(now)
            .execute(&self.pool)
            .await?;
        query!(
            self,
            "INSERT INTO molcajete_session (token_hash, user_id, created_at, expires_at, id_token) \
             VALUES (?, ?, ?, ?, ?)"
        )
        .bind(token_hash)
        .bind(user_id)
        .bind(now)
        .bind(now.saturating_add(ttl_ms))
        .bind(id_token)
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(())
    }

    /// The enabled user owning an unexpired session.
    pub async fn session_user(&self, token_hash: &str) -> DbResult<User> {
        let row = query!(
            self,
            "SELECT s.user_id FROM molcajete_session s \
             JOIN molcajete_user u ON u.user_id = s.user_id \
             WHERE s.token_hash = ? AND s.expires_at > ? AND u.disabled = 0"
        )
        .bind(token_hash)
        .bind(now_millis())
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        let user_id: String = sqlx::Row::try_get(&row, "user_id")?;
        self.user(&user_id).await
    }

    /// The ID token of a session opened through an OpenID Connect provider.
    pub async fn session_id_token(&self, token_hash: &str) -> DbResult<Option<String>> {
        let row = query!(
            self,
            "SELECT id_token FROM molcajete_session WHERE token_hash = ?"
        )
        .bind(token_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(sqlx::Row::try_get(&row, "id_token")?)
    }

    pub async fn delete_session(&self, token_hash: &str) -> DbResult<()> {
        query!(self, "DELETE FROM molcajete_session WHERE token_hash = ?")
            .bind(token_hash)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::every_backend;
    use super::super::{NewUser, Role, UserUpdate};
    use super::*;

    #[tokio::test]
    async fn sessions_expire_and_follow_the_user() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let user = db
                .create_user(NewUser {
                    username: "s",
                    password_hash: "h",
                    role: Role::User,
                    disabled: false,
                })
                .await
                .unwrap();
            db.create_session("live", &user.id, 60_000, None)
                .await
                .unwrap();
            db.create_session("expired", &user.id, -1, None)
                .await
                .unwrap();
            assert_eq!(db.session_user("live").await.unwrap().id, user.id, "{name}");
            assert!(
                matches!(db.session_user("expired").await, Err(DbError::NotFound)),
                "{name}"
            );
            assert!(
                matches!(db.session_user("unknown").await, Err(DbError::NotFound)),
                "{name}"
            );

            db.delete_session("live").await.unwrap();
            assert!(db.session_user("live").await.is_err(), "{name}");

            db.create_session("again", &user.id, 60_000, Some("id.token"))
                .await
                .unwrap();
            assert_eq!(
                db.session_id_token("again").await.unwrap().as_deref(),
                Some("id.token"),
                "{name}"
            );
            assert_eq!(db.session_id_token("live").await.ok(), None, "{name}");
            db.update_user(
                &user.id,
                UserUpdate {
                    disabled: Some(true),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert!(db.session_user("again").await.is_err(), "{name}");
        }
    }
}
