//! How fast the crawl and pipeline status are asked for.
//!
//! At `POLL` only while someone can see the answer move: the pipeline view is
//! open, a job is running, or this device has just started one. Otherwise
//! the pipeline is asked every `IDLE`, the heartbeat that notices a server
//! going away, and the crawl not at all. Opening the view or starting a job
//! wakes the loops at once, rather than after the next slow tick.

use super::POLL;
use dioxus::prelude::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// The pipeline poll between jobs. Also how long a server can be gone before
/// the notice says so, give or take the request's own timeout.
pub(crate) const IDLE: Duration = Duration::from_secs(5);

/// How long a start from this device keeps the pace up, so the job is seen
/// starting even if the server takes a moment to report it running.
const KICK: Duration = Duration::from_secs(10);

/// Views on screen showing a job's status.
static WATCHERS: AtomicUsize = AtomicUsize::new(0);
static KICKED: Mutex<Option<Instant>> = Mutex::new(None);
static WAKE: Notify = Notify::const_new();

/// Keep the status fast while the calling component is mounted.
pub(crate) fn use_watch() {
    use_hook(|| {
        WATCHERS.fetch_add(1, Ordering::SeqCst);
        wake();
    });
    use_drop(|| {
        WATCHERS.fetch_sub(1, Ordering::SeqCst);
    });
}

/// Something was just started or stopped from here: fast for a while.
pub(crate) fn kick() {
    *KICKED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    wake();
}

/// Ask now, whatever the pace. For a sign the server may have gone away.
pub(crate) fn wake() {
    WAKE.notify_waiters();
}

/// Whether the pipeline view is on screen, for work only it shows.
pub(crate) fn watched() -> bool {
    WATCHERS.load(Ordering::SeqCst) > 0
}

/// Sleep until the next ask: `POLL` if fast, else `idle`, forever if that is
/// `None`, or until woken.
pub(crate) async fn wait(busy: bool, idle: Option<Duration>) {
    // Registered before the state is read, so a wake in between still counts.
    let woken = WAKE.notified();
    tokio::pin!(woken);
    woken.as_mut().enable();

    let kicked = *KICKED.lock().unwrap_or_else(|e| e.into_inner());
    match interval(watched(), busy, kicked, Instant::now(), idle) {
        Some(pause) => {
            tokio::select! {
                _ = tokio::time::sleep(pause) => {}
                _ = woken => {}
            }
        }
        None => woken.await,
    }
}

/// Whether the view is open, the job running, or a kick still fresh decides
/// between `POLL` and `idle`.
fn interval(
    watched: bool,
    busy: bool,
    kicked: Option<Instant>,
    now: Instant,
    idle: Option<Duration>,
) -> Option<Duration> {
    let kicked = kicked.is_some_and(|at| now.saturating_duration_since(at) < KICK);
    if watched || busy || kicked {
        Some(POLL)
    } else {
        idle
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_when_nothing_shows_or_runs() {
        let now = Instant::now();
        assert_eq!(interval(false, false, None, now, Some(IDLE)), Some(IDLE));
        assert_eq!(interval(false, false, None, now, None), None);
    }

    #[test]
    fn fast_while_watched_or_busy() {
        let now = Instant::now();
        assert_eq!(interval(true, false, None, now, Some(IDLE)), Some(POLL));
        assert_eq!(interval(false, true, None, now, None), Some(POLL));
    }

    #[test]
    fn a_kick_wears_off() {
        let at = Instant::now();
        assert_eq!(interval(false, false, Some(at), at + KICK / 2, Some(IDLE)), Some(POLL));
        assert_eq!(interval(false, false, Some(at), at + KICK, Some(IDLE)), Some(IDLE));
    }
}
