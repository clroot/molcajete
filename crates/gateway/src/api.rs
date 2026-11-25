//! JSON API: login sessions, connections, folders, users, groups and recordings.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::AppState;
use crate::auth::{self, AdminUser, ConnectionManager, CurrentUser};
use crate::db::{
    Access, ConnectionRecord, DbError, Folder, Group, GroupUpdate, NewUser, Principal, Recording,
    Role, Target, User, UserUpdate, attributes,
};
use crate::recording;

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

    /// For a user whose role is below `required`.
    pub fn forbidden(required: Role) -> Self {
        let who = match required {
            Role::Admin => "Only administrators can do this.",
            _ => "Only connection managers and administrators can do this.",
        };
        Self::new(StatusCode::FORBIDDEN, who)
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
            DbError::LastAdmin => {
                Self::bad_request("At least one enabled administrator must remain.")
            }
            DbError::Invalid(message) => Self::bad_request(message),
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
    if !state.password_login() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Password sign-in is turned off; use single sign-on.",
        ));
    }
    let found = match state.db.user_credentials(body.username.trim()).await {
        Ok(found) => Some(found),
        Err(DbError::NotFound) => None,
        Err(err) => return Err(err.into()),
    };
    // Users without a password take as long to refuse as unknown ones.
    let hash = found
        .as_ref()
        .map(|(_, hash)| hash.as_str())
        .filter(|hash| *hash != auth::NO_PASSWORD)
        .unwrap_or_else(|| auth::dummy_hash())
        .to_owned();
    let valid = auth::verify_password_blocking(body.password, hash).await;
    let user = match found {
        Some((user, hash)) if valid && hash != auth::NO_PASSWORD && !user.disabled => user,
        _ => {
            warn!(username = %body.username, "login failed");
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Incorrect username or password.",
            ));
        }
    };

    let cookie = start_session(&state, &user, auth::is_https(&headers), None).await?;
    info!(username = %user.username, "login");
    Ok(([(header::SET_COOKIE, cookie)], Json(user)).into_response())
}

/// Opens a login session for the user and returns its cookie. `id_token` is
/// the provider's ID token for sessions opened through single sign-on.
pub async fn start_session(
    state: &AppState,
    user: &User,
    secure: bool,
    id_token: Option<&str>,
) -> ApiResult<HeaderValue> {
    let token = auth::new_token();
    let ttl_secs = state.config.auth.session_ttl_hours.max(1) * 3600;
    state
        .db
        .create_session(
            &auth::token_hash(&token),
            &user.id,
            ttl_secs as i64 * 1000,
            id_token,
        )
        .await?;
    state.db.record_login(&user.id).await?;
    Ok(auth::session_cookie(&token, ttl_secs, secure))
}

/// Ends the session. For a session opened through single sign-on at a
/// provider that supports RP-initiated logout, the response is
/// `{"logoutUrl": …}`: where to send the browser to sign out there too.
pub async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> ApiResult<Response> {
    let mut logout_url = None;
    if let Some(token) = auth::session_token(&headers) {
        let token_hash = auth::token_hash(token);
        let id_token = match state.db.session_id_token(&token_hash).await {
            Ok(id_token) => id_token,
            Err(DbError::NotFound) => None,
            Err(err) => return Err(err.into()),
        };
        state.db.delete_session(&token_hash).await?;
        if let (Some(oidc), Some(id_token)) = (&state.oidc, id_token) {
            logout_url = oidc.logout_url(&id_token).await;
        }
    }
    let cookie = auth::clear_session_cookie(auth::is_https(&headers));
    Ok(match logout_url {
        Some(url) => (
            [(header::SET_COOKIE, cookie)],
            Json(serde_json::json!({ "logoutUrl": url })),
        )
            .into_response(),
        None => ([(header::SET_COOKIE, cookie)], StatusCode::NO_CONTENT).into_response(),
    })
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
    Ok(Json(state.db.connection_summaries(&user).await?))
}

pub async fn get_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(_): ConnectionManager,
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
    c.parent_id = c.parent_id.filter(|p| !p.is_empty());
    // Attributes are flags the gateway acts on; unset ones are left out.
    for name in c.attributes.keys() {
        if !attributes::ALL.contains(&name.as_str()) {
            return Err(ApiError::bad_request(format!(
                "Unknown connection attribute {name:?}."
            )));
        }
    }
    c.attributes.retain(|_, value| value == "true");
    Ok(c)
}

pub async fn create_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(admin): ConnectionManager,
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
    ConnectionManager(admin): ConnectionManager,
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
    ConnectionManager(admin): ConnectionManager,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.db.delete_connection(&id).await?;
    info!(by = %admin.username, connection = %id, "connection deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentRequest {
    /// The folder to move into; `null` for the top level.
    parent_id: Option<String>,
}

fn parent(body: &ParentRequest) -> Option<&str> {
    body.parent_id.as_deref().filter(|p| !p.is_empty())
}

pub async fn move_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Path(id): Path<String>,
    Json(body): Json<ParentRequest>,
) -> ApiResult<StatusCode> {
    state.db.move_connection(&id, parent(&body)).await?;
    info!(by = %by.username, connection = %id, folder = ?parent(&body), "connection moved");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Access: who is granted a connection or a folder

pub async fn get_connection_access(
    State(state): State<Arc<AppState>>,
    ConnectionManager(_): ConnectionManager,
    Path(id): Path<String>,
) -> ApiResult<Json<Access>> {
    state.db.connection(&id).await?;
    Ok(Json(state.db.access(Target::Connection, &id).await?))
}

pub async fn set_connection_access(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Path(id): Path<String>,
    Json(body): Json<Access>,
) -> ApiResult<Json<Access>> {
    state.db.connection(&id).await?;
    state.db.set_access(Target::Connection, &id, &body).await?;
    info!(by = %by.username, connection = %id, "connection access changed");
    Ok(Json(state.db.access(Target::Connection, &id).await?))
}

pub async fn get_folder_access(
    State(state): State<Arc<AppState>>,
    ConnectionManager(_): ConnectionManager,
    Path(id): Path<String>,
) -> ApiResult<Json<Access>> {
    state.db.folder(&id).await?;
    Ok(Json(state.db.access(Target::Folder, &id).await?))
}

pub async fn set_folder_access(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Path(id): Path<String>,
    Json(body): Json<Access>,
) -> ApiResult<Json<Access>> {
    state.db.folder(&id).await?;
    state.db.set_access(Target::Folder, &id, &body).await?;
    info!(by = %by.username, folder = %id, "folder access changed");
    Ok(Json(state.db.access(Target::Folder, &id).await?))
}

// ---------------------------------------------------------------------------
// Folders

/// Every folder for connection managers and administrators; for other
/// users the folders that hold (at any depth) a connection they can open.
pub async fn list_folders(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<Folder>>> {
    let folders = state.db.list_folders().await?;
    if user.effective_role.can_manage_connections() {
        return Ok(Json(folders));
    }
    let parents: HashMap<&str, Option<&str>> = folders
        .iter()
        .map(|f| (f.id.as_str(), f.parent_id.as_deref()))
        .collect();
    let mut visible = HashSet::new();
    for connection in state.db.connection_summaries(&user).await? {
        let mut folder = connection.parent_id;
        while let Some(id) = folder {
            if !visible.insert(id.clone()) {
                break;
            }
            folder = parents
                .get(id.as_str())
                .copied()
                .flatten()
                .map(str::to_owned);
        }
    }
    Ok(Json(
        folders
            .into_iter()
            .filter(|f| visible.contains(&f.id))
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderRequest {
    name: String,
    /// Where a new folder goes; `null` for the top level.
    #[serde(default)]
    parent_id: Option<String>,
}

fn validate_folder_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 255 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request("The name needs 1–255 characters."));
    }
    Ok(name.to_owned())
}

pub async fn create_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Json(body): Json<FolderRequest>,
) -> ApiResult<(StatusCode, Json<Folder>)> {
    let name = validate_folder_name(&body.name)?;
    let parent_id = body.parent_id.as_deref().filter(|p| !p.is_empty());
    let folder = state.db.create_folder(&name, parent_id).await?;
    info!(by = %by.username, folder = %folder.id, name = %folder.name, "folder created");
    Ok((StatusCode::CREATED, Json(folder)))
}

/// Renames the folder; moving is `PUT /api/folders/{id}/parent`.
pub async fn rename_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Path(id): Path<String>,
    Json(body): Json<FolderRequest>,
) -> ApiResult<Json<Folder>> {
    let name = validate_folder_name(&body.name)?;
    let folder = state.db.rename_folder(&id, &name).await?;
    info!(by = %by.username, folder = %id, name = %folder.name, "folder renamed");
    Ok(Json(folder))
}

pub async fn move_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Path(id): Path<String>,
    Json(body): Json<ParentRequest>,
) -> ApiResult<Json<Folder>> {
    let folder = state.db.move_folder(&id, parent(&body)).await?;
    info!(by = %by.username, folder = %id, into = ?parent(&body), "folder moved");
    Ok(Json(folder))
}

/// Deletes the folder; what it holds moves up into its parent.
pub async fn delete_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    state.db.delete_folder(&id).await?;
    info!(by = %by.username, folder = %id, "folder deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Users

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UserDetail {
    #[serde(flatten)]
    user: User,
    /// Connections granted to the user directly.
    connections: Vec<String>,
    /// Folders granted to the user directly.
    folders: Vec<String>,
    /// Ids of the user's groups.
    groups: Vec<String>,
}

async fn user_detail(state: &AppState, user: User) -> ApiResult<UserDetail> {
    let grants = state.db.grants(Principal::User, &user.id).await?;
    let groups = state.db.user_group_ids(&user.id).await?;
    Ok(UserDetail {
        user,
        connections: grants.connections,
        folders: grants.folders,
        groups,
    })
}

/// Every user; connection managers need the list to grant connections.
pub async fn list_users(
    State(state): State<Arc<AppState>>,
    ConnectionManager(_): ConnectionManager,
) -> ApiResult<Json<Vec<User>>> {
    Ok(Json(state.db.list_users().await?))
}

pub async fn get_user(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<Json<UserDetail>> {
    let user = state.db.user(&id).await?;
    Ok(Json(user_detail(&state, user).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserRequest {
    username: Option<String>,
    /// Required when creating, unless single sign-on is configured (users
    /// without one sign in only through the provider); leaving it out keeps
    /// the current password.
    password: Option<String>,
    role: Option<Role>,
    /// Before roles: `true` is `role: "admin"`, `false` is `role: "user"`.
    /// Ignored when `role` is given.
    is_admin: Option<bool>,
    disabled: Option<bool>,
    /// Ids of the connections granted directly; leaving it out keeps them.
    connections: Option<Vec<String>>,
    /// Ids of the folders granted directly; leaving it out keeps them.
    folders: Option<Vec<String>>,
    /// Ids of the user's groups; leaving it out keeps them.
    groups: Option<Vec<String>>,
}

impl UserRequest {
    fn role(&self) -> Option<Role> {
        self.role.or(self
            .is_admin
            .map(|admin| if admin { Role::Admin } else { Role::User }))
    }
}

pub fn validate_username(name: &str) -> ApiResult<String> {
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

/// Applies the memberships and grants of a user or group request.
async fn apply_grants(
    state: &AppState,
    principal: Principal,
    id: &str,
    connections: Option<&[String]>,
    folders: Option<&[String]>,
) -> ApiResult<()> {
    if let Some(ids) = connections {
        state.db.set_granted_connections(principal, id, ids).await?;
    }
    if let Some(ids) = folders {
        state.db.set_granted_folders(principal, id, ids).await?;
    }
    Ok(())
}

pub async fn create_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Json(body): Json<UserRequest>,
) -> ApiResult<(StatusCode, Json<UserDetail>)> {
    let username = validate_username(body.username.as_deref().unwrap_or(""))?;
    let password = body.password.clone().unwrap_or_default();
    let hash = if password.is_empty() && state.oidc.is_some() {
        auth::NO_PASSWORD.to_owned()
    } else {
        auth::check_password_policy(&password)?;
        auth::hash_password_blocking(password).await?
    };
    let user = state
        .db
        .create_user(NewUser {
            username: &username,
            password_hash: &hash,
            role: body.role().unwrap_or_default(),
            disabled: body.disabled.unwrap_or(false),
        })
        .await
        .map_err(username_taken)?;
    if let Some(ids) = &body.groups {
        state.db.set_user_groups(&user.id, ids).await?;
    }
    apply_grants(
        &state,
        Principal::User,
        &user.id,
        body.connections.as_deref(),
        body.folders.as_deref(),
    )
    .await?;
    info!(by = %admin.username, username = %user.username, "user created");
    let user = state.db.user(&user.id).await?;
    Ok((StatusCode::CREATED, Json(user_detail(&state, user).await?)))
}

pub async fn update_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
    Json(body): Json<UserRequest>,
) -> ApiResult<Json<UserDetail>> {
    let current = state.db.user(&id).await?;
    let role = body.role();
    let demotes_self =
        role.is_some_and(|r| r != current.role && r != Role::Admin) || body.disabled == Some(true);
    if current.id == admin.id && demotes_self {
        return Err(ApiError::bad_request(
            "You cannot remove your own administrator access or disable yourself.",
        ));
    }
    let username = body
        .username
        .as_deref()
        .map(validate_username)
        .transpose()?;
    let hash = match body.password.clone().filter(|p| !p.is_empty()) {
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
                role,
                disabled: body.disabled,
            },
        )
        .await
        .map_err(username_taken)?;
    if let Some(ids) = &body.groups {
        state.db.set_user_groups(&id, ids).await?;
    }
    apply_grants(
        &state,
        Principal::User,
        &id,
        body.connections.as_deref(),
        body.folders.as_deref(),
    )
    .await?;
    info!(by = %admin.username, username = %user.username, "user updated");
    let user = state.db.user(&id).await?;
    Ok(Json(user_detail(&state, user).await?))
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
    state.db.delete_user(&id).await?;
    info!(by = %admin.username, username = %user.username, "user deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Groups

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupDetail {
    #[serde(flatten)]
    group: Group,
    /// Ids of the members.
    members: Vec<String>,
    /// Connections granted to the group directly.
    connections: Vec<String>,
    /// Folders granted to the group directly.
    folders: Vec<String>,
}

async fn group_detail(state: &AppState, id: &str) -> ApiResult<GroupDetail> {
    let group = state.db.group(id).await?;
    let grants = state.db.grants(Principal::Group, id).await?;
    Ok(GroupDetail {
        group,
        members: state.db.group_member_ids(id).await?,
        connections: grants.connections,
        folders: grants.folders,
    })
}

/// Every group; connection managers need the list to grant connections.
pub async fn list_groups(
    State(state): State<Arc<AppState>>,
    ConnectionManager(_): ConnectionManager,
) -> ApiResult<Json<Vec<Group>>> {
    Ok(Json(state.db.list_groups().await?))
}

pub async fn get_group(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<Json<GroupDetail>> {
    Ok(Json(group_detail(&state, &id).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRequest {
    name: Option<String>,
    role: Option<Role>,
    /// Ids of the members; leaving it out keeps them.
    members: Option<Vec<String>>,
    /// Ids of the connections granted; leaving it out keeps them.
    connections: Option<Vec<String>>,
    /// Ids of the folders granted; leaving it out keeps them.
    folders: Option<Vec<String>>,
}

fn validate_group_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() || name.chars().count() > 128 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request(
            "The group name needs 1–128 characters.",
        ));
    }
    Ok(name.to_owned())
}

fn group_name_taken(err: DbError) -> ApiError {
    match err {
        DbError::Conflict => {
            ApiError::new(StatusCode::CONFLICT, "That group name is already taken.")
        }
        err => err.into(),
    }
}

pub async fn create_group(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Json(body): Json<GroupRequest>,
) -> ApiResult<(StatusCode, Json<GroupDetail>)> {
    let name = validate_group_name(body.name.as_deref().unwrap_or(""))?;
    let group = state
        .db
        .create_group(&name, body.role.unwrap_or_default())
        .await
        .map_err(group_name_taken)?;
    if let Some(ids) = &body.members {
        state.db.set_group_members(&group.id, ids).await?;
    }
    apply_grants(
        &state,
        Principal::Group,
        &group.id,
        body.connections.as_deref(),
        body.folders.as_deref(),
    )
    .await?;
    info!(by = %admin.username, group = %group.name, "group created");
    Ok((
        StatusCode::CREATED,
        Json(group_detail(&state, &group.id).await?),
    ))
}

pub async fn update_group(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
    Json(body): Json<GroupRequest>,
) -> ApiResult<Json<GroupDetail>> {
    let name = body.name.as_deref().map(validate_group_name).transpose()?;
    let group = state
        .db
        .update_group(
            &id,
            GroupUpdate {
                name: name.as_deref(),
                role: body.role,
            },
        )
        .await
        .map_err(group_name_taken)?;
    if let Some(ids) = &body.members {
        state.db.set_group_members(&id, ids).await?;
    }
    apply_grants(
        &state,
        Principal::Group,
        &id,
        body.connections.as_deref(),
        body.folders.as_deref(),
    )
    .await?;
    info!(by = %admin.username, group = %group.name, "group updated");
    Ok(Json(group_detail(&state, &id).await?))
}

pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let group = state.db.group(&id).await?;
    state.db.delete_group(&id).await?;
    info!(by = %admin.username, group = %group.name, "group deleted");
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Recordings (administrators only)

fn recordings_dir(state: &AppState) -> std::path::PathBuf {
    state.config.recordings.directory(&state.config.server)
}

/// A recording still being written reports the size of its file so far.
async fn with_current_size(state: &AppState, mut recording: Recording) -> Recording {
    if recording.ended_at.is_none()
        && let Some(path) = recording::file_path(&recordings_dir(state), &recording)
        && let Ok(metadata) = tokio::fs::metadata(path).await
    {
        recording.size_bytes = metadata.len() as i64;
    }
    recording
}

pub async fn list_recordings(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
) -> ApiResult<Json<Vec<Recording>>> {
    let mut recordings = Vec::new();
    for recording in state.db.list_recordings().await? {
        recordings.push(with_current_size(&state, recording).await);
    }
    Ok(Json(recordings))
}

pub async fn get_recording(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Recording>> {
    let recording = state.db.recording(&id).await?;
    Ok(Json(with_current_size(&state, recording).await))
}

#[derive(Deserialize)]
pub struct FileQuery {
    /// Ask the browser to save the file instead of handing it to the page.
    #[serde(default)]
    download: bool,
}

/// The recording file, with range requests (`ServeFile`).
pub async fn recording_file(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    request: Request,
) -> ApiResult<Response> {
    use tower::ServiceExt as _;

    let recording = state.db.recording(&id).await?;
    let path = recording::file_path(&recordings_dir(&state), &recording)
        .ok_or_else(ApiError::not_found)?;
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "The recording file is missing.",
        ));
    }
    let mut response = tower_http::services::ServeFile::new(&path)
        .oneshot(request)
        .await
        .unwrap_or_else(|never| match never {})
        .map(axum::body::Body::new);
    if query.download {
        let disposition = format!("attachment; filename=\"{}\"", recording.file_name);
        if let Ok(value) = HeaderValue::from_str(&disposition) {
            response
                .headers_mut()
                .insert(header::CONTENT_DISPOSITION, value);
        }
        info!(by = %admin.username, recording = %id, "recording downloaded");
    }
    Ok(response)
}

pub async fn delete_recording(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let recording = state.db.recording(&id).await?;
    if recording.ended_at.is_none() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "The session is still being recorded.",
        ));
    }
    recording::delete(&state.db, &recordings_dir(&state), &recording).await?;
    info!(by = %admin.username, recording = %id, file = %recording.file_name, "recording deleted");
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
            parent_id: Some(String::new()),
            attributes: BTreeMap::from([
                (attributes::RECORD.into(), "true".into()),
                (attributes::RECORD_KEYS.into(), "false".into()),
            ]),
        }
    }

    #[test]
    fn validates_connections() {
        let c = validate_connection(record("desk-1")).unwrap();
        assert_eq!((c.name.as_str(), c.protocol.as_str()), ("Desktop", "vnc"));
        assert_eq!((c.guacd_host, c.guacd_port), (None, None));
        assert_eq!(c.parent_id, None);
        assert_eq!(
            c.parameters,
            BTreeMap::from([("hostname".into(), "10.0.0.5".into())])
        );
        assert_eq!(
            c.attributes,
            BTreeMap::from([(attributes::RECORD.into(), "true".into())])
        );
        let mut unknown = record("x");
        unknown.attributes.insert("hostname".into(), "true".into());
        assert!(validate_connection(unknown).is_err());
        assert!(validate_connection(record("has space")).is_err());
        assert!(validate_connection(record("")).is_err());
        let mut bad = record("x");
        bad.protocol = "ftp".into();
        assert!(validate_connection(bad).is_err());
    }
}
