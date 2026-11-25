//! molcajete web gateway.
//!
//! Replaces the Java web application of Apache Guacamole: serves the web
//! client, keeps users and connections in a database (PostgreSQL, MySQL or
//! SQLite), and tunnels the Guacamole protocol between the browser
//! (WebSocket) and guacd (TCP).

mod api;
mod auth;
mod bootstrap;
mod client_report;
mod config;
mod db;
mod guacd;
mod oidc;
mod tunnel;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, RawQuery, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use clap::Parser as _;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::api::ApiError;
use crate::config::Config;
use crate::db::{Database, DbError};

#[derive(clap::Parser, Debug)]
#[command(version, about)]
struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, env = "MOLCAJETE_CONFIG", default_value = "gateway.toml")]
    config: PathBuf,
    /// Overrides `server.listen` from the configuration file.
    #[arg(long, env = "MOLCAJETE_LISTEN")]
    listen: Option<SocketAddr>,
    /// Overrides `server.static_dir` from the configuration file.
    #[arg(long, env = "MOLCAJETE_STATIC_DIR")]
    static_dir: Option<PathBuf>,
    /// Overrides `server.data_dir` (where the default SQLite database lives).
    #[arg(long, env = "MOLCAJETE_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Overrides `database.url`: `postgres://…`, `mysql://…` or `sqlite://…`.
    #[arg(long, env = "MOLCAJETE_DATABASE_URL", hide_env_values = true)]
    database_url: Option<String>,
    /// Accept performance reports from the browser client
    /// (`POST /api/telemetry/client`), logged and recorded as metrics.
    #[arg(long, env = "MOLCAJETE_CLIENT_REPORTS")]
    client_reports: bool,
    #[command(flatten)]
    telemetry: molcajete_telemetry::TelemetryArgs,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Sets a user's password (read from stdin) and makes the account an
    /// enabled administrator, creating it if needed. For recovery.
    SetPassword { username: String },
}

pub struct AppState {
    pub config: Config,
    pub db: Database,
    pub client_reports: bool,
    /// Which tunnel each page session opened, for client reports.
    pub tunnels: client_report::Tunnels,
    /// Single sign-on, when `[oidc]` is configured.
    pub oidc: Option<oidc::Oidc>,
}

impl AppState {
    /// Whether the username and password form is offered.
    pub fn password_login(&self) -> bool {
        self.oidc.as_ref().is_none_or(|o| o.config.password_login)
    }
}

/// Lets the page know which optional features are on.
async fn client_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({ "clientReports": state.client_reports }))
}

async fn websocket_tunnel(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    if !auth::same_origin(&headers) {
        return ApiError::new(StatusCode::FORBIDDEN, "Cross-origin tunnel refused.")
            .into_response();
    }
    let user = match auth::user_from_headers(&state, &headers).await {
        Ok(user) => user,
        Err(err) => return err.into_response(),
    };
    let mut request = tunnel::TunnelRequest::from_query(query.as_deref().unwrap_or(""));
    request.peer = Some(client_address(&headers, peer));
    request.user = Some(user.username.clone());
    let connection = match request.connection.as_deref() {
        Some(id) => match state.db.connection_for_user(id, &user).await {
            Ok(c) => Some(c),
            Err(DbError::NotFound) => None,
            Err(err) => return ApiError::from(err).into_response(),
        },
        None => None,
    };
    // Our own client offers the binary subprotocol next to `guacamole`;
    // the first one listed here that the client offered is selected.
    let protocols: &[&'static str] = if state.config.server.binary_tunnel {
        &[tunnel::BINARY_SUBPROTOCOL, tunnel::TEXT_SUBPROTOCOL]
    } else {
        &[tunnel::TEXT_SUBPROTOCOL]
    };
    let ws = ws.protocols(protocols.iter().copied());
    request.binary = ws
        .selected_protocol()
        .is_some_and(|selected| selected.as_bytes() == tunnel::BINARY_SUBPROTOCOL.as_bytes());
    ws.max_message_size(1 << 20)
        .on_upgrade(move |socket| tunnel::serve(socket, state, request, connection))
}

/// Requests that change state must come from our own origin.
async fn same_origin_guard(request: Request, next: Next) -> Response {
    let safe = matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if safe || auth::same_origin(request.headers()) {
        next.run(request).await
    } else {
        ApiError::new(StatusCode::FORBIDDEN, "Cross-origin request refused.").into_response()
    }
}

async fn require_session(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    match auth::user_from_headers(&state, request.headers()).await {
        Ok(_) => next.run(request).await,
        Err(err) => err.into_response(),
    }
}

/// The browser's address as reported by a reverse proxy, otherwise the TCP
/// peer. Only used for logs.
///
/// `X-Real-IP` (set by the edge proxy) wins over `X-Forwarded-For`: proxies
/// in between, such as the Tailscale ingress, replace `X-Forwarded-For` with
/// their own peer address but pass `X-Real-IP` through.
fn client_address(headers: &HeaderMap, peer: SocketAddr) -> String {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    header("x-real-ip")
        .or_else(|| header("x-forwarded-for"))
        .map_or_else(
            || peer.ip().to_string(),
            |v| format!("{v} (via {})", peer.ip()),
        )
}

fn router(state: Arc<AppState>) -> Router {
    let static_files =
        ServeDir::new(&state.config.server.static_dir).append_index_html_on_directories(true);

    let api = Router::new()
        .route("/auth/login", post(api::login))
        .route("/auth/logout", post(api::logout))
        .route("/auth/me", get(api::me))
        .route("/auth/password", put(api::change_password))
        .route("/auth/providers", get(oidc::providers))
        .route("/auth/oidc/login", get(oidc::login))
        .route("/auth/oidc/callback", get(oidc::callback))
        .route(
            "/connections",
            get(api::list_connections).post(api::create_connection),
        )
        .route(
            "/connections/{id}",
            get(api::get_connection)
                .put(api::update_connection)
                .delete(api::delete_connection),
        )
        .route("/connections/{id}/parent", put(api::move_connection))
        .route(
            "/connections/{id}/access",
            get(api::get_connection_access).put(api::set_connection_access),
        )
        .route("/folders", get(api::list_folders).post(api::create_folder))
        .route(
            "/folders/{id}",
            put(api::rename_folder).delete(api::delete_folder),
        )
        .route("/folders/{id}/parent", put(api::move_folder))
        .route(
            "/folders/{id}/access",
            get(api::get_folder_access).put(api::set_folder_access),
        )
        .route("/users", get(api::list_users).post(api::create_user))
        .route(
            "/users/{id}",
            get(api::get_user)
                .put(api::update_user)
                .delete(api::delete_user),
        )
        .route("/groups", get(api::list_groups).post(api::create_group))
        .route(
            "/groups/{id}",
            get(api::get_group)
                .put(api::update_group)
                .delete(api::delete_group),
        )
        .route("/settings", get(client_settings))
        .route(
            "/telemetry/client",
            post(client_report::receive)
                .layer(DefaultBodyLimit::max(64 * 1024))
                .route_layer(middleware::from_fn_with_state(
                    state.clone(),
                    require_session,
                )),
        )
        .fallback(|| async { ApiError::not_found() })
        .layer(middleware::from_fn(same_origin_guard))
        // API responses carry per-user data.
        .layer(SetResponseHeaderLayer::overriding(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ));

    Router::new()
        .nest("/api", api)
        .route("/websocket-tunnel", get(websocket_tunnel))
        .route("/healthz", get(|| async { "ok" }))
        .fallback_service(static_files)
        // The wasm bundle changes on every build; avoid stale caches.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

/// Opens the configured database, or the SQLite fallback.
async fn open_database(config: &Config) -> anyhow::Result<Database> {
    let url = match &config.database.url {
        Some(url) => url.clone(),
        None => {
            let dir = &config.server.data_dir;
            std::fs::create_dir_all(dir)
                .map_err(|e| anyhow::anyhow!("creating data_dir {}: {e}", dir.display()))?;
            let path = dir.join("molcajete.db");
            info!(path = %path.display(), "no database configured, using SQLite");
            Database::sqlite_url(&path)
        }
    };
    let db = Database::open(&url, config.database.max_connections).await?;
    info!(backend = db.dialect().name(), "database ready");
    Ok(db)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    molcajete_telemetry::init_logging(cli.telemetry.log_format, "info,tower_http=warn,pprof=warn");

    let mut config = Config::load(&cli.config)?;
    if let Some(listen) = cli.listen {
        config.server.listen = listen;
    }
    if let Some(dir) = cli.static_dir {
        config.server.static_dir = dir;
    }
    if let Some(dir) = cli.data_dir {
        config.server.data_dir = dir;
    }
    if let Some(url) = cli.database_url.filter(|u| !u.is_empty()) {
        config.database.url = Some(url);
    }

    if let Some(Command::SetPassword { username }) = cli.command {
        let db = open_database(&config).await?;
        eprintln!("New password for {username}:");
        let mut password = String::new();
        std::io::stdin().read_line(&mut password)?;
        let password = password.trim_end_matches(['\r', '\n']);
        bootstrap::set_password(&db, &username, password).await?;
        eprintln!("Password updated; {username} is an enabled administrator.");
        return Ok(());
    }

    molcajete_telemetry::start(&cli.telemetry, "gateway").await?;
    let listen = config.server.listen;
    if !config.server.static_dir.join("index.html").exists() {
        warn!(dir = %config.server.static_dir.display(), "static_dir has no index.html");
    }

    let oidc = config.oidc.clone().map(oidc::Oidc::new).transpose()?;
    if let Some(oidc) = &oidc {
        info!(
            issuer = %oidc.config.issuer,
            password_login = oidc.config.password_login,
            "single sign-on through OpenID Connect"
        );
    }
    let db = open_database(&config).await?;
    bootstrap::seed(&db, &config).await?;

    let state = Arc::new(AppState {
        config,
        db,
        client_reports: cli.client_reports,
        tunnels: Default::default(),
        oidc,
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(%listen, "gateway listening");

    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(molcajete_telemetry::shutdown_signal())
    .await?;
    info!("gateway stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::Request;
    use futures_util::StreamExt as _;
    use tower::ServiceExt as _;

    use super::*;

    struct TestApp {
        router: Router,
        state: Arc<AppState>,
        _dirs: (tempfile::TempDir, tempfile::TempDir),
    }

    /// A gateway with an `admin`/`admin-password` administrator, a `bob`
    /// user (`bob-password`) and the connections `granted` (bob may open
    /// it) and `secret`.
    async fn app() -> TestApp {
        app_with("").await
    }

    /// Like [`app`], with more configuration (starting inside `[server]`).
    async fn app_with(extra: &str) -> TestApp {
        let web = tempfile::tempdir().unwrap();
        std::fs::write(web.path().join("index.html"), "<!doctype html>").unwrap();
        let toml = format!(
            "[server]\nstatic_dir = {:?}\n{extra}\n\
             [[users]]\nusername = \"admin\"\npassword = \"admin-password\"\n\
             [[connections]]\nid = \"granted\"\nprotocol = \"vnc\"\n\
             [[connections]]\nid = \"secret\"\nprotocol = \"ssh\"\n",
            web.path().display().to_string()
        );
        let config: Config = ::toml::from_str(&toml).unwrap();
        let (db, data) = db::tests::sqlite().await;
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
            config,
            db,
            client_reports: false,
            tunnels: Default::default(),
            oidc: None,
        });
        TestApp {
            router: router(state.clone()),
            state,
            _dirs: (web, data),
        }
    }

    impl TestApp {
        async fn send(&self, request: Request<Body>) -> Response {
            self.router.clone().oneshot(request).await.unwrap()
        }

        async fn get(&self, path: &str, cookie: Option<&str>) -> Response {
            let mut request = Request::get(path);
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            self.send(request.body(Body::empty()).unwrap()).await
        }

        async fn json(
            &self,
            method: Method,
            path: &str,
            cookie: Option<&str>,
            body: serde_json::Value,
        ) -> Response {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                request = request.header(header::COOKIE, cookie);
            }
            self.send(request.body(Body::from(body.to_string())).unwrap())
                .await
        }

        /// Logs in and returns the `Cookie` header value.
        async fn login(&self, username: &str, password: &str) -> String {
            let response = self
                .json(
                    Method::POST,
                    "/api/auth/login",
                    None,
                    serde_json::json!({ "username": username, "password": password }),
                )
                .await;
            assert_eq!(response.status(), StatusCode::OK);
            let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
            assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
            set_cookie.split(';').next().unwrap().to_owned()
        }
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn static_files_and_health_are_public() {
        let app = app().await;
        assert_eq!(app.get("/", None).await.status(), StatusCode::OK);
        assert_eq!(app.get("/healthz", None).await.status(), StatusCode::OK);
        assert_eq!(
            app.get("/api/settings", None).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            app.get("/api/connections", None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.get("/api/auth/me", None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.get("/api/nope", None).await.status(),
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn login_session_and_logout() {
        let app = app().await;
        let wrong = app
            .json(
                Method::POST,
                "/api/auth/login",
                None,
                serde_json::json!({ "username": "admin", "password": "nope" }),
            )
            .await;
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let unknown = app
            .json(
                Method::POST,
                "/api/auth/login",
                None,
                serde_json::json!({ "username": "ghost", "password": "nope" }),
            )
            .await;
        assert_eq!(unknown.status(), StatusCode::UNAUTHORIZED);

        let cookie = app.login("admin", "admin-password").await;
        let me = body_json(app.get("/api/auth/me", Some(&cookie)).await).await;
        assert_eq!(me["username"], "admin");
        assert_eq!(me["isAdmin"], true);

        let logout = Request::post("/api/auth/logout")
            .header(header::COOKIE, &cookie)
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.send(logout).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            app.get("/api/auth/me", Some(&cookie)).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn users_only_see_granted_connections() {
        let app = app().await;
        let admin = app.login("admin", "admin-password").await;
        let bob = app.login("bob", "bob-password").await;

        let all = body_json(app.get("/api/connections", Some(&admin)).await).await;
        assert_eq!(all.as_array().unwrap().len(), 2);
        let mine = body_json(app.get("/api/connections", Some(&bob)).await).await;
        assert_eq!(mine[0]["id"], "granted");
        assert_eq!(mine.as_array().unwrap().len(), 1);

        assert_eq!(
            app.get("/api/connections/granted", Some(&bob))
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.get("/api/users", Some(&bob)).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.get("/api/connections/granted", Some(&admin))
                .await
                .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn admins_manage_connections_and_users() {
        let app = app().await;
        let admin = app.login("admin", "admin-password").await;
        let connection = serde_json::json!({
            "id": "new-desktop", "name": "New desktop", "protocol": "rdp",
            "parameters": { "hostname": "10.0.0.9", "port": "3389" }
        });
        let created = app
            .json(
                Method::POST,
                "/api/connections",
                Some(&admin),
                connection.clone(),
            )
            .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let again = app
            .json(Method::POST, "/api/connections", Some(&admin), connection)
            .await;
        assert_eq!(again.status(), StatusCode::CONFLICT);

        let user = app
            .json(Method::POST, "/api/users", Some(&admin), serde_json::json!({
                "username": "carol", "password": "carol-password", "connections": ["new-desktop"]
            }))
            .await;
        assert_eq!(user.status(), StatusCode::CREATED);
        let user = body_json(user).await;
        assert_eq!(user["connections"], serde_json::json!(["new-desktop"]));
        let short = app
            .json(
                Method::POST,
                "/api/users",
                Some(&admin),
                serde_json::json!({ "username": "dave", "password": "short" }),
            )
            .await;
        assert_eq!(short.status(), StatusCode::BAD_REQUEST);

        let carol = app.login("carol", "carol-password").await;
        let mine = body_json(app.get("/api/connections", Some(&carol)).await).await;
        assert_eq!(mine[0]["hostname"], "10.0.0.9");

        // The last administrator cannot lock everyone out.
        let me = body_json(app.get("/api/auth/me", Some(&admin)).await).await;
        let path = format!("/api/users/{}", me["id"].as_str().unwrap());
        let demote = app
            .json(
                Method::PUT,
                &path,
                Some(&admin),
                serde_json::json!({ "isAdmin": false }),
            )
            .await;
        assert_eq!(demote.status(), StatusCode::BAD_REQUEST);
        let delete = Request::delete(&path)
            .header(header::COOKIE, &admin)
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.send(delete).await.status(), StatusCode::BAD_REQUEST);

        // Disabling a user ends their sessions.
        let path = format!("/api/users/{}", user["id"].as_str().unwrap());
        let disable = app
            .json(
                Method::PUT,
                &path,
                Some(&admin),
                serde_json::json!({ "disabled": true }),
            )
            .await;
        assert_eq!(disable.status(), StatusCode::OK);
        assert_eq!(
            app.get("/api/auth/me", Some(&carol)).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn changing_the_password_requires_the_current_one() {
        let app = app().await;
        let bob = app.login("bob", "bob-password").await;
        let wrong = app
            .json(
                Method::PUT,
                "/api/auth/password",
                Some(&bob),
                serde_json::json!({
                    "currentPassword": "nope", "newPassword": "new-password"
                }),
            )
            .await;
        assert_eq!(wrong.status(), StatusCode::BAD_REQUEST);
        let ok = app
            .json(
                Method::PUT,
                "/api/auth/password",
                Some(&bob),
                serde_json::json!({
                    "currentPassword": "bob-password", "newPassword": "new-password"
                }),
            )
            .await;
        assert_eq!(ok.status(), StatusCode::NO_CONTENT);
        // Every session ends with the old password.
        assert_eq!(
            app.get("/api/auth/me", Some(&bob)).await.status(),
            StatusCode::UNAUTHORIZED
        );
        app.login("bob", "new-password").await;
    }

    #[tokio::test]
    async fn refuses_cross_origin_changes() {
        let app = app().await;
        let request = Request::post("/api/auth/login")
            .header(header::HOST, "gateway.test")
            .header(header::ORIGIN, "https://evil.test")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"username":"admin","password":"admin-password"}"#,
            ))
            .unwrap();
        assert_eq!(app.send(request).await.status(), StatusCode::FORBIDDEN);
        let form = Request::post("/api/auth/login")
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(Body::from("username=admin&password=admin-password"))
            .unwrap();
        assert_eq!(
            app.send(form).await.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );
    }

    #[tokio::test]
    async fn tunnel_requires_a_session_and_permission() {
        let app = app().await;
        let bob = app.login("bob", "bob-password").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = router(app.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await });

        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        let ws_request = |query: &str, cookie: Option<&str>| {
            let mut request = format!("ws://{addr}/websocket-tunnel?{query}")
                .into_client_request()
                .unwrap();
            request
                .headers_mut()
                .insert("sec-websocket-protocol", "guacamole".parse().unwrap());
            if let Some(cookie) = cookie {
                request
                    .headers_mut()
                    .insert("cookie", cookie.parse().unwrap());
            }
            request
        };

        let anonymous =
            tokio_tungstenite::connect_async(ws_request("connection=granted", None)).await;
        match anonymous {
            Err(tokio_tungstenite::tungstenite::Error::Http(response)) => {
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            }
            other => panic!("expected 401, got {other:?}"),
        }

        // Not granted: the tunnel opens and closes with RESOURCE_NOT_FOUND,
        // exactly like an unknown connection.
        let (mut socket, _) =
            tokio_tungstenite::connect_async(ws_request("connection=secret", Some(&bob)))
                .await
                .unwrap();
        let message = socket.next().await.unwrap().unwrap();
        let tokio_tungstenite::tungstenite::Message::Close(Some(frame)) = message else {
            panic!("expected close, got {message:?}");
        };
        assert_eq!(frame.reason.as_str(), "516");
    }

    impl TestApp {
        /// Sends `body` as admin and returns the JSON answer, asserting the
        /// status.
        async fn call(
            &self,
            cookie: &str,
            method: Method,
            path: &str,
            body: serde_json::Value,
            expected: StatusCode,
        ) -> serde_json::Value {
            let response = self.json(method.clone(), path, Some(cookie), body).await;
            let status = response.status();
            let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
                .await
                .unwrap();
            assert_eq!(
                status,
                expected,
                "{method} {path}: {}",
                String::from_utf8_lossy(&bytes)
            );
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
        }

        /// Ids of the connections `cookie`'s user sees, sorted.
        async fn visible(&self, cookie: &str) -> Vec<String> {
            let list = body_json(self.get("/api/connections", Some(cookie)).await).await;
            let mut ids: Vec<String> = list
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["id"].as_str().unwrap().to_owned())
                .collect();
            ids.sort();
            ids
        }

        /// Serves the gateway on a local port for WebSocket tests.
        async fn serve(&self) -> SocketAddr {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let service =
                router(self.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
            tokio::spawn(async move { axum::serve(listener, service).await });
            addr
        }
    }

    /// The status the tunnel to `connection` closes with, as `cookie`.
    async fn tunnel_close_status(addr: SocketAddr, connection: &str, cookie: &str) -> String {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let mut request = format!("ws://{addr}/websocket-tunnel?connection={connection}")
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert("sec-websocket-protocol", "guacamole".parse().unwrap());
        headers.insert("cookie", cookie.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        loop {
            match socket.next().await.unwrap().unwrap() {
                tokio_tungstenite::tungstenite::Message::Close(Some(frame)) => {
                    return frame.reason.to_string();
                }
                tokio_tungstenite::tungstenite::Message::Text(_) => continue,
                other => panic!("expected close, got {other:?}"),
            }
        }
    }

    const NOT_FOUND: &str = "516";
    /// Allowed, but the test's guacd port is closed.
    const ALLOWED: &str = "519";

    #[tokio::test]
    async fn groups_and_folders_grant_connections() {
        // Port 1: nothing listens, so an allowed tunnel fails upstream.
        let app = app_with("[guacd]\nhost = \"127.0.0.1\"\nport = 1\n").await;
        let admin = app.login("admin", "admin-password").await;
        let (post, put) = (Method::POST, Method::PUT);

        let databases = app
            .call(
                &admin,
                post.clone(),
                "/api/folders",
                serde_json::json!({ "name": "Databases" }),
                StatusCode::CREATED,
            )
            .await;
        let databases = databases["id"].as_str().unwrap().to_owned();
        let prod = app
            .call(
                &admin,
                post.clone(),
                "/api/folders",
                serde_json::json!({ "name": "Prod", "parentId": databases }),
                StatusCode::CREATED,
            )
            .await;
        let prod = prod["id"].as_str().unwrap().to_owned();
        for (id, parent) in [
            ("pg", Some(&databases)),
            ("mysql", Some(&prod)),
            ("loose", None),
        ] {
            app.call(
                &admin,
                post.clone(),
                "/api/connections",
                serde_json::json!({ "id": id, "name": id, "protocol": "vnc", "parentId": parent }),
                StatusCode::CREATED,
            )
            .await;
        }

        // carol may open `granted` herself; her group gets Databases.
        let carol = app
            .call(
                &admin,
                post.clone(),
                "/api/users",
                serde_json::json!({
                    "username": "carol", "password": "carol-password", "connections": ["granted"]
                }),
                StatusCode::CREATED,
            )
            .await;
        let carol_id = carol["id"].as_str().unwrap().to_owned();
        let group = app
            .call(
                &admin,
                post.clone(),
                "/api/groups",
                serde_json::json!({
                    "name": "db-team", "members": [carol_id], "folders": [databases]
                }),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(group["memberCount"], 1);
        let group_path = format!("/api/groups/{}", group["id"].as_str().unwrap());

        let cookie = app.login("carol", "carol-password").await;
        assert_eq!(app.visible(&cookie).await, ["granted", "mysql", "pg"]);
        let folders = body_json(app.get("/api/folders", Some(&cookie)).await).await;
        assert_eq!(folders.as_array().unwrap().len(), 2);

        let addr = app.serve().await;
        assert_eq!(tunnel_close_status(addr, "mysql", &cookie).await, ALLOWED);
        assert_eq!(tunnel_close_status(addr, "granted", &cookie).await, ALLOWED);
        for refused in ["secret", "loose", "missing"] {
            assert_eq!(tunnel_close_status(addr, refused, &cookie).await, NOT_FOUND);
        }

        // Moving a connection into a granted folder gives access; moving
        // it out takes it away.
        let into_prod = serde_json::json!({ "parentId": prod });
        app.call(
            &admin,
            put.clone(),
            "/api/connections/loose/parent",
            into_prod,
            StatusCode::NO_CONTENT,
        )
        .await;
        assert!(app.visible(&cookie).await.contains(&"loose".to_owned()));
        assert_eq!(tunnel_close_status(addr, "loose", &cookie).await, ALLOWED);
        let to_top = serde_json::json!({ "parentId": null });
        app.call(
            &admin,
            put.clone(),
            "/api/connections/loose/parent",
            to_top,
            StatusCode::NO_CONTENT,
        )
        .await;
        assert!(!app.visible(&cookie).await.contains(&"loose".to_owned()));
        assert_eq!(tunnel_close_status(addr, "loose", &cookie).await, NOT_FOUND);

        // Granting from the connection's side.
        let access = "/api/connections/secret/access";
        let granted = app
            .call(
                &admin,
                put.clone(),
                access,
                serde_json::json!({ "users": [], "groups": [group["id"]] }),
                StatusCode::OK,
            )
            .await;
        assert_eq!(granted["groups"], serde_json::json!([group["id"]]));
        assert!(app.visible(&cookie).await.contains(&"secret".to_owned()));
        app.call(
            &admin,
            put.clone(),
            access,
            serde_json::json!({ "users": [], "groups": [] }),
            StatusCode::OK,
        )
        .await;

        // Leaving the group takes its connections away, not carol's own.
        app.call(
            &admin,
            put.clone(),
            &group_path,
            serde_json::json!({ "members": [] }),
            StatusCode::OK,
        )
        .await;
        assert_eq!(app.visible(&cookie).await, ["granted"]);
        assert_eq!(tunnel_close_status(addr, "mysql", &cookie).await, NOT_FOUND);
        let detail = body_json(
            app.get(&format!("/api/users/{carol_id}"), Some(&admin))
                .await,
        )
        .await;
        assert_eq!(detail["connections"], serde_json::json!(["granted"]));
        assert_eq!(detail["groups"], serde_json::json!([]));
        assert!(
            body_json(app.get("/api/folders", Some(&cookie)).await)
                .await
                .as_array()
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn roles_limit_what_users_manage() {
        let app = app().await;
        let admin = app.login("admin", "admin-password").await;
        let (post, put) = (Method::POST, Method::PUT);
        let manager = app
            .call(
                &admin,
                post.clone(),
                "/api/users",
                serde_json::json!({
                    "username": "manny", "password": "manny-password", "role": "connection_manager"
                }),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(
            (manager["role"].as_str(), manager["isAdmin"].as_bool()),
            (Some("connection_manager"), Some(false))
        );
        let bob_id = app.state.db.user_credentials("bob").await.unwrap().0.id;
        let manny = app.login("manny", "manny-password").await;

        // A connection manager sees every connection, manages connections
        // and folders and grants them ...
        assert_eq!(app.visible(&manny).await, ["granted", "secret"]);
        let folder = app
            .call(
                &manny,
                post.clone(),
                "/api/folders",
                serde_json::json!({ "name": "Team" }),
                StatusCode::CREATED,
            )
            .await;
        let folder_path = format!("/api/folders/{}", folder["id"].as_str().unwrap());
        app.call(
            &manny,
            put.clone(),
            &folder_path,
            serde_json::json!({ "name": "Team 2" }),
            StatusCode::OK,
        )
        .await;
        app.call(
            &manny,
            put.clone(),
            "/api/connections/secret/parent",
            serde_json::json!({ "parentId": folder["id"] }),
            StatusCode::NO_CONTENT,
        )
        .await;
        app.call(
            &manny,
            put.clone(),
            &format!("{folder_path}/access"),
            serde_json::json!({ "users": [bob_id], "groups": [] }),
            StatusCode::OK,
        )
        .await;
        let bob = app.login("bob", "bob-password").await;
        assert_eq!(app.visible(&bob).await, ["granted", "secret"]);
        assert_eq!(
            app.get("/api/users", Some(&manny)).await.status(),
            StatusCode::OK
        );
        assert_eq!(
            app.get("/api/groups", Some(&manny)).await.status(),
            StatusCode::OK
        );
        // ... but not users, groups or roles.
        let path = format!("/api/users/{bob_id}");
        assert_eq!(
            app.get(&path, Some(&manny)).await.status(),
            StatusCode::FORBIDDEN
        );
        app.call(
            &manny,
            put.clone(),
            &path,
            serde_json::json!({ "role": "admin" }),
            StatusCode::FORBIDDEN,
        )
        .await;
        app.call(
            &manny,
            post.clone(),
            "/api/groups",
            serde_json::json!({ "name": "x" }),
            StatusCode::FORBIDDEN,
        )
        .await;
        // Users manage nothing.
        app.call(
            &bob,
            post.clone(),
            "/api/folders",
            serde_json::json!({ "name": "x" }),
            StatusCode::FORBIDDEN,
        )
        .await;
        assert_eq!(
            app.get("/api/users", Some(&bob)).await.status(),
            StatusCode::FORBIDDEN
        );

        // A folder cannot go into itself.
        app.call(
            &manny,
            put.clone(),
            &format!("{folder_path}/parent"),
            serde_json::json!({ "parentId": folder["id"] }),
            StatusCode::BAD_REQUEST,
        )
        .await;

        // A group's role applies to its members.
        let admins = app
            .call(
                &admin,
                post.clone(),
                "/api/groups",
                serde_json::json!({ "name": "admins", "role": "admin", "members": [bob_id] }),
                StatusCode::CREATED,
            )
            .await;
        let me = body_json(app.get("/api/auth/me", Some(&bob)).await).await;
        assert_eq!(
            (
                me["role"].as_str(),
                me["effectiveRole"].as_str(),
                me["isAdmin"].as_bool()
            ),
            (Some("user"), Some("admin"), Some(true))
        );
        assert_eq!(app.get(&path, Some(&bob)).await.status(), StatusCode::OK);

        // Removing bob's group leaves admin as the only administrator, who
        // cannot then remove their own role.
        let admins_path = format!("/api/groups/{}", admins["id"].as_str().unwrap());
        let delete = Request::delete(&admins_path)
            .header(header::COOKIE, &admin)
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.send(delete).await.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            app.get(&path, Some(&bob)).await.status(),
            StatusCode::FORBIDDEN
        );
        let me = body_json(app.get("/api/auth/me", Some(&admin)).await).await;
        app.call(
            &admin,
            put.clone(),
            &format!("/api/users/{}", me["id"].as_str().unwrap()),
            serde_json::json!({ "role": "connection_manager" }),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }

    /// What a fake guacd saw: the `connect` instruction and everything the
    /// gateway sent after the handshake.
    type GuacdSaw = (molcajete_protocol::Instruction, Vec<u8>);

    /// A guacd that answers `select` with `args`, `connect` with `ready`
    /// followed by `payload`, and then reads until the gateway hangs up.
    async fn fake_guacd(
        args: &'static str,
        payload: Vec<u8>,
    ) -> (SocketAddr, tokio::task::JoinHandle<GuacdSaw>) {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut parser = molcajete_protocol::Parser::new();
            let mut buf = [0u8; 4096];
            let connect = 'handshake: loop {
                while let Some(ins) = parser.next_instruction().unwrap() {
                    match ins.opcode.as_str() {
                        "select" => sock.write_all(args.as_bytes()).await.unwrap(),
                        "connect" => break 'handshake ins,
                        _ => {}
                    }
                }
                let n = sock.read(&mut buf).await.unwrap();
                assert_ne!(n, 0, "gateway hung up during the handshake");
                parser.feed(&buf[..n]);
            };
            // The first bytes arrive in the same segment as `ready`, cut in
            // the middle of a record header or an instruction: the relay
            // has to take them over from the handshake parser.
            let (with_ready, payload) = payload.split_at(payload.len().min(7));
            sock.write_all(&[b"5.ready,5.$abcd;".as_slice(), with_ready].concat())
                .await
                .unwrap();
            // In pieces, so that records and instructions arrive split.
            for piece in payload.chunks(1000) {
                sock.write_all(piece).await.unwrap();
                sock.flush().await.unwrap();
                tokio::task::yield_now().await;
            }
            let mut after = parser.pending().to_vec();
            let _ = sock.read_to_end(&mut after).await;
            (connect, after)
        });
        (addr, task)
    }

    const STOCK_ARGS: &str = "4.args,13.VERSION_1_5_0,8.hostname,4.port;";
    const MOLCAJETED_ARGS: &str = "4.args,13.VERSION_1_5_0,8.hostname,4.port,16.molcajete-binary;";

    /// Data of an image stream that base64 would not survive as text: every
    /// byte value, and enough of it for several messages.
    fn image_data() -> Vec<u8> {
        (0..200_000u32).map(|i| (i ^ (i >> 8)) as u8).collect()
    }

    const IMG: &str = "3.img,1.1,2.14,1.0,9.image/png,1.0,1.0;";
    const END: &str = "3.end,1.1;4.sync,2.42;";

    /// The frame of [`image_data`] as stock guacd sends it.
    fn text_payload() -> String {
        use base64::Engine as _;

        let mut out = String::from(IMG);
        for chunk in image_data().chunks(6048) {
            let b64 = base64::engine::general_purpose::STANDARD.encode(chunk);
            out += &molcajete_protocol::InstructionBuilder::new("blob")
                .arg(1)
                .str_arg(&b64)
                .finish();
        }
        out + END
    }

    /// The same frame as molcajeted sends it to a gateway that asked.
    fn record_payload() -> Vec<u8> {
        let mut writer = molcajete_protocol::binary::RecordWriter::new();
        writer.instructions(IMG.as_bytes());
        writer.blob(1, &image_data());
        writer.instructions(END.as_bytes());
        writer.take()
    }

    struct TunnelRun {
        /// The subprotocol the gateway selected.
        selected: String,
        /// `Sec-WebSocket-Extensions` of the response, if any.
        extensions: Option<String>,
        /// Text messages, concatenated.
        text: String,
        /// Binary messages, one entry each.
        binary: Vec<Vec<u8>>,
        guacd: GuacdSaw,
    }

    impl TunnelRun {
        /// The text messages without the tunnel's own instructions (the
        /// UUID and the ping reply, which may arrive anywhere in between).
        fn relayed_text(&self) -> String {
            let uuid_len = "0.,36.;".len() + 36;
            assert!(self.text.starts_with("0.,36."), "{}", self.text);
            self.text[uuid_len..].replace("0.,4.ping,1.7;", "")
        }

        /// Instruction text and stream data carried by the binary messages,
        /// each of which must hold whole records.
        fn records(&self) -> (String, Vec<u8>) {
            use molcajete_protocol::binary::{Record, records};

            let mut text = Vec::new();
            let mut data = Vec::new();
            for message in &self.binary {
                for record in records(message) {
                    match record.expect("whole records in every message") {
                        Record::Instructions(part) => text.extend_from_slice(part),
                        Record::Blob { stream, data: part } => {
                            assert_eq!(stream, 1);
                            data.extend_from_slice(part);
                        }
                        Record::Unknown { tag, .. } => panic!("unknown record {tag}"),
                    }
                }
            }
            (String::from_utf8(text).unwrap(), data)
        }
    }

    /// Opens the tunnel of connection `granted` against a fake guacd, as a
    /// client offering `offered`, and collects what arrives up to the
    /// `sync` that ends the payload.
    async fn run_tunnel(
        server: &str,
        offered: &str,
        args: &'static str,
        payload: Vec<u8>,
    ) -> TunnelRun {
        use futures_util::SinkExt as _;
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let (guacd_addr, guacd) = fake_guacd(args, payload).await;
        let app = app_with(&format!(
            "{server}\n[guacd]\nhost = \"127.0.0.1\"\nport = {}\n",
            guacd_addr.port()
        ))
        .await;
        let bob = app.login("bob", "bob-password").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = router(app.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await });

        let mut request = format!("ws://{addr}/websocket-tunnel?connection=granted")
            .into_client_request()
            .unwrap();
        let headers = request.headers_mut();
        headers.insert("sec-websocket-protocol", offered.parse().unwrap());
        // What browsers send; the gateway must not agree to compress.
        headers.insert(
            "sec-websocket-extensions",
            "permessage-deflate; client_max_window_bits"
                .parse()
                .unwrap(),
        );
        headers.insert("cookie", bob.parse().unwrap());
        let (mut socket, response) = tokio_tungstenite::connect_async(request).await.unwrap();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_owned())
        };

        // Client → server stays text in every mode; pings are answered by
        // the gateway itself.
        socket
            .send(Message::Text("0.,4.ping,1.7;3.nop;".into()))
            .await
            .unwrap();

        let mut text = String::new();
        let mut binary = Vec::new();
        let collect = async {
            let (mut pinged, mut synced) = (false, false);
            while !(pinged && synced) {
                match socket.next().await.expect("tunnel stays open").unwrap() {
                    Message::Text(message) => {
                        pinged |= message.contains("0.,4.ping,1.7;");
                        synced |= message.contains("4.sync,2.42;");
                        text += message.as_str();
                    }
                    Message::Binary(message) => {
                        synced |= message.ends_with(b"4.sync,2.42;");
                        binary.push(message.to_vec());
                    }
                    other => panic!("unexpected {other:?}"),
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), collect)
            .await
            .expect("the whole payload arrives");
        socket.close(None).await.unwrap();
        drop(socket);
        TunnelRun {
            selected: header("sec-websocket-protocol").unwrap_or_default(),
            extensions: header("sec-websocket-extensions"),
            text,
            binary,
            guacd: guacd.await.unwrap(),
        }
    }

    const BOTH: &str = "molcajete-binary-v1, guacamole";

    /// The tunnel's own messages, which are text in every mode.
    fn assert_internal_only(text: &str) {
        let instructions = molcajete_protocol::parse_all(text).unwrap();
        assert_eq!(instructions.len(), 2, "{text}");
        assert!(instructions.iter().all(|i| i.opcode.is_empty()), "{text}");
        assert_eq!(instructions[0].args[0].len(), 36);
        assert_eq!(instructions[1].args, ["ping", "7"]);
    }

    #[tokio::test]
    async fn binary_tunnel_forwards_records_of_molcajeted() {
        let run = run_tunnel("", BOTH, MOLCAJETED_ARGS, record_payload()).await;
        assert_eq!(run.selected, "molcajete-binary-v1");
        assert_eq!(run.extensions, None);
        assert_internal_only(&run.text);
        assert_eq!(run.records(), (format!("{IMG}{END}"), image_data()));
        // Bytes pass through unchanged, in bounded messages.
        assert_eq!(run.binary.concat(), record_payload());
        assert!(run.binary.iter().all(|m| m.len() <= 64 * 1024));
        // The daemon was asked for records; the browser's input is relayed
        // as text, without the ping.
        assert_eq!(run.guacd.0.args, ["VERSION_1_5_0", "", "", "1"]);
        assert_eq!(run.guacd.1, b"3.nop;");
    }

    #[tokio::test]
    async fn binary_tunnel_decodes_blobs_of_stock_guacd() {
        let run = run_tunnel("", BOTH, STOCK_ARGS, text_payload().into_bytes()).await;
        assert_eq!(run.selected, "molcajete-binary-v1");
        assert_eq!(run.extensions, None);
        assert_internal_only(&run.text);
        assert_eq!(run.records(), (format!("{IMG}{END}"), image_data()));
        assert_eq!(run.guacd.0.args, ["VERSION_1_5_0", "", ""]);
        assert_eq!(run.guacd.1, b"3.nop;");
    }

    #[tokio::test]
    async fn stock_client_gets_text_from_molcajeted() {
        // molcajeted answers an empty value with the text protocol.
        let run = run_tunnel(
            "",
            "guacamole",
            MOLCAJETED_ARGS,
            text_payload().into_bytes(),
        )
        .await;
        assert_eq!(run.selected, "guacamole");
        assert_eq!(run.extensions, None);
        assert!(run.binary.is_empty());
        assert!(run.relayed_text() == text_payload(), "payload unchanged");
        assert_eq!(run.guacd.0.args, ["VERSION_1_5_0", "", "", ""]);
        assert_eq!(run.guacd.1, b"3.nop;");
    }

    #[tokio::test]
    async fn stock_client_gets_text_from_stock_guacd() {
        let run = run_tunnel("", "guacamole", STOCK_ARGS, text_payload().into_bytes()).await;
        assert_eq!(run.selected, "guacamole");
        assert_eq!(run.extensions, None);
        assert!(run.binary.is_empty());
        assert!(run.relayed_text() == text_payload(), "payload unchanged");
        assert_eq!(run.guacd.0.args, ["VERSION_1_5_0", "", ""]);
    }

    #[tokio::test]
    async fn binary_tunnel_can_be_turned_off() {
        let run = run_tunnel(
            "binary_tunnel = false",
            BOTH,
            MOLCAJETED_ARGS,
            text_payload().into_bytes(),
        )
        .await;
        assert_eq!(run.selected, "guacamole");
        assert_eq!(run.extensions, None);
        assert!(run.binary.is_empty());
        assert!(run.relayed_text() == text_payload(), "payload unchanged");
        assert_eq!(run.guacd.0.args, ["VERSION_1_5_0", "", "", ""]);
    }

    #[test]
    fn client_address_prefers_the_edge_proxy_header() {
        let peer: SocketAddr = "10.0.0.2:5000".parse().unwrap();
        let mut headers = HeaderMap::new();
        assert_eq!(client_address(&headers, peer), "10.0.0.2");
        headers.insert("x-forwarded-for", "100.96.0.1, 10.0.0.1".parse().unwrap());
        assert_eq!(client_address(&headers, peer), "100.96.0.1 (via 10.0.0.2)");
        headers.insert("x-real-ip", "203.0.113.7".parse().unwrap());
        assert_eq!(client_address(&headers, peer), "203.0.113.7 (via 10.0.0.2)");
    }
}
