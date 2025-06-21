// src/main.rs

use ODM_::prelude::*;
use anyhow::{bail, Result}; // Import `bail` and `Result` from anyhow
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

// A small, real file for testing downloads.
const TEST_URL: &str = "http://212.183.159.230/200MB.zip";

#[tokio::main]
async fn main() -> Result<()> { // Use anyhow's Result
    println!("--- ODM Phase 2 Test: Download Lifecycle Management ---");

    let db_path = PathBuf::from("downloads.db");
    let download_dest = PathBuf::from("200MB.zip");

    // --- Cleanup from previous runs ---
    if db_path.exists() {
        let _ = tokio::fs::remove_file(&db_path).await;
    }
    if download_dest.exists() {
        let _ = tokio::fs::remove_file(&download_dest).await;
    }
    let temp_path = format!("{}.odm-part", download_dest.display());
    if PathBuf::from(&temp_path).exists() {
        let _ = tokio::fs::remove_file(&temp_path).await;
    }

    // --- Test Scope ---
    {
        let state_manager = StateManager::new(&db_path).await?;
        let manager = Arc::new(DownloadManager::new(state_manager, 1).await?);

        let manager_clone = manager.clone();
        let manager_handle = tokio::spawn(async move {
            manager_clone.run().await;
        });

        // 1. ADD a new download.
        println!("\n[ACTION] Adding a new download...");
        let job_id = manager.add_new_job(TEST_URL.to_string(), download_dest.clone(), 8).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        
        let jobs = manager.get_all_jobs().await;
        let job_status = jobs.first().unwrap().status.clone();
        println!("[VERIFY] Job status is: {:?}", job_status);

        // --- THE FIX: Return a descriptive error instead of panicking ---
        if !matches!(job_status, JobStatus::Downloading | JobStatus::Completed) {
            bail!(
                "Job should be Downloading or already Completed, but was {:?}",
                job_status
            );
        }

        // 2. PAUSE the download.
        println!("\n[ACTION] Pausing the download...");
        manager.pause_download(job_id).await?;
        tokio::time::sleep(Duration::from_secs(2)).await;
        
        let jobs = manager.get_all_jobs().await;
        let job = jobs.first().unwrap();
        let job_status = job.status.clone();
        let progress = job.progress();
        println!("[VERIFY] Job status is: {:?}, Progress: {:.2}%", job_status, progress * 100.0);
        
        if !matches!(job_status, JobStatus::Paused) {
            bail!("Job should be Paused, but was {:?}", job_status);
        }
        if !(progress > 0.0 && progress < 1.0) {
            bail!("Progress should be partial (between 0 and 1), but was {}", progress);
        }

        // 3. RESUME the download.
        println!("\n[ACTION] Resuming the download...");
        manager.resume_download(job_id).await?;
        
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(job) = manager.get_all_jobs().await.first() {
                 if matches!(job.status, JobStatus::Completed) {
                    break;
                }
            }
        }
        let jobs = manager.get_all_jobs().await;
        let job_status = jobs.first().unwrap().status.clone();
        println!("[VERIFY] Job status is: {:?}", job_status);
        if !matches!(job_status, JobStatus::Completed) {
            bail!("Job should have completed, but status is {:?}", job_status);
        }
        if !download_dest.exists() {
             bail!("Final download file was not created.");
        }

    // 4. CANCEL a new download (and delete its files)
    println!("\n[ACTION] Adding another download to cancel it...");
    let job_to_cancel_dest = PathBuf::from("cancel_me.dat");
    let job_id_to_cancel = manager.add_new_job(TEST_URL.to_string(), job_to_cancel_dest.clone(), 4).await?;
    
    let temp_cancel_path = job_to_cancel_dest.with_extension("dat.odm-part");

    // --- THE FIX: Poll for the temp file's existence ---
    let mut temp_file_created = false;
    for _ in 0..5 { // Poll for up to 5 seconds
        if temp_cancel_path.exists() {
            temp_file_created = true;
            break;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }

    if !temp_file_created {
        bail!("Temp file {:?} for cancelable job was not created in time.", temp_cancel_path);
    }
    println!("[VERIFY] Temp file for cancelable job exists.");
    
    println!("\n[ACTION] Canceling the download and deleting files...");
    manager.cancel_download(job_id_to_cancel, true).await?;
    
    let jobs = manager.get_all_jobs().await;
    println!("[VERIFY] Total jobs now: {}", jobs.len());
    if jobs.len() != 1 {
        bail!("Expected 1 job after cancel, but found {}.", jobs.len());
    }
    if temp_cancel_path.exists() {
        bail!("Temp file should have been deleted after cancel, but still exists.");
    }

        // --- Shutdown ---
        manager_handle.abort();
        let _ = manager_handle.await;

    } // Manager is dropped here

    // --- Final Cleanup ---
    if download_dest.exists() {
        tokio::fs::remove_file(&download_dest).await?;
    }
    tokio::fs::remove_file(&db_path).await?;
    println!("\n--- Test complete and all resources cleaned up. ---");

    Ok(())
}