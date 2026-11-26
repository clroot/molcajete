//! File transfer between the browser and the remote side, as Guacamole
//! filesystem objects and file streams.
//!
//! A session exposes one filesystem object (`filesystem`). The client lists
//! a directory or downloads a file with `get`, answered by a `body` stream
//! (a JSON index for a directory), and uploads with `put` into the object
//! or with `file` into its upload directory. Every blob is acknowledged:
//! uploads after the data is written, downloads keep a window of
//! unacknowledged data in flight.
//!
//! The files live in a directory on this host for RDP, which the RDP drive
//! redirection serves to the server as well (`enable-drive`), or on the SSH
//! server, over SFTP on the terminal's connection (`enable-sftp`).
//!
//! Module overview:
//! - [`path`]: paths inside the object, listings
//! - [`backend`]: the local directory and SFTP
//! - [`task`]: the task that serves the streams

mod backend;
mod path;
mod task;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;

use molcajete_protocol::Status;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use backend::{Backend, LocalDirectory, Sftp};
use task::{FIRST_DOWNLOAD_STREAM, MAX_DOWNLOADS, Request, Task};

use crate::io::Output;

/// Parameters of SFTP in SSH sessions: guacd's, and `max-file-size`.
pub const SSH_ARGS: &[&str] = &[
    "enable-sftp",
    "sftp-root-directory",
    "sftp-disable-download",
    "sftp-disable-upload",
    MAX_FILE_SIZE,
];

/// Parameters of the RDP drive: guacd's, and `max-file-size`.
pub const RDP_ARGS: &[&str] = &[
    "enable-drive",
    "drive-name",
    "drive-path",
    "create-drive-path",
    "disable-download",
    "disable-upload",
    MAX_FILE_SIZE,
];

/// The largest file that may be uploaded or downloaded, in MiB (a
/// molcajeted extension; empty or 0 for no limit).
const MAX_FILE_SIZE: &str = "max-file-size";

/// The index of the one filesystem object of a session.
pub const OBJECT: i32 = 0;

/// The name of the drive when `drive-name` is not given.
const DEFAULT_DRIVE_NAME: &str = "molcajete";

/// Requests the session loop may queue for the task before an upload that
/// sends more is stopped. A client waits for acknowledgements, so only one
/// that ignores them gets there.
const MAX_QUEUED: usize = 1024;

/// Which directions are allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    pub download: bool,
    pub upload: bool,
    /// Bytes.
    pub max_size: Option<u64>,
}

impl Policy {
    fn from_params(
        get: impl Fn(&str) -> Option<String>,
        disable_download: &str,
        disable_upload: &str,
    ) -> Self {
        let flag = |name: &str| get(name).as_deref() == Some("true");
        Self {
            download: !flag(disable_download),
            upload: !flag(disable_upload),
            max_size: get(MAX_FILE_SIZE)
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|&mib| mib > 0)
                .map(|mib| mib.saturating_mul(1024 * 1024)),
        }
    }
}

/// SFTP in an SSH session (`enable-sftp`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SftpSettings {
    pub root: String,
    pub policy: Policy,
}

impl SftpSettings {
    /// `None` unless `enable-sftp` is `true`. `get` returns non-empty values.
    pub fn from_params(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if get("enable-sftp").as_deref() != Some("true") {
            return None;
        }
        Some(Self {
            root: get("sftp-root-directory").unwrap_or_else(|| "/".to_owned()),
            policy: Policy::from_params(&get, "sftp-disable-download", "sftp-disable-upload"),
        })
    }
}

/// The drive of an RDP session (`enable-drive`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriveSettings {
    pub path: PathBuf,
    pub name: String,
    pub create: bool,
    pub policy: Policy,
}

impl DriveSettings {
    /// `None` unless `enable-drive` is `true` and `drive-path` is given.
    pub fn from_params(get: impl Fn(&str) -> Option<String>) -> Option<Self> {
        if get("enable-drive").as_deref() != Some("true") {
            return None;
        }
        let Some(path) = get("drive-path") else {
            warn!("enable-drive is set without a drive-path; no drive is redirected");
            return None;
        };
        Some(Self {
            path: PathBuf::from(path),
            name: get("drive-name").unwrap_or_else(|| DEFAULT_DRIVE_NAME.to_owned()),
            create: get("create-drive-path").as_deref() == Some("true"),
            policy: Policy::from_params(&get, "disable-download", "disable-upload"),
        })
    }
}

/// The drive of an RDP session with its directory opened.
pub struct Drive {
    directory: LocalDirectory,
    settings: DriveSettings,
}

impl Drive {
    /// Opens the drive's directory, creating it first if asked.
    pub async fn open(settings: DriveSettings) -> std::io::Result<Self> {
        let directory = LocalDirectory::open_root(&settings.path, settings.create).await?;
        Ok(Self {
            directory,
            settings,
        })
    }

    /// The directory, resolved.
    pub fn root(&self) -> &std::path::Path {
        self.directory.root()
    }
}

/// File transfer input from the client, decoded by
/// [`crate::session::InputDecoder`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// An upload: `put` into a filesystem object (`object`, with the path as
    /// `name`) or `file` (no object, a file name for the upload directory).
    Open {
        stream: i32,
        object: Option<i32>,
        name: String,
    },
    Data {
        stream: i32,
        data: Vec<u8>,
    },
    /// The upload ended; `aborted` when its data could not be decoded.
    End {
        stream: i32,
        aborted: bool,
    },
    /// A directory listing or a download.
    Get {
        object: i32,
        name: String,
    },
}

/// What the task sends to the client.
pub enum Reply {
    Text(String),
    Blob { stream: i32, data: Vec<u8> },
}

impl Reply {
    pub fn write(self, out: &mut Output) {
        match self {
            Self::Text(text) => *out += &text,
            Self::Blob { stream, data } => out.blob(stream, &data),
        }
    }
}

type Opening = Pin<Box<dyn Future<Output = anyhow::Result<Backend>> + Send>>;

/// The file transfers of a session. Dropping it closes the task's request
/// channel; the task then deletes incomplete uploads and ends.
pub struct Transfers {
    requests: mpsc::Sender<Request>,
    replies: mpsc::UnboundedReceiver<Reply>,
    #[cfg_attr(not(test), allow(dead_code))]
    task: JoinHandle<()>,
}

impl Transfers {
    /// SFTP in `channel`, a session channel of the SSH connection, exposed
    /// once the subsystem started. A server without SFTP leaves the session
    /// without files.
    pub fn sftp(
        channel: russh::Channel<russh::client::Msg>,
        settings: SftpSettings,
        out: &Output,
    ) -> Self {
        let root = settings.root.clone();
        let opening = async move {
            channel.request_subsystem(true, "sftp").await?;
            let session = russh_sftp::client::SftpSession::new(channel.into_stream()).await?;
            Ok(Backend::Sftp(Sftp::new(session, &root).await))
        };
        Self::spawn(Box::pin(opening), settings.policy, "SFTP".to_owned(), out)
    }

    /// The RDP drive's directory, exposed under the drive's name.
    pub fn drive(drive: Drive, out: &Output) -> Self {
        let Drive {
            directory,
            settings,
        } = drive;
        let opening = async move { Ok(Backend::Local(directory)) };
        Self::spawn(Box::pin(opening), settings.policy, settings.name, out)
    }

    fn spawn(opening: Opening, policy: Policy, name: String, out: &Output) -> Self {
        let chunk = chunk_for(out);
        let (requests, requests_rx) = mpsc::channel(MAX_QUEUED);
        let (replies_tx, replies) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut task = Task::new(policy, chunk, replies_tx);
            match opening.await {
                Ok(backend) => {
                    info!(name, ?policy, "file transfer available");
                    task.ready(backend, &name);
                }
                Err(err) => warn!(name, error = %err, "file transfer unavailable"),
            }
            task.run(requests_rx).await;
        });
        Self {
            requests,
            replies,
            task,
        }
    }

    /// Passes a client event on to the task. An upload whose data cannot be
    /// queued is refused.
    pub fn event(&mut self, event: FileEvent, out: &mut Output) {
        let stream = match &event {
            FileEvent::Data { stream, .. } => Some(*stream),
            _ => None,
        };
        if let Err(mpsc::error::TrySendError::Full(_)) =
            self.requests.try_send(Request::Event(event))
            && let Some(stream) = stream
        {
            warn!(stream, "upload data arrives faster than it can be written");
            crate::session::ack(out, stream, "Too much data", Status::CLIENT_OVERRUN);
            let _ = self.requests.try_send(Request::Event(FileEvent::End {
                stream,
                aborted: true,
            }));
        }
    }

    /// Takes the client's acknowledgement of a download's data; `false`
    /// if `stream` is not a download.
    pub fn ack(&mut self, stream: i32, status: Status) -> bool {
        if !(FIRST_DOWNLOAD_STREAM..FIRST_DOWNLOAD_STREAM + MAX_DOWNLOADS).contains(&stream) {
            return false;
        }
        let ok = !status.is_error();
        if self.requests.try_send(Request::Ack { stream, ok }).is_err() {
            warn!(stream, "file transfer requests are piling up");
        }
        true
    }

    /// The client went away ([`crate::resume`]): its uploads are deleted
    /// and its downloads stop. Replies already on their way are dropped
    /// with the rest of what the session sends while detached.
    pub fn detach(&mut self) {
        self.request(Request::Detach);
    }

    /// A client attached, writing to `out`: it is told about the
    /// filesystem.
    pub fn attach(&mut self, out: &Output) {
        while self.replies.try_recv().is_ok() {}
        self.request(Request::Attach {
            chunk: chunk_for(out),
        });
    }

    fn request(&mut self, request: Request) {
        if self.requests.try_send(request).is_err() {
            warn!("file transfer requests are piling up");
        }
    }
}

/// Bytes of file data per blob for a client writing to `out`.
fn chunk_for(out: &Output) -> usize {
    if out.is_binary() {
        molcajete_protocol::binary::MAX_BLOB_DATA
    } else {
        crate::io::BLOB_MAX_BYTES
    }
}

/// The next reply of the session's transfers, if it has any; for
/// `select!` in the session loops (never ready without transfers).
pub async fn next_reply(transfers: &mut Option<Transfers>) -> Reply {
    match transfers {
        Some(transfers) => match transfers.replies.recv().await {
            Some(reply) => reply,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests;
