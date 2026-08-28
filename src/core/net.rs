//! Shared resources for background network work.
//!
//! Startup used to fan out ~20 detached threads at once, each building its own
//! `reqwest` client and tokio runtime. That is what produced the CPU spike at
//! launch: a `reqwest::blocking::Client` owns a private tokio runtime (an extra
//! OS thread) plus a full rustls/aws-lc stack, so a dozen of them meant a dozen
//! TLS initialisations and a dozen throwaway threads competing for the same four
//! cores — all to fetch a handful of thumbnails.
//!
//! This module replaces that with two shared things:
//!   * one HTTP client, so connections to `*.ytimg.com` are pooled and reused;
//!   * a small fixed worker pool, so background fetches queue instead of
//!     stampeding, and run below normal priority so they never contend with
//!     audio decode.

use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

static HTTP: OnceLock<reqwest::blocking::Client> = OnceLock::new();

/// The process-wide blocking HTTP client used for thumbnail/artwork downloads.
///
/// Falls back to a default client if the configured build fails, so callers can
/// treat this as infallible.
pub fn http() -> &'static reqwest::blocking::Client {
    HTTP.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(90))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    })
}

type Job = Box<dyn FnOnce() + Send + 'static>;

static POOL: OnceLock<Sender<Job>> = OnceLock::new();

/// Number of concurrent background fetches. Deliberately small: this work is
/// network-bound with bursts of JPEG decoding, and the point is to leave cores
/// free for the UI and for audio decode. Too low and the home screen fills in
/// visibly slowly, since several of these tasks block on multi-second API calls.
fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() / 2)
        .unwrap_or(2)
        .clamp(2, 4)
}

fn pool() -> &'static Sender<Job> {
    POOL.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));
        for i in 0..worker_count() {
            let rx = Arc::clone(&rx);
            std::thread::Builder::new()
                .name(format!("ytm-bg-{i}"))
                .spawn(move || {
                    // Background enrichment (thumbnails, home rows) must never
                    // steal a timeslice from playback.
                    crate::core::audio_priority::lower_current_thread();
                    loop {
                        // Scope the guard so the lock is released while the job
                        // runs — otherwise the pool would be serialised to one.
                        let job = {
                            let guard = match rx.lock() {
                                Ok(g) => g,
                                Err(_) => break,
                            };
                            guard.recv()
                        };
                        match job {
                            Ok(job) => job(),
                            Err(_) => break, // sender dropped: process shutting down
                        }
                    }
                })
                .ok();
        }
        tx
    })
}

/// Queues `f` on the shared background pool.
///
/// Use this instead of `std::thread::spawn` for non-urgent enrichment work
/// (thumbnails, home-screen rows). Tasks run in submission order across a few
/// workers, so a burst of them costs a bounded amount of CPU instead of one
/// thread per item.
pub fn spawn(f: impl FnOnce() + Send + 'static) {
    let _ = pool().send(Box::new(f));
}
