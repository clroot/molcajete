//! JSON API: login sessions, connections, folders, users, groups,
//! recordings and the audit log.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::error;

use crate::AppState;
use crate::audit::{self, Entry, action, target};
use crate::auth::{self, AdminUser, ClientAddress, ConnectionManager, CurrentUser};
use crate::db::{
    Access, ConnectionRecord, DbError, Folder, Group, GroupUpdate, NewUser, Principal, Recording,
    Role, Target, User, UserUpdate, attributes,
};
use crate::proxy::Client;
use crate::recording;
use crate::sessions;
use crate::throttle;

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
            DbError::Secret(err) => {
                error!(error = %err, "stored connection credentials");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Stored connection credentials could not be read.",
                )
            }
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
    address: ClientAddress,
    client: Client,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> ApiResult<Response> {
    if !state.password_login() {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Password sign-in is turned off; use single sign-on.",
        ));
    }
    if let Err(throttled) = state.throttle.check(&body.username, client.ip) {
        return Ok(throttle::refuse(&state, &body.username, &address, throttled).await);
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
    let valid = match state.throttle.verify(body.password, hash).await {
        Ok(valid) => valid,
        Err(busy) => return Ok(throttle::refuse(&state, &body.username, &address, busy).await),
    };
    let refused = match &found {
        None => Some((
            "unknown_user",
            Entry::new(action::LOGIN_FAILURE).by_name(&body.username),
        )),
        Some((user, hash)) => {
            let reason = if hash == auth::NO_PASSWORD {
                Some("no_password")
            } else if !valid {
                Some("wrong_password")
            } else if user.disabled {
                Some("disabled")
            } else {
                None
            };
            reason.map(|reason| (reason, Entry::new(action::LOGIN_FAILURE).by(user)))
        }
    };
    let user = match (found, refused) {
        (Some((user, _)), None) => user,
        (_, refused) => {
            state.throttle.failed(&body.username, client.ip);
            if let Some((reason, entry)) = refused {
                let entry = entry.from(&address).detail("reason", reason);
                audit::record(&state.db, entry).await;
            }
            return Err(ApiError::new(
                StatusCode::UNAUTHORIZED,
                "Incorrect username or password.",
            ));
        }
    };

    state.throttle.succeeded(&body.username, client.ip);
    let cookie = start_session(&state, &user, auth::is_https(&headers), None).await?;
    let entry = Entry::new(action::LOGIN_SUCCESS).by(&user).from(&address);
    audit::record(&state.db, entry).await;
    let me = Me {
        user,
        has_password: true,
    };
    Ok(([(header::SET_COOKIE, cookie)], Json(me)).into_response())
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
pub async fn logout(
    State(state): State<Arc<AppState>>,
    address: ClientAddress,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let mut logout_url = None;
    if let Some(token) = auth::session_token(&headers) {
        let token_hash = auth::token_hash(token);
        let user = match state.db.session_user(&token_hash).await {
            Ok(user) => Some(user),
            Err(DbError::NotFound) => None,
            Err(err) => return Err(err.into()),
        };
        let id_token = match state.db.session_id_token(&token_hash).await {
            Ok(id_token) => id_token,
            Err(DbError::NotFound) => None,
            Err(err) => return Err(err.into()),
        };
        state.db.delete_session(&token_hash).await?;
        if let Some(user) = &user {
            let entry = Entry::new(action::LOGOUT).by(user).from(&address);
            audit::record(&state.db, entry).await;
        }
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

/// The signed-in user, as `GET /api/auth/me` and the password login return
/// it.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Me {
    #[serde(flatten)]
    user: User,
    /// Whether the user has a password of their own, which they can change.
    /// Users created through single sign-on, or by an administrator without
    /// a password, have none.
    has_password: bool,
}

pub async fn me(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Me>> {
    let has_password = state.db.password_hash(&user.id).await? != auth::NO_PASSWORD;
    Ok(Json(Me { user, has_password }))
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
    address: ClientAddress,
    Json(body): Json<ChangePasswordRequest>,
) -> ApiResult<StatusCode> {
    let hash = state.db.password_hash(&user.id).await?;
    if hash == auth::NO_PASSWORD {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "This account has no password to change: it signs in through single sign-on, \
             or an administrator sets its password.",
        ));
    }
    let valid = state
        .throttle
        .verify(body.current_password, hash)
        .await
        .map_err(|_| {
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "The server is busy. Try again in a moment.",
            )
        })?;
    if !valid {
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
    let entry = Entry::new(action::USER_PASSWORD)
        .by(&user)
        .from(&address)
        .target(target::USER, &user.id, &user.username);
    audit::record(&state.db, entry).await;
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

/// Stored credentials are never returned, to anyone: the connection names
/// them in `storedSecrets` instead.
pub async fn get_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(_): ConnectionManager,
    Path(id): Path<String>,
) -> ApiResult<Json<ConnectionRecord>> {
    Ok(Json(state.db.connection(&id).await?))
}

pub const PROTOCOLS: &[&str] = &["vnc", "rdp", "ssh", "telnet", "kubernetes"];

/// The longest handshake timeout a connection may ask for, in seconds.
const MAX_HANDSHAKE_TIMEOUT: u64 = 3600;

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
    if c.guacd_handshake_timeout
        .is_some_and(|t| !(1..=MAX_HANDSHAKE_TIMEOUT).contains(&t))
    {
        return Err(ApiError::bad_request(format!(
            "The handshake timeout needs 1–{MAX_HANDSHAKE_TIMEOUT} seconds."
        )));
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
    c.clear_secrets = c
        .clear_secrets
        .into_iter()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
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
    let ironrdp = c.flag(attributes::IRONRDP_DEFAULT) || c.flag(attributes::IRONRDP_SWITCH);
    if ironrdp && c.protocol != "rdp" {
        return Err(ApiError::bad_request(
            "Only RDP connections can use IronRDP's client.",
        ));
    }
    if ironrdp && c.flag(attributes::RECORD) {
        return Err(ApiError::bad_request(
            "Sessions with IronRDP's client cannot be recorded; turn off one of them.",
        ));
    }
    Ok(c)
}

pub async fn create_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(admin): ConnectionManager,
    address: ClientAddress,
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
    let entry = Entry::new(action::CONNECTION_CREATE)
        .by(&admin)
        .from(&address)
        .target(target::CONNECTION, &created.id, &created.name)
        .changes(audit::diff(
            &Value::Null,
            &audit_view(&created, &BTreeSet::new()),
        ));
    audit::record(&state.db, entry).await;
    Ok((StatusCode::CREATED, Json(created)))
}

/// A connection as the audit log sees it: each stored credential as
/// `"stored"`, or `"replaced"` when `replaced` names it, so the log shows
/// that a credential was set, changed or removed but never its value
/// (encrypted or not). `storedSecrets` is left out, being the same news.
fn audit_view(c: &ConnectionRecord, replaced: &BTreeSet<String>) -> Value {
    let mut value = json!(c);
    if let Some(map) = value.as_object_mut() {
        map.remove("storedSecrets");
        if let Some(Value::Object(parameters)) = map.get_mut("parameters") {
            for name in &c.stored_secrets {
                let mark = if replaced.contains(name) {
                    "replaced"
                } else {
                    "stored"
                };
                parameters.insert(name.clone(), mark.into());
            }
        }
    }
    value
}

pub async fn update_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(admin): ConnectionManager,
    address: ClientAddress,
    Path(id): Path<String>,
    Json(mut body): Json<ConnectionRecord>,
) -> ApiResult<Json<ConnectionRecord>> {
    body.id = id;
    let c = validate_connection(body)?;
    let before = state.db.connection(&c.id).await?;
    let updated = state.db.update_connection(&c).await?;
    // Credentials given a value: the audit log shows they changed.
    let replaced = c
        .parameters
        .keys()
        .filter(|name| updated.stored_secrets.contains(*name))
        .cloned()
        .collect();
    let entry = Entry::new(action::CONNECTION_UPDATE)
        .by(&admin)
        .from(&address)
        .target(target::CONNECTION, &updated.id, &updated.name)
        .changes(audit::diff(
            &audit_view(&before, &BTreeSet::new()),
            &audit_view(&updated, &replaced),
        ));
    audit::record(&state.db, entry).await;
    Ok(Json(updated))
}

pub async fn delete_connection(
    State(state): State<Arc<AppState>>,
    ConnectionManager(admin): ConnectionManager,
    address: ClientAddress,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let before = state.db.connection(&id).await?;
    state.db.delete_connection(&id).await?;
    let entry = Entry::new(action::CONNECTION_DELETE)
        .by(&admin)
        .from(&address)
        .target(target::CONNECTION, &id, &before.name)
        .changes(audit::diff(
            &audit_view(&before, &BTreeSet::new()),
            &Value::Null,
        ));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &admin, &address).await;
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
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<ParentRequest>,
) -> ApiResult<StatusCode> {
    let before = state.db.connection(&id).await?;
    state.db.move_connection(&id, parent(&body)).await?;
    let names = Names::load(&state).await?;
    let entry = Entry::new(action::CONNECTION_MOVE)
        .by(&by)
        .from(&address)
        .target(target::CONNECTION, &id, &before.name)
        .changes(audit::diff(
            &json!({ "folder": names.folder(before.parent_id.as_deref()) }),
            &json!({ "folder": names.folder(parent(&body)) }),
        ));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &by, &address).await;
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
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<Access>,
) -> ApiResult<Json<Access>> {
    let connection = state.db.connection(&id).await?;
    let before = state.db.access(Target::Connection, &id).await?;
    state.db.set_access(Target::Connection, &id, &body).await?;
    let after = state.db.access(Target::Connection, &id).await?;
    let entry = Entry::new(action::CONNECTION_ACCESS)
        .by(&by)
        .from(&address)
        .target(target::CONNECTION, &id, &connection.name);
    record_access(&state, entry, &before, &after).await?;
    sessions::policy::enforce(&state, &by, &address).await;
    Ok(Json(after))
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
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<Access>,
) -> ApiResult<Json<Access>> {
    let folder = state.db.folder(&id).await?;
    let before = state.db.access(Target::Folder, &id).await?;
    state.db.set_access(Target::Folder, &id, &body).await?;
    let after = state.db.access(Target::Folder, &id).await?;
    let entry = Entry::new(action::FOLDER_ACCESS)
        .by(&by)
        .from(&address)
        .target(target::FOLDER, &id, &folder.name);
    record_access(&state, entry, &before, &after).await?;
    sessions::policy::enforce(&state, &by, &address).await;
    Ok(Json(after))
}

/// Records who was granted or lost a connection or folder, by name.
async fn record_access(
    state: &AppState,
    entry: Entry,
    before: &Access,
    after: &Access,
) -> ApiResult<()> {
    if before == after {
        return Ok(());
    }
    let names = Names::load(state).await?;
    let named = |access: &Access| {
        json!({
            "users": names.users(&access.users),
            "groups": names.groups(&access.groups),
        })
    };
    let entry = entry.changes(audit::diff(&named(before), &named(after)));
    audit::record(&state.db, entry).await;
    Ok(())
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
    address: ClientAddress,
    Json(body): Json<FolderRequest>,
) -> ApiResult<(StatusCode, Json<Folder>)> {
    let name = validate_folder_name(&body.name)?;
    let parent_id = body.parent_id.as_deref().filter(|p| !p.is_empty());
    let folder = state.db.create_folder(&name, parent_id).await?;
    let names = Names::load(&state).await?;
    let entry = Entry::new(action::FOLDER_CREATE)
        .by(&by)
        .from(&address)
        .target(target::FOLDER, &folder.id, &folder.name)
        .changes(audit::diff(
            &Value::Null,
            &json!({ "name": folder.name, "folder": names.folder(parent_id) }),
        ));
    audit::record(&state.db, entry).await;
    Ok((StatusCode::CREATED, Json(folder)))
}

/// Renames the folder; moving is `PUT /api/folders/{id}/parent`.
pub async fn rename_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<FolderRequest>,
) -> ApiResult<Json<Folder>> {
    let name = validate_folder_name(&body.name)?;
    let before = state.db.folder(&id).await?;
    let folder = state.db.rename_folder(&id, &name).await?;
    let entry = Entry::new(action::FOLDER_RENAME)
        .by(&by)
        .from(&address)
        .target(target::FOLDER, &id, &folder.name)
        .changes(audit::diff(
            &json!({ "name": before.name }),
            &json!({ "name": folder.name }),
        ));
    audit::record(&state.db, entry).await;
    Ok(Json(folder))
}

pub async fn move_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<ParentRequest>,
) -> ApiResult<Json<Folder>> {
    let before = state.db.folder(&id).await?;
    let folder = state.db.move_folder(&id, parent(&body)).await?;
    let names = Names::load(&state).await?;
    let entry = Entry::new(action::FOLDER_MOVE)
        .by(&by)
        .from(&address)
        .target(target::FOLDER, &id, &folder.name)
        .changes(audit::diff(
            &json!({ "folder": names.folder(before.parent_id.as_deref()) }),
            &json!({ "folder": names.folder(folder.parent_id.as_deref()) }),
        ));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &by, &address).await;
    Ok(Json(folder))
}

/// Deletes the folder; what it holds moves up into its parent.
pub async fn delete_folder(
    State(state): State<Arc<AppState>>,
    ConnectionManager(by): ConnectionManager,
    address: ClientAddress,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let before = state.db.folder(&id).await?;
    let names = Names::load(&state).await?;
    state.db.delete_folder(&id).await?;
    let entry = Entry::new(action::FOLDER_DELETE)
        .by(&by)
        .from(&address)
        .target(target::FOLDER, &id, &before.name)
        .changes(audit::diff(
            &json!({ "name": before.name, "folder": names.folder(before.parent_id.as_deref()) }),
            &Value::Null,
        ));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &by, &address).await;
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

/// Names of users, groups and folders, which audit entries show instead of
/// ids.
struct Names {
    users: HashMap<String, String>,
    groups: HashMap<String, String>,
    folders: HashMap<String, String>,
}

impl Names {
    async fn load(state: &AppState) -> ApiResult<Self> {
        Ok(Self {
            users: (state.db.list_users().await?)
                .into_iter()
                .map(|u| (u.id, u.username))
                .collect(),
            groups: (state.db.list_groups().await?)
                .into_iter()
                .map(|g| (g.id, g.name))
                .collect(),
            folders: (state.db.list_folders().await?)
                .into_iter()
                .map(|f| (f.id, f.name))
                .collect(),
        })
    }

    fn lookup(map: &HashMap<String, String>, ids: &[String]) -> Vec<String> {
        ids.iter()
            .map(|id| map.get(id).unwrap_or(id).clone())
            .collect()
    }

    fn users(&self, ids: &[String]) -> Vec<String> {
        Self::lookup(&self.users, ids)
    }

    fn groups(&self, ids: &[String]) -> Vec<String> {
        Self::lookup(&self.groups, ids)
    }

    fn folders(&self, ids: &[String]) -> Vec<String> {
        Self::lookup(&self.folders, ids)
    }

    /// The folder's name; `null` for the top level.
    fn folder(&self, id: Option<&str>) -> Value {
        id.map_or(Value::Null, |id| {
            json!(self.folders.get(id).map_or(id, String::as_str))
        })
    }
}

/// What a user's audit entries compare.
fn user_snapshot(detail: &UserDetail, names: &Names) -> Value {
    json!({
        "username": detail.user.username,
        "role": detail.user.role,
        "disabled": detail.user.disabled,
        "groups": names.groups(&detail.groups),
        "connections": detail.connections,
        "folders": names.folders(&detail.folders),
    })
}

/// The `password` change of an audit entry: never the password itself.
fn password_change(had_one: bool) -> Value {
    use crate::sensitive::REDACTED;
    json!({ "password": { "from": had_one.then_some(REDACTED), "to": REDACTED } })
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
    address: ClientAddress,
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
    let user = state.db.user(&user.id).await?;
    let detail = user_detail(&state, user).await?;
    let names = Names::load(&state).await?;
    let mut entry = Entry::new(action::USER_CREATE)
        .by(&admin)
        .from(&address)
        .target(target::USER, &detail.user.id, &detail.user.username)
        .changes(audit::diff(&Value::Null, &user_snapshot(&detail, &names)));
    if hash != auth::NO_PASSWORD {
        entry = entry.changes(password_change(false));
    }
    audit::record(&state.db, entry).await;
    Ok((StatusCode::CREATED, Json(detail)))
}

pub async fn update_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<UserRequest>,
) -> ApiResult<Json<UserDetail>> {
    let current = state.db.user(&id).await?;
    let names = Names::load(&state).await?;
    let before = user_snapshot(&user_detail(&state, current.clone()).await?, &names);
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
    let had_password = match hash {
        Some(_) => Some(state.db.password_hash(&id).await? != auth::NO_PASSWORD),
        None => None,
    };
    state
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
    let user = state.db.user(&id).await?;
    let detail = user_detail(&state, user).await?;
    let mut entry = Entry::new(action::USER_UPDATE)
        .by(&admin)
        .from(&address)
        .target(target::USER, &id, &detail.user.username)
        .changes(audit::diff(&before, &user_snapshot(&detail, &names)));
    if let Some(had_one) = had_password {
        entry = entry.changes(password_change(had_one));
    }
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &admin, &address).await;
    Ok(Json(detail))
}

pub async fn delete_user(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    if id == admin.id {
        return Err(ApiError::bad_request("You cannot delete your own account."));
    }
    let user = state.db.user(&id).await?;
    let names = Names::load(&state).await?;
    let before = user_snapshot(&user_detail(&state, user.clone()).await?, &names);
    state.db.delete_user(&id).await?;
    let entry = Entry::new(action::USER_DELETE)
        .by(&admin)
        .from(&address)
        .target(target::USER, &id, &user.username)
        .changes(audit::diff(&before, &Value::Null));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &admin, &address).await;
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

/// What a group's audit entries compare.
fn group_snapshot(detail: &GroupDetail, names: &Names) -> Value {
    json!({
        "name": detail.group.name,
        "role": detail.group.role,
        "members": names.users(&detail.members),
        "connections": detail.connections,
        "folders": names.folders(&detail.folders),
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
    address: ClientAddress,
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
    let detail = group_detail(&state, &group.id).await?;
    let names = Names::load(&state).await?;
    let entry = Entry::new(action::GROUP_CREATE)
        .by(&admin)
        .from(&address)
        .target(target::GROUP, &group.id, &group.name)
        .changes(audit::diff(&Value::Null, &group_snapshot(&detail, &names)));
    audit::record(&state.db, entry).await;
    Ok((StatusCode::CREATED, Json(detail)))
}

pub async fn update_group(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
    Path(id): Path<String>,
    Json(body): Json<GroupRequest>,
) -> ApiResult<Json<GroupDetail>> {
    let name = body.name.as_deref().map(validate_group_name).transpose()?;
    let names = Names::load(&state).await?;
    let before = group_snapshot(&group_detail(&state, &id).await?, &names);
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
    let detail = group_detail(&state, &id).await?;
    let entry = Entry::new(action::GROUP_UPDATE)
        .by(&admin)
        .from(&address)
        .target(target::GROUP, &id, &group.name)
        .changes(audit::diff(&before, &group_snapshot(&detail, &names)));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &admin, &address).await;
    Ok(Json(detail))
}

pub async fn delete_group(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let detail = group_detail(&state, &id).await?;
    let names = Names::load(&state).await?;
    state.db.delete_group(&id).await?;
    let entry = Entry::new(action::GROUP_DELETE)
        .by(&admin)
        .from(&address)
        .target(target::GROUP, &id, &detail.group.name)
        .changes(audit::diff(&group_snapshot(&detail, &names), &Value::Null));
    audit::record(&state.db, entry).await;
    sessions::policy::enforce(&state, &admin, &address).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Recordings: administrators see every one; other users the ones they
// started from the session menu. Only administrators delete them.

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

/// Recording `id`, if `user` may open it: administrators every one, others
/// those they started. 404 otherwise, as for one that does not exist.
async fn visible_recording(state: &AppState, user: &User, id: &str) -> ApiResult<Recording> {
    let recording = state.db.recording(id).await?;
    if user.is_admin || recording.started_by.as_deref() == Some(user.id.as_str()) {
        Ok(recording)
    } else {
        Err(ApiError::not_found())
    }
}

pub async fn list_recordings(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<Recording>>> {
    let all = if user.is_admin {
        state.db.list_recordings().await?
    } else {
        state.db.recordings_started_by(&user.id).await?
    };
    let mut recordings = Vec::new();
    for recording in all {
        recordings.push(with_current_size(&state, recording).await);
    }
    Ok(Json(recordings))
}

pub async fn get_recording(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> ApiResult<Json<Recording>> {
    let recording = visible_recording(&state, &user, &id).await?;
    Ok(Json(with_current_size(&state, recording).await))
}

#[derive(Deserialize)]
pub struct FileQuery {
    /// Ask the browser to save the file instead of handing it to the page.
    #[serde(default)]
    download: bool,
}

/// The recording file, with range requests (`ServeFile`).
/// The audit entry about a recording.
fn recording_entry(action: &'static str, by: &User, recording: &Recording) -> Entry {
    Entry::new(action)
        .by(by)
        .target(target::RECORDING, &recording.id, &recording.file_name)
        .detail("connection", recording.connection_id.as_str())
        .detail("user", recording.username.as_str())
        .detail("startedAt", recording.started_at)
}

pub async fn recording_file(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Path(id): Path<String>,
    Query(query): Query<FileQuery>,
    request: Request,
) -> ApiResult<Response> {
    use tower::ServiceExt as _;

    let recording = visible_recording(&state, &user, &id).await?;
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
        let entry = recording_entry(action::RECORDING_DOWNLOAD, &user, &recording);
        audit::record(&state.db, entry.from(&address)).await;
    }
    Ok(response)
}

pub async fn delete_recording(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
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
    let entry = recording_entry(action::RECORDING_DELETE, &admin, &recording);
    audit::record(&state.db, entry.from(&address)).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Audit log (administrators only)

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AuditQuery {
    /// The actor's username or id.
    actor: Option<String>,
    /// An action or a prefix of whole parts: `user`, `oidc.user`, `login.failure`.
    action: Option<String>,
    /// `user`, `group`, `connection`, `folder`, `recording` or `token`.
    target_type: Option<String>,
    /// The target's id or name.
    target: Option<String>,
    /// From this time on, Unix milliseconds.
    from: Option<i64>,
    /// Before this time, Unix milliseconds.
    to: Option<i64>,
    /// Entries per page (default 100, at most 1000).
    limit: Option<i64>,
    offset: Option<i64>,
    /// `csv` for a CSV file of every matching entry (up to 100 000).
    format: Option<String>,
}

const AUDIT_PAGE: i64 = 100;
const AUDIT_MAX_PAGE: i64 = 1000;
const AUDIT_MAX_CSV: usize = 100_000;

impl AuditQuery {
    fn filter(&self) -> ApiResult<crate::db::AuditFilter> {
        let text = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let action = text(&self.action);
        if let Some(action) = &action
            && !action.chars().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '*')
            })
        {
            return Err(ApiError::bad_request(
                "Filter actions by a name such as user.update or a prefix such as user.",
            ));
        }
        Ok(crate::db::AuditFilter {
            actor: text(&self.actor),
            action,
            target_type: text(&self.target_type),
            target: text(&self.target),
            since: self.from,
            until: self.to,
        })
    }
}

/// `GET /api/audit`: `{"entries": […], "hasMore": bool}`, newest first, or
/// with `format=csv` every matching entry as a CSV download.
pub async fn list_audit(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Query(query): Query<AuditQuery>,
) -> ApiResult<Response> {
    let filter = query.filter()?;
    if query.format.as_deref() == Some("csv") {
        return audit_csv(&state, &filter).await;
    }
    if query.format.as_deref().is_some_and(|f| f != "json") {
        return Err(ApiError::bad_request("The format is json or csv."));
    }
    let limit = query.limit.unwrap_or(AUDIT_PAGE).clamp(1, AUDIT_MAX_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);
    let mut entries = state
        .db
        .list_audit_entries(&filter, limit + 1, offset)
        .await?;
    let has_more = entries.len() as i64 > limit;
    entries.truncate(limit as usize);
    Ok(Json(json!({ "entries": entries, "hasMore": has_more })).into_response())
}

/// `GET /api/audit/actions`: every action the gateway records.
pub async fn audit_actions(AdminUser(_): AdminUser) -> Json<&'static [&'static str]> {
    Json(audit::action::ALL)
}

async fn audit_csv(state: &AppState, filter: &crate::db::AuditFilter) -> ApiResult<Response> {
    let mut csv = String::from(
        "time,action,actor,actor_id,source_address,target_type,target_id,target_name,changes\r\n",
    );
    let mut written = 0;
    while written < AUDIT_MAX_CSV {
        let page = state
            .db
            .list_audit_entries(filter, AUDIT_MAX_PAGE, written as i64)
            .await?;
        for entry in &page {
            let changes = entry.changes.as_ref().map(Value::to_string);
            let fields = [
                Some(audit::rfc3339(entry.occurred_at)),
                Some(entry.action.clone()),
                entry.actor_name.clone(),
                entry.actor_id.clone(),
                entry.source_address.clone(),
                entry.target_type.clone(),
                entry.target_id.clone(),
                entry.target_name.clone(),
                changes,
            ];
            let row: Vec<String> = fields
                .iter()
                .map(|f| audit::csv_field(f.as_deref().unwrap_or("")))
                .collect();
            csv.push_str(&row.join(","));
            csv.push_str("\r\n");
        }
        written += page.len();
        if (page.len() as i64) < AUDIT_MAX_PAGE {
            break;
        }
    }
    let name = format!(
        "attachment; filename=\"molcajete-audit-{}.csv\"",
        audit::rfc3339(crate::db::now_millis()).replace([':', '.'], "-")
    );
    let mut response = csv.into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    if let Ok(value) = HeaderValue::from_str(&name) {
        headers.insert(header::CONTENT_DISPOSITION, value);
    }
    Ok(response)
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
            guacd_handshake_timeout: Some(30),
            parameters: BTreeMap::from([
                (" hostname ".into(), "10.0.0.5".into()),
                ("empty".into(), String::new()),
            ]),
            stored_secrets: Default::default(),
            clear_secrets: [" password ".into(), " ".into()].into(),
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
        // The timeout applies to the default guacd too.
        assert_eq!(c.guacd_handshake_timeout, Some(30));
        for bad in [0, MAX_HANDSHAKE_TIMEOUT + 1] {
            let c = ConnectionRecord {
                guacd_handshake_timeout: Some(bad),
                ..record("desk-1")
            };
            assert!(validate_connection(c).is_err(), "{bad}");
        }
        assert_eq!(c.parent_id, None);
        assert_eq!(
            c.parameters,
            BTreeMap::from([("hostname".into(), "10.0.0.5".into())])
        );
        assert_eq!(
            c.attributes,
            BTreeMap::from([(attributes::RECORD.into(), "true".into())])
        );
        assert_eq!(c.clear_secrets, ["password".to_owned()].into());
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
