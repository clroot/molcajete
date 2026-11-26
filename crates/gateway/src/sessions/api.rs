//! Administrators' API for sessions: the live ones (`/api/sessions`) and
//! the connection history (`/api/history`).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use molcajete_protocol::Status;
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use super::{EndReason, LiveSession, policy};
use crate::AppState;
use crate::api::ApiError;
use crate::audit;
use crate::auth::{AdminUser, ClientAddress};
use crate::db::{HistoryFilter, now_millis};

/// `GET /api/sessions`: every live session, oldest first.
pub async fn list(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
) -> Json<Vec<LiveSession>> {
    Json(state.sessions.list())
}

/// `DELETE /api/sessions/{id}`: ends the session; its user sees that an
/// administrator ended it.
pub async fn end(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session = Uuid::parse_str(&id)
        .ok()
        .and_then(|id| state.sessions.get(id))
        .ok_or_else(ApiError::not_found)?;
    if !policy::end(&state, &session, EndReason::Admin, &admin, &address).await {
        return Err(ApiError::not_found());
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
pub struct EndAllQuery {
    /// Only the sessions of this user (id or username).
    user: Option<String>,
}

/// `DELETE /api/sessions`: ends every live session, or with `?user=` every
/// session of that user, except the caller's own. `{"ended": n}`.
pub async fn end_all(
    State(state): State<Arc<AppState>>,
    AdminUser(admin): AdminUser,
    address: ClientAddress,
    Query(query): Query<EndAllQuery>,
) -> Json<serde_json::Value> {
    let user = query
        .user
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let mut ended = 0;
    for session in state.sessions.list() {
        let selected = match user {
            Some(user) => session.user_id == user || session.username == user,
            None => session.user_id != admin.id,
        };
        if selected && policy::end(&state, &session, EndReason::Admin, &admin, &address).await {
            ended += 1;
        }
    }
    Json(json!({ "ended": ended }))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct HistoryQuery {
    /// The user's username or id.
    user: Option<String>,
    /// The connection's id or name.
    connection: Option<String>,
    /// Started at this time or later, Unix milliseconds.
    from: Option<i64>,
    /// Started before this time, Unix milliseconds.
    to: Option<i64>,
    /// Sessions per page (default 100, at most 1000).
    limit: Option<i64>,
    offset: Option<i64>,
    /// `csv` for a CSV file of every matching session (up to 100 000).
    format: Option<String>,
}

const PAGE: i64 = 100;
const MAX_PAGE: i64 = 1000;
const MAX_CSV: usize = 100_000;

impl HistoryQuery {
    fn filter(&self) -> HistoryFilter {
        let text = |value: &Option<String>| {
            value
                .as_deref()
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        HistoryFilter {
            user: text(&self.user),
            connection: text(&self.connection),
            since: self.from,
            until: self.to,
        }
    }
}

/// `GET /api/history`: `{"entries": […], "hasMore": bool}`, most recently
/// started first, or with `format=csv` every matching session as a CSV
/// download.
pub async fn history(
    State(state): State<Arc<AppState>>,
    AdminUser(_): AdminUser,
    Query(query): Query<HistoryQuery>,
) -> Result<Response, ApiError> {
    let filter = query.filter();
    match query.format.as_deref() {
        Some("csv") => return history_csv(&state, &filter).await,
        None | Some("json") => {}
        Some(_) => return Err(ApiError::bad_request("The format is json or csv.")),
    }
    let limit = query.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE);
    let offset = query.offset.unwrap_or(0).max(0);
    let mut entries = state.db.list_history(&filter, limit + 1, offset).await?;
    let has_more = entries.len() as i64 > limit;
    entries.truncate(limit as usize);
    Ok(Json(json!({ "entries": entries, "hasMore": has_more })).into_response())
}

/// How a session ended, for the CSV file: the reason someone else ended it,
/// `closed` or the name of the error status.
fn outcome(status: i64, reason: Option<&str>) -> String {
    if let Some(reason) = reason {
        return reason.to_owned();
    }
    let status = Status(status as u16);
    if status.is_error() {
        status.name().to_owned()
    } else {
        "closed".to_owned()
    }
}

async fn history_csv(state: &AppState, filter: &HistoryFilter) -> Result<Response, ApiError> {
    let mut csv = String::from(
        "started_at,ended_at,duration_seconds,user,user_id,connection,connection_id,protocol,\
         peer_address,outcome,status,ended_by,bytes_to_browser,bytes_to_guacd,recording_id,recording_ids\r\n",
    );
    let mut written = 0;
    while written < MAX_CSV {
        let page = state
            .db
            .list_history(filter, MAX_PAGE, written as i64)
            .await?;
        for entry in &page {
            let fields = [
                audit::rfc3339(entry.started_at),
                audit::rfc3339(entry.ended_at),
                ((entry.ended_at - entry.started_at).max(0) / 1000).to_string(),
                entry.username.clone(),
                entry.user_id.clone(),
                entry.connection_name.clone(),
                entry.connection_id.clone(),
                entry.protocol.clone(),
                entry.peer_address.clone().unwrap_or_default(),
                outcome(entry.end_status, entry.end_reason.as_deref()),
                entry.end_status.to_string(),
                entry.ended_by.clone().unwrap_or_default(),
                entry.bytes_to_browser.to_string(),
                entry.bytes_to_guacd.to_string(),
                entry.recording_id.clone().unwrap_or_default(),
                // Every recording of the session, separated by spaces.
                entry.recording_ids.join(" "),
            ];
            let row: Vec<String> = fields.iter().map(|f| audit::csv_field(f)).collect();
            csv.push_str(&row.join(","));
            csv.push_str("\r\n");
        }
        written += page.len();
        if (page.len() as i64) < MAX_PAGE {
            break;
        }
    }
    let name = format!(
        "attachment; filename=\"molcajete-history-{}.csv\"",
        audit::rfc3339(now_millis()).replace([':', '.'], "-")
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

    #[test]
    fn names_outcomes() {
        assert_eq!(outcome(0, None), "closed");
        assert_eq!(outcome(0x020B, Some("admin")), "admin");
        assert_eq!(outcome(0x0203, None), "UPSTREAM_ERROR");
    }
}
