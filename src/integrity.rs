use std::path::Path;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, BufReader};
use sha2::{Sha256, Digest};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IntegrityError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Compute SHA256 hash of a file asynchronously.
pub async fn sha256_sum(path: &Path) -> Result<String, IntegrityError> {
    let file = File::open(path).await?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 1024 * 8];
    loop {
        let n = reader.read(&mut buffer).await?;
        if n == 0 { break; }
        hasher.update(&buffer[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

