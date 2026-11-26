//! Recording a session on demand, from its session menu.
//!
//! *Record sessions* decides when a tunnel opens and records it from the
//! owner's own stream. A recording started from the menu instead joins the
//! live session with a read-only connection of its own (`select,<guacd
//! id>`, like sharing), so it begins with the screen as it is: stock guacd
//! and molcajeted send a joining user the whole display first. That
//! connection announces the stock image formats only, unless the
//! connection keeps molcajete's, whatever the owner's tunnel uses. The
//! owner's mouse and keys still come from their tunnel
//! ([`Slot::recorder`]), as the connection's recording choices allow, and
//! the pointer positions the daemon sends the recorder are left out.
//!
//! A session's [`Slot`] holds the recording that runs, if any, and the ids
//! of those it made. Each start-to-stop span is a recording of its own,
//! linked to the session by `session_id` and owned by whoever started it
//! (`started_by`), who may play and download it. The recording stops when
//! the owner stops it, when the session ends, or when the daemon closes the
//! recorder's connection.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use molcajete_protocol::InstructionBuilder;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::{oneshot, watch};
use tracing::{Instrument as _, error, info, info_span, warn};

use super::LiveSession;
use super::share::own_session;
use crate::AppState;
use crate::api::ApiError;
use crate::audit::{self, Entry, action, target};
use crate::auth::{ClientAddress, CurrentUser};
use crate::db::{ConnectionRecord, DbError, User, attributes, now_millis};
use crate::guacd::{self, GuacdSession};
use crate::recording::{self, Origin, RecordedUser, Recorder};

/// How long stopping waits for the recording to be complete.
const STOP_WAIT: Duration = Duration::from_secs(10);

/// Why a recording started from the menu stopped.
pub enum Stop {
    /// The owner stopped it.
    ByUser { user: User, address: String },
    /// The session ended.
    SessionEnded,
    /// The daemon closed the recorder's connection.
    DaemonClosed,
    /// The daemon sent something the recorder could not read.
    Failed,
}

impl Stop {
    fn as_str(&self) -> &'static str {
        match self {
            Self::ByUser { .. } => "stopped",
            Self::SessionEnded => "session_ended",
            Self::DaemonClosed => "daemon_closed",
            Self::Failed => "error",
        }
    }
}

/// The recording that runs.
struct Current {
    recorder: Arc<Recorder>,
    stop: oneshot::Sender<Stop>,
    /// Turns true once the recording is complete.
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct SlotState {
    current: Option<Current>,
    /// A recording is being started.
    starting: bool,
    /// The ids of every recording started, oldest first.
    made: Vec<String>,
}

/// The recordings of one session started from its menu.
#[derive(Default)]
pub struct Slot {
    state: Mutex<SlotState>,
}

impl Slot {
    fn state(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The recording that runs, for the owner's tunnel to add their input.
    pub fn recorder(&self) -> Option<Arc<Recorder>> {
        self.state().current.as_ref().map(|c| c.recorder.clone())
    }

    /// The id and start (Unix milliseconds) of the recording that runs.
    pub fn current(&self) -> Option<(String, i64)> {
        self.state()
            .current
            .as_ref()
            .map(|c| (c.recorder.id().to_owned(), c.recorder.started_at()))
    }

    /// Every recording started, oldest first, the one that runs included.
    pub fn recordings(&self) -> Vec<String> {
        self.state().made.clone()
    }

    /// Reserves the slot for a recording about to start; `false` when one
    /// runs or is starting.
    fn reserve(&self) -> bool {
        let mut state = self.state();
        if state.current.is_some() || state.starting {
            return false;
        }
        state.starting = true;
        true
    }

    fn release(&self) {
        self.state().starting = false;
    }

    /// Makes `recorder` the one that runs, after [`Slot::reserve`].
    fn begin(&self, recorder: Arc<Recorder>) -> (oneshot::Receiver<Stop>, watch::Sender<bool>) {
        let (stop, stopped) = oneshot::channel();
        let (finished, done) = watch::channel(false);
        let mut state = self.state();
        state.starting = false;
        state.made.push(recorder.id().to_owned());
        state.current = Some(Current {
            recorder,
            stop,
            done,
        });
        (stopped, finished)
    }

    /// Forgets recording `id` once it stopped by itself.
    fn clear(&self, id: &str) {
        let mut state = self.state();
        if state
            .current
            .as_ref()
            .is_some_and(|c| c.recorder.id() == id)
        {
            state.current = None;
        }
    }

    /// Stops the recording that runs, if any, without waiting for it to be
    /// complete; returns what turns true once it is.
    pub fn stop(&self, reason: Stop) -> Option<watch::Receiver<bool>> {
        let current = self.state().current.take()?;
        let _ = current.stop.send(reason);
        Some(current.done)
    }
}

/// What the session menu shows about recording (`recording` in `GET
/// /api/my-sessions/{page}`).
pub async fn menu_state(state: &AppState, session: &LiveSession) -> Value {
    let always = session.recording_id.is_some();
    let current = state
        .sessions
        .recording_slot(session.id)
        .and_then(|slot| slot.current());
    let on_demand = !always
        && state.config.recordings.enabled
        && state
            .db
            .connection(&session.connection_id)
            .await
            .is_ok_and(|c| c.flag(attributes::RECORD_ON_DEMAND));
    json!({
        "always": always,
        "active": always || current.is_some(),
        "onDemand": on_demand,
        "recordingId": current.as_ref().map(|(id, _)| id.clone()),
        "startedAt": current.map(|(_, at)| at),
    })
}

fn forbidden(message: &str) -> ApiError {
    ApiError::new(StatusCode::FORBIDDEN, message)
}

fn conflict(message: &str) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, message)
}

/// `POST /api/my-sessions/{page}/recording`: starts recording the caller's
/// session from page `page`. 403 when recording is off in the gateway or
/// the connection does not allow it, 409 when the session is recorded
/// already.
pub async fn start(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Path(page): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    if !state.config.recordings.enabled {
        return Err(forbidden("Recording is turned off in the gateway."));
    }
    let session = own_session(&state, &user, &page)?;
    if session.recording_id.is_some() {
        return Err(conflict("Every session of this connection is recorded."));
    }
    let connection = match state.db.connection(&session.connection_id).await {
        Ok(connection) => connection,
        Err(DbError::NotFound) => return Err(ApiError::not_found()),
        Err(err) => return Err(err.into()),
    };
    if !connection.flag(attributes::RECORD_ON_DEMAND) {
        return Err(forbidden(
            "This connection does not allow recording from the session menu.",
        ));
    }
    let slot = state
        .sessions
        .recording_slot(session.id)
        .ok_or_else(ApiError::not_found)?;
    if !slot.reserve() {
        return Err(conflict("The session is already being recorded."));
    }
    let started = begin(&state, &slot, &session, &connection, &user, &address).await;
    if started.is_err() {
        slot.release();
    }
    let (id, started_at) = started?;
    Ok((
        StatusCode::CREATED,
        Json(json!({ "recordingId": id, "startedAt": started_at })),
    ))
}

/// Joins the session, creates the recording and leaves it running.
async fn begin(
    state: &Arc<AppState>,
    slot: &Arc<Slot>,
    session: &LiveSession,
    connection: &ConnectionRecord,
    user: &User,
    address: &ClientAddress,
) -> Result<(String, i64), ApiError> {
    let mut info = session.client.clone();
    if !connection.flag(attributes::RECORD_MOLCAJETE_FORMATS) {
        recording::stock_formats_only(&mut info);
    }
    let parameters = BTreeMap::from([("read-only".to_owned(), "true".to_owned())]);
    let (host, port) = (session.guacd.0.as_str(), session.guacd.1);
    let guacd = guacd::connect(
        (host, port),
        &session.guacd_id,
        &parameters,
        &info,
        false,
        Duration::from_secs(state.config.guacd.handshake_timeout),
    )
    .await
    .map_err(|err| {
        warn!(session = %session.id, error = %err, "the recorder could not join the session");
        metrics::counter!("molcajete_gateway_recording_failures_total", "connection" => connection.id.clone(), "protocol" => connection.protocol.clone()).increment(1);
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "The recording could not start: the remote desktop daemon refused it.",
        )
    })?;
    let owner = RecordedUser {
        id: session.user_id.clone(),
        username: session.username.clone(),
    };
    let starter = RecordedUser {
        id: user.id.clone(),
        username: user.username.clone(),
    };
    let directory = state.config.recordings.directory(&state.config.server);
    let origin = Origin {
        session: session.id,
        user: &owner,
        started_by: Some(&starter),
    };
    let recorder = match Recorder::start(&state.db, &directory, connection, origin).await {
        Ok(recorder) => Arc::new(recorder),
        Err(err) => {
            error!(error = %err, dir = %directory.display(), "cannot start the recording");
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The recording could not be created.",
            ));
        }
    };
    let (id, started_at) = (recorder.id().to_owned(), recorder.started_at());
    let (stop, finished) = slot.begin(recorder.clone());
    audit::record(
        &state.db,
        Entry::new(action::RECORDING_START)
            .by(user)
            .from(address)
            .target(target::RECORDING, &id, &session.connection_name)
            .detail("session", session.id.to_string())
            .detail("connection", session.connection_id.as_str()),
    )
    .await;
    let span = info_span!("recorder", session = %session.id, recording = %id);
    let task = Finish {
        state: state.clone(),
        slot: slot.clone(),
        recorder,
        session: session.id.to_string(),
        connection_name: session.connection_name.clone(),
        finished,
    };
    tokio::spawn(task.run(guacd, stop).instrument(span));
    // The session may have ended meanwhile, with nothing left to stop this.
    if state.sessions.get(session.id).is_none() {
        slot.stop(Stop::SessionEnded);
    }
    Ok((id, started_at))
}

/// `DELETE /api/my-sessions/{page}/recording`: stops the recording started
/// from the menu once it is complete. 409 when *Record sessions* records
/// the session, which cannot be stopped; 404 when nothing is recorded.
pub async fn stop(
    State(state): State<Arc<AppState>>,
    CurrentUser(user): CurrentUser,
    address: ClientAddress,
    Path(page): Path<String>,
) -> Result<StatusCode, ApiError> {
    let session = own_session(&state, &user, &page)?;
    if session.recording_id.is_some() {
        return Err(conflict(
            "Every session of this connection is recorded; it cannot be stopped.",
        ));
    }
    let slot = state
        .sessions
        .recording_slot(session.id)
        .ok_or_else(ApiError::not_found)?;
    let reason = Stop::ByUser {
        user,
        address: address.0,
    };
    let Some(mut done) = slot.stop(reason) else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "The session is not being recorded.",
        ));
    };
    let _ = tokio::time::timeout(STOP_WAIT, done.wait_for(|done| *done)).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The recorder's task, from the join to the complete recording.
struct Finish {
    state: Arc<AppState>,
    slot: Arc<Slot>,
    recorder: Arc<Recorder>,
    session: String,
    connection_name: String,
    finished: watch::Sender<bool>,
}

impl Finish {
    async fn run(self, guacd: GuacdSession, stop: oneshot::Receiver<Stop>) {
        let reason = pump(guacd, &self.recorder, stop).await;
        let id = self.recorder.id();
        self.slot.clear(id);
        self.recorder.finish(&self.state.db).await;
        info!(reason = reason.as_str(), "stopped recording the session");
        let mut entry = Entry::new(action::RECORDING_STOP)
            .target(target::RECORDING, id, &self.connection_name)
            .detail("session", self.session.as_str())
            .detail("reason", reason.as_str());
        if let Stop::ByUser { user, address } = &reason {
            entry = entry.by(user).from(&ClientAddress(address.clone()));
        }
        audit::record(&self.state.db, entry).await;
        let _ = self.finished.send(true);
    }
}

/// Writes what the daemon sends into the recording, answering its frames
/// as a client must, until `stop` or the daemon closes the connection.
async fn pump(guacd: GuacdSession, recorder: &Recorder, mut stop: oneshot::Receiver<Stop>) -> Stop {
    let GuacdSession {
        stream, mut parser, ..
    } = guacd;
    let (mut rd, mut wr) = stream.into_split();
    let mut buf = vec![0u8; 64 * 1024];
    let mut replies = Vec::new();
    loop {
        loop {
            match parser.next_raw() {
                Ok(Some(raw)) => {
                    // A client that does not acknowledge frames is
                    // considered lagging, and in the end gone.
                    if raw.opcode() == "sync" {
                        let ack = InstructionBuilder::new("sync").str_arg(raw.arg(0)).finish();
                        replies.extend_from_slice(ack.as_bytes());
                    }
                    let now = now_millis();
                    recorder.record(|e| e.server_instruction(&raw, now));
                }
                Ok(None) => break,
                Err(err) => {
                    warn!(error = %err, "invalid data from guacd");
                    return Stop::Failed;
                }
            }
        }
        recorder.flush().await;
        if !replies.is_empty() {
            if wr.write_all(&replies).await.is_err() {
                return Stop::DaemonClosed;
            }
            replies.clear();
        }
        tokio::select! {
            read = rd.read(&mut buf) => match read {
                Ok(0) | Err(_) => return Stop::DaemonClosed,
                Ok(n) => parser.feed(&buf[..n]),
            },
            reason = &mut stop => {
                let _ = wr.write_all(b"10.disconnect;").await;
                return reason.unwrap_or(Stop::SessionEnded);
            }
        }
    }
}
