//! A crawl running in the background while you browse.
//!
//! The loop lives in the backend, its own thread locally, the server
//! remotely, and this is the view's handle on it. That is what lets a crawl
//! outlive the panel, and remotely the client.

use super::POLL;
use dioxus::prelude::*;
use crate::api::CrawlStatus;
use crate::backend::backend;
use crate::api::DEFAULT_MAX_DISTANCE;

#[derive(Clone, Copy)]
pub struct Crawler {
    pub status: Signal<CrawlStatus>,
    /// Set the moment start or stop is pressed, so the buttons react before
    /// the next poll comes back and confirms it.
    pub pending: Signal<bool>,
}

impl Crawler {
    pub fn new() -> Self {
        Self {
            status: Signal::new(CrawlStatus::default()),
            pending: Signal::new(false),
        }
    }

    pub fn running(&self) -> bool {
        self.status.read().running
    }

    pub fn last(&self) -> Option<String> {
        self.status.read().last.clone()
    }

    pub fn start(mut self) {
        if self.running() {
            return;
        }
        self.pending.set(true);
        spawn(async move {
            let mut crawler = self;
            if let Err(err) = backend().crawl_start(DEFAULT_MAX_DISTANCE).await {
                crawler.status.write().last = Some(format!("{err:#}"));
            }
            crawler.pending.set(false);
        });
    }

    pub fn request_stop(self) {
        spawn(async move {
            let mut crawler = self;
            if let Err(err) = backend().crawl_stop().await {
                crawler.status.write().last = Some(format!("{err:#}"));
            }
        });
    }
}

impl Default for Crawler {
    fn default() -> Self {
        Self::new()
    }
}

/// Keep the crawl status fresh. Mounted once, for the life of the window.
pub fn use_crawl_status(crawler: Crawler) {
    use_future(move || async move {
        let mut crawler = crawler;
        loop {
            match backend().crawl_status().await {
                Ok(status) => crawler.status.set(status),
                // A server that has gone away should not spin the log; the
                // pipeline panel reports it once and the next poll retries.
                Err(_) => crawler.status.write().running = false,
            }
            tokio::time::sleep(POLL).await;
        }
    });
}
