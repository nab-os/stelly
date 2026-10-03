//! Core of the Stelly client.
//!
//! Holds nothing of the space: navigation, the map's points and the search
//! over the space are all asked of the server, like everything else, see
//! `backend`. What it keeps on disk is the pairing and a small cache, so the
//! window has something to show before the network answers.
//!
//! Shared by the desktop and Android apps. What the server needs too lives in
//! `stelly_core`, re-exported here under the same names.

pub mod backend;
pub mod cache;
pub mod update;

pub use stelly_core::{api, logbuffer, qobuz, session};

/// The window, and everything in it. Shared by every platform that has one.
#[cfg(feature = "gui")]
pub mod app;
#[cfg(feature = "gui")]
pub mod ui;

// No JNI entry point here on purpose: `dioxus-desktop` already exports
// `start_app` for Android, so defining another would be a duplicate
// `#[no_mangle]`. The Android build is the ordinary binary target below.

/// The webview backend, under whichever name this build's platform gives it.
/// `dioxus::desktop` and `dioxus::mobile` are the same crate re-exported
/// twice, so aliasing lets `app` and `ui` ignore which screen they got.
#[cfg(feature = "desktop")]
pub(crate) use dioxus::desktop as platform;
#[cfg(all(feature = "mobile", not(feature = "desktop")))]
pub(crate) use dioxus::mobile as platform;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::OnceLock;

// ------------------------------------------------------------------ paths

/// Where a client keeps its own files. Overridden by `set_data_dir` from the
/// Android entry point, a hardcoded `/data/data/<pkg>` is wrong on work
/// profiles and secondary users.
static DATA_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Set the client data directory. Call before anything reads a path.
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR_OVERRIDE.set(dir);
}

/// Where the client keeps its pairing and its cache.
///
/// Never the server's `STELLY_DATA_DIR`: with both on one machine, the two
/// would otherwise share files neither expects the other to touch.
pub fn client_data_dir() -> PathBuf {
    if let Some(dir) = DATA_DIR_OVERRIDE.get() {
        return dir.clone();
    }
    if let Ok(dir) = std::env::var("STELLY_CLIENT_DIR") {
        return PathBuf::from(dir);
    }

    #[cfg(target_os = "android")]
    if let Some(dir) = android_files_dir() {
        return dir;
    }

    // Windows sets neither XDG_DATA_HOME nor HOME, so the fallback below would
    // land in whatever directory the app was started from. Local rather than
    // roaming: a cache has no business following a profile around.
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(dir).join("stelly");
    }

    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".local")
                .join("share")
        });
    base.join("stelly")
}

/// The app's private directory, worked out without JNI.
///
/// Android sets no `HOME`, so the XDG fallback lands on `/` and every write
/// fails with EROFS. `/proc/self/cmdline` holds the package name, which is
/// enough to build the path without hardcoding it.
#[cfg(target_os = "android")]
fn android_files_dir() -> Option<PathBuf> {
    let raw = std::fs::read_to_string("/proc/self/cmdline").ok()?;
    let package = raw
        .split('\0')
        .next()?
        .split(':')
        .next()?
        .trim()
        .to_string();
    if package.is_empty() {
        return None;
    }

    // `/data/user/0` is the real location; `/data/data` is a compatibility
    // symlink that does not exist for secondary users or work profiles.
    for base in ["/data/user/0", "/data/data"] {
        let dir = PathBuf::from(base).join(&package).join("files");
        if std::fs::create_dir_all(&dir).is_ok() {
            return Some(dir.join("stelly"));
        }
    }
    None
}

// ----------------------------------------------------------------- wiring

/// The server this client talks to.
///
/// Two environment variables on desktop:
///
/// ```sh
/// STELLY_SERVER=https://nas.tailnet.ts.net:7700
/// STELLY_TOKEN=<what `stelly-server pair` printed>
/// ```
///
/// Without them, whatever the setup screen stored in `server.json`.
pub struct Wiring {
    base: String,
    token: String,
}

/// A server address and a device token, as the setup screen stores them.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct ServerConfig {
    pub base: String,
    pub token: String,
}

impl ServerConfig {
    pub fn path() -> PathBuf {
        client_data_dir().join("server.json")
    }

    pub fn load() -> Option<Self> {
        serde_json::from_slice(&std::fs::read(Self::path()).ok()?).ok()
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(self)?)?;
        Ok(())
    }

    /// Forget the pairing, so the next start falls back to the local half.
    /// Absent is the goal, so a file that was not there is success.
    pub fn clear() -> Result<()> {
        match std::fs::remove_file(Self::path()) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        }
    }
}

impl Wiring {
    /// Environment first, stored config second. Neither is an error the
    /// caller answers with the setup screen.
    pub fn from_env() -> Result<Self> {
        if let Ok(base) = std::env::var("STELLY_SERVER") {
            let token = std::env::var("STELLY_TOKEN").map_err(|_| {
                anyhow::anyhow!(
                    "STELLY_SERVER is set but STELLY_TOKEN is not.\n\
                     Pair this device on the server:\n  \
                     stelly-server pair --name \"{}\" --scope play",
                    hostname()
                )
            })?;
            return Ok(Wiring::remote(base, token));
        }

        if let Some(stored) = ServerConfig::load() {
            return Ok(Wiring::remote(stored.base, stored.token));
        }

        anyhow::bail!("not paired with a server yet")
    }

    pub fn remote(base: String, token: String) -> Self {
        Wiring { base, token }
    }

    pub fn into_backend(self) -> backend::Backend {
        backend::Backend::new(self.base, self.token)
    }
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this device".into())
}
