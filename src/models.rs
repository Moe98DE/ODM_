// src/models.rs

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf}; // Add Path to imports

/// Represents the persistent state of a single download segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start_byte: u64,
    pub end_byte: u64,
    /// The current byte position of this segment's download.
    /// Initially equals `start_byte`.
    pub current_pos: u64,
}

impl Segment {
    /// Returns the number of bytes remaining to be downloaded for this segment.
    pub fn remaining_bytes(&self) -> u64 {
        self.end_byte.saturating_sub(self.current_pos)
    }

    /// Checks if the segment download is complete.
    pub fn is_complete(&self) -> bool {
        self.current_pos >= self.end_byte
    }
}

/// The status of a download job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)] // <-- REMOVED `Copy`
pub enum JobStatus {
    Queued,
    Downloading,
    Paused,
    Completed,
    Failed(Option<String>), // Storing a reason for failure
}

/// Represents the complete, persistent state of a single download job.
/// This struct is designed to be serialized to a database or file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadJob {
    /// Unique identifier for the job.
    pub id: u64,
    pub url: String,
    pub destination: PathBuf,
    pub status: JobStatus,
    pub total_size: u64,
    pub downloaded_bytes: u64,
    
    /// The state of each individual download chunk.
    pub segments: Vec<Segment>,
    
    // Configuration for the job
    pub num_threads: usize,
    pub retries: u32,
    pub current_retries: u32,
    pub sha256_checksum: Option<String>,
}

impl DownloadJob {
    pub fn new(id: u64, url: String, destination: PathBuf, num_threads: usize) -> Self {
        Self {
            id,
            url,
            destination,
            status: JobStatus::Queued,
            total_size: 0,
            downloaded_bytes: 0,
            segments: Vec::new(),
            num_threads,
            retries: 3, // Default retries
            current_retries: 0,
            sha256_checksum: None,
        }
    }

    /// Calculates download progress as a fraction from 0.0 to 1.0.
    pub fn progress(&self) -> f32 {
        if self.total_size == 0 {
            0.0
        } else {
            self.downloaded_bytes as f32 / self.total_size as f32
        }
    }

    /// **NEW METHOD**
    /// Returns the path for the temporary download file.
    /// e.g., for "/path/to/file.zip", it returns "/path/to/file.zip.odm-part"
    pub fn temporary_path(&self) -> PathBuf {
        let destination_str = self.destination.to_string_lossy();
        PathBuf::from(format!("{}.odm-part", destination_str))
    }
}