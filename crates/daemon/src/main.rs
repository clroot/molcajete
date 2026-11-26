//! molcajeted: a proxy daemon for the Guacamole protocol, written in Rust.
//!
//! Accepts Guacamole protocol connections (from `molcajete-gateway` or the stock
//! Java web application), performs the handshake and bridges to the remote
//! desktop protocol selected by the client.

use std::path::PathBuf;

use clap::Parser as _;
use molcajete_daemon::{VideoCodec, VideoEncoders, handle};
use tokio::net::TcpListener;
use tracing::info;

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
    /// Seconds an RDP or VNC session is kept after its connection to the
    /// gateway ended unasked, for the same browser tab to resume it; 0 ends
    /// it at once. A connection's `resume-timeout` parameter overrides it.
    #[arg(long, env = "MOLCAJETED_RESUME_TIMEOUT", default_value_t = 120)]
    resume_timeout: u64,
    /// The same for SSH sessions.
    #[arg(long, env = "MOLCAJETED_RESUME_TIMEOUT_SSH", default_value_t = 300)]
    resume_timeout_ssh: u64,
    /// Sessions kept without a client at once; a session whose client
    /// leaves while this many are kept ends at once.
    #[arg(long, env = "MOLCAJETED_RESUME_MAX_DETACHED", default_value_t = 64)]
    resume_max_detached: usize,
    #[command(flatten)]
    telemetry: molcajete_telemetry::TelemetryArgs,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    molcajete_telemetry::init_logging(cli.telemetry.log_format, "info,pprof=warn");
    molcajete_telemetry::start(&cli.telemetry, "molcajeted").await?;
    molcajete_daemon::set_zstd_images(cli.image_zstd);
    molcajete_daemon::set_encode_threads(cli.encode_threads);
    molcajete_daemon::configure_resume(molcajete_daemon::ResumeConfig {
        desktop: std::time::Duration::from_secs(cli.resume_timeout),
        ssh: std::time::Duration::from_secs(cli.resume_timeout_ssh),
        max_detached: cli.resume_max_detached,
    });
    let allowed: Vec<_> = cli
        .video
        .iter()
        .filter_map(|name| match name.trim() {
            "av1" => Some(VideoCodec::Av1),
            "h265" | "hevc" => Some(VideoCodec::H265),
            "h264" | "avc" => Some(VideoCodec::H264),
            _ => None,
        })
        .collect();
    if !allowed.is_empty() {
        let ffmpeg = cli.ffmpeg.clone();
        let encoders =
            tokio::task::spawn_blocking(move || VideoEncoders::detect(ffmpeg.as_deref())).await?;
        info!(encoders = %encoders.describe(), "video encoders");
        molcajete_daemon::init_video(encoders, allowed);
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
