//! A bounded log read from a cursor: a pipeline stage's output, as the server
//! streams it over SSE and the pipeline view reads it back.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tokio::sync::Notify;

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
    /// Woken on every push, so a stream of the log sleeps until there is
    /// something to send rather than looking every so often.
    grown: Notify,
}

impl LogBuffer {
    pub fn push(&self, line: impl Into<String>) {
        let mut lines = self.lines.lock().unwrap();
        lines.push_back(line.into());
        while lines.len() > MAX_LOG_LINES {
            lines.pop_front();
        }
        self.pushed.fetch_add(1, Ordering::SeqCst);
        drop(lines);
        self.grown.notify_waiters();
    }

    /// Returns once something has been pushed past `since`, at once if it
    /// already has.
    pub async fn past(&self, since: u64) {
        loop {
            // Listening before looking, so a push in between is not missed.
            let grown = self.grown.notified();
            let mut grown = std::pin::pin!(grown);
            grown.as_mut().enable();
            if self.cursor() > since {
                return;
            }
            grown.await;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    async fn a_reader_wakes_on_the_next_line() {
        let log = Arc::new(LogBuffer::default());
        log.push("one");
        // Already past: no wait.
        log.past(0).await;

        let waiting = tokio::spawn({
            let log = log.clone();
            async move { log.past(1).await }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiting.is_finished());
        log.push("two");
        tokio::time::timeout(Duration::from_secs(1), waiting).await.unwrap().unwrap();
        assert_eq!(log.since(1).lines, vec!["two".to_string()]);
    }
}
