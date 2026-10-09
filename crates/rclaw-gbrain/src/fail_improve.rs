//! Fail-Improve Loop — deterministic-first, LLM-fallback with failure logging
//! Mirrors gbrain's src/core/fail-improve.ts
//!
//! P2-2 enhancements:
//! - Call count tracking (total + deterministic hits) in separate JSON files
//! - Log rotation (MAX_ENTRIES = 1000)
//! - Improvement tracking (improvements.json)
//!
//! When a deterministic function fails, falls back to LLM.
//! All failures are logged as JSONL for future analysis and improvement.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{debug, info, warn};

/// Maximum entries before log rotation (mirrors TS MAX_ENTRIES = 1000)
const MAX_ENTRIES: usize = 1000;

/// A single failure entry in the JSONL log
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureEntry {
    pub timestamp: String,
    pub operation: String,
    pub input: String,
    pub deterministic_result: Option<String>,
    pub llm_result: Option<String>,
    pub metadata: Option<serde_json::Value>,
}

/// Call count tracking (mirrors TS incrementCallCount)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CallCounts {
    pub total: usize,
    pub deterministic: usize,
}

/// Improvement tracking entry (mirrors TS logImprovement)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImprovementEntry {
    pub timestamp: String,
    pub description: String,
}

/// Analysis of failure patterns for an operation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureAnalysis {
    pub operation: String,
    pub total_failures: usize,
    pub failures_by_pattern: HashMap<String, usize>,
    pub total_improvements: usize,
    pub last_improvement: Option<String>,
    pub total_calls: usize,
    pub deterministic_hits: usize,
    pub deterministic_rate: f64,
}

/// A generated test case from failure analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestCase {
    pub operation: String,
    pub input: String,
    pub expected_output: String,
    pub source_failure: String,
}

/// Fail-Improve Loop — try deterministic first, fall back to LLM, log failures
pub struct FailImproveLoop {
    log_dir: PathBuf,
}

impl FailImproveLoop {
    pub fn new(log_dir: PathBuf) -> Self {
        debug!(log_dir = %log_dir.display(), "FailImproveLoop initialized");
        Self { log_dir }
    }

    /// Get the log file path for an operation
    fn log_path(&self, operation: &str) -> PathBuf {
        self.log_dir.join(format!("{}.jsonl", operation))
    }

    /// P2-2: Get the call count file path for an operation
    fn call_count_path(&self, operation: &str) -> PathBuf {
        self.log_dir.join(format!("{}_counts.json", operation))
    }

    /// P2-2: Get the improvements file path for an operation
    fn improvements_path(&self, operation: &str) -> PathBuf {
        self.log_dir
            .join(format!("{}_improvements.json", operation))
    }

    /// P2-2: Increment call count (mirrors TS incrementCallCount)
    pub fn increment_call_count(&self, operation: &str, type_: &str) {
        let path = self.call_count_path(operation);
        let mut counts = self.load_call_counts(operation);
        match type_ {
            "total" => counts.total += 1,
            "deterministic" => counts.deterministic += 1,
            _ => {}
        }
        if let Err(e) = self.write_json(&path, &counts) {
            warn!(operation = %operation, error = %e, "Failed to write call counts");
        }
    }

    /// P2-2: Load call counts from file
    fn load_call_counts(&self, operation: &str) -> CallCounts {
        let path = self.call_count_path(operation);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(counts) = serde_json::from_str(&content) {
                    return counts;
                }
            }
        }
        CallCounts::default()
    }

    /// P2-2: Log an improvement (mirrors TS logImprovement)
    pub fn log_improvement(&self, operation: &str, description: &str) {
        let path = self.improvements_path(operation);
        let mut improvements: Vec<ImprovementEntry> = if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                serde_json::from_str(&content).unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        };

        improvements.push(ImprovementEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            description: description.to_string(),
        });

        if let Err(e) = self.write_json(&path, &improvements) {
            warn!(operation = %operation, error = %e, "Failed to write improvements");
        }
    }

    /// P2-2: Load improvements for an operation
    pub fn load_improvements(&self, operation: &str) -> Vec<ImprovementEntry> {
        let path = self.improvements_path(operation);
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                serde_json::from_str(&content).unwrap_or_default()
            } else {
                Vec::new()
            }
        } else {
            Vec::new()
        }
    }

    /// Try deterministic first, fall back to LLM, log failures
    /// Returns the result from whichever source succeeded.
    /// P2-2: Also tracks call counts (total + deterministic hits).
    pub fn execute<F, D>(
        &self,
        operation: &str,
        input: &str,
        deterministic_fn: D,
        fallback_fn: F,
    ) -> Result<String, String>
    where
        D: Fn(&str) -> Option<String>,
        F: Fn(&str) -> Result<String, String>,
    {
        // P2-2: Track total call count
        self.increment_call_count(operation, "total");

        // 1. Try deterministic
        if let Some(result) = deterministic_fn(input) {
            debug!(operation = %operation, "Deterministic function succeeded");
            // P2-2: Track deterministic hit
            self.increment_call_count(operation, "deterministic");
            return Ok(result);
        }

        // 2. Fall back to LLM
        info!(operation = %operation, "Deterministic failed, falling back to LLM");
        let result = match fallback_fn(input) {
            Ok(r) => {
                debug!(operation = %operation, "LLM fallback succeeded");
                r
            }
            Err(e) => {
                // LLM also failed — log the failure before propagating
                self.log_failure(operation, input, None, None);
                return Err(e);
            }
        };

        // 3. Log failure for future improvement
        self.log_failure(operation, input, None, Some(&result));

        Ok(result)
    }

    /// Log a failure entry
    pub fn log_failure(
        &self,
        operation: &str,
        input: &str,
        deterministic_result: Option<&str>,
        llm_result: Option<&str>,
    ) {
        let input_preview: String = input.chars().take(80).collect();
        warn!(operation = %operation, input_preview = %input_preview, "Logging failure entry");
        let entry = FailureEntry {
            timestamp: chrono::Utc::now().to_rfc3339(),
            operation: operation.to_string(),
            input: input.to_string(),
            deterministic_result: deterministic_result.map(|s| s.to_string()),
            llm_result: llm_result.map(|s| s.to_string()),
            metadata: None,
        };

        if let Err(e) = self.append_to_log(operation, &entry) {
            warn!(operation = %operation, error = %e, "Failed to write failure log");
        }

        // P2-2: Rotate log if needed
        self.rotate_if_needed(operation);
    }

    /// Load all failure entries for an operation
    pub fn load_failures(&self, operation: &str) -> Vec<FailureEntry> {
        let path = self.log_path(operation);
        if !path.exists() {
            return Vec::new();
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    /// Analyze failure patterns for an operation
    /// P2-2: Uses accurate call counts instead of approximations
    pub fn analyze_failures(&self, operation: &str) -> FailureAnalysis {
        let failures = self.load_failures(operation);
        let improvements = self.load_improvements(operation);

        let mut failures_by_pattern: HashMap<String, usize> = HashMap::new();
        for entry in &failures {
            // Group by input prefix (first 50 chars) as a simple pattern
            let prefix: String = entry.input.chars().take(50).collect();
            let pattern = if prefix.len() < entry.input.len() {
                format!("{}...", prefix)
            } else {
                entry.input.clone()
            };
            *failures_by_pattern.entry(pattern).or_insert(0) += 1;
        }

        // P2-2: Use accurate call counts from separate file
        let counts = self.load_call_counts(operation);
        let total_calls = counts.total;
        let deterministic_hits = counts.deterministic;
        let total_failures = failures.len();

        let deterministic_rate = if total_calls > 0 {
            deterministic_hits as f64 / total_calls as f64
        } else {
            1.0
        };

        debug!(
            operation = %operation,
            total_failures,
            total_calls,
            deterministic_rate,
            "Failure analysis complete"
        );

        let last_improvement = improvements.last().map(|i| i.timestamp.clone());

        FailureAnalysis {
            operation: operation.to_string(),
            total_failures,
            failures_by_pattern,
            total_improvements: improvements.len(),
            last_improvement,
            total_calls,
            deterministic_hits,
            deterministic_rate,
        }
    }

    /// Generate test cases from failure entries
    pub fn generate_test_cases(&self, operation: &str) -> Vec<TestCase> {
        let failures = self.load_failures(operation);
        failures
            .iter()
            .filter_map(|entry| {
                entry.llm_result.as_ref().map(|expected| TestCase {
                    operation: operation.to_string(),
                    input: entry.input.clone(),
                    expected_output: expected.clone(),
                    source_failure: entry.timestamp.clone(),
                })
            })
            .collect()
    }

    /// Append a failure entry to the JSONL log
    fn append_to_log(&self, operation: &str, entry: &FailureEntry) -> std::io::Result<()> {
        // Ensure directory exists
        if let Some(parent) = self.log_path(operation).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let path = self.log_path(operation);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let line = serde_json::to_string(entry)?;
        use std::io::Write;
        file.write_all(line.as_bytes())?;
        file.write_all(b"\n")?;
        Ok(())
    }

    /// P2-2: Rotate log if entries exceed MAX_ENTRIES (mirrors TS)
    fn rotate_if_needed(&self, operation: &str) {
        let path = self.log_path(operation);
        if !path.exists() {
            return;
        }

        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

        if lines.len() > MAX_ENTRIES {
            let kept = lines
                .iter()
                .rev()
                .take(MAX_ENTRIES)
                .rev() // restore chronological order (oldest first)
                .map(|l| l.to_string())
                .collect::<Vec<String>>();
            let rotated_content = format!("{}\n", kept.join("\n"));
            if let Err(e) = std::fs::write(&path, rotated_content) {
                warn!(operation = %operation, error = %e, "Failed to rotate log");
            }
        }
    }

    /// Write a JSON-serializable value to a file
    fn write_json<T: Serialize>(&self, path: &PathBuf, value: &T) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = serde_json::to_string_pretty(value)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_deterministic_first() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test");
        let loop_ = FailImproveLoop::new(dir.clone());

        let result = loop_.execute(
            "test_op",
            "hello",
            |input| Some(format!("deterministic: {}", input)),
            |input| Ok(format!("llm: {}", input)),
        );

        assert_eq!(result.unwrap(), "deterministic: hello");

        // P2-2: Verify call count was tracked
        let counts = loop_.load_call_counts("test_op");
        assert_eq!(counts.total, 1);
        assert_eq!(counts.deterministic, 1);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_execute_fallback_to_llm() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test2");
        let loop_ = FailImproveLoop::new(dir.clone());

        let result = loop_.execute(
            "test_op",
            "hello",
            |_input| None, // deterministic fails
            |input| Ok(format!("llm: {}", input)),
        );

        assert_eq!(result.unwrap(), "llm: hello");

        // Verify failure was logged
        let failures = loop_.load_failures("test_op");
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].input, "hello");

        // P2-2: Verify call count was tracked (total=1, deterministic=0)
        let counts = loop_.load_call_counts("test_op");
        assert_eq!(counts.total, 1);
        assert_eq!(counts.deterministic, 0);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_analyze_failures() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test3");
        let loop_ = FailImproveLoop::new(dir.clone());

        loop_.log_failure("analyze_op", "input1", None, Some("result1"));
        loop_.log_failure("analyze_op", "input2", None, Some("result2"));

        let analysis = loop_.analyze_failures("analyze_op");
        assert_eq!(analysis.total_failures, 2);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_generate_test_cases() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test4");
        let loop_ = FailImproveLoop::new(dir.clone());

        loop_.log_failure("gen_op", "input1", None, Some("expected1"));

        let test_cases = loop_.generate_test_cases("gen_op");
        assert_eq!(test_cases.len(), 1);
        assert_eq!(test_cases[0].input, "input1");
        assert_eq!(test_cases[0].expected_output, "expected1");

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_call_count_tracking() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test5");
        let loop_ = FailImproveLoop::new(dir.clone());

        loop_.increment_call_count("count_op", "total");
        loop_.increment_call_count("count_op", "total");
        loop_.increment_call_count("count_op", "deterministic");

        let counts = loop_.load_call_counts("count_op");
        assert_eq!(counts.total, 2);
        assert_eq!(counts.deterministic, 1);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_improvement_tracking() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test6");
        let loop_ = FailImproveLoop::new(dir.clone());

        loop_.log_improvement("imp_op", "Added better heuristic for names");
        loop_.log_improvement("imp_op", "Improved CJK detection");

        let improvements = loop_.load_improvements("imp_op");
        assert_eq!(improvements.len(), 2);
        assert_eq!(
            improvements[0].description,
            "Added better heuristic for names"
        );

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_log_rotation() {
        let dir = std::env::temp_dir().join("gbrain_fail_improve_test7");
        let loop_ = FailImproveLoop::new(dir.clone());

        // Write more than MAX_ENTRIES entries
        for i in 0..(MAX_ENTRIES + 100) {
            loop_.log_failure("rot_op", &format!("input{}", i), None, Some("result"));
        }

        // Verify rotation happened
        let failures = loop_.load_failures("rot_op");
        assert!(failures.len() <= MAX_ENTRIES);

        // Clean up
        let _ = std::fs::remove_dir_all(&dir);
    }
}
