// src/main.rs

use ODM_::prelude::*;
use anyhow::{bail, Result};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant}; // Add Instant

// Use a larger file to make the speed limit test meaningful.
const TEST_URL: &str = "http://212.183.159.230/50MB.zip";
const FILE_SIZE: u64 = 52428800; // 50 * 1024 * 1024

#[tokio::main]
async fn main() -> Result<()> {
    println!("--- ODM Phase 3 Test: Speed Limiter ---");

    let db_path = PathBuf::from("downloads.db");
    let download_dest = PathBuf::from("50MB.zip");

    // --- Cleanup ---
    let _ = tokio::fs::remove_file(&db_path).await;
    let _ = tokio::fs::remove_file(&download_dest).await;
    let temp_path = download_dest.with_extension("zip.odm-part");
    let _ = tokio::fs::remove_file(&temp_path).await;

    // --- Test Scope ---
    {
        let state_manager = StateManager::new(&db_path).await?;
        let manager = Arc::new(DownloadManager::new(state_manager, 1).await?);
        
        // Set a speed limit of 10 MB/s.
        let limit_mbps = 10.0;
        let limit_bytes_per_sec = (limit_mbps * 1024.0 * 1024.0) as u64;
        manager.set_speed_limit(limit_bytes_per_sec).await;

        let manager_clone = manager.clone();
        let manager_handle = tokio::spawn(async move { manager_clone.run().await });

        // Add the download and start a timer.
        println!("\n[ACTION] Starting download with a {} MB/s limit...", limit_mbps);
        let start_time = Instant::now();
        let job_id = manager.add_new_job(TEST_URL.to_string(), download_dest.clone(), 8).await?;

        // Wait for it to complete.
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            if let Some(job) = manager.get_all_jobs().await.first() {
                if job.id == job_id {
                    println!("[PROGRESS] {:.2}%", job.progress() * 100.0);
                    if matches!(job.status, JobStatus::Completed) {
                        break;
                    }
                    if matches!(job.status, JobStatus::Failed(_)) {
                         bail!("Download failed unexpectedly: {:?}", job.status);
                    }
                }
            } else {
                 bail!("Job disappeared from manager.");
            }
        }
        
        let elapsed = start_time.elapsed();
        let actual_speed_mbps = (FILE_SIZE as f64 / (1024.0 * 1024.0)) / elapsed.as_secs_f64();
        
        println!("\n[VERIFY] Download completed in {:.2} seconds.", elapsed.as_secs_f32());
        println!("[VERIFY] Actual average speed: {:.2} MB/s", actual_speed_mbps);

        let expected_time_secs = FILE_SIZE as f64 / limit_bytes_per_sec as f64;
        println!("[INFO] Theoretical minimum time: {:.2} seconds.", expected_time_secs);

        // Assert that the elapsed time is reasonably close to the expected time.
        // It should be *at least* the theoretical minimum.
        if elapsed.as_secs_f64() < expected_time_secs * 0.9 {
            bail!("Download finished too fast! Limiter may not be working.");
        }
        // And it shouldn't take excessively long (e.g., more than twice the expected time).
        if elapsed.as_secs_f64() > expected_time_secs * 2.0 {
            bail!("Download took too long! Limiter may be too aggressive or network is slow.");
        }
        
        println!("\n✅ Test successful: Speed limiter worked as expected.");

        // --- THE FIX: Explicitly shut down the manager task ---
        println!("\n--- Shutting down manager ---");
        manager_handle.abort();
        let _ = manager_handle.await;
        println!("Manager task stopped.");
    }

    // --- Final Cleanup ---
    if download_dest.exists() {
        tokio::fs::remove_file(&download_dest).await?;
    }
    tokio::fs::remove_file(&db_path).await?;
    println!("\n--- Test complete and all resources cleaned up. ---");

    Ok(())
}