use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use serde::{Serialize, Deserialize};

use crate::downloader::{Downloader, DownloadOptions, DownloadError};

/// Represents a task in the download queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadTask {
    pub options: DownloadOptions,
    /// Optional SHA-256 checksum for integrity verification.
    pub sha256: Option<String>,
}

/// Download queue manages multiple downloads sequentially.
#[derive(Debug)]
pub struct DownloadQueue {
    queue: Mutex<VecDeque<DownloadTask>>,
    active: Mutex<Option<Arc<Downloader>>>,
}

impl DownloadQueue {
    /// Create a new empty download queue.
    pub fn new() -> Self {
        Self { queue: Mutex::new(VecDeque::new()), active: Mutex::new(None) }
    }

    /// Add a new task to the queue.
    pub async fn push(&self, task: DownloadTask) {
        let mut queue = self.queue.lock().await;
        queue.push_back(task);
    }

    /// Start processing the queue sequentially.
    pub async fn start(&self) {
        loop {
            let task = {
                let mut queue = self.queue.lock().await;
                queue.pop_front()
            };
            match task {
                Some(task) => {
                    let downloader = Arc::new(Downloader::new(task.options));
                    {
                        let mut active = self.active.lock().await;
                        *active = Some(downloader.clone());
                    }
                    if let Err(e) = downloader.start().await {
                        eprintln!("download failed: {e}");
                    } else if let Some(expected) = task.sha256.as_deref() {
                        match downloader.verify_sha256(expected).await {
                            Ok(true) => {},
                            Ok(false) => eprintln!("integrity check failed"),
                            Err(err) => eprintln!("integrity error: {err}")
                        }
                    }
                    {
                        let mut active = self.active.lock().await;
                        *active = None;
                    }
                }
                None => break,
            }
        }
    }

    /// Attempt to pause the currently active download.
    pub async fn pause_current(&self) {
        if let Some(active) = self.active.lock().await.as_ref() {
            active.pause().await;
        }
    }

    /// Attempt to resume the currently active download.
    pub async fn resume_current(&self) -> Result<(), DownloadError> {
        if let Some(active) = self.active.lock().await.as_ref() {
            active.resume().await
        } else {
            Ok(())
        }
    }

    /// Get the progress of the currently active download if any.
    pub async fn progress(&self) -> Option<f32> {
        if let Some(active) = self.active.lock().await.as_ref() {
            active.progress()
        } else {
            None
        }
    }
}

