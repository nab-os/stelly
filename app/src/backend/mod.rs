//! Where the app gets everything that is not the space: Qobuz, the block
//! list, the crawl and the pipeline, all through the server over HTTP.
//!
//! Deliberately absent: neighbours, paths, drift, the map, the sliders. Those
//! stay on the in-process `engine()`, over the synced copy of the space. Only
//! `embed` crosses, because the CLAP text tower is 500MB and its answer is 512
//! floats.

pub mod remote;

pub use crate::logbuffer::LogBuffer;
pub use remote::Remote;
use std::sync::RwLock;

/// The one backend there is. Named for what callers use it as.
pub type Backend = Remote;

/// The backend. A global for the same reason the engine is: every panel needs
/// it, and UI event handlers must be `Copy`.
///
/// Replaceable, not set-once: `bootstrap` wires up whatever pairing was saved,
/// and when that server does not answer the setup screen has to be able to
/// point somewhere else. With a `OnceLock` its `init` was silently ignored and
/// every attempt went back to the old address, whatever had been typed.
static BACKEND: RwLock<Option<&'static Backend>> = RwLock::new(None);

/// Leaks the one it replaces: callers hold `&'static Backend` across awaits,
/// and this happens once per pairing attempt, not per request.
pub fn init(backend: Backend) {
    let backend: &'static Backend = Box::leak(Box::new(backend));
    *BACKEND.write().unwrap_or_else(|e| e.into_inner()) = Some(backend);
}

pub fn backend() -> &'static Backend {
    BACKEND
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .expect("backend::init runs before launch")
}
