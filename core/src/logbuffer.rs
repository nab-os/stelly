//! A bounded log read from a cursor: a pipeline stage's output, as the server
//! streams it over SSE and the pipeline view reads it back.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Keep the log bounded; `analyse` over a large corpus prints thousands of
/// lines and nobody scrolls back that far.
pub const MAX_LOG_LINES: usize = 400;

// ------------------------------------------------------------------ the log

/// A bounded log that can be read from a cursor. The server fills one as a
/// stage runs; a client fills its own from the SSE stream. Both are read the
/// same way.
#[derive(Default)]
pub struct LogBuffer {
    lines: Mutex<VecDeque<String>>,
    /// How many lines have ever been pushed. Counting rather than indexing
    /// means a cursor taken before an overflow still resolves.
    pushed: AtomicU64,
}

impl LogBuffer {
    pub fn push(&self, line: impl Into<String>) {
        let mut lines = self.lines.lock().unwrap();
        lines.push_back(line.into());
        while lines.len() > MAX_LOG_LINES {
            lines.pop_front();
        }
        self.pushed.fetch_add(1, Ordering::SeqCst);
    }

    pub fn clear(&self) {
        // `pushed` deliberately does not reset: an old cursor should see the
        // clear as "nothing new", not a full replay.
        self.lines.lock().unwrap().clear();
    }

    pub fn cursor(&self) -> u64 {
        self.pushed.load(Ordering::SeqCst)
    }

    /// Everything currently retained. The views mirror this rather than
    /// accumulating their own copy, so a reconnecting stream does not show the
    /// tail twice.
    pub fn all(&self) -> Vec<String> {
        self.lines.lock().unwrap().iter().cloned().collect()
    }

    /// Everything pushed since `since`, and the cursor to pass next time.
    pub fn since(&self, since: u64) -> crate::api::LogSlice {
        let lines = self.lines.lock().unwrap();
        let pushed = self.pushed.load(Ordering::SeqCst);
        let first_retained = pushed.saturating_sub(lines.len() as u64);
        let start = since.max(first_retained);
        let skip = (start - first_retained) as usize;

        crate::api::LogSlice {
            lines: lines.iter().skip(skip).cloned().collect(),
            cursor: pushed,
        }
    }
}
