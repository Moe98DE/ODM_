// src/limiter.rs

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{Duration, Instant};

/// A token bucket for rate limiting, shared across multiple tasks.
#[derive(Clone)]
pub struct SpeedLimiter {
    state: Arc<Mutex<TokenBucket>>,
}

/// The internal state of the token bucket.
struct TokenBucket {
    /// The maximum number of tokens (bytes) the bucket can hold. This controls burstiness.
    capacity: u64,
    /// The current number of tokens (bytes) in the bucket.
    tokens: u64,
    /// The rate at which tokens are added to the bucket, in tokens per second.
    rate: u64,
    /// The last time the bucket was refilled with tokens.
    last_refill: Instant,
}

impl SpeedLimiter {
    /// Creates a new speed limiter.
    /// A rate of 0 means the limiter is disabled (unlimited speed).
    pub fn new(rate_bytes_per_sec: u64) -> Self {
        // We'll set the capacity (burst size) to be equal to the rate.
        let capacity = if rate_bytes_per_sec == 0 {
            u64::MAX // Effectively infinite capacity if unlimited
        } else {
            rate_bytes_per_sec
        };

        Self {
            state: Arc::new(Mutex::new(TokenBucket {
                capacity,
                tokens: capacity, // Start with a full bucket
                rate: rate_bytes_per_sec,
                last_refill: Instant::now(),
            })),
        }
    }

    /// Sets a new speed limit. A rate of 0 means unlimited.
    pub async fn set_rate(&self, rate_bytes_per_sec: u64) {
        let mut bucket = self.state.lock().await;
        bucket.rate = rate_bytes_per_sec;
        bucket.capacity = if rate_bytes_per_sec == 0 {
            u64::MAX
        } else {
            rate_bytes_per_sec
        };
        // Reset tokens to new capacity to avoid unexpected behavior
        bucket.tokens = bucket.capacity;
    }

    /// Asynchronously takes `amount` tokens from the bucket, waiting if necessary.
    pub async fn take(&self, amount: u64) {
        if amount == 0 {
            return;
        }

        loop {
            let mut bucket = self.state.lock().await;

            // Refill the bucket with tokens that have accumulated since the last check.
            bucket.refill();

            if bucket.tokens >= amount {
                // We have enough tokens, take them and proceed.
                bucket.tokens -= amount;
                return;
            }

            // Not enough tokens. Calculate how long we need to wait.
            let tokens_needed = amount - bucket.tokens;
            // Avoid division by zero if rate is 0 (unlimited). Should not happen here.
            let wait_time = if bucket.rate > 0 {
                Duration::from_secs_f64(tokens_needed as f64 / bucket.rate as f64)
            } else {
                Duration::from_secs(0)
            };

            // It's crucial to drop the lock *before* sleeping.
            drop(bucket);
            tokio::time::sleep(wait_time).await;
        }
    }
}

impl TokenBucket {
    /// Adds tokens to the bucket based on elapsed time.
    fn refill(&mut self) {
        if self.rate == 0 {
            // Unlimited rate, so bucket is always "full".
            self.tokens = self.capacity;
            return;
        }
        
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        let tokens_to_add = (elapsed.as_secs_f64() * self.rate as f64) as u64;

        if tokens_to_add > 0 {
            self.tokens = (self.tokens + tokens_to_add).min(self.capacity);
            self.last_refill = now;
        }
    }
}