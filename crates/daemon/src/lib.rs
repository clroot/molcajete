//! molcajeted as a library: the connection handling of the daemon, which
//! `main.rs` wraps in a command line and a listener.
//!
//! Only what the binary needs is public, plus [`fuzzing`], the entry points
//! of the fuzz targets in `fuzz/` (not a stable interface).

// Pixel loops iterate RGBA quadruplets with `chunks_exact(4)` throughout;
// switching every one to `as_chunks::<4>()` would only be churn.
#![allow(clippy::chunks_exact_to_as_chunks)]

mod display;
mod files;
#[doc(hidden)]
pub mod fuzzing;
mod handshake;
mod io;
mod jump;
mod rdp;
mod remote;
mod session;
mod share;
mod ssh;
mod video;
mod vnc;

use std::net::SocketAddr;
use std::time::Instant;

use molcajete_protocol::{InstructionBuilder, Status};
use tokio::net::TcpStream;
use tracing::{Instrument as _, info, info_span, warn};

pub use display::{set_encode_threads, set_zstd_images};
pub use session::init_video;
pub use video::{VideoCodec, VideoEncoders};

use crate::io::{GuacReader, GuacWriter};

/// Sends an `error` instruction and flushes.
pub async fn send_error(writer: &mut GuacWriter, message: &str, status: Status) {
    writer.push(
        &InstructionBuilder::new("error")
            .str_arg(message)
            .arg(status.code())
            .finish(),
    );
    let _ = writer.flush().await;
}

/// Counts a finished (or refused) connection.
fn connection_done(protocol: &'static str, result: &'static str) {
    metrics::counter!(
        "molcajeted_connections_total",
        "protocol" => protocol,
        "result" => result,
    )
    .increment(1);
}

/// Serves one client connection, from the handshake to the end of the session.
pub async fn handle(stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let (rd, wr) = stream.into_split();
    let mut reader = GuacReader::new(rd);
    let mut writer = GuacWriter::new(wr);

    let selected = match handshake::read_select(&mut reader).await {
        Ok(s) => s,
        Err(err) => {
            warn!(%peer, error = %err, "handshake failed");
            connection_done("unknown", "handshake_failed");
            return;
        }
    };

    if selected.starts_with('$') {
        share::join(reader, writer, &selected).await;
        return;
    }

    let connection_id = format!("${}", uuid::Uuid::new_v4());
    if selected == RELAY {
        let span = info_span!("connection", id = %connection_id, protocol = RELAY, %peer);
        relay(reader, writer, connection_id).instrument(span).await;
        return;
    }
    let (protocol, args, file_args) = match selected.as_str() {
        "vnc" => ("vnc", vnc::ARGS, &[][..]),
        "ssh" => ("ssh", ssh::ARGS, files::SSH_ARGS),
        "rdp" => ("rdp", rdp::ARGS, files::RDP_ARGS),
        other => {
            warn!(%peer, protocol = other, "unsupported protocol requested");
            connection_done("unknown", "unsupported");
            send_error(
                &mut writer,
                &format!("Protocol \"{other}\" is not supported"),
                Status::UNSUPPORTED,
            )
            .await;
            return;
        }
    };
    // Everything logged for this connection carries its id, which the
    // gateway logs as `guacd_id`.
    let span = info_span!("connection", id = %connection_id, protocol, %peer);
    // Every protocol can go through SSH jump hosts.
    let args: Vec<&'static str> = args
        .iter()
        .chain(file_args)
        .chain(jump::ARGS)
        .copied()
        .collect();
    session(reader, writer, protocol, &args, connection_id)
        .instrument(span)
        .await;
}

/// `select` value of an RDP connection relayed to a client that runs RDP
/// itself (`rdp::relay`). Only a molcajete gateway asks for it.
const RELAY: &str = "rdp-relay";

/// Serves a relayed RDP connection: the handshake, `ready`, and then raw
/// RDCleanPath and RDP bytes instead of Guacamole instructions.
async fn relay(mut reader: GuacReader, mut writer: GuacWriter, connection_id: String) {
    let args: Vec<&'static str> = rdp::ARGS
        .iter()
        .chain(jump::ARGS)
        .chain(rdp::relay::ARGS)
        .copied()
        .collect();
    let (info, params) =
        match handshake::negotiate(&mut reader, &mut writer, &args, &connection_id).await {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, "handshake failed");
                connection_done(RELAY, "handshake_failed");
                return;
            }
        };
    info!("relay established");
    let active = metrics::gauge!("molcajeted_connections_active", "protocol" => RELAY);
    active.increment(1.0);
    let started = Instant::now();
    rdp::relay::run(reader, writer, info, params).await;
    active.decrement(1.0);
    let duration = started.elapsed();
    metrics::histogram!("molcajeted_session_duration_seconds", "protocol" => RELAY)
        .record(duration);
    connection_done(RELAY, "ok");
    info!(duration_s = duration.as_secs_f64(), "connection closed");
}

async fn session(
    mut reader: GuacReader,
    mut writer: GuacWriter,
    protocol: &'static str,
    args: &[&'static str],
    connection_id: String,
) {
    writer.record_metrics(protocol);
    let (info, params) =
        match handshake::negotiate(&mut reader, &mut writer, args, &connection_id).await {
            Ok(v) => v,
            Err(err) => {
                warn!(error = %err, "handshake failed");
                connection_done(protocol, "handshake_failed");
                return;
            }
        };
    info!(
        width = info.width,
        height = info.height,
        dpi = info.dpi,
        image = ?info.image,
        video = ?info.video,
        binary = info.binary,
        "connection established"
    );
    let active = metrics::gauge!("molcajeted_connections_active", "protocol" => protocol);
    active.increment(1.0);
    let started = Instant::now();
    // Others may join (`select,<connection id>`) until the session ends.
    let owner = share::Owner::register(&connection_id, protocol, args);
    let result = owner
        .run(async {
            match protocol {
                "vnc" => vnc::run(reader, writer, info, params).await,
                "ssh" => ssh::run(reader, writer, info, params).await,
                "rdp" => rdp::run(reader, writer, info, params).await,
                _ => unreachable!("protocol validated above"),
            }
        })
        .await;
    drop(owner);
    active.decrement(1.0);
    let duration = started.elapsed();
    metrics::histogram!("molcajeted_session_duration_seconds", "protocol" => protocol)
        .record(duration);
    match result {
        Ok(()) => connection_done(protocol, "ok"),
        Err(err) => {
            connection_done(protocol, "error");
            warn!(error = %err, "session ended with error");
        }
    }
    info!(duration_s = duration.as_secs_f64(), "connection closed");
}
