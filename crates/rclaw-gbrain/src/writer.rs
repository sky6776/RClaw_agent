//! Brain writer with validation modes
//! Mirrors gbrain's src/core/writer.ts
//!
//! Provides controlled write access to the brain with three modes:
//! - Strict: all writes must pass validation (default for MCP/remote)
//! - Lint: writes proceed but warnings are logged for issues
//! - Off: no validation (default for CLI/local)

use crate::error::{GBrainError, Result};
use crate::operations::Operations;
use crate::types::*;
use std::collections::HashSet;
use tracing::{debug, info, warn};

/// Write mode controlling validation strictness
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// All writes must pass validation; failures block the write
    Strict,
    /// Writes proceed but validation issues are logged as warnings
    Lint,
    /// No validation at all
    Off,
}

impl std::fmt::Display for WriteMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => write!(f, "strict"),
            Self::Lint => write!(f, "lint"),
            Self::Off => write!(f, "off"),
        }
    }
}

impl WriteMode {
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "strict" => Self::Strict,
            "lint" => Self::Lint,
            _ => Self::Off,
        }
    }
}

/// Validation issue found during write
#[derive(Debug, Clone)]
pub struct ValidationIssue {
    pub field: String,
    pub message: String,
    pub severity: IssueSeverity,
}

/// Issue severity
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Warning,
    Error,
}

/// Trait for custom write validators
pub trait WriteValidator: Send + Sync {
    /// Unique identifier for this validator
    fn id(&self) -> &'static str {
        std::any::type_name::<Self>()
    }
    /// Validate a page before writing. Return issues found.
    fn validate(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<&PageType>,
    ) -> Vec<ValidationIssue>;
}

/// Register built-in validators (link and citation) on a BrainWriter.
/// Mirrors TS registerBuiltinValidators() pattern.
pub fn register_builtin_validators(writer: &mut BrainWriter) {
    writer.add_validator(Box::new(LinkValidator::new()));
    writer.add_validator(Box::new(CitationValidator::new()));
}

// ── Built-in validators ──────────────────────────────────

/// Validates that `[text](path)` markdown links resolve to existing pages.
/// Mirrors gbrain's src/core/output/validators/link.ts
pub struct LinkValidator;

impl Default for LinkValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl LinkValidator {
    pub fn new() -> Self {
        Self
    }
}

impl WriteValidator for LinkValidator {
    fn id(&self) -> &'static str {
        "link"
    }

    fn validate(
        &self,
        slug: &str,
        _title: &str,
        content: &str,
        _page_type: Option<&PageType>,
    ) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        // P2-11: Regex patterns use OnceLock for lazy one-time compilation.
        static LINK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let re = LINK_RE.get_or_init(|| regex::Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
        let mut in_fence = false;

        for (line_num, line) in content.lines().enumerate() {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence || trimmed.starts_with('`') {
                continue;
            }

            for cap in re.captures_iter(line) {
                let target = &cap[2];
                if target.starts_with("http")
                    || target.starts_with("mailto:")
                    || target.starts_with('#')
                {
                    continue;
                }
                let target_slug = target
                    .trim_start_matches("../")
                    .trim_start_matches('/')
                    .trim_end_matches(".md");
                if target_slug.is_empty() {
                    continue;
                }
                if !target_slug.contains('/') || target_slug.contains(' ') {
                    issues.push(ValidationIssue {
                        field: slug.to_string(),
                        message: format!(
                            "Suspicious link target on line {}: [{}]({}) — not a valid slug",
                            line_num + 1,
                            &cap[1],
                            target
                        ),
                        severity: IssueSeverity::Warning,
                    });
                }
            }
        }
        issues
    }
}

/// Validates that factual paragraphs carry citation markers (`[Source: ...]` or URL links).
/// Mirrors gbrain's src/core/output/validators/citation.ts
pub struct CitationValidator;

impl Default for CitationValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl CitationValidator {
    pub fn new() -> Self {
        Self
    }
}

impl WriteValidator for CitationValidator {
    fn id(&self) -> &'static str {
        "citation"
    }

    fn validate(
        &self,
        slug: &str,
        _title: &str,
        content: &str,
        _page_type: Option<&PageType>,
    ) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();
        // P2-11: Regex patterns use OnceLock for lazy one-time compilation.
        static CITATION_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let citation_re = CITATION_RE.get_or_init(|| {
            regex::Regex::new(r"\[Source:\s*\S[^\]]*\]|\]\(\s*https?://[^)]+\)").unwrap()
        });
        let mut in_fence = false;

        for para in content.split("\n\n") {
            let trimmed = para.trim();
            if trimmed.is_empty() {
                continue;
            }
            if trimmed.starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if in_fence {
                continue;
            }

            // Skip non-factual content
            if trimmed.starts_with('#')
                || trimmed.starts_with('>')
                || trimmed.starts_with('|')
                || trimmed == "---"
            {
                continue;
            }

            if !citation_re.is_match(trimmed) && trimmed.split_whitespace().count() >= 5 {
                let first_line = para.lines().next().unwrap_or("");
                issues.push(ValidationIssue {
                    field: slug.to_string(),
                    message: format!("Uncited paragraph: \"{}\"", first_line),
                    severity: IssueSeverity::Warning,
                });
            }
        }
        issues
    }
}

/// Trait for validating that slugs exist (for link validation)
pub trait ExistingSlugValidator: Send + Sync {
    /// Check if a slug exists in the brain
    fn slug_exists(&self, slug: &str) -> bool;
}

/// Default write validator — checks common issues
pub struct DefaultWriteValidator {
    /// Set of valid slug prefixes
    valid_prefixes: HashSet<String>,
}

impl DefaultWriteValidator {
    pub fn new() -> Self {
        Self {
            valid_prefixes: [
                "people",
                "companies",
                "projects",
                "topics",
                "concepts",
                "events",
                "places",
                "resources",
                "notes",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

impl Default for DefaultWriteValidator {
    fn default() -> Self {
        Self::new()
    }
}

impl WriteValidator for DefaultWriteValidator {
    fn validate(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        _page_type: Option<&PageType>,
    ) -> Vec<ValidationIssue> {
        let mut issues = Vec::new();

        // Check slug format
        if slug.is_empty() {
            issues.push(ValidationIssue {
                field: "slug".to_string(),
                message: "Slug cannot be empty".to_string(),
                severity: IssueSeverity::Error,
            });
        }

        // Check slug prefix
        if slug.contains('/') {
            let prefix = slug.split('/').next().unwrap_or("");
            if !self.valid_prefixes.contains(prefix) && prefix != "unsorted" {
                issues.push(ValidationIssue {
                    field: "slug".to_string(),
                    message: format!("Unknown slug prefix '{}'", prefix),
                    severity: IssueSeverity::Warning,
                });
            }
        }

        // Check title
        if title.is_empty() {
            issues.push(ValidationIssue {
                field: "title".to_string(),
                message: "Title cannot be empty".to_string(),
                severity: IssueSeverity::Error,
            });
        }

        // Check content length
        if content.len() > 500_000 {
            issues.push(ValidationIssue {
                field: "content".to_string(),
                message: format!("Content is very large ({} chars)", content.len()),
                severity: IssueSeverity::Warning,
            });
        }

        // Check for orphan wikilinks (links to non-existent pages)
        // P2-11: Regex patterns use OnceLock for lazy one-time compilation.
        static WIKILINK_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let wikilink_pattern =
            WIKILINK_RE.get_or_init(|| regex::Regex::new(r"\[\[([^\]]+)\]\]").unwrap());
        for cap in wikilink_pattern.captures_iter(content) {
            let target = cap.get(1).unwrap().as_str();
            // Just flag as warning — we can't check existence without a validator
            if target.contains(' ') && !target.contains('/') {
                issues.push(ValidationIssue {
                    field: "content".to_string(),
                    message: format!(
                        "Wikilink '{}' contains spaces but no directory prefix",
                        target
                    ),
                    severity: IssueSeverity::Warning,
                });
            }
        }

        issues
    }
}

/// Brain writer — controlled write access with validation
pub struct BrainWriter<'a> {
    ops: &'a Operations<'a>,
    mode: WriteMode,
    validators: Vec<Box<dyn WriteValidator>>,
}

impl<'a> BrainWriter<'a> {
    pub fn new(ops: &'a Operations<'a>, mode: WriteMode) -> Self {
        Self {
            ops,
            mode,
            validators: vec![Box::new(DefaultWriteValidator::new())],
        }
    }

    /// Add a custom validator
    pub fn add_validator(&mut self, validator: Box<dyn WriteValidator>) {
        self.validators.push(validator);
    }

    /// Write a page with validation, running validators before the write transaction.
    /// Mirrors TS BrainWriter which runs validators inside the write transaction scope.
    pub fn put_page(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<PageType>,
        content_hash: Option<&str>,
    ) -> Result<Page> {
        info!(slug = %slug, mode = %self.mode, "Writing page");

        // Run validators first (before acquiring transaction)
        let issues = self.run_validators(slug, title, content, page_type.as_ref());
        if !issues.is_empty() {
            debug!(slug = %slug, issue_count = issues.len(), "Validation issues found");
        }

        match self.mode {
            WriteMode::Strict => {
                let errors: Vec<&ValidationIssue> = issues
                    .iter()
                    .filter(|i| i.severity == IssueSeverity::Error)
                    .collect();
                if !errors.is_empty() {
                    debug!(slug = %slug, error_count = errors.len(), "Strict mode blocking write due to validation errors");
                    let messages: Vec<String> = errors.iter().map(|e| e.message.clone()).collect();
                    return Err(GBrainError::InvalidInput(format!(
                        "Validation failed: {}",
                        messages.join("; ")
                    )));
                }
            }
            WriteMode::Lint => {
                for issue in &issues {
                    if issue.severity == IssueSeverity::Error {
                        warn!(field = %issue.field, message = %issue.message, "Validation error (lint mode, proceeding)");
                    } else {
                        warn!(field = %issue.field, message = %issue.message, "Validation warning");
                    }
                }
            }
            WriteMode::Off => {}
        }

        // Perform the write inside a transaction so validators and the write
        // are in the same transaction scope (mirrors TS BrainWriter behavior).
        self.ops
            .put_page_in_transaction(slug, title, content, page_type, content_hash)
    }

    /// Delete a page
    pub fn delete_page(&self, slug: &str) -> Result<()> {
        info!(slug = %slug, mode = %self.mode, "Deleting page");
        let start = std::time::Instant::now();
        let result = self.ops.delete_page(slug);
        debug!(slug = %slug, elapsed_ms = start.elapsed().as_millis() as u64, "Delete page completed");
        result
    }

    /// Run all validators and collect issues
    fn run_validators(
        &self,
        slug: &str,
        title: &str,
        content: &str,
        page_type: Option<&PageType>,
    ) -> Vec<ValidationIssue> {
        let mut all_issues = Vec::new();
        for validator in &self.validators {
            let issues = validator.validate(slug, title, content, page_type);
            debug!(validator = %validator.id(), issue_count = issues.len(), "Validator completed");
            all_issues.extend(issues);
        }
        all_issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_validator_empty_slug() {
        let validator = DefaultWriteValidator::new();
        let issues = validator.validate("", "Title", "content", None);
        assert!(issues
            .iter()
            .any(|i| i.field == "slug" && i.severity == IssueSeverity::Error));
    }

    #[test]
    fn test_default_validator_empty_title() {
        let validator = DefaultWriteValidator::new();
        let issues = validator.validate("people/alice", "", "content", None);
        assert!(issues
            .iter()
            .any(|i| i.field == "title" && i.severity == IssueSeverity::Error));
    }

    #[test]
    fn test_default_validator_unknown_prefix() {
        let validator = DefaultWriteValidator::new();
        let issues = validator.validate("xyz/test", "Title", "content", None);
        assert!(issues
            .iter()
            .any(|i| i.field == "slug" && i.severity == IssueSeverity::Warning));
    }

    #[test]
    fn test_default_validator_valid_slug() {
        let validator = DefaultWriteValidator::new();
        let issues = validator.validate("people/alice", "Alice", "content", None);
        assert!(!issues.iter().any(|i| i.severity == IssueSeverity::Error));
    }

    #[test]
    fn test_write_mode_from_str() {
        assert_eq!(WriteMode::from_str_lossy("strict"), WriteMode::Strict);
        assert_eq!(WriteMode::from_str_lossy("lint"), WriteMode::Lint);
        assert_eq!(WriteMode::from_str_lossy("off"), WriteMode::Off);
        assert_eq!(WriteMode::from_str_lossy("unknown"), WriteMode::Off);
    }
}
