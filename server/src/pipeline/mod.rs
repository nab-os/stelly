//! The pipeline: from a catalogue of track ids to a space you can walk.
//!
//!   crawl        favourites, then similar artists, into `tracks`   (crate::crawl)
//!   analyse      middle 90s of each track -> CLAP + descriptors    (analyse)
//!   build-space  features -> space.bin / space.json                (assemble)
//!   layout       UMAP of the space -> 2D map coordinates           (layout)
//!
//! Every stage is resumable and runs in the server's process, whether a
//! client started it or the command line did. A stage reports through a
//! `Job`, which is also how it learns it has been asked to stop.

pub mod analyse;
pub mod assemble;
pub mod audio;
pub mod clap;
pub mod demo;
pub mod descriptors;
pub mod labels;
pub mod layout;
pub mod models;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use two_khz::api::Progress;

/// Where a `Job` leaves its progress: the step, and when that step began.
pub type ProgressSlot = Arc<Mutex<Option<(Progress, Instant)>>>;

/// Where a stage's progress goes, and whether it should stop.
#[derive(Clone)]
pub struct Job {
    sink: Arc<dyn Fn(&str) + Send + Sync>,
    cancel: Arc<AtomicBool>,
    /// How far along the current step is, for a status to show as a bar.
    progress: ProgressSlot,
}

impl Job {
    pub fn new(sink: impl Fn(&str) + Send + Sync + 'static, cancel: Arc<AtomicBool>) -> Self {
        Self {
            sink: Arc::new(sink),
            cancel,
            progress: Arc::default(),
        }
    }

    /// Report progress into `slot`, where whoever runs the job can read it.
    pub fn reporting(mut self, slot: ProgressSlot) -> Self {
        self.progress = slot;
        self
    }

    /// Progress to stderr, as the command line wants it. Never cancelled.
    pub fn stderr() -> Self {
        Self::new(|line| eprintln!("{line}"), Arc::new(AtomicBool::new(false)))
    }

    pub fn log(&self, line: impl AsRef<str>) {
        (self.sink)(line.as_ref());
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    /// `done` of `total` through `step`. The clock restarts when the step
    /// changes, so the time spent is the step's own.
    pub fn advance(&self, step: &str, done: u64, total: u64) {
        let mut slot = self.progress.lock().unwrap_or_else(|e| e.into_inner());
        let started = match &*slot {
            Some((current, started)) if current.step == step => *started,
            _ => Instant::now(),
        };
        let progress = Progress {
            step: step.to_string(),
            done,
            total,
            seconds: started.elapsed().as_secs_f64(),
        };
        *slot = Some((progress, started));
    }
}

/// What a status reports from a `Job`'s progress slot.
pub fn progress_of(slot: &ProgressSlot) -> Option<Progress> {
    let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    slot.as_ref().map(|(progress, started)| Progress {
        seconds: started.elapsed().as_secs_f64(),
        ..progress.clone()
    })
}

/// Where everything lives on this machine.
#[derive(Debug, Clone)]
pub struct Paths {
    /// Where `.env` is read from, and where `login` writes the token.
    pub env_dir: PathBuf,
    /// The corpus: the database, the space, the device tokens. The only thing
    /// here worth a backup.
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// Model weights, shared across corpora, so they do not follow the data
    /// directory unless told to.
    pub model_dir: PathBuf,
    /// Fetched excerpts. Disposable.
    pub cache_dir: PathBuf,
}

impl Paths {
    /// `TWO_KHZ_DATA_DIR`, `TWO_KHZ_MODEL_DIR`, `TWO_KHZ_CACHE_DIR` and
    /// `TWO_KHZ_ENV_DIR`, each falling back to a place in the checkout.
    pub fn from_env() -> Self {
        let repo_root = crate::qobuz::repo_root();
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let data_dir = var("TWO_KHZ_DATA_DIR").unwrap_or_else(|| repo_root.join("data"));
        Self {
            db_path: data_dir.join("two_khz.db"),
            model_dir: var("TWO_KHZ_MODEL_DIR")
                .unwrap_or_else(|| repo_root.join("data").join("models")),
            cache_dir: var("TWO_KHZ_CACHE_DIR")
                .unwrap_or_else(|| repo_root.join("cache").join("audio")),
            env_dir: var("TWO_KHZ_ENV_DIR").unwrap_or_else(|| repo_root.clone()),
            data_dir,
        }
    }

    /// Everything under one directory, for the demo corpus and tests. Models
    /// stay shared: nobody wants 600MB of weights per scratch directory.
    pub fn under(root: &std::path::Path) -> Self {
        let shared = Self::from_env();
        let data_dir = root.join("data");
        Self {
            db_path: data_dir.join("two_khz.db"),
            cache_dir: root.join("cache"),
            data_dir,
            model_dir: shared.model_dir,
            env_dir: shared.env_dir,
        }
    }
}
