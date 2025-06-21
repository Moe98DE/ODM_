// src/manager.rs

// Add new imports
use crate::downloader::{DownloadError, DownloadWorker};
use crate::models::{DownloadJob, JobStatus};
use crate::state_manager::{StateError, StateManager};
use reqwest::header::{HeaderMap, HeaderValue, USER_AGENT};
use reqwest::{Client, redirect::Policy}; // Import Policy
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering}; // For pause flags
use std::sync::Arc;
use std::time::Duration; // For polling delay
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle; // To track running tasks

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
    http_client: Client, // A single client for all downloads
    max_concurrent_downloads: usize,
    // Tracks the JoinHandles of currently running download workers.
    active_workers: Arc<Mutex<HashMap<u64, JoinHandle<()>>>>,
    // Tracks pause flags for each job.
    pause_flags: Arc<Mutex<HashMap<u64, Arc<AtomicBool>>>>,
    // Add a counter to generate unique job IDs.
    next_job_id: AtomicU64,
}

impl DownloadManager {
    pub async fn new(
        state_manager: StateManager,
        max_concurrent_downloads: usize,
    ) -> Result<Self, ManagerError> {
        let jobs_map = Arc::new(Mutex::new(HashMap::new())); // This is the map we will use
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
            jobs: jobs_map, // <-- FIX: Use the map we populated from the DB.
            max_concurrent_downloads,
            active_workers: Arc::new(Mutex::new(HashMap::new())),
            pause_flags: Arc::new(Mutex::new(HashMap::new())),
            next_job_id: AtomicU64::new(max_id + 1),
        })
    }
    
 /// **NEW PUBLIC METHOD**
    /// Creates a new download job, saves it, and adds it to the queue.
    pub async fn add_new_job(
        &self,
        url: String,
        destination: PathBuf,
        num_threads: usize,
    ) -> Result<u64, ManagerError> {
        // Atomically fetch and increment the job ID.
        let job_id = self.next_job_id.fetch_add(1, Ordering::SeqCst);
        
        let new_job = DownloadJob::new(job_id, url, destination, num_threads);
        
        // Save the job to the database first.
        self.state_manager.save_job(&new_job).await?;
        
        // Then, add it to the in-memory map.
        self.jobs
            .lock()
            .await
            .insert(job_id, Arc::new(Mutex::new(new_job)));
            
        println!("Manager: Added new job {} to the queue.", job_id);
            
        Ok(job_id)
    }

    /// **NEW FUNCTION**
    /// The main loop of the download manager. This should be spawned as a background task.
    pub async fn run(self: Arc<Self>) {
        loop {
            self.prune_completed_workers().await;

            let mut startable_jobs = Vec::new();
            {
                let jobs_lock = self.jobs.lock().await;
                let workers_lock = self.active_workers.lock().await;

                let available_slots = self.max_concurrent_downloads.saturating_sub(workers_lock.len());
                if available_slots == 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
                
                // Find all queued jobs that are not already running
                let mut queued_candidates: Vec<_> = jobs_lock.iter()
                    .filter(|(id, _)| !workers_lock.contains_key(id))
                    .filter_map(|(_, job_arc)| {
                        // Use try_lock to avoid deadlocking if a job is being modified elsewhere.
                        // This is a defensive measure.
                        if let Ok(job) = job_arc.try_lock() {
                            if matches!(job.status, JobStatus::Queued) {
                                Some(job.id)
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect();

                // **THE FIX**: Sort candidates by ID to ensure FIFO behavior.
                queued_candidates.sort();

                // Select the jobs to start based on available capacity.
                for job_id in queued_candidates.into_iter().take(available_slots) {
                    if let Some(job_arc) = jobs_lock.get(&job_id) {
                        startable_jobs.push(job_arc.clone());
                    }
                }
            } // Release locks

            for job_arc in startable_jobs {
                self.spawn_worker(job_arc).await;
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// **NEW FUNCTION**
    /// Spawns a DownloadWorker for a given job.
    async fn spawn_worker(self: &Arc<Self>, job_arc: Arc<Mutex<DownloadJob>>) {
        let job_id = { job_arc.lock().await.id };

        // Create a new pause flag for this job.
        let pause_flag = Arc::new(AtomicBool::new(false));
        self.pause_flags.lock().await.insert(job_id, pause_flag.clone());

        let self_clone = self.clone();
        let http_client = self.http_client.clone();

        let handle = tokio::spawn(async move {
            println!("Worker starting for job ID: {}", job_id);
            
            // The download logic
            let result = DownloadWorker::run(&http_client, job_arc.clone(), pause_flag).await;

            // After the worker finishes (or fails), update the job state and save it.
            let mut job_lock = job_arc.lock().await;
            match result {
                Ok(_) => {
                    // The run function handles setting the Completed status internally.
                    println!("Worker for job {} finished successfully.", job_id);
                }
                Err(DownloadError::Paused) => {
                    job_lock.status = JobStatus::Paused;
                    println!("Worker for job {} was paused.", job_id);
                }
                Err(e) => {
                    let reason = e.to_string();
                    job_lock.status = JobStatus::Failed(Some(reason));
                    eprintln!("Worker for job {} failed: {}", job_id, e);
                }
            }

            // Persist the final state of the job to the database.
            if let Err(e) = self_clone.state_manager.save_job(&job_lock).await {
                eprintln!("Failed to save final state for job {}: {}", job_id, e);
            }
        });
        
        // Add the worker's handle to our tracking map.
        self.active_workers.lock().await.insert(job_id, handle);
    }
    
    /// **NEW FUNCTION**
    /// Removes workers from the tracking map if they have finished their execution.
    async fn prune_completed_workers(&self) {
        self.active_workers.lock().await.retain(|_id, handle| !handle.is_finished());
    }

    /// **NEW PUBLIC METHOD**
    /// Pauses an active download.
    pub async fn pause_download(&self, job_id: u64) -> Result<(), ManagerError> {
        println!("Manager: Received pause request for job {}.", job_id);
        
        // Signal the worker to pause by setting its flag to true.
        if let Some(pause_flag) = self.pause_flags.lock().await.get(&job_id) {
            pause_flag.store(true, Ordering::SeqCst);
        } else {
            // If there's no pause flag, the job might not be running.
            // We can still update its state if it's Queued.
            let jobs = self.jobs.lock().await;
            if let Some(job_arc) = jobs.get(&job_id) {
                let mut job = job_arc.lock().await;
                if job.status == JobStatus::Queued {
                    job.status = JobStatus::Paused;
                    self.state_manager.save_job(&job).await?;
                    println!("Manager: Job {} was Queued, moved to Paused state.", job_id);
                }
            } else {
                return Err(ManagerError::JobNotFound(job_id));
            }
        }
        Ok(())
    }

    /// **NEW PUBLIC METHOD**
    /// Resumes a paused download by setting its state back to 'Queued'.
    /// The main 'run' loop will automatically pick it up.
    pub async fn resume_download(&self, job_id: u64) -> Result<(), ManagerError> {
        println!("Manager: Received resume request for job {}.", job_id);
        let jobs = self.jobs.lock().await;
        if let Some(job_arc) = jobs.get(&job_id) {
            let mut job = job_arc.lock().await;
            if matches!(job.status, JobStatus::Paused) {
                job.status = JobStatus::Queued;
                // Save the new state to the database.
                self.state_manager.save_job(&job).await?;
                println!("Manager: Job {} set to Queued. It will be picked up by a worker shortly.", job_id);
                Ok(())
            } else {
                // Optionally, handle cases where the job is in another state.
                // For now, we only act on 'Paused'.
                println!("Manager: Job {} is not in a Paused state, resume request ignored.", job_id);
                Ok(())
            }
        } else {
            Err(ManagerError::JobNotFound(job_id))
        }
    }

    /// **NEW PUBLIC METHOD**
    /// Cancels a download, stops the worker, and optionally deletes the file.
    pub async fn cancel_download(&self, job_id: u64, delete_file: bool) -> Result<(), ManagerError> {
        println!("Manager: Received cancel request for job {}.", job_id);
        
        // Step 1: Stop the running worker, if any.
        if let Some(handle) = self.active_workers.lock().await.remove(&job_id) {
            handle.abort();
            println!("Manager: Aborted worker for job {}.", job_id);
        }
        self.pause_flags.lock().await.remove(&job_id);

        // Step 2: Remove the job from the in-memory map.
        let job_to_delete = self.jobs.lock().await.remove(&job_id);

        if let Some(job_arc) = job_to_delete {
            // Step 3: Delete the job from the database.
            self.state_manager.delete_job(job_id).await?;
            println!("Manager: Removed job {} from the database.", job_id);

            // Step 4: Optionally delete the downloaded file(s).
            if delete_file {
                let job = job_arc.lock().await;
                let temp_path = job.temporary_path();
                let final_path = &job.destination;

                if let Err(e) = tokio::fs::remove_file(&temp_path).await {
                    // It's okay if the file doesn't exist.
                    if e.kind() != std::io::ErrorKind::NotFound {
                        eprintln!("Could not delete temporary file {:?}: {}", temp_path, e);
                    }
                }
                if let Err(e) = tokio::fs::remove_file(&final_path).await {
                     if e.kind() != std::io::ErrorKind::NotFound {
                        eprintln!("Could not delete final file {:?}: {}", final_path, e);
                    }
                }
                println!("Manager: Cleaned up files for job {}.", job_id);
            }
            Ok(())
        } else {
            Err(ManagerError::JobNotFound(job_id))
        }
    }


    /// (Helper for UI/Testing) Returns a clone of all current jobs.
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