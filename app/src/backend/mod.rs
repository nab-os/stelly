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
use std::sync::OnceLock;

/// The one backend there is. Named for what callers use it as.
pub type Backend = Remote;

/// The backend, for the life of the process. A global for the same reason the
/// engine is: every panel needs it, and UI event handlers must be `Copy`.
static BACKEND: OnceLock<Backend> = OnceLock::new();

pub fn init(backend: Backend) {
    let _ = BACKEND.set(backend);
}

pub fn backend() -> &'static Backend {
    BACKEND.get().expect("backend::init runs before launch")
}
