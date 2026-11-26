//! Sharing a live session: share links, and administrators watching
//! (shadowing).
//!
//! The page of a session names its tunnel with `page=<id>`; its user
//! manages sharing under `/api/my-sessions/{page}`, which finds the session
//! with [`super::Registry::by_page`]. A link is a random token whose hash
//! is kept in the registry with the session's tunnel id, whether whoever
//! opens it only watches, and when it expires. Anyone signed in to the
//! gateway may open it (`/websocket-tunnel?join=<token>`): the link is the
//! grant, to that one session, so no grant of the connection is needed.
//! Administrators watch any live session read-only
//! (`/websocket-tunnel?shadow=<tunnel id>`).
//!
//! Either way the gateway opens a guacd connection of its own that selects
//! the owner's (`select,<guacd id>`) on the same guacd, with `read-only`
//! set for those who only watch, which stock guacd and molcajeted both
//! enforce.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use molcajete_protocol::Status;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{JoinVia, Joined, LiveSession};
use crate::AppState;
use crate::api::ApiError;
use crate::audit::{self, Entry, action, target};
use crate::auth::{ClientAddress, CurrentUser};
use crate::db::{Role, User};

/// What a tunnel that joins a live session joins, and how.
#[derive(Debug, Clone)]
pub struct Join {
    pub session: LiveSession,
    pub joined: Joined,
}

/// The session a tunnel request joins: through share link `token`, or,
/// for an administrator, `shadow` (a tunnel id). `Err` carries the status
/// the tunnel closes with: `RESOURCE_NOT_FOUND` for an unknown, expired or
/// revoked link or session, `CLIENT_FORBIDDEN` when the feature is off or
/// the user may not watch.
pub fn resolve(
    state: &AppState,
    user: &User,
    token: Option<&str>,
    shadow: Option<&str>,
) -> Result<Join, Status> {
    let sharing = &state.config.sharing;
    if let Some(token) = token {
        if !sharing.links {
            return Err(Status::CLIENT_FORBIDDEN);
        }
        let (link, session) = state
            .sessions
            .link_by_token(token)
            .ok_or(Status::RESOURCE_NOT_FOUND)?;
        return Ok(Join {
            joined: Joined {
                owner: session.id,
                via: JoinVia::Link(link.id),
                read_only: link.read_only,
            },
            session,
        });
    }
    let shadow = shadow.ok_or(Status::RESOURCE_NOT_FOUND)?;
    if !sharing.shadowing || user.effective_role < Role::Admin {
        return Err(Status::CLIENT_FORBIDDEN);
    }
    let session = Uuid::parse_str(shadow)
        .ok()
        .and_then(|id| state.sessions.get(id))
        .filter(|session| session.joined.is_none())
        .ok_or(Status::RESOURCE_NOT_FOUND)?;
    Ok(Join {
        joined: Joined {
            owner: session.id,
            via: JoinVia::Shadow,
            read_only: true,
        },
        session,
    })
}

/// Records that `user` joined, once the tunnel is established.
pub async fn record_join(state: &AppState, user: &User, address: &ClientAddress, join: &Join) {
    let session = &join.session;
    let entry = match join.joined.via {
        JoinVia::Link(link) => Entry::new(action::SESSION_JOIN)
            .detail("linkId", link.to_string())
            .detail("readOnly", join.joined.read_only),
        JoinVia::Shadow => Entry::new(action::SESSION_SHADOW),
    };
    let entry = entry
        .by(user)
        .from(address)
        .target(
            target::SESSION,
            &session.id.to_string(),
            &session.connection_name,
        )
        .detail("owner", session.username.as_str())
        .detail("connection", session.connection_id.as_str());
    audit::record(&state.db, entry).await;
}

/// The caller's own session opened from page `page`.
pub(super) fn own_session(
    state: &AppState,
    user: &User,
    page: &str,
) -> Result<LiveSession, ApiError> {
    state
        .sessions
        .by_page(&user.id, page)
        .ok_or_else(ApiError::not_found)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Participant {
    id: String,
    username: String,
    /// `link` or `shadow`.
    kind: &'static str,
    read_only: bool,
    started_at: i64,
    /// The link joined through.
    link_id: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkView {
    #[serde(flatten)]
    link: super::Link,
    /// Sessions joined through it right now.
    joined: usize,
}

/// `GET /api/my-sessions/{page}`: the caller's live session from page
/// `page`: who joined it, its links, whether it may be shared, and whether
/// it is recorded or may be ([`super::record::menu_state`]).
pub async fn get(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    Path(page): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let session = own_session(&state, &user, &page)?;
    let joined = state.sessions.joined_to(session.id);
    let participants: Vec<Participant> = joined
        .iter()
        .map(|s| Participant {
            id: s.id.to_string(),
            username: s.username.clone(),
            kind: s.join_kind.unwrap_or("link"),
            read_only: s.read_only,
            started_at: s.started_at,
            link_id: match s.joined.map(|j| j.via) {
                Some(JoinVia::Link(link)) => Some(link.to_string()),
                _ => None,
            },
        })
        .collect();
    let links: Vec<LinkView> = state
        .sessions
        .links_of(session.id)
        .into_iter()
        .map(|link| LinkView {
            joined: joined
                .iter()
                .filter(|s| s.joined.is_some_and(|j| j.via == JoinVia::Link(link.id)))
                .count(),
            link,
        })
        .collect();
    Ok(Json(json!({
        "id": session.id.to_string(),
        "connectionName": session.connection_name,
        "protocol": session.protocol,
        "startedAt": session.started_at,
        "participants": participants,
        "links": links,
        "sharing": state.config.sharing.links,
        "maxLinkMinutes": state.config.sharing.max_link_minutes,
        "recording": super::record::menu_state(&state, &session).await,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NewLink {
    /// Who opens the link only watches (the default).
    #[serde(default = "yes")]
    read_only: bool,
    /// How long the link lasts (default and at most
    /// `[sharing] max_link_minutes`).
    expires_in_minutes: Option<u64>,
}

fn yes() -> bool {
    true
}

/// `POST /api/my-sessions/{page}/links`: creates a share link of the
/// caller's session. The response carries the token, which is not shown
/// again.
pub async fn create_link(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Path(page): Path<String>,
    Json(body): Json<NewLink>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let sharing = &state.config.sharing;
    if !sharing.links {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "Sharing sessions is turned off.",
        ));
    }
    let session = own_session(&state, &user, &page)?;
    let minutes = body.expires_in_minutes.unwrap_or(sharing.max_link_minutes);
    if minutes == 0 || minutes > sharing.max_link_minutes {
        return Err(ApiError::bad_request(format!(
            "A link lasts from 1 to {} minutes.",
            sharing.max_link_minutes
        )));
    }
    let (link, token) = state.sessions.create_link(
        session.id,
        body.read_only,
        Duration::from_secs(minutes * 60),
    );
    audit::record(
        &state.db,
        Entry::new(action::SESSION_SHARE)
            .by(&user)
            .from(&address)
            .target(
                target::SESSION,
                &session.id.to_string(),
                &session.connection_name,
            )
            .detail("linkId", link.id.to_string())
            .detail("readOnly", link.read_only)
            .detail("expiresAt", link.expires_at),
    )
    .await;
    let mut response = serde_json::to_value(LinkView { link, joined: 0 })
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    response["token"] = json!(token);
    Ok((StatusCode::CREATED, Json(response)))
}

/// `DELETE /api/my-sessions/{page}/links/{id}`: revokes a link; whoever
/// joined through it is disconnected.
pub async fn revoke_link(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Path((page, id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let session = own_session(&state, &user, &page)?;
    let id = Uuid::parse_str(&id).map_err(|_| ApiError::not_found())?;
    if !state.sessions.revoke_link(session.id, id) {
        return Err(ApiError::not_found());
    }
    audit::record(
        &state.db,
        Entry::new(action::SESSION_UNSHARE)
            .by(&user)
            .from(&address)
            .target(
                target::SESSION,
                &session.id.to_string(),
                &session.connection_name,
            )
            .detail("linkId", id.to_string()),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

/// `GET /api/join/{token}`: what a share link opens, for the page to show
/// before joining and to tell while joined whether the session is being
/// recorded. 404 for an unknown, expired or revoked link.
pub async fn describe_link(
    State(state): State<Arc<AppState>>,
    CurrentUser(_): CurrentUser,
    Path(token): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !state.config.sharing.links {
        return Err(ApiError::not_found());
    }
    let (link, session) = state
        .sessions
        .link_by_token(&token)
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({
        "connectionName": session.connection_name,
        "protocol": session.protocol,
        "owner": session.username,
        "readOnly": link.read_only,
        "expiresAt": link.expires_at,
        "recording": session.recording,
    })))
}
