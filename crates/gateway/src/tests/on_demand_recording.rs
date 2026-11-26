//! Recording a session from its session menu, end to end: real tunnels to
//! a fake guacd whose sessions can be joined, as the recorder does.

use std::time::Duration;

use futures_util::SinkExt as _;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

use super::sharing::{ARGS, BOB_PAGE, Socket, carol, share, tunnel};
use super::*;

/// What the screen looks like to someone joining: the whole display, and
/// the owner's pointer, which the daemon sends whoever joined.
const SCREEN: &str = "4.size,1.0,4.1024,3.768;5.mouse,1.5,1.5,1.0,3.100;4.sync,3.100;";

/// What happened at the fake guacd, by the `select` of the connection
/// (`vnc` for the owner's, `$1` for a join of it).
#[derive(Debug)]
enum Event {
    Opened { select: String, read_only: String },
    Received { select: String, text: String },
    Closed { select: String },
}

/// A guacd whose first connection (`$1`) can be joined; a join is sent
/// [`SCREEN`] first.
async fn guacd() -> (SocketAddr, mpsc::UnboundedReceiver<Event>) {
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
                let joins = select.starts_with('$');
                let _ = tx.send(Event::Opened {
                    select: select.clone(),
                    read_only,
                });
                let after = if joins { SCREEN } else { "4.sync,2.42;" };
                sock.write_all(format!("5.ready,2.$1;{after}").as_bytes())
                    .await
                    .unwrap();
                while let Ok(n) = sock.read(&mut buf).await
                    && n > 0
                {
                    let text = String::from_utf8_lossy(&buf[..n]).into_owned();
                    let select = select.clone();
                    let _ = tx.send(Event::Received { select, text });
                }
                let _ = tx.send(Event::Closed { select });
            });
        }
    });
    (addr, rx)
}

/// The next event that `f` picks, within a second.
async fn next<T>(
    rx: &mut mpsc::UnboundedReceiver<Event>,
    mut f: impl FnMut(Event) -> Option<T>,
) -> T {
    let find = async {
        loop {
            if let Some(found) = f(rx.recv().await.expect("an event")) {
                return found;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(1), find)
        .await
        .expect("the event within a second")
}

/// Everything connection `select` sent guacd until it closed.
async fn sent_until_closed(rx: &mut mpsc::UnboundedReceiver<Event>, select: &str) -> String {
    let mut sent = String::new();
    next(rx, |event| match event {
        Event::Received { select: s, text } if s == select => {
            sent.push_str(&text);
            None
        }
        Event::Closed { select: s } if s == select => Some(()),
        _ => None,
    })
    .await;
    sent
}

/// A gateway on the fake guacd where `granted` has `attributes`.
async fn recording_app(
    extra: &str,
    attributes: &[&str],
) -> (TestApp, SocketAddr, mpsc::UnboundedReceiver<Event>) {
    let (guacd, events) = guacd().await;
    let app = app_with(&format!(
        "[guacd]\nhost = \"127.0.0.1\"\nport = {}\n{extra}",
        guacd.port()
    ))
    .await;
    let mut granted = app.state.db.connection("granted").await.unwrap();
    for name in attributes {
        granted
            .attributes
            .insert((*name).to_owned(), "true".to_owned());
    }
    app.state.db.update_connection(&granted).await.unwrap();
    let addr = app.serve().await;
    (app, addr, events)
}

const RECORDING: &str = "/api/my-sessions/bob-tab/recording";

async fn menu(app: &TestApp, bob: &str) -> Value {
    app.call(
        bob,
        Method::GET,
        "/api/my-sessions/bob-tab",
        json!(null),
        StatusCode::OK,
    )
    .await["recording"]
        .clone()
}

/// Bob's session, as `/api/sessions` lists it to administrators.
async fn bobs_session(app: &TestApp, admin: &str) -> Value {
    let live = body_json(app.get("/api/sessions", Some(admin)).await).await;
    live.as_array()
        .unwrap()
        .iter()
        .find(|s| s["username"] == "bob" && s["joinedTo"].is_null())
        .unwrap()
        .clone()
}

/// The file of recording `id` once it is complete.
async fn complete(app: &TestApp, id: &str) -> (db::Recording, String) {
    for _ in 0..200 {
        let recording = app.state.db.recording(id).await.unwrap();
        if recording.ended_at.is_some() {
            let directory = app
                .state
                .config
                .recordings
                .directory(&app.state.config.server);
            let path = recording::file_path(&directory, &recording).unwrap();
            return (recording, std::fs::read_to_string(path).unwrap());
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("recording {id} is not complete");
}

async fn send(socket: &mut Socket, text: &str) {
    socket.send(Message::Text(text.into())).await.unwrap();
}

#[tokio::test]
async fn owners_record_spans_of_their_session_from_the_menu() {
    let (app, addr, mut guacd) = recording_app("", &[attributes::RECORD_ON_DEMAND]).await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;
    let carol = carol(&app, &admin).await;
    let mut bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    next(&mut guacd, |e| {
        matches!(e, Event::Opened { .. }).then_some(())
    })
    .await;

    let state = menu(&app, &bob).await;
    assert_eq!(
        state,
        json!({ "always": false, "active": false, "onDemand": true, "recordingId": null, "startedAt": null })
    );
    // Only the session's user starts it.
    app.call(
        &carol,
        Method::POST,
        RECORDING,
        json!(null),
        StatusCode::NOT_FOUND,
    )
    .await;

    let started = app
        .call(
            &bob,
            Method::POST,
            RECORDING,
            json!(null),
            StatusCode::CREATED,
        )
        .await;
    let first = started["recordingId"].as_str().unwrap().to_owned();
    // The recorder joins the owner's connection, read-only.
    let (select, read_only) = next(&mut guacd, |e| match e {
        Event::Opened { select, read_only } => Some((select, read_only)),
        _ => None,
    })
    .await;
    assert_eq!((select.as_str(), read_only.as_str()), ("$1", "true"));
    app.call(
        &bob,
        Method::POST,
        RECORDING,
        json!(null),
        StatusCode::CONFLICT,
    )
    .await;
    let state = menu(&app, &bob).await;
    assert_eq!(state["active"], true);
    assert_eq!(state["recordingId"], first.as_str());
    assert_eq!(bobs_session(&app, &admin).await["recording"], true);

    // Who joins through a link learns that the session is recorded.
    let link = share(&app, &bob, "bob-tab", json!({})).await;
    let token = link["token"].as_str().unwrap();
    let described = app
        .call(
            &carol,
            Method::GET,
            &format!("/api/join/{token}"),
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(described["recording"], true);

    // Bob's mouse goes into the recording on its way to guacd.
    send(&mut bobs, "5.mouse,2.10,2.20,1.1;").await;
    let mut recorder_sent = String::new();
    next(&mut guacd, |e| match e {
        Event::Received { select, text } if select == "vnc" && text.contains("mouse") => Some(()),
        Event::Received { select, text } if select == "$1" => {
            recorder_sent.push_str(&text);
            None
        }
        _ => None,
    })
    .await;
    assert_eq!(delete(&app, RECORDING, &bob).await, StatusCode::NO_CONTENT);
    recorder_sent.push_str(&sent_until_closed(&mut guacd, "$1").await);
    assert_eq!(
        recorder_sent, "4.sync,3.100;10.disconnect;",
        "acknowledges frames, then leaves"
    );
    let (recording, text) = complete(&app, &first).await;
    assert!(text.starts_with("4.size,1.0,4.1024,3.768;"), "{text}");
    assert!(text.contains("5.mouse,2.10,2.20,1.1,"), "{text}");
    assert!(
        !text.contains("5.mouse,1.5,1.5"),
        "the daemon's pointer: {text}"
    );
    assert_eq!(recording.username, "bob");
    assert_eq!(
        recording.started_by,
        Some(
            bobs_session(&app, &admin).await["userId"]
                .as_str()
                .unwrap()
                .to_owned()
        )
    );
    assert_eq!(menu(&app, &bob).await["active"], false);
    assert_eq!(delete(&app, RECORDING, &bob).await, StatusCode::NOT_FOUND);
    let described = app
        .call(
            &carol,
            Method::GET,
            &format!("/api/join/{token}"),
            json!(null),
            StatusCode::OK,
        )
        .await;
    assert_eq!(described["recording"], false);

    // A second span is a recording of its own, which the session's end stops.
    let second = app
        .call(
            &bob,
            Method::POST,
            RECORDING,
            json!(null),
            StatusCode::CREATED,
        )
        .await["recordingId"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_ne!(second, first);
    let session = bobs_session(&app, &admin).await;
    assert_eq!(session["recordingIds"], json!([first, second]));
    // Bob closes the page.
    bobs.close(None).await.unwrap();
    drop(bobs);
    complete(&app, &second).await;

    // Bob plays and downloads his own; carol cannot; administrators see all.
    let mine = body_json(app.get("/api/recordings", Some(&bob)).await).await;
    assert_eq!(mine.as_array().unwrap().len(), 2);
    for path in [
        format!("/api/recordings/{first}"),
        format!("/api/recordings/{first}/file"),
    ] {
        assert_eq!(app.get(&path, Some(&bob)).await.status(), StatusCode::OK);
        assert_eq!(
            app.get(&path, Some(&carol)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let theirs = body_json(app.get("/api/recordings", Some(&carol)).await).await;
    assert_eq!(theirs, json!([]));
    assert_eq!(
        delete(&app, &format!("/api/recordings/{first}"), &bob).await,
        StatusCode::FORBIDDEN
    );
    let all = body_json(app.get("/api/recordings", Some(&admin)).await).await;
    assert_eq!(all.as_array().unwrap().len(), 2);

    // The history links both to the session.
    let mut history = Value::Null;
    for _ in 0..40 {
        history = body_json(app.get("/api/history", Some(&admin)).await).await;
        if !history["entries"].as_array().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let entry = &history["entries"][0];
    assert_eq!(entry["recordingId"], Value::Null);
    assert_eq!(entry["recordingIds"], json!([first, second]));

    let entries = audit(&app, &admin, "action=recording").await;
    let actions: Vec<(&str, &str, &str)> = entries
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap(),
                e["changes"]["reason"].as_str().unwrap_or(""),
                e["actorName"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        actions,
        [
            ("recording.stop", "session_ended", ""),
            ("recording.start", "", "bob"),
            ("recording.stop", "stopped", "bob"),
            ("recording.start", "", "bob"),
        ]
    );
    assert_eq!(entries[0]["changes"]["session"], session["id"]);
    assert_eq!(entries[0]["targetId"], second.as_str());
}

#[tokio::test]
async fn refused_without_the_setting_and_when_every_session_is_recorded() {
    // The connection does not allow it.
    let (app, addr, _guacd) = recording_app("", &[]).await;
    let bob = app.login("bob", "bob-password").await;
    let _bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    assert_eq!(menu(&app, &bob).await["onDemand"], false);
    app.call(
        &bob,
        Method::POST,
        RECORDING,
        json!(null),
        StatusCode::FORBIDDEN,
    )
    .await;

    // Recording is off in the gateway.
    let (app, addr, _guacd) = recording_app(
        "[recordings]\nenabled = false\n",
        &[attributes::RECORD_ON_DEMAND],
    )
    .await;
    let bob = app.login("bob", "bob-password").await;
    let _bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    assert_eq!(menu(&app, &bob).await["onDemand"], false);
    app.call(
        &bob,
        Method::POST,
        RECORDING,
        json!(null),
        StatusCode::FORBIDDEN,
    )
    .await;

    // *Record sessions* records it whole; the menu cannot stop it.
    let (app, addr, _guacd) =
        recording_app("", &[attributes::RECORD, attributes::RECORD_ON_DEMAND]).await;
    let bob = app.login("bob", "bob-password").await;
    let _bobs = tunnel(addr, BOB_PAGE, &bob).await.unwrap();
    let state = menu(&app, &bob).await;
    assert_eq!(
        (
            state["always"].clone(),
            state["active"].clone(),
            state["onDemand"].clone()
        ),
        (json!(true), json!(true), json!(false))
    );
    app.call(
        &bob,
        Method::POST,
        RECORDING,
        json!(null),
        StatusCode::CONFLICT,
    )
    .await;
    assert_eq!(delete(&app, RECORDING, &bob).await, StatusCode::CONFLICT);
}
