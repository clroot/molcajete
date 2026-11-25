//! The transfers of a session against a drive directory, as the client
//! sees them.

use std::path::PathBuf;
use std::time::Duration;

use molcajete_protocol::{Instruction, Status, parse_all};

use super::*;

struct Fixture {
    dir: PathBuf,
    transfers: Transfers,
    out: Output,
}

impl Fixture {
    async fn new(policy: Policy) -> Self {
        let dir = std::env::temp_dir().join(format!("molcajete-drive-{}", uuid::Uuid::new_v4()));
        let settings = DriveSettings {
            path: dir.clone(),
            name: "Drive".into(),
            create: true,
            policy,
        };
        let drive = Drive::open(settings).await.unwrap();
        let out = Output::text();
        let transfers = Transfers::drive(drive, &out);
        let mut fixture = Self {
            dir,
            transfers,
            out,
        };
        let filesystem = fixture.next().await;
        assert_eq!(filesystem.opcode, "filesystem");
        let flags = |on: bool| if on { "1" } else { "0" };
        assert_eq!(
            filesystem.args,
            ["0", "Drive", flags(policy.download), flags(policy.upload)]
        );
        fixture
    }

    /// The next instruction the task sends.
    async fn next(&mut self) -> Instruction {
        loop {
            if !self.out.is_empty() {
                let text = self.out.to_string();
                let mut all = parse_all(&text).unwrap();
                let first = all.remove(0);
                self.out.clear();
                for rest in all {
                    self.out += &rest.encode();
                }
                return first;
            }
            let reply = tokio::time::timeout(Duration::from_secs(5), self.transfers.replies.recv())
                .await
                .expect("a reply")
                .expect("the task is running");
            reply.write(&mut self.out);
        }
    }

    fn send(&mut self, event: FileEvent) {
        let mut out = Output::text();
        self.transfers.event(event, &mut out);
        assert!(out.is_empty(), "{out}");
    }

    fn ack(&mut self, stream: i32, status: Status) {
        assert!(self.transfers.ack(stream, status));
    }

    async fn upload(&mut self, stream: i32, object: Option<i32>, name: &str, data: &[u8]) {
        self.send(FileEvent::Open {
            stream,
            object,
            name: name.into(),
        });
        assert_ack(&self.next().await, stream, Status::SUCCESS);
        for chunk in data.chunks(1000) {
            self.send(FileEvent::Data {
                stream,
                data: chunk.to_vec(),
            });
            assert_ack(&self.next().await, stream, Status::SUCCESS);
        }
        self.send(FileEvent::End {
            stream,
            aborted: false,
        });
    }

    /// Lists or downloads `name`: its mimetype and data.
    async fn get(&mut self, name: &str) -> (String, Vec<u8>) {
        self.send(FileEvent::Get {
            object: OBJECT,
            name: name.into(),
        });
        let body = self.next().await;
        assert_eq!(body.opcode, "body", "{body}");
        assert_eq!(body.arg(3), name);
        let stream: i32 = body.arg(1).parse().unwrap();
        self.ack(stream, Status::SUCCESS);
        let mut data = Vec::new();
        loop {
            let ins = self.next().await;
            assert_eq!(ins.arg(0), stream.to_string(), "{ins}");
            match ins.opcode.as_str() {
                "blob" => {
                    let b64 = base64::engine::general_purpose::STANDARD;
                    data.extend(base64::Engine::decode(&b64, ins.arg(1)).unwrap());
                    self.ack(stream, Status::SUCCESS);
                }
                "end" => return (body.arg(2).to_owned(), data),
                other => panic!("unexpected {other}"),
            }
        }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn assert_ack(ins: &Instruction, stream: i32, status: Status) {
    assert_eq!(ins.opcode, "ack", "{ins}");
    assert_eq!(ins.arg(0), stream.to_string(), "{ins}");
    assert_eq!(ins.arg(2), status.code().to_string(), "{ins}");
}

const OPEN: Policy = Policy {
    download: true,
    upload: true,
    max_size: None,
};

/// More than the window, in blobs of the text protocol.
fn large_file() -> Vec<u8> {
    (0..2_000_000u32).map(|i| (i % 251) as u8).collect()
}

#[tokio::test]
async fn uploads_lists_and_downloads() {
    let mut f = Fixture::new(OPEN).await;
    let data = large_file();
    f.upload(1, Some(OBJECT), "/big.bin", &data).await;
    f.upload(2, None, "../notes.txt", b"hello").await;
    std::fs::create_dir(f.path("sub")).unwrap();

    let (mimetype, index) = f.get("/").await;
    assert_eq!(mimetype, path::INDEX_MIMETYPE);
    let index: serde_json::Value = serde_json::from_slice(&index).unwrap();
    assert_eq!(
        index,
        serde_json::json!({
            "/big.bin": format!("application/octet-stream; size={}", data.len()),
            "/notes.txt": "application/octet-stream; size=5",
            "/sub": path::INDEX_MIMETYPE,
        })
    );

    let (mimetype, downloaded) = f.get("/big.bin").await;
    assert_eq!(
        mimetype,
        format!("application/octet-stream; size={}", data.len())
    );
    assert_eq!(downloaded.len(), data.len());
    assert!(downloaded == data);
    assert_eq!(std::fs::read(f.path("notes.txt")).unwrap(), b"hello");
}

#[tokio::test]
async fn downloads_keep_a_window_of_unacknowledged_blobs() {
    let mut f = Fixture::new(OPEN).await;
    std::fs::write(f.path("big.bin"), large_file()).unwrap();
    f.send(FileEvent::Get {
        object: OBJECT,
        name: "/big.bin".into(),
    });
    let body = f.next().await;
    let stream: i32 = body.arg(1).parse().unwrap();
    // Nothing before the client accepts the stream.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(f.transfers.replies.try_recv().is_err());
    f.ack(stream, Status::SUCCESS);
    let mut blobs = 0;
    while let Ok(Some(reply)) =
        tokio::time::timeout(Duration::from_millis(200), f.transfers.replies.recv()).await
    {
        assert!(matches!(reply, Reply::Blob { .. }));
        blobs += 1;
    }
    let window = 512 * 1024 / crate::io::BLOB_MAX_BYTES;
    assert_eq!(blobs, window);
    // Each acknowledgement lets one more go.
    f.ack(stream, Status::SUCCESS);
    let reply = f.transfers.replies.recv().await.unwrap();
    assert!(matches!(reply, Reply::Blob { stream: s, .. } if s == stream));
    // A refusal stops it.
    f.ack(stream, Status::CLIENT_FORBIDDEN);
    tokio::time::sleep(Duration::from_millis(50)).await;
    f.ack(stream, Status::SUCCESS);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(f.transfers.replies.try_recv().is_err());
}

#[tokio::test]
async fn paths_outside_the_drive_are_refused() {
    let mut f = Fixture::new(OPEN).await;
    f.send(FileEvent::Open {
        stream: 3,
        object: Some(OBJECT),
        name: "/../escape.txt".into(),
    });
    assert_ack(&f.next().await, 3, Status::CLIENT_BAD_REQUEST);
    // Its data is dropped without further answers.
    f.send(FileEvent::Data {
        stream: 3,
        data: b"x".to_vec(),
    });
    f.send(FileEvent::Get {
        object: OBJECT,
        name: "/../..".into(),
    });
    let error = f.next().await;
    assert_eq!(error.opcode, "get-error");
    assert_eq!(error.arg(2), Status::CLIENT_BAD_REQUEST.code().to_string());
    f.send(FileEvent::Get {
        object: OBJECT,
        name: "/missing".into(),
    });
    let error = f.next().await;
    assert_eq!(
        (error.opcode.as_str(), error.arg(1), error.arg(2)),
        (
            "get-error",
            "/missing",
            Status::RESOURCE_NOT_FOUND.code().to_string().as_str()
        )
    );
}

#[tokio::test]
async fn disabled_directions_are_refused() {
    let mut f = Fixture::new(Policy {
        download: false,
        upload: false,
        max_size: None,
    })
    .await;
    std::fs::write(f.path("a.txt"), b"a").unwrap();
    f.send(FileEvent::Open {
        stream: 1,
        object: None,
        name: "b.txt".into(),
    });
    assert_ack(&f.next().await, 1, Status::CLIENT_FORBIDDEN);
    assert!(!f.path("b.txt").exists());
    // Listing still works, downloading does not.
    let (_, index) = f.get("/").await;
    assert!(String::from_utf8(index).unwrap().contains("/a.txt"));
    f.send(FileEvent::Get {
        object: OBJECT,
        name: "/a.txt".into(),
    });
    let error = f.next().await;
    assert_eq!(error.opcode, "get-error");
    assert_eq!(error.arg(2), Status::CLIENT_FORBIDDEN.code().to_string());
}

#[tokio::test]
async fn files_over_the_size_limit_are_refused_and_removed() {
    let mut f = Fixture::new(Policy {
        max_size: Some(1024 * 1024),
        ..OPEN
    })
    .await;
    f.send(FileEvent::Open {
        stream: 1,
        object: Some(OBJECT),
        name: "/big.bin".into(),
    });
    assert_ack(&f.next().await, 1, Status::SUCCESS);
    let chunk = vec![7u8; 600 * 1024];
    f.send(FileEvent::Data {
        stream: 1,
        data: chunk.clone(),
    });
    assert_ack(&f.next().await, 1, Status::SUCCESS);
    f.send(FileEvent::Data {
        stream: 1,
        data: chunk,
    });
    assert_ack(&f.next().await, 1, Status::CLIENT_OVERRUN);
    f.send(FileEvent::End {
        stream: 1,
        aborted: false,
    });
    // The partial file is gone.
    let (_, index) = f.get("/").await;
    assert_eq!(index, b"{}");

    std::fs::write(f.path("large.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();
    f.send(FileEvent::Get {
        object: OBJECT,
        name: "/large.bin".into(),
    });
    let error = f.next().await;
    assert_eq!(error.arg(2), Status::CLIENT_OVERRUN.code().to_string());
}

#[tokio::test]
async fn incomplete_uploads_are_removed_when_the_session_ends() {
    let mut f = Fixture::new(OPEN).await;
    let dir = f.dir.clone();
    let mut out = Output::text();
    let (requests, _) = mpsc::channel(1);
    let (_, replies) = mpsc::unbounded_channel();
    let mut transfers = std::mem::replace(
        &mut f.transfers,
        Transfers {
            requests,
            replies,
            task: tokio::spawn(async {}),
        },
    );
    transfers.event(
        FileEvent::Open {
            stream: 1,
            object: None,
            name: "partial.bin".into(),
        },
        &mut out,
    );
    transfers.event(
        FileEvent::Data {
            stream: 1,
            data: vec![1; 100],
        },
        &mut out,
    );
    // Both acknowledged: the file exists.
    for _ in 0..2 {
        transfers.replies.recv().await.unwrap();
    }
    assert!(dir.join("partial.bin").exists());
    let task = std::mem::replace(&mut transfers.task, tokio::spawn(async {}));
    drop(transfers);
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert!(!dir.join("partial.bin").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn settings_follow_guacd() {
    let params = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        }
    };
    assert_eq!(SftpSettings::from_params(params(&[])), None);
    assert_eq!(
        SftpSettings::from_params(params(&[
            ("enable-sftp", "true"),
            ("sftp-disable-upload", "true"),
            ("max-file-size", "10"),
        ])),
        Some(SftpSettings {
            root: "/".into(),
            policy: Policy {
                download: true,
                upload: false,
                max_size: Some(10 * 1024 * 1024),
            },
        })
    );
    // A drive needs a directory.
    assert_eq!(
        DriveSettings::from_params(params(&[("enable-drive", "true")])),
        None
    );
    assert_eq!(
        DriveSettings::from_params(params(&[
            ("enable-drive", "true"),
            ("drive-path", "/srv/drives/a"),
            ("disable-download", "true"),
            ("max-file-size", "0"),
        ])),
        Some(DriveSettings {
            path: "/srv/drives/a".into(),
            name: DEFAULT_DRIVE_NAME.into(),
            create: false,
            policy: Policy {
                download: false,
                upload: true,
                max_size: None,
            },
        })
    );
}
