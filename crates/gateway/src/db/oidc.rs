//! `molcajete_user_identity` and `molcajete_oidc_login`: identities at an
//! OpenID Connect provider and sign-ins in progress.

use sqlx::Row as _;

use super::{Database, DbError, DbResult, User, now_millis, query};

/// What a sign-in started at the provider needs to finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLogin {
    pub nonce: String,
    pub pkce_verifier: String,
}

impl Database {
    /// Remembers a sign-in sent to the provider, keyed by its `state`'s hash.
    pub async fn create_oidc_login(
        &self,
        state_hash: &str,
        pending: &PendingLogin,
        ttl_ms: i64,
    ) -> DbResult<()> {
        let now = now_millis();
        query!(
            self,
            "DELETE FROM molcajete_oidc_login WHERE expires_at <= ?"
        )
        .bind(now)
        .execute(&self.pool)
        .await?;
        query!(
            self,
            "INSERT INTO molcajete_oidc_login \
             (state_hash, nonce, pkce_verifier, created_at, expires_at) VALUES (?, ?, ?, ?, ?)"
        )
        .bind(state_hash)
        .bind(&pending.nonce)
        .bind(&pending.pkce_verifier)
        .bind(now)
        .bind(now.saturating_add(ttl_ms))
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(())
    }

    /// Removes and returns an unexpired sign-in; a second call for the same
    /// state finds nothing, so a replayed callback is refused.
    pub async fn take_oidc_login(&self, state_hash: &str) -> DbResult<PendingLogin> {
        let row = query!(
            self,
            "SELECT nonce, pkce_verifier, expires_at FROM molcajete_oidc_login \
             WHERE state_hash = ?"
        )
        .bind(state_hash)
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        let deleted = query!(
            self,
            "DELETE FROM molcajete_oidc_login WHERE state_hash = ?"
        )
        .bind(state_hash)
        .execute(&self.pool)
        .await?;
        // Whoever deletes the row owns the sign-in; a concurrent duplicate
        // finds nothing left to delete.
        let expires_at: i64 = row.try_get("expires_at")?;
        if deleted.rows_affected() != 1 || expires_at <= now_millis() {
            return Err(DbError::NotFound);
        }
        Ok(PendingLogin {
            nonce: row.try_get("nonce")?,
            pkce_verifier: row.try_get("pkce_verifier")?,
        })
    }

    /// The user linked to an identity at a provider.
    pub async fn user_by_identity(&self, issuer: &str, subject: &str) -> DbResult<User> {
        let row = query!(
            self,
            "SELECT user_id FROM molcajete_user_identity WHERE issuer = ? AND subject = ?"
        )
        .bind(issuer)
        .bind(subject)
        .fetch_one(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        let user_id: String = row.try_get("user_id")?;
        self.user(&user_id).await
    }

    /// Whether the user is linked to some identity at this provider.
    pub async fn has_identity(&self, user_id: &str, issuer: &str) -> DbResult<bool> {
        let row = query!(
            self,
            "SELECT COUNT(*) AS n FROM molcajete_user_identity WHERE user_id = ? AND issuer = ?"
        )
        .bind(user_id)
        .bind(issuer)
        .fetch_one(&self.pool)
        .await?;
        Ok(row.try_get::<i64, _>("n")? > 0)
    }

    pub async fn link_identity(&self, issuer: &str, subject: &str, user_id: &str) -> DbResult<()> {
        query!(
            self,
            "INSERT INTO molcajete_user_identity (issuer, subject, user_id, created_at) \
             VALUES (?, ?, ?, ?)"
        )
        .bind(issuer)
        .bind(subject)
        .bind(user_id)
        .bind(now_millis())
        .execute(&self.pool)
        .await
        .map_err(DbError::from_sqlx)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::NewUser;
    use super::super::tests::every_backend;
    use super::*;

    #[tokio::test]
    async fn pending_logins_are_used_once() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let pending = PendingLogin {
                nonce: "n".into(),
                pkce_verifier: "v".into(),
            };
            db.create_oidc_login("s1", &pending, 60_000).await.unwrap();
            db.create_oidc_login("old", &pending, -1).await.unwrap();
            assert_eq!(db.take_oidc_login("s1").await.unwrap(), pending, "{name}");
            assert!(
                matches!(db.take_oidc_login("s1").await, Err(DbError::NotFound)),
                "{name}"
            );
            assert!(
                matches!(db.take_oidc_login("old").await, Err(DbError::NotFound)),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn identities_follow_the_user() {
        for (db, _dir) in every_backend().await {
            let name = db.dialect().name();
            let user = db
                .create_user(NewUser {
                    username: "ida",
                    password_hash: "!",
                    role: super::super::Role::User,
                    disabled: false,
                })
                .await
                .unwrap();
            assert!(!db.has_identity(&user.id, "https://idp").await.unwrap());
            db.link_identity("https://idp", "sub-1", &user.id)
                .await
                .unwrap();
            assert!(
                matches!(
                    db.link_identity("https://idp", "sub-1", &user.id).await,
                    Err(DbError::Conflict)
                ),
                "{name}"
            );
            assert!(db.has_identity(&user.id, "https://idp").await.unwrap());
            assert_eq!(
                db.user_by_identity("https://idp", "sub-1").await.unwrap(),
                user,
                "{name}"
            );
            assert!(
                matches!(
                    db.user_by_identity("https://other", "sub-1").await,
                    Err(DbError::NotFound)
                ),
                "{name}"
            );
            db.delete_user(&user.id).await.unwrap();
            assert!(db.user_by_identity("https://idp", "sub-1").await.is_err());
        }
    }
}
