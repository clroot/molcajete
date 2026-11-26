use std::collections::HashMap;

use molcajete_protocol::Parser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::*;
use crate::session::{self, ClientEvent, InputDecoder};

/// A connected socket: the client's end and the session's halves.
async fn connection() -> (TcpStream, GuacReader, GuacWriter) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    let (rd, wr) = server.into_split();
    (client, GuacReader::new(rd), GuacWriter::new(wr))
}

/// Reads instructions from the client's end until `opcode` arrives.
async fn read_until(client: &mut TcpStream, opcode: &str) -> Instruction {
    let mut parser = Parser::new();
    let mut buf = [0u8; 4096];
    loop {
        while let Some(ins) = parser.next_instruction().unwrap() {
            if ins.opcode == opcode {
                return ins;
            }
        }
        let n = tokio::time::timeout(Duration::from_secs(5), client.read(&mut buf))
            .await
            .expect("an instruction in time")
            .unwrap();
        assert!(n > 0, "closed before `{opcode}`");
        parser.feed(&buf[..n]);
    }
}

/// What a session loop saw, in order.
#[derive(Debug, PartialEq, Eq)]
enum Seen {
    Key(u32),
    Detached,
    Attached,
    Disconnect,
}

/// A session loop in the manner of the protocols': established once it
/// runs, writes `ack` for every key, ends at `disconnect`.
async fn session_loop(
    reader: GuacReader,
    mut writer: GuacWriter,
    seen: mpsc::UnboundedSender<Seen>,
) {
    let (mut inputs, _task) = session::spawn_reader(reader);
    let mut decoder = InputDecoder::default();
    establish(&mut writer);
    while let Some(ins) = inputs.recv().await {
        match decoder.decode(&ins, writer.buffer()) {
            Some(ClientEvent::Key { keysym, .. }) => {
                seen.send(Seen::Key(keysym)).unwrap();
                writer.push(&InstructionBuilder::new("ack").arg(keysym).finish());
            }
            Some(ClientEvent::Detached) => seen.send(Seen::Detached).unwrap(),
            Some(ClientEvent::Attached(attach)) => {
                assert!(take_over(&mut writer, &attach).await);
                writer.push(
                    &InstructionBuilder::new("size")
                        .arg(attach.info().width)
                        .finish(),
                );
                seen.send(Seen::Attached).unwrap();
            }
            Some(ClientEvent::Disconnect) => {
                seen.send(Seen::Disconnect).unwrap();
                return;
            }
            _ => {}
        }
        writer.flush().await.unwrap();
    }
}

fn start(
    key: &str,
    grace: Duration,
) -> (
    Arc<Hub>,
    mpsc::UnboundedReceiver<Seen>,
    impl FnOnce(GuacReader, GuacWriter) -> JoinHandle<()>,
) {
    let held = Held::register("$test", "ssh", key.to_owned(), grace);
    let hub = held.0.clone();
    let (tx, rx) = mpsc::unbounded_channel();
    let spawn = move |reader, writer| {
        tokio::spawn(async move {
            held.run(session_loop(reader, writer, tx)).await;
            drop(held);
        })
    };
    (hub, rx, spawn)
}

async fn next(seen: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    tokio::time::timeout(Duration::from_secs(5), seen.recv())
        .await
        .expect("the session loop in time")
        .expect("the session loop running")
}

async fn wait_established(key: &str) -> Arc<Hub> {
    for _ in 0..100 {
        if let Some(hub) = find("ssh", key) {
            return hub;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the session was never established");
}

fn info(width: u32) -> ClientInfo {
    ClientInfo {
        width,
        ..ClientInfo::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_leaves_can_come_back() {
    let (_hub, mut seen, spawn) = start("back", Duration::from_secs(30));
    let (mut client, reader, writer) = connection().await;
    let session = spawn(reader, writer);
    let hub = wait_established("back").await;
    client.write_all(b"3.key,2.65,1.1;").await.unwrap();
    assert_eq!(next(&mut seen).await, Seen::Key(65));
    read_until(&mut client, "ack").await;

    // The connection drops: the session goes on without a client.
    drop(client);
    assert_eq!(next(&mut seen).await, Seen::Detached);
    assert!(!session.is_finished());

    // The next client gets it, and its input reaches the same loop.
    let (mut client, reader, writer) = connection().await;
    assert!(hub.attach(reader, writer.into_inner(), info(1234)).await);
    assert_eq!(next(&mut seen).await, Seen::Attached);
    assert_eq!(read_until(&mut client, "size").await.args, ["1234"]);
    client.write_all(b"3.key,2.66,1.1;").await.unwrap();
    assert_eq!(next(&mut seen).await, Seen::Key(66));
    read_until(&mut client, "ack").await;

    // A `disconnect` still ends it, and it can no longer be found.
    client.write_all(b"10.disconnect;").await.unwrap();
    assert_eq!(next(&mut seen).await, Seen::Disconnect);
    session.await.unwrap();
    assert!(find("ssh", "back").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_held_session_ends_after_the_grace_period() {
    let (_hub, mut seen, spawn) = start("expire", Duration::from_millis(200));
    let (client, reader, writer) = connection().await;
    let session = spawn(reader, writer);
    wait_established("expire").await;
    drop(client);
    assert_eq!(next(&mut seen).await, Seen::Detached);
    assert_eq!(next(&mut seen).await, Seen::Disconnect);
    session.await.unwrap();
    assert!(find("ssh", "expire").is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_client_takes_the_session_over() {
    let (_hub, mut seen, spawn) = start("twice", Duration::from_secs(30));
    let (mut first, reader, writer) = connection().await;
    let session = spawn(reader, writer);
    let hub = wait_established("twice").await;

    let (mut second, reader, writer) = connection().await;
    assert!(hub.attach(reader, writer.into_inner(), info(800)).await);
    assert_eq!(next(&mut seen).await, Seen::Attached);
    // The first is told why, and closed.
    let error = read_until(&mut first, "error").await;
    assert_eq!(error.arg(1), Status::SESSION_CONFLICT.code().to_string());
    let mut buf = [0u8; 64];
    loop {
        match first.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    // Its socket closing does not detach the second.
    read_until(&mut second, "size").await;
    second.write_all(b"3.key,2.67,1.1;").await.unwrap();
    assert_eq!(next(&mut seen).await, Seen::Key(67));
    second.write_all(b"10.disconnect;").await.unwrap();
    assert_eq!(next(&mut seen).await, Seen::Disconnect);
    session.await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_leaves_while_connecting_ends_the_session() {
    let held = Held::register("$early", "ssh", "early".to_owned(), Duration::from_secs(30));
    let (client, reader, _writer) = connection().await;
    let ended = held
        .run(async {
            let (mut inputs, _task) = session::spawn_reader(reader);
            drop(client);
            // Not established: the loop is told the client left.
            let ins = tokio::time::timeout(Duration::from_secs(5), inputs.recv())
                .await
                .unwrap()
                .unwrap();
            ins.opcode == "disconnect" && ins.control.is_none()
        })
        .await;
    assert!(ended);
    assert!(find("ssh", "early").is_none(), "never established");
}

#[test]
fn the_handshake_decides_whether_a_session_is_held() {
    let params = |pairs: &[(&str, &str)]| {
        Params::from(
            pairs
                .iter()
                .map(|(n, v)| (n.to_string(), v.to_string()))
                .collect::<HashMap<_, _>>(),
        )
    };
    // Stock clients send no key.
    assert_eq!(wanted("rdp", &params(&[])), None);
    assert_eq!(wanted("rdp", &params(&[(HANDSHAKE_ARG, "")])), None);
    assert_eq!(
        wanted("rdp", &params(&[(HANDSHAKE_ARG, "k")])),
        Some(("k".to_owned(), Duration::from_secs(120)))
    );
    assert_eq!(
        wanted("ssh", &params(&[(HANDSHAKE_ARG, "k")])),
        Some(("k".to_owned(), Duration::from_secs(300)))
    );
    // The connection's own grace period, or none.
    assert_eq!(
        wanted("vnc", &params(&[(HANDSHAKE_ARG, "k"), (TIMEOUT_ARG, "30")])),
        Some(("k".to_owned(), Duration::from_secs(30)))
    );
    assert_eq!(
        wanted("vnc", &params(&[(HANDSHAKE_ARG, "k"), (TIMEOUT_ARG, "0")])),
        None
    );
    let long = "k".repeat(MAX_KEY_LEN + 1);
    assert_eq!(wanted("vnc", &params(&[(HANDSHAKE_ARG, &long)])), None);
}
