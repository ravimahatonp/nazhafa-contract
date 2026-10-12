//! Exponential backoff retry utility for network, Horizon, and Soroban RPC calls.

use std::future::Future;
use std::time::Duration;
use tracing::warn;

/// Trait to determine if an error is transient and can be retried.
pub trait IsRetryable {
    /// Returns true if the error is transient and should be retried.
    fn is_retryable(&self) -> bool;
}

/// Generic retry function with exponential backoff.
///
/// Calls `f()` up to `max_retries` times. If `f()` returns an error:
/// - If `is_retryable()` is false, returns immediately with the error (non-retryable).
/// - If `is_retryable()` is true and attempts remain, logs a warning and sleeps for
///   `base_delay * 2^(attempt - 1)` before retrying.
/// - If `max_retries` is exhausted, returns the last error.
pub async fn retry_with_backoff<F, Fut, T, E>(
    mut f: F,
    max_retries: u32,
    base_delay: Duration,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: IsRetryable + std::fmt::Display,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f().await {
            Ok(val) => return Ok(val),
            Err(err) => {
                if !err.is_retryable() || attempt > max_retries {
                    return Err(err);
                }
                let delay = base_delay * (1 << (attempt - 1).min(6));
                warn!(
                    attempt = attempt,
                    max_retries = max_retries,
                    delay_ms = delay.as_millis(),
                    error = %err,
                    "Transient error encountered, retrying operation after backoff"
                );
                tokio::time::sleep(delay).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;
    use thiserror::Error;

    #[derive(Debug, Error, PartialEq, Eq)]
    enum TestError {
        #[error("Transient network timeout")]
        TransientTimeout,
        #[error("Fatal invalid signature")]
        FatalSignature,
    }

    impl IsRetryable for TestError {
        fn is_retryable(&self) -> bool {
            match self {
                TestError::TransientTimeout => true,
                TestError::FatalSignature => false,
            }
        }
    }

    #[tokio::test]
    async fn test_retry_succeeds_on_transient_failure() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result = retry_with_backoff(
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    let count = attempts.fetch_add(1, Ordering::SeqCst);
                    if count < 2 {
                        Err(TestError::TransientTimeout)
                    } else {
                        Ok("success")
                    }
                }
            },
            3,
            Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, Ok("success"));
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_non_retryable_fails_immediately() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<&str, TestError> = retry_with_backoff(
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(TestError::FatalSignature)
                }
            },
            5,
            Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, Err(TestError::FatalSignature));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_retry_exhaustion_returns_last_error() {
        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();

        let result: Result<&str, TestError> = retry_with_backoff(
            move || {
                let attempts = attempts_clone.clone();
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    Err(TestError::TransientTimeout)
                }
            },
            2,
            Duration::from_millis(1),
        )
        .await;

        assert_eq!(result, Err(TestError::TransientTimeout));
        // Initial attempt (1) + 2 retries = 3 calls total
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }
}
