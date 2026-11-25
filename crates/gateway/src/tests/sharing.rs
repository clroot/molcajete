//! Sharing live sessions, end to end: share links and administrators
//! watching, through real tunnels to a fake guacd that can be joined.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

use super::*;

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// What the fake guacd was asked for: the `select` and the `read-only`
/// value of `connect`.
#[derive(Debug, PartialEq, Eq)]
struct Opened {
    select: String,
    read_only: String,
}

const ARGS: &str = "4.args,13.VERSION_1_5_0,8.hostname,9.read-only;";

/// A guacd whose connections can be joined, like stock guacd: a new
/// connection gets the id `$<n>`, a join (`select,$<n>`) answers `ready`
/// with the id it joined. Reports what it was asked for.
async fn joinable_guacd() -> (SocketAddr, mpsc::UnboundedReceiver<Opened>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut n = 0u32;
        loop {
            let (mut sock, _) = listener.accept().await.unwrap();
            n += 1;
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut parser = molcajete_protocol::Parser::new();
                let mut buf = [0u8; 4096];
                let mut select = String::new();
                let read_only = 'handshake: loop {
                    while let Some(ins) = parser.next_instruction().unwrap() {
                        match ins.opcode.as_str() {
                            "select" => {
                                select = ins.arg(0).to_owned();
                                sock.write_all(ARGS.as_bytes()).await.unwrap();
                            }
                            "connect" => break 'handshake ins.arg(2).to_owned(),
                            _ => {}
                        }
                    }
                    let read = sock.read(&mut buf).await.unwrap();
                    if read == 0 {
                        return;
                    }
                    parser.feed(&buf[..read]);
                };
                let id = if select.starts_with('$') {
                    select.clone()
                } else {
                    format!("${n}")
                };
                let _ = tx.send(Opened { select, read_only });
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
    (addr, rx)
}

async fn sharing_app(extra: &str) -> (TestApp, SocketAddr, mpsc::UnboundedReceiver<Opened>) {
    let (guacd, opened) = joinable_guacd().await;
    let app = app_with(&format!(
        "[guacd]\nhost = \"127.0.0.1\"\nport = {}\n{extra}",
        guacd.port()
    ))
    .await;
    let addr = app.serve().await;
    (app, addr, opened)
}

/// Opens a tunnel with `query` as `cookie`; `Ok` once the first frame
/// arrived, `Err` with the close status otherwise.
async fn tunnel(addr: SocketAddr, query: &str, cookie: &str) -> Result<Socket, String> {
    let mut request = format!("ws://{addr}/websocket-tunnel?{query}")
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert("sec-websocket-protocol", "guacamole".parse().unwrap());
    headers.insert("cookie", cookie.parse().unwrap());
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    loop {
        match socket.next().await.expect("an answer").unwrap() {
            Message::Text(text) if text.contains("4.sync,2.42;") => return Ok(socket),
            Message::Text(_) => continue,
            Message::Close(Some(frame)) => return Err(frame.reason.to_string()),
            other => panic!("unexpected {other:?}"),
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

async fn opened(rx: &mut mpsc::UnboundedReceiver<Opened>) -> Opened {
    tokio::time::timeout(Duration::from_secs(1), rx.recv())
        .await
        .unwrap()
        .unwrap()
}

/// Creates user `carol`, who has no grant of `granted`, and signs in.
async fn carol(app: &TestApp, admin: &str) -> String {
    app.call(
        admin,
        Method::POST,
        "/api/users",
        json!({ "username": "carol", "password": "carol-password-1" }),
        StatusCode::CREATED,
    )
    .await;
    app.login("carol", "carol-password-1").await
}

async fn share(app: &TestApp, cookie: &str, page: &str, body: Value) -> Value {
    app.call(
        cookie,
        Method::POST,
        &format!("/api/my-sessions/{page}/links"),
        body,
        StatusCode::CREATED,
    )
    .await
}

const BOB_PAGE: &str = "connection=granted&page=bob-tab";

#[tokio::test]
async fn owners_share_links_that_others_join_and_revoke_them() {
    let (app, addr, mut guacd) = sharing_app("").await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let carol = carol(&app, &admin).await;
    assert_eq!(
        tunnel(addr, "connection=granted", &carol).await.err(),
        Some("516".into()),
        "carol has no grant"
    );

    let _bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    let owner = opened(&mut guacd).await;
    assert_eq!(owner.select, "vnc");
    let mine = app
        .call(
            &bob,
            Method::GET,
            "/api/my-sessions/bob-tab",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mine["participants"], json!([]));
    assert_eq!(mine["links"], json!([]));
    assert_eq!(mine["sharing"], true);
    // Only the session's user manages it.
    app.call(
        &carol,
        Method::GET,
        "/api/my-sessions/bob-tab",
        json!(null),
        StatusCode::NOT_FOUND,
    )
    .await;
    app.call(
        &carol,
        Method::POST,
        "/api/my-sessions/bob-tab/links",
        json!({}),
        StatusCode::NOT_FOUND,
    )
    .await;

    let link = share(&app, &bob, "bob-tab", json!({ "expiresInMinutes": 30 })).await;
    let token = link["token"].as_str().unwrap().to_owned();
    assert_eq!(link["readOnly"], true, "view-only by default");
    let expires = link["expiresAt"].as_i64().unwrap() - link["createdAt"].as_i64().unwrap();
    assert_eq!(expires, 30 * 60 * 1000);

    // Anyone signed in may look at the link and join through it.
    let described = app
        .call(
            &carol,
            Method::GET,
            &format!("/api/join/{token}"),
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(described["owner"], "bob");
    assert_eq!(described["readOnly"], true);
    assert_eq!(
        app.get(&format!("/api/join/{token}"), None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let mut carols = tunnel(addr, &format!("join={token}"), &carol)
        .await
        .unwrap();
    assert_eq!(
        opened(&mut guacd).await,
        Opened {
            select: owner_id(&app, &admin).await,
            read_only: "true".into()
        }
    );

    // The owner sees who joined; administrators see both sessions.
    let mine = app
        .call(
            &bob,
            Method::GET,
            "/api/my-sessions/bob-tab",
            json!(null),
            StatusCode::OK,
        )
        .await;
    let participants = mine["participants"].as_array().unwrap();
    assert_eq!(participants.len(), 1);
    assert_eq!(participants[0]["username"], "carol");
    assert_eq!(participants[0]["kind"], "link");
    assert_eq!(participants[0]["readOnly"], true);
    assert_eq!(mine["links"][0]["joined"], 1);
    assert_eq!(mine["links"][0].get("token"), None);
    let live = body_json(app.get("/api/sessions", Some(&admin)).await).await;
    let live = live.as_array().unwrap();
    assert_eq!(live.len(), 2);
    let joined = live.iter().find(|s| s["username"] == "carol").unwrap();
    assert_eq!(joined["joinedTo"], mine["id"]);
    assert_eq!(joined["joinKind"], "link");

    // Revoking the link disconnects carol and refuses it from then on.
    let link_id = link["id"].as_str().unwrap();
    assert_eq!(
        delete(
            &app,
            &format!("/api/my-sessions/bob-tab/links/{link_id}"),
            &bob
        )
        .await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(closed_with(&mut carols).await, "771");
    assert_eq!(
        app.get(&format!("/api/join/{token}"), Some(&carol))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        tunnel(addr, &format!("join={token}"), &carol).await.err(),
        Some("516".into())
    );

    // An interactive link joins without `read-only`.
    let link = share(&app, &bob, "bob-tab", json!({ "readOnly": false })).await;
    let token = link["token"].as_str().unwrap();
    let _carols = tunnel(addr, &format!("join={token}"), &carol)
        .await
        .unwrap();
    assert_eq!(opened(&mut guacd).await.read_only, "");

    let actions: Vec<Value> = audit(&app, &admin, "action=session")
        .await
        .iter()
        .map(|e| e["action"].clone())
        .collect();
    assert_eq!(
        actions,
        [
            "session.join",
            "session.share",
            "session.unshare",
            "session.join",
            "session.share"
        ]
    );
}

/// The tunnel id of bob's own session.
async fn owner_session(app: &TestApp, admin: &str) -> Value {
    let live = body_json(app.get("/api/sessions", Some(admin)).await).await;
    live.as_array()
        .unwrap()
        .iter()
        .find(|s| s["username"] == "bob" && s["joinedTo"].is_null())
        .unwrap()
        .clone()
}

async fn owner_id(app: &TestApp, admin: &str) -> String {
    owner_session(app, admin).await["guacdId"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn administrators_watch_and_everyone_leaves_with_the_owner() {
    let (app, addr, mut guacd) = sharing_app("").await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let carol = carol(&app, &admin).await;

    let mut bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    opened(&mut guacd).await;
    let session = owner_session(&app, &admin).await;
    let id = session["id"].as_str().unwrap();

    // Only administrators watch, and only owners' sessions.
    assert_eq!(
        tunnel(addr, &format!("shadow={id}"), &carol).await.err(),
        Some("771".into())
    );
    let mut admins = tunnel(addr, &format!("shadow={id}"), &admin).await.unwrap();
    assert_eq!(
        opened(&mut guacd).await,
        Opened {
            select: session["guacdId"].as_str().unwrap().into(),
            read_only: "true".into()
        }
    );
    let admin_watch = body_json(app.get("/api/sessions", Some(&admin)).await).await;
    let watch_id = admin_watch
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["joinKind"] == "shadow")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        tunnel(addr, &format!("shadow={watch_id}"), &admin)
            .await
            .err(),
        Some("516".into())
    );
    let watched = only_entry(&app, &admin, "session.shadow").await;
    assert_eq!(watched["targetId"], id);
    assert_eq!(watched["changes"]["owner"], "bob");

    // The owner sees that an administrator is watching.
    let mine = app
        .call(
            &bob,
            Method::GET,
            "/api/my-sessions/bob-tab",
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(mine["participants"][0]["kind"], "shadow");
    assert_eq!(mine["participants"][0]["username"], "admin");

    let link = share(&app, &bob, "bob-tab", json!({})).await;
    let token = link["token"].as_str().unwrap().to_owned();
    let mut carols = tunnel(addr, &format!("join={token}"), &carol)
        .await
        .unwrap();
    opened(&mut guacd).await;

    // The owner leaving ends everyone who joined, and the links.
    bobs.close(None).await.unwrap();
    assert_eq!(closed_with(&mut admins).await, "0");
    assert_eq!(closed_with(&mut carols).await, "0");
    assert_eq!(
        app.get(&format!("/api/join/{token}"), Some(&carol))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // So does an administrator ending the owner's session.
    let mut bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    opened(&mut guacd).await;
    let link = share(&app, &bob, "bob-tab", json!({})).await;
    let mut carols = tunnel(
        addr,
        &format!("join={}", link["token"].as_str().unwrap()),
        &carol,
    )
    .await
    .unwrap();
    opened(&mut guacd).await;
    let id = owner_session(&app, &admin).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        delete(&app, &format!("/api/sessions/{id}"), &admin).await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(closed_with(&mut bobs).await, "523");
    assert_eq!(closed_with(&mut carols).await, "0");
    // Sessions that joined are not in the connection history.
    for _ in 0..50 {
        if app.state.sessions.list().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let history = body_json(app.get("/api/history", Some(&admin)).await).await;
    let users: Vec<Value> = history["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["username"].clone())
        .collect();
    assert_eq!(users, ["bob", "bob"]);
}

#[tokio::test]
async fn expired_links_and_turned_off_sharing_are_refused() {
    let (app, addr, mut guacd) = sharing_app("[sharing]\nmax_link_minutes = 60\n").await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let carol = carol(&app, &admin).await;
    let _bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    opened(&mut guacd).await;

    for minutes in [0, 61] {
        app.call(
            &bob,
            Method::POST,
            "/api/my-sessions/bob-tab/links",
            json!({ "expiresInMinutes": minutes }),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    app.call(
        &bob,
        Method::POST,
        "/api/my-sessions/other-tab/links",
        json!({}),
        StatusCode::NOT_FOUND,
    )
    .await;

    let id = owner_session(&app, &admin).await["id"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap();
    let (_, token) = app.state.sessions.create_link(id, true, Duration::ZERO);
    assert_eq!(
        tunnel(addr, &format!("join={token}"), &carol).await.err(),
        Some("516".into())
    );
    assert_eq!(
        tunnel(addr, "join=not-a-token", &carol).await.err(),
        Some("516".into())
    );

    let (app, addr, mut guacd) = sharing_app("[sharing]\nlinks = false\nshadowing = false\n").await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let _bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    opened(&mut guacd).await;
    app.call(
        &bob,
        Method::POST,
        "/api/my-sessions/bob-tab/links",
        json!({}),
        StatusCode::FORBIDDEN,
    )
    .await;
    let session = owner_session(&app, &admin).await;
    assert_eq!(
        tunnel(
            addr,
            &format!("shadow={}", session["id"].as_str().unwrap()),
            &admin
        )
        .await
        .err(),
        Some("771".into())
    );
}
