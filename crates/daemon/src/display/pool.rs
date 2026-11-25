//! Threads that encode the images of a frame, shared by every session.
//!
//! The pixel phase of a flush cuts what changed into rectangles and encodes
//! each of them (PNG, zstd image or JPEG). Those encodings are independent of
//! each other and take most of a large frame's time, so they run side by
//! side here; the instructions are still written in the original order
//! afterwards (see `Display::emit_rects`).
//!
//! The pool is bounded and shared rather than per session, so that a busy
//! session cannot take every core: by default it has half the cores the
//! process may use, at most 8 (`--encode-threads`).

use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use rayon::{ThreadPool, ThreadPoolBuilder};
use tracing::{info, warn};

use super::encode::ZstdEncoder;

/// Threads of the pool as configured (0: the default).
static THREADS: AtomicUsize = AtomicUsize::new(0);
/// The pool, built on first use, or `None` when frames are encoded on the
/// session's own thread (one thread configured).
static POOL: OnceLock<Option<ThreadPool>> = OnceLock::new();

/// Upper bound of the default number of threads.
const MAX_DEFAULT_THREADS: usize = 8;

/// Sets the number of encoding threads; 1 encodes on the session's own
/// thread, 0 chooses. Takes effect only before the first frame is encoded.
pub fn set_threads(threads: usize) {
    THREADS.store(threads, Ordering::Relaxed);
}

fn default_threads() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get() / 2)
        .clamp(1, MAX_DEFAULT_THREADS)
}

/// The shared pool, if images are encoded on one.
pub(super) fn shared() -> Option<&'static ThreadPool> {
    POOL.get_or_init(|| {
        let threads = match THREADS.load(Ordering::Relaxed) {
            0 => default_threads(),
            n => n,
        };
        if threads <= 1 {
            info!("encoding images on the session threads");
            return None;
        }
        match ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("encode-{i}"))
            .build()
        {
            Ok(pool) => {
                info!(threads, "encoding images on a shared pool");
                Some(pool)
            }
            Err(err) => {
                warn!(error = %err, "no encoding pool, encoding images on the session threads");
                None
            }
        }
    })
    .as_ref()
}

thread_local! {
    /// zstd image encoder of this thread, created on first use and reused
    /// for every image (its context is several megabytes).
    static ZSTD: RefCell<Option<ZstdEncoder>> = const { RefCell::new(None) };
}

/// Runs `f` with this thread's zstd image encoder, or returns `None` if
/// none can be created.
pub(super) fn with_zstd<T>(f: impl FnOnce(&mut ZstdEncoder) -> Option<T>) -> Option<T> {
    ZSTD.with_borrow_mut(|encoder| {
        if encoder.is_none() {
            *encoder = ZstdEncoder::new().ok();
        }
        encoder.as_mut().and_then(f)
    })
}
