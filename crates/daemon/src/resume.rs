//! Sessions that outlive the connection of their client
//! (`molcajete_protocol::resume`).
//!
//! A session whose handshake carried a resume key is registered here under
//! that key. Its client's reader no longer ends the session when the
//! socket closes: once the session is up (see [`establish`]), the session
//! loop is told [`ClientEvent::Detached`](crate::session::ClientEvent::Detached), keeps the remote side going
//! without a client, and ends when the grace period passes. A handshake
//! with the same key and protocol meanwhile attaches to it instead of
//! starting a session: the session loop gets
//! [`ClientEvent::Attached`](crate::session::ClientEvent::Attached) with
//! the new client's socket and what it announced, and from there sends that
//! client the whole screen.
//!
//! A key that matches a session whose client is still attached takes the
//! session over: the old client is told [`Status::SESSION_CONFLICT`] and
//! closed (its connection may be half dead after a change of network).
//!
//! Until the session is established (while it connects to the remote host
//! or asks for credentials) a client that leaves ends it, as before.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock, Weak};
use std::time::{Duration, Instant};

use molcajete_protocol::resume::{HANDSHAKE_ARG, TIMEOUT_ARG};
use molcajete_protocol::{Instruction, InstructionBuilder, Status};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use tracing::{Instrument as _, info, warn};

use crate::handshake::{ClientInfo, Params};
use crate::io::{GuacReader, GuacWriter};
use crate::session::Input;

/// Longest key accepted (a gateway sends 64 hex digits).
const MAX_KEY_LEN: usize = 128;

/// Longest grace period a connection may ask for.
const MAX_GRACE: Duration = Duration::from_secs(24 * 3600);

/// How long the old client of a session taken over gets to read why it
/// was closed.
const TAKEOVER_WRITE: Duration = Duration::from_secs(1);

/// The grace periods and the limit, set once at startup.
#[derive(Debug, Clone, Copy)]
pub struct Config {
    /// RDP and VNC.
    pub desktop: Duration,
    pub ssh: Duration,
    /// Sessions held without a client at once; one more ends at once.
    pub max_detached: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            desktop: Duration::from_secs(120),
            ssh: Duration::from_secs(300),
            max_detached: 16,
        }
    }
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Sets the grace periods (call once at startup; the defaults apply
/// otherwise).
pub fn configure(config: Config) {
    let _ = CONFIG.set(config);
}

fn config() -> Config {
    CONFIG.get().copied().unwrap_or_default()
}

/// The key and grace period of a session whose handshake asked for it, or
/// `None` when it ends with its connection: no key (stock clients), a key
/// that is too long, or a grace period of zero.
pub fn wanted(protocol: &str, params: &Params) -> Option<(String, Duration)> {
    let key = params.get(HANDSHAKE_ARG)?;
    if key.len() > MAX_KEY_LEN {
        warn!(
            len = key.len(),
            "resume key too long; the session ends with its connection"
        );
        return None;
    }
    let grace = match params.parse::<u64>(TIMEOUT_ARG) {
        Some(seconds) => Duration::from_secs(seconds).min(MAX_GRACE),
        None => match protocol {
            "ssh" => config().ssh,
            _ => config().desktop,
        },
    };
    (!grace.is_zero()).then(|| (key.to_owned(), grace))
}

/// What a client brings to a held session: its socket and what it
/// announced in the handshake. Handed to the session loop as
/// [`ClientEvent::Attached`](crate::session::ClientEvent::Attached).
#[derive(Clone)]
pub struct Attach(Arc<Attachment>);

struct Attachment {
    info: ClientInfo,
    socket: Mutex<Option<OwnedWriteHalf>>,
    /// The previous client was still attached.
    took_over: bool,
}

impl Attach {
    pub fn info(&self) -> &ClientInfo {
        &self.0.info
    }

    fn take_socket(&self) -> Option<OwnedWriteHalf> {
        self.0
            .socket
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }
}

impl PartialEq for Attach {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Attach {}

impl fmt::Debug for Attach {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Attach")
            .field("width", &self.0.info.width)
            .field("height", &self.0.info.height)
            .finish_non_exhaustive()
    }
}

/// What the session loop learns from the client's side, besides its
/// instructions.
pub enum Control {
    /// The client's connection ended.
    Detached,
    /// A client took the session (back).
    Attached(Attach),
}

#[derive(Default)]
struct State {
    /// The session's input channel.
    input: Option<mpsc::Sender<Input>>,
    /// Counts clients; a reader only speaks for the latest.
    generation: u64,
    reader: Option<AbortHandle>,
    /// The session loop runs (see [`establish`]).
    established: bool,
    /// No client is attached.
    detached: bool,
    /// Ends the session when the grace period passes.
    expiry: Option<AbortHandle>,
    closed: bool,
}

/// A session that may be held without a client.
pub(crate) struct Hub {
    /// The connection id (`ready`).
    id: String,
    protocol: &'static str,
    key: String,
    grace: Duration,
    state: Mutex<State>,
    /// Keeps the log lines of the readers and the expiry in the
    /// connection's span.
    span: tracing::Span,
}

type Map = HashMap<(&'static str, String), Weak<Hub>>;

static HELD: LazyLock<Mutex<Map>> = LazyLock::new(Mutex::default);

/// Sessions without a client right now, against [`Config::max_detached`].
static DETACHED: AtomicUsize = AtomicUsize::new(0);

fn held() -> MutexGuard<'static, Map> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

tokio::task_local! {
    static CURRENT: Arc<Hub>;
}

/// The session on this task, when it may be held.
pub(crate) fn current() -> Option<Arc<Hub>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// The session loop runs: from here on a client that leaves detaches
/// instead of ending the session, and `writer` drops what it cannot write.
/// Does nothing for a session without a key.
pub fn establish(writer: &mut GuacWriter) {
    let Some(hub) = current() else {
        return;
    };
    writer.set_detachable();
    hub.lock().established = true;
}

/// A held session with this key, which a new client may attach to.
pub fn find(protocol: &str, key: &str) -> Option<Arc<Hub>> {
    let hub = held()
        .iter()
        .find(|((p, k), _)| *p == protocol && k == key)
        .and_then(|(_, hub)| hub.upgrade())?;
    let attachable = {
        let state = hub.lock();
        state.established && !state.closed
    };
    attachable.then_some(hub)
}

/// The owner's hold on a session that may be resumed; it is forgotten when
/// this is dropped (the session ended).
pub(crate) struct Held(Arc<Hub>);

impl Held {
    /// Registers session `id` under `key`. A session already registered
    /// under it (not established yet, or lost) is no longer found.
    pub fn register(id: &str, protocol: &'static str, key: String, grace: Duration) -> Self {
        let hub = Arc::new(Hub {
            id: id.to_owned(),
            protocol,
            key: key.clone(),
            grace,
            state: Mutex::default(),
            span: tracing::Span::current(),
        });
        held().insert((protocol, key), Arc::downgrade(&hub));
        Self(hub)
    }

    /// Runs the session, during which [`current`] returns it.
    pub async fn run<F: Future>(&self, session: F) -> F::Output {
        CURRENT.scope(self.0.clone(), session).await
    }
}

impl Drop for Held {
    fn drop(&mut self) {
        let hub = &self.0;
        {
            let mut map = held();
            let entry = (hub.protocol, hub.key.clone());
            if map
                .get(&entry)
                .is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(hub)))
            {
                map.remove(&entry);
            }
        }
        let mut state = hub.lock();
        state.closed = true;
        state.input = None;
        if let Some(reader) = state.reader.take() {
            reader.abort();
        }
        if let Some(expiry) = state.expiry.take() {
            expiry.abort();
        }
        if std::mem::take(&mut state.detached) {
            hub.counted_detached(false);
        }
    }
}

fn input(instruction: Instruction, control: Option<Control>) -> Input {
    Input {
        instruction,
        received: Instant::now(),
        from_viewer: false,
        control,
    }
}

/// The instruction that ends a session the way a client's `disconnect`
/// does.
fn disconnect() -> Input {
    input(Instruction::new("disconnect", [""; 0]), None)
}

impl Hub {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    fn counted_detached(&self, detached: bool) {
        let gauge = metrics::gauge!("molcajeted_sessions_detached", "protocol" => self.protocol);
        if detached {
            DETACHED.fetch_add(1, Ordering::Relaxed);
            gauge.increment(1.0);
        } else {
            DETACHED.fetch_sub(1, Ordering::Relaxed);
            gauge.decrement(1.0);
        }
    }

    /// Reads the session's first client (`session::spawn_reader`).
    pub fn first_reader(
        self: &Arc<Self>,
        reader: GuacReader,
        input: mpsc::Sender<Input>,
    ) -> JoinHandle<()> {
        let mut state = self.lock();
        state.input = Some(input.clone());
        let task = self.spawn_reader(reader, input, state.generation);
        state.reader = Some(task.abort_handle());
        task
    }

    fn spawn_reader(
        self: &Arc<Self>,
        mut reader: GuacReader,
        tx: mpsc::Sender<Input>,
        generation: u64,
    ) -> JoinHandle<()> {
        let hub = Arc::downgrade(self);
        tokio::spawn(
            async move {
                while let Ok(Some(instruction)) = reader.next().await {
                    if tx.send(input(instruction, None)).await.is_err() {
                        return;
                    }
                }
                if let Some(hub) = hub.upgrade() {
                    hub.reader_ended(generation, &tx).await;
                }
            }
            .instrument(self.span.clone()),
        )
    }

    /// The connection of client `generation` ended.
    async fn reader_ended(self: &Arc<Self>, generation: u64, tx: &mpsc::Sender<Input>) {
        let message = {
            let mut state = self.lock();
            if state.generation != generation || state.closed {
                return;
            }
            state.reader = None;
            if !state.established {
                // Still connecting: the client gave up on it.
                Some(disconnect())
            } else {
                state.detached = true;
                self.counted_detached(true);
                let over = DETACHED.load(Ordering::Relaxed) > config().max_detached;
                if over {
                    warn!(
                        limit = config().max_detached,
                        "too many sessions held without a client; ending this one"
                    );
                    metrics::counter!("molcajeted_sessions_expired_total", "protocol" => self.protocol)
                        .increment(1);
                } else {
                    info!(
                        grace_s = self.grace.as_secs(),
                        "client detached; holding the session"
                    );
                    let hub = Arc::downgrade(self);
                    let grace = self.grace;
                    let expiry = tokio::spawn(
                        async move {
                            tokio::time::sleep(grace).await;
                            if let Some(hub) = hub.upgrade() {
                                hub.expire(generation).await;
                            }
                        }
                        .instrument(self.span.clone()),
                    );
                    state.expiry = Some(expiry.abort_handle());
                }
                // The session loop hears of it either way; over the limit
                // the disconnect right behind it ends the session.
                let _ = tx.try_send(input(
                    Instruction::new("", [""; 0]),
                    Some(Control::Detached),
                ));
                over.then(disconnect)
            }
        };
        if let Some(message) = message {
            let _ = tx.send(message).await;
        }
    }

    /// The grace period of client `generation`'s absence passed.
    async fn expire(&self, generation: u64) {
        let tx = {
            let state = self.lock();
            if state.generation != generation || !state.detached || state.closed {
                return;
            }
            state.input.clone()
        };
        info!(
            grace_s = self.grace.as_secs(),
            "no client came back; ending the held session"
        );
        metrics::counter!("molcajeted_sessions_expired_total", "protocol" => self.protocol)
            .increment(1);
        if let Some(tx) = tx {
            let _ = tx.send(disconnect()).await;
        }
    }

    /// Gives the session to a new client, which has been answered `ready`.
    /// `false` if the session ended meanwhile.
    pub async fn attach(
        self: &Arc<Self>,
        reader: GuacReader,
        socket: OwnedWriteHalf,
        info: ClientInfo,
    ) -> bool {
        let (tx, generation, took_over) = {
            let mut state = self.lock();
            let Some(tx) = state.input.clone().filter(|_| !state.closed) else {
                return false;
            };
            state.generation += 1;
            if let Some(reader) = state.reader.take() {
                reader.abort();
            }
            if let Some(expiry) = state.expiry.take() {
                expiry.abort();
            }
            let took_over = !std::mem::take(&mut state.detached);
            if !took_over {
                self.counted_detached(false);
            }
            (tx, state.generation, took_over)
        };
        let from = if took_over { "attached" } else { "detached" };
        self.span.in_scope(|| {
            info!(
                from,
                width = info.width,
                height = info.height,
                "client attached to the held session"
            );
        });
        metrics::counter!(
            "molcajeted_sessions_resumed_total",
            "protocol" => self.protocol,
            "from" => from,
        )
        .increment(1);
        let attach = Attach(Arc::new(Attachment {
            info,
            socket: Mutex::new(Some(socket)),
            took_over,
        }));
        if tx
            .send(input(
                Instruction::new("", [""; 0]),
                Some(Control::Attached(attach)),
            ))
            .await
            .is_err()
        {
            return false;
        }
        let mut state = self.lock();
        if state.generation != generation || state.closed {
            // Another client came, or the session ended, meanwhile.
            return !state.closed;
        }
        let task = self.spawn_reader(reader, tx, generation);
        state.reader = Some(task.abort_handle());
        true
    }
}

/// Writes to the client of `attach` from now on (the session loop's part
/// of [`ClientEvent::Attached`](crate::session::ClientEvent::Attached)). A client still attached is told that the
/// session was taken over and closed. `false` when the socket was taken
/// already (never, for an event read once).
pub async fn take_over(writer: &mut GuacWriter, attach: &Attach) -> bool {
    let Some(socket) = attach.take_socket() else {
        return false;
    };
    // A client that detached left a socket that is closed or about to be.
    if attach.0.took_over && writer.is_attached() {
        writer.buffer().clear();
        writer.push(
            &InstructionBuilder::new("error")
                .str_arg("The session was opened in another window")
                .arg(Status::SESSION_CONFLICT.code())
                .finish(),
        );
        let _ = tokio::time::timeout(TAKEOVER_WRITE, writer.flush()).await;
        info!("the previous client was closed");
    }
    drop(writer.detach());
    writer.attach(socket, attach.info().binary);
    true
}

#[cfg(test)]
mod tests;
