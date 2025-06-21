// src/manager.rs

use crate::downloader::{DownloadError, DownloadWorker};
use crate::models::{DownloadJob, JobStatus};
use crate::state_manager::{StateError, StateManager};
use reqwest::Client;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Error)]
pub enum ManagerError {
    #[error("state manager error: {0}")]
    State(#[from] StateError),
    #[error("job with ID {0} not found")]
    JobNotFound(u64),
    #[error("download worker error: {0}")]
    Worker(#[from] DownloadError),
}

/// The central component that manages the state and lifecycle of all download jobs.
pub struct DownloadManager {
    state_manager: StateManager,
    jobs: Arc<Mutex<HashMap<u64, Arc<Mutex<DownloadJob>>>>>,
    http_client: Client,
    max_concurrent_downloads: usize,
    active_workers: Arc<Mutex<HashMap<u64, JoinHandle<()>>>>,
    // Stores cancellation tokens for the saver sub-tasks.
    cancellation_tokens: Arc<Mutex<HashMap<u64, CancellationToken>>>,
    // Stores pause flags for downloads.
    pause_flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    next_job_id: AtomicU64,
}

impl DownloadManager {
    pub async fn new(
        state_manager: StateManager,
        max_concurrent_downloads: usize,
    ) -> Result<Self, ManagerError> {
        let jobs_map = Arc::new(Mutex::new(HashMap::new()));
        let loaded_jobs = state_manager.load_all_jobs().await?;
        let mut max_id = 0;

        {
            let mut jobs_lock = jobs_map.lock().await;
            for mut job in loaded_jobs {
                if job.id > max_id {
                    max_id = job.id;
                }
                if matches!(job.status, JobStatus::Downloading) {
                    job.status = JobStatus::Paused;
                }
                jobs_lock.insert(job.id, Arc::new(Mutex::new(job)));
            }
        }

        let http_client = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/108.0.0.0 Safari/537.36")
            .build()
            .unwrap();

        Ok(Self {
            state_manager,
            http_client,
            jobs: jobs_map,
            max_concurrent_downloads,
            active_workers: Arc::new(Mutex::new(HashMap::new())),
            cancellation_tokens: Arc::new(Mutex::new(HashMap::new())),
            pause_flags: Arc::new(Mutex::new(HashMap::new())),
            next_job_id: AtomicU64::new(max_id + 1),
        })
    }
    
    pub async fn add_new_job(
        &self,
        url: String,
        destination: PathBuf,
        num_threads: usize,
    ) -> Result<u64, ManagerError> {
        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        let new_job = DownloadJob::new(job_id, url, destination, num_threads);
        
        self.state_manager.save_job(&new_job).await?;
        self.jobs.lock().await.insert(job_id, Arc::new(Mutex::new(new_job)));
            
        println!("Manager: Added new job {} to the queue.", job_id);
        Ok(job_id)
    }

    pub async fn run(self: Arc<Self>) {
        loop {
            self.prune_completed_workers().await;

            let mut startable_jobs = Vec::new();
            {
                let jobs_lock = self.jobs.lock().await;
                let workers_lock = self.active_workers.lock().await;
                let available_slots = self.max_concurrent_downloads.saturating_sub(workers_lock.len());

                if available_slots > 0 {
                    let mut queued_candidates: Vec<_> = jobs_lock.iter()
                        .filter(|(id, _)| !workers_lock.contains_key(id))
                        .filter_map(|(_, job_arc)| {
                            if let Ok(job) = job_arc.try_lock() {
                                if matches!(job.status, JobStatus::Queued) {
                                    Some(job.id)
                                } else { None }
                            } else { None }
                        })
                        .collect();

                    queued_candidates.sort();

                    for job_id in queued_candidates.into_iter().take(available_slots) {
                        if let Some(job_arc) = jobs_lock.get(&job_id) {
                            startable_jobs.push(job_arc.clone());
                        }
                    }
                }
            }

            for job_arc in startable_jobs {
                self.spawn_worker(job_arc).await;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    async fn spawn_worker(self: &Arc<Self>, job_arc: Arc<Mutex<DownloadJob>>) {
        let job_id = { job_arc.lock().await.id };

        let pause_flag = Arc::new(AtomicBool::new(false));
        let cancellation_token = CancellationToken::new();

        self.pause_flags.lock().await.insert(job_id, pause_flag.clone());
        self.cancellation_tokens.lock().await.insert(job_id, cancellation_token.clone());

        let self_clone = self.clone();
        let http_client = self.http_client.clone();

        let handle = tokio::spawn(async move {
            println!("Worker starting for job ID: {}", job_id);
            
            let saver_handle = {
                let job_clone_for_saver = job_arc.clone();
                let state_manager_clone = self_clone.state_manager.clone();
                let saver_token = cancellation_token.clone();

                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                            _ = saver_token.cancelled() => { break; }
                        }
                        let job = job_clone_for_saver.lock().await;
                        if matches!(job.status, JobStatus::Downloading) {
                            if let Err(e) = state_manager_clone.save_job(&job).await {
                                eprintln!("Periodic save for job {} failed: {}", job.id, e);
                            }
                        } else { break; }
                    }
                })
            };

            let result = DownloadWorker::run(&http_client, job_arc.clone(), pause_flag).await;

            cancellation_token.cancel();
            let _ = saver_handle.await;

            let mut job_lock = job_arc.lock().await;
            match result {
                Ok(_) => { println!("Worker for job {} finished successfully.", job_id); },
                Err(DownloadError::Paused) => {
                    job_lock.status = JobStatus::Paused;
                    println!("Worker for job {} was paused.", job_id);
                },
                Err(e) => {
                    let reason = e.to_string();
                    job_lock.status = JobStatus::Failed(Some(reason));
                    eprintln!("Worker for job {} failed: {}", job_id, e);
                }
            }

            if let Err(e) = self_clone.state_manager.save_job(&job_lock).await {
                eprintln!("Failed to save final state for job {}: {}", job_id, e);
            }
        });
        
        self.active_workers.lock().await.insert(job_id, handle);
    }
    
    async fn prune_completed_workers(&self) {
        // Step 1: Collect the IDs of all workers that have finished.
        // We do this in a separate scope to release the lock on active_workers quickly.
        let mut finished_ids = Vec::new();
        {
            let workers = self.active_workers.lock().await;
            for (id, handle) in workers.iter() {
                if handle.is_finished() {
                    finished_ids.push(*id);
                }
            }
        } // Lock on active_workers is released here.

        // Step 2: If there are any finished workers, remove them and their
        // associated resources from all maps.
        if !finished_ids.is_empty() {
            // Lock all the maps we need to modify.
            let mut workers = self.active_workers.lock().await;
            let mut tokens = self.cancellation_tokens.lock().await;
            let mut flags = self.pause_flags.lock().await;

            for id in finished_ids {
                println!("Manager: Pruning completed worker for job {}.", id);
                workers.remove(&id);
                tokens.remove(&id);
                flags.remove(&id);
            }
        }
    }

    pub async fn pause_download(&self, job_id: u64) -> Result<(), ManagerError> {
        println!("Manager: Received pause request for job {}.", job_id);
        if let Some(pause_flag) = self.pause_flags.lock().await.get(&job_id) {
            pause_flag.store(true, Ordering::SeqCst);
        } else {
            let jobs = self.jobs.lock().await;
            if let Some(job_arc) = jobs.get(&job_id) {
                let mut job = job_arc.lock().await;
                if job.status == JobStatus::Queued {
                    job.status = JobStatus::Paused;
                    self.state_manager.save_job(&job).await?;
                    println!("Manager: Job {} was Queued, moved to Paused state.", job_id);
                }
            } else { return Err(ManagerError::JobNotFound(job_id)); }
        }
        Ok(())
    }

    pub async fn resume_download(&self, job_id: u64) -> Result<(), ManagerError> {
        println!("Manager: Received resume request for job {}.", job_id);
        let jobs = self.jobs.lock().await;
        if let Some(job_arc) = jobs.get(&job_id) {
            let mut job = job_arc.lock().await;
            if matches!(job.status, JobStatus::Paused) {
                job.status = JobStatus::Queued;
                self.state_manager.save_job(&job).await?;
                println!("Manager: Job {} set to Queued.", job_id);
            } else {
                println!("Manager: Job {} is not Paused, resume ignored.", job_id);
            }
            Ok(())
        } else {
            Err(ManagerError::JobNotFound(job_id))
        }
    }

    pub async fn cancel_download(&self, job_id: u64, delete_file: bool) -> Result<(), ManagerError> {
        println!("Manager: Received cancel request for job {}.", job_id);
        
        if let Some(token) = self.cancellation_tokens.lock().await.remove(&job_id) {
            token.cancel();
        }
        self.pause_flags.lock().await.remove(&job_id);
        
        if let Some(handle) = self.active_workers.lock().await.remove(&job_id) {
            handle.abort();
            let _ = handle.await; // Wait for it to fully stop.
            println!("Manager: Aborted and awaited worker for job {}.", job_id);
        }

        if let Some(job_arc) = self.jobs.lock().await.remove(&job_id) {
            self.state_manager.delete_job(job_id).await?;
            println!("Manager: Removed job {} from the database.", job_id);

            if delete_file {
                let job = job_arc.lock().await;
                let temp_path = job.temporary_path();
                let final_path = &job.destination;

                let _ = tokio::fs::remove_file(&temp_path).await;
                let _ = tokio::fs::remove_file(&final_path).await;
                println!("Manager: Cleaned up files for job {}.", job_id);
            }
            Ok(())
        } else {
            Err(ManagerError::JobNotFound(job_id))
        }
    }

    pub async fn get_all_jobs(&self) -> Vec<DownloadJob> {
        let jobs_lock = self.jobs.lock().await;
        let mut result = Vec::with_capacity(jobs_lock.len());
        for job_arc in jobs_lock.values() {
            let job = job_arc.lock().await.clone();
            result.push(job);
        }
        result
    }
}