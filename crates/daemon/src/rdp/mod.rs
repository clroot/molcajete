//! RDP support: bridges an RDP server to a Guacamole client, using IronRDP
//! for the protocol (connection sequence, CredSSP, bitmap codecs, channels).
//!
//! - [`settings`]: guacd-compatible connection parameters
//! - [`connect`]: TCP, TLS ([`tls`]) and the connection sequence
//! - [`active`]: the session loop (graphics, pointer, input, clipboard,
//!   resize)
//! - [`keyboard`], [`mouse`], [`slow_path`]: input translation
//! - [`graphics`], [`clipboard`], [`audio`], [`pacing`]: output helpers

mod active;
mod audio;
mod clipboard;
mod connect;
mod graphics;
mod keyboard;
mod mouse;
mod pacing;
mod settings;
mod slow_path;
mod tls;

use molcajete_protocol::{Instruction, Status};
use tokio::sync::mpsc;
use tracing::{info, warn};

use self::active::{EarlyInput, Session};
use self::connect::ConnectError;
use self::settings::Settings;
use crate::handshake::{ClientInfo, Params};
use crate::io::{GuacReader, GuacWriter};
use crate::send_error;
use crate::session::{self, ClientEvent, InputDecoder};

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
    let (mut inputs, client_task) = session::spawn_reader(reader);
    let mut client = Client {
        inputs: &mut inputs,
        decoder: InputDecoder::default(),
        writer: &mut writer,
        early: EarlyInput::default(),
    };
    connect_and_run(&settings, &info, &mut client).await;
    writer.push("10.disconnect;");
    let _ = writer.flush().await;
    client_task.abort();
    Ok(())
}

/// The Guacamole side of the connection while the RDP side is set up.
struct Client<'a> {
    inputs: &'a mut mpsc::Receiver<Instruction>,
    decoder: InputDecoder,
    writer: &'a mut GuacWriter,
    early: EarlyInput,
}

impl Client<'_> {
    /// Decodes one instruction. Clipboard and size changes are kept for when
    /// the session starts; other events are returned.
    fn decode(&mut self, ins: &Instruction) -> Option<ClientEvent> {
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

async fn connect_and_run(settings: &Settings, info: &ClientInfo, client: &mut Client<'_>) {
    info!(
        host = %settings.hostname,
        port = settings.port,
        security = ?settings.security,
        "connecting to RDP server"
    );
    let (sound, audio) = match settings.audio {
        Some(encoding) => {
            let (sink, output) = audio::output(encoding);
            (Some(sink), Some(output))
        }
        None => (None, None),
    };
    let mut negotiated = match client
        .alongside(connect::begin(settings, sound.as_ref()))
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
    let session = Session::new(settings, info, client.writer, result, framed, audio);
    session.run(client.inputs, &mut client.decoder, early).await;
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
