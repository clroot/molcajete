//! The connection history: every established session, stored when its
//! tunnel closes, and deleted after `[history] retention_days`.

use std::sync::atomic::Ordering;
use std::time::Duration;

use molcajete_protocol::Status;
use tracing::{error, info, warn};

use super::Registration;
use crate::config::HistoryConfig;
use crate::db::{Database, HistoryEntry, now_millis};

/// Stores the session of `registration`, which closed with `status`.
/// Failing to store it is logged and counted, never returned.
pub async fn record(db: &Database, registration: &Registration, status: Status) {
    let start = registration.start();
    let traffic = registration.traffic();
    let ending = registration.ending();
    let entry = HistoryEntry {
        id: registration.id.to_string(),
        user_id: start.user_id.clone(),
        username: start.username.clone(),
        connection_id: start.connection_id.clone(),
        connection_name: start.connection_name.clone(),
        protocol: start.protocol.clone(),
        peer_address: start.peer.clone(),
        started_at: registration.started_at(),
        ended_at: now_millis(),
        end_status: i64::from(status.code()),
        end_reason: ending.as_ref().map(|e| e.reason.as_str().to_owned()),
        ended_by: ending.and_then(|e| e.by),
        bytes_to_browser: traffic.to_browser.load(Ordering::Relaxed) as i64,
        bytes_to_guacd: traffic.to_guacd.load(Ordering::Relaxed) as i64,
        recording_id: start.recording_id.clone(),
    };
    if let Err(err) = db.insert_history_entry(&entry).await {
        metrics::counter!("molcajete_gateway_history_write_failures_total").increment(1);
        error!(error = %err, session = %entry.id, "cannot store the connection history entry");
    }
}

/// How often expired sessions are deleted.
const RETENTION_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Deletes sessions that ended more than `[history] retention_days` ago,
/// now and every hour.
pub fn start_maintenance(db: Database, config: HistoryConfig) {
    if config.retention_days == 0 {
        return;
    }
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(RETENTION_INTERVAL);
        loop {
            interval.tick().await;
            let cutoff = now_millis().saturating_sub(config.retention_days as i64 * 86_400_000);
            match db.delete_history_before(cutoff).await {
                Ok(0) => {}
                Ok(deleted) => info!(deleted, "deleted expired connection history"),
                Err(err) => warn!(error = %err, "connection history retention failed"),
            }
        }
    });
}
