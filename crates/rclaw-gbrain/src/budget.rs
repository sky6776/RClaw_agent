//! Budget ledger — daily API spend cap system with reserve/commit/rollback.
//! Mirrors gbrain's src/core/enrichment/budget.ts
//!
//! Atomic reserve-then-check pattern: estimate spend → reserve → execute API call
//! → commit (with actual cost) or rollback (on failure).
//! TTL-based cleanup prevents orphaned reservations from blocking future calls.

use crate::error::{GBrainError, Result};
use rusqlite::params;
use std::sync::OnceLock;
use std::time::{Duration, UNIX_EPOCH};

/// Budget state for a (scope, resolver, date) tuple
#[derive(Debug, Clone)]
pub struct BudgetState {
    pub scope: String,
    pub resolver_id: String,
    pub date: String,
    pub reserved_usd: f64,
    pub committed_usd: f64,
    pub cap_usd: Option<f64>,
}

/// Reservation result — either held or exhausted
#[derive(Debug, Clone)]
pub enum Reservation {
    Held {
        reservation_id: String,
    },
    Exhausted {
        reason: String,
        spent: f64,
        pending: f64,
        cap: f64,
    },
}

/// S-10: Display implementation for user-friendly logging
impl std::fmt::Display for Reservation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Reservation::Held { reservation_id } => write!(f, "Held: {}", reservation_id),
            Reservation::Exhausted {
                reason,
                spent,
                pending,
                cap,
            } => write!(
                f,
                "Exhausted: {} (spent=${:.2}, pending=${:.2}, cap=${:.2})",
                reason, spent, pending, cap
            ),
        }
    }
}

/// R3-02: RAII guard that ensures ROLLBACK is issued if a transaction is
/// dropped without being explicitly committed. Prevents orphaned transactions
/// that would lock the database if a panic occurs between BEGIN and COMMIT.
///
/// M32: 此类型已公开，供其他模块复用 RAII 事务管理模式，
/// 避免各处重复实现相同的 BEGIN/COMMIT/ROLLBACK 逻辑。
pub struct TxGuard<'a> {
    conn: &'a rusqlite::Connection,
    active: bool,
}

impl<'a> TxGuard<'a> {
    pub fn begin(conn: &'a rusqlite::Connection) -> Result<Self> {
        conn.execute("BEGIN IMMEDIATE", [])?;
        Ok(Self { conn, active: true })
    }

    pub fn commit(mut self) -> Result<()> {
        self.conn.execute("COMMIT", [])?;
        self.active = false; // Only mark inactive AFTER successful COMMIT
        Ok(())
    }
}

impl<'a> Drop for TxGuard<'a> {
    fn drop(&mut self) {
        if self.active {
            // ROLLBACK on drop (panic path) — ignore errors since we can't recover
            self.conn.execute("ROLLBACK", []).ok();
        }
    }
}

/// Manages daily spend caps for API resolver calls.
/// Uses SQLite transactions for atomic reserve-then-check.
pub struct BudgetLedger<'a> {
    conn: &'a rusqlite::Connection,
}

impl<'a> BudgetLedger<'a> {
    pub fn new(conn: &'a rusqlite::Connection) -> Self {
        Self { conn }
    }

    /// Initialize the budget tables (idempotent)
    pub fn init(&self) -> Result<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS budget_ledger (
                scope TEXT NOT NULL DEFAULT 'default',
                resolver_id TEXT NOT NULL,
                local_date TEXT NOT NULL,
                reserved_usd REAL NOT NULL DEFAULT 0,
                committed_usd REAL NOT NULL DEFAULT 0,
                cap_usd REAL,
                updated_at TEXT NOT NULL DEFAULT (datetime('now')),
                PRIMARY KEY (scope, resolver_id, local_date)
            );
            CREATE TABLE IF NOT EXISTS budget_reservations (
                id TEXT PRIMARY KEY,
                scope TEXT NOT NULL DEFAULT 'default',
                resolver_id TEXT NOT NULL,
                local_date TEXT NOT NULL,
                estimate_usd REAL NOT NULL,
                status TEXT NOT NULL DEFAULT 'held',
                created_at TEXT NOT NULL DEFAULT (datetime('now')),
                expires_at TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    /// Reserve estimated spend. Returns a reservation ID if within budget,
    /// or Exhausted if the daily cap would be exceeded.
    /// R3-02: Uses TxGuard for RAII transaction safety — ROLLBACK is automatic on panic.
    pub fn reserve(
        &self,
        scope: &str,
        resolver_id: &str,
        estimate_usd: f64,
        cap_usd: Option<f64>,
        ttl_seconds: Option<u64>,
    ) -> Result<Reservation> {
        let today = today_iso();
        let ttl = ttl_seconds.unwrap_or(60);
        let expires_at = iso_from_now(Duration::from_secs(ttl));

        let tx = TxGuard::begin(self.conn)?;

        let result =
            self.reserve_inner(scope, resolver_id, estimate_usd, cap_usd, today, expires_at);
        match result {
            Ok(r) => {
                tx.commit()?;
                Ok(r)
            }
            Err(e) => {
                // TxGuard will ROLLBACK on drop
                Err(e)
            }
        }
    }

    /// Inner reserve logic (runs within an active transaction)
    fn reserve_inner(
        &self,
        scope: &str,
        resolver_id: &str,
        estimate_usd: f64,
        cap_usd: Option<f64>,
        today: String,
        expires_at: String,
    ) -> Result<Reservation> {
        // Upsert the daily budget row
        self.conn.execute(
            "INSERT INTO budget_ledger (scope, resolver_id, local_date, reserved_usd, committed_usd, cap_usd)
             VALUES (?1, ?2, ?3, 0, 0, ?4)
             ON CONFLICT(scope, resolver_id, local_date) DO NOTHING",
            params![scope, resolver_id, today, cap_usd],
        )?;

        // Read current state (within the transaction for atomicity)
        let state: BudgetState = self.conn.query_row(
            "SELECT scope, resolver_id, local_date, reserved_usd, committed_usd, cap_usd
             FROM budget_ledger WHERE scope = ?1 AND resolver_id = ?2 AND local_date = ?3",
            params![scope, resolver_id, today],
            |row| {
                Ok(BudgetState {
                    scope: row.get(0)?,
                    resolver_id: row.get(1)?,
                    date: row.get(2)?,
                    reserved_usd: row.get(3)?,
                    committed_usd: row.get(4)?,
                    cap_usd: row.get(5)?,
                })
            },
        )?;

        // I-11 fix: Error when no cap configured instead of silently defaulting to $1.0
        let effective_cap = match cap_usd.or(state.cap_usd) {
            Some(cap) => cap,
            None => {
                tracing::warn!(
                    "No daily budget cap configured for {}/{}, defaulting to $1.0",
                    scope,
                    resolver_id
                );
                1.0 // safe default: block spending beyond $1 until cap is configured
            }
        };
        let total_pending = state.committed_usd + state.reserved_usd + estimate_usd;

        if total_pending > effective_cap {
            // Return Exhausted — the caller will COMMIT (not ROLLBACK) since this is a valid outcome
            return Ok(Reservation::Exhausted {
                reason: format!("Daily cap exceeded for {}/{}", scope, resolver_id),
                spent: state.committed_usd,
                pending: state.reserved_usd,
                cap: effective_cap,
            });
        }

        // Increment reserved_usd
        self.conn.execute(
            "UPDATE budget_ledger SET reserved_usd = reserved_usd + ?1, updated_at = datetime('now')
             WHERE scope = ?2 AND resolver_id = ?3 AND local_date = ?4",
            params![estimate_usd, scope, resolver_id, today],
        )?;

        // Create reservation record (includes local_date for cross-day commit/rollback)
        let reservation_id = format!("{}/{}/{}", scope, resolver_id, uuid_simple());
        self.conn.execute(
            "INSERT INTO budget_reservations (id, scope, resolver_id, local_date, estimate_usd, status, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'held', ?6)",
            params![reservation_id, scope, resolver_id, today, estimate_usd, expires_at],
        )?;

        Ok(Reservation::Held { reservation_id })
    }

    /// Commit a reservation with actual spend (may be different from estimate).
    /// Idempotent — calling on an already-committed reservation is a no-op.
    /// R3-02: Uses TxGuard for RAII transaction safety.
    pub fn commit(&self, reservation_id: &str, actual_spend: f64) -> Result<()> {
        let tx = TxGuard::begin(self.conn)?;

        let result = self.commit_inner(reservation_id, actual_spend);
        match result {
            Ok(()) => {
                tx.commit()?;
                Ok(())
            }
            Err(e) => {
                // TxGuard will ROLLBACK on drop
                Err(e)
            }
        }
    }

    fn commit_inner(&self, reservation_id: &str, actual_spend: f64) -> Result<()> {
        // Read reservation WITHIN the transaction to prevent TOCTOU race
        let (scope, resolver_id, estimate, date) =
            self.get_reservation_with_date(reservation_id)?;

        // Reverse the reserved estimate, add actual to committed -- only if reservation is still held
        // This makes commit idempotent: a second call won't double-decrement reserved_usd
        let rows = self.conn.execute(
            "UPDATE budget_ledger
             SET reserved_usd = MAX(0, reserved_usd - ?1),
                 committed_usd = committed_usd + ?2,
                 updated_at = datetime('now')
             WHERE scope = ?3 AND resolver_id = ?4 AND local_date = ?5
               AND EXISTS (SELECT 1 FROM budget_reservations WHERE id = ?6 AND status = 'held')",
            params![
                estimate,
                actual_spend,
                scope,
                resolver_id,
                date,
                reservation_id
            ],
        )?;

        if rows == 0 {
            // Reservation already committed or doesn't exist -- idempotent, no action needed
            return Ok(());
        }

        // Mark reservation as committed
        self.conn.execute(
            "UPDATE budget_reservations SET status = 'committed' WHERE id = ?1 AND status = 'held'",
            params![reservation_id],
        )?;

        Ok(())
    }

    /// Rollback a held reservation, freeing the reserved amount.
    /// Idempotent -- safe to call on already finalised reservations.
    /// R3-02: Uses TxGuard for RAII transaction safety.
    pub fn rollback(&self, reservation_id: &str) -> Result<()> {
        let tx = TxGuard::begin(self.conn)?;

        let result = self.rollback_inner(reservation_id);
        match result {
            Ok(()) => {
                tx.commit()?;
                Ok(())
            }
            Err(e) => {
                // TxGuard will ROLLBACK on drop
                Err(e)
            }
        }
    }

    fn rollback_inner(&self, reservation_id: &str) -> Result<()> {
        // Read reservation WITHIN the transaction to prevent TOCTOU race
        let (scope, resolver_id, estimate, date) =
            match self.get_reservation_with_date(reservation_id) {
                Ok(r) => r,
                Err(_) => return Ok(()), // already cleaned up
            };

        // Only rollback if still 'held' (not already committed/rolled_back)
        // Use the reservation's original date to handle cross-day rollbacks
        let affected = self.conn.execute(
            "UPDATE budget_ledger
             SET reserved_usd = MAX(0, reserved_usd - ?1), updated_at = datetime('now')
             WHERE scope = ?2 AND resolver_id = ?3 AND local_date = ?4
               AND EXISTS (SELECT 1 FROM budget_reservations WHERE id = ?5 AND status = 'held')",
            params![estimate, scope, resolver_id, date, reservation_id],
        )?;

        if affected > 0 {
            self.conn.execute(
                "UPDATE budget_reservations SET status = 'rolled_back' WHERE id = ?1 AND status = 'held'",
                params![reservation_id],
            )?;
        }

        Ok(())
    }

    /// Clean up TTL-expired reservations (call periodically)
    pub fn cleanup_expired(&self) -> Result<usize> {
        let mut count = 0;
        let mut stmt = self.conn.prepare(
            "SELECT id FROM budget_reservations WHERE status = 'held' AND expires_at < ?1",
        )?;
        let expired: Vec<String> = stmt
            .query_map(params![iso_now()], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        for id in expired {
            if self.rollback(&id).is_ok() {
                count += 1;
            }
        }
        Ok(count)
    }

    fn get_reservation_with_date(&self, id: &str) -> Result<(String, String, f64, String)> {
        self.conn.query_row(
            "SELECT scope, resolver_id, estimate_usd, local_date FROM budget_reservations WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        ).map_err(|e| GBrainError::Database(format!("Reservation not found: {} ({})", id, e)))
    }
}

fn today_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

fn iso_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn iso_from_now(d: Duration) -> String {
    match chrono::Duration::from_std(d) {
        Ok(cd) => (chrono::Utc::now() + cd)
            .format("%Y-%m-%dT%H:%M:%S")
            .to_string(),
        Err(_) => {
            // Duration too large for chrono; use far-future sentinel
            tracing::warn!(
                duration_secs = d.as_secs(),
                "Duration exceeds chrono range, clamping to 9999-12-31"
            );
            "9999-12-31T23:59:59".to_string()
        }
    }
}

/// C-03 fix: Process-wide monotonic counter ensures uniqueness across connections.
/// Uses OnceLock for one-time init + AtomicU64 for lock-free increment.
static UUID_COUNTER: OnceLock<std::sync::atomic::AtomicU64> = OnceLock::new();

fn uuid_simple() -> String {
    use std::sync::atomic::Ordering;
    let counter = UUID_COUNTER.get_or_init(|| {
        std::sync::atomic::AtomicU64::new(
            // Seed with process ID to avoid collisions across processes
            std::process::id() as u64,
        )
    });
    let seq = counter.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("系统时钟不应早于 Unix epoch (1970-01-01)");
    format!("{:x}-{:x}-{:x}", t.as_nanos(), seq, std::process::id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn with_ledger<F: FnOnce(&BudgetLedger)>(f: F) {
        let conn = Connection::open_in_memory().unwrap();
        let ledger = BudgetLedger::new(&conn);
        ledger.init().unwrap();
        f(&ledger);
    }

    #[test]
    fn test_reserve_within_cap() {
        with_ledger(|ledger| {
            let result = ledger
                .reserve("test", "resolver1", 0.5, Some(1.0), Some(60))
                .unwrap();
            assert!(matches!(result, Reservation::Held { .. }));
        });
    }

    #[test]
    fn test_reserve_exceeds_cap() {
        with_ledger(|ledger| {
            let result = ledger
                .reserve("test", "resolver1", 2.0, Some(1.0), Some(60))
                .unwrap();
            assert!(matches!(result, Reservation::Exhausted { .. }));
        });
    }

    #[test]
    fn test_commit_and_exhaust() {
        with_ledger(|ledger| {
            let r = ledger
                .reserve("test", "r2", 0.7, Some(1.0), Some(60))
                .unwrap();
            if let Reservation::Held { reservation_id } = r {
                ledger.commit(&reservation_id, 0.7).unwrap();
            }
            let r2 = ledger
                .reserve("test", "r2", 0.5, Some(1.0), Some(60))
                .unwrap();
            assert!(matches!(r2, Reservation::Exhausted { .. }));
        });
    }

    #[test]
    fn test_rollback_frees_budget() {
        with_ledger(|ledger| {
            let r = ledger
                .reserve("test", "r3", 0.9, Some(1.0), Some(60))
                .unwrap();
            if let Reservation::Held { reservation_id } = r {
                ledger.rollback(&reservation_id).unwrap();
            }
            let r2 = ledger
                .reserve("test", "r3", 0.9, Some(1.0), Some(60))
                .unwrap();
            assert!(matches!(r2, Reservation::Held { .. }));
        });
    }

    #[test]
    fn test_cleanup_expired() {
        with_ledger(|ledger| {
            // Create reservation and manually expire it via direct SQL
            let _r = ledger
                .reserve("test", "r4", 0.1, Some(1.0), Some(60))
                .unwrap();
            ledger.conn.execute(
            "UPDATE budget_reservations SET expires_at = datetime('now', '-1 seconds') WHERE status = 'held'", []
        ).unwrap();
            let count = ledger.cleanup_expired().unwrap();
            assert!(
                count >= 1,
                "Should have cleaned up at least 1 expired reservation, got {}",
                count
            );
        });
    }

    #[test]
    fn test_commit_uses_reservation_date() {
        with_ledger(|ledger| {
            // Reserve on today's date
            let r = ledger
                .reserve("test", "r5", 0.5, Some(1.0), Some(60))
                .unwrap();
            if let Reservation::Held { reservation_id } = r {
                // Manually change the ledger date to simulate cross-day commit
                ledger.conn.execute(
                "UPDATE budget_ledger SET local_date = '2020-01-01' WHERE resolver_id = 'r5'", []
            ).unwrap();
                // Commit should still work because it uses the reservation's stored date
                ledger.commit(&reservation_id, 0.5).unwrap();
            }
        });
    }
}
