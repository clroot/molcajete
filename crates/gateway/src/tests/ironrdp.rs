//! Sessions with IronRDP's web client: the one-time credential, the
//! `rdp-relay` handshake and the raw relay, through a fake molcajeted.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use futures_util::SinkExt as _;

use super::*;

const RELAY_ARGS: &str = "4.args,13.VERSION_1_5_0,8.hostname,8.password,\
                          14.relay-username,14.relay-password;";

/// A molcajeted that answers `select,rdp-relay`, reports the `select` and
/// the parameters it was given, and then echoes every byte back.
async fn fake_daemon() -> (SocketAddr, mpsc::UnboundedReceiver<(String, Vec<String>)>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut parser = molcajete_protocol::Parser::new();
                let mut buf = [0u8; 4096];
                let mut selected = String::new();
                'handshake: loop {
                    while let Some(ins) = parser.next_instruction().unwrap() {
                        match ins.opcode.as_str() {
                            "select" => {
                                selected = ins.arg(0).to_owned();
                                sock.write_all(RELAY_ARGS.as_bytes()).await.unwrap();
                            }
                            "connect" => {
                                let _ = tx.send((selected.clone(), ins.args));
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
                sock.write_all(b"5.ready,2.$1;").await.unwrap();
                loop {
                    match sock.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => sock.write_all(&buf[..n]).await.unwrap(),
                    }
                }
            });
        }
    });
    (addr, rx)
}

async fn create(app: &TestApp, admin: &str, body: Value, expected: StatusCode) -> Value {
    app.call(admin, Method::POST, "/api/connections", body, expected)
        .await
}

fn desktop(id: &str, attributes: Value) -> Value {
    json!({
        "id": id,
        "name": id,
        "protocol": "rdp",
        "parameters": { "hostname": "desk.example", "password": "stored-password" },
        "attributes": attributes,
    })
}

async fn grant(app: &TestApp, cookie: &str, connection: &str, expected: StatusCode) -> Value {
    app.call(
        cookie,
        Method::POST,
        &format!("/api/connections/{connection}/ironrdp"),
        json!({}),
        expected,
    )
    .await
}

/// Opens the tunnel as IronRDP's client does: no subprotocol.
async fn open(
    addr: SocketAddr,
    cookie: &str,
    query: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let mut request = format!("ws://{addr}/websocket-tunnel?{query}")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("cookie", cookie.parse().unwrap());
    tokio_tungstenite::connect_async(request).await.unwrap().0
}

#[tokio::test]
async fn ironrdp_tunnel_relays_bytes_with_a_one_time_credential() {
    let (daemon, mut handshakes) = fake_daemon().await;
    let app = app_with(&format!(
        "[guacd]\nhost = \"127.0.0.1\"\nport = {}\n",
        daemon.port()
    ))
    .await;
    let addr = app.serve().await;
    let admin = app.login("admin", "admin-password").await;
    create(
        &app,
        &admin,
        desktop("desk", json!({ "ironrdp-default": "true" })),
        StatusCode::CREATED,
    )
    .await;

    let list = body_json(app.get("/api/connections", Some(&admin)).await).await;
    let desk = list
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == "desk")
        .unwrap();
    assert_eq!(desk["ironrdpDefault"], true);
    assert_eq!(desk["ironrdpSwitch"], false);

    let issued = grant(&app, &admin, "desk", StatusCode::OK).await;
    let token = issued["token"].as_str().unwrap();
    let mut socket = open(addr, &admin, &format!("connection=desk&ironrdp={token}")).await;

    let (selected, values) = tokio::time::timeout(Duration::from_secs(2), handshakes.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(selected, "rdp-relay");
    // VERSION, hostname, password, relay-username, relay-password.
    assert_eq!(values[1], "desk.example");
    assert_eq!(values[2], "stored-password");
    assert_eq!(values[3], issued["username"].as_str().unwrap());
    assert_eq!(values[4], issued["password"].as_str().unwrap());

    // Bytes go through unchanged, with no tunnel instructions around them.
    socket
        .send(Message::Binary(vec![0x30, 0x03, 0x02, 0x01, 0x00].into()))
        .await
        .unwrap();
    let echoed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        echoed,
        Message::Binary(vec![0x30, 0x03, 0x02, 0x01, 0x00].into())
    );
    drop(socket);

    // The token opened one tunnel; a second one is refused.
    let mut again = open(addr, &admin, &format!("connection=desk&ironrdp={token}")).await;
    match again.next().await.unwrap().unwrap() {
        Message::Close(Some(frame)) => assert_eq!(frame.reason.as_str(), "771"),
        other => panic!("expected close, got {other:?}"),
    }
}

#[tokio::test]
async fn only_rdp_connections_that_chose_ironrdp_get_a_credential() {
    let app = app().await;
    let admin = app.login("admin", "admin-password").await;
    create(
        &app,
        &admin,
        desktop("plain", json!({})),
        StatusCode::CREATED,
    )
    .await;
    create(
        &app,
        &admin,
        desktop("switch", json!({ "ironrdp-switch": "true" })),
        StatusCode::CREATED,
    )
    .await;
    grant(&app, &admin, "plain", StatusCode::BAD_REQUEST).await;
    grant(&app, &admin, "switch", StatusCode::OK).await;
    // bob may not open `switch` at all, and does not learn that it exists.
    let bob = app.login("bob", "bob-password").await;
    grant(&app, &bob, "switch", StatusCode::NOT_FOUND).await;

    // Not for other protocols, and not together with recording.
    let mut vnc = desktop("vnc", json!({ "ironrdp-default": "true" }));
    vnc["protocol"] = json!("vnc");
    create(&app, &admin, vnc, StatusCode::BAD_REQUEST).await;
    create(
        &app,
        &admin,
        desktop(
            "recorded",
            json!({ "ironrdp-switch": "true", attributes::RECORD: "true" }),
        ),
        StatusCode::BAD_REQUEST,
    )
    .await;
}
