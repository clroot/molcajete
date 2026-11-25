//! API tokens and one-time session links end to end, through the router
//! (and a real socket for the tunnel).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use axum::response::Response;
use serde_json::{Value, json};
use tower::ServiceExt as _;

use crate::config::Config;
use crate::db::{self, NewApiToken, now_millis};
use crate::{AppState, auth, bootstrap, router};

const DAY_MS: i64 = 86_400_000;

struct App {
    router: Router,
    state: Arc<AppState>,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

/// How a request authenticates.
#[derive(Clone, Copy)]
enum As<'a> {
    Cookie(&'a str),
    Token(&'a str),
}

/// A gateway with an `admin` administrator, a `bob` user who may open
/// `granted` (not `secret`), and guacd at `guacd_port`.
async fn app(guacd_port: u16) -> App {
    let web = tempfile::tempdir().unwrap();
    std::fs::write(web.path().join("index.html"), "<!doctype html>").unwrap();
    let (db, data) = db::tests::sqlite().await;
    let toml = format!(
        "[server]\nstatic_dir = {:?}\ndata_dir = {:?}\ntrusted_proxies = [\"127.0.0.1\"]\n\
         [guacd]\nhost = \"127.0.0.1\"\nport = {guacd_port}\n\
         [api_tokens]\nmax_lifetime_days = 30\n\
         [[users]]\nusername = \"admin\"\npassword = \"admin-password\"\n\
         [[connections]]\nid = \"granted\"\nname = \"Granted desk\"\nprotocol = \"rdp\"\n\
         [connections.parameters]\ndpi = \"96\"\n\
         [[connections]]\nid = \"secret\"\nprotocol = \"ssh\"\n",
        web.path().display().to_string(),
        data.path().display().to_string()
    );
    let config: Config = ::toml::from_str(&toml).unwrap();
    bootstrap::seed(&db, &config).await.unwrap();
    let bob = db
        .create_user(db::NewUser {
            username: "bob",
            password_hash: &auth::hash_password("bob-password").unwrap(),
            role: db::Role::User,
            disabled: false,
        })
        .await
        .unwrap();
    db.set_user_connections(&bob.id, &["granted".into()])
        .await
        .unwrap();
    let state = Arc::new(AppState {
        throttle: crate::throttle::LoginThrottle::new(&config.auth.throttle),
        config,
        db,
        client_reports: false,
        tunnels: Default::default(),
        sessions: Default::default(),
        oidc: None,
    });
    App {
        router: router(state.clone()),
        state,
        _dirs: (web, data),
    }
}

impl App {
    async fn send(&self, request: Request<Body>) -> Response {
        self.router.clone().oneshot(request).await.unwrap()
    }

    /// Sends a request with `headers` and returns the status and the JSON
    /// answer (`null` for none).
    async fn call_with(
        &self,
        method: Method,
        path: &str,
        auth: As<'_>,
        body: Option<Value>,
        headers: &[(&str, &str)],
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        request = match auth {
            As::Cookie(cookie) => request.header(header::COOKIE, cookie),
            As::Token(token) => request.header(header::AUTHORIZATION, format!("Bearer {token}")),
        };
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let request = match body {
            Some(body) => request
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body.to_string())),
            None => request.body(Body::empty()),
        };
        let mut request = request.unwrap();
        // Behind a trusted proxy, which names the client in `X-Real-IP`.
        request
            .extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 40000))));
        let response = self.send(request).await;
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        auth: As<'_>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        self.call_with(method, path, auth, body, &[]).await
    }

    async fn login(&self, username: &str, password: &str) -> String {
        let request = Request::post("/api/auth/login")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({ "username": username, "password": password }).to_string(),
            ))
            .unwrap();
        let response = self.send(request).await;
        assert_eq!(response.status(), StatusCode::OK);
        let cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        cookie.split(';').next().unwrap().to_owned()
    }

    /// Creates a token as `cookie`'s user; returns the token and its id.
    async fn token(&self, cookie: &str, name: &str, allowed_ips: &[&str]) -> (String, String) {
        let (status, body) = self
            .call(
                Method::POST,
                "/api/tokens",
                As::Cookie(cookie),
                Some(json!({
                    "name": name,
                    "expiresAt": now_millis() + DAY_MS,
                    "allowedIps": allowed_ips,
                })),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        (
            body["token"].as_str().unwrap().to_owned(),
            body["id"].as_str().unwrap().to_owned(),
        )
    }

    /// Audit entries with `action`, newest first.
    async fn audit(&self, admin: &str, action: &str) -> Vec<Value> {
        let path = format!("/api/audit?action={action}");
        let (status, body) = self.call(Method::GET, &path, As::Cookie(admin), None).await;
        assert_eq!(status, StatusCode::OK);
        body["entries"].as_array().unwrap().clone()
    }
}

#[tokio::test]
async fn tokens_automate_what_their_owner_may_do() {
    let app = app(1).await;
    let admin = app.login("admin", "admin-password").await;
    let (token, token_id) = app.token(&admin, "inventory sync", &[]).await;
    assert!(token.starts_with("mlc_") && token.len() == 68, "{token}");
    let bearer = As::Token(&token);

    // Create a connection and grant it, from another origin even.
    let cross_site = [("host", "gw.test"), ("origin", "https://cmdb.test")];
    let (status, body) = app
        .call_with(
            Method::POST,
            "/api/connections",
            bearer,
            Some(
                json!({ "id": "from-cmdb", "name": "From the CMDB", "protocol": "rdp",
                         "parameters": { "hostname": "10.0.0.7" } }),
            ),
            &cross_site,
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, me) = app
        .call(Method::GET, "/api/auth/me", As::Cookie(&admin), None)
        .await;
    let bob_id = app
        .call(Method::GET, "/api/users", bearer, None)
        .await
        .1
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["username"] == "bob")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let (status, body) = app
        .call(
            Method::PUT,
            "/api/connections/from-cmdb/access",
            bearer,
            Some(json!({ "users": [bob_id], "groups": [] })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let bob = app.login("bob", "bob-password").await;
    let (_, mine) = app
        .call(Method::GET, "/api/connections", As::Cookie(&bob), None)
        .await;
    assert!(
        mine.as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == "from-cmdb")
    );

    // A cookie next to the token does not change who this is.
    let (_, who) = app
        .call_with(
            Method::GET,
            "/api/auth/me",
            bearer,
            None,
            &[("cookie", &bob)],
        )
        .await;
    assert_eq!(who["username"], "admin");

    // What needs a person.
    let me_path = format!("/api/users/{}", me["id"].as_str().unwrap());
    for (method, path, body) in [
        (
            Method::POST,
            "/api/tokens",
            json!({ "name": "more", "expiresAt": now_millis() + DAY_MS }),
        ),
        (
            Method::PUT,
            "/api/auth/password",
            json!({ "currentPassword": "admin-password", "newPassword": "taken-over" }),
        ),
        (
            Method::PUT,
            me_path.as_str(),
            json!({ "password": "taken-over" }),
        ),
    ] {
        let (status, body) = app.call(method.clone(), path, bearer, Some(body)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}: {body}");
    }

    // Changes made with the token say so in the audit log.
    let created = app.audit(&admin, "connection.create").await;
    assert_eq!(created[0]["actorName"], "admin");
    let source = created[0]["sourceAddress"].as_str().unwrap();
    assert!(source.contains("API token \"inventory sync\""), "{source}");

    // Revoked.
    let (status, _) = app
        .call(
            Method::DELETE,
            &format!("/api/tokens/{token_id}"),
            As::Cookie(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = app
        .call(Method::GET, "/api/connections", bearer, None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Expired.
    let expired = "mlc_expired";
    app.state
        .db
        .create_api_token(NewApiToken {
            token_hash: &auth::token_hash(expired),
            prefix: "mlc_expired",
            user_id: me["id"].as_str().unwrap(),
            name: "old",
            allowed_ips: &[],
            expires_at: now_millis() - 1,
        })
        .await
        .unwrap();
    let (status, _) = app
        .call(Method::GET, "/api/connections", As::Token(expired), None)
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = app
        .call(
            Method::GET,
            "/api/connections",
            As::Token("mlc_unknown"),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Created, used once (not per request) and revoked, in the audit log.
    let entries = app.audit(&admin, "token.create").await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["targetName"], "inventory sync");
    assert_eq!(app.audit(&admin, "token.use").await.len(), 1);
    let revoked = app.audit(&admin, "token.revoke").await;
    assert_eq!(revoked[0]["targetId"], token_id.as_str());
}

#[tokio::test]
async fn tokens_follow_their_owner_and_allowlist() {
    let app = app(1).await;
    let admin = app.login("admin", "admin-password").await;
    let bob = app.login("bob", "bob-password").await;

    let (status, body) = app
        .call(
            Method::POST,
            "/api/tokens",
            As::Cookie(&bob),
            Some(json!({ "name": "too long", "expiresAt": now_millis() + 31 * DAY_MS })),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, _) = app
        .call(
            Method::POST,
            "/api/tokens",
            As::Cookie(&bob),
            Some(
                json!({ "name": "no ips", "expiresAt": now_millis() + DAY_MS,
                         "allowedIps": ["somewhere"] }),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (token, _) = app
        .token(&bob, "office only", &["192.0.2.0/24", "2001:db8::/32"])
        .await;
    let bearer = As::Token(&token);
    let from = |ip: &'static str| [("x-real-ip", ip)];
    let (status, _) = app
        .call_with(
            Method::GET,
            "/api/connections",
            bearer,
            None,
            &from("192.0.2.40"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .call_with(
            Method::GET,
            "/api/connections",
            bearer,
            None,
            &from("2001:db8::9"),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = app
        .call_with(
            Method::GET,
            "/api/connections",
            bearer,
            None,
            &from("198.51.100.1"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    // Nor is the proxy itself.
    let (status, _) = app
        .call(Method::GET, "/api/connections", bearer, None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A new address is a new audit entry.
    assert_eq!(app.audit(&admin, "token.use").await.len(), 2);

    // The owner's rights, not more.
    let (status, _) = app
        .call_with(Method::GET, "/api/users", bearer, None, &from("192.0.2.40"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Lists: their own for users, everyone's for administrators.
    app.token(&admin, "admin's", &[]).await;
    let (_, mine) = app
        .call(Method::GET, "/api/tokens", As::Cookie(&bob), None)
        .await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
    assert_eq!(
        mine[0]["allowedIps"],
        json!(["192.0.2.0/24", "2001:db8::/32"])
    );
    assert_eq!(mine[0]["lastUsedFrom"], "192.0.2.40 (via 127.0.0.1)");
    assert!(mine[0].get("token").is_none());
    let (status, _) = app
        .call(Method::GET, "/api/tokens?scope=all", As::Cookie(&bob), None)
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (_, all) = app
        .call(
            Method::GET,
            "/api/tokens?scope=all",
            As::Cookie(&admin),
            None,
        )
        .await;
    assert_eq!(all.as_array().unwrap().len(), 2);
    // Users cannot revoke someone else's.
    let admins = all
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["username"] == "admin")
        .unwrap();
    let path = format!("/api/tokens/{}", admins["id"].as_str().unwrap());
    let (status, _) = app
        .call(Method::DELETE, &path, As::Cookie(&bob), None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Disabling the owner stops the token.
    let bob_id = mine[0]["userId"].as_str().unwrap();
    let (status, _) = app
        .call(
            Method::PUT,
            &format!("/api/users/{bob_id}"),
            As::Cookie(&admin),
            Some(json!({ "disabled": true })),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = app
        .call_with(
            Method::GET,
            "/api/connections",
            bearer,
            None,
            &from("192.0.2.40"),
        )
        .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}
