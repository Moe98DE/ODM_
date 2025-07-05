// src/lib.rs

pub mod downloader;
pub mod integrity;
pub mod manager; // New module
pub mod limiter; // Add new module
pub mod models;
pub mod state_manager;

/// Convenient type alias exposing common structs.
pub mod prelude {
    pub use crate::downloader::DownloadWorker;
    pub use crate::integrity::sha256_sum;
    pub use crate::limiter::SpeedLimiter; // Expose SpeedLimiter
    pub use crate::manager::{DownloadManager, ManagerError}; // Expose Manager
    pub use crate::models::{DownloadJob, JobStatus, Segment};
    pub use crate::state_manager::{StateManager, StateError};
}