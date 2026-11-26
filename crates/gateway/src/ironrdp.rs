//! RDP sessions run by IronRDP's web client in the browser, with
//! molcajeted only relaying the RDP stream (`select,rdp-relay`).
//!
//! ```text
//! POST /api/connections/{id}/ironrdp   → { token, username, password }
//! GET  /websocket-tunnel?connection={id}&ironrdp={token}   (WebSocket)
//! ```
//!
//! The browser's RDP client has to authenticate with something, but the
//! connection's stored credentials must not reach it. So the page first
//! asks for a one-time credential, valid for [`GRANT_TTL`] and for one
//! tunnel of the same user to the same connection. The tunnel hands it to
//! molcajeted (`relay-username`, `relay-password`), which accepts only that
//! credential from the browser and logs on to the server with the stored
//! ones.
//!
//! On the WebSocket the browser and molcajeted exchange RDCleanPath and
//! RDP bytes in binary messages, which the gateway carries without looking
//! at them. Sessions of this kind cannot be recorded, shared or watched:
//! the gateway never sees Guacamole instructions.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Path, State};
use futures_util::{SinkExt as _, StreamExt as _};
use metrics::{counter, gauge, histogram};
use molcajete_protocol::Status;
use serde::Serialize;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::AppState;
use crate::api::ApiError;
use crate::auth::{self, CurrentUser};
use crate::config::RecordingsConfig;
use crate::db::{ConnectionRecord, attributes};
use crate::guacd;
use crate::recording;
use crate::sessions::{self, SessionStart};
use crate::tunnel::{self, RelayCounters, TunnelRequest};

/// `select` value molcajeted answers with an RDP relay.
pub const SELECT: &str = "rdp-relay";

/// How long a one-time credential can be used to open its tunnel.
pub const GRANT_TTL: Duration = Duration::from_secs(60);

const READ_BUFFER_BYTES: usize = 64 * 1024;

/// Whether sessions of `connection` may use IronRDP's web client: an RDP
/// connection whose administrator chose it as the default or allowed
/// switching to it, and whose sessions are not recorded.
pub fn allowed(connection: &ConnectionRecord, recordings: &RecordingsConfig) -> bool {
    connection.protocol == "rdp"
        && (connection.flag(attributes::IRONRDP_DEFAULT)
            || connection.flag(attributes::IRONRDP_SWITCH))
        && !recording::wanted(recordings, connection)
}

/// A one-time credential, and whom it was issued to.
struct Grant {
    user_id: String,
    connection_id: String,
    credential: Credential,
    expires: Instant,
}

/// What the browser's RDP client authenticates to molcajeted with.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Credential {
    pub username: String,
    pub password: String,
}

/// One-time credentials issued and not used yet. They live in memory
/// only: a gateway that restarts in between asks the page to try again.
#[derive(Default)]
pub struct Grants(Mutex<HashMap<String, Grant>>);

impl Grants {
    /// Issues a credential for one tunnel of `user_id` to `connection_id`.
    /// Returns the token that opens the tunnel and the credential.
    pub fn issue(&self, user_id: &str, connection_id: &str) -> (String, Credential) {
        let token = auth::new_token();
        let credential = Credential {
            // NTLM user names are at most 20 characters for older servers'
            // sake; this one never reaches a server.
            username: format!("molcajete-{}", &auth::new_token()[..8]),
            password: auth::new_token(),
        };
        let now = Instant::now();
        let mut grants = self.0.lock().expect("grants lock");
        grants.retain(|_, grant| grant.expires > now);
        grants.insert(
            token.clone(),
            Grant {
                user_id: user_id.to_owned(),
                connection_id: connection_id.to_owned(),
                credential: credential.clone(),
                expires: now + GRANT_TTL,
            },
        );
        (token, credential)
    }

    /// Takes the credential of `token` if it was issued to this user for
    /// this connection and has not expired. A token is used once, whether
    /// or not it matches.
    pub fn take(&self, token: &str, user_id: &str, connection_id: &str) -> Option<Credential> {
        let grant = self.0.lock().expect("grants lock").remove(token)?;
        (grant.expires > Instant::now()
            && grant.user_id == user_id
            && grant.connection_id == connection_id)
            .then_some(grant.credential)
    }
}

#[derive(Debug, Serialize)]
pub struct Issued {
    token: String,
    #[serde(flatten)]
    credential: Credential,
}

/// `POST /api/connections/{id}/ironrdp`
pub async fn create(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
) -> Result<Json<Issued>, ApiError> {
    let connection = state.db.connection_for_user(&id, &user).await?;
    if !allowed(&connection, &state.config.recordings) {
        return Err(ApiError::bad_request(
            "This connection does not use IronRDP's client.",
        ));
    }
    let (token, credential) = state.ironrdp.issue(&user.id, &connection.id);
    Ok(Json(Issued { token, credential }))
}

/// Serves a tunnel opened with `ironrdp=<token>`.
pub async fn serve(
    mut socket: WebSocket,
    state: Arc<AppState>,
    request: TunnelRequest,
    connection: Option<ConnectionRecord>,
    token: &str,
    tunnel_id: Uuid,
) {
    let Some(connection) = connection else {
        warn!(connection = ?request.connection, "IronRDP tunnel requested for unknown or forbidden connection");
        counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "unknown_connection")
            .increment(1);
        let _ = socket
            .send(tunnel::close_message(Status::RESOURCE_NOT_FOUND))
            .await;
        return;
    };
    let user = request.user.clone().unwrap_or_default();
    let credential = state.ironrdp.take(token, &user.id, &connection.id);
    let Some(credential) = credential.filter(|_| allowed(&connection, &state.config.recordings))
    else {
        warn!(connection = %connection.id, "IronRDP tunnel without a valid one-time credential");
        counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "ironrdp_grant")
            .increment(1);
        let _ = socket
            .send(tunnel::close_message(Status::CLIENT_FORBIDDEN))
            .await;
        return;
    };
    let labels = [
        ("connection", connection.id.clone()),
        ("protocol", connection.protocol.clone()),
    ];

    let defaults = &state.config.guacd;
    let guacd_host = connection.guacd_host.as_deref().unwrap_or(&defaults.host);
    let guacd_port = connection.guacd_port.unwrap_or(defaults.port);
    let handshake_timeout = connection
        .guacd_handshake_timeout
        .unwrap_or(defaults.handshake_timeout);
    let mut parameters = connection.parameters.clone();
    match state.db.connection_secrets(&connection.id).await {
        Ok(secrets) => parameters.extend(secrets),
        Err(err) => {
            error!(connection = %connection.id, error = %err, "reading the connection's credentials failed");
            counter!("molcajete_gateway_tunnels_rejected_total", "reason" => "credentials")
                .increment(1);
            let _ = socket
                .send(tunnel::close_message(Status::SERVER_ERROR))
                .await;
            return;
        }
    }
    // The one-time credential is the gateway's alone: stored parameters of
    // these names are replaced.
    parameters.insert("relay-username".to_owned(), credential.username);
    parameters.insert("relay-password".to_owned(), credential.password);
    info!(connection = %connection.id, client = "ironrdp", "opening tunnel");

    let handshake_started = Instant::now();
    let session = match guacd::connect(
        (guacd_host, guacd_port),
        SELECT,
        &parameters,
        &request.info,
        false,
        Duration::from_secs(handshake_timeout),
    )
    .await
    {
        Ok(session) => session,
        Err(err) => {
            // Stock guacd answers `select,rdp-relay` with an error.
            warn!(error = %err, "RDP relay handshake failed");
            counter!("molcajete_gateway_handshake_failures_total", &labels).increment(1);
            let _ = socket.send(tunnel::close_message(err.status())).await;
            return;
        }
    };
    let handshake = handshake_started.elapsed();
    histogram!("molcajete_gateway_handshake_seconds", &labels).record(handshake);
    info!(
        guacd_id = %session.id,
        framing = "rdp",
        handshake_ms = handshake.as_millis() as u64,
        "tunnel established"
    );

    let registration = state.sessions.register(
        tunnel_id,
        SessionStart {
            user_id: user.id,
            username: user.username,
            connection_id: connection.id.clone(),
            connection_name: connection.name.clone(),
            protocol: connection.protocol.clone(),
            peer: request.peer.clone(),
            guacd: (guacd_host.to_owned(), guacd_port),
            guacd_id: session.id.clone(),
            recording_id: None,
            page_id: request.page.clone(),
            joined: None,
            client: request.info.clone(),
        },
    );
    let active = gauge!("molcajete_gateway_tunnels_active", &labels);
    active.increment(1.0);
    let started = Instant::now();
    let counters = RelayCounters {
        to_browser: counter!(
            "molcajete_gateway_relayed_bytes_total",
            &tunnel::with(&labels, "direction", "to_browser")
        ),
        to_guacd: counter!(
            "molcajete_gateway_relayed_bytes_total",
            &tunnel::with(&labels, "direction", "to_guacd")
        ),
    };
    let status = relay(socket, session, &counters, registration.traffic(), async {
        registration.ended().await.reason.status()
    })
    .await;
    sessions::history::record(&state.db, &registration, status).await;
    let traffic = registration.traffic();
    let (to_browser, to_guacd) = (
        traffic.to_browser.load(Ordering::Relaxed),
        traffic.to_guacd.load(Ordering::Relaxed),
    );
    let ending = registration.ending();
    drop(registration);
    active.decrement(1.0);
    let duration = started.elapsed();
    histogram!("molcajete_gateway_tunnel_duration_seconds", &labels).record(duration);
    counter!(
        "molcajete_gateway_tunnels_total",
        &tunnel::with(&labels, "status", status.code().to_string())
    )
    .increment(1);
    info!(
        %status,
        duration_s = duration.as_secs_f64(),
        bytes_to_browser = to_browser,
        bytes_to_guacd = to_guacd,
        ended_for = ending.as_ref().map(|e| e.reason.as_str()),
        ended_by = ending.as_ref().and_then(|e| e.by.as_deref()),
        "tunnel closed"
    );
}

/// Carries bytes both ways until either side closes or the session is
/// ended: binary WebSocket messages to molcajeted as they are, and what
/// molcajeted sends as binary messages.
async fn relay(
    socket: WebSocket,
    session: guacd::GuacdSession,
    counters: &RelayCounters,
    traffic: &sessions::Traffic,
    ended: impl Future<Output = Status>,
) -> Status {
    let (mut ws_tx, mut ws_rx) = socket.split();
    let pending = session.parser.pending().to_vec();
    let (mut rd, mut wr) = session.stream.into_split();

    let to_browser = async {
        let mut chunk = pending;
        let mut buf = vec![0u8; READ_BUFFER_BYTES];
        loop {
            if !chunk.is_empty() {
                counters.to_browser.increment(chunk.len() as u64);
                traffic
                    .to_browser
                    .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                if ws_tx.send(Message::Binary(chunk.into())).await.is_err() {
                    return (Status::SUCCESS, ws_tx);
                }
            }
            match rd.read(&mut buf).await {
                Ok(0) => return (Status::SUCCESS, ws_tx),
                Ok(n) => chunk = buf[..n].to_vec(),
                Err(err) => {
                    debug!(error = %err, "molcajeted read failed");
                    return (Status::UPSTREAM_ERROR, ws_tx);
                }
            }
        }
    };
    let to_daemon = async {
        while let Some(message) = ws_rx.next().await {
            let data = match message {
                Ok(Message::Binary(data)) => data,
                Ok(Message::Close(_)) => break,
                Ok(_) => continue,
                Err(err) => {
                    debug!(error = %err, "websocket receive failed");
                    break;
                }
            };
            if wr.write_all(&data).await.is_err() {
                return Status::UPSTREAM_ERROR;
            }
            counters.to_guacd.increment(data.len() as u64);
            traffic
                .to_guacd
                .fetch_add(data.len() as u64, Ordering::Relaxed);
        }
        let _ = wr.shutdown().await;
        Status::SUCCESS
    };
    tokio::pin!(to_browser);
    let (status, ws_tx) = tokio::select! {
        result = &mut to_browser => (result.0, Some(result.1)),
        status = to_daemon => (status, None),
        status = ended => {
            debug!(%status, "session ended by the gateway");
            (status, None)
        }
    };
    if let Some(mut ws_tx) = ws_tx {
        let _ = ws_tx.send(tunnel::close_message(status)).await;
        let _ = ws_tx.close().await;
    }
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_grant_opens_one_tunnel_of_its_user_and_connection() {
        let grants = Grants::default();
        let (token, credential) = grants.issue("alice", "rdp-1");
        assert!(credential.username.starts_with("molcajete-"));
        assert_ne!(credential.password, token);
        assert_eq!(grants.take(&token, "alice", "rdp-1"), Some(credential));
        assert_eq!(grants.take(&token, "alice", "rdp-1"), None);
    }

    #[test]
    fn a_grant_is_spent_by_a_wrong_use() {
        let grants = Grants::default();
        let (token, _) = grants.issue("alice", "rdp-1");
        assert_eq!(grants.take(&token, "bob", "rdp-1"), None);
        assert_eq!(grants.take(&token, "alice", "rdp-1"), None);
        let (token, _) = grants.issue("alice", "rdp-1");
        assert_eq!(grants.take(&token, "alice", "rdp-2"), None);
        assert_eq!(grants.take("unknown", "alice", "rdp-1"), None);
    }

    #[test]
    fn an_expired_grant_is_refused() {
        let grants = Grants::default();
        let (token, _) = grants.issue("alice", "rdp-1");
        grants.0.lock().unwrap().get_mut(&token).unwrap().expires = Instant::now();
        assert_eq!(grants.take(&token, "alice", "rdp-1"), None);
    }

    fn connection(protocol: &str, attributes: &[&str]) -> ConnectionRecord {
        let attributes: serde_json::Map<String, serde_json::Value> = attributes
            .iter()
            .map(|name| ((*name).to_owned(), "true".into()))
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": "c",
            "name": "c",
            "protocol": protocol,
            "attributes": attributes,
        }))
        .unwrap()
    }

    #[test]
    fn only_unrecorded_rdp_connections_that_opted_in_are_allowed() {
        let recordings = RecordingsConfig::default();
        assert!(!allowed(&connection("rdp", &[]), &recordings));
        assert!(allowed(
            &connection("rdp", &[attributes::IRONRDP_DEFAULT]),
            &recordings
        ));
        assert!(allowed(
            &connection("rdp", &[attributes::IRONRDP_SWITCH]),
            &recordings
        ));
        assert!(!allowed(
            &connection("vnc", &[attributes::IRONRDP_DEFAULT]),
            &recordings
        ));
        assert!(!allowed(
            &connection("rdp", &[attributes::IRONRDP_DEFAULT, attributes::RECORD]),
            &recordings
        ));
    }
}
