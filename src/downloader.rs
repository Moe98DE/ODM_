use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::time::Duration;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::Mutex;
use reqwest::Client;
use thiserror::Error;
use serde::{Serialize, Deserialize};
use futures_util::StreamExt;

/// Custom errors for download operations.
#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid url")]
    InvalidUrl,
    #[error("download paused")]
    Paused,
}

/// Download status enumeration.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DownloadStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed,
}

/// Options for a download task.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadOptions {
    pub url: String,
    pub destination: PathBuf,
    pub retries: usize,
    pub timeout: Duration,
    /// Number of concurrent segments to use when downloading.
    pub threads: usize,
}

impl Default for DownloadOptions {
    fn default() -> Self {
        Self {
            url: String::new(),
            destination: PathBuf::new(),
            retries: 3,
            timeout: Duration::from_secs(30),
            threads: 4,
        }
    }
}

/// Downloader struct manages the downloading of a single file.
#[derive(Debug)]
pub struct Downloader {
    client: Client,
    options: DownloadOptions,
    status: Mutex<DownloadStatus>,
    downloaded: Arc<AtomicU64>,
    total_size: Arc<AtomicU64>,
    pause_flag: Arc<AtomicBool>,
}

impl Downloader {
    /// Create a new downloader with provided options.
    pub fn new(options: DownloadOptions) -> Self {
        let client = Client::new();
        Self {
            client,
            options,
            status: Mutex::new(DownloadStatus::Queued),
            downloaded: Arc::new(AtomicU64::new(0)),
            total_size: Arc::new(AtomicU64::new(0)),
            pause_flag: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Begin the download process.
    pub async fn start(&self) -> Result<(), DownloadError> {
        {
            let mut status = self.status.lock().await;
            *status = DownloadStatus::Downloading;
        }
        self.pause_flag.store(false, Ordering::SeqCst);

        let mut attempts = 0;
        loop {
            let result = self.download_once().await;
            match result {
                Ok(_) => {
                    let mut status = self.status.lock().await;
                    *status = DownloadStatus::Completed;
                    return Ok(());
                }
                Err(DownloadError::Paused) => {
                    let mut status = self.status.lock().await;
                    *status = DownloadStatus::Paused;
                    return Err(DownloadError::Paused);
                }
                Err(e) => {
                    attempts += 1;
                    if attempts > self.options.retries {
                        let mut status = self.status.lock().await;
                        *status = DownloadStatus::Failed;
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Pause the download process.
    pub async fn pause(&self) {
        self.pause_flag.store(true, Ordering::SeqCst);
        let mut status = self.status.lock().await;
        *status = DownloadStatus::Paused;
    }

    /// Resume the download from pause state.
    pub async fn resume(&self) -> Result<(), DownloadError> {
        let status = self.status.lock().await.clone();
        if status != DownloadStatus::Paused {
            return Err(DownloadError::Paused);
        }
        drop(status);
        self.start().await
    }

    /// Internal method to perform one attempt at downloading.
    async fn download_once(&self) -> Result<(), DownloadError> {
        let resp = self.client.head(&self.options.url)
            .timeout(self.options.timeout)
            .send()
            .await?;
        let size = resp.headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or(DownloadError::InvalidUrl)?;
        self.total_size.store(size, Ordering::SeqCst);

        if let Some(parent) = self.options.destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(&self.options.destination)
            .await?;
        let existing = file.metadata().await?.len();
        if existing < size {
            file.set_len(size).await?;
        }
        self.downloaded.store(existing.min(size), Ordering::SeqCst);
        drop(file);

        if existing >= size {
            return Ok(());
        }

        let start_offset = existing;

        let threads = self.options.threads.max(1);
        let mut tasks = Vec::new();
        let remaining = size - start_offset;
        let chunk_size = remaining / threads as u64;
        for i in 0..threads {
            let start = start_offset + i as u64 * chunk_size;
            let mut end = if i == threads - 1 { size - 1 } else { start + chunk_size - 1 };
            if end >= size { end = size - 1; }
            let url = self.options.url.clone();
            let path = self.options.destination.clone();
            let client = self.client.clone();
            let downloaded = self.downloaded.clone();
            let pause_flag = self.pause_flag.clone();
            let timeout = self.options.timeout;
            tasks.push(tokio::spawn(async move {
                let mut resp = client.get(&url)
                    .header(reqwest::header::RANGE, format!("bytes={}-{}", start, end))
                    .timeout(timeout)
                    .send()
                    .await?
                    .error_for_status()?;

                let mut file = OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .await?;
                file.seek(SeekFrom::Start(start)).await?;

                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    if pause_flag.load(Ordering::SeqCst) {
                        return Err(DownloadError::Paused);
                    }
                    let bytes = chunk?;
                    file.write_all(&bytes).await?;
                    downloaded.fetch_add(bytes.len() as u64, Ordering::SeqCst);
                }
                Ok::<(), DownloadError>(())
            }));
        }

        for task in tasks {
            task.await??;
        }

        Ok(())
    }
}

impl Downloader {
    /// Get the current progress as a fraction in range 0.0..=1.0 if known.
    pub fn progress(&self) -> Option<f32> {
        let total = self.total_size.load(Ordering::SeqCst);
        if total == 0 {
            return None;
        }
        let done = self.downloaded.load(Ordering::SeqCst);
        Some(done as f32 / total as f32)
    }

    /// Get the current status of the download.
    pub async fn status(&self) -> DownloadStatus {
        *self.status.lock().await
    }

    /// Verify the SHA-256 checksum of the downloaded file.
    pub async fn verify_sha256(&self, expected: &str) -> Result<bool, crate::integrity::IntegrityError> {
        use crate::integrity::sha256_sum;
        let sum = sha256_sum(&self.options.destination).await?;
        Ok(sum == expected)
    }
}

