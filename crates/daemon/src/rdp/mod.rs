//! RDP support: bridges an RDP server to a Guacamole client, using IronRDP
//! for the protocol (connection sequence, CredSSP, bitmap codecs, channels).
//!
//! - [`settings`]: guacd-compatible connection parameters
//! - [`connect`]: TCP, TLS ([`tls`]) and the connection sequence
//! - [`active`]: the session loop (graphics, pointer, input, clipboard,
//!   resize)
//! - [`keyboard`], [`mouse`], [`slow_path`]: input translation
//! - [`graphics`], [`clipboard`], [`audio`], [`pacing`]: output helpers
//! - [`gfx`]: graphics over the graphics pipeline, for servers that have it

mod active;
mod audio;
#[cfg(test)]
mod bench;
mod clipboard;
mod connect;
mod display_control;
mod drive;
mod gfx;
mod graphics;
mod keyboard;
mod mouse;
mod pacing;
mod settings;
mod slow_path;
mod tls;

use molcajete_protocol::Status;
use tokio::sync::mpsc;
use tracing::{info, warn};

use self::active::{EarlyInput, Session};
use self::connect::ConnectError;
use self::drive::DriveBackend;
use self::settings::Settings;
use crate::files;
use crate::handshake::{ClientInfo, Params};
use crate::io::{GuacReader, GuacWriter};
use crate::jump::Chain;
use crate::send_error;
use crate::session::{self, ClientEvent, Input, InputDecoder};

pub use self::settings::ARGS;

pub async fn run(
    reader: GuacReader,
    mut writer: GuacWriter,
    info: ClientInfo,
    params: Params,
) -> anyhow::Result<()> {
    let settings = match Settings::parse(&params, &info) {
        Ok(settings) => settings,
        Err(err) => {
            send_error(&mut writer, &err.to_string(), Status::CLIENT_BAD_REQUEST).await;
            return Ok(());
        }
    };
    let mut jump = match Chain::from_params(&params) {
        Ok(jump) => jump,
        Err(err) => {
            send_error(&mut writer, &err, Status::CLIENT_BAD_REQUEST).await;
            return Ok(());
        }
    };
    let (mut inputs, client_task) = session::spawn_reader(reader);
    let mut client = Client {
        inputs: &mut inputs,
        decoder: InputDecoder::default(),
        writer: &mut writer,
        early: EarlyInput::default(),
    };
    connect_and_run(&settings, &mut jump, &info, &mut client).await;
    writer.push("10.disconnect;");
    let _ = writer.flush().await;
    client_task.abort();
    Ok(())
}

/// The Guacamole side of the connection while the RDP side is set up.
struct Client<'a> {
    inputs: &'a mut mpsc::Receiver<Input>,
    decoder: InputDecoder,
    writer: &'a mut GuacWriter,
    early: EarlyInput,
}

impl Client<'_> {
    /// Decodes one instruction. Clipboard and size changes are kept for when
    /// the session starts; other events are returned.
    fn decode(&mut self, ins: &Input) -> Option<ClientEvent> {
        match self.decoder.decode(ins, self.writer.buffer())? {
            ClientEvent::Clipboard(text) => {
                self.early.clipboard = Some(text);
                None
            }
            ClientEvent::Size { width, height } => {
                self.early.size = Some((width, height));
                None
            }
            event => Some(event),
        }
    }

    /// Sends pending acknowledgements; `None` if the client is gone.
    async fn flush(&mut self) -> Option<()> {
        if self.writer.pending() > 0 {
            self.writer.flush().await.ok()?;
        }
        Some(())
    }

    /// Awaits `future` while watching the client; `None` if it left.
    async fn alongside<T>(&mut self, future: impl Future<Output = T>) -> Option<T> {
        tokio::pin!(future);
        loop {
            // Only cancel-safe operations are raced against the future.
            let ins = tokio::select! {
                result = &mut future => return Some(result),
                ins = self.inputs.recv() => ins?,
            };
            if let Some(ClientEvent::Disconnect) = self.decode(&ins) {
                return None;
            }
            self.flush().await?;
        }
    }

    /// Asks the user for missing credentials (protocol 1.3+) and waits for
    /// all of them. `None` if the client left.
    async fn ask(&mut self, names: &[&'static str]) -> Option<Vec<(String, String)>> {
        session::send_required(self.writer.buffer(), names);
        self.flush().await?;
        let mut answers: Vec<(String, String)> = Vec::new();
        while answers.len() < names.len() {
            let ins = self.inputs.recv().await?;
            let event = self.decode(&ins);
            self.flush().await?;
            match event {
                Some(ClientEvent::Disconnect) => return None,
                Some(ClientEvent::Argv { name, value }) if names.contains(&name.as_str()) => {
                    answers.retain(|(n, _)| *n != name);
                    answers.push((name, value));
                }
                _ => {}
            }
        }
        Some(answers)
    }
}

async fn connect_and_run(
    settings: &Settings,
    jump: &mut Chain,
    info: &ClientInfo,
    client: &mut Client<'_>,
) {
    let missing = jump.missing();
    if !missing.is_empty() && info.version.supports_required() {
        let Some(answers) = client.ask(&missing).await else {
            return;
        };
        for (name, value) in answers {
            jump.set(&name, value);
        }
    }
    info!(
        host = %settings.hostname,
        port = settings.port,
        security = ?settings.security,
        jump_hosts = jump.hops().len(),
        "connecting to RDP server"
    );
    let (drive, drive_channel) = open_drive(settings).await.unzip();
    let (sound, audio) = match settings.audio {
        Some(encoding) => {
            let (sink, output) = audio::output(encoding);
            (Some(sink), Some(output))
        }
        None => (None, None),
    };
    let mut negotiated = match client
        .alongside(connect::begin(
            settings,
            jump,
            sound.as_ref(),
            drive_channel,
        ))
        .await
    {
        None => return,
        Some(Ok(negotiated)) => negotiated,
        Some(Err(err)) => return report(client.writer, settings, err).await,
    };
    let missing = negotiated.missing_credentials();
    if !missing.is_empty() && info.version.supports_required() {
        let Some(answers) = client.ask(&missing).await else {
            return;
        };
        for (name, value) in answers {
            negotiated.set_credential(&name, value);
        }
    }
    let (result, framed) = match client.alongside(negotiated.finish()).await {
        None => return,
        Some(Ok(connected)) => connected,
        Some(Err(err)) => return report(client.writer, settings, err).await,
    };

    let early = std::mem::take(&mut client.early);
    let files = drive.map(|directory| {
        client.decoder.accept_files();
        files::Transfers::drive(directory, client.writer.buffer())
    });
    let session = Session::new(settings, info, client.writer, result, framed, audio, files);
    session.run(client.inputs, &mut client.decoder, early).await;
}

/// The drive's directory, opened (and created if asked) before connecting.
/// A drive whose directory cannot be opened is left out; the session goes
/// ahead without it.
async fn open_drive(settings: &Settings) -> Option<(files::Drive, connect::Drive)> {
    let drive = settings.drive.as_ref()?;
    match files::Drive::open(drive.clone()).await {
        Ok(opened) => {
            let backend = match DriveBackend::new(
                connect::DRIVE_DEVICE_ID,
                &drive.name,
                opened.root(),
            ) {
                Ok(backend) => backend,
                Err(err) => {
                    warn!(path = %opened.root().display(), error = %err, "cannot redirect the drive directory");
                    return None;
                }
            };
            let channel = connect::Drive {
                name: drive.name.clone(),
                backend,
            };
            Some((opened, channel))
        }
        Err(err) => {
            warn!(path = %drive.path.display(), error = %err, "cannot open the drive directory");
            None
        }
    }
}

async fn report(writer: &mut GuacWriter, settings: &Settings, err: ConnectError) {
    warn!(
        host = %settings.hostname,
        port = settings.port,
        error = %err,
        "RDP connection failed"
    );
    send_error(writer, &err.to_string(), err.status()).await;
}
