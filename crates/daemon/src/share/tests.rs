use std::time::Instant as StdInstant;

use molcajete_protocol::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::display::tests::Client;

const ARGS: &[&str] = &["hostname", "read-only"];

/// Noise, so that every image is large and nothing is found as a copy.
fn noise(w: u32, h: u32, seed: u64) -> Vec<u8> {
    let mut state = seed * 2 + 1;
    (0..w * h)
        .flat_map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            [state as u8, (state >> 8) as u8, (state >> 16) as u8, 255]
        })
        .collect()
}

fn owner_display() -> Display {
    Display::new(ImageFormats::default(), true)
        .with_protocol("vnc")
        .with_pacing(Pacing::DESKTOP)
}

/// Takes what changed for `viewer` into its display and client.
fn catch_up(viewer: &Viewer, display: &mut Display, client: &mut Client) -> Pulled {
    let mut out = Output::text();
    let pulled = viewer.pull(display, &mut out);
    display.flush(&mut out);
    client.apply(&out);
    pulled
}

#[tokio::test]
async fn joined_users_get_the_whole_screen_then_the_changes() {
    let owner = Owner::register("$screen", "vnc", ARGS);
    owner
        .run(async {
            let shared = current().expect("owner's connection");
            let mut display = owner_display();
            let mut out = Output::text();
            display.resize(300, 150, &mut out);
            display.put(0, 0, 300, 150, &noise(300, 150, 1));
            display.flush(&mut out);

            // Nobody has joined: the screen is not copied.
            assert!(!shared.has_viewers());
            assert!(shared.lock().screen.pixels.is_empty());

            let first = shared.join();
            assert!(first.setup().is_none(), "waits for the owner");
            // The owner's session is woken with an empty sync.
            display.ack(0, StdInstant::now());
            assert_eq!(
                first.setup(),
                Some(Setup {
                    pacing: Pacing::DESKTOP,
                    lossless: true,
                    video: false
                })
            );
            let mut first_display = Display::new(ImageFormats::default(), true);
            let mut first_client = Client::new();
            assert_eq!(
                catch_up(&first, &mut first_display, &mut first_client),
                Pulled::Changed
            );
            assert!(first_client.pixels == display.framebuffer());

            // Changes reach the user with the owner's next frame.
            display.put(70, 20, 40, 30, &noise(40, 30, 2));
            display.fill(200, 100, 50, 20, [1, 2, 3, 255]);
            assert_eq!(
                catch_up(&first, &mut first_display, &mut first_client),
                Pulled::Nothing
            );
            display.flush(&mut Output::text());
            assert_eq!(
                catch_up(&first, &mut first_display, &mut first_client),
                Pulled::Changed
            );
            assert!(first_client.pixels == display.framebuffer());

            // A second user gets the screen kept for the first at once.
            let second = shared.join();
            assert!(!shared.wants_snapshot());
            let mut second_display = Display::new(ImageFormats::default(), true);
            let mut second_client = Client::new();
            catch_up(&second, &mut second_display, &mut second_client);
            assert!(second_client.pixels == display.framebuffer());

            // A new size reaches both.
            display.resize(320, 160, &mut Output::text());
            display.put(0, 0, 320, 160, &noise(320, 160, 3));
            display.flush(&mut Output::text());
            for (viewer, display_, client) in [
                (&first, &mut first_display, &mut first_client),
                (&second, &mut second_display, &mut second_client),
            ] {
                catch_up(viewer, display_, client);
                assert_eq!(display_.width(), 320);
                assert!(client.pixels == display.framebuffer());
            }

            // The cursor too.
            let cursor = noise(8, 8, 4);
            display.set_cursor(1, 1, 8, 8, &cursor, &mut Output::text());
            let mut out = Output::text();
            assert_eq!(first.pull(&mut first_display, &mut out), Pulled::Changed);
            assert!(out.to_string().contains("6.cursor,1.1,1.1,2.-1"), "{out}");

            // Once everyone left, the copy is dropped.
            drop((first, second));
            assert!(!shared.has_viewers());
            assert!(shared.lock().screen.pixels.is_empty());
        })
        .await;
    drop(owner);
    assert!(connections().get("$screen").is_none());
}

/// A client of a joined user, over TCP.
struct Peer {
    stream: TcpStream,
    parser: Parser,
}

impl Peer {
    /// Joins `id` with these `read-only` value and image types.
    async fn join(id: &'static str, read_only: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (rd, wr) = socket.into_split();
            let mut reader = GuacReader::new(rd);
            let selected = handshake::read_select(&mut reader).await.unwrap();
            join(reader, GuacWriter::new(wr), &selected).await;
        });
        let mut peer = Self {
            stream: TcpStream::connect(addr).await.unwrap(),
            parser: Parser::new(),
        };
        let select = Instruction::new("select", [id]).to_string();
        peer.stream.write_all(select.as_bytes()).await.unwrap();
        let args = peer.next().await.unwrap();
        if args.opcode == "error" {
            return peer.with_error(args);
        }
        assert_eq!(
            args.args[1..],
            ["hostname", "read-only", "molcajete-binary"]
        );
        let connect = format!(
            "4.size,3.640,3.480,2.96;5.image,9.image/png;7.connect,13.VERSION_1_5_0,0.,{},0.;",
            if read_only { "4.true" } else { "5.false" }
        );
        peer.stream.write_all(connect.as_bytes()).await.unwrap();
        let ready = peer.next().await.unwrap();
        assert_eq!((ready.opcode.as_str(), ready.arg(0)), ("ready", id));
        peer
    }

    fn with_error(mut self, error: Instruction) -> Self {
        self.parser = Parser::new();
        self.parser.feed(error.to_string().as_bytes());
        self
    }

    /// The next instruction, or `None` when the connection closed or
    /// nothing came within a second.
    async fn next(&mut self) -> Option<Instruction> {
        let mut buf = [0u8; 65536];
        loop {
            if let Some(ins) = self.parser.next_instruction().unwrap() {
                return Some(ins);
            }
            let read = tokio::time::timeout(Duration::from_secs(1), self.stream.read(&mut buf));
            match read.await {
                Ok(Ok(n)) if n > 0 => self.parser.feed(&buf[..n]),
                _ => return None,
            }
        }
    }

    async fn send(&mut self, text: &str) {
        self.stream.write_all(text.as_bytes()).await.unwrap();
    }
}

/// The owner's input channel, as its session's reader would register it.
fn owner_inputs(shared: &Shared) -> mpsc::Receiver<Input> {
    let (tx, rx) = mpsc::channel(512);
    shared.set_input(tx);
    rx
}

async fn next_input(inputs: &mut mpsc::Receiver<Input>) -> Option<Input> {
    tokio::time::timeout(Duration::from_millis(300), inputs.recv())
        .await
        .ok()
        .flatten()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_user_does_not_hold_up_the_owner() {
    let owner = Owner::register("$slow", "vnc", ARGS);
    let owner_task = owner.run(async {
        let shared = current().unwrap();
        let mut inputs = owner_inputs(&shared);
        let mut display = owner_display();
        display.resize(256, 256, &mut Output::text());
        display.flush(&mut Output::text());

        // A user joins and never reads what it is sent, nor acknowledges.
        let mut peer = Peer::join("$slow", true).await;
        let wake = next_input(&mut inputs).await.expect("woken");
        assert_eq!((wake.opcode.as_str(), wake.from_viewer), ("sync", true));
        let mut decoder = InputDecoder::default();
        let Some(ClientEvent::Sync {
            timestamp,
            received,
        }) = decoder.decode(&wake, &mut Output::text())
        else {
            panic!("a sync");
        };
        display.ack(timestamp, received);

        // The owner goes on at its own pace, 12 frames of over 150 kB
        // each, while the user's socket is full. (Waiting for the user
        // would never end.)
        let frames = async {
            for frame in 0..12 {
                display.put(0, 0, 256, 256, &noise(256, 256, frame + 10));
                let mut out = Output::text();
                display.flush(&mut out);
                display.sync(&mut out);
                assert!(out.len() > 150_000, "{}", out.len());
                display.ack(display_last_sync(&out), StdInstant::now());
                tokio::task::yield_now().await;
            }
        };
        tokio::time::timeout(Duration::from_secs(60), frames)
            .await
            .expect("the owner is not held up");

        // The user got as many frames as its own window allows, the first
        // one (the whole screen) included.
        let mut syncs = 0;
        while let Some(ins) = peer.next().await {
            if ins.opcode == "sync" {
                syncs += 1;
            }
        }
        assert_eq!(syncs, MAX_UNACKED_FRAMES + 1);
    });
    owner_task.await;
}

/// The timestamp of the `sync` that ends `out`.
fn display_last_sync(out: &Output) -> i64 {
    let mut parser = Parser::new();
    parser.feed(out.as_bytes());
    let mut last = 0;
    while let Ok(Some(ins)) = parser.next_instruction() {
        if ins.opcode == "sync" {
            last = ins.arg(0).parse().unwrap();
        }
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn input_of_read_only_users_is_dropped_and_others_merged() {
    let owner = Owner::register("$input", "ssh", ARGS);
    owner
        .run(async {
            let shared = current().unwrap();
            let mut inputs = owner_inputs(&shared);
            let mut display = Display::new(ImageFormats::default(), true)
                .with_protocol("ssh")
                .with_pacing(Pacing::TERMINAL);
            display.resize(64, 64, &mut Output::text());
            display.flush(&mut Output::text());

            let mut watcher = Peer::join("$input", true).await;
            assert_eq!(next_input(&mut inputs).await.unwrap().opcode, "sync");
            watcher
                .send("5.mouse,2.10,2.20,1.1;3.key,2.97,1.1;4.size,3.800,3.600;")
                .await;
            watcher
                .send("9.clipboard,1.1,10.text/plain;4.blob,1.1,4.aGk=;3.end,1.1;")
                .await;
            assert!(next_input(&mut inputs).await.is_none());

            let mut helper = Peer::join("$input", false).await;
            helper
                .send("5.mouse,2.10,2.20,1.1;4.size,3.800,3.600;3.key,2.97,1.1;")
                .await;
            let mut decoder = InputDecoder::default();
            let mut owed = Output::text();
            let mut events = Vec::new();
            while let Some(input) = next_input(&mut inputs).await {
                assert!(input.from_viewer);
                // (The second user's join woke the owner again.)
                if let Some(event @ (ClientEvent::Mouse { .. } | ClientEvent::Key { .. })) =
                    decoder.decode(&input, &mut owed)
                {
                    events.push(event);
                }
                if events.len() == 2 {
                    break;
                }
            }
            assert_eq!(
                events,
                [
                    ClientEvent::Mouse {
                        x: 10,
                        y: 20,
                        mask: 1
                    },
                    ClientEvent::Key {
                        keysym: 97,
                        pressed: true
                    }
                ]
            );
            // The clipboard arrives as a stream of the user's own, whose
            // acknowledgements went to that user.
            helper
                .send("9.clipboard,1.1,10.text/plain;4.blob,1.1,4.aGk=;3.end,1.1;")
                .await;
            let mut clipboard = None;
            while clipboard.is_none() {
                let input = next_input(&mut inputs).await.expect("clipboard");
                clipboard = decoder.decode(&input, &mut owed);
            }
            assert_eq!(clipboard, Some(ClientEvent::Clipboard("hi".into())));
            assert!(owed.is_empty(), "{owed}");
            let mut acks = 0;
            while let Some(ins) = helper.next().await {
                acks += usize::from(ins.opcode == "ack");
            }
            assert_eq!(acks, 2);
            drop(watcher);
        })
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn users_leave_when_the_owner_does() {
    let owner = Owner::register("$end", "vnc", ARGS);
    let mut peer = owner
        .run(async {
            let shared = current().unwrap();
            let _inputs = owner_inputs(&shared);
            let mut display = owner_display();
            display.resize(64, 64, &mut Output::text());
            let peer = Peer::join("$end", true).await;
            display.flush(&mut Output::text());
            peer
        })
        .await;
    drop(owner);
    let mut last = None;
    while let Some(ins) = peer.next().await {
        last = Some(ins.opcode);
    }
    assert_eq!(last.as_deref(), Some("disconnect"));

    // Nothing to join any more.
    let mut late = Peer::join("$end", true).await;
    let error = late.next().await.unwrap();
    assert_eq!(error.opcode, "error");
    assert_eq!(error.arg(1), Status::RESOURCE_NOT_FOUND.code().to_string());
}
