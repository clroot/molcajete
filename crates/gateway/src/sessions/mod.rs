//! Remote desktop sessions: the tunnels open right now and the history of
//! closed ones. (Not to be confused with login sessions, `molcajete_session`.)
//!
//! Every established tunnel registers itself in the [`Registry`] for as
//! long as it relays, through a [`Registration`] that removes it again when
//! dropped. The registry is what administrators list (`GET /api/sessions`)
//! and end (`DELETE /api/sessions/{id}`); [`policy`] ends the sessions a
//! user may no longer open after an administrative change; [`history`]
//! stores each session in `molcajete_connection_history` when it closes.
//!
//! # Looking up a live session
//!
//! Other features find a session by its tunnel id with [`Registry::get`] or
//! by the id guacd gave its connection with [`Registry::by_guacd_id`]. A
//! [`LiveSession`] carries what joining needs: the guacd it runs on
//! ([`LiveSession::guacd`]), guacd's connection id
//! ([`LiveSession::guacd_id`], selected with `select,<id>` to join) and
//! the owner ([`LiveSession::user_id`]). Ending a session
//! ([`Registry::end`]) closes the owner's tunnel only.

pub mod api;
pub mod history;
pub mod policy;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use metrics::counter;
use molcajete_protocol::Status;
use serde::Serialize;
use tokio::sync::watch;
use uuid::Uuid;

use crate::db::now_millis;

/// Why a session was ended by someone other than its user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// An administrator ended it.
    Admin,
    /// The user was disabled.
    UserDisabled,
    /// The user was deleted.
    UserDeleted,
    /// The user can no longer open the connection (a grant, a group
    /// membership, a role or a folder changed, or the connection was
    /// deleted).
    AccessRevoked,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::UserDisabled => "user_disabled",
            Self::UserDeleted => "user_deleted",
            Self::AccessRevoked => "access_revoked",
        }
    }

    /// The Guacamole status the tunnel closes with, which the page turns
    /// into its message: `SESSION_CLOSED` ("Ended by an administrator") or
    /// `CLIENT_FORBIDDEN` (the user may no longer use the connection).
    pub fn status(self) -> Status {
        match self {
            Self::Admin => Status::SESSION_CLOSED,
            Self::UserDisabled | Self::UserDeleted | Self::AccessRevoked => {
                Status::CLIENT_FORBIDDEN
            }
        }
    }
}

/// Why and by whom a session was ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ending {
    pub reason: EndReason,
    /// The administrator whose action ended it.
    pub by: Option<String>,
}

/// What is known about a session when its tunnel is established.
#[derive(Debug, Clone)]
pub struct SessionStart {
    pub user_id: String,
    pub username: String,
    pub connection_id: String,
    pub connection_name: String,
    pub protocol: String,
    /// The browser's address.
    pub peer: Option<String>,
    /// Where the session's guacd (or molcajeted) listens.
    pub guacd: (String, u16),
    /// guacd's id of the connection (`ready,<id>`).
    pub guacd_id: String,
    pub recording_id: Option<String>,
}

/// A live session, as listed to administrators.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LiveSession {
    /// The tunnel id, which the browser also receives first.
    #[serde(serialize_with = "as_string")]
    pub id: Uuid,
    pub user_id: String,
    pub username: String,
    pub connection_id: String,
    pub connection_name: String,
    pub protocol: String,
    pub peer: Option<String>,
    /// Unix milliseconds.
    pub started_at: i64,
    pub bytes_to_browser: u64,
    pub bytes_to_guacd: u64,
    /// The latest round trip the page reported (client reports only).
    pub rtt_ms: Option<u64>,
    pub recording_id: Option<String>,
    pub guacd_id: String,
    #[serde(skip)]
    pub guacd: (String, u16),
}

fn as_string<S: serde::Serializer>(id: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

/// Bytes relayed by one tunnel so far.
#[derive(Debug, Default)]
pub struct Traffic {
    pub to_browser: AtomicU64,
    pub to_guacd: AtomicU64,
}

const NO_RTT: u64 = u64::MAX;

struct Entry {
    start: SessionStart,
    started_at: i64,
    traffic: Arc<Traffic>,
    rtt_ms: AtomicU64,
    end: watch::Sender<Option<Ending>>,
}

impl Entry {
    fn snapshot(&self, id: Uuid) -> LiveSession {
        let s = &self.start;
        let rtt = self.rtt_ms.load(Ordering::Relaxed);
        LiveSession {
            id,
            user_id: s.user_id.clone(),
            username: s.username.clone(),
            connection_id: s.connection_id.clone(),
            connection_name: s.connection_name.clone(),
            protocol: s.protocol.clone(),
            peer: s.peer.clone(),
            started_at: self.started_at,
            bytes_to_browser: self.traffic.to_browser.load(Ordering::Relaxed),
            bytes_to_guacd: self.traffic.to_guacd.load(Ordering::Relaxed),
            rtt_ms: (rtt != NO_RTT).then_some(rtt),
            recording_id: s.recording_id.clone(),
            guacd_id: s.guacd_id.clone(),
            guacd: s.guacd.clone(),
        }
    }
}

/// The sessions open right now. Empty on startup: tunnels do not outlive
/// the gateway.
#[derive(Default)]
pub struct Registry(Mutex<HashMap<Uuid, Arc<Entry>>>);

impl Registry {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Arc<Entry>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Registers tunnel `id` until the returned registration is dropped.
    pub fn register(self: &Arc<Self>, id: Uuid, start: SessionStart) -> Registration {
        let (end, ended) = watch::channel(None);
        let entry = Arc::new(Entry {
            start,
            started_at: now_millis(),
            traffic: Arc::default(),
            rtt_ms: AtomicU64::new(NO_RTT),
            end,
        });
        self.map().insert(id, entry.clone());
        Registration {
            registry: self.clone(),
            id,
            entry,
            ended,
        }
    }

    /// Every live session, oldest first.
    pub fn list(&self) -> Vec<LiveSession> {
        let mut all: Vec<LiveSession> = self
            .map()
            .iter()
            .map(|(id, entry)| entry.snapshot(*id))
            .collect();
        all.sort_by_key(|s| (s.started_at, s.id));
        all
    }

    pub fn get(&self, id: Uuid) -> Option<LiveSession> {
        self.map().get(&id).map(|entry| entry.snapshot(id))
    }

    /// The session whose guacd connection is `guacd_id`.
    pub fn by_guacd_id(&self, guacd_id: &str) -> Option<LiveSession> {
        self.map()
            .iter()
            .find(|(_, entry)| entry.start.guacd_id == guacd_id)
            .map(|(id, entry)| entry.snapshot(*id))
    }

    /// Asks session `id` to end; its tunnel closes with
    /// [`EndReason::status`] right away. `false` when there is no such
    /// session or it is already ending.
    pub fn end(&self, id: Uuid, ending: Ending) -> bool {
        let Some(entry) = self.map().get(&id).cloned() else {
            return false;
        };
        let reason = ending.reason;
        let first = entry.end.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(ending);
            true
        });
        if first {
            counter!("molcajete_gateway_sessions_ended_total", "reason" => reason.as_str())
                .increment(1);
        }
        first
    }

    /// Records the round trip the page of tunnel `id` measured.
    pub fn set_rtt(&self, id: Uuid, rtt_ms: f64) {
        if let Some(entry) = self.map().get(&id)
            && rtt_ms.is_finite()
            && rtt_ms >= 0.0
        {
            entry
                .rtt_ms
                .store((rtt_ms.round() as u64).min(NO_RTT - 1), Ordering::Relaxed);
        }
    }
}

/// A tunnel's place in the [`Registry`]; leaving it removes the session.
pub struct Registration {
    registry: Arc<Registry>,
    id: Uuid,
    entry: Arc<Entry>,
    ended: watch::Receiver<Option<Ending>>,
}

impl Registration {
    /// What the tunnel counts its bytes into.
    pub fn traffic(&self) -> &Traffic {
        &self.entry.traffic
    }

    /// Resolves once the session is ended through [`Registry::end`].
    pub async fn ended(&self) -> Ending {
        let mut ended = self.ended.clone();
        let ending = match ended.wait_for(Option::is_some).await {
            Ok(ending) => ending.clone(),
            // The sender lives in the entry this registration holds.
            Err(_) => None,
        };
        match ending {
            Some(ending) => ending,
            None => std::future::pending().await,
        }
    }

    /// How the session was ended, if it was.
    pub fn ending(&self) -> Option<Ending> {
        self.ended.borrow().clone()
    }

    pub fn started_at(&self) -> i64 {
        self.entry.started_at
    }

    pub fn start(&self) -> &SessionStart {
        &self.entry.start
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.map().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(user: &str, guacd_id: &str) -> SessionStart {
        SessionStart {
            user_id: format!("id-{user}"),
            username: user.into(),
            connection_id: "desk".into(),
            connection_name: "Desk".into(),
            protocol: "vnc".into(),
            peer: Some("192.0.2.1".into()),
            guacd: ("127.0.0.1".into(), 4822),
            guacd_id: guacd_id.into(),
            recording_id: None,
        }
    }

    #[tokio::test]
    async fn registers_lists_and_ends_sessions() {
        let registry = Arc::new(Registry::default());
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let first = registry.register(a, start("bob", "$a"));
        let second = registry.register(b, start("carol", "$b"));
        assert_eq!(registry.list().len(), 2);
        assert_eq!(registry.by_guacd_id("$b").unwrap().id, b);
        assert_eq!(registry.get(a).unwrap().username, "bob");

        first.traffic().to_browser.fetch_add(10, Ordering::Relaxed);
        registry.set_rtt(a, 12.4);
        let listed = registry.get(a).unwrap();
        assert_eq!((listed.bytes_to_browser, listed.rtt_ms), (10, Some(12)));
        assert_eq!(registry.get(b).unwrap().rtt_ms, None);

        let ending = Ending {
            reason: EndReason::Admin,
            by: Some("root".into()),
        };
        assert!(registry.end(a, ending.clone()));
        assert!(!registry.end(a, ending.clone()), "ends once");
        assert_eq!(first.ended().await, ending);
        assert_eq!(first.ending(), Some(ending));
        assert_eq!(second.ending(), None);

        drop(first);
        assert!(registry.get(a).is_none());
        assert!(!registry.end(
            a,
            Ending {
                reason: EndReason::Admin,
                by: None
            }
        ));
        drop(second);
        assert!(registry.list().is_empty());
    }
}
