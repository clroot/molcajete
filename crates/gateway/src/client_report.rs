//! Performance reports from the browser client.
//!
//! The page (`web/app.js`) collects the client's once-per-second `stats`
//! events and posts them here in batches. Each sample is recorded into
//! histograms (labelled by connection and client mode only, so cardinality
//! stays bounded) and each batch is summarised in one log event, which keeps
//! per-session history in the log store.
//!
//! This is the only view into the browser side of the pipeline: decoding,
//! GPU work and the main thread, which the gateway and molcajeted cannot see.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use metrics::histogram;
use serde::Deserialize;
use tracing::info;

use crate::AppState;

/// More than a minute of 1 Hz samples per batch is not expected.
const MAX_SAMPLES: usize = 120;

#[derive(Debug, Deserialize)]
pub struct ClientReport {
    /// Id the page generated for this session (also sent as `session=` in
    /// the tunnel URL).
    session: String,
    connection: String,
    /// `worker` or `main`.
    #[serde(default)]
    mode: String,
    samples: Vec<Sample>,
}

/// One `stats` event of the client (field names as emitted by the wasm client).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Sample {
    frames_per_second: f64,
    #[serde(default)]
    composites_per_second: f64,
    #[serde(default)]
    draw_calls_per_second: f64,
    #[serde(default)]
    instructions_per_second: f64,
    #[serde(default)]
    bytes_per_second: f64,
    #[serde(default)]
    queued_ops: f64,
    /// Time from receiving a frame's `sync` to applying it (last frame of
    /// the second, then average and maximum over the second).
    #[serde(default)]
    frame_lag_ms: f64,
    #[serde(default)]
    frame_lag_ms_avg: Option<f64>,
    #[serde(default)]
    frame_lag_ms_max: f64,
    /// Time from a frame's first instruction to its `sync`.
    #[serde(default)]
    frame_receive_ms_avg: Option<f64>,
    #[serde(default)]
    frame_receive_ms_max: f64,
    #[serde(default)]
    images_per_second: f64,
    /// Time to decode one image.
    #[serde(default)]
    image_decode_ms_avg: Option<f64>,
    #[serde(default)]
    image_decode_ms_max: f64,
    #[serde(default)]
    rtt_ms: Option<f64>,
    #[serde(default)]
    send_buffered: f64,
    #[serde(default)]
    display_width: f64,
    #[serde(default)]
    display_height: f64,
}

fn clean(v: f64) -> f64 {
    if v.is_finite() { v.max(0.0) } else { 0.0 }
}

fn ms(v: f64) -> Duration {
    Duration::from_secs_f64(clean(v).min(3_600_000.0) / 1000.0)
}

pub async fn receive(
    State(state): State<Arc<AppState>>,
    Json(report): Json<ClientReport>,
) -> StatusCode {
    if !state.client_reports {
        return StatusCode::NOT_FOUND;
    }
    // Only existing connections become label values.
    if !state
        .db
        .connection_exists(&report.connection)
        .await
        .unwrap_or(false)
    {
        return StatusCode::BAD_REQUEST;
    }
    if report.samples.is_empty() || report.samples.len() > MAX_SAMPLES {
        return StatusCode::BAD_REQUEST;
    }
    let mode = match report.mode.as_str() {
        "worker" | "main" => report.mode.as_str(),
        _ => "unknown",
    };
    let session: String = report
        .session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect();
    let labels = [
        ("connection", report.connection.clone()),
        ("mode", mode.to_owned()),
    ];

    let lag = histogram!("molcajete_client_frame_lag_seconds", &labels);
    let rtt = histogram!("molcajete_client_rtt_seconds", &labels);
    let fps = histogram!("molcajete_client_frames_fps", &labels);
    let queued = histogram!("molcajete_client_queued_ops", &labels);
    let received = histogram!("molcajete_client_receive_rate_bytes", &labels);
    let frame_receive = histogram!("molcajete_client_frame_receive_seconds", &labels);
    let decode = histogram!("molcajete_client_image_decode_seconds", &labels);
    let images = histogram!("molcajete_client_images_ops", &labels);

    let n = report.samples.len() as f64;
    let mut sum = Summary::default();
    for s in &report.samples {
        // Older clients only report the last frame of each second.
        lag.record(ms(s.frame_lag_ms_avg.unwrap_or(s.frame_lag_ms)));
        // Seconds without frames or images say nothing about their cost.
        if let Some(v) = s.frame_receive_ms_avg.filter(|_| s.frames_per_second > 0.0) {
            frame_receive.record(ms(v));
        }
        if let Some(v) = s.image_decode_ms_avg.filter(|_| s.images_per_second > 0.0) {
            decode.record(ms(v));
            images.record(clean(s.images_per_second));
        }
        if let Some(r) = s.rtt_ms {
            rtt.record(ms(r));
        }
        fps.record(clean(s.frames_per_second));
        queued.record(clean(s.queued_ops));
        received.record(clean(s.bytes_per_second));
        sum.add(s);
    }
    let last = report.samples.last().expect("checked non-empty");
    info!(
        target: "molcajete::client_report",
        session = %session,
        connection = %report.connection,
        mode,
        samples = report.samples.len(),
        fps_avg = sum.fps / n,
        fps_min = sum.fps_min,
        composites_per_s = sum.composites / n,
        draw_calls_per_s = sum.draws / n,
        instructions_per_s = sum.instructions / n,
        recv_bytes_per_s = sum.bytes / n,
        frame_lag_ms_avg = sum.lag / n,
        frame_lag_ms_max = sum.lag_max,
        frame_receive_ms_max = sum.receive_max,
        images_per_s = sum.images / n,
        image_decode_ms_max = sum.decode_max,
        rtt_ms_avg = (sum.rtt_n > 0).then(|| sum.rtt / sum.rtt_n as f64),
        queued_ops_max = sum.queued_max,
        send_buffered_max = sum.buffered_max,
        display_width = clean(last.display_width) as u32,
        display_height = clean(last.display_height) as u32,
        "client report"
    );
    StatusCode::NO_CONTENT
}

#[derive(Default)]
struct Summary {
    fps: f64,
    fps_min: Option<f64>,
    composites: f64,
    draws: f64,
    instructions: f64,
    bytes: f64,
    lag: f64,
    lag_max: f64,
    receive_max: f64,
    images: f64,
    decode_max: f64,
    rtt: f64,
    rtt_n: u32,
    queued_max: f64,
    buffered_max: f64,
}

impl Summary {
    fn add(&mut self, s: &Sample) {
        let fps = clean(s.frames_per_second);
        self.fps_min = Some(self.fps_min.map_or(fps, |m| m.min(fps)));
        self.fps += fps;
        self.composites += clean(s.composites_per_second);
        self.draws += clean(s.draw_calls_per_second);
        self.instructions += clean(s.instructions_per_second);
        self.bytes += clean(s.bytes_per_second);
        self.lag += clean(s.frame_lag_ms_avg.unwrap_or(s.frame_lag_ms));
        self.lag_max = self
            .lag_max
            .max(clean(s.frame_lag_ms))
            .max(clean(s.frame_lag_ms_max));
        self.receive_max = self.receive_max.max(clean(s.frame_receive_ms_max));
        self.images += clean(s.images_per_second);
        self.decode_max = self.decode_max.max(clean(s.image_decode_ms_max));
        if let Some(r) = s.rtt_ms {
            self.rtt += clean(r);
            self.rtt_n += 1;
        }
        self.queued_max = self.queued_max.max(clean(s.queued_ops));
        self.buffered_max = self.buffered_max.max(clean(s.send_buffered));
    }
}
