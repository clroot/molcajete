//! Joining a running connection (`select,$<id>`), as guacd does.
//!
//! The session that opened a connection (the owner) runs as before; every
//! user who joins gets a task of its own with its own [`Display`]: its own
//! copy of what its client shows, its own frame pacing and window of
//! unacknowledged frames, and its own video encoder. A user on a slow link
//! therefore never holds up the owner or anyone else.
//!
//! What connects them is a [`Shared`] per connection:
//!
//! - **Screen.** While anyone has joined, the owner's display copies the
//!   tiles touched since its last frame into the shared screen and marks
//!   them for every joined user ([`Shared::publish`]), those it sends its
//!   own client as video included; each user's task copies the tiles
//!   marked for it into its own framebuffer and sends them when its own
//!   pacing allows. Marks are a bit per tile, so a
//!   user who falls behind costs no memory: it simply gets the newest
//!   pixels of more tiles in its next frame. A user who joins asks for the
//!   whole screen; the owner's session is woken with an empty `sync` (which
//!   every protocol hands to [`Display::ack`]) and publishes it.
//! - **Input.** Mouse, keys and clipboard of users who are not read-only go
//!   into the owner's input channel ([`Shared::forward`]), so the protocol
//!   merges them with the owner's own. Size changes, `argv` and everything
//!   else stay with the user: the owner's display decides the size of the
//!   remote desktop, and each user's `video-mode` applies to their own
//!   display.
//!
//! Joined users get no audio and no clipboard from the remote side, and
//! their frames follow the owner's: the shared screen is updated when the
//! owner's display builds a frame.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, Weak};
use std::time::Duration;

use base64::Engine as _;
use molcajete_protocol::{Instruction, Status};
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;
use tracing::{Instrument as _, debug, info, info_span, warn};

use crate::display::{
    ChannelOrder, Display, ImageFormats, Pacing, Rect, SourceImage, VIDEO_MODE, VideoMode,
};
use crate::handshake::{self, ClientInfo};
use crate::io::{GuacReader, GuacWriter, Output};
use crate::send_error;
use crate::session::{self, ClientEvent, Input, InputDecoder};

/// Tile size of the marks (the display's tile size).
const TILE: u32 = 64;

/// Frames a joined user may have unacknowledged before the next waits, as
/// for the owner of a desktop session.
const MAX_UNACKED_FRAMES: u32 = 2;

const KEEPALIVE: Duration = Duration::from_secs(5);

/// Stream ids from which the clipboard of joined users is passed to the
/// owner's session, one per user, below any a client opens itself.
const FIRST_FORWARD_STREAM: i32 = -1000;

/// How the owner's display is set up, which a joined user's follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Setup {
    pub pacing: Pacing,
    pub lossless: bool,
    /// Busy areas may become video (the owner's session allows it).
    pub video: bool,
}

/// The owner's framebuffer, as [`Shared::publish`] reads it.
pub(crate) struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    pub pixels: &'a [u8],
}

/// Cursor as the display keeps it: hotspot, size, RGBA pixels.
type Cursor = (u32, u32, u32, u32, Vec<u8>);

#[derive(Default)]
struct Screen {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
    cursor: Option<Cursor>,
    /// Counts cursor changes, so each user sends each one once.
    cursor_version: u64,
    setup: Option<Setup>,
}

impl Screen {
    fn tiles(&self) -> (u32, u32) {
        (self.width.div_ceil(TILE), self.height.div_ceil(TILE))
    }
}

/// What a joined user has yet to take from the screen.
struct Slot {
    /// Tiles changed since the user last looked.
    dirty: Vec<bool>,
    changed: bool,
    /// The user waits for the whole screen.
    waiting: bool,
    cursor_version: u64,
    wake: Arc<Notify>,
}

impl Slot {
    fn mark_all(&mut self, tiles: usize) {
        self.dirty = vec![true; tiles];
        self.changed = true;
        self.waiting = false;
    }
}

#[derive(Default)]
struct State {
    screen: Screen,
    slots: HashMap<u64, Slot>,
    next_slot: u64,
    /// The owner session's input channel.
    input: Option<mpsc::Sender<Input>>,
    closed: bool,
}

/// A connection others may join.
pub(crate) struct Shared {
    id: String,
    protocol: &'static str,
    /// The parameter names of the owner's `args`, which a joining user is
    /// offered too.
    args: Vec<&'static str>,
    /// Joined users right now (read without the lock on every frame).
    viewers: AtomicUsize,
    /// A user who joined waits for the whole screen.
    snapshot: AtomicBool,
    state: Mutex<State>,
}

static CONNECTIONS: LazyLock<Mutex<HashMap<String, Weak<Shared>>>> = LazyLock::new(Mutex::default);

fn connections() -> MutexGuard<'static, HashMap<String, Weak<Shared>>> {
    CONNECTIONS.lock().unwrap_or_else(|e| e.into_inner())
}

tokio::task_local! {
    static CURRENT: Arc<Shared>;
}

/// The connection whose session runs on this task, if any. Its display
/// ([`Display::with_protocol`]) and its input reader
/// ([`session::spawn_reader`]) attach themselves to it.
pub(crate) fn current() -> Option<Arc<Shared>> {
    CURRENT.try_with(Arc::clone).ok()
}

/// The owner's hold on a connection: it can be joined while this lives,
/// and everyone who joined is disconnected when it is dropped.
pub(crate) struct Owner(Arc<Shared>);

impl Owner {
    /// Makes connection `id` joinable.
    pub fn register(id: &str, protocol: &'static str, args: &[&'static str]) -> Self {
        let shared = Arc::new(Shared {
            id: id.to_owned(),
            protocol,
            args: args.to_vec(),
            viewers: AtomicUsize::new(0),
            snapshot: AtomicBool::new(false),
            state: Mutex::default(),
        });
        connections().insert(id.to_owned(), Arc::downgrade(&shared));
        Self(shared)
    }

    /// Runs the owner's session, during which [`current`] returns this
    /// connection.
    pub async fn run<F: Future>(&self, session: F) -> F::Output {
        CURRENT.scope(self.0.clone(), session).await
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        connections().remove(&self.0.id);
        let mut state = self.0.lock();
        state.closed = true;
        state.input = None;
        for slot in state.slots.values() {
            slot.wake.notify_one();
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether anyone has joined (cheap; checked for every frame).
    pub fn has_viewers(&self) -> bool {
        self.viewers.load(Ordering::Relaxed) > 0
    }

    /// Whether a user who joined waits for the whole screen.
    pub fn wants_snapshot(&self) -> bool {
        self.snapshot.load(Ordering::Relaxed)
    }

    /// Where the owner's session reads its input.
    pub fn set_input(&self, input: mpsc::Sender<Input>) {
        self.lock().input = Some(input);
    }

    /// Takes the owner's framebuffer: the pixels of `regions` (the tiles
    /// touched since the owner's last frame), or all of them
    /// when someone waits for the whole screen or the size changed.
    pub fn publish(
        &self,
        setup: Setup,
        frame: Frame<'_>,
        regions: &[Rect],
        cursor: Option<&Cursor>,
    ) {
        let mut state = self.lock();
        let state = &mut *state;
        let screen = &mut state.screen;
        screen.setup = Some(setup);
        let resized = (screen.width, screen.height) != (frame.width, frame.height)
            || screen.pixels.len() != frame.pixels.len();
        let whole = self.snapshot.swap(false, Ordering::Relaxed) || resized;
        if whole {
            screen.width = frame.width;
            screen.height = frame.height;
            screen.pixels.clear();
            screen.pixels.extend_from_slice(frame.pixels);
            if screen.cursor.as_ref() != cursor {
                screen.cursor = cursor.cloned();
                screen.cursor_version += 1;
            }
        } else {
            let stride = frame.width as usize * 4;
            for rect in regions {
                let rect = rect.clip(frame.width, frame.height);
                for y in rect.y..rect.y + rect.h {
                    let start = y as usize * stride + rect.x as usize * 4;
                    let end = start + rect.w as usize * 4;
                    screen.pixels[start..end].copy_from_slice(&frame.pixels[start..end]);
                }
            }
        }
        let (cols, rows) = screen.tiles();
        let tiles = (cols * rows) as usize;
        for slot in state.slots.values_mut() {
            if resized || slot.waiting || slot.dirty.len() != tiles {
                if whole {
                    slot.mark_all(tiles);
                }
            } else {
                for rect in regions {
                    let rect = rect.clip(frame.width, frame.height);
                    for ty in rect.y / TILE..(rect.y + rect.h).div_ceil(TILE).min(rows) {
                        for tx in rect.x / TILE..(rect.x + rect.w).div_ceil(TILE).min(cols) {
                            slot.dirty[(ty * cols + tx) as usize] = true;
                            slot.changed = true;
                        }
                    }
                }
            }
            if slot.changed {
                slot.wake.notify_one();
            }
        }
    }

    /// The owner's display changed the cursor.
    pub fn set_cursor(&self, cursor: Option<&Cursor>) {
        if !self.has_viewers() {
            return;
        }
        let mut state = self.lock();
        if state.screen.cursor.as_ref() == cursor {
            return;
        }
        state.screen.cursor = cursor.cloned();
        state.screen.cursor_version += 1;
        for slot in state.slots.values() {
            slot.wake.notify_one();
        }
    }

    /// Adds a joined user. It waits for the whole screen, unless the
    /// shared screen is already kept up to date for others.
    fn join(self: &Arc<Self>) -> Viewer {
        let wake = Arc::new(Notify::new());
        let mut state = self.lock();
        let id = state.next_slot;
        state.next_slot += 1;
        let mut slot = Slot {
            dirty: Vec::new(),
            changed: false,
            waiting: true,
            cursor_version: 0,
            wake: wake.clone(),
        };
        let current = self.has_viewers() && state.screen.width > 0;
        if current {
            let (cols, rows) = state.screen.tiles();
            slot.mark_all((cols * rows) as usize);
        } else {
            self.snapshot.store(true, Ordering::Relaxed);
        }
        state.slots.insert(id, slot);
        self.viewers.fetch_add(1, Ordering::Relaxed);
        let input = state.input.clone();
        drop(state);
        if !current && let Some(input) = input {
            // Wakes the owner's session, whose display then publishes the
            // screen (`Display::ack` ignores a timestamp it never sent).
            let _ = input.try_send(Input {
                instruction: Instruction::new("sync", ["0"]),
                received: std::time::Instant::now(),
                from_viewer: true,
            });
        }
        Viewer {
            shared: self.clone(),
            slot: id,
            wake,
        }
    }

    fn leave(&self, slot: u64) {
        let mut state = self.lock();
        state.slots.remove(&slot);
        if self.viewers.fetch_sub(1, Ordering::Relaxed) == 1 {
            // Nobody to keep it for: the next user starts from a snapshot.
            state.screen = Screen::default();
            self.snapshot.store(false, Ordering::Relaxed);
        }
    }
}

/// What [`Viewer::pull`] found.
#[derive(Debug, PartialEq, Eq)]
enum Pulled {
    Nothing,
    Changed,
    /// The owner's session has ended.
    Closed,
}

/// A joined user's place in a [`Shared`] connection.
struct Viewer {
    shared: Arc<Shared>,
    slot: u64,
    wake: Arc<Notify>,
}

impl Drop for Viewer {
    fn drop(&mut self) {
        self.shared.leave(self.slot);
    }
}

impl Viewer {
    /// How the owner's display is set up, once it has published a frame.
    fn setup(&self) -> Option<Setup> {
        let state = self.shared.lock();
        if state.closed {
            return None;
        }
        state.screen.setup.filter(|_| state.screen.width > 0)
    }

    fn closed(&self) -> bool {
        self.shared.lock().closed
    }

    /// Copies what changed for this user into its display. Instructions
    /// that cannot wait for the frame (a new size) go to `out`.
    fn pull(&self, display: &mut Display, out: &mut Output) -> Pulled {
        let mut state = self.shared.lock();
        let state = &mut *state;
        if state.closed {
            return Pulled::Closed;
        }
        let screen = &state.screen;
        let Some(slot) = state.slots.get_mut(&self.slot) else {
            return Pulled::Closed;
        };
        if slot.waiting || screen.width == 0 {
            return Pulled::Nothing;
        }
        let mut pulled = Pulled::Nothing;
        if slot.cursor_version != screen.cursor_version {
            slot.cursor_version = screen.cursor_version;
            if let Some((hx, hy, w, h, rgba)) = &screen.cursor {
                display.set_cursor(*hx, *hy, *w, *h, rgba, out);
                pulled = Pulled::Changed;
            }
        }
        if !slot.changed {
            return pulled;
        }
        slot.changed = false;
        if (display.width(), display.height()) != (screen.width, screen.height) {
            display.resize(screen.width, screen.height, out);
        }
        let (cols, rows) = screen.tiles();
        let stride = screen.width as usize * 4;
        for ty in 0..rows {
            let mut tx = 0;
            while tx < cols {
                if !std::mem::take(&mut slot.dirty[(ty * cols + tx) as usize]) {
                    tx += 1;
                    continue;
                }
                let first = tx;
                tx += 1;
                while tx < cols && std::mem::take(&mut slot.dirty[(ty * cols + tx) as usize]) {
                    tx += 1;
                }
                let rect = Rect::new(first * TILE, ty * TILE, (tx - first) * TILE, TILE)
                    .clip(screen.width, screen.height);
                let source =
                    SourceImage::at(&screen.pixels, stride, ChannelOrder::RGBX, rect.x, rect.y);
                if let Some(source) = source {
                    display.put_image(rect.x, rect.y, rect.w, rect.h, &source);
                    pulled = Pulled::Changed;
                }
            }
        }
        pulled
    }

    /// Passes an input instruction to the owner's session.
    async fn forward(&self, instruction: Instruction) {
        let input = self.shared.lock().input.clone();
        if let Some(input) = input {
            let _ = input
                .send(Input {
                    instruction,
                    received: std::time::Instant::now(),
                    from_viewer: true,
                })
                .await;
        }
    }

    /// Passes clipboard text to the owner's session, as a stream of this
    /// user's own.
    async fn forward_clipboard(&self, text: &str) {
        let stream = (FIRST_FORWARD_STREAM - (self.slot % 1_000_000) as i32).to_string();
        let data = base64::engine::general_purpose::STANDARD.encode(text);
        for instruction in [
            Instruction::new("clipboard", [stream.as_str(), "text/plain"]),
            Instruction::new("blob", [stream.as_str(), data.as_str()]),
            Instruction::new("end", [stream.as_str()]),
        ] {
            self.forward(instruction).await;
        }
    }
}

/// Serves a user joining connection `id` (`select,$<id>`), from the
/// handshake until either side leaves.
pub async fn join(mut reader: GuacReader, mut writer: GuacWriter, id: &str) {
    let shared = connections().get(id).and_then(Weak::upgrade);
    let Some(shared) = shared else {
        warn!(connection = id, "join of an unknown connection refused");
        send_error(
            &mut writer,
            "No such connection",
            Status::RESOURCE_NOT_FOUND,
        )
        .await;
        return;
    };
    let protocol = shared.protocol;
    let span = info_span!("join", id = %shared.id, protocol);
    async move {
        writer.record_metrics(protocol);
        let (info, params) =
            match handshake::negotiate(&mut reader, &mut writer, &shared.args, &shared.id).await {
                Ok(v) => v,
                Err(err) => {
                    warn!(error = %err, "handshake failed");
                    return;
                }
            };
        let read_only = params.bool("read-only");
        let viewer = shared.join();
        drop(shared);
        info!(read_only, binary = info.binary, "user joined");
        let active = metrics::gauge!("molcajeted_joined_users_active", "protocol" => protocol);
        active.increment(1.0);
        let (inputs, reader_task) = session::spawn_reader(reader);
        let result = run(&viewer, inputs, &mut writer, &info, protocol, read_only).await;
        reader_task.abort();
        active.decrement(1.0);
        match result {
            Ok(()) => info!("joined user left"),
            Err(err) => debug!(error = %err, "joined user lost"),
        }
    }
    .instrument(span)
    .await;
}

/// A joined user's session: its own frames from the shared screen, its
/// input to the owner's session.
async fn run(
    viewer: &Viewer,
    mut inputs: mpsc::Receiver<Input>,
    writer: &mut GuacWriter,
    info: &ClientInfo,
    protocol: &'static str,
    read_only: bool,
) -> std::io::Result<()> {
    let mut user = User {
        viewer,
        decoder: InputDecoder::default(),
        read_only,
    };
    let mut keepalive = tokio::time::interval_at(Instant::now() + KEEPALIVE, KEEPALIVE);

    // Until the owner's display has published its first frame.
    let setup = loop {
        if let Some(setup) = viewer.setup() {
            break setup;
        }
        if viewer.closed() {
            return finish(writer).await;
        }
        tokio::select! {
            () = viewer.wake.notified() => {}
            ins = inputs.recv() => {
                let Some(ins) = ins else { return Ok(()) };
                if !user.input(ins, None, writer).await {
                    return Ok(());
                }
            }
            _ = keepalive.tick() => writer.push("3.nop;"),
        }
        if writer.pending() > 0 {
            writer.flush().await?;
        }
    };
    let mut display = Display::new(ImageFormats::from_mimetypes(&info.image), setup.lossless)
        .with_protocol(protocol)
        .with_pacing(setup.pacing);
    if setup.video {
        session::enable_video(&mut display, info);
    }

    // Changes taken into the display and not sent yet.
    let mut pending = false;
    loop {
        match viewer.pull(&mut display, writer.buffer()) {
            Pulled::Closed => return finish(writer).await,
            Pulled::Changed => pending = true,
            Pulled::Nothing => {}
        }
        let window_open = display.unacked_frames() <= MAX_UNACKED_FRAMES;
        let due = (pending && window_open).then(|| Instant::from_std(display.next_frame_at()));
        if due.is_some_and(|due| due <= Instant::now()) {
            send_frame(&mut display, writer).await?;
            pending = false;
            continue;
        }
        let idle = (!pending)
            .then(|| display.idle_deadline())
            .flatten()
            .map(Instant::from_std);
        tokio::select! {
            () = viewer.wake.notified() => {}
            ins = inputs.recv() => {
                let Some(ins) = ins else { return Ok(()) };
                if !user.input(ins, Some(&mut display), writer).await {
                    return Ok(());
                }
            }
            () = sleep_until(due), if due.is_some() => {}
            () = sleep_until(idle), if idle.is_some() => {
                send_frame(&mut display, writer).await?;
            }
            _ = keepalive.tick() => {
                // A client that stopped acknowledging must not freeze its
                // screen forever.
                if pending && !window_open {
                    send_frame(&mut display, writer).await?;
                    pending = false;
                }
                writer.push("3.nop;");
            }
        }
        if writer.pending() > 0 {
            writer.flush().await?;
        }
    }
}

/// The input side of a joined user.
struct User<'a> {
    viewer: &'a Viewer,
    decoder: InputDecoder,
    read_only: bool,
}

impl User<'_> {
    /// Handles an instruction of the user's client: frame acknowledgements
    /// for its own display (once it has one), mouse, keys and clipboard for
    /// the owner's session. `false` when the user leaves.
    async fn input(
        &mut self,
        ins: Input,
        display: Option<&mut Display>,
        writer: &mut GuacWriter,
    ) -> bool {
        let event = self.decoder.decode(&ins, writer.buffer());
        match (event, display) {
            (Some(ClientEvent::Disconnect), _) => return false,
            (
                Some(ClientEvent::Sync {
                    timestamp,
                    received,
                }),
                Some(display),
            ) => {
                display.ack(timestamp, received);
            }
            (Some(ClientEvent::Ack { stream, status }), Some(display)) => {
                display.stream_ack(stream, status, writer.buffer());
            }
            (Some(ClientEvent::Mouse { .. } | ClientEvent::Key { .. }), _) if !self.read_only => {
                self.viewer.forward(ins.instruction).await;
            }
            (Some(ClientEvent::Clipboard(text)), _) if !self.read_only => {
                self.viewer.forward_clipboard(&text).await;
            }
            // Each user chooses the video mode of their own display.
            (Some(ClientEvent::Argv { name, value }), Some(display)) if name == VIDEO_MODE => {
                display.set_video_mode(VideoMode::from_param(Some(&value)), writer.buffer());
            }
            _ => {}
        }
        true
    }
}

async fn sleep_until(at: Option<Instant>) {
    if let Some(at) = at {
        tokio::time::sleep_until(at).await;
    }
}

/// The owner's session ended: so does this one.
async fn finish(writer: &mut GuacWriter) -> std::io::Result<()> {
    writer.push("10.disconnect;");
    writer.flush().await
}

async fn send_frame(display: &mut Display, writer: &mut GuacWriter) -> std::io::Result<()> {
    tokio::task::block_in_place(|| display.flush(writer.buffer()));
    if writer.pending() > 0 {
        display.sync(writer.buffer());
    }
    writer.flush().await
}

#[cfg(test)]
mod tests;
