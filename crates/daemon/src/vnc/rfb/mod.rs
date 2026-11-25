//! RFB (VNC) client.
//!
//! Implements the client side of the Remote Framebuffer protocol (RFC 6143 plus the community
//! pseudo-encodings guacd relies on) on top of tokio. [`connect`] performs the handshake and
//! returns a reader/writer pair that can be driven from separate tasks:
//!
//! * [`RfbReader::next_event`] decodes server messages into [`RfbEvent`]s whose pixel data is
//!   already converted to RGBA8.
//! * [`RfbWriter`] sends update requests, input events and clipboard text.
//!
//! The client always asks the server for 32 bpp little-endian true-colour pixels, so every
//! decoder only has to deal with that single pixel layout.

mod auth;
mod handshake;
mod hextile;
mod pixel;
mod reader;
mod writer;
mod zrle;

#[cfg(test)]
mod tests;

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::io::BufReader;
use tokio::net::TcpStream;

use crate::remote::{self, CountingStream};

pub use reader::RfbReader;
pub use writer::RfbWriter;

/// Size of the buffer wrapped around the socket's read half.
const READ_BUFFER_SIZE: usize = 64 * 1024;

/// Encoding and pseudo-encoding numbers understood by this client.
pub mod encoding {
    pub const RAW: i32 = 0;
    pub const COPY_RECT: i32 = 1;
    pub const RRE: i32 = 2;
    pub const HEXTILE: i32 = 5;
    pub const ZRLE: i32 = 16;
    pub const CURSOR: i32 = -239;
    pub const DESKTOP_SIZE: i32 = -223;
    pub const LAST_RECT: i32 = -224;
    pub const EXTENDED_DESKTOP_SIZE: i32 = -308;

    /// Encodings announced when [`super::ConnectOptions::encodings`] is `None`, in preference
    /// order.
    pub const DEFAULT: [i32; 8] = [
        ZRLE,
        HEXTILE,
        COPY_RECT,
        RAW,
        CURSOR,
        DESKTOP_SIZE,
        EXTENDED_DESKTOP_SIZE,
        LAST_RECT,
    ];

    /// Returns whether announcing `encoding` is safe, i.e. the reader can decode every
    /// rectangle the server may send in response.
    ///
    /// The compression level (-256..=-247) and JPEG quality (-32..=-23) pseudo-encodings never
    /// produce rectangles, so they are passed through as well.
    pub fn is_supported(encoding: i32) -> bool {
        matches!(
            encoding,
            RAW | COPY_RECT
                | RRE
                | HEXTILE
                | ZRLE
                | CURSOR
                | DESKTOP_SIZE
                | LAST_RECT
                | EXTENDED_DESKTOP_SIZE
        ) || (-256..=-247).contains(&encoding)
            || (-32..=-23).contains(&encoding)
    }
}

/// Parameters for [`connect`].
#[derive(Clone)]
pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    /// Password for VNC authentication (security type 2).
    pub password: Option<String>,
    /// Value of the ClientInit shared flag: `false` asks the server to disconnect other clients.
    pub shared: bool,
    /// Encodings to announce in preference order; `None` selects [`encoding::DEFAULT`].
    /// Encodings this client cannot decode are dropped with a warning.
    pub encodings: Option<Vec<i32>>,
}

impl fmt::Debug for ConnectOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConnectOptions")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("shared", &self.shared)
            .field("encodings", &self.encodings)
            .finish()
    }
}

/// Information from the server's ServerInit message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerInfo {
    pub width: u16,
    pub height: u16,
    pub name: String,
}

/// Decoded server events. All pixel data is RGBA8, row-major, top row first, alpha = 255 for
/// framebuffer pixels.
#[derive(Clone, PartialEq, Eq)]
pub enum RfbEvent {
    /// A rectangle of new framebuffer content.
    Rect {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        rgba: Vec<u8>,
    },
    /// Copy an already-received region (CopyRect encoding).
    CopyRect {
        src_x: u16,
        src_y: u16,
        x: u16,
        y: u16,
        width: u16,
        height: u16,
    },
    /// Solid fill. Emitted instead of [`RfbEvent::Rect`] whenever a decoded rectangle turns out
    /// to consist of a single colour.
    Fill {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        rgba: [u8; 4],
    },
    /// Framebuffer size changed (DesktopSize / ExtendedDesktopSize pseudo-encodings). Only
    /// emitted when the size actually differs from the previous one. The framebuffer content is
    /// undefined afterwards, so a non-incremental update should be requested.
    Resize {
        width: u16,
        height: u16,
    },
    /// New cursor image (Cursor pseudo-encoding -239); rgba has alpha 0 or 255 from the
    /// bitmask. A zero-sized cursor means the server wants the cursor hidden.
    Cursor {
        hotspot_x: u16,
        hotspot_y: u16,
        width: u16,
        height: u16,
        rgba: Vec<u8>,
    },
    /// End of one FramebufferUpdate message (after all its rectangles).
    UpdateEnd,
    Bell,
    /// ServerCutText, decoded from Latin-1.
    ClipboardText(String),
}

impl fmt::Debug for RfbEvent {
    // Pixel buffers are summarised by their length so logging an event stays readable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rect {
                x,
                y,
                width,
                height,
                rgba,
            } => f
                .debug_struct("Rect")
                .field("x", x)
                .field("y", y)
                .field("width", width)
                .field("height", height)
                .field("rgba_len", &rgba.len())
                .finish(),
            Self::CopyRect {
                src_x,
                src_y,
                x,
                y,
                width,
                height,
            } => f
                .debug_struct("CopyRect")
                .field("src_x", src_x)
                .field("src_y", src_y)
                .field("x", x)
                .field("y", y)
                .field("width", width)
                .field("height", height)
                .finish(),
            Self::Fill {
                x,
                y,
                width,
                height,
                rgba,
            } => f
                .debug_struct("Fill")
                .field("x", x)
                .field("y", y)
                .field("width", width)
                .field("height", height)
                .field("rgba", rgba)
                .finish(),
            Self::Resize { width, height } => f
                .debug_struct("Resize")
                .field("width", width)
                .field("height", height)
                .finish(),
            Self::Cursor {
                hotspot_x,
                hotspot_y,
                width,
                height,
                rgba,
            } => f
                .debug_struct("Cursor")
                .field("hotspot_x", hotspot_x)
                .field("hotspot_y", hotspot_y)
                .field("width", width)
                .field("height", height)
                .field("rgba_len", &rgba.len())
                .finish(),
            Self::UpdateEnd => f.write_str("UpdateEnd"),
            Self::Bell => f.write_str("Bell"),
            Self::ClipboardText(text) => f.debug_tuple("ClipboardText").field(text).finish(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RfbError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("RFB protocol error: {0}")]
    Protocol(String),
    #[error("VNC authentication failed: {0}")]
    AuthFailed(String),
    #[error("the server offers no supported security type")]
    UnsupportedSecurity,
}

/// Connects to a VNC server and performs the RFB handshake: version negotiation,
/// authentication, ClientInit/ServerInit, SetPixelFormat and SetEncodings.
///
/// No FramebufferUpdateRequest is sent; call [`RfbWriter::request_update`] to start receiving
/// framebuffer content.
pub async fn connect(
    options: &ConnectOptions,
) -> Result<(RfbReader, RfbWriter, ServerInfo), RfbError> {
    let stream = TcpStream::connect((options.host.as_str(), options.port)).await?;
    stream.set_nodelay(true)?;
    let (read_half, mut write_half) = stream.into_split();
    let read_half = CountingStream::new(read_half, remote::received_bytes("vnc"));
    let mut input = BufReader::with_capacity(READ_BUFFER_SIZE, read_half);

    let info = handshake::run(&mut input, &mut write_half, options).await?;

    let shared = Arc::new(Mutex::new(SharedState::default()));
    let reader = RfbReader::new(input, info.width, info.height, Arc::clone(&shared));
    let writer = RfbWriter::new(write_half, shared);
    Ok((reader, writer, info))
}

/// State the reader learns from the server and the writer needs for its messages.
#[derive(Debug, Default)]
struct SharedState {
    /// Set once the server has sent an ExtendedDesktopSize rectangle, which is how it signals
    /// support for SetDesktopSize.
    extended_desktop_size: bool,
    /// First screen of the most recent ExtendedDesktopSize layout.
    screen: Option<Screen>,
}

/// One entry of an ExtendedDesktopSize screen layout (only the fields SetDesktopSize echoes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Screen {
    id: u32,
    flags: u32,
}

/// Locks the shared state, ignoring poisoning: the state stays consistent even if a holder
/// panicked because every update is a plain field assignment.
fn lock_shared(shared: &Mutex<SharedState>) -> MutexGuard<'_, SharedState> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}
