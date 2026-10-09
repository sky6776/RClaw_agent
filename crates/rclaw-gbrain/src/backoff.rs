//! Generic exponential backoff with jitter.
//! Mirrors gbrain's src/core/backoff.ts
//!
//! Eliminates duplicated retry logic in embedding.rs, expansion.rs, transcription.rs.

use std::future::Future;
use std::time::Duration;

/// Configuration for exponential backoff
#[derive(Debug, Clone)]
pub struct BackoffOpts {
    /// Maximum number of retries (default 5)
    pub max_retries: u32,
    /// Base delay in milliseconds (default 500)
    pub base_ms: u64,
    /// Maximum delay in milliseconds (default 120000 = 2 min)
    pub max_ms: u64,
    /// Apply random jitter to delay
    pub jitter: bool,
}

impl Default for BackoffOpts {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_ms: 500,
            max_ms: 120000,
            jitter: true,
        }
    }
}

/// Execute an async function with exponential backoff on error.
///
/// The function `f` is called at most `opts.max_retries + 1` times (initial + retries).
/// Between retries, sleeps for `base_ms * 2^(attempt-1)` ms, capped at `max_ms`.
pub async fn with_backoff<F, Fut, T, E>(mut f: F, opts: BackoffOpts) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut attempt = 0u32;

    loop {
        attempt += 1;
        match f().await {
            Ok(val) => return Ok(val),
            Err(e) => {
                if attempt > opts.max_retries {
                    return Err(e);
                }
                let delay_ms =
                    (opts.base_ms * 2u64.pow(attempt.saturating_sub(1))).min(opts.max_ms);
                let delay_ms = if opts.jitter {
                    // Add up to 25% random jitter
                    let jitter = (delay_ms as f64 * 0.25 * rand_factor()) as u64;
                    (delay_ms + jitter).min(opts.max_ms)
                } else {
                    delay_ms
                };
                tracing::warn!(
                    attempt,
                    max_retries = opts.max_retries,
                    delay_ms,
                    error = %e,
                    "Operation failed, retrying with backoff"
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }
}

/// Blocking version for sync contexts
///
/// M35 注意：此函数使用 `std::thread::sleep` 阻塞当前 OS 线程。
/// 仅允许从专用阻塞线程（如 `std::thread::spawn`）或非异步上下文调用。
/// 在 tokio 异步运行时内应使用 `with_backoff` 异步版本，避免阻塞 tokio worker 线程。
pub fn with_backoff_sync<F, T, E>(mut f: F, opts: BackoffOpts) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
    E: std::fmt::Display,
{
    let mut attempt = 0u32;
    let start = std::time::Instant::now();

    loop {
        attempt += 1;
        match f() {
            Ok(val) => {
                tracing::debug!(
                    attempt,
                    elapsed_ms = start.elapsed().as_millis() as u64,
                    "with_backoff_sync succeeded"
                );
                return Ok(val);
            }
            Err(e) => {
                if attempt > opts.max_retries {
                    return Err(e);
                }
                let delay_ms =
                    (opts.base_ms * 2u64.pow(attempt.saturating_sub(1))).min(opts.max_ms);
                let delay_ms = if opts.jitter {
                    let jitter = (delay_ms as f64 * 0.25 * rand_factor()) as u64;
                    (delay_ms + jitter).min(opts.max_ms)
                } else {
                    delay_ms
                };
                tracing::warn!(
                    attempt,
                    max_retries = opts.max_retries,
                    delay_ms,
                    error = %e,
                    "Operation failed (sync), retrying with backoff"
                );
                // M35: 此处使用阻塞 sleep 会占用 OS 线程。
                // 在 sync 场景下可接受（单线程批处理），但 tokio 异步上下文中应改用 tokio::time::sleep。
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
        }
    }
}

/// Simple random factor 0.0..1.0 (no crypto needed for jitter)
fn rand_factor() -> f64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let val = RandomState::new().build_hasher().finish();
    (val as f64) / (u64::MAX as f64)
}

/// Calculate backoff delay in milliseconds for a given attempt number.
/// Uses exponential backoff: `base_ms * 2^(attempt-1)`, capped at `max_ms`.
/// Optionally adds up to 25% random jitter.
///
/// Useful for HTTP-level retry logic that needs to combine this delay with
/// server-provided Retry-After headers.
pub fn backoff_delay_ms(attempt: u32, opts: &BackoffOpts) -> u64 {
    let delay_ms = (opts.base_ms * 2u64.pow(attempt.saturating_sub(1))).min(opts.max_ms);
    if opts.jitter {
        let jitter = (delay_ms as f64 * 0.25 * rand_factor()) as u64;
        (delay_ms + jitter).min(opts.max_ms)
    } else {
        delay_ms
    }
}
