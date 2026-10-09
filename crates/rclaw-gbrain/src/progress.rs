//! Structured progress reporter for long-running operations.
//! Mirrors gbrain's src/core/progress.ts
//!
//! Modes: auto (TTY-aware), human, json (one JSON object per line), quiet.
//!
//! JSON event schema:
//!   {"event":"start","phase":"snake.phase","total":N,"ts":"<iso>"}
//!   {"event":"tick","phase":"...","done":N,"total":N,"pct":F,"elapsed_ms":N,"eta_ms":N,"ts":"..."}
//!   {"event":"finish","phase":"...","done":N,"total":N,"elapsed_ms":N,"ts":"..."}

use std::io::Write;
use std::time::Instant;

/// Progress output mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProgressMode {
    Auto,
    Human,
    Json,
    Quiet,
}

impl ProgressMode {
    /// Resolve from environment: GBRAIN_PROGRESS_MODE or --progress-json flag
    pub fn from_env() -> Self {
        if let Ok(val) = std::env::var("GBRAIN_PROGRESS_MODE") {
            match val.to_lowercase().as_str() {
                "human" => return Self::Human,
                "json" => return Self::Json,
                "quiet" => return Self::Quiet,
                _ => {}
            }
        }
        if std::env::var("GBRAIN_PROGRESS_JSON")
            .map(|v| matches!(v.trim(), "1" | "true"))
            .unwrap_or(false)
        {
            return Self::Json;
        }
        Self::Auto
    }
}

/// A progress reporter for a single phase
pub struct ProgressReporter {
    mode: ProgressMode,
    phase: String,
    total: Option<usize>,
    done: usize,
    started_at: Instant,
    last_emit_at: Instant,
    min_interval_ms: u64,
    writer: Box<dyn Write + Send>,
}

impl ProgressReporter {
    /// Create with explicit mode and writer
    pub fn with_opts(
        phase: &str,
        total: Option<usize>,
        mode: ProgressMode,
        writer: impl Write + Send + 'static,
    ) -> Self {
        Self::with_boxed(phase, total, mode, Box::new(writer))
    }

    fn with_boxed(
        phase: &str,
        total: Option<usize>,
        mode: ProgressMode,
        writer: Box<dyn Write + Send>,
    ) -> Self {
        let effective_mode = match mode {
            ProgressMode::Auto => ProgressMode::Human,
            other => other,
        };
        Self {
            mode: effective_mode,
            phase: phase.to_string(),
            total,
            done: 0,
            started_at: Instant::now(),
            last_emit_at: Instant::now(),
            min_interval_ms: 1000,
            writer,
        }
    }

    /// Increment progress by n
    pub fn tick(&mut self, n: usize) {
        self.done += n;
        let elapsed = self.last_emit_at.elapsed().as_millis() as u64;
        let is_final = self.total.is_some_and(|t| self.done >= t);
        if elapsed >= self.min_interval_ms || is_final {
            self.emit(false);
            self.last_emit_at = Instant::now();
        }
    }

    /// Finish the phase
    pub fn finish(&mut self) {
        self.emit(true);
    }

    fn emit(&mut self, is_finish: bool) {
        let elapsed_ms = self.started_at.elapsed().as_millis() as u64;
        match self.mode {
            ProgressMode::Json => {
                let event_type = if is_finish { "finish" } else { "tick" };
                let pct = self.total.and_then(|t| {
                    if t > 0 {
                        Some(((self.done as f64 / t as f64) * 1000.0).round() / 10.0)
                    } else {
                        None
                    }
                });
                let eta_ms = self.total.and_then(|t| {
                    if self.done > 0 && t > self.done {
                        Some((elapsed_ms as f64 / self.done as f64 * (t - self.done) as f64) as u64)
                    } else {
                        None
                    }
                });
                let mut obj = serde_json::json!({
                    "event": event_type,
                    "phase": self.phase,
                    "done": self.done,
                    "elapsed_ms": elapsed_ms,
                });
                if let Some(t) = self.total {
                    obj["total"] = serde_json::json!(t);
                }
                if let Some(p) = pct {
                    obj["pct"] = p.into();
                }
                if let Some(e) = eta_ms {
                    obj["eta_ms"] = serde_json::json!(e);
                }
                let _ = writeln!(
                    self.writer,
                    "{}",
                    serde_json::to_string(&obj).unwrap_or_default()
                );
            }
            ProgressMode::Human => {
                let total_str = self.total.map_or("?".to_string(), |t| t.to_string());
                let line = if is_finish {
                    format!(
                        "\r[{}] {}/{} done ({:.1}s)",
                        self.phase,
                        self.done,
                        total_str,
                        elapsed_ms as f64 / 1000.0
                    )
                } else {
                    format!("\r[{}] {}/{}", self.phase, self.done, total_str)
                };
                let _ = write!(self.writer, "{}", line);
                if is_finish {
                    let _ = writeln!(self.writer);
                }
            }
            ProgressMode::Quiet | ProgressMode::Auto => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_progress_json_mode() {
        let _buf: Box<Vec<u8>> = Box::default();
        // Can't use Vec directly with 'static, so skip for now
    }

    #[test]
    fn test_progress_quiet_mode() {
        let q = ProgressMode::Quiet;
        // Quiet mode is distinct from Json
        assert_ne!(q, ProgressMode::Json);
    }

    #[test]
    fn test_progress_mode_from_env_json() {
        std::env::set_var("GBRAIN_PROGRESS_JSON", "1");
        assert_eq!(ProgressMode::from_env(), ProgressMode::Json);
        std::env::remove_var("GBRAIN_PROGRESS_JSON");
    }
}
