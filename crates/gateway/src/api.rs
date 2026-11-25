//! JSON API: login sessions, connections and users.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::AppState;
use crate::auth::{self, AdminUser, CurrentUser};
use crate::db::{ConnectionRecord, DbError, NewUser, User, UserUpdate};

/// Error response: `{"error": "<message>"}` with a status code.
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Sign in to continue.")
    }

    pub fn forbidden() -> Self {
        Self::new(StatusCode::FORBIDDEN, "Only administrators can do this.")
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "Not found.")
    }
}

impl From<DbError> for ApiError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => Self::not_found(),
            DbError::Conflict => Self::new(StatusCode::CONFLICT, "That name is already taken."),
            DbError::Other(err) => {
                error!(error = %err, "database error");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Database error.")
            }
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        error!(error = %err, "request failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "Internal error.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// Sessions

#[derive(Deserialize)]
pub struct LoginRequest {
    username: String,
    password: String,
}

pub async fn login(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Response> {
    let found = match state.db.user_credentials(body.username.trim()).await {
        Ok(found) => Some(found),
        Err(DbError::NotFound) => None,
        Err(err) => return Err(err.into()),
    };
    let hash = found
        .as_ref()
        .map_or_else(|| auth::dummy_hash().to_owned(), |(_, hash)| hash.clone());
    let valid = auth::verify_password_blocking(body.password, hash).await;
    let user = match found {
        Some((user, _)) if valid && !user.disabled => user,
        _ => {
            warn!(username = %body.username, "login failed");
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Incorrect username or password.",
            ));
        }
    };

    let token = auth::new_token();
    let ttl_secs = state.config.auth.session_ttl_hours.max(1) * 3600;
    state
        .db
        .create_session(&auth::token_hash(&token), &user.id, ttl_secs as i64 * 1000)
        .await?;
    state.db.record_login(&user.id).await?;
    info!(username = %user.username, "login");
    let cookie = auth::session_cookie(&token, ttl_secs, auth::is_https(&headers));
    Ok(([(header::SET_COOKIE, cookie)], Json(user)).into_response())
}

pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    if let Some(token) = auth::session_token(&headers) {
        state.db.delete_session(&auth::token_hash(token)).await?;
    }
    let cookie = auth::clear_session_cookie(auth::is_https(&headers));
    Ok(([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response())
}

pub async fn me(CurrentUser(user): CurrentUser) -> Json<User> {
    Json(user)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangePasswordRequest {
    current_password: String,
    new_password: String,
}

pub async fn change_password(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    Json(body): Json<ChangePasswordRequest>,
) -> ApiResult<StatusCode> {
    let hash = state.db.password_hash(&user.id).await?;
    if !auth::verify_password_blocking(body.current_password, hash).await {
        return Err(ApiError::bad_request(
            "The current password is not correct.",
        ));
    }
    auth::check_password_policy(&body.new_password)?;
    let hash = auth::hash_password_blocking(body.new_password).await?;
    state
        .db
        .update_user(
            &user.id,
            UserUpdate {
                password_hash: Some(&hash),
                ..Default::default()
            },
        )
        .await?;
    info!(username = %user.username, "password changed");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Connections

pub async fn list_connections(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<impl IntoResponse> {
    Ok(Json(
        state
            .db
            .connection_summaries(&user.id, user.is_admin)
            .await?,
    ))
}

pub async fn get_connection(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<Json<ConnectionRecord>> {
    Ok(Json(state.db.connection(&id).await?))
}

pub const PROTOCOLS: &[&str] = &["vnc", "rdp", "ssh", "telnet", "kubernetes"];

/// Trims and checks a connection from the browser.
fn validate_connection(mut c: ConnectionRecord) -> ApiResult<ConnectionRecord> {
    c.id = c.id.trim().to_owned();
    c.name = c.name.trim().to_owned();
    c.protocol = c.protocol.trim().to_ascii_lowercase();
    let id_ok = !c.id.is_empty()
        && c.id.len() <= 64
        && c.id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if !id_ok {
        return Err(ApiError::bad_request(
            "The id needs 1–64 letters, digits, '-', '_' or '.'.",
        ));
    }
    if c.name.is_empty() || c.name.chars().count() > 255 {
        return Err(ApiError::bad_request("The name needs 1–255 characters."));
    }
    if !PROTOCOLS.contains(&c.protocol.as_str()) {
        return Err(ApiError::bad_request(format!(
            "Unsupported protocol {:?}.",
            c.protocol
        )));
    }
    c.guacd_host = c
        .guacd_host
        .map(|h| h.trim().to_owned())
        .filter(|h| !h.is_empty());
    if c.guacd_host.is_none() {
        c.guacd_port = None;
    }
    let mut parameters = BTreeMap::new();
    for (name, value) in c.parameters {
        let name = name.trim().to_owned();
        if name.is_empty() || value.is_empty() {
            continue;
        }
        if name.len() > 128 {
            return Err(ApiError::bad_request(format!(
                "Parameter name {name:?} is too long."
            )));
        }
        parameters.insert(name, value);
    }
    c.parameters = parameters;
    Ok(c)
}

pub async fn create_connection(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Json(body): Json<ConnectionRecord>,
) -> ApiResult<(StatusCode, Json<ConnectionRecord>)> {
    let c = validate_connection(body)?;
    let created = state
        .db
        .create_connection(&c)
        .await
        .map_err(|err| match err {
            DbError::Conflict => ApiError::new(
                StatusCode::CONFLICT,
                format!("A connection with the id {:?} already exists.", c.id),
            ),
            err => err.into(),
        })?;
    info!(by = %admin.username, connection = %created.id, "connection created");
    Ok((StatusCode::CREATED, Json(created)))
}

pub async fn update_connection(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
    Json(mut body): Json<ConnectionRecord>,
) -> ApiResult<Json<ConnectionRecord>> {
    body.id = id;
    let c = validate_connection(body)?;
    let updated = state.db.update_connection(&c).await?;
    info!(by = %admin.username, connection = %updated.id, "connection updated");
    Ok(Json(updated))
}

pub async fn delete_connection(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.db.delete_connection(&id).await?;
    info!(by = %admin.username, connection = %id, "connection deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Users

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDetail {
    #[serde(flatten)]
    user: User,
    connections: Vec<String>,
}

pub async fn list_users(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
) -> ApiResult<Json<Vec<User>>> {
    Ok(Json(state.db.list_users().await?))
}

pub async fn get_user(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<Json<UserDetail>> {
    let user = state.db.user(&id).await?;
    let connections = state.db.user_connection_ids(&id).await?;
    Ok(Json(UserDetail { user, connections }))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRequest {
    username: Option<String>,
    /// Required when creating; leaving it out keeps the current password.
    password: Option<String>,
    is_admin: Option<bool>,
    disabled: Option<bool>,
    /// Ids of the granted connections; leaving it out keeps them.
    connections: Option<Vec<String>>,
}

fn validate_username(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "The username needs 1–128 characters.",
        ));
    }
    Ok(name.to_owned())
}

fn username_taken(err: DbError) -> ApiError {
    match err {
        DbError::Conflict => ApiError::new(StatusCode::CONFLICT, "That username is already taken."),
        err => err.into(),
    }
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Json(body): Json<UserRequest>,
) -> ApiResult<(StatusCode, Json<UserDetail>)> {
    let username = validate_username(body.username.as_deref().unwrap_or(""))?;
    let password = body.password.unwrap_or_default();
    auth::check_password_policy(&password)?;
    let hash = auth::hash_password_blocking(password).await?;
    let user = state
        .db
        .create_user(NewUser {
            username: &username,
            password_hash: &hash,
            is_admin: body.is_admin.unwrap_or(false),
            disabled: body.disabled.unwrap_or(false),
        })
        .await
        .map_err(username_taken)?;
    if let Some(ids) = &body.connections {
        state.db.set_user_connections(&user.id, ids).await?;
    }
    info!(by = %admin.username, username = %user.username, "user created");
    let connections = state.db.user_connection_ids(&user.id).await?;
    Ok((StatusCode::CREATED, Json(UserDetail { user, connections })))
}

pub async fn update_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
    Json(body): Json<UserRequest>,
) -> ApiResult<Json<UserDetail>> {
    let current = state.db.user(&id).await?;
    let demotes = body.is_admin == Some(false) || body.disabled == Some(true);
    if demotes && current.is_admin && !current.disabled {
        if current.id == admin.id {
            return Err(ApiError::bad_request(
                "You cannot remove your own administrator access or disable yourself.",
            ));
        }
        if state.db.count_other_admins(&current.id).await? == 0 {
            return Err(ApiError::bad_request(
                "At least one enabled administrator must remain.",
            ));
        }
    }
    let username = body
        .username
        .as_deref()
        .map(validate_username)
        .transpose()?;
    let hash = match body.password.filter(|p| !p.is_empty()) {
        Some(password) => {
            auth::check_password_policy(&password)?;
            Some(auth::hash_password_blocking(password).await?)
        }
        None => None,
    };
    let user = state
        .db
        .update_user(
            &id,
            UserUpdate {
                username: username.as_deref(),
                password_hash: hash.as_deref(),
                is_admin: body.is_admin,
                disabled: body.disabled,
            },
        )
        .await
        .map_err(username_taken)?;
    if let Some(ids) = &body.connections {
        state.db.set_user_connections(&id, ids).await?;
    }
    info!(by = %admin.username, username = %user.username, "user updated");
    let connections = state.db.user_connection_ids(&id).await?;
    Ok(Json(UserDetail { user, connections }))
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if id == admin.id {
        return Err(ApiError::bad_request("You cannot delete your own account."));
    }
    let user = state.db.user(&id).await?;
    if user.is_admin && !user.disabled && state.db.count_other_admins(&id).await? == 0 {
        return Err(ApiError::bad_request(
            "At least one enabled administrator must remain.",
        ));
    }
    state.db.delete_user(&id).await?;
    info!(by = %admin.username, username = %user.username, "user deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str) -> ConnectionRecord {
        ConnectionRecord {
            id: id.into(),
            name: " Desktop ".into(),
            protocol: "VNC".into(),
            guacd_host: Some(" ".into()),
            guacd_port: Some(4823),
            parameters: BTreeMap::from([
                (" hostname ".into(), "10.0.0.5".into()),
                ("empty".into(), String::new()),
            ]),
        }
    }

    #[test]
    fn validates_connections() {
        let c = validate_connection(record("desk-1")).unwrap();
        assert_eq!((c.name.as_str(), c.protocol.as_str()), ("Desktop", "vnc"));
        assert_eq!((c.guacd_host, c.guacd_port), (None, None));
        assert_eq!(
            c.parameters,
            BTreeMap::from([("hostname".into(), "10.0.0.5".into())])
        );
        assert!(validate_connection(record("has space")).is_err());
        assert!(validate_connection(record("")).is_err());
        let mut bad = record("x");
        bad.protocol = "ftp".into();
        assert!(validate_connection(bad).is_err());
    }
}
