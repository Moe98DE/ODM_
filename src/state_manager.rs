// src/state_manager.rs

use crate::models::DownloadJob;
use rusqlite::params;
use std::path::Path;
use thiserror::Error;
use tokio_rusqlite::Connection;

#[derive(Debug, Error)]
pub enum StateError {
    #[error("database error: {0}")]
    Database(#[from] tokio_rusqlite::Error),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("database query failed: {0}")]
    Query(#[from] rusqlite::Error),
}

/// Manages the persistence of download jobs to an SQLite database.
pub struct StateManager {
    conn: Connection,
}

impl StateManager {
    /// Creates a new StateManager and connects to the database file.
    /// It will create the database and necessary tables if they don't exist.
    pub async fn new(db_path: &Path) -> Result<Self, StateError> {
        let conn = Connection::open(db_path).await?;
        let manager = Self { conn };
        manager.setup_database().await?;
        Ok(manager)
    }

    /// Creates the 'downloads' table if it doesn't already exist.
    async fn setup_database(&self) -> Result<(), StateError> {
        self.conn
            .call(|conn| {
                conn.execute(
                    "CREATE TABLE IF NOT EXISTS downloads (
                        id              INTEGER PRIMARY KEY,
                        job_data        TEXT NOT NULL
                    )",
                    [],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Saves (inserts or updates) a DownloadJob to the database.
    pub async fn save_job(&self, job: &DownloadJob) -> Result<(), StateError> {
        let job_data = serde_json::to_string(job)?;
        let job_id = job.id; // <-- FIX: Extract the ID here. u64 is Copy.

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO downloads (id, job_data) VALUES (?1, ?2)",
                    // Use the owned variables, not the reference.
                    params![job_id, job_data],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    
    /// Loads all download jobs from the database.
    pub async fn load_all_jobs(&self) -> Result<Vec<DownloadJob>, StateError> {
        let jobs = self.conn
            .call(|conn| {
                let mut stmt = conn.prepare("SELECT job_data FROM downloads")?;
                let job_iter = stmt.query_map([], |row| {
                    let job_data: String = row.get(0)?;
                    // Deserialize the JSON string back into a DownloadJob
                    let job: DownloadJob = serde_json::from_str(&job_data)
                        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
                    Ok(job)
                })?;

                // Collect the results, handling potential errors during iteration
                let jobs: Result<Vec<DownloadJob>, rusqlite::Error> = job_iter.collect();
                jobs
            })
            .await?;
        Ok(jobs)
    }

    /// Deletes a job from the database by its ID.
    pub async fn delete_job(&self, job_id: u64) -> Result<(), StateError> {
        self.conn
            .call(move |conn| {
                conn.execute("DELETE FROM downloads WHERE id = ?1", params![job_id])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}