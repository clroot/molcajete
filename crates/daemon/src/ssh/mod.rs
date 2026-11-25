//! SSH support: a terminal emulator rendered on the server side, like
//! guacd's SSH plugin.
//!
//! The SSH session's output is interpreted by a terminal emulator
//! (`alacritty_terminal`); its cells are rasterised with bundled or system
//! fonts into the display, which sends the changed pixels to the client as
//! ordinary Guacamole drawing instructions. Any Guacamole client works.
//!
//! Module overview:
//! - [`settings`]: connection parameters (guacd's names and defaults)
//! - [`client`]: SSH connection, host key check, authentication, channel
//! - [`terminal`]: the emulator, scrollback and selection
//! - [`render`], [`font`], [`boxdraw`], [`color`]: drawing cells
//! - [`keys`], [`mouse`]: input translation
//! - [`bridge`]: the session loop tying it all together

mod boxdraw;
mod bridge;
mod client;
mod color;
mod font;
mod keys;
mod mouse;
mod render;
mod settings;
mod terminal;

use std::future::Future;

use molcajete_protocol::Status;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::display::{Display, ImageFormats, Pacing};
use crate::handshake::{ClientInfo, Params};
use crate::io::{GuacReader, GuacWriter};
use crate::send_error;
use crate::session::{self, ClientEvent, Input, InputDecoder};

use bridge::{Bridge, KEEPALIVE};
use client::Credentials;
use color::{ColorScheme, Rgb};
use font::{FontOptions, Fonts};
use render::Renderer;
use settings::Settings;

pub use settings::ARGS;

pub async fn run(
    reader: GuacReader,
    mut writer: GuacWriter,
    info: ClientInfo,
    params: Params,
) -> anyhow::Result<()> {
    let mut settings = Settings::from_params(&params, &info);

    let font_options = FontOptions {
        name: settings.font_name.clone(),
        size_pt: settings.font_size,
        dpi: info.dpi,
        search_paths: settings.font_path.clone(),
    };
    let fonts = tokio::task::spawn_blocking(move || Fonts::load(&font_options)).await?;
    let (scheme, errors) = ColorScheme::parse(settings.color_scheme.as_deref().unwrap_or(""));
    for error in errors {
        warn!(%error, "ignoring invalid color-scheme entry");
    }
    let background = scheme.background;
    let renderer = Renderer::new(fonts, scheme);

    // Terminal text must stay crisp: never use lossy encodings.
    let mut display = Display::new(ImageFormats::from_mimetypes(&info.image), true)
        .with_protocol("ssh")
        .with_pacing(Pacing::TERMINAL);
    session::send_name(writer.buffer(), &settings.hostname);
    display.resize(info.width, info.height, writer.buffer());
    paint_background(&mut display, &mut writer, background);
    writer.flush().await?;

    let (mut inputs, client_task) = session::spawn_reader(reader);
    let mut decoder = InputDecoder::default();
    let result = async {
        if !obtain_credentials(
            &mut settings,
            &info,
            &mut inputs,
            &mut decoder,
            &mut display,
            &mut writer,
            background,
        )
        .await?
        {
            return Ok(());
        }
        connect_and_run(settings, renderer, display, writer, inputs, decoder).await
    }
    .await;
    client_task.abort();
    result
}

/// Fills the whole display with the background colour and ends the frame.
fn paint_background(display: &mut Display, writer: &mut GuacWriter, background: Rgb) {
    let [r, g, b] = background;
    display.fill(0, 0, display.width(), display.height(), [r, g, b, 255]);
    display.flush(writer.buffer());
    display.sync(writer.buffer());
}

/// Asks the client for a missing username and password (protocol 1.3+).
/// Returns `false` when the client left or cannot provide them.
async fn obtain_credentials(
    settings: &mut Settings,
    info: &ClientInfo,
    inputs: &mut mpsc::Receiver<Input>,
    decoder: &mut InputDecoder,
    display: &mut Display,
    writer: &mut GuacWriter,
    background: Rgb,
) -> anyhow::Result<bool> {
    let mut missing: Vec<&str> = Vec::new();
    if settings.username.is_none() {
        missing.push("username");
    }
    if settings.password.is_none() && settings.private_key.is_none() {
        missing.push("password");
    }
    if missing.is_empty() {
        return Ok(true);
    }
    if !info.version.supports_required() {
        if settings.username.is_none() {
            send_error(writer, "No username was given", Status::CLIENT_UNAUTHORIZED).await;
            return Ok(false);
        }
        // Without a password only keyless methods can succeed; let the
        // server decide.
        return Ok(true);
    }

    session::send_required(writer.buffer(), &missing);
    writer.flush().await?;
    let mut keepalive = tokio::time::interval(KEEPALIVE);
    while !missing.is_empty() {
        tokio::select! {
            ins = inputs.recv() => {
                let Some(ins) = ins else { return Ok(false) };
                match decoder.decode(&ins, writer.buffer()) {
                    Some(ClientEvent::Argv { name, value }) => {
                        match name.as_str() {
                            "username" => settings.username = Some(value),
                            "password" => settings.password = Some(value),
                            _ => {}
                        }
                        missing.retain(|m| *m != name);
                    }
                    Some(ClientEvent::Size { width, height }) => {
                        display.resize(width, height, writer.buffer());
                        paint_background(display, writer, background);
                    }
                    Some(ClientEvent::Sync { timestamp, received }) => {
                        display.ack(timestamp, received)
                    }
                    Some(ClientEvent::Disconnect) => return Ok(false),
                    _ => {}
                }
            }
            _ = keepalive.tick() => writer.push("3.nop;"),
        }
        writer.flush().await?;
    }
    Ok(true)
}

async fn connect_and_run(
    settings: Settings,
    renderer: Renderer,
    display: Display,
    mut writer: GuacWriter,
    inputs: mpsc::Receiver<Input>,
    decoder: InputDecoder,
) -> anyhow::Result<()> {
    let target = format!("{}:{}", settings.hostname, settings.port);
    let credentials = Credentials {
        username: settings.username.as_deref().unwrap_or_default(),
        password: settings.password.as_deref(),
        private_key: settings.private_key.as_deref(),
        passphrase: settings.passphrase.as_deref(),
    };
    let connecting = async {
        let mut handle = client::connect(&settings).await?;
        client::authenticate(&mut handle, &credentials).await?;
        Ok::<_, client::ConnectError>(handle)
    };
    let handle = match with_keepalive(&mut writer, connecting).await? {
        Ok(handle) => handle,
        Err(err) => {
            warn!(%target, error = %err, "SSH connection failed");
            send_error(
                &mut writer,
                &format!("SSH connection failed: {err}"),
                err.status(),
            )
            .await;
            return Ok(());
        }
    };
    info!(%target, user = credentials.username, "SSH authenticated");

    let size = renderer.grid_size(display.width(), display.height());
    let channel =
        match client::open_session(&handle, &settings, Bridge::pty_size(&renderer, size)).await {
            Ok(channel) => channel,
            Err(err) => {
                warn!(%target, error = %err, "cannot open the SSH session");
                send_error(
                    &mut writer,
                    "Unable to open an SSH session",
                    Status::UPSTREAM_ERROR,
                )
                .await;
                return Ok(());
            }
        };
    let (channel_reader, channel_writer) = channel.split();
    let (ssh, writer_task) = client::spawn_writer(channel_writer);
    let bridge = Bridge::new(settings, renderer, display, writer, ssh);
    let result = bridge.run(channel_reader, inputs, decoder).await;
    writer_task.abort();
    let _ = handle
        .disconnect(russh::Disconnect::ByApplication, "session closed", "en")
        .await;
    result
}

/// Awaits `future` while sending `nop`s so that the client does not time
/// out during slow operations (DNS, key exchange, authentication).
async fn with_keepalive<F: Future>(
    writer: &mut GuacWriter,
    future: F,
) -> std::io::Result<F::Output> {
    tokio::pin!(future);
    let start = tokio::time::Instant::now() + KEEPALIVE;
    let mut keepalive = tokio::time::interval_at(start, KEEPALIVE);
    loop {
        tokio::select! {
            output = &mut future => return Ok(output),
            _ = keepalive.tick() => {
                writer.push("3.nop;");
                writer.flush().await?;
            }
        }
    }
}
