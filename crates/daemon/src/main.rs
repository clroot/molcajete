//! molcajeted: a proxy daemon for the Guacamole protocol, written in Rust.
//!
//! Accepts Guacamole protocol connections (from `molcajete-gateway` or the stock
//! Java web application), performs the handshake and bridges to the remote
//! desktop protocol selected by the client.

// Pixel loops iterate RGBA quadruplets with `chunks_exact(4)` throughout;
// switching every one to `as_chunks::<4>()` would only be churn.
#![allow(clippy::chunks_exact_to_as_chunks)]

mod display;
mod handshake;
mod io;
mod rdp;
mod remote;
mod session;
mod ssh;
mod video;
mod vnc;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Instant;

use clap::Parser as _;
use molcajete_protocol::{InstructionBuilder, Status};
use tokio::net::{TcpListener, TcpStream};
use tracing::{Instrument as _, info, info_span, warn};

use crate::io::{GuacReader, GuacWriter};

#[derive(clap::Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Address to listen on.
    #[arg(
        short = 'b',
        long,
        env = "MOLCAJETED_BIND_HOST",
        default_value = "127.0.0.1"
    )]
    bind_host: String,
    /// Port to listen on.
    #[arg(
        short = 'l',
        long,
        env = "MOLCAJETED_BIND_PORT",
        default_value_t = 4822
    )]
    bind_port: u16,
    /// ffmpeg binary used for additional video encoders (default: `ffmpeg`
    /// from PATH, if present).
    #[arg(long, env = "MOLCAJETED_FFMPEG")]
    ffmpeg: Option<PathBuf>,
    /// Video codecs that may be used for busy screen areas, most preferred
    /// first among those the client supports: any of `av1`, `h265`, `h264`,
    /// or `none` to always send images.
    #[arg(
        long,
        env = "MOLCAJETED_VIDEO",
        value_delimiter = ',',
        default_value = "av1,h265,h264"
    )]
    video: Vec<String>,
    /// Send lossless updates as zstd images to the clients that announce
    /// the type (molcajete's web client); `false` keeps every client on PNG.
    #[arg(
        long,
        env = "MOLCAJETED_IMAGE_ZSTD",
        default_value_t = true,
        action = clap::ArgAction::Set
    )]
    image_zstd: bool,
    /// Threads that encode the images of a frame side by side, shared by
    /// all sessions: 0 for half the available cores (at most 8), 1 to
    /// encode on the session's own thread.
    #[arg(long, env = "MOLCAJETED_ENCODE_THREADS", default_value_t = 0)]
    encode_threads: usize,
    #[command(flatten)]
    telemetry: molcajete_telemetry::TelemetryArgs,
}

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

async fn handle(stream: TcpStream, peer: SocketAddr) {
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
        send_error(
            &mut writer,
            "Joining existing connections is not supported yet",
            Status::RESOURCE_NOT_FOUND,
        )
        .await;
        return;
    }

    let connection_id = format!("${}", uuid::Uuid::new_v4());
    let (protocol, args) = match selected.as_str() {
        "vnc" => ("vnc", vnc::ARGS),
        "ssh" => ("ssh", ssh::ARGS),
        "rdp" => ("rdp", rdp::ARGS),
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
    session(reader, writer, protocol, args, connection_id)
        .instrument(span)
        .await;
}

async fn session(
    mut reader: GuacReader,
    mut writer: GuacWriter,
    protocol: &'static str,
    args: &[&str],
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
    let result = match protocol {
        "vnc" => vnc::run(reader, writer, info, params).await,
        "ssh" => ssh::run(reader, writer, info, params).await,
        "rdp" => rdp::run(reader, writer, info, params).await,
        _ => unreachable!("protocol validated above"),
    };
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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    molcajete_telemetry::init_logging(cli.telemetry.log_format, "info,pprof=warn");
    molcajete_telemetry::start(&cli.telemetry, "molcajeted").await?;
    display::set_zstd_images(cli.image_zstd);
    display::set_encode_threads(cli.encode_threads);
    let allowed: Vec<_> = cli
        .video
        .iter()
        .filter_map(|name| match name.trim() {
            "av1" => Some(video::VideoCodec::Av1),
            "h265" | "hevc" => Some(video::VideoCodec::H265),
            "h264" | "avc" => Some(video::VideoCodec::H264),
            _ => None,
        })
        .collect();
    if !allowed.is_empty() {
        let ffmpeg = cli.ffmpeg.clone();
        let encoders =
            tokio::task::spawn_blocking(move || video::VideoEncoders::detect(ffmpeg.as_deref()))
                .await?;
        info!(encoders = %encoders.describe(), "video encoders");
        session::init_video(encoders, allowed);
    }
    let listener = TcpListener::bind((cli.bind_host.as_str(), cli.bind_port)).await?;
    info!(addr = %listener.local_addr()?, "molcajeted listening");
    let shutdown = molcajete_telemetry::shutdown_signal();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                tokio::spawn(handle(stream, peer));
            }
            () = &mut shutdown => {
                info!("shutting down");
                return Ok(());
            }
        }
    }
}
