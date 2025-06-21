pub mod downloader;
pub mod queue;
pub mod integrity;

use downloader::{DownloadOptions, Downloader};
use queue::DownloadQueue;

/// Convenient type alias exposing common structs.
pub mod prelude {
    pub use crate::downloader::{DownloadOptions, Downloader, DownloadStatus};
    pub use crate::queue::{DownloadQueue, DownloadTask};
}

