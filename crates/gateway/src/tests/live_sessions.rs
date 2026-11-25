//! Live sessions and the connection history, end to end: real tunnels
//! through the gateway to a fake guacd.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use super::*;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// A guacd that takes any number of sessions: it answers the handshake,
/// sends a frame and keeps the session open until the gateway hangs up.
/// Each connection gets the id `$<n>`.
async fn fake_guacd() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut n = 0u32;
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            n += 1;
            tokio::spawn(async move {
                let mut parser = molcajete_protocol::Parser::new();
                let mut buf = [0u8; 4096];
                'handshake: loop {
                    while let Some(ins) = parser.next_instruction().unwrap() {
                        match ins.opcode.as_str() {
                            "select" => sock.write_all(STOCK_ARGS.as_bytes()).await.unwrap(),
                            "connect" => break 'handshake,
                            _ => {}
                        }
                    }
                    let read = sock.read(&mut buf).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    parser.feed(&buf[..read]);
                }
                let id = format!("${n}");
                let ready = molcajete_protocol::InstructionBuilder::new("ready")
                    .str_arg(&id)
                    .finish();
                sock.write_all(format!("{ready}4.sync,2.42;").as_bytes())
                    .await
                    .unwrap();
                while sock.read(&mut buf).await.is_ok_and(|n| n > 0) {}
            });
        }
    });
    addr
}

/// A gateway whose guacd is a [`fake_guacd`], served on a local port.
async fn live_app() -> (TestApp, SocketAddr) {
    let guacd = fake_guacd().await;
    let app = app_with(&format!(
        "[guacd]\nhost = \"127.0.0.1\"\nport = {}\n",
        guacd.port()
    ))
    .await;
    let addr = app.serve().await;
    (app, addr)
}

/// Opens the tunnel of connection `granted` as `cookie` and waits for the
/// first frame.
async fn open(addr: SocketAddr, cookie: &str) -> Socket {
    let mut request = format!("ws://{addr}/websocket-tunnel?connection=granted")
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

/// The status the tunnel closes with, which must come within a second.
async fn closed_with(socket: &mut Socket) -> String {
    let close = async {
        loop {
            match socket.next().await.expect("a close frame").unwrap() {
                Message::Close(Some(frame)) => return frame.reason.to_string(),
                Message::Text(_) => continue,
                other => panic!("expected close, got {other:?}"),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(1), close)
        .await
        .expect("the tunnel closes within a second")
}

async fn live(app: &TestApp, admin: &str) -> Vec<Value> {
    let response = app.get("/api/sessions", Some(admin)).await;
    assert_eq!(response.status(), StatusCode::OK);
    body_json(response).await.as_array().unwrap().clone()
}

/// The history once `n` sessions are in it (they are written as the
/// tunnels wind down).
async fn history(app: &TestApp, admin: &str, query: &str, n: usize) -> Vec<Value> {
    for _ in 0..100 {
        let response = app.get(&format!("/api/history?{query}"), Some(admin)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let entries = body_json(response).await["entries"]
            .as_array()
            .unwrap()
            .clone();
        if entries.len() >= n {
            assert_eq!(entries.len(), n, "{entries:#?}");
            return entries;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the history never had {n} sessions");
}

async fn user_id(app: &TestApp, admin: &str, username: &str) -> String {
    let users = body_json(app.get("/api/users", Some(admin)).await).await;
    users
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == username)
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn admins_see_live_sessions_and_end_them() {
    let (app, addr) = live_app().await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let before = db::now_millis();
    let mut bobs = open(addr, &bob).await;
    let mut admins = open(addr, &admin).await;

    let sessions = live(&app, &admin).await;
    assert_eq!(sessions.len(), 2, "{sessions:#?}");
    let session = sessions.iter().find(|s| s["username"] == "bob").unwrap();
    assert_eq!(session["connectionId"], "granted");
    assert_eq!(session["protocol"], "vnc");
    assert_eq!(session["peer"], "127.0.0.1");
    let started = session["startedAt"].as_i64().unwrap();
    assert!(started >= before && started <= db::now_millis());
    assert!(session["bytesToBrowser"].is_u64() && session["bytesToGuacd"].is_u64());
    assert_eq!(session["rttMs"], Value::Null);
    assert!(session["guacdId"].as_str().unwrap().starts_with('$'));
    let id = session["id"].as_str().unwrap().to_owned();

    // Administrators only.
    assert_eq!(
        app.get("/api/sessions", Some(&bob)).await.status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        delete(&app, &format!("/api/sessions/{id}"), &bob).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        app.get("/api/history", Some(&bob)).await.status(),
        StatusCode::FORBIDDEN
    );

    // Ending one closes it with SESSION_CLOSED, which the page shows as
    // ended by an administrator.
    assert_eq!(
        delete(&app, &format!("/api/sessions/{id}"), &admin).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(closed_with(&mut bobs).await, "523");
    assert_eq!(
        delete(&app, &format!("/api/sessions/{id}"), &admin).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete(&app, "/api/sessions/not-a-uuid", &admin).await,
        StatusCode::NOT_FOUND
    );

    let entries = history(&app, &admin, "user=bob", 1).await;
    let entry = &entries[0];
    assert_eq!(entry["id"], id.as_str());
    assert_eq!(entry["connectionId"], "granted");
    assert_eq!(entry["endStatus"], 0x020B);
    assert_eq!(entry["endReason"], "admin");
    assert_eq!(entry["endedBy"], "admin");
    assert!(entry["endedAt"].as_i64().unwrap() >= entry["startedAt"].as_i64().unwrap());
    assert!(entry["bytesToBrowser"].is_i64());
    let ended = only_entry(&app, &admin, "session.end").await;
    assert_eq!(ended["targetType"], "session");
    assert_eq!(ended["targetId"], id.as_str());
    assert_eq!(ended["changes"]["reason"], "admin");
    assert_eq!(ended["changes"]["user"], "bob");

    // The administrator's own session is still open; it closes normally.
    let sessions = live(&app, &admin).await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["username"], "admin");
    admins.close(None).await.unwrap();
    let entries = history(&app, &admin, "user=admin", 1).await;
    assert_eq!(entries[0]["endStatus"], 0);
    assert_eq!(entries[0]["endReason"], Value::Null);
    assert!(live(&app, &admin).await.is_empty());

    // CSV export.
    let csv = app
        .get("/api/history?format=csv&connection=granted", Some(&admin))
        .await;
    assert_eq!(csv.status(), StatusCode::OK);
    assert_eq!(
        csv.headers()[header::CONTENT_TYPE],
        "text/csv; charset=utf-8"
    );
    let bytes = axum::body::to_bytes(csv.into_body(), 1 << 20)
        .await
        .unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert!(lines[0].starts_with("started_at,ended_at,duration_seconds,user,"));
    assert!(lines[2].contains(",bob,") && lines[2].contains(",admin,523,admin,"));
}

#[tokio::test]
async fn ending_all_spares_the_caller_or_takes_one_user() {
    let (app, addr) = live_app().await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let mut first = open(addr, &bob).await;
    let mut second = open(addr, &bob).await;
    let mut admins = open(addr, &admin).await;

    let ended = app
        .call(
            &admin,
            Method::DELETE,
            "/api/sessions?user=nobody",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(ended["ended"], 0);
    assert_eq!(live(&app, &admin).await.len(), 3);

    let ended = app
        .call(
            &admin,
            Method::DELETE,
            "/api/sessions",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(ended["ended"], 2);
    assert_eq!(closed_with(&mut first).await, "523");
    assert_eq!(closed_with(&mut second).await, "523");
    history(&app, &admin, "user=bob", 2).await;

    let ended = app
        .call(
            &admin,
            Method::DELETE,
            "/api/sessions?user=admin",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(ended["ended"], 1);
    assert_eq!(closed_with(&mut admins).await, "523");
}

#[tokio::test]
async fn sessions_end_when_their_user_loses_access() {
    let (app, addr) = live_app().await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let bob_id = user_id(&app, &admin, "bob").await;
    let path = format!("/api/users/{bob_id}");

    // An unrelated change leaves the session open.
    let mut socket = open(addr, &bob).await;
    app.call(
        &admin,
        Method::PUT,
        &path,
        json!({ "connections": ["granted", "secret"] }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(live(&app, &admin).await.len(), 1);

    // Revoking the grant ends it with CLIENT_FORBIDDEN.
    app.call(
        &admin,
        Method::PUT,
        &path,
        json!({ "connections": ["secret"] }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(closed_with(&mut socket).await, "771");
    let entries = history(&app, &admin, "user=bob", 1).await;
    assert_eq!(entries[0]["endReason"], "access_revoked");
    assert_eq!(entries[0]["endStatus"], 0x0303);

    // Through a group: removing bob from it ends the session.
    let group = app
        .call(
            &admin,
            Method::POST,
            "/api/groups",
            json!({ "name": "staff", "members": [bob_id], "connections": ["granted"] }),
            StatusCode::CREATED,
        )
        .await;
    let mut socket = open(addr, &bob).await;
    app.call(
        &admin,
        Method::PUT,
        &format!("/api/groups/{}", group["id"].as_str().unwrap()),
        json!({ "members": [] }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(closed_with(&mut socket).await, "771");

    // Disabling the user ends their sessions.
    app.call(
        &admin,
        Method::PUT,
        &path,
        json!({ "connections": ["granted"] }),
        StatusCode::OK,
    )
    .await;
    let mut socket = open(addr, &bob).await;
    app.call(
        &admin,
        Method::PUT,
        &path,
        json!({ "disabled": true }),
        StatusCode::OK,
    )
    .await;
    assert_eq!(closed_with(&mut socket).await, "771");
    let entries = history(&app, &admin, "user=bob", 3).await;
    assert_eq!(entries[0]["endReason"], "user_disabled");
    assert_eq!(entries[0]["endedBy"], "admin");
    let reasons: Vec<Value> = audit(&app, &admin, "action=session.end")
        .await
        .iter()
        .map(|e| e["changes"]["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        [
            json!("user_disabled"),
            json!("access_revoked"),
            json!("access_revoked")
        ]
    );
}

#[tokio::test]
async fn deleting_a_user_or_connection_ends_their_sessions() {
    let (app, addr) = live_app().await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;

    let mut socket = open(addr, &bob).await;
    assert_eq!(
        delete(&app, "/api/connections/granted", &admin).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(closed_with(&mut socket).await, "771");

    app.call(
        &admin,
        Method::POST,
        "/api/connections",
        json!({ "id": "granted", "name": "Granted", "protocol": "vnc" }),
        StatusCode::CREATED,
    )
    .await;
    let bob_id = user_id(&app, &admin, "bob").await;
    let path = format!("/api/users/{bob_id}");
    app.call(
        &admin,
        Method::PUT,
        &path,
        json!({ "connections": ["granted"] }),
        StatusCode::OK,
    )
    .await;
    let mut socket = open(addr, &bob).await;
    assert_eq!(delete(&app, &path, &admin).await, StatusCode::NO_CONTENT);
    assert_eq!(closed_with(&mut socket).await, "771");
    let entries = history(&app, &admin, &format!("user={bob_id}"), 2).await;
    assert_eq!(entries[0]["endReason"], "user_deleted");
    assert_eq!(entries[1]["endReason"], "access_revoked");
}
