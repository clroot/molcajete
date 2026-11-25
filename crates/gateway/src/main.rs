//! molcajete web gateway.
//!
//! Replaces the Java web application of Apache Guacamole: serves the wasm
//! client, exposes the configured connections and tunnels the Guacamole
//! protocol between the browser (WebSocket) and guacd (TCP).

mod config;
mod guacd;
mod tunnel;

use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{RawQuery, Request, State, WebSocketUpgrade};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
    #[arg(short, long, default_value = "gateway.toml")]
    config: PathBuf,
}

pub struct AppState {
    pub config: Config,
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
    RawQuery(query): RawQuery,
    ws: WebSocketUpgrade,
) -> Response {
    let request = tunnel::TunnelRequest::from_query(query.as_deref().unwrap_or(""));
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

    app.layer(TraceLayer::new_for_http()).with_state(state)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let listen = config.server.listen;

    if config.users.is_empty() && !listen.ip().is_loopback() {
        warn!(%listen, "no [[users]] configured: every visitor can open every connection");
    }
    if !config.server.static_dir.join("index.html").exists() {
        warn!(dir = %config.server.static_dir.display(), "static_dir has no index.html");
    }

    let state = Arc::new(AppState { config });
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(%listen, connections = state.config.connections.len(), "gateway listening");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
