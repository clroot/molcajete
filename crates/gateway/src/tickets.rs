//! One-time session links: `POST /api/connections/{id}/tickets` (with an
//! API token or a session) makes a random ticket that opens that
//! connection once, as the caller, without a molcajete login. Another
//! application hands the link to its user, typically in an iframe.
//!
//! The database keeps the ticket's SHA-256 with the user, the connection,
//! the API token that made it (revoking the token deletes its tickets) and
//! display parameters for the session. The WebSocket tunnel takes the
//! ticket (`ticket=` in its URL) and deletes it in the same step, so it
//! works once across replicas; it expires after
//! `[api_tokens] ticket_ttl_seconds`. The session runs with the user's
//! rights as they are when the ticket is used.
//!
//! A ticket authenticates the tunnel by itself, so the tunnel's
//! same-origin check does not apply to it: that check protects the
//! session cookie, which a ticket does not use, and an embedding page on
//! another origin must be able to use the link.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::AppState;
use crate::api::ApiError;
use crate::audit::{self, Entry, action, target};
use crate::auth::{self, ClientAddress, CurrentUser};
use crate::db::{ConnectionRecord, DbError, NewSessionTicket, User, now_millis};
use crate::tokens::TokenAuth;
use crate::tunnel;

#[derive(Deserialize, Default)]
pub struct CreateRequest {
    /// Display parameters for the session: only those a browser may set
    /// for itself (see [`tunnel::valid_override`]), never credentials.
    #[serde(default)]
    parameters: BTreeMap<String, String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Created {
    /// The page that opens the session.
    url: String,
    /// The ticket alone, for a page that runs `MolcajeteSession` itself
    /// (`ticket=` in the tunnel URL).
    ticket: String,
    /// Unix milliseconds.
    expires_at: i64,
}

/// The gateway's address as the browser sees it: `[server] public_url`,
/// otherwise from the request (proxy headers first). Empty when unknown,
/// which makes links relative.
fn public_base(state: &AppState, headers: &HeaderMap) -> String {
    if let Some(url) = &state.config.server.public_url {
        return url.trim_end_matches('/').to_owned();
    }
    let host = ["x-forwarded-host", header::HOST.as_str()]
        .iter()
        .find_map(|name| headers.get(*name).and_then(|v| v.to_str().ok()))
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty());
    match host {
        Some(host) => {
            let scheme = if auth::is_https(headers) {
                "https"
            } else {
                "http"
            };
            format!("{scheme}://{host}")
        }
        None => String::new(),
    }
}

/// `POST /api/connections/{id}/tickets`: a one-time link to a connection
/// the caller may open.
pub async fn create(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    token: Option<Extension<TokenAuth>>,
    address: ClientAddress,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<CreateRequest>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    let connection = state.db.connection_for_user(&id, &user).await?;
    for (name, value) in &body.parameters {
        if !tunnel::valid_override(name, value) {
            return Err(ApiError::bad_request(format!(
                "Parameter {name:?} cannot be set for a session."
            )));
        }
    }
    let parameters = (!body.parameters.is_empty()).then(|| json!(body.parameters).to_string());
    let ticket = auth::new_token();
    let ttl_ms = state.config.api_tokens.ticket_ttl_seconds.max(1) as i64 * 1000;
    let expires_at = now_millis().saturating_add(ttl_ms);
    state
        .db
        .create_session_ticket(NewSessionTicket {
            ticket_hash: &auth::token_hash(&ticket),
            user_id: &user.id,
            connection_id: &connection.id,
            token_id: token.as_ref().map(|t| t.token.id.as_str()),
            parameters: parameters.as_deref(),
            expires_at,
        })
        .await?;
    let entry = Entry::new(action::TICKET_CREATE)
        .by(&user)
        .from(&address)
        .target(target::CONNECTION, &connection.id, &connection.name)
        .detail("expiresAt", expires_at)
        .changes(match &parameters {
            Some(_) => json!({ "parameters": body.parameters }),
            None => json!({}),
        });
    audit::record(&state.db, entry).await;
    let url = format!("{}/#/ticket/{ticket}", public_base(&state, &headers));
    Ok((
        StatusCode::CREATED,
        Json(Created {
            url,
            ticket,
            expires_at,
        }),
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketInfo {
    connection_id: String,
    name: String,
    protocol: String,
    /// Display parameters the session starts from.
    dpi: Option<String>,
    resize_method: Option<String>,
    /// What the connection turns off (`read-only`, `disable-copy`,
    /// `disable-paste`), for the page to say so.
    read_only: bool,
    disable_copy: bool,
    disable_paste: bool,
    expires_at: i64,
}

fn not_usable() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "This session link has expired or has already been used.",
    )
}

/// The ticket's user, if still enabled, and connection, if they may still
/// open it.
async fn resolve(
    state: &AppState,
    user_id: &str,
    connection_id: &str,
) -> Result<(User, ConnectionRecord), ApiError> {
    let user = match state.db.user(user_id).await {
        Ok(user) if !user.disabled => user,
        Ok(_) | Err(DbError::NotFound) => return Err(not_usable()),
        Err(err) => return Err(err.into()),
    };
    match state.db.connection_for_user(connection_id, &user).await {
        Ok(connection) => Ok((user, connection)),
        Err(DbError::NotFound) => Err(not_usable()),
        Err(err) => Err(err.into()),
    }
}

fn parameters(json: Option<&str>) -> BTreeMap<String, String> {
    json.and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or_default()
}

/// `GET /api/tickets/{ticket}`: what the session page needs to show before
/// it connects (no sign-in; the ticket is the credential). Does not use
/// the ticket up.
pub async fn info(
    State(state): State<Arc<AppState>>,
    Path(ticket): Path<String>,
) -> Result<Json<TicketInfo>, ApiError> {
    let found = match state.db.session_ticket(&auth::token_hash(&ticket)).await {
        Ok(found) => found,
        Err(DbError::NotFound) => return Err(not_usable()),
        Err(err) => return Err(err.into()),
    };
    let (_, connection) = resolve(&state, &found.user_id, &found.connection_id).await?;
    let flag = |name: &str| connection.parameters.get(name).is_some_and(|v| v == "true");
    let mut display = connection.parameters.clone();
    display.extend(parameters(found.parameters.as_deref()));
    Ok(Json(TicketInfo {
        connection_id: connection.id,
        name: connection.name,
        protocol: connection.protocol,
        dpi: display.remove("dpi"),
        resize_method: display.remove("resize-method"),
        read_only: flag("read-only"),
        disable_copy: flag("disable-copy"),
        disable_paste: flag("disable-paste"),
        expires_at: found.expires_at,
    }))
}

/// The `ticket=` of a tunnel URL query.
pub fn from_query(query: &str) -> Option<String> {
    form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == "ticket")
        .map(|(_, value)| value.into_owned())
        .filter(|value| !value.is_empty())
}

/// A ticket taken by the tunnel: who opens which connection, with which
/// display parameters.
pub struct Redeemed {
    pub user: User,
    pub connection: ConnectionRecord,
    pub parameters: BTreeMap<String, String>,
}

/// Uses the ticket up for the tunnel and records `ticket.use`.
pub async fn redeem(state: &AppState, ticket: &str, address: &str) -> Result<Redeemed, ApiError> {
    let found = match state.db.redeem_ticket(&auth::token_hash(ticket)).await {
        Ok(found) => found,
        Err(DbError::NotFound) => return Err(not_usable()),
        Err(err) => return Err(err.into()),
    };
    let (user, connection) = resolve(state, &found.user_id, &found.connection_id).await?;
    let entry = Entry::new(action::TICKET_USE)
        .by(&user)
        .from(&ClientAddress(address.to_owned()))
        .target(target::CONNECTION, &connection.id, &connection.name);
    audit::record(&state.db, entry).await;
    Ok(Redeemed {
        user,
        connection,
        parameters: parameters(found.parameters.as_deref()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_ticket_from_the_tunnel_query() {
        assert_eq!(
            from_query("connection=a&ticket=abc&width=10").as_deref(),
            Some("abc")
        );
        assert_eq!(from_query("connection=a&ticket="), None);
        assert_eq!(from_query("connection=a"), None);
    }
}
