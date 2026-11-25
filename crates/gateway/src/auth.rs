//! Passwords, login sessions and the request extractors that enforce them.
//!
//! A login sets an opaque random token in an `HttpOnly`, `SameSite=Strict`
//! cookie; the database stores only its SHA-256. Requests that change state
//! must be JSON (cross-site forms cannot send that without a CORS
//! preflight) and, like the WebSocket tunnel, come from the gateway's own
//! origin when the browser says where they come from.
//!
//! Requests with an API token (`Authorization: Bearer …`) are
//! authenticated by [`crate::tokens`] instead; the extractors here take the
//! token's owner from the request then.

use std::convert::Infallible;
use std::sync::Arc;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher as _, PasswordVerifier as _};
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, header};
use rand::RngCore as _;
use sha2::{Digest as _, Sha256};

use crate::AppState;
use crate::api::ApiError;
use crate::db::{DbError, Role, User};
use crate::tokens::TokenAuth;

pub const SESSION_COOKIE: &str = "molcajete_session";
pub const MIN_PASSWORD_LENGTH: usize = 8;
/// Stored instead of a hash for users without a password (created through
/// single sign-on); no password matches it.
pub const NO_PASSWORD: &str = "!";

pub fn hash_password(password: &str) -> anyhow::Result<String> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|err| anyhow::anyhow!("hashing password: {err}"))
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|parsed| {
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    })
}

/// Argon2 is deliberately slow; keep it off the async workers.
pub async fn hash_password_blocking(password: String) -> anyhow::Result<String> {
    tokio::task::spawn_blocking(move || hash_password(&password)).await?
}

pub async fn verify_password_blocking(password: String, hash: String) -> bool {
    tokio::task::spawn_blocking(move || verify_password(&password, &hash))
        .await
        .unwrap_or(false)
}

/// A hash to verify against when the user does not exist, so that the
/// response time does not reveal which usernames are valid.
pub fn dummy_hash() -> &'static str {
    static HASH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    HASH.get_or_init(|| hash_password("molcajete-dummy-password").unwrap_or_default())
}

pub fn new_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

pub fn token_hash(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

pub fn random_password() -> String {
    use rand::distr::{Alphanumeric, SampleString as _};
    Alphanumeric.sample_string(&mut rand::rng(), 20)
}

pub fn check_password_policy(password: &str) -> Result<(), ApiError> {
    if password.chars().count() < MIN_PASSWORD_LENGTH {
        return Err(ApiError::bad_request(format!(
            "Passwords need at least {MIN_PASSWORD_LENGTH} characters."
        )));
    }
    Ok(())
}

/// A non-empty cookie from the `Cookie` header.
pub fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, value)| value)
        .filter(|v| !v.is_empty())
}

/// The session token from the `Cookie` header.
pub fn session_token(headers: &HeaderMap) -> Option<&str> {
    cookie(headers, SESSION_COOKIE)
}

/// Whether the browser reached us over HTTPS, possibly through a proxy.
pub fn is_https(headers: &HeaderMap) -> bool {
    let value = |name| headers.get(name).and_then(|v| v.to_str().ok());
    value("x-forwarded-proto").is_some_and(|v| v.trim().eq_ignore_ascii_case("https"))
        || value(header::ORIGIN.as_str()).is_some_and(|v| v.starts_with("https://"))
}

/// The browser's address for logs and the audit log, as
/// [`crate::proxy::resolve`] finds it behind the trusted proxies.
pub struct ClientAddress(pub String);

impl FromRequestParts<Arc<AppState>> for ClientAddress {
    type Rejection = Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        let client = crate::proxy::Client::from_request_parts(parts, state).await?;
        // What is done with an API token says which one.
        Ok(Self(match parts.extensions.get::<TokenAuth>() {
            Some(auth) => format!("{} (API token {:?})", client.display, auth.token.name),
            None => client.display,
        }))
    }
}

pub fn session_cookie(token: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={max_age_secs}{secure}"
    ))
    .expect("cookie is ASCII")
}

pub fn clear_session_cookie(secure: bool) -> HeaderValue {
    session_cookie("", 0, secure)
}

/// False when the browser says the request comes from another origin.
///
/// The `Origin` host must match `Host` (or `X-Forwarded-Host`, for proxies
/// that rewrite `Host`). Requests without `Origin` (same-origin GETs from
/// older browsers, command-line clients) are allowed.
pub fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return true;
    };
    let Some(origin_host) = origin.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let origin_host = origin_host.trim_end_matches('/');
    ["x-forwarded-host", header::HOST.as_str()]
        .iter()
        .filter_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
        .flat_map(|v| v.split(','))
        .any(|host| host.trim().eq_ignore_ascii_case(origin_host))
}

pub async fn user_from_headers(state: &AppState, headers: &HeaderMap) -> Result<User, ApiError> {
    let token = session_token(headers).ok_or_else(ApiError::unauthorized)?;
    match state.db.session_user(&token_hash(token)).await {
        Ok(user) => Ok(user),
        Err(DbError::NotFound) => Err(ApiError::unauthorized()),
        Err(err) => Err(err.into()),
    }
}

/// The owner of the request's API token (see [`crate::tokens`]), otherwise
/// the user of the session cookie.
async fn user_from_parts(state: &AppState, parts: &Parts) -> Result<User, ApiError> {
    match parts.extensions.get::<TokenAuth>() {
        Some(auth) => Ok(auth.user.clone()),
        None => user_from_headers(state, &parts.headers).await,
    }
}

/// The signed-in user (or an API token's owner); rejects the request with
/// 401 otherwise.
pub struct CurrentUser(pub User);

impl FromRequestParts<Arc<AppState>> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        user_from_parts(state, parts).await.map(Self)
    }
}

/// The signed-in user if their effective role is at least `role`; 401 when
/// signed out, 403 for users with a lesser role.
async fn user_with_role(parts: &Parts, state: &AppState, role: Role) -> Result<User, ApiError> {
    let user = user_from_parts(state, parts).await?;
    if user.effective_role < role {
        return Err(ApiError::forbidden(role));
    }
    Ok(user)
}

/// A signed-in administrator ([`Role::Admin`]): manages users, groups and
/// roles, and everything a [`ConnectionManager`] does.
pub struct AdminUser(pub User);

impl FromRequestParts<Arc<AppState>> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        user_with_role(parts, state, Role::Admin).await.map(Self)
    }
}

/// A signed-in connection manager or administrator
/// ([`Role::ConnectionManager`] or higher): manages connections and
/// folders and grants them to users and groups.
pub struct ConnectionManager(pub User);

impl FromRequestParts<Arc<AppState>> for ConnectionManager {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<AppState>,
    ) -> Result<Self, Self::Rejection> {
        user_with_role(parts, state, Role::ConnectionManager)
            .await
            .map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_and_verifies_passwords() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$"));
        assert!(verify_password("correct horse", &hash));
        assert!(!verify_password("wrong", &hash));
        assert!(!verify_password("correct horse", "not a hash"));
        assert!(!verify_password("", NO_PASSWORD));
        assert!(!verify_password(NO_PASSWORD, NO_PASSWORD));
    }

    #[test]
    fn tokens_are_random_and_hashed() {
        let (a, b) = (new_token(), new_token());
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_eq!(token_hash(&a), token_hash(&a));
        assert_ne!(token_hash(&a), a);
    }

    #[test]
    fn reads_the_session_cookie() {
        let mut headers = HeaderMap::new();
        assert_eq!(session_token(&headers), None);
        headers.insert(
            header::COOKIE,
            "a=1; molcajete_session=tok; b=2".parse().unwrap(),
        );
        assert_eq!(session_token(&headers), Some("tok"));
    }

    #[test]
    fn origin_must_match_host() {
        let mut headers = HeaderMap::new();
        assert!(same_origin(&headers));
        headers.insert(header::HOST, "gw.example:8080".parse().unwrap());
        headers.insert(header::ORIGIN, "http://gw.example:8080".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert("x-forwarded-host", "evil.example".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "null".parse().unwrap());
        assert!(!same_origin(&headers));
    }

    #[test]
    fn secure_cookie_behind_https_proxy() {
        let mut headers = HeaderMap::new();
        assert!(!is_https(&headers));
        headers.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(is_https(&headers));
        let cookie = session_cookie("t", 60, true);
        assert_eq!(
            cookie.to_str().unwrap(),
            "molcajete_session=t; Path=/; HttpOnly; SameSite=Strict; Max-Age=60; Secure"
        );
    }
}
