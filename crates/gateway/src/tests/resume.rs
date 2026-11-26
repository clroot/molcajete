//! Resuming sessions held by molcajeted (`crate::resume`), end to end: real
//! tunnels through the gateway to a fake molcajeted.

use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use super::*;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What the fake molcajeted saw of one connection.
#[derive(Debug, Default)]
struct Seen {
    /// The value of `molcajete-resume` in `connect`.
    key: String,
    /// The gateway sent `disconnect`.
    disconnected: bool,
}

/// A molcajeted that offers `molcajete-resume` and answers `resumed` to a
/// key it has seen before. Reports every connection once it closed.
async fn fake_molcajeted() -> (SocketAddr, mpsc::UnboundedReceiver<Seen>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let known = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (tx, known) = (tx.clone(), known.clone());
            tokio::spawn(async move {
                let mut parser = molcajete_protocol::Parser::new();
                let mut buf = [0u8; 4096];
                let mut seen = Seen::default();
                'handshake: loop {
                    while let Some(ins) = parser.next_instruction().unwrap() {
                        match ins.opcode.as_str() {
                            "select" => sock
                                .write_all(
                                    b"4.args,13.VERSION_1_5_0,8.hostname,16.molcajete-resume,\
                                      16.molcajete-binary;",
                                )
                                .await
                                .unwrap(),
                            "connect" => {
                                seen.key = ins.arg(2).to_owned();
                                break 'handshake;
                            }
                            _ => {}
                        }
                    }
                    let read = sock.read(&mut buf).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    parser.feed(&buf[..read]);
                }
                let resumed = {
                    let mut known = known.lock().unwrap();
                    let resumed = !seen.key.is_empty() && known.contains(&seen.key);
                    known.push(seen.key.clone());
                    resumed
                };
                let ready = if resumed {
                    "5.ready,2.$1,7.resumed;"
                } else {
                    "5.ready,2.$1;"
                };
                sock.write_all(format!("{ready}4.sync,2.42;").as_bytes())
                    .await
                    .unwrap();
                loop {
                    while let Some(ins) = parser.next_instruction().unwrap() {
                        seen.disconnected |= ins.opcode == "disconnect";
                    }
                    match sock.read(&mut buf).await {
                        Ok(n) if n > 0 => parser.feed(&buf[..n]),
                        _ => break,
                    }
                }
                let _ = tx.send(seen);
            });
        }
    });
    (addr, rx)
}

async fn resume_app() -> (TestApp, SocketAddr, mpsc::UnboundedReceiver<Seen>) {
    let (daemon, seen) = fake_molcajeted().await;
    let app = app_with(&format!(
        "[guacd]\nhost = \"127.0.0.1\"\nport = {}\n",
        daemon.port()
    ))
    .await;
    let addr = app.serve().await;
    (app, addr, seen)
}

/// Opens connection `granted` as `cookie` with `query` added, and waits for
/// the first frame.
async fn open(addr: SocketAddr, cookie: &str, query: &str) -> Socket {
    let mut request = format!("ws://{addr}/websocket-tunnel?connection=granted{query}")
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert("sec-websocket-protocol", "guacamole".parse().unwrap());
    headers.insert("cookie", cookie.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    loop {
        match socket.next().await.expect("tunnel stays open").unwrap() {
            Message::Text(text) if text.contains("4.sync,2.42;") => return socket,
            Message::Text(_) => continue,
            other => panic!("expected the first frame, got {other:?}"),
        }
    }
}

async fn next(seen: &mut mpsc::UnboundedReceiver<Seen>) -> Seen {
    tokio::time::timeout(Duration::from_secs(5), seen.recv())
        .await
        .expect("the daemon's connection closes")
        .unwrap()
}

const TAB: &str = "&resume=2f1c9a6e-8f0b-4d55-9f0e-6c1d2b3a4f5e";

#[tokio::test]
async fn a_tab_gets_its_session_back() {
    let (app, addr, mut seen) = resume_app().await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;

    // The tunnel drops: no `disconnect`, so molcajeted may hold it.
    let mut first = open(addr, &bob, TAB).await;
    first.close(None).await.unwrap();
    let held = next(&mut seen).await;
    assert_eq!(held.key.len(), 64, "{held:?}");
    assert!(!held.disconnected);

    // The same tab: the same key, and molcajeted resumed it.
    let mut second = open(addr, &bob, TAB).await;
    second.close(None).await.unwrap();
    let again = next(&mut seen).await;
    assert_eq!(again.key, held.key);
    let entries = audit(&app, &admin, "action=session.resume").await;
    assert_eq!(entries.len(), 1, "{entries:#?}");
    assert_eq!(entries[0]["actorName"], "bob");
    assert_eq!(entries[0]["changes"]["connection"], "granted");
    assert_eq!(entries[0]["changes"]["guacdId"], "$1");

    // Another tab, another user, or no id: another key, or none.
    let other_tab = open(addr, &bob, "&resume=0f1c9a6e-8f0b-4d55-9f0e-6c1d2b3a4f5e").await;
    drop(other_tab);
    assert_ne!(next(&mut seen).await.key, held.key);
    let admins = open(addr, &admin, TAB).await;
    drop(admins);
    assert_ne!(next(&mut seen).await.key, held.key);
    let plain = open(addr, &bob, "").await;
    drop(plain);
    assert_eq!(next(&mut seen).await.key, "");
}

#[tokio::test]
async fn a_session_ended_by_the_gateway_ends_in_molcajeted() {
    let (app, addr, mut seen) = resume_app().await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let mut bobs = open(addr, &bob, TAB).await;
    let sessions = body_json(app.get("/api/sessions", Some(&admin)).await).await;
    let id = sessions[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        delete(&app, &format!("/api/sessions/{id}"), &admin).await,
        StatusCode::NO_CONTENT
    );
    // The browser hears why, and molcajeted is told to end it.
    loop {
        match bobs.next().await.expect("a close frame").unwrap() {
            Message::Close(Some(frame)) => {
                assert_eq!(frame.reason.as_str(), "523");
                break;
            }
            _ => continue,
        }
    }
    let ended = next(&mut seen).await;
    assert!(ended.disconnected, "{ended:?}");
}
