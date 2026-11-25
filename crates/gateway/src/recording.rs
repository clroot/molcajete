//! Session recording in Guacamole's format.
//!
//! A recording is what guacd writes for `recording-path`: the instruction
//! stream the daemon sent after the handshake, as Guacamole protocol text,
//! with `mouse` (x, y, button mask, timestamp) and, on request, `key`
//! (keysym, pressed, timestamp) instructions of the user in between. The
//! stock player (`guacamole-common-js`'s `SessionRecording`), `guacenc` and
//! our own playback page read it.
//!
//! The gateway records rather than the daemon: it sees the stream of stock
//! guacd and molcajeted alike, owns the index, and molcajeted may run on a
//! host whose disks the gateway cannot reach. What is written is what the
//! browser got. Blob records of the binary tunnel are turned back into
//! base64 `blob` instructions of at most 6048 bytes, as guacd sends them, so
//! a recording does not depend on the tunnel the browser used.
//!
//! molcajeted's own formats (zstd images, video streams) are unknown to the
//! stock tools. Unless a connection asks to keep them
//! ([`attributes::RECORD_MOLCAJETE_FORMATS`]), a recorded session does not
//! announce them in the handshake ([`stock_formats_only`]), so that the
//! daemon sends PNG and JPEG as it would to the stock client.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine as _;
use molcajete_protocol::{RawInstruction, binary::Record};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

use crate::config::RecordingsConfig;
use crate::db::{ConnectionRecord, Database, Recording, attributes, now_millis};
use crate::guacd::ClientInfo;

/// Most raw bytes in one `blob` instruction, as libguac writes them.
pub const MAX_BLOB_BYTES: usize = 6048;

/// File name extension of recordings.
pub const EXTENSION: &str = "guac";

/// How often old recordings are looked for.
const RETENTION_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// Turns the traffic of a tunnel into recording text.
#[derive(Debug)]
pub struct Encoder {
    out: Vec<u8>,
    include_mouse: bool,
    include_keys: bool,
    base64: String,
    /// The start of a blob shorter than [`MAX_BLOB_BYTES`], kept until the
    /// stream's next data, so that blob records of any size become blobs
    /// of the size guacd writes.
    partial: Vec<u8>,
    partial_stream: i32,
}

impl Encoder {
    pub fn new(include_mouse: bool, include_keys: bool) -> Self {
        Self {
            out: Vec::new(),
            include_mouse,
            include_keys,
            base64: String::new(),
            partial: Vec::new(),
            partial_stream: 0,
        }
    }

    /// An instruction from the daemon. The tunnel's own instructions (empty
    /// opcode) are not part of the session.
    pub fn server_instruction(&mut self, raw: &RawInstruction<'_>) {
        if !raw.opcode().is_empty() {
            self.end_blob();
            self.out.extend_from_slice(raw.as_bytes());
        }
    }

    /// A record of the binary framing from molcajeted.
    pub fn server_record(&mut self, record: Record<'_>) {
        match record {
            Record::Instructions(text) => {
                self.end_blob();
                self.out.extend_from_slice(text);
            }
            Record::Blob { stream, data } => self.blob(stream, data),
            Record::Unknown { .. } => {}
        }
    }

    /// Stream data, as the `blob` instructions guacd would have sent:
    /// consecutive data of a stream is cut into blobs of
    /// [`MAX_BLOB_BYTES`], whatever the records it came in.
    pub fn blob(&mut self, stream: i32, mut data: &[u8]) {
        if stream != self.partial_stream {
            self.end_blob();
        }
        if data.is_empty() {
            if self.partial.is_empty() {
                self.write_blob(stream, &[]);
            }
            return;
        }
        self.partial_stream = stream;
        if !self.partial.is_empty() {
            let take = data.len().min(MAX_BLOB_BYTES - self.partial.len());
            let (head, rest) = data.split_at(take);
            self.partial.extend_from_slice(head);
            data = rest;
            if self.partial.len() < MAX_BLOB_BYTES {
                return;
            }
            self.end_blob();
        }
        let (chunks, rest) = data.as_chunks::<MAX_BLOB_BYTES>();
        for chunk in chunks {
            self.write_blob(stream, chunk);
        }
        self.partial.extend_from_slice(rest);
    }

    /// Writes the data held back for the current blob.
    fn end_blob(&mut self) {
        if !self.partial.is_empty() {
            let mut partial = std::mem::take(&mut self.partial);
            self.write_blob(self.partial_stream, &partial);
            partial.clear();
            self.partial = partial;
        }
    }

    fn write_blob(&mut self, stream: i32, data: &[u8]) {
        self.base64.clear();
        base64::engine::general_purpose::STANDARD.encode_string(data, &mut self.base64);
        let stream = stream.to_string();
        let lengths = format!("{}.{stream},{}.", stream.len(), self.base64.len());
        let out = &mut self.out;
        out.extend_from_slice(b"4.blob,");
        out.extend_from_slice(lengths.as_bytes());
        out.extend_from_slice(self.base64.as_bytes());
        out.push(b';');
    }

    /// An instruction from the browser: `mouse` and `key` are kept, with the
    /// time they passed (Unix milliseconds) appended as guacd does.
    pub fn client_instruction(&mut self, raw: &RawInstruction<'_>, now_ms: i64) {
        let builder = match raw.opcode() {
            "mouse" if self.include_mouse && raw.arg_count() >= 3 => {
                molcajete_protocol::InstructionBuilder::new("mouse")
                    .str_arg(raw.arg(0))
                    .str_arg(raw.arg(1))
                    .str_arg(raw.arg(2))
            }
            "key" if self.include_keys && raw.arg_count() >= 2 => {
                molcajete_protocol::InstructionBuilder::new("key")
                    .str_arg(raw.arg(0))
                    .str_arg(raw.arg(1))
            }
            _ => return,
        };
        self.end_blob();
        self.out
            .extend_from_slice(builder.arg(now_ms).finish().as_bytes());
    }

    /// The text encoded since the last call. The start of a blob may be
    /// held back for the data that follows it.
    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.out)
    }

    /// Everything that is left, at the end of the session.
    pub fn finish(&mut self) -> Vec<u8> {
        self.end_blob();
        self.take()
    }
}

/// The client capabilities of a recorded session: without molcajeted's own
/// image and video formats, which the stock tools cannot play.
pub fn stock_formats_only(info: &mut ClientInfo) {
    info.image
        .retain(|mimetype| mimetype != molcajete_protocol::zimage::MIMETYPE);
    info.video.clear();
}

/// Whether sessions of `connection` are recorded under `config`.
pub fn wanted(config: &RecordingsConfig, connection: &ConnectionRecord) -> bool {
    config.enabled && connection.flag(attributes::RECORD)
}

/// The user whose session is recorded.
#[derive(Debug, Clone, Default)]
pub struct RecordedUser {
    pub id: String,
    pub username: String,
}

/// A recording being written. Both directions of the tunnel encode into it
/// and [`Recorder::flush`] hands the text to a writer thread; a full queue
/// slows the tunnel down rather than losing data.
pub struct Recorder {
    entry: Recording,
    path: PathBuf,
    encoder: Mutex<Encoder>,
    tx: mpsc::Sender<Vec<u8>>,
    writer: JoinHandle<u64>,
}

impl Recorder {
    /// Creates the file and its index entry.
    pub async fn start(
        db: &Database,
        directory: &Path,
        connection: &ConnectionRecord,
        user: &RecordedUser,
    ) -> anyhow::Result<Self> {
        let started_at = now_millis();
        let id = uuid::Uuid::new_v4().to_string();
        let file_name = file_name(started_at, &connection.id, &id);
        let path = directory.join(&file_name);
        tokio::fs::create_dir_all(directory).await?;
        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?
            .into_std()
            .await;
        let entry = Recording {
            id,
            connection_id: connection.id.clone(),
            connection_name: connection.name.clone(),
            protocol: connection.protocol.clone(),
            user_id: user.id.clone(),
            username: user.username.clone(),
            file_name,
            include_keys: connection.flag(attributes::RECORD_KEYS),
            started_at,
            ended_at: None,
            size_bytes: 0,
        };
        if let Err(err) = db.create_recording(&entry).await {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(err.into());
        }
        let (tx, rx) = mpsc::channel(64);
        let writer = tokio::task::spawn_blocking({
            let path = path.clone();
            move || write_all(file, rx, &path)
        });
        info!(recording = %entry.id, file = %path.display(), "recording the session");
        Ok(Self {
            encoder: Mutex::new(Encoder::new(
                !connection.flag(attributes::RECORD_EXCLUDE_MOUSE),
                entry.include_keys,
            )),
            entry,
            path,
            tx,
            writer,
        })
    }

    /// Encodes something into the recording (never held across an await).
    pub fn record(&self, f: impl FnOnce(&mut Encoder)) {
        if let Ok(mut encoder) = self.encoder.lock() {
            f(&mut encoder);
        }
    }

    /// Queues what was encoded for writing.
    pub async fn flush(&self) {
        let chunk = match self.encoder.lock() {
            Ok(mut encoder) => encoder.take(),
            Err(_) => return,
        };
        if !chunk.is_empty() {
            // The writer only goes away by panicking, which it reports.
            let _ = self.tx.send(chunk).await;
        }
    }

    /// Writes the rest, closes the file and completes the index entry.
    pub async fn finish(self, db: &Database) {
        let rest = match self.encoder.lock() {
            Ok(mut encoder) => encoder.finish(),
            Err(_) => Vec::new(),
        };
        if !rest.is_empty() {
            let _ = self.tx.send(rest).await;
        }
        let Self {
            entry,
            path,
            tx,
            writer,
            ..
        } = self;
        drop(tx);
        let size = match writer.await {
            Ok(size) => size,
            Err(err) => {
                error!(error = %err, file = %path.display(), "recording writer failed");
                tokio::fs::metadata(&path).await.map_or(0, |m| m.len())
            }
        };
        if let Err(err) = db
            .finish_recording(&entry.id, now_millis(), size as i64)
            .await
        {
            error!(error = %err, recording = %entry.id, "cannot complete the recording index");
        }
        info!(recording = %entry.id, bytes = size, "recording complete");
    }
}

/// The writer thread: appends every chunk and returns the size written. A
/// failed write is logged once and the rest of the session is not written.
fn write_all(file: std::fs::File, mut rx: mpsc::Receiver<Vec<u8>>, path: &Path) -> u64 {
    use std::io::Write as _;

    let mut out = std::io::BufWriter::with_capacity(256 * 1024, file);
    let mut written = 0u64;
    let mut failed = false;
    while let Some(chunk) = rx.blocking_recv() {
        if failed {
            continue;
        }
        match out.write_all(&chunk) {
            Ok(()) => written += chunk.len() as u64,
            Err(err) => {
                error!(error = %err, file = %path.display(), "writing the recording failed; the rest of the session is not recorded");
                failed = true;
            }
        }
    }
    if let Err(err) = out.flush().and_then(|()| out.get_ref().sync_all()) {
        error!(error = %err, file = %path.display(), "closing the recording failed");
    }
    written
}

/// `20261125-143000_<connection>_<id>.guac`: sorted by time in a listing.
fn file_name(started_at: i64, connection: &str, id: &str) -> String {
    let (date, time) = utc(started_at);
    // Connection ids are restricted to [A-Za-z0-9._-]; keep it that way for
    // ids imported from older configurations.
    let connection: String = connection
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{date}-{time}_{connection}_{id}.{EXTENSION}")
}

/// `YYYYMMDD` and `HHMMSS` of Unix milliseconds, in UTC.
fn utc(ms: i64) -> (String, String) {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    (
        format!("{year:04}{month:02}{day:02}"),
        format!("{:02}{:02}{:02}", rem / 3600, rem / 60 % 60, rem % 60),
    )
}

/// The file of a recording, if its name is one the gateway could have
/// written (never a path out of the directory).
pub fn file_path(directory: &Path, recording: &Recording) -> Option<PathBuf> {
    let name = &recording.file_name;
    let plain = !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    plain.then(|| directory.join(name))
}

/// Deletes a recording's file and index entry.
pub async fn delete(db: &Database, directory: &Path, recording: &Recording) -> anyhow::Result<()> {
    if let Some(path) = file_path(directory, recording) {
        match tokio::fs::remove_file(&path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    match db.delete_recording(&recording.id).await {
        Ok(()) | Err(crate::db::DbError::NotFound) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

/// Deletes the recordings that retention no longer keeps: those that ended
/// more than `max_age_days` ago, and the oldest ones beyond
/// `max_total_size`. Sessions still being recorded are left alone. Returns
/// how many were deleted.
pub async fn enforce_retention(
    db: &Database,
    config: &RecordingsConfig,
    directory: &Path,
    now_ms: i64,
) -> anyhow::Result<usize> {
    if config.max_age_days == 0 && config.max_total_size == 0 {
        return Ok(0);
    }
    let cutoff = (config.max_age_days > 0)
        .then(|| now_ms.saturating_sub(config.max_age_days as i64 * 86_400_000));
    let mut kept = 0u64;
    let mut over = false;
    let mut deleted = 0;
    // Newest first: once the limit is passed, everything older goes.
    for recording in db.list_recordings().await? {
        let Some(ended_at) = recording.ended_at else {
            continue;
        };
        let size = recording.size_bytes.max(0) as u64;
        over |= config.max_total_size > 0 && kept + size > config.max_total_size;
        let expired = cutoff.is_some_and(|cutoff| ended_at < cutoff);
        if over || expired {
            delete(db, directory, &recording).await?;
            info!(
                recording = %recording.id,
                file = %recording.file_name,
                reason = if expired { "age" } else { "size" },
                "deleted an old recording"
            );
            deleted += 1;
        } else {
            kept += size;
        }
    }
    Ok(deleted)
}

/// Completes the index entries of recordings that were open when the
/// gateway stopped, from their files.
pub async fn finish_interrupted(db: &Database, directory: &Path) -> anyhow::Result<()> {
    for recording in db.unfinished_recordings().await? {
        let metadata = match file_path(directory, &recording) {
            Some(path) => tokio::fs::metadata(path).await.ok(),
            None => None,
        };
        let size = metadata.as_ref().map_or(0, |m| m.len() as i64);
        let ended_at = metadata
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(recording.started_at, |d| d.as_millis() as i64);
        db.finish_recording(&recording.id, ended_at.max(recording.started_at), size)
            .await?;
        warn!(recording = %recording.id, "completed a recording interrupted by a gateway restart");
    }
    Ok(())
}

/// Prepares the recordings directory and runs retention now and then.
pub async fn start_maintenance(db: Database, config: RecordingsConfig, directory: PathBuf) {
    if let Err(err) = tokio::fs::create_dir_all(&directory).await {
        warn!(error = %err, dir = %directory.display(), "cannot create the recordings directory");
    }
    if let Err(err) = finish_interrupted(&db, &directory).await {
        warn!(error = %err, "cannot complete interrupted recordings");
    }
    if config.max_age_days == 0 && config.max_total_size == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_INTERVAL);
        loop {
            interval.tick().await;
            if let Err(err) = enforce_retention(&db, &config, &directory, now_millis()).await {
                warn!(error = %err, "recording retention failed");
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use molcajete_protocol::{Parser, binary::RecordWriter};

    fn raw_all(text: &str, mut f: impl FnMut(&RawInstruction<'_>)) {
        let mut parser = Parser::new();
        parser.feed(text.as_bytes());
        while let Some(raw) = parser.next_raw().unwrap() {
            f(&raw);
        }
    }

    #[test]
    fn keeps_server_text_without_tunnel_instructions() {
        let mut encoder = Encoder::new(true, false);
        raw_all(
            "0.,36.00000000-0000-0000-0000-000000000000;4.size,1.0,3.640,3.480;4.sync,2.42,1.1;",
            |raw| encoder.server_instruction(raw),
        );
        assert_eq!(encoder.take(), b"4.size,1.0,3.640,3.480;4.sync,2.42,1.1;");
        assert!(encoder.take().is_empty());
    }

    #[test]
    fn writes_blob_records_as_guacd_blobs() {
        let data: Vec<u8> = (0..15_000u32).map(|i| (i * 7) as u8).collect();
        let mut records = RecordWriter::new();
        records.instructions(b"3.img,1.3,2.14,1.0,9.image/png,1.0,1.0;");
        records.blob(3, &data);
        records.instructions(b"3.end,1.3;");
        let bytes = records.take();

        let mut encoder = Encoder::new(true, false);
        for record in molcajete_protocol::binary::records(&bytes) {
            encoder.server_record(record.unwrap());
        }
        encoder.blob(4, &[]);
        let text = String::from_utf8(encoder.take()).unwrap();
        let instructions = molcajete_protocol::parse_all(&text).unwrap();
        let opcodes: Vec<_> = instructions.iter().map(|i| i.opcode.as_str()).collect();
        assert_eq!(opcodes, ["img", "blob", "blob", "blob", "end", "blob"]);
        let mut decoded = Vec::new();
        for blob in &instructions[1..4] {
            assert_eq!(blob.args[0], "3");
            let chunk = base64::engine::general_purpose::STANDARD
                .decode(&blob.args[1])
                .unwrap();
            assert!(chunk.len() <= MAX_BLOB_BYTES);
            decoded.extend(chunk);
        }
        assert_eq!(decoded, data);
        assert_eq!(instructions[5].args, ["4", ""]);
        // Within the element limit of every Guacamole parser.
        assert!(
            instructions
                .iter()
                .all(|i| i.args.iter().all(|a| a.len() <= 8192))
        );
    }

    #[test]
    fn records_mouse_by_default_and_keys_only_when_asked() {
        let input = "5.mouse,2.10,2.20,1.1;3.key,5.65307,1.1;4.sync,2.42;4.size,4.1024,3.768;";
        let mut encoder = Encoder::new(true, false);
        raw_all(input, |raw| encoder.client_instruction(raw, 1234));
        assert_eq!(encoder.take(), b"5.mouse,2.10,2.20,1.1,4.1234;");

        let mut encoder = Encoder::new(false, true);
        raw_all(input, |raw| encoder.client_instruction(raw, 1234));
        assert_eq!(encoder.take(), b"3.key,5.65307,1.1,4.1234;");
    }

    #[test]
    fn stock_formats_drop_zstd_and_video() {
        let mut info = ClientInfo {
            image: vec![
                "image/png".into(),
                molcajete_protocol::zimage::MIMETYPE.into(),
                "image/jpeg".into(),
            ],
            video: vec!["video/h264".into()],
            ..ClientInfo::default()
        };
        stock_formats_only(&mut info);
        assert_eq!(info.image, ["image/png", "image/jpeg"]);
        assert!(info.video.is_empty());
    }

    #[test]
    fn names_files_by_time_and_connection() {
        // 2026-11-25 14:30:05 UTC.
        let name = file_name(1_795_617_005_123, "rdp/../x", "abc");
        assert_eq!(name, "20261125-143005_rdp_.._x_abc.guac");
        assert_eq!(utc(0), ("19700101".into(), "000000".into()));
        assert_eq!(utc(951_782_400_000).0, "20000229");
    }

    const DAY: i64 = 86_400_000;

    fn entry(id: &str, ended_days_ago: Option<i64>, size: i64, now: i64) -> Recording {
        let ended_at = ended_days_ago.map(|days| now - days * DAY);
        Recording {
            id: id.into(),
            connection_id: "c".into(),
            connection_name: "c".into(),
            protocol: "vnc".into(),
            user_id: "u".into(),
            username: "u".into(),
            file_name: format!("{id}.guac"),
            include_keys: false,
            started_at: ended_at.unwrap_or(now) - 1000,
            ended_at,
            size_bytes: size,
        }
    }

    /// Recordings `(id, ended days ago, size)`, each with a file, newest
    /// first after the call.
    async fn index(db: &Database, dir: &Path, now: i64, recordings: &[(&str, Option<i64>, i64)]) {
        for &(id, ended, size) in recordings {
            db.create_recording(&entry(id, ended, size, now))
                .await
                .unwrap();
            std::fs::write(dir.join(format!("{id}.guac")), vec![b'x'; size as usize]).unwrap();
        }
    }

    async fn remaining(db: &Database, dir: &Path) -> Vec<String> {
        let ids: Vec<String> = db
            .list_recordings()
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.id)
            .collect();
        for id in &ids {
            assert!(dir.join(format!("{id}.guac")).exists(), "{id}");
        }
        ids
    }

    #[tokio::test]
    async fn retention_deletes_by_age_and_total_size() {
        let (db, _db_dir) = crate::db::tests::sqlite().await;
        let dir = tempfile::tempdir().unwrap();
        let now = 1_800_000_000_000;
        let recordings = [
            ("live", None, 0),
            ("new", Some(1), 300),
            ("week", Some(7), 300),
            ("month", Some(31), 300),
            ("year", Some(365), 10),
        ];
        index(&db, dir.path(), now, &recordings).await;

        let mut config = RecordingsConfig::default();
        assert_eq!(
            enforce_retention(&db, &config, dir.path(), now)
                .await
                .unwrap(),
            0
        );
        config.max_age_days = 30;
        assert_eq!(
            enforce_retention(&db, &config, dir.path(), now)
                .await
                .unwrap(),
            2
        );
        assert!(!dir.path().join("month.guac").exists());
        assert_eq!(remaining(&db, dir.path()).await, ["live", "new", "week"]);

        // By size, the oldest go first, even when a smaller one would fit.
        let (db, _db_dir) = crate::db::tests::sqlite().await;
        let dir = tempfile::tempdir().unwrap();
        index(&db, dir.path(), now, &recordings).await;
        let config = RecordingsConfig {
            max_total_size: 650,
            ..RecordingsConfig::default()
        };
        assert_eq!(
            enforce_retention(&db, &config, dir.path(), now)
                .await
                .unwrap(),
            2
        );
        assert_eq!(remaining(&db, dir.path()).await, ["live", "new", "week"]);
    }

    #[tokio::test]
    async fn completes_recordings_interrupted_by_a_restart() {
        let (db, _db_dir) = crate::db::tests::sqlite().await;
        let dir = tempfile::tempdir().unwrap();
        index(&db, dir.path(), now_millis(), &[("cut", None, 123)]).await;
        finish_interrupted(&db, dir.path()).await.unwrap();
        let cut = db.recording("cut").await.unwrap();
        assert_eq!(cut.size_bytes, 123);
        assert!(cut.ended_at.unwrap() >= cut.started_at);
        assert!(db.unfinished_recordings().await.unwrap().is_empty());
    }

    #[test]
    fn refuses_file_names_outside_the_directory() {
        let mut recording = Recording {
            id: "x".into(),
            connection_id: String::new(),
            connection_name: String::new(),
            protocol: String::new(),
            user_id: String::new(),
            username: String::new(),
            file_name: "a.guac".into(),
            include_keys: false,
            started_at: 0,
            ended_at: None,
            size_bytes: 0,
        };
        let dir = Path::new("/rec");
        assert_eq!(
            file_path(dir, &recording),
            Some(PathBuf::from("/rec/a.guac"))
        );
        for bad in ["../a.guac", "/etc/passwd", "..", "", "a/b.guac"] {
            recording.file_name = bad.into();
            assert_eq!(file_path(dir, &recording), None, "{bad}");
        }
    }
}
