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
mod session;
mod ssh;
mod video;
mod vnc;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser as _;
use molcajete_protocol::{InstructionBuilder, Status};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::io::{GuacReader, GuacWriter};

#[derive(clap::Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Address to listen on.
    #[arg(short = 'b', long, default_value = "127.0.0.1")]
    bind_host: String,
    /// Port to listen on.
    #[arg(short = 'l', long, default_value_t = 4822)]
    bind_port: u16,
    /// ffmpeg binary used for additional video encoders (default: `ffmpeg`
    /// from PATH, if present).
    #[arg(long)]
    ffmpeg: Option<PathBuf>,
    /// Video codecs that may be used for busy screen areas, most preferred
    /// first among those the client supports: any of `av1`, `h265`, `h264`,
    /// or `none` to always send images.
    #[arg(long, value_delimiter = ',', default_value = "av1,h265,h264")]
    video: Vec<String>,
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

async fn handle(stream: TcpStream, peer: SocketAddr) {
    let _ = stream.set_nodelay(true);
    let (rd, wr) = stream.into_split();
    let mut reader = GuacReader::new(rd);
    let mut writer = GuacWriter::new(wr);

    let selected = match handshake::read_select(&mut reader).await {
        Ok(s) => s,
        Err(err) => {
            warn!(%peer, error = %err, "handshake failed");
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
    let args = match selected.as_str() {
        "vnc" => vnc::ARGS,
        "ssh" => ssh::ARGS,
        "rdp" => rdp::ARGS,
        other => {
            warn!(%peer, protocol = other, "unsupported protocol requested");
            send_error(
                &mut writer,
                &format!("Protocol \"{other}\" is not supported"),
                Status::UNSUPPORTED,
            )
            .await;
            return;
        }
    };
    let (info, params) =
        match handshake::negotiate(&mut reader, &mut writer, args, &connection_id).await {
            Ok(v) => v,
            Err(err) => {
                warn!(%peer, error = %err, "handshake failed");
                return;
            }
        };
    info!(
        %peer,
        id = %connection_id,
        protocol = %selected,
        width = info.width,
        height = info.height,
        "connection established"
    );
    let result = match selected.as_str() {
        "vnc" => vnc::run(reader, writer, info, params).await,
        "ssh" => ssh::run(reader, writer, info, params).await,
        "rdp" => rdp::run(reader, writer, info, params).await,
        _ => unreachable!("protocol validated above"),
    };
    if let Err(err) = result {
        warn!(id = %connection_id, error = %err, "session ended with error");
    }
    info!(id = %connection_id, "connection closed");
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let cli = Cli::parse();
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
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = accepted?;
                tokio::spawn(handle(stream, peer));
            }
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
        }
    }
}
