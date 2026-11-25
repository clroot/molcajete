//! Throttling of password sign-ins (`[auth.throttle]`).
//!
//! Failed sign-ins are counted in sliding windows, in memory (so per
//! gateway replica, and forgotten on restart):
//!
//! - **per client address** (an IPv6 address counts as its /64): more than
//!   `address_failures` within `address_window_secs` and that address gets
//!   `429` for any username;
//! - **per username**, from any address: more than `username_failures`
//!   within `username_window_secs` and that username gets `429`, which
//!   stops a guess spread over many addresses. So that an attacker cannot
//!   lock a user out everywhere, addresses the user signed in from within
//!   `known_address_days` are exempt from this limit (not from the
//!   address limit).
//!
//! A refused attempt is answered before the database or Argon2 is touched,
//! and does not count as a failure: once the window has passed the guesses
//! start again, as do the user's. A successful sign-in forgets that
//! address's failures for that username only, so signing in to one's own
//! account does not clear guesses at another.
//!
//! Argon2 verifications run at most `max_concurrent_verifications` at a
//! time, so a burst of sign-ins cannot take every CPU from the tunnels.
//!
//! Memory: each failure is kept until its window ends. The address limit
//! and the cost of Argon2 bound how fast failures can be added.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::sync::Semaphore;
use tracing::warn;

use crate::AppState;
use crate::audit::{self, Entry, action};
use crate::auth::{self, ClientAddress};
use crate::config::ThrottleConfig;

const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// `Retry-After` when every verification slot stayed busy.
const BUSY_RETRY_AFTER: Duration = Duration::from_secs(5);

/// Why a sign-in was not attempted; the `reason` of
/// `molcajete_gateway_logins_throttled_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Too many failures from the client's address.
    Address,
    /// Too many failures for the username.
    Username,
    /// Every verification slot stayed busy.
    Busy,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Address => "address",
            Self::Username => "username",
            Self::Busy => "busy",
        }
    }
}

/// A refused sign-in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Throttled {
    pub reason: Reason,
    pub retry_after: Duration,
    /// The first refusal of this lockout (audited once).
    pub started: bool,
}

impl Throttled {
    fn retry_after_secs(&self) -> u64 {
        self.retry_after.as_secs_f64().ceil().max(1.0) as u64
    }
}

impl IntoResponse for Throttled {
    fn into_response(self) -> Response {
        let secs = self.retry_after_secs();
        let (status, message) = match self.reason {
            Reason::Busy => (
                StatusCode::SERVICE_UNAVAILABLE,
                "The server is busy. Try again in a moment.".to_owned(),
            ),
            // The same answer for both, so that it does not tell whether
            // the username exists or is under attack.
            Reason::Address | Reason::Username => (
                StatusCode::TOO_MANY_REQUESTS,
                format!(
                    "Too many failed sign-ins. Try again in {}.",
                    human_duration(secs)
                ),
            ),
        };
        let mut response = (status, Json(serde_json::json!({ "error": message }))).into_response();
        response
            .headers_mut()
            .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        response
    }
}

fn human_duration(secs: u64) -> String {
    match secs {
        0..=1 => "a second".to_owned(),
        2..=59 => format!("{secs} seconds"),
        60..=119 => "a minute".to_owned(),
        _ => format!("{} minutes", secs.div_ceil(60)),
    }
}

/// The failed sign-in counters and the verification slots; one per
/// gateway, in [`AppState`].
pub struct LoginThrottle {
    config: ThrottleConfig,
    counters: Mutex<Counters>,
    verifications: Semaphore,
    /// The time, when a test has stopped the clock.
    frozen: Mutex<Option<Instant>>,
}

#[derive(Default)]
struct Counters {
    /// Failures from an address, oldest first, with the username tried.
    by_address: HashMap<IpAddr, VecDeque<(Instant, String)>>,
    /// Failures for a username, oldest first, with the address.
    by_username: HashMap<String, VecDeque<(Instant, Option<IpAddr>)>>,
    /// When a user last signed in from an address.
    known: HashMap<(String, IpAddr), Instant>,
    /// Until when an address or username is refused, to tell when a
    /// lockout starts.
    lockouts: HashMap<Lock, Instant>,
    last_sweep: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Lock {
    Address(IpAddr),
    Username(String),
}

/// Usernames are compared trimmed and case-insensitively, so that `Admin`
/// and `admin ` share one budget whatever the database's collation.
fn username_key(username: &str) -> String {
    username
        .trim()
        .chars()
        .take(255)
        .collect::<String>()
        .to_lowercase()
}

/// An IPv6 client usually holds a whole /64.
fn address_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let bits = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(bits))
        }
        v4 => v4,
    }
}

/// Drops failures older than `window`.
fn prune<T>(failures: &mut VecDeque<(Instant, T)>, now: Instant, window: Duration) {
    while failures
        .front()
        .is_some_and(|(at, _)| now.saturating_duration_since(*at) >= window)
    {
        failures.pop_front();
    }
}

/// How long until fewer than `limit` failures are within `window`, if
/// `limit` are now.
fn over_limit<T>(
    failures: &VecDeque<(Instant, T)>,
    limit: u32,
    now: Instant,
    window: Duration,
) -> Option<Duration> {
    let limit = limit as usize;
    if limit == 0 || failures.len() < limit {
        return None;
    }
    let (at, _) = &failures[failures.len() - limit];
    Some((*at + window).saturating_duration_since(now))
}

impl LoginThrottle {
    pub fn new(config: &ThrottleConfig) -> Self {
        let slots = match config.max_concurrent_verifications {
            0 => std::thread::available_parallelism().map_or(4, |n| n.get()),
            n => n,
        };
        Self {
            config: config.clone(),
            counters: Mutex::default(),
            verifications: Semaphore::new(slots),
            frozen: Mutex::default(),
        }
    }

    fn now(&self) -> Instant {
        self.frozen.lock().unwrap().unwrap_or_else(Instant::now)
    }

    /// Stops this throttle's clock (it only moves with [`Self::advance`]).
    #[cfg(test)]
    pub fn freeze(&self) {
        let now = self.now();
        *self.frozen.lock().unwrap() = Some(now);
    }

    /// Stops this throttle's clock `by` from now.
    #[cfg(test)]
    pub fn advance(&self, by: Duration) {
        let later = self.now() + by;
        *self.frozen.lock().unwrap() = Some(later);
    }

    fn address_window(&self) -> Duration {
        Duration::from_secs(self.config.address_window_secs)
    }

    fn username_window(&self) -> Duration {
        Duration::from_secs(self.config.username_window_secs)
    }

    fn known_for(&self) -> Duration {
        Duration::from_secs(self.config.known_address_days.saturating_mul(86_400))
    }

    /// Whether `username` may try to sign in from `ip` now.
    pub fn check(&self, username: &str, ip: Option<IpAddr>) -> Result<(), Throttled> {
        let now = self.now();
        let mut counters = self.counters.lock().unwrap();
        self.sweep(&mut counters, now);
        let ip = ip.map(address_key);
        if let Some(ip) = ip
            && let Some(failures) = counters.by_address.get_mut(&ip)
        {
            let window = self.address_window();
            prune(failures, now, window);
            if let Some(wait) = over_limit(failures, self.config.address_failures, now, window) {
                return Err(counters.lockout(Lock::Address(ip), Reason::Address, wait, now));
            }
        }
        let name = username_key(username);
        let known = ip.is_some_and(|ip| {
            counters
                .known
                .get(&(name.clone(), ip))
                .is_some_and(|at| now.saturating_duration_since(*at) < self.known_for())
        });
        if !known && let Some(failures) = counters.by_username.get_mut(&name) {
            let window = self.username_window();
            prune(failures, now, window);
            if let Some(wait) = over_limit(failures, self.config.username_failures, now, window) {
                return Err(counters.lockout(Lock::Username(name), Reason::Username, wait, now));
            }
        }
        Ok(())
    }

    /// Counts a refused sign-in.
    pub fn failed(&self, username: &str, ip: Option<IpAddr>) {
        let now = self.now();
        let name = username_key(username);
        let ip = ip.map(address_key);
        let mut counters = self.counters.lock().unwrap();
        if let Some(ip) = ip {
            counters
                .by_address
                .entry(ip)
                .or_default()
                .push_back((now, name.clone()));
        }
        counters
            .by_username
            .entry(name)
            .or_default()
            .push_back((now, ip));
    }

    /// Forgets the address's failures for this username (only), and
    /// remembers the address as one the user signs in from.
    pub fn succeeded(&self, username: &str, ip: Option<IpAddr>) {
        let Some(ip) = ip.map(address_key) else {
            return;
        };
        let now = self.now();
        let name = username_key(username);
        let mut counters = self.counters.lock().unwrap();
        if let Some(failures) = counters.by_address.get_mut(&ip) {
            failures.retain(|(_, tried)| *tried != name);
        }
        if let Some(failures) = counters.by_username.get_mut(&name) {
            failures.retain(|(_, from)| *from != Some(ip));
        }
        counters.known.insert((name, ip), now);
    }

    /// Verifies a password in one of the verification slots; [`Reason::Busy`]
    /// when none frees up within `verification_wait_secs`.
    pub async fn verify(&self, password: String, hash: String) -> Result<bool, Throttled> {
        let wait = Duration::from_secs(self.config.verification_wait_secs);
        match tokio::time::timeout(wait, self.verifications.acquire()).await {
            Ok(Ok(_slot)) => Ok(auth::verify_password_blocking(password, hash).await),
            _ => Err(Throttled {
                reason: Reason::Busy,
                retry_after: BUSY_RETRY_AFTER,
                started: false,
            }),
        }
    }

    /// Drops what has expired, once a minute.
    fn sweep(&self, counters: &mut Counters, now: Instant) {
        if counters
            .last_sweep
            .is_some_and(|at| now.saturating_duration_since(at) < SWEEP_INTERVAL)
        {
            return;
        }
        counters.last_sweep = Some(now);
        let (address_window, username_window) = (self.address_window(), self.username_window());
        counters.by_address.retain(|_, failures| {
            prune(failures, now, address_window);
            !failures.is_empty()
        });
        counters.by_username.retain(|_, failures| {
            prune(failures, now, username_window);
            !failures.is_empty()
        });
        let known_for = self.known_for();
        counters
            .known
            .retain(|_, at| now.saturating_duration_since(*at) < known_for);
        counters.lockouts.retain(|_, until| *until > now);
    }
}

impl Counters {
    fn lockout(&mut self, lock: Lock, reason: Reason, wait: Duration, now: Instant) -> Throttled {
        let until = now + wait;
        let started = self
            .lockouts
            .insert(lock, until)
            .is_none_or(|previous| previous <= now);
        Throttled {
            reason,
            retry_after: wait,
            started,
        }
    }
}

/// The response to a throttled sign-in for `username`: counted in
/// `molcajete_gateway_logins_throttled_total`, and logged and audited
/// (`login.throttled`) when a lockout starts.
pub async fn refuse(
    state: &AppState,
    username: &str,
    address: &ClientAddress,
    throttled: Throttled,
) -> Response {
    let reason = throttled.reason.as_str();
    metrics::counter!("molcajete_gateway_logins_throttled_total", "reason" => reason).increment(1);
    if throttled.started {
        let retry_after = throttled.retry_after_secs();
        warn!(
            reason,
            username,
            source = %address.0,
            retry_after,
            "too many failed sign-ins; refusing more for a while"
        );
        let entry = Entry::new(action::LOGIN_THROTTLED)
            .by_name(username.trim())
            .from(address)
            .detail("reason", reason)
            .detail("retryAfterSecs", retry_after);
        audit::record(&state.db, entry).await;
    }
    throttled.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn throttle(address_failures: u32, username_failures: u32) -> LoginThrottle {
        let throttle = LoginThrottle::new(&ThrottleConfig {
            address_failures,
            username_failures,
            ..Default::default()
        });
        throttle.freeze();
        throttle
    }

    fn ip(text: &str) -> Option<IpAddr> {
        Some(text.parse().unwrap())
    }

    #[test]
    fn sliding_window_per_address() {
        let t = throttle(3, 0);
        let a = ip("192.0.2.1");
        for _ in 0..3 {
            t.check("bob", a).unwrap();
            t.failed("bob", a);
            t.advance(Duration::from_secs(60));
        }
        let refused = t.check("alice", a).unwrap_err();
        assert_eq!(refused.reason, Reason::Address);
        assert!(refused.started);
        // The first failure leaves the window 300 s after it.
        assert_eq!(refused.retry_after, Duration::from_secs(120));
        assert!(!t.check("bob", a).unwrap_err().started);
        t.check("bob", ip("192.0.2.2")).unwrap();
        t.advance(Duration::from_secs(120));
        t.check("bob", a).unwrap();
        t.failed("bob", a);
        let again = t.check("bob", a).unwrap_err();
        assert!(again.started, "a new lockout");
        assert_eq!(again.retry_after, Duration::from_secs(60));
    }

    #[test]
    fn ipv6_counts_per_64() {
        let t = throttle(1, 0);
        t.failed("bob", ip("2001:db8:1:2::1"));
        assert!(t.check("bob", ip("2001:db8:1:2:ffff::9")).is_err());
        t.check("bob", ip("2001:db8:1:3::1")).unwrap();
    }

    #[test]
    fn success_forgets_only_that_username() {
        let t = throttle(3, 0);
        let a = ip("192.0.2.1");
        t.failed("alice", a);
        t.failed("alice", a);
        t.failed("Bob", a);
        t.succeeded("bob ", a);
        t.check("x", a).unwrap();
        t.failed("alice", a);
        assert!(t.check("bob", a).is_err());
    }

    #[test]
    fn username_limit_spares_known_addresses() {
        let t = throttle(0, 3);
        let home = ip("198.51.100.1");
        t.check("bob", home).unwrap();
        t.succeeded("bob", home);
        for n in 0..3 {
            t.failed("bob", ip(&format!("203.0.113.{n}")));
        }
        let refused = t.check("BOB", ip("203.0.113.200")).unwrap_err();
        assert_eq!(refused.reason, Reason::Username);
        assert_eq!(refused.retry_after, Duration::from_secs(900));
        t.check("bob", home).unwrap();
        t.check("alice", ip("203.0.113.200")).unwrap();
        t.advance(Duration::from_secs(31 * 86_400));
        assert!(t.check("bob", home).is_ok(), "the failures expired too");
        for n in 0..3 {
            t.failed("bob", ip(&format!("203.0.113.{n}")));
        }
        assert!(t.check("bob", home).is_err(), "no longer known");
    }

    #[test]
    fn sweeps_expired_entries() {
        let t = throttle(5, 5);
        t.failed("bob", ip("192.0.2.1"));
        t.check("bob", ip("192.0.2.9")).unwrap();
        t.advance(Duration::from_secs(3600));
        t.check("x", None).unwrap();
        let counters = t.counters.lock().unwrap();
        assert!(counters.by_address.is_empty() && counters.by_username.is_empty());
    }

    #[tokio::test]
    async fn busy_verifications_wait_then_give_up() {
        let t = LoginThrottle::new(&ThrottleConfig {
            max_concurrent_verifications: 1,
            verification_wait_secs: 0,
            ..Default::default()
        });
        let slot = t.verifications.acquire().await.unwrap();
        let refused = t
            .verify("x".into(), auth::dummy_hash().into())
            .await
            .unwrap_err();
        assert_eq!(refused.reason, Reason::Busy);
        drop(slot);
        assert_eq!(
            t.verify("x".into(), auth::dummy_hash().into()).await,
            Ok(false)
        );
    }

    #[test]
    fn messages() {
        assert_eq!(human_duration(1), "a second");
        assert_eq!(human_duration(42), "42 seconds");
        assert_eq!(human_duration(61), "a minute");
        assert_eq!(human_duration(299), "5 minutes");
    }
}
