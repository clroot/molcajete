//! The task that serves a session's file streams, so that slow disks and
//! SFTP round trips never hold up the session loop.

use std::collections::{HashMap, HashSet};
use std::io;

use molcajete_protocol::{InstructionBuilder, Status};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::backend::{Backend, Reader, Writer};
use super::path::{self, Kind, VirtualPath};
use super::{FileEvent, OBJECT, Policy, Reply};

/// Stream indexes of downloads and listings; molcajeted's other streams
/// (clipboard, video, audio) use indexes below these.
pub const FIRST_DOWNLOAD_STREAM: i32 = 1000;
/// Downloads and listings one session may have open at once.
pub const MAX_DOWNLOADS: i32 = 64;
/// Data of a download on its way to the client, not yet acknowledged.
/// More keeps a fast link busier; less keeps the screen updates that
/// share the connection from waiting behind file data on a slow one.
const WINDOW_BYTES: usize = 512 * 1024;

/// What the session loop asks of the task.
pub enum Request {
    Event(FileEvent),
    /// The client acknowledged (or refused) data of a download.
    Ack {
        stream: i32,
        ok: bool,
    },
    /// The client went away ([`crate::resume`]): its transfers are over.
    Detach,
    /// A client attached, whose blobs carry `chunk` bytes: it is told
    /// about the filesystem.
    Attach {
        chunk: usize,
    },
}

struct Upload {
    writer: Writer,
    path: VirtualPath,
    written: u64,
}

enum Source {
    Reader(Reader),
    Memory(std::io::Cursor<Vec<u8>>),
}

impl Source {
    /// Reads until `buf` is full or the data ends.
    async fn fill(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut filled = 0;
        while filled < buf.len() {
            let n = match self {
                Self::Reader(reader) => reader.read(&mut buf[filled..]).await?,
                Self::Memory(cursor) => std::io::Read::read(cursor, &mut buf[filled..])?,
            };
            if n == 0 {
                break;
            }
            filled += n;
        }
        Ok(filled)
    }
}

struct Download {
    source: Source,
    name: String,
    /// The client accepted the stream (its first `ack`).
    started: bool,
    /// Blobs sent and not yet acknowledged.
    in_flight: usize,
}

pub struct Task {
    backend: Option<Backend>,
    policy: Policy,
    replies: mpsc::UnboundedSender<Reply>,
    /// Blobs of a download that may be unacknowledged.
    window: usize,
    uploads: HashMap<i32, Upload>,
    /// Uploads refused or failed, whose remaining data is dropped.
    refused: HashSet<i32>,
    downloads: HashMap<i32, Download>,
    /// The filesystem's name, once it is ready.
    name: String,
    /// One blob's worth: each blob is one `blob` instruction or one record,
    /// so the client acknowledges each with one `ack`.
    buffer: Vec<u8>,
}

impl Task {
    pub fn new(policy: Policy, chunk: usize, replies: mpsc::UnboundedSender<Reply>) -> Self {
        Self {
            backend: None,
            policy,
            replies,
            window: (WINDOW_BYTES / chunk).max(1),
            uploads: HashMap::new(),
            refused: HashSet::new(),
            downloads: HashMap::new(),
            name: String::new(),
            buffer: vec![0; chunk],
        }
    }

    /// Exposes `backend` to the client as the filesystem `name`.
    pub fn ready(&mut self, backend: Backend, name: &str) {
        self.backend = Some(backend);
        self.name = name.to_owned();
        self.announce();
    }

    /// Tells the client about the filesystem.
    fn announce(&self) {
        let name = &self.name;
        self.send(
            InstructionBuilder::new("filesystem")
                .arg(OBJECT)
                .str_arg(name)
                .arg(u8::from(self.policy.download))
                .arg(u8::from(self.policy.upload))
                .finish(),
        );
    }

    pub async fn run(mut self, mut requests: mpsc::Receiver<Request>) {
        while let Some(request) = requests.recv().await {
            match request {
                Request::Event(FileEvent::Open {
                    stream,
                    object,
                    name,
                }) => self.open_upload(stream, object, &name).await,
                Request::Event(FileEvent::Data { stream, data }) => {
                    self.upload_data(stream, &data).await
                }
                Request::Event(FileEvent::End { stream, aborted }) => {
                    self.end_upload(stream, aborted).await
                }
                Request::Event(FileEvent::Get { object, name }) => self.get(object, &name).await,
                Request::Ack { stream, ok } => self.download_ack(stream, ok).await,
                Request::Detach => self.detach().await,
                Request::Attach { chunk } => {
                    self.window = (WINDOW_BYTES / chunk).max(1);
                    self.buffer = vec![0; chunk];
                    if self.backend.is_some() {
                        self.announce();
                    }
                }
            }
        }
        // The session is over: what was being uploaded is incomplete.
        for (_, upload) in self.uploads.drain() {
            discard(self.backend.as_ref(), upload).await;
        }
    }

    /// Ends the transfers of a client that went away: incomplete uploads
    /// are deleted, as at the end of a session, and downloads stop.
    async fn detach(&mut self) {
        let (uploads, downloads) = (self.uploads.len(), self.downloads.len());
        if uploads + downloads > 0 {
            info!(
                uploads,
                downloads, "the client went away; its transfers are aborted"
            );
        }
        for (_, upload) in self.uploads.drain() {
            discard(self.backend.as_ref(), upload).await;
        }
        self.downloads.clear();
        self.refused.clear();
    }

    fn send(&self, text: String) {
        let _ = self.replies.send(Reply::Text(text));
    }

    fn ack_reply(&self, stream: i32, message: &str, status: Status) -> Reply {
        Reply::Text(
            InstructionBuilder::new("ack")
                .arg(stream)
                .str_arg(message)
                .arg(status.code())
                .finish(),
        )
    }

    fn refuse_upload(&mut self, stream: i32, message: &str, status: Status) {
        self.refused.insert(stream);
        let _ = self.replies.send(self.ack_reply(stream, message, status));
    }

    async fn open_upload(&mut self, stream: i32, object: Option<i32>, name: &str) {
        let Some(backend) = &self.backend else {
            return self.refuse_upload(stream, "No filesystem", Status::RESOURCE_NOT_FOUND);
        };
        if !self.policy.upload {
            return self.refuse_upload(stream, "Uploads are disabled", Status::CLIENT_FORBIDDEN);
        }
        let path = match object {
            Some(OBJECT) => VirtualPath::parse(name).filter(|p| !p.is_root()),
            Some(_) => {
                return self.refuse_upload(
                    stream,
                    "No such filesystem",
                    Status::RESOURCE_NOT_FOUND,
                );
            }
            None => VirtualPath::file_name(name).map(|n| backend.upload_directory().join(&n)),
        };
        let Some(path) = path else {
            return self.refuse_upload(stream, "Invalid file name", Status::CLIENT_BAD_REQUEST);
        };
        match backend.create(&path).await {
            Ok(writer) => {
                info!(path = %path, "upload started");
                self.uploads.insert(
                    stream,
                    Upload {
                        writer,
                        path,
                        written: 0,
                    },
                );
                self.refused.remove(&stream);
                let _ = self
                    .replies
                    .send(self.ack_reply(stream, "OK", Status::SUCCESS));
            }
            Err(err) => {
                warn!(path = %path, error = %err, "cannot create the uploaded file");
                let (message, status) = io_status(&err);
                self.refuse_upload(stream, message, status);
            }
        }
    }

    async fn upload_data(&mut self, stream: i32, data: &[u8]) {
        let Some(upload) = self.uploads.get_mut(&stream) else {
            if !self.refused.contains(&stream) {
                let _ = self.replies.send(self.ack_reply(
                    stream,
                    "No such stream",
                    Status::CLIENT_BAD_REQUEST,
                ));
            }
            return;
        };
        let size = upload.written + data.len() as u64;
        let failure = if self.policy.max_size.is_some_and(|max| size > max) {
            Some(("The file is too large", Status::CLIENT_OVERRUN))
        } else {
            match upload.writer.write_all(data).await {
                Ok(()) => None,
                Err(err) => {
                    warn!(path = %upload.path, error = %err, "upload write failed");
                    Some(io_status(&err))
                }
            }
        };
        match failure {
            None => {
                upload.written = size;
                let _ = self
                    .replies
                    .send(self.ack_reply(stream, "OK", Status::SUCCESS));
            }
            Some((message, status)) => {
                if let Some(upload) = self.uploads.remove(&stream) {
                    discard(self.backend.as_ref(), upload).await;
                }
                self.refuse_upload(stream, message, status);
            }
        }
    }

    async fn end_upload(&mut self, stream: i32, aborted: bool) {
        self.refused.remove(&stream);
        let Some(mut upload) = self.uploads.remove(&stream) else {
            return;
        };
        if aborted {
            return discard(self.backend.as_ref(), upload).await;
        }
        match upload.writer.shutdown().await {
            Ok(()) => info!(path = %upload.path, bytes = upload.written, "upload finished"),
            Err(err) => warn!(path = %upload.path, error = %err, "upload could not be completed"),
        }
    }

    fn get_error(&self, name: &str, message: &str, status: Status) {
        self.send(
            InstructionBuilder::new("get-error")
                .arg(OBJECT)
                .str_arg(name)
                .arg(status.code())
                .str_arg(message)
                .finish(),
        );
    }

    async fn get(&mut self, object: i32, name: &str) {
        let Some(backend) = &self.backend else {
            return self.get_error(name, "No filesystem", Status::RESOURCE_NOT_FOUND);
        };
        if object != OBJECT {
            return self.get_error(name, "No such filesystem", Status::RESOURCE_NOT_FOUND);
        }
        let Some(path) = VirtualPath::parse(name) else {
            return self.get_error(name, "Invalid path", Status::CLIENT_BAD_REQUEST);
        };
        let Some(stream) = (FIRST_DOWNLOAD_STREAM..FIRST_DOWNLOAD_STREAM + MAX_DOWNLOADS)
            .find(|s| !self.downloads.contains_key(s))
        else {
            return self.get_error(name, "Too many transfers", Status::CLIENT_TOO_MANY);
        };
        let opened = match backend.stat(&path).await {
            Ok(Kind::Directory) => backend.list(&path).await.map(|entries| {
                let index = path::index(&path, &entries);
                (
                    Source::Memory(std::io::Cursor::new(index)),
                    path::INDEX_MIMETYPE.to_owned(),
                )
            }),
            Ok(Kind::File(_)) if !self.policy.download => {
                return self.get_error(name, "Downloads are disabled", Status::CLIENT_FORBIDDEN);
            }
            Ok(Kind::File(size)) if self.policy.max_size.is_some_and(|max| size > max) => {
                return self.get_error(name, "The file is too large", Status::CLIENT_OVERRUN);
            }
            Ok(Kind::File(size)) => backend
                .open(&path)
                .await
                .map(|reader| (Source::Reader(reader), path::file_mimetype(size))),
            Err(err) => Err(err),
        };
        let (source, mimetype) = match opened {
            Ok(opened) => opened,
            Err(err) => {
                debug!(path = %path, error = %err, "get failed");
                let (message, status) = io_status(&err);
                return self.get_error(name, message, status);
            }
        };
        if mimetype != path::INDEX_MIMETYPE {
            info!(path = %path, "download started");
        }
        self.downloads.insert(
            stream,
            Download {
                source,
                name: name.to_owned(),
                started: false,
                in_flight: 0,
            },
        );
        self.send(
            InstructionBuilder::new("body")
                .arg(OBJECT)
                .arg(stream)
                .str_arg(&mimetype)
                .str_arg(name)
                .finish(),
        );
    }

    async fn download_ack(&mut self, stream: i32, ok: bool) {
        // Taken out while it is worked on, and put back unless it ended.
        let Some(mut download) = self.downloads.remove(&stream) else {
            return;
        };
        if !ok {
            debug!(stream, "download stopped by the client");
            return;
        }
        if download.started {
            download.in_flight = download.in_flight.saturating_sub(1);
        }
        download.started = true;
        while download.in_flight < self.window {
            match download.source.fill(&mut self.buffer).await {
                Ok(0) => {
                    self.send(InstructionBuilder::new("end").arg(stream).finish());
                    return;
                }
                Ok(n) => {
                    download.in_flight += 1;
                    let _ = self.replies.send(Reply::Blob {
                        stream,
                        data: self.buffer[..n].to_vec(),
                    });
                }
                Err(err) => {
                    warn!(name = %download.name, error = %err, "download read failed");
                    let (message, status) = io_status(&err);
                    // The error first, so that the client does not take
                    // the end for a complete file.
                    self.get_error(&download.name, message, status);
                    self.send(InstructionBuilder::new("end").arg(stream).finish());
                    return;
                }
            }
        }
        self.downloads.insert(stream, download);
    }
}

/// Closes and deletes an upload that did not complete.
async fn discard(backend: Option<&Backend>, mut upload: Upload) {
    let _ = upload.writer.shutdown().await;
    drop(upload.writer);
    if let Some(backend) = backend {
        match backend.remove(&upload.path).await {
            Ok(()) => info!(path = %upload.path, "incomplete upload removed"),
            Err(err) => {
                warn!(path = %upload.path, error = %err, "cannot remove an incomplete upload")
            }
        }
    }
}

fn io_status(err: &io::Error) -> (&'static str, Status) {
    match err.kind() {
        io::ErrorKind::NotFound => ("Not found", Status::RESOURCE_NOT_FOUND),
        io::ErrorKind::PermissionDenied => ("Permission denied", Status::CLIENT_FORBIDDEN),
        io::ErrorKind::IsADirectory => ("Is a directory", Status::CLIENT_BAD_REQUEST),
        io::ErrorKind::StorageFull => ("No space left", Status::RESOURCE_CONFLICT),
        io::ErrorKind::TimedOut => ("Timed out", Status::UPSTREAM_TIMEOUT),
        _ => ("Input/output error", Status::SERVER_ERROR),
    }
}
