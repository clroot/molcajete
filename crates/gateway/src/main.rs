//! molcajete web gateway.
//!
//! Replaces the Java web application of Apache Guacamole: serves the web
//! client, keeps users and connections in a database (PostgreSQL, MySQL or
//! SQLite), and tunnels the Guacamole protocol between the browser
//! (WebSocket) and guacd (TCP).

mod api;
mod audit;
mod auth;
mod bootstrap;
mod client_report;
mod config;
mod db;
mod guacd;
mod oidc;
mod proxy;
mod recording;
mod secrets;
mod sensitive;
mod sessions;
mod throttle;
mod tunnel;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, RawQuery, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
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
    /// Re-encrypts every stored connection credential with the current
    /// secret key. Values under a retired key are read with the keys in
    /// `[secrets] previous_key_files` / `previous_keys`, which can be
    /// removed afterwards.
    RotateKey,
}

pub struct AppState {
    pub config: Config,
    pub db: Database,
    pub client_reports: bool,
    /// Which tunnel each page session opened, for client reports.
    pub tunnels: client_report::Tunnels,
    /// The remote desktop sessions open right now.
    pub sessions: Arc<sessions::Registry>,
    /// Single sign-on, when `[oidc]` is configured.
    pub oidc: Option<oidc::Oidc>,
    /// Failed password sign-ins and the Argon2 verification slots.
    pub throttle: throttle::LoginThrottle,
}

impl AppState {
    /// Whether the username and password form is offered.
    pub fn password_login(&self) -> bool {
        self.oidc.as_ref().is_none_or(|o| o.config.password_login)
    }
}

/// Lets the page know which optional features are on.
async fn client_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "clientReports": state.client_reports,
        "recordings": state.config.recordings.enabled,
    }))
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
    let client = proxy::resolve(&headers, Some(peer), &state.config.server.trusted_proxies);
    request.peer = Some(client.display);
    request.user = Some(recording::RecordedUser {
        id: user.id.clone(),
        username: user.username.clone(),
    });
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
        .route("/recordings", get(api::list_recordings))
        .route(
            "/recordings/{id}",
            get(api::get_recording).delete(api::delete_recording),
        )
        .route("/recordings/{id}/file", get(api::recording_file))
        .route("/audit", get(api::list_audit))
        .route("/audit/actions", get(api::audit_actions))
        .route(
            "/sessions",
            get(sessions::api::list).delete(sessions::api::end_all),
        )
        .route("/sessions/{id}", delete(sessions::api::end))
        .route("/history", get(sessions::api::history))
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

/// Loads the secret key for stored connection credentials (generating
/// `<data_dir>/secret.key` on first start) and hands it to the database.
async fn unlock_secrets(db: Database, config: &Config) -> anyhow::Result<Database> {
    let keys = secrets::load(config, db.has_sealed_secrets().await?)?;
    Ok(db.with_secrets(keys))
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
    if let Some(Command::RotateKey) = cli.command {
        let db = unlock_secrets(open_database(&config).await?, &config).await?;
        let rotated = db
            .rotate_secrets()
            .await
            .context("re-encrypting the stored connection credentials")?;
        let entry = audit::Entry::new(audit::action::SECRETS_ROTATE)
            .source("command line")
            .detail("count", rotated)
            .detail("keyId", db.secret_key_id().unwrap_or_default());
        audit::record(&db, entry).await;
        eprintln!(
            "Re-encrypted {rotated} stored credential(s). Every stored credential is now under \
             the current key; previous keys can be removed from the configuration."
        );
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
    let db = unlock_secrets(open_database(&config).await?, &config).await?;
    let sealed = db
        .seal_plaintext_secrets()
        .await
        .context("checking the stored connection credentials against the configured secret key")?;
    if sealed > 0 {
        info!(
            count = sealed,
            "encrypted stored connection credentials that were in plain text"
        );
        let entry = audit::Entry::new(audit::action::SECRETS_ENCRYPT)
            .source("gateway start")
            .detail("count", sealed);
        audit::record(&db, entry).await;
    }
    bootstrap::seed(&db, &config).await?;
    if config.recordings.enabled {
        let directory = config.recordings.directory(&config.server);
        info!(dir = %directory.display(), "session recordings enabled");
        recording::start_maintenance(db.clone(), config.recordings.clone(), directory).await;
    }

    audit::start_maintenance(db.clone(), config.audit.clone());
    sessions::history::start_maintenance(db.clone(), config.history.clone());

    let state = Arc::new(AppState {
        throttle: throttle::LoginThrottle::new(&config.auth.throttle),
        config,
        db,
        client_reports: cli.client_reports,
        tunnels: Default::default(),
        sessions: Default::default(),
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
    use crate::db::attributes;
    use axum::body::Body;
    use axum::http::Request;
    use futures_util::StreamExt as _;
    use std::time::Duration;
    use tower::ServiceExt as _;

    use super::*;

    /// Live sessions and the connection history (src/tests/live_sessions.rs).
    mod live_sessions;

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
        let (db, data) = db::tests::sqlite().await;
        let toml = format!(
            "[server]\nstatic_dir = {:?}\ndata_dir = {:?}\n{extra}\n\
             [[users]]\nusername = \"admin\"\npassword = \"admin-password\"\n\
             [[connections]]\nid = \"granted\"\nprotocol = \"vnc\"\n\
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
            throttle: throttle::LoginThrottle::new(&config.auth.throttle),
            config,
            db,
            client_reports: false,
            tunnels: Default::default(),
            sessions: Default::default(),
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
        assert_eq!(me["hasPassword"], true);

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

    /// What a fake guacd saw: the `connect` instruction, everything the
    /// gateway sent after the handshake, and the handshake before `connect`.
    type GuacdSaw = (
        molcajete_protocol::Instruction,
        Vec<u8>,
        Vec<molcajete_protocol::Instruction>,
    );

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
            let mut handshake = Vec::new();
            let connect = 'handshake: loop {
                while let Some(ins) = parser.next_instruction().unwrap() {
                    match ins.opcode.as_str() {
                        "select" => sock.write_all(args.as_bytes()).await.unwrap(),
                        "connect" => break 'handshake ins,
                        _ => handshake.push(ins),
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
            (connect, after, handshake)
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
        app: TestApp,
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
        run_tunnel_with(server, offered, args, payload, &[], &[], "3.nop;").await
    }

    /// Like [`run_tunnel`], with these flag `attributes` and `parameters`
    /// set on the connection, and `input` sent by the browser.
    async fn run_tunnel_with(
        server: &str,
        offered: &str,
        args: &'static str,
        payload: Vec<u8>,
        attributes: &[&str],
        parameters: &[(&str, &str)],
        input: &str,
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
        if !attributes.is_empty() || !parameters.is_empty() {
            let mut granted = app.state.db.connection("granted").await.unwrap();
            for (name, value) in parameters {
                granted
                    .parameters
                    .insert((*name).to_owned(), (*value).to_owned());
            }
            for name in attributes {
                granted
                    .attributes
                    .insert((*name).to_owned(), "true".to_owned());
            }
            app.state.db.update_connection(&granted).await.unwrap();
        }
        let bob = app.login("bob", "bob-password").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = router(app.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await });

        let mut request = format!(
            "ws://{addr}/websocket-tunnel?connection=granted\
             &image=image%2Fpng&image=image%2Fx-molcajete-zstd-v1&video=video%2Fh264"
        )
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
            .send(Message::Text(format!("0.,4.ping,1.7;{input}").into()))
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
            app,
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

    #[tokio::test]
    async fn stored_credentials_reach_guacd_decrypted() {
        const ARGS: &str = "4.args,13.VERSION_1_5_0,8.hostname,8.password,11.private-key;";
        let key =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3Blbg==\n-----END OPENSSH PRIVATE KEY-----";
        let run = run_tunnel_with(
            "",
            "guacamole",
            ARGS,
            text_payload().into_bytes(),
            &[],
            &[
                ("hostname", "10.0.0.5"),
                ("password", "hunter2"),
                ("private-key", key),
            ],
            "3.nop;",
        )
        .await;
        assert_eq!(
            run.guacd.0.args,
            ["VERSION_1_5_0", "10.0.0.5", "hunter2", key]
        );
        for name in ["password", "private-key"] {
            let stored = run.app.state.db.stored_parameter("granted", name).await;
            let stored = stored.unwrap();
            assert!(secrets::is_sealed(&stored), "{stored}");
            assert!(!stored.contains("hunter2") && !stored.contains("OPENSSH"));
        }
    }

    #[tokio::test]
    async fn stored_credentials_are_write_only() {
        let app = app().await;
        let admin = app.login("admin", "admin-password").await;
        let (post, put) = (Method::POST, Method::PUT);
        app.call(
            &admin,
            post.clone(),
            "/api/users",
            serde_json::json!({
                "username": "manny", "password": "manny-password", "role": "connection_manager"
            }),
            StatusCode::CREATED,
        )
        .await;
        let manny = app.login("manny", "manny-password").await;

        let created = app
            .call(
                &admin,
                post,
                "/api/connections",
                serde_json::json!({
                    "id": "box", "name": "Box", "protocol": "ssh",
                    "parameters": {
                        "hostname": "10.0.0.7", "password": "hunter2", "passphrase": "key pass"
                    }
                }),
                StatusCode::CREATED,
            )
            .await;
        assert_eq!(
            created["parameters"],
            serde_json::json!({ "hostname": "10.0.0.7" })
        );
        assert_eq!(
            created["storedSecrets"],
            serde_json::json!(["passphrase", "password"])
        );
        assert!(created.get("clearSecrets").is_none());

        // Nobody reads them back, administrators included.
        let mut form = serde_json::Value::Null;
        for cookie in [&admin, &manny] {
            let response = app.get("/api/connections/box", Some(cookie)).await;
            assert_eq!(response.status(), StatusCode::OK);
            form = body_json(response).await;
            let text = form.to_string();
            assert!(
                !text.contains("hunter2") && !text.contains("key pass"),
                "{text}"
            );
            assert_eq!(form["storedSecrets"], created["storedSecrets"]);
        }

        // Saving the form as it came back keeps them ...
        form["name"] = "Box 2".into();
        app.call(
            &manny,
            put.clone(),
            "/api/connections/box",
            form.clone(),
            StatusCode::OK,
        )
        .await;
        let secrets = app.state.db.connection_secrets("box").await.unwrap();
        assert_eq!(
            (secrets["password"].as_str(), secrets["passphrase"].as_str()),
            ("hunter2", "key pass")
        );

        // ... a new value replaces one, and an explicit clear removes one.
        form["parameters"]["password"] = "new password".into();
        form["clearSecrets"] = serde_json::json!(["passphrase"]);
        let saved = app
            .call(&manny, put, "/api/connections/box", form, StatusCode::OK)
            .await;
        assert_eq!(saved["storedSecrets"], serde_json::json!(["password"]));
        assert!(!saved.to_string().contains("new password"));
        let secrets = app.state.db.connection_secrets("box").await.unwrap();
        assert_eq!(
            secrets,
            [("password".to_owned(), "new password".to_owned())].into()
        );
        let stored = app.state.db.stored_parameter("box", "password").await;
        assert!(!stored.unwrap().contains("new password"));
    }

    /// The recording of the session that ended last, once it is complete,
    /// and the text of its file.
    async fn finished_recording(app: &TestApp) -> (db::Recording, String) {
        for _ in 0..400 {
            let all = app.state.db.list_recordings().await.unwrap();
            if let Some(recording) = all.into_iter().next()
                && recording.ended_at.is_some()
            {
                let directory = app
                    .state
                    .config
                    .recordings
                    .directory(&app.state.config.server);
                let path = recording::file_path(&directory, &recording).unwrap();
                let text = std::fs::read_to_string(path).unwrap();
                return (recording, text);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("no complete recording");
    }

    /// The `mime` arguments the browser announced to guacd for `opcode`.
    fn announced(run: &TunnelRun, opcode: &str) -> Vec<String> {
        let found = run.guacd.2.iter().find(|i| i.opcode == opcode);
        found.map(|i| i.args.clone()).unwrap_or_default()
    }

    const INPUT: &str = "5.mouse,2.10,2.20,1.1;3.key,5.65307,1.1;";

    #[tokio::test]
    async fn recordings_hold_what_the_browser_got_as_guacamole_text() {
        for (offered, args, payload) in [
            (BOTH, MOLCAJETED_ARGS, record_payload()),
            (BOTH, STOCK_ARGS, text_payload().into_bytes()),
            ("guacamole", MOLCAJETED_ARGS, text_payload().into_bytes()),
            ("guacamole", STOCK_ARGS, text_payload().into_bytes()),
        ] {
            let started = db::now_millis();
            let run = run_tunnel_with(
                "",
                offered,
                args,
                payload,
                &[attributes::RECORD],
                &[],
                INPUT,
            )
            .await;
            let (recording, text) = finished_recording(&run.app).await;
            assert_eq!(recording.connection_id, "granted");
            assert_eq!(recording.username, "bob");
            assert!(!recording.include_keys);
            assert!(recording.started_at >= started);
            assert_eq!(recording.size_bytes as usize, text.len());

            // The daemon's stream, blobs as guacd sends them, whatever the
            // tunnel; the mouse with its time, and no keys. Times never go
            // back: the sync (42, long ago) follows the mouse, whose time is
            // the gateway's until a sync tells the daemon's clock apart.
            let instructions = molcajete_protocol::parse_all(&text).unwrap();
            let (mouse, from_daemon): (Vec<_>, Vec<_>) =
                instructions.into_iter().partition(|i| i.opcode == "mouse");
            let (syncs, from_daemon): (Vec<_>, Vec<_>) =
                from_daemon.into_iter().partition(|i| i.opcode == "sync");
            let from_daemon: String = from_daemon.iter().map(|i| i.encode()).collect();
            assert!(
                from_daemon + "4.sync,2.42;" == text_payload(),
                "{offered} {args}"
            );
            assert_eq!(mouse.len(), 1);
            assert_eq!(mouse[0].args[..3], ["10", "20", "1"]);
            let mouse_at: i64 = mouse[0].args[3].parse().unwrap();
            let sync_at: i64 = syncs[0].args[0].parse().unwrap();
            let mouse_first = text.find("5.mouse") < text.find("4.sync");
            if mouse_first {
                assert!(mouse_at >= started && sync_at == mouse_at);
            } else {
                assert_eq!(sync_at, 42);
                assert!(mouse_at >= 42);
            }

            // Recording changes nothing for guacd but the formats offered.
            assert_eq!(run.guacd.1, INPUT.as_bytes());
            assert_eq!(announced(&run, "image"), ["image/png"]);
            assert!(announced(&run, "video").is_empty());
        }
    }

    #[tokio::test]
    async fn recordings_take_keys_and_molcajete_formats_only_when_asked() {
        let run = run_tunnel_with(
            "",
            BOTH,
            MOLCAJETED_ARGS,
            record_payload(),
            &[
                attributes::RECORD,
                attributes::RECORD_KEYS,
                attributes::RECORD_EXCLUDE_MOUSE,
                attributes::RECORD_MOLCAJETE_FORMATS,
            ],
            &[],
            INPUT,
        )
        .await;
        let (recording, text) = finished_recording(&run.app).await;
        assert!(recording.include_keys);
        let instructions = molcajete_protocol::parse_all(&text).unwrap();
        assert!(instructions.iter().all(|i| i.opcode != "mouse"));
        let keys: Vec<_> = instructions.iter().filter(|i| i.opcode == "key").collect();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].args[..2], ["65307", "1"]);
        assert_eq!(
            announced(&run, "image"),
            ["image/png", "image/x-molcajete-zstd-v1"]
        );
        assert_eq!(announced(&run, "video"), ["video/h264"]);
    }

    #[tokio::test]
    async fn sessions_are_not_recorded_unless_both_sides_agree() {
        // Recording off in the configuration.
        let run = run_tunnel_with(
            "\n[recordings]\nenabled = false",
            BOTH,
            STOCK_ARGS,
            text_payload().into_bytes(),
            &[attributes::RECORD],
            &[],
            INPUT,
        )
        .await;
        assert!(run.app.state.db.list_recordings().await.unwrap().is_empty());
        assert_eq!(announced(&run, "image").len(), 2);
        // Not asked for by the connection.
        let run = run_tunnel("", BOTH, STOCK_ARGS, text_payload().into_bytes()).await;
        assert!(run.app.state.db.list_recordings().await.unwrap().is_empty());
        assert_eq!(announced(&run, "video"), ["video/h264"]);
    }

    #[tokio::test]
    async fn a_session_that_cannot_be_recorded_is_refused() {
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let (guacd_addr, _guacd) = fake_guacd(STOCK_ARGS, text_payload().into_bytes()).await;
        // A directory that cannot be created: its parent is a file.
        let app = app_with(&format!(
            "\n[guacd]\nport = {}\n[recordings]\ndirectory = \"/dev/null/recordings\"\n",
            guacd_addr.port()
        ))
        .await;
        let mut granted = app.state.db.connection("granted").await.unwrap();
        granted
            .attributes
            .insert(attributes::RECORD.into(), "true".into());
        app.state.db.update_connection(&granted).await.unwrap();
        let bob = app.login("bob", "bob-password").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = router(app.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await });

        let mut request = format!("ws://{addr}/websocket-tunnel?connection=granted")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "guacamole".parse().unwrap());
        request.headers_mut().insert("cookie", bob.parse().unwrap());
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let message = socket.next().await.unwrap().unwrap();
        let Message::Close(Some(frame)) = message else {
            panic!("expected close, got {message:?}");
        };
        assert_eq!(frame.reason.as_str(), "512");
        assert!(app.state.db.list_recordings().await.unwrap().is_empty());
    }

    /// Opens connection `granted`, whose own handshake timeout is
    /// `connection_timeout`, against a guacd that never answers, with
    /// `[guacd] handshake_timeout = global_timeout`; returns how long the
    /// gateway waited before closing the tunnel, and the close status.
    async fn time_a_silent_handshake(
        global_timeout: u64,
        connection_timeout: Option<u64>,
    ) -> (std::time::Duration, String) {
        use tokio_tungstenite::tungstenite::Message;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;

        let guacd = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let guacd_port = guacd.local_addr().unwrap().port();
        // Accepts and holds the connection without saying anything.
        tokio::spawn(async move {
            let (_sock, _) = guacd.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let app = app_with(&format!(
            "\n[guacd]\nport = {guacd_port}\nhandshake_timeout = {global_timeout}\n"
        ))
        .await;
        let mut granted = app.state.db.connection("granted").await.unwrap();
        granted.guacd_handshake_timeout = connection_timeout;
        app.state.db.update_connection(&granted).await.unwrap();
        let bob = app.login("bob", "bob-password").await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let service = router(app.state.clone()).into_make_service_with_connect_info::<SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, service).await });

        let mut request = format!("ws://{addr}/websocket-tunnel?connection=granted")
            .into_client_request()
            .unwrap();
        request
            .headers_mut()
            .insert("sec-websocket-protocol", "guacamole".parse().unwrap());
        request.headers_mut().insert("cookie", bob.parse().unwrap());
        let started = std::time::Instant::now();
        let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
        let message = tokio::time::timeout(std::time::Duration::from_secs(10), socket.next())
            .await
            .expect("the handshake times out")
            .unwrap()
            .unwrap();
        let Message::Close(Some(frame)) = message else {
            panic!("expected close, got {message:?}");
        };
        (started.elapsed(), frame.reason.to_string())
    }

    #[tokio::test]
    async fn a_connection_handshake_timeout_overrides_the_global_one() {
        use std::time::Duration;

        let timeout = molcajete_protocol::Status::UPSTREAM_TIMEOUT
            .code()
            .to_string();
        let (waited, status) = time_a_silent_handshake(60, Some(1)).await;
        assert_eq!(status, timeout);
        assert!(
            (Duration::from_secs(1)..Duration::from_secs(5)).contains(&waited),
            "{waited:?}"
        );

        let (waited, status) = time_a_silent_handshake(1, None).await;
        assert_eq!(status, timeout);
        assert!(
            (Duration::from_secs(1)..Duration::from_secs(5)).contains(&waited),
            "{waited:?}"
        );
    }

    #[tokio::test]
    async fn only_administrators_reach_recordings() {
        let app = app().await;
        let directory = app
            .state
            .config
            .recordings
            .directory(&app.state.config.server);
        std::fs::create_dir_all(&directory).unwrap();
        let mut done = db::Recording {
            id: "done".into(),
            connection_id: "granted".into(),
            connection_name: "granted".into(),
            protocol: "vnc".into(),
            user_id: "u".into(),
            username: "bob".into(),
            file_name: "done.guac".into(),
            include_keys: false,
            started_at: 1_000,
            ended_at: Some(2_000),
            size_bytes: 22,
        };
        std::fs::write(directory.join("done.guac"), "4.sync,4.1000,1.0;0123").unwrap();
        app.state.db.create_recording(&done).await.unwrap();
        done.id = "live".into();
        done.file_name = "live.guac".into();
        done.ended_at = None;
        done.started_at = 3_000;
        std::fs::write(directory.join("live.guac"), "4.sync,4.3000;").unwrap();
        app.state.db.create_recording(&done).await.unwrap();

        let bob = app.login("bob", "bob-password").await;
        for path in [
            "/api/recordings",
            "/api/recordings/done",
            "/api/recordings/done/file",
        ] {
            assert_eq!(app.get(path, None).await.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                app.get(path, Some(&bob)).await.status(),
                StatusCode::FORBIDDEN
            );
        }
        let delete = |cookie: &str, id: &str| {
            Request::delete(format!("/api/recordings/{id}"))
                .header(header::COOKIE, cookie)
                .body(Body::empty())
                .unwrap()
        };
        assert_eq!(
            app.send(delete(&bob, "done")).await.status(),
            StatusCode::FORBIDDEN
        );

        let admin = app.login("admin", "admin-password").await;
        let list = body_json(app.get("/api/recordings", Some(&admin)).await).await;
        assert_eq!(list[0]["id"], "live");
        // In progress: the size of the file so far.
        assert_eq!(list[0]["sizeBytes"], 14);
        assert_eq!(list[0]["endedAt"], serde_json::Value::Null);
        assert_eq!(list[1]["connectionId"], "granted");

        let file = app.get("/api/recordings/done/file", Some(&admin)).await;
        assert_eq!(file.status(), StatusCode::OK);
        assert!(file.headers().get(header::CONTENT_DISPOSITION).is_none());
        let bytes = axum::body::to_bytes(file.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"4.sync,4.1000,1.0;0123");
        let download = app
            .get("/api/recordings/done/file?download=true", Some(&admin))
            .await;
        assert_eq!(
            download.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"done.guac\""
        );
        // Ranges, for players that read a part.
        let range = Request::get("/api/recordings/done/file")
            .header(header::COOKIE, &admin)
            .header(header::RANGE, "bytes=18-")
            .body(Body::empty())
            .unwrap();
        let range = app.send(range).await;
        assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
        let bytes = axum::body::to_bytes(range.into_body(), 1 << 20)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"0123");

        assert_eq!(
            app.send(delete(&admin, "live")).await.status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            app.send(delete(&admin, "done")).await.status(),
            StatusCode::NO_CONTENT
        );
        assert!(!directory.join("done.guac").exists());
        assert_eq!(
            app.get("/api/recordings/done", Some(&admin)).await.status(),
            StatusCode::NOT_FOUND
        );
        assert!(directory.join("live.guac").exists());
    }

    /// The audit entries `query` (`action=…&…`) finds, newest first.
    async fn audit(app: &TestApp, cookie: &str, query: &str) -> Vec<serde_json::Value> {
        let response = app.get(&format!("/api/audit?{query}"), Some(cookie)).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        body["entries"].as_array().unwrap().clone()
    }

    /// The one entry with `action` done by `admin`.
    async fn only_entry(app: &TestApp, cookie: &str, action: &str) -> serde_json::Value {
        let entries = audit(app, cookie, &format!("action={action}&actor=admin")).await;
        assert_eq!(entries.len(), 1, "{action}: {entries:#?}");
        entries[0].clone()
    }

    async fn delete(app: &TestApp, path: &str, cookie: &str) -> StatusCode {
        let request = Request::delete(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap();
        app.send(request).await.status()
    }

    const SECRETS: &[&str] = &[
        "carol-password",
        "carol-new-password",
        "rdp-secret-1",
        "rdp-secret-2",
        "ssh-key-material",
        "key-passphrase",
    ];

    #[tokio::test]
    async fn audit_log_records_changes_with_actor_and_diff() {
        let app = app().await;
        let admin = app.login("admin", "admin-password").await;
        let admin_id = only_entry(&app, &admin, "login.success").await["actorId"].clone();

        // Users.
        let carol = app
            .json(
                Method::POST,
                "/api/users",
                Some(&admin),
                serde_json::json!({
                    "username": "carol", "password": "carol-password", "connections": ["granted"]
                }),
            )
            .await;
        assert_eq!(carol.status(), StatusCode::CREATED);
        let carol_id = body_json(carol).await["id"].as_str().unwrap().to_owned();
        let created = only_entry(&app, &admin, "user.create").await;
        assert_eq!(created["actorName"], "admin");
        assert_eq!(created["actorId"], admin_id);
        assert_eq!(
            (
                &created["targetType"],
                &created["targetId"],
                &created["targetName"]
            ),
            (&"user".into(), &carol_id.clone().into(), &"carol".into())
        );
        assert_eq!(created["changes"]["username"]["to"], "carol");
        assert_eq!(
            created["changes"]["connections"],
            serde_json::json!({ "added": ["granted"] })
        );
        assert_eq!(created["changes"]["password"]["to"], "[redacted]");

        let carol_path = format!("/api/users/{carol_id}");
        let update = app
            .json(
                Method::PUT,
                &carol_path,
                Some(&admin),
                serde_json::json!({
                    "role": "connection_manager",
                    "password": "carol-new-password",
                    "connections": ["secret"],
                }),
            )
            .await;
        assert_eq!(update.status(), StatusCode::OK);
        let updated = only_entry(&app, &admin, "user.update").await;
        assert_eq!(updated["actorName"], "admin");
        assert_eq!(
            updated["changes"],
            serde_json::json!({
                "role": { "from": "user", "to": "connection_manager" },
                "connections": { "added": ["secret"], "removed": ["granted"] },
                "password": { "from": "[redacted]", "to": "[redacted]" },
            })
        );
        assert_eq!(
            delete(&app, &carol_path, &admin).await,
            StatusCode::NO_CONTENT
        );
        let deleted = only_entry(&app, &admin, "user.delete").await;
        assert_eq!(deleted["targetName"], "carol");
        assert_eq!(deleted["changes"]["role"]["from"], "connection_manager");

        // Connections, with secrets among the parameters.
        let created = app
            .json(
                Method::POST,
                "/api/connections",
                Some(&admin),
                serde_json::json!({
                    "id": "jump", "name": "Jump host", "protocol": "ssh",
                    "parameters": {
                        "hostname": "10.0.0.9", "password": "rdp-secret-1",
                        "private-key": "ssh-key-material", "passphrase": "key-passphrase"
                    }
                }),
            )
            .await;
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = only_entry(&app, &admin, "connection.create").await;
        assert_eq!(created["targetId"], "jump");
        assert_eq!(created["changes"]["parameters.hostname"]["to"], "10.0.0.9");
        assert_eq!(
            created["changes"]["parameters.password"]["to"],
            "[redacted]"
        );
        assert_eq!(
            created["changes"]["parameters.private-key"]["to"],
            "[redacted]"
        );

        let updated = app
            .json(
                Method::PUT,
                "/api/connections/jump",
                Some(&admin),
                serde_json::json!({
                    "id": "jump", "name": "Jump host", "protocol": "ssh",
                    "guacdHandshakeTimeout": 30,
                    "parameters": {
                        "hostname": "10.0.0.10", "password": "rdp-secret-2",
                        "private-key": "ssh-key-material", "passphrase": "key-passphrase"
                    }
                }),
            )
            .await;
        assert_eq!(updated.status(), StatusCode::OK);
        // Stored credentials are not decrypted to compare them: each one
        // sent again counts as changed.
        let updated = only_entry(&app, &admin, "connection.update").await;
        let replaced = serde_json::json!({ "from": "[redacted]", "to": "[redacted]" });
        assert_eq!(
            updated["changes"],
            serde_json::json!({
                "guacdHandshakeTimeout": { "from": null, "to": 30 },
                "parameters.hostname": { "from": "10.0.0.9", "to": "10.0.0.10" },
                "parameters.password": replaced,
                "parameters.private-key": replaced,
                "parameters.passphrase": replaced,
            })
        );
        let text = updated.to_string();
        assert!(
            !text.contains("rdp-secret") && !text.contains("enc:v1:"),
            "{text}"
        );

        // Saved without credentials: they are kept, and no change shows.
        let renamed = app
            .json(
                Method::PUT,
                "/api/connections/jump",
                Some(&admin),
                serde_json::json!({
                    "id": "jump", "name": "Jump host 2", "protocol": "ssh",
                    "parameters": { "hostname": "10.0.0.10" }
                }),
            )
            .await;
        assert_eq!(renamed.status(), StatusCode::OK);
        let entries = audit(&app, &admin, "action=connection.update").await;
        assert_eq!(
            entries[0]["changes"],
            serde_json::json!({
                "name": { "from": "Jump host", "to": "Jump host 2" },
                "guacdHandshakeTimeout": { "from": 30, "to": null },
            })
        );
        // A cleared credential shows as removed.
        let cleared = app
            .json(
                Method::PUT,
                "/api/connections/jump",
                Some(&admin),
                serde_json::json!({
                    "id": "jump", "name": "Jump host 2", "protocol": "ssh",
                    "parameters": { "hostname": "10.0.0.10" }, "clearSecrets": ["passphrase"]
                }),
            )
            .await;
        assert_eq!(cleared.status(), StatusCode::OK);
        let entries = audit(&app, &admin, "action=connection.update").await;
        assert_eq!(
            entries[0]["changes"],
            serde_json::json!({ "parameters.passphrase": { "from": "[redacted]", "to": null } })
        );

        // Grants.
        let bob_id = app.state.db.user_credentials("bob").await.unwrap().0.id;
        let access = app
            .json(
                Method::PUT,
                "/api/connections/jump/access",
                Some(&admin),
                serde_json::json!({ "users": [bob_id], "groups": [] }),
            )
            .await;
        assert_eq!(access.status(), StatusCode::OK);
        let granted = only_entry(&app, &admin, "connection.access").await;
        assert_eq!(
            granted["changes"],
            serde_json::json!({ "users": { "added": ["bob"] } })
        );
        let revoke = app
            .json(
                Method::PUT,
                "/api/connections/jump/access",
                Some(&admin),
                serde_json::json!({ "users": [], "groups": [] }),
            )
            .await;
        assert_eq!(revoke.status(), StatusCode::OK);
        let entries = audit(&app, &admin, "action=connection.access").await;
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0]["changes"],
            serde_json::json!({ "users": { "removed": ["bob"] } })
        );

        assert_eq!(
            delete(&app, "/api/connections/jump", &admin).await,
            StatusCode::NO_CONTENT
        );
        let deleted = only_entry(&app, &admin, "connection.delete").await;
        assert_eq!(
            deleted["changes"]["parameters.hostname"]["from"],
            "10.0.0.10"
        );

        // Filters.
        assert_eq!(
            audit(&app, &admin, "action=connection&actor=admin")
                .await
                .len(),
            7
        );
        assert_eq!(
            audit(&app, &admin, "target=jump&targetType=connection")
                .await
                .len(),
            7
        );
        assert_eq!(audit(&app, &admin, "actor=bob").await.len(), 0);
        assert_eq!(audit(&app, &admin, "limit=2").await.len(), 2);
        assert_eq!(
            app.get("/api/audit?action=DROP%20TABLE", Some(&admin))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );

        // No secret anywhere: not in the stored entries, not in the CSV.
        let json = app.get("/api/audit?limit=1000", Some(&admin)).await;
        let json = String::from_utf8(
            axum::body::to_bytes(json.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let csv = app.get("/api/audit?format=csv", Some(&admin)).await;
        assert_eq!(
            csv.headers()[header::CONTENT_TYPE],
            "text/csv; charset=utf-8"
        );
        assert!(
            csv.headers()[header::CONTENT_DISPOSITION]
                .to_str()
                .unwrap()
                .starts_with("attachment; filename=\"molcajete-audit-")
        );
        let csv = String::from_utf8(
            axum::body::to_bytes(csv.into_body(), 1 << 20)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(csv.starts_with("time,action,actor,"));
        assert!(csv.contains(",connection.update,admin,"), "{csv}");
        for secret in SECRETS {
            assert!(!json.contains(secret), "{secret} in {json}");
            assert!(!csv.contains(secret), "{secret} in {csv}");
        }
        let rows = app
            .state
            .db
            .list_audit_entries(&Default::default(), 1000, 0)
            .await;
        let rows = format!("{:?}", rows.unwrap());
        for secret in SECRETS {
            assert!(!rows.contains(secret), "{secret} stored");
        }
    }

    #[tokio::test]
    async fn audit_log_records_sign_ins_with_the_source_address() {
        let app = app_with("trusted_proxies = [\"10.0.0.0/8\"]").await;
        let peer: SocketAddr = "10.0.0.2:40000".parse().unwrap();
        let attempt = |username: &str, password: &str, real_ip: Option<&str>| {
            let mut request =
                Request::post("/api/auth/login").header(header::CONTENT_TYPE, "application/json");
            if let Some(ip) = real_ip {
                request = request.header("x-real-ip", ip);
            }
            let mut request = request
                .body(Body::from(
                    serde_json::json!({ "username": username, "password": password }).to_string(),
                ))
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(peer));
            request
        };
        let status = app
            .send(attempt("ghost", "nope", Some("203.0.113.9")))
            .await
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let status = app
            .send(attempt("bob", "wrong-password", None))
            .await
            .status();
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let status = app
            .send(attempt("bob", "bob-password", None))
            .await
            .status();
        assert_eq!(status, StatusCode::OK);

        let admin = app.login("admin", "admin-password").await;
        let failures = audit(&app, &admin, "action=login.failure").await;
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[1]["actorName"], "ghost");
        assert_eq!(failures[1]["actorId"], serde_json::Value::Null);
        assert_eq!(failures[1]["sourceAddress"], "203.0.113.9 (via 10.0.0.2)");
        assert_eq!(failures[1]["changes"]["reason"], "unknown_user");
        assert_eq!(failures[0]["actorName"], "bob");
        assert_eq!(failures[0]["sourceAddress"], "10.0.0.2");
        assert_eq!(failures[0]["changes"]["reason"], "wrong_password");
        let json = serde_json::to_string(&failures).unwrap();
        assert!(!json.contains("wrong-password") && !json.contains("nope"));

        let bob_logins = audit(&app, &admin, "action=login.success&actor=bob").await;
        assert_eq!(bob_logins.len(), 1);
        assert_eq!(bob_logins[0]["sourceAddress"], "10.0.0.2");

        let logout = Request::post("/api/auth/logout")
            .header(header::COOKIE, &admin)
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.send(logout).await.status(), StatusCode::NO_CONTENT);
        let admin = app.login("admin", "admin-password").await;
        assert_eq!(
            only_entry(&app, &admin, "logout").await["actorName"],
            "admin"
        );
    }

    #[tokio::test]
    async fn only_administrators_read_the_audit_log() {
        let app = app().await;
        let bob = app.login("bob", "bob-password").await;
        assert_eq!(
            app.get("/api/audit", None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.get("/api/audit", Some(&bob)).await.status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            app.get("/api/audit/actions", Some(&bob)).await.status(),
            StatusCode::FORBIDDEN
        );
        let admin = app.login("admin", "admin-password").await;
        let actions = body_json(app.get("/api/audit/actions", Some(&admin)).await).await;
        assert!(actions.as_array().unwrap().contains(&"user.update".into()));
    }

    /// A sign-in from `peer` with extra headers.
    async fn sign_in_from(
        app: &TestApp,
        peer: &str,
        headers: &[(&str, &str)],
        username: &str,
        password: &str,
    ) -> Response {
        let mut request =
            Request::post("/api/auth/login").header(header::CONTENT_TYPE, "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let mut request = request
            .body(Body::from(
                serde_json::json!({ "username": username, "password": password }).to_string(),
            ))
            .unwrap();
        let peer: SocketAddr = peer.parse().unwrap();
        request.extensions_mut().insert(ConnectInfo(peer));
        app.send(request).await
    }

    #[tokio::test]
    async fn repeated_failed_sign_ins_are_throttled_per_address() {
        let app = app().await;
        app.state.throttle.freeze();
        let attacker = "192.0.2.10:40000";
        for _ in 0..10 {
            let response = sign_in_from(&app, attacker, &[], "bob", "guess").await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = sign_in_from(&app, attacker, &[], "bob", "guess").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let retry_after: u64 = response.headers()[header::RETRY_AFTER]
            .to_str()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(retry_after, 300);
        let message = body_json(response).await["error"].clone();
        assert_eq!(message, "Too many failed sign-ins. Try again in 5 minutes.");
        // Not even the right password, nor another user, until the window passes.
        for username in ["bob", "admin"] {
            let response = sign_in_from(&app, attacker, &[], username, "bob-password").await;
            assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        }
        // Another address is not affected.
        let response = sign_in_from(&app, "192.0.2.11:40000", &[], "bob", "bob-password").await;
        assert_eq!(response.status(), StatusCode::OK);

        app.state.throttle.advance(Duration::from_secs(300));
        let response = sign_in_from(&app, attacker, &[], "bob", "bob-password").await;
        assert_eq!(response.status(), StatusCode::OK);

        let admin = app.login("admin", "admin-password").await;
        let throttled = audit(&app, &admin, "action=login.throttled").await;
        assert_eq!(throttled.len(), 1, "once per lockout: {throttled:#?}");
        assert_eq!(throttled[0]["actorName"], "bob");
        assert_eq!(throttled[0]["sourceAddress"], "192.0.2.10");
        assert_eq!(throttled[0]["changes"]["reason"], "address");
        let failures = audit(&app, &admin, "action=login.failure").await;
        assert_eq!(failures.len(), 10, "throttled attempts are not failures");
    }

    #[tokio::test]
    async fn a_username_limit_stops_a_distributed_guess() {
        let app = app_with("[auth.throttle]\nusername_failures = 3").await;
        app.state.throttle.freeze();
        // Where admin usually signs in from.
        let office = "198.51.100.1:40000";
        let response = sign_in_from(&app, office, &[], "admin", "admin-password").await;
        assert_eq!(response.status(), StatusCode::OK);
        for n in 1..=3 {
            let peer = format!("203.0.113.{n}:40000");
            let response = sign_in_from(&app, &peer, &[], "admin", "guess").await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = sign_in_from(&app, "203.0.113.4:40000", &[], "admin", "guess").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(response.headers().contains_key(header::RETRY_AFTER));
        let response =
            sign_in_from(&app, "203.0.113.5:40000", &[], "admin", "admin-password").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // Unknown usernames are counted and answered the same way.
        for n in 1..=3 {
            let peer = format!("203.0.113.{n}:40000");
            let response = sign_in_from(&app, &peer, &[], "ghost", "guess").await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = sign_in_from(&app, "203.0.113.4:40000", &[], "ghost", "guess").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // Other users, and admin from an address they signed in from, are not.
        let response = sign_in_from(&app, "203.0.113.6:40000", &[], "bob", "bob-password").await;
        assert_eq!(response.status(), StatusCode::OK);
        let response = sign_in_from(&app, office, &[], "admin", "admin-password").await;
        assert_eq!(response.status(), StatusCode::OK);

        app.state.throttle.advance(Duration::from_secs(900));
        let response =
            sign_in_from(&app, "203.0.113.5:40000", &[], "admin", "admin-password").await;
        assert_eq!(response.status(), StatusCode::OK);
        let admin = app.login("admin", "admin-password").await;
        let throttled = audit(&app, &admin, "action=login.throttled&actor=admin").await;
        assert_eq!(throttled.len(), 1);
        assert_eq!(throttled[0]["changes"]["reason"], "username");
    }

    #[tokio::test]
    async fn only_trusted_proxies_name_the_client() {
        let app = app_with("trusted_proxies = [\"10.0.0.0/8\"]").await;
        app.state.throttle.freeze();
        let proxy = "10.0.0.2:40000";
        let attacker = [("x-real-ip", "203.0.113.1")];
        for _ in 0..10 {
            let response = sign_in_from(&app, proxy, &attacker, "bob", "guess").await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let response = sign_in_from(&app, proxy, &attacker, "bob", "guess").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        // Others behind the same proxy are told apart.
        let neighbour = [("x-forwarded-for", "203.0.113.2, 10.0.0.1")];
        let response = sign_in_from(&app, proxy, &neighbour, "bob", "bob-password").await;
        assert_eq!(response.status(), StatusCode::OK);

        // A client that talks to the gateway directly cannot pick its address.
        for n in 0..10 {
            let spoofed = format!("198.51.100.{n}");
            let headers = [
                ("x-forwarded-for", spoofed.as_str()),
                ("x-real-ip", spoofed.as_str()),
            ];
            let response = sign_in_from(&app, "192.0.2.50:40000", &headers, "admin", "guess").await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
        let headers = [("x-real-ip", "198.51.100.99")];
        let response = sign_in_from(&app, "192.0.2.50:40000", &headers, "admin", "guess").await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);

        let admin = app.login("admin", "admin-password").await;
        let throttled = audit(&app, &admin, "action=login.throttled").await;
        let sources: Vec<_> = throttled
            .iter()
            .map(|e| e["sourceAddress"].clone())
            .collect();
        assert_eq!(sources, ["192.0.2.50", "203.0.113.1 (via 10.0.0.2)"]);
    }
}
