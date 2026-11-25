//! Sign-in through an OpenID Connect identity provider.
//!
//! The authorization code flow with PKCE: `GET /api/auth/oidc/login` sends
//! the browser to the provider with a random `state`, `nonce` and PKCE
//! challenge. Those are kept in `molcajete_oidc_login` (keyed by the
//! state's hash, used once, ten minutes at most) and the state also in an
//! `HttpOnly` cookie, so only the browser that started a sign-in can finish
//! it. `GET /api/auth/oidc/callback` checks the state against the cookie,
//! exchanges the code, verifies the ID token (signature, issuer, audience,
//! expiry, nonce, access token hash), finds or creates the user by the
//! token's `iss` and `sub`, and issues the usual session cookie.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow};
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use openidconnect::core::{CoreAuthenticationFlow, CoreClient, CoreClientAuthMethod, CoreIdToken};
use openidconnect::{
    AccessTokenHash, AuthType, AuthorizationCode, ClaimsVerificationError, ClientId, ClientSecret,
    CsrfToken, EndpointMaybeSet, EndpointNotSet, EndpointSet, IssuerUrl, LogoutRequest, Nonce,
    OAuth2TokenResponse as _, PkceCodeChallenge, PkceCodeVerifier, PostLogoutRedirectUrl,
    ProviderMetadataWithLogout, RedirectUrl, Scope, TokenResponse as _, reqwest,
};
use serde::Deserialize;
use serde_json::{Map, Value};
use tracing::{info, warn};

use crate::AppState;
use crate::api::{self, ApiError};
use crate::auth;
use crate::config::OidcConfig;
use crate::db::{Database, DbError, NewUser, PendingLogin, Role, User, UserUpdate};

/// Holds the `state` of the sign-in this browser started.
pub const STATE_COOKIE: &str = "molcajete_oidc_state";
/// Where the provider sends the browser back to.
pub const CALLBACK_PATH: &str = "/api/auth/oidc/callback";
/// How long a sign-in at the provider may take.
const LOGIN_TTL: Duration = Duration::from_secs(600);
/// How long discovery (endpoints and signing keys) is reused.
const METADATA_TTL: Duration = Duration::from_secs(600);
/// From `/api/auth/oidc/{login,callback}` back to the page, wherever the
/// gateway is mounted.
const APP_PATH: &str = "../../../";

type OidcClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

/// Who the provider says signed in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub issuer: String,
    pub subject: String,
    pub username: String,
    /// Names from `groups_claim`.
    pub groups: Vec<String>,
    /// The raw ID token, kept for RP-initiated logout.
    pub id_token: String,
}

/// Why a sign-in through the provider failed. [`LoginError::code`] is what
/// the page learns (`?login_error=<code>`); the details go to the log.
#[derive(Debug, thiserror::Error)]
pub enum LoginError {
    #[error("no sign-in in progress for this browser, or it expired")]
    Expired,
    #[error("the state does not match the one this browser started with")]
    StateMismatch,
    #[error("the user cancelled at the provider")]
    Cancelled,
    #[error("the provider returned {0}")]
    Provider(String),
    #[error("the provider cannot be reached: {0:#}")]
    Unavailable(anyhow::Error),
    #[error("{0:#}")]
    Failed(anyhow::Error),
    #[error("the ID token has no usable {0:?} claim")]
    NoUsername(String),
    #[error("the account is disabled")]
    Disabled,
    #[error("no account for {0:?} and auto_create_users is off")]
    NotAllowed(String),
    #[error("the username {0:?} belongs to another identity at the provider")]
    UsernameTaken(String),
    #[error(transparent)]
    Db(#[from] DbError),
}

impl LoginError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Expired | Self::StateMismatch => "expired",
            Self::Cancelled => "cancelled",
            Self::Unavailable(_) => "unavailable",
            Self::NoUsername(_) => "no_username",
            Self::Disabled => "disabled",
            Self::NotAllowed(_) => "not_allowed",
            Self::UsernameTaken(_) => "username_taken",
            Self::Provider(_) | Self::Failed(_) | Self::Db(_) => "failed",
        }
    }
}

fn failed(err: impl std::error::Error + Send + Sync + 'static, context: &str) -> LoginError {
    LoginError::Failed(anyhow::Error::new(err).context(context.to_owned()))
}

/// The configured provider and what discovery found out about it.
pub struct Oidc {
    pub config: OidcConfig,
    issuer: IssuerUrl,
    client_id: ClientId,
    client_secret: Option<ClientSecret>,
    redirect_url: RedirectUrl,
    post_logout_redirect_url: Option<PostLogoutRedirectUrl>,
    /// Cookies get `Secure` when the provider sends browsers back over HTTPS.
    secure_cookies: bool,
    http: reqwest::Client,
    metadata: tokio::sync::Mutex<Option<(Instant, Arc<ProviderMetadataWithLogout>)>>,
}

impl Oidc {
    /// Checks the configuration; the provider is contacted on first use.
    pub fn new(config: OidcConfig) -> anyhow::Result<Self> {
        let issuer = IssuerUrl::new(config.issuer.clone()).context("oidc.issuer")?;
        let redirect_url =
            RedirectUrl::new(config.redirect_url.clone()).context("oidc.redirect_url")?;
        if !redirect_url.url().path().ends_with(CALLBACK_PATH) {
            warn!(
                redirect_url = %config.redirect_url,
                "oidc.redirect_url should end with {CALLBACK_PATH}"
            );
        }
        let post_logout_redirect_url = config
            .post_logout_redirect_url
            .clone()
            .map(PostLogoutRedirectUrl::new)
            .transpose()
            .context("oidc.post_logout_redirect_url")?;
        let client_secret = config.resolve_client_secret()?.map(ClientSecret::new);
        let http = reqwest::Client::builder()
            // Following redirects would let a provider point us anywhere.
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .context("building the HTTP client")?;
        Ok(Self {
            client_id: ClientId::new(config.client_id.clone()),
            secure_cookies: redirect_url.url().scheme() == "https",
            config,
            issuer,
            client_secret,
            redirect_url,
            post_logout_redirect_url,
            http,
            metadata: Default::default(),
        })
    }

    /// The provider's discovery document and signing keys, cached for a
    /// while; `refresh` fetches them again (keys may have been rotated).
    async fn metadata(&self, refresh: bool) -> anyhow::Result<Arc<ProviderMetadataWithLogout>> {
        let mut cached = self.metadata.lock().await;
        if !refresh
            && let Some((fetched, metadata)) = cached.as_ref()
            && fetched.elapsed() < METADATA_TTL
        {
            return Ok(metadata.clone());
        }
        let metadata = ProviderMetadataWithLogout::discover_async(self.issuer.clone(), &self.http)
            .await
            .with_context(|| format!("discovering {}", self.issuer.as_str()))?;
        let metadata = Arc::new(metadata);
        *cached = Some((Instant::now(), metadata.clone()));
        Ok(metadata)
    }

    fn client(&self, metadata: &ProviderMetadataWithLogout) -> OidcClient {
        let client = CoreClient::from_provider_metadata(
            metadata.clone(),
            self.client_id.clone(),
            self.client_secret.clone(),
        )
        .set_redirect_uri(self.redirect_url.clone());
        // HTTP Basic is the default; use the form body when that is all the
        // provider takes.
        let methods = metadata.token_endpoint_auth_methods_supported();
        let post_only = methods.is_some_and(|m| {
            !m.contains(&CoreClientAuthMethod::ClientSecretBasic)
                && m.contains(&CoreClientAuthMethod::ClientSecretPost)
        });
        if post_only {
            client.set_auth_type(AuthType::RequestBody)
        } else {
            client
        }
    }

    /// The provider's authorization URL for a new sign-in, its `state` and
    /// what [`Oidc::finish`] needs later.
    pub async fn begin(&self) -> anyhow::Result<(String, String, PendingLogin)> {
        let metadata = self.metadata(false).await?;
        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let scopes = self
            .config
            .scopes
            .iter()
            .filter(|s| s.as_str() != "openid")
            .map(|s| Scope::new(s.clone()));
        let (url, state, nonce) = self
            .client(&metadata)
            .authorize_url(
                CoreAuthenticationFlow::AuthorizationCode,
                CsrfToken::new_random,
                Nonce::new_random,
            )
            .add_scopes(scopes)
            .set_pkce_challenge(challenge)
            .url();
        let pending = PendingLogin {
            nonce: nonce.secret().clone(),
            pkce_verifier: verifier.secret().clone(),
        };
        Ok((url.to_string(), state.secret().clone(), pending))
    }

    /// Exchanges the code and verifies the ID token.
    pub async fn finish(
        &self,
        code: String,
        pending: PendingLogin,
    ) -> Result<Identity, LoginError> {
        let mut metadata = self
            .metadata(false)
            .await
            .map_err(LoginError::Unavailable)?;
        let mut client = self.client(&metadata);
        let token = client
            .exchange_code(AuthorizationCode::new(code))
            .map_err(|err| failed(err, "the provider has no token endpoint"))?
            .set_pkce_verifier(PkceCodeVerifier::new(pending.pkce_verifier))
            .request_async(&self.http)
            .await
            .map_err(|err| failed(err, "exchanging the code"))?;
        let id_token = token
            .id_token()
            .ok_or_else(|| LoginError::Failed(anyhow!("the token response has no ID token")))?;
        let nonce = Nonce::new(pending.nonce);

        // An unknown signing key may mean the provider rotated its keys.
        let unknown_key = matches!(
            id_token.claims(&client.id_token_verifier(), &nonce),
            Err(ClaimsVerificationError::SignatureVerification(_))
        );
        if unknown_key {
            metadata = self.metadata(true).await.map_err(LoginError::Unavailable)?;
            client = self.client(&metadata);
        }
        let verifier = client.id_token_verifier();
        let claims = id_token
            .claims(&verifier, &nonce)
            .map_err(|err| failed(err, "verifying the ID token"))?;
        if let Some(expected) = claims.access_token_hash() {
            let alg = id_token
                .signing_alg()
                .map_err(|err| failed(err, "checking the access token hash"))?;
            let key = id_token
                .signing_key(&verifier)
                .map_err(|err| failed(err, "checking the access token hash"))?;
            let actual = AccessTokenHash::from_token(token.access_token(), alg, key)
                .map_err(|err| failed(err, "checking the access token hash"))?;
            if actual != *expected {
                return Err(LoginError::Failed(anyhow!(
                    "the access token does not match the ID token's at_hash"
                )));
            }
        }
        let issuer = claims.issuer().as_str().to_owned();
        let subject = claims.subject().as_str().to_owned();

        // The ID token is verified; read any claim from its payload.
        let raw = id_token.to_string();
        let mut all = jwt_payload(&raw)
            .ok_or_else(|| LoginError::Failed(anyhow!("the ID token payload is not JSON")))?;
        let missing = |all: &Map<String, Value>| {
            claim(all, &self.config.username_claim).is_none()
                || claim(all, &self.config.groups_claim).is_none()
        };
        if missing(&all)
            && let Some(url) = metadata.userinfo_endpoint()
        {
            match self
                .userinfo(url.as_str(), token.access_token().secret())
                .await
            {
                Ok(info) if info.get("sub").and_then(Value::as_str) == Some(subject.as_str()) => {
                    for (name, value) in info {
                        all.entry(name).or_insert(value);
                    }
                }
                Ok(_) => warn!("userinfo is about another subject; ignored"),
                Err(err) => warn!(error = %format!("{err:#}"), "reading userinfo failed"),
            }
        }

        let username = claim(&all, &self.config.username_claim)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| api::validate_username(name).is_ok())
            .ok_or_else(|| LoginError::NoUsername(self.config.username_claim.clone()))?
            .to_owned();
        let groups = claim(&all, &self.config.groups_claim)
            .map(string_list)
            .unwrap_or_default();
        Ok(Identity {
            issuer,
            subject,
            username,
            groups,
            id_token: raw,
        })
    }

    async fn userinfo(&self, url: &str, access_token: &str) -> anyhow::Result<Map<String, Value>> {
        let response = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .header(header::ACCEPT, "application/json")
            .send()
            .await?
            .error_for_status()?;
        let body = response.bytes().await?;
        serde_json::from_slice(&body).context("userinfo is not a JSON object")
    }

    /// Where to send the browser to sign out at the provider too, when it
    /// supports RP-initiated logout.
    pub async fn logout_url(&self, id_token: &str) -> Option<String> {
        let metadata = match self.metadata(false).await {
            Ok(metadata) => metadata,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "cannot sign out at the provider");
                return None;
            }
        };
        let endpoint = metadata
            .additional_metadata()
            .end_session_endpoint
            .clone()?;
        let mut request = LogoutRequest::from(endpoint).set_client_id(self.client_id.clone());
        if let Ok(token) = id_token.parse::<CoreIdToken>() {
            request = request.set_id_token_hint(&token);
        }
        if let Some(url) = &self.post_logout_redirect_url {
            request = request.set_post_logout_redirect_uri(url.clone());
        }
        Some(request.http_get_url().to_string())
    }
}

/// The claims of a JWT, without checking anything.
fn jwt_payload(jwt: &str) -> Option<Map<String, Value>> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// A claim by name, or by a dotted path into nested objects
/// (`realm_access.roles`).
fn claim<'a>(claims: &'a Map<String, Value>, name: &str) -> Option<&'a Value> {
    if let Some(value) = claims.get(name) {
        return Some(value);
    }
    let mut parts = name.split('.');
    let mut value = claims.get(parts.next()?)?;
    for part in parts {
        value = value.get(part)?;
    }
    Some(value)
}

/// A list of strings, or one string.
fn string_list(value: &Value) -> Vec<String> {
    match value {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Accounts

/// Finds, links or creates the user for an identity and applies what the
/// provider says about them.
pub async fn sign_in(
    db: &Database,
    config: &OidcConfig,
    identity: &Identity,
) -> Result<User, LoginError> {
    let user = match db
        .user_by_identity(&identity.issuer, &identity.subject)
        .await
    {
        Ok(user) => user,
        Err(DbError::NotFound) => link_or_create(db, config, identity).await?,
        Err(err) => return Err(err.into()),
    };
    if user.disabled {
        return Err(LoginError::Disabled);
    }
    let user = follow_username(db, user, &identity.username).await?;
    sync_from_provider(db, config, user, &identity.groups).await
}

/// First sign-in of an identity: link the user with that username (when it
/// is not linked to someone else at this provider), or create one.
async fn link_or_create(
    db: &Database,
    config: &OidcConfig,
    identity: &Identity,
) -> Result<User, LoginError> {
    let username = identity.username.as_str();
    let user = match db.user_credentials(username).await {
        Ok((user, _)) => {
            if db.has_identity(&user.id, &identity.issuer).await? {
                return Err(LoginError::UsernameTaken(username.to_owned()));
            }
            info!(%username, "linked an existing user to their identity at the provider");
            user
        }
        Err(DbError::NotFound) if config.auto_create_users => {
            let user = db
                .create_user(NewUser {
                    username,
                    password_hash: auth::NO_PASSWORD,
                    role: Role::User,
                    disabled: false,
                })
                .await
                .map_err(|err| match err {
                    DbError::Conflict => LoginError::UsernameTaken(username.to_owned()),
                    err => err.into(),
                })?;
            info!(%username, "created a user on their first single sign-on");
            user
        }
        Err(DbError::NotFound) => return Err(LoginError::NotAllowed(username.to_owned())),
        Err(err) => return Err(err.into()),
    };
    db.link_identity(&identity.issuer, &identity.subject, &user.id)
        .await?;
    Ok(user)
}

/// Renames the user when the provider's username changed and the new one
/// is free here.
async fn follow_username(db: &Database, user: User, username: &str) -> Result<User, LoginError> {
    if user.username == username {
        return Ok(user);
    }
    let update = UserUpdate {
        username: Some(username),
        ..Default::default()
    };
    match db.update_user(&user.id, update).await {
        Ok(renamed) => {
            info!(from = %user.username, to = %username, "renamed after the provider");
            Ok(renamed)
        }
        Err(DbError::Conflict) => {
            warn!(
                username = %user.username,
                wanted = %username,
                "the provider's username is taken here; keeping the old one"
            );
            Ok(user)
        }
        Err(err) => Err(err.into()),
    }
}

/// Applies the provider's groups on every sign-in: memberships in molcajete
/// user groups (`sync_groups`) and the user's own role (`admin_groups`,
/// `connection_manager_groups`).
///
/// A change that would leave no enabled administrator is skipped with a
/// warning and the user still signs in: the provider cannot lock everyone
/// out of administration.
async fn sync_from_provider(
    db: &Database,
    config: &OidcConfig,
    user: User,
    groups: &[String],
) -> Result<User, LoginError> {
    if config.sync_groups {
        let names: Vec<String> = groups
            .iter()
            .filter(|g| {
                config
                    .group_prefix
                    .as_deref()
                    .is_none_or(|prefix| g.starts_with(prefix))
            })
            .cloned()
            .collect();
        match db
            .sync_user_groups_by_name(&user.id, &names, config.create_groups)
            .await
        {
            Ok(joined) => {
                let joined: Vec<&str> = joined.iter().map(|g| g.name.as_str()).collect();
                info!(username = %user.username, groups = ?joined, "groups follow the provider");
            }
            Err(DbError::LastAdmin) => warn!(
                username = %user.username,
                "the provider's groups would remove the last administrator; \
                 keeping the user's groups"
            ),
            Err(err) => return Err(err.into()),
        }
    }

    if let Some(mapped) = mapped_role(config, groups) {
        let role = if config.sync_role {
            mapped
        } else {
            user.role.max(mapped)
        };
        if role != user.role {
            let update = UserUpdate {
                role: Some(role),
                ..Default::default()
            };
            match db.update_user(&user.id, update).await {
                Ok(_) => {
                    info!(username = %user.username, role = role.as_str(), "role follows the provider's groups");
                }
                Err(DbError::LastAdmin) => warn!(
                    username = %user.username,
                    "left the provider's administrator groups but is the last administrator; \
                     keeping the administrator role"
                ),
                Err(err) => return Err(err.into()),
            }
        }
    }
    // Group memberships change the effective role too.
    Ok(db.user(&user.id).await?)
}

/// The role `admin_groups` and `connection_manager_groups` give someone in
/// `groups`; `None` when neither is configured.
fn mapped_role(config: &OidcConfig, groups: &[String]) -> Option<Role> {
    if config.admin_groups.is_empty() && config.connection_manager_groups.is_empty() {
        return None;
    }
    let member_of = |list: &[String]| groups.iter().any(|g| list.contains(g));
    Some(if member_of(&config.admin_groups) {
        Role::Admin
    } else if member_of(&config.connection_manager_groups) {
        Role::ConnectionManager
    } else {
        Role::User
    })
}

// ---------------------------------------------------------------------------
// Handlers

fn state_cookie(state: &str, max_age_secs: u64, secure: bool) -> HeaderValue {
    // `Lax`: the provider sends the browser back with a cross-site
    // top-level navigation, which `Strict` cookies do not accompany.
    let secure = if secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{STATE_COOKIE}={state}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age_secs}{secure}"
    ))
    .expect("cookie is ASCII")
}

fn see_other(location: &str, cookies: impl IntoIterator<Item = HeaderValue>) -> Response {
    let mut response = StatusCode::SEE_OTHER.into_response();
    let headers = response.headers_mut();
    if let Ok(location) = HeaderValue::from_str(location) {
        headers.insert(header::LOCATION, location);
    }
    for cookie in cookies {
        headers.append(header::SET_COOKIE, cookie);
    }
    response
}

fn back_with_error(code: &str, cookies: impl IntoIterator<Item = HeaderValue>) -> Response {
    see_other(&format!("{APP_PATH}?login_error={code}"), cookies)
}

/// `GET /api/auth/providers`: how the login page lets people sign in.
pub async fn providers(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(serde_json::json!({
        "passwordLogin": state.password_login(),
        "oidc": state.oidc.as_ref().map(|o| serde_json::json!({
            "displayName": o.config.display_name,
        })),
    }))
}

/// `GET /api/auth/oidc/login`: off to the provider.
pub async fn login(State(state): State<Arc<AppState>>) -> Response {
    let Some(oidc) = &state.oidc else {
        return ApiError::not_found().into_response();
    };
    let (url, csrf, pending) = match oidc.begin().await {
        Ok(started) => started,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "cannot reach the OpenID Connect provider");
            return back_with_error("unavailable", []);
        }
    };
    let ttl_ms = LOGIN_TTL.as_millis() as i64;
    if let Err(err) = state
        .db
        .create_oidc_login(&auth::token_hash(&csrf), &pending, ttl_ms)
        .await
    {
        return ApiError::from(err).into_response();
    }
    let cookie = state_cookie(&csrf, LOGIN_TTL.as_secs(), oidc.secure_cookies);
    see_other(&url, [cookie])
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

/// `GET /api/auth/oidc/callback`: back from the provider.
pub async fn callback(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let Some(oidc) = &state.oidc else {
        return ApiError::not_found().into_response();
    };
    let clear = state_cookie("", 0, oidc.secure_cookies);
    let result = finish_callback(&state, oidc, &headers, query).await;
    let (user, id_token) = match result {
        Ok(done) => done,
        Err(err) => {
            warn!(code = err.code(), error = %err, "single sign-on failed");
            return back_with_error(err.code(), [clear]);
        }
    };
    let secure = oidc.secure_cookies || auth::is_https(&headers);
    match api::start_session(&state, &user, secure, Some(&id_token)).await {
        Ok(session) => {
            info!(username = %user.username, "single sign-on");
            see_other(APP_PATH, [clear, session])
        }
        Err(err) => {
            warn!(error = ?err, "starting a session failed");
            back_with_error("failed", [clear])
        }
    }
}

async fn finish_callback(
    state: &AppState,
    oidc: &Oidc,
    headers: &HeaderMap,
    query: CallbackQuery,
) -> Result<(User, String), LoginError> {
    let started = auth::cookie(headers, STATE_COOKIE).ok_or(LoginError::Expired)?;
    let started = auth::token_hash(started);
    // Used up whatever happens next.
    let pending = match state.db.take_oidc_login(&started).await {
        Ok(pending) => pending,
        Err(DbError::NotFound) => return Err(LoginError::Expired),
        Err(err) => return Err(err.into()),
    };
    if query.state.as_deref().map(auth::token_hash) != Some(started) {
        return Err(LoginError::StateMismatch);
    }
    if let Some(error) = query.error {
        return Err(if error == "access_denied" {
            LoginError::Cancelled
        } else {
            LoginError::Provider(error)
        });
    }
    let code = query
        .code
        .ok_or_else(|| LoginError::Failed(anyhow!("the callback has no code")))?;
    let identity = oidc.finish(code, pending).await?;
    let user = sign_in(&state.db, &oidc.config, &identity).await?;
    Ok((user, identity.id_token))
}

#[cfg(test)]
mod fake_provider;
#[cfg(test)]
mod flow_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_claims_by_name_or_path() {
        let claims: Map<String, Value> = serde_json::from_value(serde_json::json!({
            "preferred_username": "ada",
            "groups": ["a", "b", 3],
            "realm_access": {"roles": ["admin"]},
            "single": "only",
        }))
        .unwrap();
        assert_eq!(
            claim(&claims, "preferred_username").and_then(Value::as_str),
            Some("ada")
        );
        assert_eq!(string_list(claim(&claims, "groups").unwrap()), ["a", "b"]);
        assert_eq!(
            string_list(claim(&claims, "realm_access.roles").unwrap()),
            ["admin"]
        );
        assert_eq!(string_list(claim(&claims, "single").unwrap()), ["only"]);
        assert!(claim(&claims, "missing").is_none());
        assert!(claim(&claims, "realm_access.missing").is_none());
    }

    #[test]
    fn decodes_jwt_payloads() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"x"}"#);
        let claims = jwt_payload(&format!("e30.{payload}.sig")).unwrap();
        assert_eq!(claims["sub"], "x");
        assert!(jwt_payload("nope").is_none());
    }
}
