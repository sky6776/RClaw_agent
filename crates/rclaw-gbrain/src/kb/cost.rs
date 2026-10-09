//! 成本和队列治理 (P5-022~P5-026)
//!
//! Daily token budget tracking, API QPS rate limiter, queue pause/resume.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// Daily token budget tracker
pub struct TokenBudget {
    daily_limit: u64,
    used_today: AtomicU64,
    last_reset_day: AtomicU64, // unix day (days since epoch)
    reset_in_progress: AtomicBool,
}

impl TokenBudget {
    pub fn new(daily_limit: u64) -> Self {
        Self {
            daily_limit,
            used_today: AtomicU64::new(0),
            last_reset_day: AtomicU64::new(current_day()),
            reset_in_progress: AtomicBool::new(false),
        }
    }

    /// 尝试消费 tokens，返回是否允许
    /// FIX11-08: 使用 compare_exchange 循环替代 load+check+fetch_add，
    /// 避免两个线程同时通过检查后都消费导致预算超支（TOCTOU 竞态）
    pub fn try_consume(&self, tokens: u64) -> bool {
        self.check_reset();
        let mut current = self.used_today.load(Ordering::Acquire);
        loop {
            if current + tokens > self.daily_limit {
                return false;
            }
            let new_val = current + tokens;
            match self.used_today.compare_exchange(
                current,
                new_val,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual, // 其他线程已修改，重试
            }
        }
    }

    fn check_reset(&self) {
        let today = current_day();
        if self.last_reset_day.load(Ordering::Acquire) == today {
            return;
        }

        if self
            .reset_in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.used_today.store(0, Ordering::Release);
            self.last_reset_day.store(today, Ordering::Release);
            self.reset_in_progress.store(false, Ordering::Release);
        } else {
            while self.reset_in_progress.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
        }
    }

    pub fn used_today(&self) -> u64 {
        self.used_today.load(Ordering::Relaxed)
    }

    pub fn remaining(&self) -> u64 {
        self.daily_limit.saturating_sub(self.used_today())
    }
}

fn current_day() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86400)
        .unwrap_or(0)
}

/// 简单令牌桶限流器
pub struct RateLimiter {
    rate_per_sec: f64,
    last_check: Instant,
    tokens: f64,
}

impl RateLimiter {
    pub fn new(qps: f64) -> Self {
        Self {
            rate_per_sec: qps,
            last_check: Instant::now(),
            tokens: qps,
        }
    }

    /// 尝试获取一个令牌，成功返回 true
    pub fn try_acquire(&mut self) -> bool {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_check).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.rate_per_sec);
        self.last_check = now;

        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// 队列暂停/恢复标志
#[derive(Default)]
pub struct QueueControl {
    paused: AtomicBool,
}

impl QueueControl {
    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_token_budget_consume() {
        let budget = TokenBudget::new(100);
        assert!(budget.try_consume(30));
        assert!(budget.try_consume(50));
        assert!(!budget.try_consume(50));
    }

    #[test]
    fn test_rate_limiter() {
        let mut rl = RateLimiter::new(10.0);
        let mut acquired = 0;
        for _ in 0..10 {
            if rl.try_acquire() {
                acquired += 1;
            }
        }
        assert!(acquired > 0 && acquired <= 10);
    }

    #[test]
    fn test_queue_control() {
        let qc = QueueControl::default();
        assert!(!qc.is_paused());
        qc.pause();
        assert!(qc.is_paused());
        qc.resume();
        assert!(!qc.is_paused());
    }
}
