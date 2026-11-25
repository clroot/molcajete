//! molcajete web gateway.
//!
//! Replaces the Java web application of Apache Guacamole: serves the wasm
//! client, exposes the configured connections and tunnels the Guacamole
//! protocol between the browser (WebSocket) and guacd (TCP).

mod client_report;
mod config;
mod guacd;
mod tunnel;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, RawQuery, Request, State, WebSocketUpgrade};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use base64::Engine;
use clap::Parser as _;
use serde::Serialize;
use tower_http::services::ServeDir;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::config::Config;

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
    /// Accept performance reports from the browser client
    /// (`POST /api/telemetry/client`), logged and recorded as metrics.
    #[arg(long, env = "MOLCAJETE_CLIENT_REPORTS")]
    client_reports: bool,
    #[command(flatten)]
    telemetry: molcajete_telemetry::TelemetryArgs,
}

pub struct AppState {
    pub config: Config,
    pub client_reports: bool,
}

/// Lets the page know which optional features are on.
async fn client_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    axum::Json(serde_json::json!({ "clientReports": state.client_reports }))
}

#[derive(Serialize)]
struct ConnectionSummary<'a> {
    id: &'a str,
    name: &'a str,
    protocol: &'a str,
}

async fn list_connections(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    let list: Vec<_> = state
        .config
        .connections
        .iter()
        .map(|c| ConnectionSummary {
            id: &c.id,
            name: c.display_name(),
            protocol: &c.protocol,
        })
        .collect();
    axum::Json(serde_json::to_value(list).unwrap_or_default())
}

async fn websocket_tunnel(
    State(state): State<Arc<AppState>>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let mut request = tunnel::TunnelRequest::from_query(query.as_deref().unwrap_or(""));
    request.peer = Some(client_address(&headers, peer));
    ws.protocols(["guacamole"])
        .max_message_size(1 << 20)
        .on_upgrade(move |socket| tunnel::serve(socket, state, request))
}

/// HTTP Basic authentication against the configured users.
async fn basic_auth(State(state): State<Arc<AppState>>, request: Request, next: Next) -> Response {
    let authorized = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Basic "))
        .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
        .and_then(|raw| String::from_utf8(raw).ok())
        .is_some_and(|creds| {
            let (user, pass) = creds.split_once(':').unwrap_or((&creds, ""));
            state.config.users.iter().any(|u| {
                constant_time_eq(u.username.as_bytes(), user.as_bytes())
                    & constant_time_eq(u.password.as_bytes(), pass.as_bytes())
            })
        });
    if authorized {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                "Basic realm=\"molcajete\", charset=\"UTF-8\"",
            )],
            "authentication required",
        )
            .into_response()
    }
}

/// The browser's address: the first `X-Forwarded-For` hop when the gateway
/// sits behind a reverse proxy, otherwise the TCP peer. Only used for logs.
fn client_address(headers: &HeaderMap, peer: SocketAddr) -> String {
    headers
        .get("x-forwarded-for")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map_or_else(
            || peer.ip().to_string(),
            |v| format!("{v} (via {})", peer.ip()),
        )
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn router(state: Arc<AppState>) -> Router {
    let static_files =
        ServeDir::new(&state.config.server.static_dir).append_index_html_on_directories(true);

    let mut app = Router::new()
        .route("/api/connections", get(list_connections))
        .route("/api/settings", get(client_settings))
        .route(
            "/api/telemetry/client",
            post(client_report::receive).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/websocket-tunnel", get(websocket_tunnel))
        .fallback_service(static_files)
        // The wasm bundle changes on every build; avoid stale caches.
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache"),
        ));

    if !state.config.users.is_empty() {
        app = app.layer(middleware::from_fn_with_state(state.clone(), basic_auth));
    }

    // Liveness for Docker / Kubernetes probes. Added after the
    // authentication layer, which therefore does not apply to it.
    app.route("/healthz", get(|| async { "ok" }))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    molcajete_telemetry::init_logging(cli.telemetry.log_format, "info,tower_http=warn,pprof=warn");
    molcajete_telemetry::start(&cli.telemetry, "gateway").await?;

    let mut config = Config::load(&cli.config)?;
    if let Some(listen) = cli.listen {
        config.server.listen = listen;
    }
    if let Some(dir) = cli.static_dir {
        config.server.static_dir = dir;
    }
    let listen = config.server.listen;

    if config.users.is_empty() && !listen.ip().is_loopback() {
        warn!(%listen, "no [[users]] configured: every visitor can open every connection");
    }
    if !config.server.static_dir.join("index.html").exists() {
        warn!(dir = %config.server.static_dir.display(), "static_dir has no index.html");
    }

    let state = Arc::new(AppState {
        config,
        client_reports: cli.client_reports,
    });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(%listen, connections = state.config.connections.len(), "gateway listening");

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
    use tower::ServiceExt as _;

    use super::*;

    fn app(users: &str) -> Router {
        let dir = std::env::temp_dir().join(format!("molcajete-web-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("index.html"), "<!doctype html>").unwrap();
        let toml = format!(
            "[server]\nstatic_dir = {:?}\n{users}",
            dir.display().to_string()
        );
        let config: Config = ::toml::from_str(&toml).unwrap();
        router(Arc::new(AppState {
            config,
            client_reports: false,
        }))
    }

    async fn status(app: &Router, path: &str) -> StatusCode {
        let request = Request::get(path).body(Body::empty()).unwrap();
        app.clone().oneshot(request).await.unwrap().status()
    }

    #[tokio::test]
    async fn serves_static_files_and_health() {
        let app = app("");
        assert_eq!(status(&app, "/").await, StatusCode::OK);
        assert_eq!(status(&app, "/healthz").await, StatusCode::OK);
        assert_eq!(status(&app, "/api/settings").await, StatusCode::OK);
    }

    #[tokio::test]
    async fn health_skips_authentication() {
        let app = app("[[users]]\nusername = \"a\"\npassword = \"b\"\n");
        assert_eq!(status(&app, "/").await, StatusCode::UNAUTHORIZED);
        assert_eq!(
            status(&app, "/api/connections").await,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(status(&app, "/healthz").await, StatusCode::OK);
    }
}
