//! API tokens: bearer tokens for automation.
//!
//! Every user can create tokens for themselves; a token acts as its owner,
//! with the owner's role and grants as they are when it is used (demoting
//! or disabling the owner limits or stops the token). A token is
//! `mlc_` followed by 64 hex digits, shown once; the database keeps its
//! SHA-256, its first characters (to recognise it), a name, an expiry, an
//! optional list of source addresses and when and from where it was last
//! used.
//!
//! A request with `Authorization: Bearer <token>` is authenticated by
//! [`authenticate`] alone: the session cookie is ignored and the
//! same-origin check does not apply (a browser cannot attach the header to
//! a cross-site request without a CORS preflight, which the gateway never
//! allows). Some things need a signed-in person rather than a token; see
//! [`restricted`].

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use ipnet::IpNet;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::api::ApiError;
use crate::audit::{self, Entry, action, target};
use crate::auth::{self, ClientAddress, CurrentUser};
use crate::db::{ApiToken, DbError, NewApiToken, Role, User, now_millis};
use crate::{AppState, config, proxy};

/// What every token starts with, so that secret scanners and people
/// recognise one.
pub const TOKEN_PREFIX: &str = "mlc_";
/// How many characters of a token are kept to recognise it.
const SHOWN_PREFIX_CHARS: usize = 12;
/// The most addresses and ranges a token may list.
const MAX_ALLOWED_IPS: usize = 64;
/// `last_used_at` is written at most this often per token.
const TOUCH_INTERVAL_MS: i64 = 60_000;
/// A use after this long without one is recorded in the audit log again.
const USE_AUDIT_GAP_MS: i64 = 3_600_000;

/// Set on requests authenticated with an API token.
#[derive(Debug, Clone)]
pub struct TokenAuth {
    pub token: ApiToken,
    /// The owner, as they are now.
    pub user: User,
}

/// The token in an `Authorization: Bearer …` header.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.trim().split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// Why a token may not do this, for requests only a signed-in person may
/// make: creating tokens (a leaked token must not be able to outlive its
/// revocation or expiry through new ones), and changing the owner's
/// password or account (which would let it take the account over or keep
/// it). `path` is relative to `/api`.
pub fn restricted(method: &Method, path: &str, owner_id: &str) -> Option<&'static str> {
    let path = path.trim_end_matches('/');
    if *method == Method::POST && path == "/tokens" {
        return Some("API tokens cannot create API tokens; sign in to create one.");
    }
    if *method == Method::PUT && path == "/auth/password" {
        return Some("API tokens cannot change passwords; sign in to change yours.");
    }
    let own_account = path
        .strip_prefix("/users/")
        .is_some_and(|id| percent_decode(id) == owner_id);
    if own_account && matches!(*method, Method::PUT | Method::DELETE) {
        return Some("API tokens cannot change or delete their own user; sign in instead.");
    }
    None
}

fn percent_decode(text: &str) -> String {
    form_urlencoded::parse(format!("x={text}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// Middleware on the API: authenticates `Authorization: Bearer` requests
/// (401 for an unknown, revoked or expired token or a disabled owner, 403
/// from an address the token does not allow or for [`restricted`]
/// requests) and marks them with [`TokenAuth`]. Requests without the
/// header pass through untouched.
pub async fn authenticate(
    State(state): State<Arc<AppState>>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some(token) = bearer(request.headers()) else {
        return next.run(request).await;
    };
    let invalid = || {
        ApiError::new(
            StatusCode::UNAUTHORIZED,
            "The API token is not valid; it may have expired or been revoked.",
        )
        .into_response()
    };
    if !token.starts_with(TOKEN_PREFIX) {
        return invalid();
    }
    let found = match state.db.usable_api_token(&auth::token_hash(token)).await {
        Ok(found) => found,
        Err(DbError::NotFound) => return invalid(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let client = proxy::resolve(
        request.headers(),
        peer,
        &state.config.server.trusted_proxies,
    );
    let address = client.display;
    if !allows(&found.allowed_ips, client.ip) {
        warn!(token = %found.id, name = %found.name, %address, "API token used from an address it does not allow");
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "This API token cannot be used from this address.",
        )
        .into_response();
    }
    // Nested routers see the path without `/api`; be sure either way.
    let path = request.uri().path();
    let path = path
        .strip_prefix("/api")
        .filter(|p| p.starts_with('/'))
        .unwrap_or(path);
    if let Some(reason) = restricted(request.method(), path, &found.user_id) {
        return ApiError::new(StatusCode::FORBIDDEN, reason).into_response();
    }
    let user = match state.db.user(&found.user_id).await {
        Ok(user) => user,
        Err(DbError::NotFound) => return invalid(),
        Err(err) => return ApiError::from(err).into_response(),
    };
    note_use(&state, &found, &user, &address).await;
    // The token decides who this is, whatever cookie came along.
    request.headers_mut().remove(header::COOKIE);
    request
        .extensions_mut()
        .insert(TokenAuth { token: found, user });
    next.run(request).await
}

/// Updates the token's last use (at most once a minute, or when the
/// address changes) and records `token.use` for the first use, a use from
/// a new address or the first use after an hour.
async fn note_use(state: &AppState, token: &ApiToken, user: &User, address: &str) {
    let now = now_millis();
    let new_address = token.last_used_from.as_deref() != Some(address);
    let idle = |gap| token.last_used_at.is_none_or(|at| now - at >= gap);
    if !new_address && !idle(TOUCH_INTERVAL_MS) {
        return;
    }
    match state
        .db
        .touch_api_token(&token.id, token.last_used_at, now, address)
        .await
    {
        Ok(true) if new_address || idle(USE_AUDIT_GAP_MS) => {
            let entry = Entry::new(action::TOKEN_USE)
                .by(user)
                .from(&ClientAddress(address.to_owned()))
                .target(target::TOKEN, &token.id, &token.name);
            audit::record(&state.db, entry).await;
        }
        Ok(_) => {} // Not worth an entry, or another request recorded it.
        Err(err) => warn!(error = %err, token = %token.id, "cannot record the use of an API token"),
    }
}

/// An address or a CIDR range in a token's allowlist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IpRange(IpNet);

impl IpRange {
    /// `"10.0.0.0/8"` (host bits are dropped) or a single address.
    pub fn parse(text: &str) -> Option<Self> {
        let net = config::parse_ip_network(text)?.trunc();
        Some(Self(match net.addr().to_canonical() {
            // An IPv4 address written as IPv6 is matched as IPv4.
            IpAddr::V4(v4) if net.addr().is_ipv6() => {
                IpNet::new(v4.into(), net.prefix_len().saturating_sub(96)).ok()?
            }
            _ => net,
        }))
    }

    pub fn contains(&self, address: IpAddr) -> bool {
        self.0.contains(&address.to_canonical())
    }
}

/// A single address without `/32` or `/128`.
impl fmt::Display for IpRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.prefix_len() == self.0.max_prefix_len() {
            write!(f, "{}", self.0.addr())
        } else {
            write!(f, "{}", self.0)
        }
    }
}

/// Whether a token with this allowlist may be used from `address`. An
/// empty list allows any address; a list refuses requests whose address
/// is unknown.
fn allows(allowed: &[String], address: Option<IpAddr>) -> bool {
    if allowed.is_empty() {
        return true;
    }
    address.is_some_and(|address| {
        allowed
            .iter()
            .filter_map(|range| IpRange::parse(range))
            .any(|range| range.contains(address))
    })
}

// ---------------------------------------------------------------------------
// Handlers

#[derive(Deserialize)]
pub struct ListQuery {
    /// `all`: every user's tokens (administrators only).
    scope: Option<String>,
}

/// `GET /api/tokens`: the caller's tokens; `?scope=all`: everyone's
/// (administrators).
pub async fn list(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<ListQuery>,
) -> Result<Json<Vec<ApiToken>>, ApiError> {
    let owner = match query.scope.as_deref() {
        Some("all") if user.effective_role == Role::Admin => None,
        Some("all") => return Err(ApiError::forbidden(Role::Admin)),
        _ => Some(user.id.as_str()),
    };
    Ok(Json(state.db.list_api_tokens(owner).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateRequest {
    name: String,
    /// Unix milliseconds; required.
    expires_at: i64,
    /// Addresses and CIDR ranges; empty for any.
    #[serde(default)]
    allowed_ips: Vec<String>,
}

#[derive(Serialize)]
pub struct Created {
    /// The token itself, shown this once.
    token: String,
    #[serde(flatten)]
    info: ApiToken,
}

/// `POST /api/tokens`: a token for the signed-in user. Not with a token
/// (see [`restricted`]).
pub async fn create(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Json(body): Json<CreateRequest>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let name = body.name.trim();
    if name.is_empty() || name.chars().count() > 255 {
        return Err(ApiError::bad_request("The name needs 1–255 characters."));
    }
    let now = now_millis();
    let max_days = state.config.api_tokens.max_lifetime_days.max(1);
    let latest = now.saturating_add((max_days as i64).saturating_mul(86_400_000));
    if body.expires_at <= now {
        return Err(ApiError::bad_request("The expiry must be in the future."));
    }
    if body.expires_at > latest {
        return Err(ApiError::bad_request(format!(
            "Tokens can be valid for at most {max_days} days."
        )));
    }
    let mut allowed_ips = Vec::new();
    for entry in body.allowed_ips.iter().filter(|e| !e.trim().is_empty()) {
        let range = IpRange::parse(entry).ok_or_else(|| {
            ApiError::bad_request(format!(
                "{:?} is not an IP address or a CIDR range such as 10.0.0.0/8.",
                entry.trim()
            ))
        })?;
        let range = range.to_string();
        if !allowed_ips.contains(&range) {
            allowed_ips.push(range);
        }
    }
    if allowed_ips.len() > MAX_ALLOWED_IPS {
        return Err(ApiError::bad_request(format!(
            "A token can list at most {MAX_ALLOWED_IPS} addresses."
        )));
    }

    let token = format!("{TOKEN_PREFIX}{}", auth::new_token());
    let info = state
        .db
        .create_api_token(NewApiToken {
            token_hash: &auth::token_hash(&token),
            prefix: &token[..SHOWN_PREFIX_CHARS],
            user_id: &user.id,
            name,
            allowed_ips: &allowed_ips,
            expires_at: body.expires_at,
        })
        .await?;
    let entry = Entry::new(action::TOKEN_CREATE)
        .by(&user)
        .from(&address)
        .target(target::TOKEN, &info.id, &info.name)
        .changes(json!({
            "expiresAt": info.expires_at,
            "allowedIps": info.allowed_ips,
        }));
    audit::record(&state.db, entry).await;
    Ok((StatusCode::CREATED, Json(Created { token, info })))
}

/// `DELETE /api/tokens/{id}`: revokes one of the caller's tokens, or
/// anyone's for administrators.
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let token = state.db.api_token(&id).await?;
    if token.user_id != user.id && user.effective_role != Role::Admin {
        return Err(ApiError::not_found());
    }
    state.db.delete_api_token(&id).await?;
    let mut entry = Entry::new(action::TOKEN_REVOKE)
        .by(&user)
        .from(&address)
        .target(target::TOKEN, &token.id, &token.name);
    if token.user_id != user.id {
        entry = entry.detail("owner", token.username.as_str());
    }
    audit::record(&state.db, entry).await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_matches_ranges() {
        let range = IpRange::parse("10.1.2.3/8").unwrap();
        assert_eq!(range.to_string(), "10.0.0.0/8");
        assert!(range.contains("10.200.0.1".parse().unwrap()));
        assert!(!range.contains("11.0.0.1".parse().unwrap()));
        // IPv4 clients seen through an IPv6 socket.
        assert!(range.contains("::ffff:10.9.9.9".parse().unwrap()));

        let single = IpRange::parse(" 192.0.2.7 ").unwrap();
        assert_eq!(single.to_string(), "192.0.2.7");
        assert!(single.contains("192.0.2.7".parse().unwrap()));
        assert!(!single.contains("192.0.2.8".parse().unwrap()));

        let v6 = IpRange::parse("2001:db8::1/32").unwrap();
        assert_eq!(v6.to_string(), "2001:db8::/32");
        assert!(v6.contains("2001:db8:ffff::1".parse().unwrap()));
        assert!(!v6.contains("10.0.0.1".parse().unwrap()));

        assert_eq!(
            IpRange::parse("0.0.0.0/0").unwrap().to_string(),
            "0.0.0.0/0"
        );
        for bad in ["", "10.0.0.0/33", "host.example", "10.0.0.0/x", "::/129"] {
            assert_eq!(IpRange::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn allowlists() {
        let ip = |s: &str| Some(s.parse().unwrap());
        assert!(allows(&[], None));
        let list = vec!["10.0.0.0/8".to_owned(), "192.0.2.7".to_owned()];
        assert!(allows(&list, ip("10.3.4.5")));
        assert!(allows(&list, ip("192.0.2.7")));
        assert!(!allows(&list, ip("192.0.2.8")));
        assert!(!allows(&list, None));
    }

    #[test]
    fn reads_bearer_tokens() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer mlc_abc".parse().unwrap());
        assert_eq!(bearer(&headers), Some("mlc_abc"));
        headers.insert(header::AUTHORIZATION, "bearer  mlc_abc ".parse().unwrap());
        assert_eq!(bearer(&headers), Some("mlc_abc"));
        headers.insert(header::AUTHORIZATION, "Basic YTpi".parse().unwrap());
        assert_eq!(bearer(&headers), None);
    }

    #[test]
    fn some_requests_need_a_person() {
        let owner = "u-1";
        assert!(restricted(&Method::POST, "/tokens", owner).is_some());
        assert!(restricted(&Method::GET, "/tokens", owner).is_none());
        assert!(restricted(&Method::DELETE, "/tokens/t-1", owner).is_none());
        assert!(restricted(&Method::PUT, "/auth/password", owner).is_some());
        assert!(restricted(&Method::PUT, "/users/u-1", owner).is_some());
        assert!(restricted(&Method::DELETE, "/users/u%2D1/", owner).is_some());
        assert!(restricted(&Method::GET, "/users/u-1", owner).is_none());
        assert!(restricted(&Method::PUT, "/users/u-2", owner).is_none());
        assert!(restricted(&Method::POST, "/connections", owner).is_none());
    }
}
