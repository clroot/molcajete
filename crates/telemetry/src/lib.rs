//! Observability shared by `molcajete-gateway` and `molcajeted`.
//!
//! Everything is opt-in and costs nothing when unused:
//!
//! - **logs** go to stdout, as text (default) or one JSON object per line
//!   (`--log-format json`) for collectors such as Grafana Alloy / Loki;
//! - **metrics** are recorded with the [`metrics`] facade; only when
//!   `--telemetry-listen` is set is a Prometheus recorder installed, and
//!   `GET /metrics` on that address renders them;
//! - **CPU profiles** (`--profiling`, needs the `profiling` feature and
//!   Unix) are served in pprof format at `GET /debug/pprof/profile?seconds=N`,
//!   the endpoint Pyroscope / Alloy `pyroscope.scrape` and `go tool pprof`
//!   expect, and as an SVG at `GET /debug/pprof/flamegraph?seconds=N`.
//!
//! The telemetry listener is separate from the service port so that it is
//! never exposed through the public reverse proxy.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;
use axum::Router;
use axum::http::header;
use axum::routing::get;
use metrics_exporter_prometheus::{Matcher, PrometheusBuilder, PrometheusHandle};
use tracing::info;

/// Buckets for latency histograms (most of them are hot-path timings in the
/// sub-millisecond to second range).
const LATENCY_BUCKETS: &[f64] = &[
    0.000_25, 0.0005, 0.001, 0.002, 0.004, 0.008, 0.016, 0.033, 0.066, 0.1, 0.25, 0.5, 1.0, 2.5,
    5.0, 10.0,
];
/// Buckets for the time between frames (`*_interval_seconds`). Their bounds
/// lie between the intervals of the common frame rates (16.7, 33.3 and
/// 66.7 ms for 60, 30 and 15 frames per second), which the latency buckets
/// have theirs right next to (33 and 66 ms).
const INTERVAL_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.014, 0.02, 0.025, 0.03, 0.04, 0.05, 0.06, 0.075, 0.1, 0.15, 0.25, 0.5, 1.0,
];
/// Buckets for whole-session durations (`*_duration_seconds`).
const DURATION_BUCKETS: &[f64] = &[
    1.0, 10.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 7200.0, 14400.0, 28800.0,
];
/// Buckets for frame rates (`*_fps`).
const FPS_BUCKETS: &[f64] = &[
    1.0, 5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 45.0, 60.0, 90.0, 120.0, 144.0,
];
/// Buckets for queue lengths and other small counts (`*_ops`, `*_frames`,
/// `*_images`).
const COUNT_BUCKETS: &[f64] = &[
    0.0, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 256.0, 1024.0,
];
/// Buckets for sizes (`*_bytes`).
const SIZE_BUCKETS: &[f64] = &[
    256.0,
    1024.0,
    4096.0,
    16384.0,
    65536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum LogFormat {
    /// Human readable, one line per event.
    #[default]
    Text,
    /// One JSON object per line (fields flattened, current span included).
    Json,
}

/// Command line / environment options shared by both binaries.
#[derive(clap::Args, Debug, Clone)]
pub struct TelemetryArgs {
    /// Log output format. The level filter comes from `RUST_LOG`.
    #[arg(long, env = "MOLCAJETE_LOG_FORMAT", value_enum, default_value_t = LogFormat::Text)]
    pub log_format: LogFormat,
    /// Address of the telemetry HTTP listener (`/metrics`, `/healthz` and,
    /// with `--profiling`, `/debug/pprof/*`). Disabled when unset.
    #[arg(long, env = "MOLCAJETE_TELEMETRY_LISTEN")]
    pub telemetry_listen: Option<SocketAddr>,
    /// Serve CPU profiles on the telemetry listener.
    #[arg(long, env = "MOLCAJETE_PROFILING")]
    pub profiling: bool,
}

/// Installs the global tracing subscriber.
///
/// `default_filter` applies when `RUST_LOG` is not set.
pub fn init_logging(format: LogFormat, default_filter: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| default_filter.into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Text => builder.init(),
        LogFormat::Json => builder
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .init(),
    }
}

/// Starts the telemetry listener if one is configured. Must be called from
/// within a tokio runtime, before metrics are recorded (metrics recorded
/// without a recorder are discarded).
///
/// Returns whether metrics are being collected.
pub async fn start(args: &TelemetryArgs, service: &'static str) -> anyhow::Result<bool> {
    let Some(listen) = args.telemetry_listen else {
        if args.profiling {
            tracing::warn!("--profiling has no effect without --telemetry-listen");
        }
        return Ok(false);
    };
    let handle = install_recorder()?;
    metrics::gauge!(
        "molcajete_build_info",
        "service" => service,
        "version" => env!("CARGO_PKG_VERSION"),
    )
    .set(1.0);

    let mut app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route(
            "/metrics",
            get(move || {
                let handle = handle.clone();
                async move {
                    (
                        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
                        handle.render(),
                    )
                }
            }),
        );
    if args.profiling {
        app = profiling::routes(app)?;
    }

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding telemetry listener {listen}"))?;
    info!(%listen, profiling = args.profiling, "telemetry listening");
    tokio::spawn(async move {
        if let Err(err) = axum::serve(listener, app).await {
            tracing::error!(error = %err, "telemetry listener failed");
        }
    });
    Ok(true)
}

/// The exporter with each histogram family's buckets, picked by name suffix;
/// anything else is a latency.
fn builder() -> anyhow::Result<PrometheusBuilder> {
    Ok(PrometheusBuilder::new()
        .set_buckets(LATENCY_BUCKETS)?
        .set_buckets_for_metric(
            Matcher::Suffix("_duration_seconds".into()),
            DURATION_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Suffix("_interval_seconds".into()),
            INTERVAL_BUCKETS,
        )?
        .set_buckets_for_metric(Matcher::Suffix("_bytes".into()), SIZE_BUCKETS)?
        .set_buckets_for_metric(Matcher::Suffix("_fps".into()), FPS_BUCKETS)?
        .set_buckets_for_metric(Matcher::Suffix("_ops".into()), COUNT_BUCKETS)?
        .set_buckets_for_metric(Matcher::Suffix("_frames".into()), COUNT_BUCKETS)?
        .set_buckets_for_metric(Matcher::Suffix("_images".into()), COUNT_BUCKETS)?)
}

fn install_recorder() -> anyhow::Result<PrometheusHandle> {
    let handle = builder()?.install_recorder()?;
    // Histograms are kept in rolling buckets; drain them regularly so memory
    // stays bounded even when nothing scrapes /metrics.
    let upkeep = handle.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            upkeep.run_upkeep();
        }
    });
    Ok(handle)
}

#[cfg(all(feature = "profiling", unix))]
mod profiling {
    use std::sync::Mutex;
    use std::time::Duration;

    use axum::Router;
    use axum::extract::Query;
    use axum::http::{StatusCode, header};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use pprof::protos::Message as _;
    use serde::Deserialize;

    /// pprof-rs supports one active profiler per process.
    static ACTIVE: Mutex<()> = Mutex::new(());

    const MAX_SECONDS: u64 = 300;

    #[derive(Deserialize)]
    pub(super) struct ProfileQuery {
        seconds: Option<u64>,
        /// Sampling frequency in Hz.
        frequency: Option<i32>,
    }

    pub(super) fn routes(app: Router) -> anyhow::Result<Router> {
        Ok(app
            .route("/debug/pprof/profile", get(|q| serve(q, Format::Pprof)))
            .route(
                "/debug/pprof/flamegraph",
                get(|q| serve(q, Format::Flamegraph)),
            ))
    }

    #[derive(Clone, Copy)]
    enum Format {
        Pprof,
        Flamegraph,
    }

    async fn serve(Query(q): Query<ProfileQuery>, format: Format) -> Response {
        let seconds = q.seconds.unwrap_or(10).clamp(1, MAX_SECONDS);
        let frequency = q.frequency.unwrap_or(99).clamp(1, 1000);
        // The profiler guard is not Send; sample on a blocking thread.
        let result = tokio::task::spawn_blocking(move || {
            collect(Duration::from_secs(seconds), frequency, format)
        })
        .await;
        match result {
            Ok(Ok(Some((content_type, body)))) => {
                ([(header::CONTENT_TYPE, content_type)], body).into_response()
            }
            Ok(Ok(None)) => {
                (StatusCode::CONFLICT, "another profile is being collected").into_response()
            }
            Ok(Err(err)) => (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")).into_response(),
            Err(err) => (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()).into_response(),
        }
    }

    fn collect(
        duration: Duration,
        frequency: i32,
        format: Format,
    ) -> anyhow::Result<Option<(&'static str, Vec<u8>)>> {
        let Ok(_active) = ACTIVE.try_lock() else {
            return Ok(None);
        };
        let guard = pprof::ProfilerGuardBuilder::default()
            .frequency(frequency)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()?;
        std::thread::sleep(duration);
        let report = guard.report().build()?;
        drop(guard);
        Ok(Some(match format {
            Format::Pprof => ("application/octet-stream", report.pprof()?.encode_to_vec()),
            Format::Flamegraph => {
                let mut svg = Vec::new();
                report.flamegraph(&mut svg)?;
                ("image/svg+xml", svg)
            }
        }))
    }
}

#[cfg(not(all(feature = "profiling", unix)))]
mod profiling {
    pub(super) fn routes(_app: axum::Router) -> anyhow::Result<axum::Router> {
        if cfg!(unix) {
            anyhow::bail!("--profiling needs a build with the `profiling` feature")
        }
        anyhow::bail!("--profiling is only available on Unix")
    }
}

/// Resolves when the process is asked to stop: Ctrl+C, or SIGTERM (what
/// `docker stop` and Kubernetes send; as PID 1 in a container the default
/// action would be ignored).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders one observation of 3 for each histogram through the exporter.
    fn render(names: &[&'static str]) -> String {
        let recorder = builder().unwrap().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            for name in names {
                metrics::histogram!(*name).record(3.0);
            }
        });
        handle.render()
    }

    #[test]
    fn count_histograms_get_count_buckets() {
        let names = ["molcajeted_frame_images", "molcajeted_unacked_frames"];
        let out = render(&names);
        for name in names {
            assert!(
                out.contains(&format!("{name}_bucket{{le=\"2\"}} 0")),
                "{out}"
            );
            assert!(
                out.contains(&format!("{name}_bucket{{le=\"4\"}} 1")),
                "{out}"
            );
            assert!(
                out.contains(&format!("{name}_bucket{{le=\"1024\"}} 1")),
                "{out}"
            );
            assert!(
                !out.contains(&format!("{name}_bucket{{le=\"0.00025\"}}")),
                "{out}"
            );
        }
    }

    #[test]
    fn frame_intervals_get_buckets_between_the_frame_rates() {
        let name = "molcajeted_frame_interval_seconds";
        let out = render(&[name]);
        for bound in ["0.02", "0.04", "0.075"] {
            assert!(
                out.contains(&format!("{name}_bucket{{le=\"{bound}\"}} 0")),
                "{out}"
            );
        }
        assert!(
            !out.contains(&format!("{name}_bucket{{le=\"0.033\"}}")),
            "{out}"
        );
    }

    #[test]
    fn latency_histograms_keep_latency_buckets() {
        let name = "molcajeted_frame_flush_seconds";
        let out = render(&[name]);
        assert!(
            out.contains(&format!("{name}_bucket{{le=\"0.00025\"}} 0")),
            "{out}"
        );
        assert!(
            out.contains(&format!("{name}_bucket{{le=\"5\"}} 1")),
            "{out}"
        );
        assert!(
            !out.contains(&format!("{name}_bucket{{le=\"1024\"}}")),
            "{out}"
        );
    }
}
