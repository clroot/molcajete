//! Performance reports from the browser client.
//!
//! The page (`web/public/client-report.js`) collects the client's
//! once-per-second `stats` events and posts them here in batches. Each sample is recorded into
//! histograms and each batch is summarised in one log event, which keeps
//! per-session history in the log store.
//!
//! The histograms are labelled by connection, client mode and the kind of
//! client ([`ClientClass`]: browser engine and device), so cardinality
//! stays bounded while a phone and a desktop using one connection can still
//! be told apart. Individual sessions are told apart in the logs: each
//! report names its tunnel (`tunnel_id`) and molcajeted's connection id
//! (`guacd_id`, which molcajeted logs as the `id` of its connection span,
//! `display summary` included), looked up in [`Tunnels`].
//!
//! This is the only view into the browser side of the pipeline: decoding,
//! GPU work and the main thread, which the gateway and molcajeted cannot see.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use metrics::histogram;
use serde::Deserialize;
use tracing::info;
use uuid::Uuid;

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
    /// The page has a touch screen (`navigator.maxTouchPoints > 1`), which
    /// tells an iPad, whose browsers claim to be desktop Safari, from a Mac.
    #[serde(default)]
    touch: bool,
    samples: Vec<Sample>,
}

/// How long a closed tunnel can still be named by a report: the page sends
/// its last samples when it is closed or reconnects, and otherwise every
/// 10 seconds.
const CLOSED_TUNNEL_KEPT: Duration = Duration::from_secs(120);

/// The tunnel each page session (`session=` in the tunnel URL) opened, so
/// that a report can name it.
#[derive(Default)]
pub struct Tunnels(Mutex<HashMap<String, TunnelEntry>>);

struct TunnelEntry {
    connection: String,
    tunnel_id: Uuid,
    guacd_id: String,
    closed: Option<Instant>,
}

impl Tunnels {
    pub fn opened(&self, session: &str, connection: &str, tunnel_id: Uuid, guacd_id: &str) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, t| t.closed.is_none_or(|at| at.elapsed() < CLOSED_TUNNEL_KEPT));
        map.insert(
            session.to_owned(),
            TunnelEntry {
                connection: connection.to_owned(),
                tunnel_id,
                guacd_id: guacd_id.to_owned(),
                closed: None,
            },
        );
    }

    pub fn closed(&self, session: &str, tunnel_id: Uuid) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = map.get_mut(session)
            && t.tunnel_id == tunnel_id
        {
            t.closed = Some(Instant::now());
        }
    }

    /// The tunnel id and molcajeted's connection id of a session's tunnel
    /// to `connection`.
    fn find(&self, session: &str, connection: &str) -> Option<(Uuid, String)> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(session)
            .filter(|t| t.connection == connection)
            .map(|t| (t.tunnel_id, t.guacd_id.clone()))
    }
}

/// The kind of client, from its `User-Agent`: a handful of values, since
/// they become label values.
///
/// The browser engine and the kind of device are what decide how fast the
/// page decodes and draws (the decoders and the WebGL implementation, and
/// the CPU and GPU behind them). The display size would split one session
/// into several series whenever the window is resized, and the full
/// `User-Agent` changes with every browser update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientClass {
    /// `chromium` (Chrome, Edge and other Chromium browsers), `firefox`,
    /// `safari` (every browser on iOS and iPadOS) or `other`.
    pub browser: &'static str,
    /// `desktop`, `tablet` or `mobile`.
    pub device: &'static str,
}

impl ClientClass {
    pub fn of(user_agent: &str, touch: bool) -> Self {
        let ua = user_agent;
        let ios = ["iPhone", "iPad", "iPod"].iter().any(|d| ua.contains(d));
        // iPadOS browsers send the `User-Agent` of desktop Safari.
        let ipad = ua.contains("iPad") || (ua.contains("Macintosh") && touch);
        let browser = if ios || ipad {
            "safari"
        } else if ua.contains("Firefox/") {
            "firefox"
        } else if ua.contains("Chrome/") || ua.contains("Chromium/") {
            "chromium"
        } else if ua.contains("Safari/") {
            "safari"
        } else {
            "other"
        };
        let device = if ipad || (ua.contains("Android") && !ua.contains("Mobile")) {
            "tablet"
        } else if ios || ua.contains("Mobi") {
            "mobile"
        } else {
            "desktop"
        };
        Self { browser, device }
    }
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
    /// Time from a frame's first drawing instruction to its `sync`.
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
    /// Time from applying a frame to having drawn it to the canvas. Absent
    /// from older clients, null for a second in which no frame was drawn.
    #[serde(default)]
    frame_present_ms_avg: Option<f64>,
    #[serde(default)]
    frame_present_ms_max: f64,
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
    headers: HeaderMap,
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
    let class = ClientClass::of(
        headers
            .get(header::USER_AGENT)
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        report.touch,
    );
    let tunnel = state.tunnels.find(&session, &report.connection);
    let labels = [
        ("connection", report.connection.clone()),
        ("mode", mode.to_owned()),
        ("browser", class.browser.to_owned()),
        ("device", class.device.to_owned()),
    ];

    let lag = histogram!("molcajete_client_frame_lag_seconds", &labels);
    let rtt = histogram!("molcajete_client_rtt_seconds", &labels);
    let fps = histogram!("molcajete_client_frames_fps", &labels);
    let queued = histogram!("molcajete_client_queued_ops", &labels);
    let received = histogram!("molcajete_client_receive_rate_bytes", &labels);
    let frame_receive = histogram!("molcajete_client_frame_receive_seconds", &labels);
    let decode = histogram!("molcajete_client_image_decode_seconds", &labels);
    let images = histogram!("molcajete_client_images_ops", &labels);
    let present = histogram!("molcajete_client_frame_present_seconds", &labels);

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
        if let Some(v) = s.frame_present_ms_avg {
            present.record(ms(v));
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
        tunnel_id = tunnel.as_ref().map(|t| t.0.to_string()),
        guacd_id = tunnel.as_ref().map(|t| t.1.as_str()),
        connection = %report.connection,
        mode,
        browser = class.browser,
        device = class.device,
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
        frame_present_ms_avg = (sum.present_n > 0).then(|| sum.present / sum.present_n as f64),
        frame_present_ms_max = sum.present_max,
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
    present: f64,
    present_n: u32,
    present_max: f64,
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
        if let Some(v) = s.frame_present_ms_avg {
            self.present += clean(v);
            self.present_n += 1;
            self.present_max = self.present_max.max(clean(s.frame_present_ms_max));
        }
        if let Some(r) = s.rtt_ms {
            self.rtt += clean(r);
            self.rtt_n += 1;
        }
        self.queued_max = self.queued_max.max(clean(s.queued_ops));
        self.buffered_max = self.buffered_max.max(clean(s.send_buffered));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(json: &str) -> Sample {
        serde_json::from_str(json).expect("sample parses")
    }

    #[test]
    fn client_classes() {
        let class = |ua, touch| {
            let c = ClientClass::of(ua, touch);
            (c.browser, c.device)
        };
        let mac_safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 \
                          (KHTML, like Gecko) Version/18.1 Safari/605.1.15";
        assert_eq!(class(mac_safari, false), ("safari", "desktop"));
        assert_eq!(class(mac_safari, true), ("safari", "tablet"));
        let mac_chrome = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
                          (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        assert_eq!(class(mac_chrome, false), ("chromium", "desktop"));
        let edge = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
                    (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";
        assert_eq!(class(edge, true), ("chromium", "desktop"));
        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:133.0) Gecko/20100101 Firefox/133.0";
        assert_eq!(class(firefox, false), ("firefox", "desktop"));
        let iphone_chrome = "Mozilla/5.0 (iPhone; CPU iPhone OS 18_1 like Mac OS X) \
                             AppleWebKit/605.1.15 (KHTML, like Gecko) CriOS/131.0.6778.73 \
                             Mobile/15E148 Safari/604.1";
        assert_eq!(class(iphone_chrome, true), ("safari", "mobile"));
        let android_phone = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 \
                             (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";
        assert_eq!(class(android_phone, true), ("chromium", "mobile"));
        let android_tablet = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 \
                              (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
        assert_eq!(class(android_tablet, true), ("chromium", "tablet"));
        assert_eq!(class("curl/8.7.1", false), ("other", "desktop"));
        assert_eq!(class("", false), ("other", "desktop"));
    }

    #[test]
    fn reports_name_their_tunnel() {
        let tunnels = Tunnels::default();
        let first = Uuid::new_v4();
        tunnels.opened("page-1", "granted", first, "$abc");
        assert_eq!(
            tunnels.find("page-1", "granted"),
            Some((first, "$abc".to_owned()))
        );
        // Another connection's report does not get it.
        assert_eq!(tunnels.find("page-1", "secret"), None);
        assert_eq!(tunnels.find("page-2", "granted"), None);
        // The last reports arrive after the tunnel closed.
        tunnels.closed("page-1", first);
        assert!(tunnels.find("page-1", "granted").is_some());
        // A reconnect of the same page replaces it; the old tunnel closing
        // later leaves the new one alone.
        let second = Uuid::new_v4();
        tunnels.opened("page-1", "granted", second, "$def");
        tunnels.closed("page-1", first);
        let map = tunnels.0.lock().unwrap();
        assert!(map["page-1"].closed.is_none());
    }

    #[test]
    fn present_time_is_optional() {
        // A client from before the field existed.
        let old = sample(r#"{"framesPerSecond": 30, "frameLagMs": 1.5}"#);
        assert_eq!(old.frame_present_ms_avg, None);
        // A second in which no frame was drawn to the canvas.
        let idle =
            sample(r#"{"framesPerSecond": 0, "framePresentMsAvg": null, "framePresentMsMax": 0}"#);
        assert_eq!(idle.frame_present_ms_avg, None);
        let busy = sample(
            r#"{"framesPerSecond": 30, "framePresentMsAvg": 8.5, "framePresentMsMax": 16.2}"#,
        );
        assert_eq!(busy.frame_present_ms_avg, Some(8.5));

        let mut sum = Summary::default();
        for s in [&old, &idle, &busy] {
            sum.add(s);
        }
        assert_eq!((sum.present, sum.present_n), (8.5, 1));
        assert_eq!(sum.present_max, 16.2);
    }
}
