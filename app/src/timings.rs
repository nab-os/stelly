//! How long things take, for recording a baseline and checking a change
//! against it.
//!
//! Off unless `STELLY_TIMINGS=1` is set. An Android app is started with no
//! environment of its own, so there the system property does it too:
//! `adb shell setprop debug.stelly.timings 1`, then start the app again. Off,
//! every call below is a `OnceLock` read and a branch.
//!
//! One line per event, on stderr, which Android passes on to logcat:
//!
//! ```text
//! timing <name> <ms since launch> <ms the span took, or -> [detail]
//! ```
//!
//! Launch is the process starting, as the kernel tells it, so the first line
//! already says how long it took to reach `main`. Spans are started under a
//! key with `start`, and measured by any event that names the key, `span`
//! leaving it running and `end` closing it.

use std::fmt::Display;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// When the process started, if timings are on.
static LAUNCH: OnceLock<Option<Instant>> = OnceLock::new();

fn launch() -> Option<Instant> {
    *LAUNCH.get_or_init(|| {
        if !switched_on() {
            return None;
        }
        let now = Instant::now();
        Some(since_process_start().and_then(|ago| now.checked_sub(ago)).unwrap_or(now))
    })
}

pub fn enabled() -> bool {
    launch().is_some()
}

fn switched_on() -> bool {
    if std::env::var("STELLY_TIMINGS").is_ok_and(|value| value == "1") {
        return true;
    }
    #[cfg(target_os = "android")]
    {
        let mut value = [0u8; 92];
        // SAFETY: PROP_VALUE_MAX is 92, the most the call ever writes.
        let len = unsafe {
            libc::__system_property_get(c"debug.stelly.timings".as_ptr(), value.as_mut_ptr().cast())
        };
        if len == 1 && value[0] == b'1' {
            return true;
        }
    }
    false
}

/// How long ago the kernel started this process. Field 22 of
/// `/proc/self/stat` is when, in ticks since boot, and a tick is 10ms on
/// every Linux and Android this runs on.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn since_process_start() -> Option<Duration> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The command name, field 2, is in parentheses and may hold spaces, so
    // fields are counted from after it, starting at 3.
    let rest = stat.get(stat.rfind(')')? + 2..)?;
    let ticks: u64 = rest.split(' ').nth(22 - 3)?.parse().ok()?;
    let uptime: f64 = std::fs::read_to_string("/proc/uptime")
        .ok()?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    Some(Duration::from_secs_f64((uptime - ticks as f64 / 100.0).max(0.0)))
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn since_process_start() -> Option<Duration> {
    None
}

/// Now, if timings are on. For timing something that is not a span, an HTTP
/// call say, without paying for the clock when they are off.
pub fn clock() -> Option<Instant> {
    launch().map(|_| Instant::now())
}

fn print(name: &str, span: Option<Duration>, detail: &dyn Display) {
    let Some(launch) = launch() else { return };
    let at = launch.elapsed().as_millis();
    let line = match span {
        Some(span) => format!("timing {name} {at} {:.1} {detail}", span.as_secs_f64() * 1000.0),
        None => format!("timing {name} {at} - {detail}"),
    };
    eprintln!("{}", line.trim_end());
}

/// Something happened.
pub fn mark(name: &str, detail: impl Display) {
    if enabled() {
        print(name, None, &detail);
    }
}

/// Something happened that `started` began.
pub fn took(name: &str, started: Option<Instant>, detail: impl Display) {
    if let Some(started) = started {
        print(name, Some(started.elapsed()), &detail);
    }
}

/// Spans by key, latest start wins. A handful at most, so a list.
static SPANS: Mutex<Vec<(&'static str, Instant)>> = Mutex::new(Vec::new());

fn spans() -> std::sync::MutexGuard<'static, Vec<(&'static str, Instant)>> {
    SPANS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Begin the span `key`, or begin it again from now.
pub fn start(key: &'static str) {
    if !enabled() {
        return;
    }
    let mut spans = spans();
    spans.retain(|(held, _)| *held != key);
    spans.push((key, Instant::now()));
}

/// Something on the way through `key`, which keeps running. A plain mark if
/// `key` is not running: a search picked from the history had no keystroke.
pub fn span(name: &str, key: &str, detail: impl Display) {
    if !enabled() {
        return;
    }
    let started = spans().iter().find(|(held, _)| *held == key).map(|(_, at)| *at);
    print(name, started.map(|at| at.elapsed()), &detail);
}

/// Begin the span `key` unless it is already running, for a start that can
/// be asked for again while the first is still on its way.
pub fn begin(key: &'static str) {
    if enabled() && !spans().iter().any(|(held, _)| *held == key) {
        start(key);
    }
}

/// The end of `key`, which a later `span` or `end` will not see again.
/// Nothing if `key` is not running: the audio element says it is playing
/// again after every stall, and only the first time after a press counts.
pub fn end(name: &str, key: &str, detail: impl Display) {
    if !enabled() {
        return;
    }
    let started = {
        let mut spans = spans();
        let found = spans.iter().position(|(held, _)| *held == key);
        found.map(|at| spans.remove(at).1)
    };
    took(name, started, detail);
}

/// One request to the server: never its query, which for a search is what
/// was typed, and never the token, which is a header and not seen here.
/// `status` is `None` when no answer came at all.
pub fn http(method: &str, path: &str, status: Option<u16>, started: Option<Instant>, bytes: usize) {
    if started.is_none() {
        return;
    }
    let path = path.split('?').next().unwrap_or(path);
    let status = status.map_or_else(|| "failed".to_string(), |code| code.to_string());
    took("http", started, format_args!("{method} {path} {status} {bytes}B"));
}

// ------------------------------------------------------------- navigation

/// The page asked for last, until what it shows has been rendered: a page
/// `Library` remembered first from memory, then again once the refetch
/// underneath lands.
struct Nav {
    name: String,
    started: Instant,
    /// `Library::epoch` when asked, which is how a fetch says it was this
    /// page's.
    epoch: u64,
    cached: bool,
    shown: bool,
    landed: bool,
}

static NAV: Mutex<Option<Nav>> = Mutex::new(None);

fn nav() -> std::sync::MutexGuard<'static, Option<Nav>> {
    NAV.lock().unwrap_or_else(|e| e.into_inner())
}

/// A page was asked for, `cached` if it can be shown from what was kept.
pub fn nav_asked(kind: &str, epoch: u64, cached: bool) {
    if !enabled() {
        return;
    }
    *nav() = Some(Nav {
        name: format!("nav-{kind}"),
        started: Instant::now(),
        epoch,
        cached,
        shown: false,
        landed: false,
    });
}

/// The fetch for the page asked for at `epoch` has its answer in the shelf.
pub fn nav_landed(epoch: u64) {
    if !enabled() {
        return;
    }
    if let Some(nav) = nav().as_mut().filter(|nav| nav.epoch == epoch) {
        nav.landed = true;
    }
}

/// The main area has rendered, `loading` or not. Called after every render
/// that touched the shelf, so it says which of them was the page's.
pub fn nav_rendered(loading: bool) {
    if !enabled() || loading {
        return;
    }
    let mut held = nav();
    let Some(nav) = held.as_mut() else { return };
    let span = Some(nav.started.elapsed());
    if !nav.shown {
        nav.shown = true;
        let from = if nav.cached { "cached" } else { "fetched" };
        print(&nav.name, span, &from);
        if nav.name == "nav-search" {
            end("search-shown", "search", from);
        }
        if !nav.cached {
            *held = None;
        }
    } else if nav.landed {
        print(&format!("{}-refreshed", nav.name), span, &"");
        *held = None;
    }
}

/// Prepended to a script, so its timing reports are sent only when asked
/// for. See map.js and player.js.
pub fn script_flag() -> &'static str {
    if enabled() {
        "window.stellyTimings = true;\n"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn the_process_started_a_moment_ago() {
        let ago = since_process_start().expect("readable on Linux");
        assert!(ago < Duration::from_secs(600), "{ago:?}");
    }
}
