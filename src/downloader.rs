// src/downloader.rs

use crate::models::{DownloadJob, JobStatus, Segment};
use futures_util::StreamExt;
use reqwest::Client;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncSeekExt, AsyncWriteExt, SeekFrom};
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not get content length from server")]
    NoContentLength,
    #[error("download was paused by user")]
    Paused,
    #[error("task was aborted")]
    Aborted, // For cancellation
}

/// A stateless download worker that executes a DownloadJob.
pub struct DownloadWorker;

impl DownloadWorker {
    /// Executes or resumes a download job.
    /// This function handles the entire lifecycle of a single download attempt.
    pub async fn run(
        client: &Client,
        job: Arc<Mutex<DownloadJob>>,
        pause_flag: Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        loop { // NEW: Add a main loop
            // If the job is paused, wait here until it's un-paused.
            if pause_flag.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue; // Go back to the top of the loop and check the flag again.
            }

            // This only runs if we are not paused.
            Self::initialize_job_state(client, job.clone()).await?;

            let mut tasks = Vec::new();
            let num_segments = { job.lock().await.segments.len() };

            for i in 0..num_segments {
                let job_clone = Arc::clone(&job);
                let client_clone = client.clone();
                let pause_clone = Arc::clone(&pause_flag);
                tasks.push(tokio::spawn(async move {
                    Self::download_segment(client_clone, job_clone, i, pause_clone).await
                }));
            }
            
            for task_handle in tasks {
                match task_handle.await {
                    Ok(Ok(_)) => {}
                    Ok(Err(DownloadError::Paused)) => {
                        // A segment was paused. We need to signal the main `run` loop to
                        // stop and re-evaluate. We can't just `continue` here because
                        // other segment tasks are still running.
                        // The simplest way is to return a Paused error, which the outer
                        // logic will now handle by re-entering the waiting loop.
                        return Err(DownloadError::Paused);
                    }
                    Ok(Err(e)) => return Err(e),
                    Err(_) => return Err(DownloadError::Aborted),
                }
            }
            
            let mut job_lock = job.lock().await;
            if job_lock.progress() >= 1.0 {
                let temp_path = job_lock.temporary_path();
                let final_path = job_lock.destination.clone();
                tokio::fs::rename(temp_path, final_path).await?;
                job_lock.status = JobStatus::Completed;
                return Ok(()); // THE ONLY successful exit point.
            } else if !pause_flag.load(Ordering::SeqCst) {
                job_lock.status = JobStatus::Failed(Some("Incomplete download".to_string()));
                return Err(DownloadError::Aborted); // Use a specific error
            }
            // If we get here, it means we were paused. The main loop will handle waiting.
        }
    }
    /// Prepares the DownloadJob by fetching metadata and creating segments if needed.
    /// **MODIFIED FUNCTION**
    /// Prepares the DownloadJob by fetching metadata, creating segments, or loading resume state.
    async fn initialize_job_state(
        client: &Client,
        job: Arc<Mutex<DownloadJob>>,
    ) -> Result<(), DownloadError> {
        let mut job_lock = job.lock().await;

        if job_lock.status == JobStatus::Completed {
            return Ok(());
        }

        let temp_path = job_lock.temporary_path();

        // Check if we are resuming. We determine this if segments are already defined.
        let is_resuming = !job_lock.segments.is_empty();

        if is_resuming {
            println!("Resuming download for job ID: {}", job_lock.id);
            // Verify the temporary file exists and has the correct size.
            // If it's missing or wrong, we might need to restart the download.
            if let Ok(metadata) = tokio::fs::metadata(&temp_path).await {
                if metadata.len() != job_lock.total_size {
                    // The temporary file is corrupt or has changed.
                    // For now, we'll error out. A more advanced implementation
                    // could restart the download from scratch.
                    let reason = "Temporary file size mismatch. Please restart download.".to_string();
                    job_lock.status = JobStatus::Failed(Some(reason.clone()));
                    return Err(DownloadError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        reason,
                    )));
                }
            } else {
                // Temporary file is missing.
                let reason = "Temporary file is missing for resume.".to_string();
                job_lock.status = JobStatus::Failed(Some(reason.clone()));
                return Err(DownloadError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    reason,
                )));
            }
        } else {
            println!("Starting new download for job ID: {}", job_lock.id);
            // This is a new download. Fetch metadata and create segments.
            let resp = client.head(&job_lock.url).send().await?;
            let size = resp
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or(DownloadError::NoContentLength)?;
            
            job_lock.total_size = size;

            // Ensure the destination DIRECTORY exists.
            if let Some(parent) = job_lock.destination.parent() {
                tokio::fs::create_dir_all(parent).await?;
            }

            // Create and pre-allocate the TEMPORARY file.
            let file = OpenOptions::new().create(true).write(true).open(&temp_path).await?;
            file.set_len(size).await?;
            
            // Create the segments for the first time.
            let threads = job_lock.num_threads.max(1);
            let chunk_size = job_lock.total_size / threads as u64;
            for i in 0..threads {
                let start = i as u64 * chunk_size;
                // Make sure the last segment goes to the very end of the file.
                let end = if i == threads - 1 {
                    job_lock.total_size
                } else {
                    start + chunk_size
                };
                job_lock.segments.push(Segment {
                    start_byte: start,
                    end_byte: end,
                    current_pos: start,
                });
            }
        }
        
        job_lock.status = JobStatus::Downloading;
        Ok(())
    }

    /// Downloads a single segment of a file.
    async fn download_segment(
        client: Client,
        job: Arc<Mutex<DownloadJob>>,
        segment_index: usize,
        pause_flag: Arc<AtomicBool>,
    ) -> Result<(), DownloadError> {
        // --- MODIFIED FILE HANDLING ---
        // The temporary path is now retrieved here.
        let (url, temp_destination, range_header) = {
            let job_lock = job.lock().await;
            let segment = &job_lock.segments[segment_index];

            if segment.is_complete() {
                return Ok(());
            }

            // This now correctly starts from the segment's current position,
            // which is the key to resuming.
             let range = format!("bytes={}-{}", segment.current_pos, segment.end_byte - 1);
            (
                job_lock.url.clone(),
                job_lock.temporary_path(),
                range,
            )
        };

        let mut resp = client
            .get(&url)
            .header(reqwest::header::RANGE, range_header)
            .timeout(Duration::from_secs(30))
            .send()
            .await?
            .error_for_status()?;
        
        // Open the temporary file for writing.
        let mut file = OpenOptions::new().write(true).open(&temp_destination).await?;

        let mut stream = resp.bytes_stream();
        while let Some(chunk_result) = stream.next().await {
            if pause_flag.load(Ordering::SeqCst) {
                return Err(DownloadError::Paused);
            }
            
            let chunk = chunk_result?;
            
            // Lock scope to update state
            {
                let mut job_lock = job.lock().await;
                let segment = &mut job_lock.segments[segment_index];

                file.seek(SeekFrom::Start(segment.current_pos)).await?;
                file.write_all(&chunk).await?;

                let bytes_written = chunk.len() as u64;
                segment.current_pos += bytes_written;
                job_lock.downloaded_bytes += bytes_written;
            }
        }

        Ok(())
    }
}