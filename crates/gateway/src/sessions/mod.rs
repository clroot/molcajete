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
//! the owner ([`LiveSession::user_id`]).
//!
//! # Joined sessions
//!
//! Someone who opens a share link ([`share`]) or an administrator who
//! watches a session joins the owner's guacd connection with a tunnel of
//! their own, which registers too, with [`SessionStart::joined`] saying
//! whose session it joined and how. Ending a session ([`Registry::end`])
//! ends everyone who joined it; so does the owner's tunnel closing, and
//! revoking a link ends those who joined through it.
//!
//! # Recording on demand
//!
//! An owner's session has a [`record::Slot`] for recordings started from
//! its session menu: [`record`] joins the session with a connection of its
//! own that writes the recording, and the owner's tunnel adds their mouse
//! and keys to it while one runs.

pub mod api;
pub mod history;
pub mod policy;
pub mod record;
pub mod share;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use metrics::counter;
use molcajete_protocol::Status;
use serde::Serialize;
use tokio::sync::watch;
use uuid::Uuid;

use crate::db::now_millis;
use crate::guacd::ClientInfo;

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
    /// The session this one joined has ended.
    OwnerLeft,
    /// The share link this session joined through was revoked.
    LinkRevoked,
}

impl EndReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::UserDisabled => "user_disabled",
            Self::UserDeleted => "user_deleted",
            Self::AccessRevoked => "access_revoked",
            Self::OwnerLeft => "owner_left",
            Self::LinkRevoked => "link_revoked",
        }
    }

    /// The Guacamole status the tunnel closes with, which the page turns
    /// into its message: `SESSION_CLOSED` ("Ended by an administrator"),
    /// `CLIENT_FORBIDDEN` (the user may no longer use the connection or
    /// the link) or `SUCCESS` (the shared session ended normally).
    pub fn status(self) -> Status {
        match self {
            Self::Admin => Status::SESSION_CLOSED,
            Self::UserDisabled | Self::UserDeleted | Self::AccessRevoked | Self::LinkRevoked => {
                Status::CLIENT_FORBIDDEN
            }
            Self::OwnerLeft => Status::SUCCESS,
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
    /// The id the page chose for its tunnel (`page=`), by which its user
    /// manages sharing ([`Registry::by_page`]).
    pub page_id: Option<String>,
    /// Set for a session that joined another one.
    pub joined: Option<Joined>,
    /// What the browser announced in the handshake, which a recorder
    /// joining the session announces too ([`record`]).
    pub client: ClientInfo,
}

/// How a session joined another (its owner's).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Joined {
    /// The tunnel id of the session joined.
    pub owner: Uuid,
    pub via: JoinVia,
    /// The user only watches: guacd drops their mouse and keys.
    pub read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinVia {
    /// A share link, by its id.
    Link(Uuid),
    /// An administrator watching (always read-only).
    Shadow,
}

impl JoinVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Link(_) => "link",
            Self::Shadow => "shadow",
        }
    }
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
    /// The recording of *Record sessions*, which lasts the whole session.
    pub recording_id: Option<String>,
    /// The session is being recorded right now, by *Record sessions* or
    /// from its session menu.
    pub recording: bool,
    /// Every recording of the session so far, oldest first.
    pub recording_ids: Vec<String>,
    pub guacd_id: String,
    #[serde(skip)]
    pub guacd: (String, u16),
    /// For a session that joined another: the tunnel id of that one.
    #[serde(serialize_with = "as_optional_string")]
    pub joined_to: Option<Uuid>,
    /// `link` or `shadow` for a session that joined another.
    pub join_kind: Option<&'static str>,
    /// A joined session that only watches.
    pub read_only: bool,
    #[serde(skip)]
    pub joined: Option<Joined>,
    #[serde(skip)]
    pub client: ClientInfo,
}

fn as_string<S: serde::Serializer>(id: &Uuid, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.collect_str(id)
}

fn as_optional_string<S: serde::Serializer>(
    id: &Option<Uuid>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match id {
        Some(id) => serializer.collect_str(id),
        None => serializer.serialize_none(),
    }
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
    recording: Arc<record::Slot>,
}

impl Entry {
    fn snapshot(&self, id: Uuid) -> LiveSession {
        let s = &self.start;
        let rtt = self.rtt_ms.load(Ordering::Relaxed);
        let on_demand = self.recording.recordings();
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
            recording: s.recording_id.is_some() || self.recording.current().is_some(),
            recording_ids: s.recording_id.iter().cloned().chain(on_demand).collect(),
            guacd_id: s.guacd_id.clone(),
            guacd: s.guacd.clone(),
            joined_to: s.joined.map(|j| j.owner),
            join_kind: s.joined.map(|j| j.via.as_str()),
            read_only: s.joined.is_some_and(|j| j.read_only),
            joined: s.joined,
            client: s.client.clone(),
        }
    }

    /// Ends this session once; `false` when it is already ending.
    fn end(&self, ending: Ending) -> bool {
        let reason = ending.reason;
        let first = self.end.send_if_modified(|current| {
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
}

/// A share link of a live session ([`share`]).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Link {
    #[serde(serialize_with = "as_string")]
    pub id: Uuid,
    /// The tunnel id of the session shared.
    #[serde(skip)]
    pub session: Uuid,
    /// SHA-256 of the token, hex; the token itself is only shown once.
    #[serde(skip)]
    token_hash: String,
    /// Who opens it only watches.
    pub read_only: bool,
    /// Unix milliseconds.
    pub created_at: i64,
    pub expires_at: i64,
}

impl Link {
    fn expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }
}

/// The sessions open right now and their share links. Empty on startup:
/// tunnels do not outlive the gateway, and links do not outlive their
/// session.
#[derive(Default)]
pub struct Registry {
    sessions: Mutex<HashMap<Uuid, Arc<Entry>>>,
    links: Mutex<HashMap<Uuid, Link>>,
}

impl Registry {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Arc<Entry>>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn links(&self) -> std::sync::MutexGuard<'_, HashMap<Uuid, Link>> {
        self.links.lock().unwrap_or_else(|e| e.into_inner())
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
            recording: Arc::default(),
        });
        let owner = entry.start.joined.map(|j| j.owner);
        self.map().insert(id, entry.clone());
        // The session joined may have closed meanwhile, in which case
        // nothing else would end this one.
        if let Some(owner) = owner
            && self.get(owner).is_none()
        {
            entry.end(Ending {
                reason: EndReason::OwnerLeft,
                by: None,
            });
        }
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

    /// The session whose guacd connection is `guacd_id` (its owner's,
    /// not one that joined it).
    pub fn by_guacd_id(&self, guacd_id: &str) -> Option<LiveSession> {
        self.map()
            .iter()
            .find(|(_, entry)| entry.start.guacd_id == guacd_id && entry.start.joined.is_none())
            .map(|(id, entry)| entry.snapshot(*id))
    }

    /// The session `user_id` opened from the page that called its tunnel
    /// `page_id` (not one they joined).
    pub fn by_page(&self, user_id: &str, page_id: &str) -> Option<LiveSession> {
        self.map()
            .iter()
            .find(|(_, entry)| {
                let s = &entry.start;
                s.user_id == user_id && s.page_id.as_deref() == Some(page_id) && s.joined.is_none()
            })
            .map(|(id, entry)| entry.snapshot(*id))
    }

    /// The sessions that joined session `owner`, oldest first.
    pub fn joined_to(&self, owner: Uuid) -> Vec<LiveSession> {
        let mut joined: Vec<LiveSession> = self
            .map()
            .iter()
            .filter(|(_, entry)| entry.start.joined.is_some_and(|j| j.owner == owner))
            .map(|(id, entry)| entry.snapshot(*id))
            .collect();
        joined.sort_by_key(|s| (s.started_at, s.id));
        joined
    }

    /// Asks session `id` to end, and every session that joined it; their
    /// tunnels close with [`EndReason::status`] right away (those that
    /// joined with [`EndReason::OwnerLeft`]'s). `false` when there is no
    /// such session or it is already ending.
    pub fn end(&self, id: Uuid, ending: Ending) -> bool {
        let Some(entry) = self.map().get(&id).cloned() else {
            return false;
        };
        let first = entry.end(ending);
        if first {
            self.end_joined(|joined| joined.owner == id, EndReason::OwnerLeft);
        }
        first
    }

    /// Ends the sessions that joined another one as `which` selects.
    fn end_joined(&self, which: impl Fn(&Joined) -> bool, reason: EndReason) {
        let joined: Vec<Arc<Entry>> = self
            .map()
            .values()
            .filter(|entry| entry.start.joined.as_ref().is_some_and(&which))
            .cloned()
            .collect();
        for entry in joined {
            entry.end(Ending { reason, by: None });
        }
    }

    /// Creates a share link of live session `session` that lasts `valid`;
    /// returns it with its token, which is not kept.
    pub fn create_link(&self, session: Uuid, read_only: bool, valid: Duration) -> (Link, String) {
        let token = crate::auth::new_token();
        let now = now_millis();
        let link = Link {
            id: Uuid::new_v4(),
            session,
            token_hash: crate::auth::token_hash(&token),
            read_only,
            created_at: now,
            expires_at: now.saturating_add(valid.as_millis().min(i64::MAX as u128) as i64),
        };
        self.links().insert(link.id, link.clone());
        (link, token)
    }

    /// The link `token` opens and its session, while both are live and the
    /// link has not expired.
    pub fn link_by_token(&self, token: &str) -> Option<(Link, LiveSession)> {
        let hash = crate::auth::token_hash(token);
        let now = now_millis();
        let link = {
            let mut links = self.links();
            links.retain(|_, link| !link.expired(now));
            links.values().find(|link| link.token_hash == hash)?.clone()
        };
        let session = self.get(link.session)?;
        Some((link, session))
    }

    /// The links of session `session` that have not expired, oldest first.
    pub fn links_of(&self, session: Uuid) -> Vec<Link> {
        let now = now_millis();
        let mut links: Vec<Link> = self
            .links()
            .values()
            .filter(|link| link.session == session && !link.expired(now))
            .cloned()
            .collect();
        links.sort_by_key(|link| (link.created_at, link.id));
        links
    }

    /// Revokes link `id` of session `session` and ends the sessions that
    /// joined through it. `false` when there is no such link.
    pub fn revoke_link(&self, session: Uuid, id: Uuid) -> bool {
        {
            let mut links = self.links();
            if links.get(&id).is_none_or(|link| link.session != session) {
                return false;
            }
            links.remove(&id);
        }
        self.end_joined(
            |joined| joined.via == JoinVia::Link(id),
            EndReason::LinkRevoked,
        );
        true
    }

    /// Where recordings of session `id` started from its menu are kept.
    pub fn recording_slot(&self, id: Uuid) -> Option<Arc<record::Slot>> {
        self.map().get(&id).map(|entry| entry.recording.clone())
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

    /// The recording started from the session menu, if one runs: the
    /// owner's tunnel adds their input to it.
    pub fn recording(&self) -> &record::Slot {
        &self.entry.recording
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        self.registry.map().remove(&self.id);
        if self.entry.start.joined.is_none() {
            self.entry.recording.stop(record::Stop::SessionEnded);
            // Whoever joined leaves with the owner (stock guacd would keep
            // them), and nobody can join any more.
            let id = self.id;
            self.registry
                .end_joined(|joined| joined.owner == id, EndReason::OwnerLeft);
            self.registry.links().retain(|_, link| link.session != id);
        }
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
            page_id: None,
            joined: None,
            client: ClientInfo::default(),
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

    fn joined(owner: Uuid, via: JoinVia) -> SessionStart {
        SessionStart {
            joined: Some(Joined {
                owner,
                via,
                read_only: true,
            }),
            ..start("carol", "$a")
        }
    }

    #[tokio::test]
    async fn joined_sessions_end_with_their_owner_or_link() {
        let registry = Arc::new(Registry::default());
        let owner = Uuid::new_v4();
        let owners = registry.register(
            owner,
            SessionStart {
                page_id: Some("tab".into()),
                ..start("bob", "$a")
            },
        );
        let (link, token) = registry.create_link(owner, true, Duration::from_secs(60));
        let (found, session) = registry.link_by_token(&token).unwrap();
        assert_eq!((found.id, session.id), (link.id, owner));
        assert!(registry.link_by_token("other").is_none());

        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let through_link = registry.register(a, joined(owner, JoinVia::Link(link.id)));
        let watching = registry.register(b, joined(owner, JoinVia::Shadow));
        // The owner's, not those that joined the same guacd connection.
        assert_eq!(registry.by_guacd_id("$a").unwrap().id, owner);
        assert_eq!(registry.by_page("id-bob", "tab").unwrap().id, owner);
        assert!(registry.by_page("id-carol", "tab").is_none());
        let joined_now: Vec<Uuid> = registry.joined_to(owner).iter().map(|s| s.id).collect();
        assert_eq!(joined_now.len(), 2);
        assert!(joined_now.contains(&a) && joined_now.contains(&b));
        assert_eq!(registry.get(b).unwrap().join_kind, Some("shadow"));

        // Revoking the link ends those who came through it.
        assert!(!registry.revoke_link(Uuid::new_v4(), link.id));
        assert!(registry.revoke_link(owner, link.id));
        assert_eq!(
            through_link.ending().unwrap().reason,
            EndReason::LinkRevoked
        );
        assert_eq!(watching.ending(), None);
        assert!(registry.link_by_token(&token).is_none());

        // The owner leaving ends the rest, and its links.
        let (_, token) = registry.create_link(owner, false, Duration::from_secs(60));
        drop(owners);
        assert_eq!(watching.ending().unwrap().reason, EndReason::OwnerLeft);
        assert!(registry.link_by_token(&token).is_none());
        // Joining a session that has just closed ends at once.
        let late = registry.register(Uuid::new_v4(), joined(owner, JoinVia::Shadow));
        assert_eq!(late.ending().unwrap().reason, EndReason::OwnerLeft);

        // Expired links are not found.
        let owner = Uuid::new_v4();
        let _owners = registry.register(owner, start("bob", "$b"));
        let (_, token) = registry.create_link(owner, true, Duration::ZERO);
        assert!(registry.link_by_token(&token).is_none());
        assert!(registry.links_of(owner).is_empty());
    }
}
