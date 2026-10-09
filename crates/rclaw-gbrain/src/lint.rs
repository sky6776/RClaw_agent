//! Lint command — zero-LLM quality checker for brain pages
//! Mirrors gbrain's src/commands/lint.ts
//!
//! Checks:
//! - LLM preamble detection ("Of course", "Certainly", "Here is", "I've created")
//! - Placeholder dates (YYYY-MM-DD still present)
//! - Missing frontmatter
//! - Broken citations [Source:xxx] referencing non-existent slugs
//! - Empty sections (## heading with no content)
//! - Code fence residue (unclosed ```)

use crate::engine::BrainEngine;
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

static BROKEN_CITE_RE: OnceLock<regex::Regex> = OnceLock::new();

/// A single lint finding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintIssue {
    pub slug: String,
    pub severity: LintSeverity,
    pub rule: String,
    pub message: String,
    pub line: Option<usize>,
}

/// Lint severity levels
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LintSeverity {
    Info,
    Warning,
    Error,
}

impl std::fmt::Display for LintSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LintSeverity::Info => write!(f, "INFO"),
            LintSeverity::Warning => write!(f, "WARN"),
            LintSeverity::Error => write!(f, "ERR "),
        }
    }
}

/// Lint result for a single page
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintResult {
    pub slug: String,
    pub issues: Vec<LintIssue>,
    /// Fixed content after applying auto-fixes (Some if fixes were applied, None otherwise)
    pub fixed_content: Option<String>,
}

impl LintResult {
    pub fn has_errors(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity == LintSeverity::Error)
    }

    pub fn has_warnings(&self) -> bool {
        self.issues
            .iter()
            .any(|i| i.severity >= LintSeverity::Warning)
    }
}

/// Lint options
#[derive(Debug, Clone, Default)]
pub struct LintOpts {
    /// Fix issues automatically where possible
    pub fix: bool,
    /// Don't write changes, just report
    pub dry_run: bool,
}

/// LLM preamble patterns — these indicate AI-generated content that wasn't cleaned up
const LLM_PREAMBLE_PATTERNS: &[&str] = &[
    "of course,",
    "certainly,",
    "here is",
    "here's",
    "i've created",
    "i have created",
    "sure!",
    "absolutely!",
    "let me",
    "as an ai",
    "as a language model",
    "i'd be happy to",
    "i would be happy to",
    "i can help",
    "great question",
];

/// Run lint checks on all pages (or a specific slug)
pub fn lint_pages(engine: &SqliteEngine, slug: Option<&str>, opts: LintOpts) -> Vec<LintResult> {
    let pages = if let Some(s) = slug {
        match engine.get_page(s) {
            Ok(Some(page)) => vec![page],
            Ok(None) => {
                warn!(slug = %s, "Page not found for lint");
                return vec![];
            }
            Err(e) => {
                warn!(slug = %s, error = %e, "Failed to get page for lint");
                return vec![];
            }
        }
    } else {
        match engine.list_pages(PageFilters::default()) {
            Ok(pages) => pages,
            Err(e) => {
                warn!(error = %e, "Failed to list pages for lint");
                return vec![];
            }
        }
    };

    // Collect all slugs for citation validation (must use ALL pages, not just those being linted)
    let all_slugs: HashSet<String> = engine
        .get_all_slugs()
        .unwrap_or_else(|_| pages.iter().map(|p| p.slug.clone()).collect())
        .into_iter()
        .collect();

    let mut results = Vec::new();
    info!(
        page_count = pages.len(),
        fix = opts.fix,
        dry_run = opts.dry_run,
        "Starting lint run"
    );
    for page in &pages {
        let result = lint_page(engine, page, &all_slugs, &opts);

        // If fixes were applied and not a dry run, write the fixed content back
        if let Some(ref fixed) = result.fixed_content {
            if opts.fix && !opts.dry_run {
                match engine.put_page(
                    &page.slug,
                    PageInput {
                        page_type: page.page_type.clone(),
                        title: page.title.clone(),
                        compiled_truth: fixed.clone(),
                        // M37: Page.timeline 是 Option<String>（JSON 序列化后的字符串），
                        // PageInput.timeline 需要 Option<serde_json::Value>，必须反序列化。
                        // 如果反序列化失败（格式损坏），传入 None 会导致 timeline 被清空。
                        // TODO: 考虑为 put_page 增加 timeline_raw: Option<String> 字段，
                        //       直接透传原始字符串，避免反序列化再序列化的精度损失。
                        timeline: page
                            .timeline
                            .as_ref()
                            .and_then(|s| serde_json::from_str(s).ok()),
                        frontmatter: page
                            .frontmatter
                            .as_ref()
                            .and_then(|s| serde_json::from_str(s).ok()),
                        content_hash: None,
                    },
                ) {
                    Ok(_) => info!(slug = %page.slug, "Lint: applied auto-fixes"),
                    Err(e) => {
                        warn!(slug = %page.slug, error = %e, "Lint: failed to write fixed content")
                    }
                }
            }
        }

        results.push(result);
    }

    let total_issues: usize = results.iter().map(|r| r.issues.len()).sum();
    let pages_with_issues = results.iter().filter(|r| !r.issues.is_empty()).count();
    info!(
        page_count = pages.len(),
        pages_with_issues, total_issues, "Lint complete"
    );
    results
}

/// Lint a single page. When `opts.fix` is true, fixable issues are corrected
/// and the modified content is returned in `LintResult::fixed_content`.
fn lint_page(
    _engine: &SqliteEngine,
    page: &Page,
    all_slugs: &HashSet<String>,
    opts: &LintOpts,
) -> LintResult {
    let mut issues = Vec::new();
    let mut content = page.compiled_truth.clone();
    let mut fixed = false;

    // 1. LLM preamble detection (+ fix)
    check_llm_preamble(page, &mut issues);
    if opts.fix && issues.iter().any(|i| i.rule == "llm-preamble") {
        let fixed_content = fix_llm_preamble(&content);
        if fixed_content != content {
            content = fixed_content;
            fixed = true;
        }
    }

    // 2. Placeholder dates (flag only, not auto-fixable)
    check_placeholder_dates(page, &mut issues);

    // 3. Missing frontmatter
    check_missing_frontmatter(page, &mut issues);

    // 4. Broken citations
    check_broken_citations(page, all_slugs, &mut issues);

    // 5. Empty sections
    check_empty_sections(page, &mut issues);

    // 6. Code fence residue (+ fix)
    check_code_fence_residue(page, &mut issues);
    if opts.fix && issues.iter().any(|i| i.rule == "code-fence-residue") {
        let fixed_content = fix_code_fence_residue(&content);
        if fixed_content != content {
            content = fixed_content;
            fixed = true;
        }
    }

    if issues.is_empty() {
        debug!(slug = %page.slug, "Lint: clean");
    } else {
        info!(slug = %page.slug, issue_count = issues.len(), "Lint: issues found");
    }

    LintResult {
        slug: page.slug.clone(),
        issues,
        fixed_content: if fixed { Some(content) } else { None },
    }
}

/// Check for LLM preamble patterns in compiled_truth
fn check_llm_preamble(page: &Page, issues: &mut Vec<LintIssue>) {
    // Check each line individually (consistent with fix_llm_preamble)
    // to avoid missing preamble lines with leading whitespace
    for (i, line) in page.compiled_truth.lines().enumerate() {
        let trimmed_lower = line.trim().to_lowercase();
        for pattern in LLM_PREAMBLE_PATTERNS {
            if trimmed_lower.starts_with(pattern) {
                issues.push(LintIssue {
                    slug: page.slug.clone(),
                    severity: LintSeverity::Warning,
                    rule: "llm-preamble".to_string(),
                    message: format!("LLM preamble detected: \"{}\"", pattern),
                    line: Some(i + 1),
                });
                // Only report one issue per line
                break;
            }
        }
    }
}

/// Check for placeholder dates (YYYY-MM-DD that look like templates)
fn check_placeholder_dates(page: &Page, issues: &mut Vec<LintIssue>) {
    let content = &page.compiled_truth;
    for (i, line) in content.lines().enumerate() {
        // Detect standalone date-like placeholders: "YYYY-MM-DD" without surrounding context
        if line.contains("YYYY-MM-DD") {
            issues.push(LintIssue {
                slug: page.slug.clone(),
                severity: LintSeverity::Error,
                rule: "placeholder-date".to_string(),
                message: "Placeholder date YYYY-MM-DD found".to_string(),
                line: Some(i + 1),
            });
        }
        // Also check for "DATE" as a standalone placeholder
        if line.contains("[DATE]") || line.contains("{{DATE}}") {
            issues.push(LintIssue {
                slug: page.slug.clone(),
                severity: LintSeverity::Error,
                rule: "placeholder-date".to_string(),
                message: "Placeholder date variable found".to_string(),
                line: Some(i + 1),
            });
        }
    }
}

/// Check for missing frontmatter
fn check_missing_frontmatter(page: &Page, issues: &mut Vec<LintIssue>) {
    // If compiled_truth starts with # or text (not ---), frontmatter is missing
    let ct = page.compiled_truth.trim_start();
    if !ct.starts_with("---") && !ct.is_empty() {
        // Only warn if page has substantial content (more than just a title)
        if ct.lines().count() > 2 {
            issues.push(LintIssue {
                slug: page.slug.clone(),
                severity: LintSeverity::Info,
                rule: "missing-frontmatter".to_string(),
                message: "Page has no frontmatter (--- yaml header)".to_string(),
                line: Some(1),
            });
        }
    }
}

/// Check for broken citations [Source:slug] referencing non-existent slugs
fn check_broken_citations(page: &Page, all_slugs: &HashSet<String>, issues: &mut Vec<LintIssue>) {
    let content = &page.compiled_truth;
    let re = BROKEN_CITE_RE.get_or_init(|| regex::Regex::new(r"\[Source:([^\]]+)\]").unwrap());

    for cap in re.captures_iter(content) {
        if let Some(slug_ref) = cap.get(1) {
            let slug = slug_ref.as_str().trim();
            if !slug.is_empty() && !all_slugs.contains(slug) {
                let line = cap
                    .get(0)
                    .map(|m| content[..m.start()].matches('\n').count() + 1);
                issues.push(LintIssue {
                    slug: page.slug.clone(),
                    severity: LintSeverity::Warning,
                    rule: "broken-citation".to_string(),
                    message: format!("Citation references non-existent slug: {}", slug),
                    line,
                });
            }
        }
    }
}

/// Check for empty sections (## heading with no content before next heading)
fn check_empty_sections(page: &Page, issues: &mut Vec<LintIssue>) {
    let content = &page.compiled_truth;
    let lines: Vec<&str> = content.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim();
        if line.starts_with("## ") {
            let heading = line;
            // Check if next non-empty line is also a heading or end of content
            let mut j = i + 1;
            while j < lines.len() && lines[j].trim().is_empty() {
                j += 1;
            }
            if j >= lines.len() || lines[j].trim().starts_with('#') {
                issues.push(LintIssue {
                    slug: page.slug.clone(),
                    severity: LintSeverity::Info,
                    rule: "empty-section".to_string(),
                    message: format!("Empty section: {}", heading),
                    line: Some(i + 1),
                });
            }
        }
        i += 1;
    }
}

/// Check for code fence residue (unclosed ```)
fn check_code_fence_residue(page: &Page, issues: &mut Vec<LintIssue>) {
    let content = &page.compiled_truth;
    let mut fence_count = 0u32;
    let mut last_fence_line = 0usize;

    for (i, line) in content.lines().enumerate() {
        if line.trim().starts_with("```") {
            fence_count += 1;
            last_fence_line = i + 1;
        }
    }

    if !fence_count.is_multiple_of(2) {
        issues.push(LintIssue {
            slug: page.slug.clone(),
            severity: LintSeverity::Error,
            rule: "code-fence-residue".to_string(),
            message: format!(
                "Unclosed code fence ({} opening, expected even count)",
                fence_count
            ),
            line: Some(last_fence_line),
        });
    }
}

/// Fix LLM preamble lines: remove lines that start with a known preamble pattern.
///
/// A line is removed if its trimmed, lowercased form starts with one of the
/// `LLM_PREAMBLE_PATTERNS`. This handles both preamble lines at the very top
/// of the content and preamble lines that appear after a newline (mid-text
/// AI injections).
fn fix_llm_preamble(content: &str) -> String {
    let mut result_lines = Vec::new();
    let mut removed_count = 0usize;
    for line in content.lines() {
        let trimmed_lower = line.trim().to_lowercase();
        let is_preamble = LLM_PREAMBLE_PATTERNS
            .iter()
            .any(|pat| trimmed_lower.starts_with(pat));
        if !is_preamble {
            result_lines.push(line);
        } else {
            removed_count += 1;
        }
    }
    if removed_count > 0 {
        debug!(
            lines_removed = removed_count,
            "fix_llm_preamble removed preamble lines"
        );
    }
    // Re-join with newlines; preserve trailing newline if original had one
    let mut fixed = result_lines.join("\n");
    if content.ends_with('\n') && !fixed.ends_with('\n') {
        fixed.push('\n');
    }
    fixed
}

/// Fix unclosed code fences: if there is an odd number of ``` markers,
/// append a closing ``` at the end of the content.
fn fix_code_fence_residue(content: &str) -> String {
    let fence_count = content
        .lines()
        .filter(|line| line.trim().starts_with("```"))
        .count();

    if fence_count % 2 != 0 {
        // Odd number of fences — append closing fence
        debug!("fix_code_fence_residue: appending closing fence");
        let mut fixed = content.to_string();
        // Ensure content ends with a newline before appending ```
        if !fixed.ends_with('\n') {
            fixed.push('\n');
        }
        fixed.push_str("```\n");
        fixed
    } else {
        content.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lint_severity_ordering() {
        assert!(LintSeverity::Error > LintSeverity::Warning);
        assert!(LintSeverity::Warning > LintSeverity::Info);
    }

    #[test]
    fn test_check_llm_preamble() {
        let page = Page {
            id: 0,
            slug: "test/preamble".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "Of course, here is the information about Alice.".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let mut issues = Vec::new();
        check_llm_preamble(&page, &mut issues);
        assert!(!issues.is_empty());
        assert_eq!(issues[0].rule, "llm-preamble");
    }

    #[test]
    fn test_check_placeholder_dates() {
        let page = Page {
            id: 0,
            slug: "test/dates".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "Event on YYYY-MM-DD was important.".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let mut issues = Vec::new();
        check_placeholder_dates(&page, &mut issues);
        assert!(!issues.is_empty());
        assert_eq!(issues[0].rule, "placeholder-date");
    }

    #[test]
    fn test_check_code_fence_residue() {
        let page = Page {
            id: 0,
            slug: "test/fence".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "```python\nprint('hello')\n".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let mut issues = Vec::new();
        check_code_fence_residue(&page, &mut issues);
        assert!(!issues.is_empty());
        assert_eq!(issues[0].rule, "code-fence-residue");
    }

    #[test]
    fn test_check_empty_sections() {
        let page = Page {
            id: 0,
            slug: "test/empty".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "## Overview\n\n## Details\nSome content".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let mut issues = Vec::new();
        check_empty_sections(&page, &mut issues);
        assert!(!issues.is_empty());
        assert_eq!(issues[0].rule, "empty-section");
    }

    #[test]
    fn test_check_broken_citations() {
        let mut all_slugs = HashSet::new();
        all_slugs.insert("people/alice".to_string());

        let page = Page {
            id: 0,
            slug: "test/cite".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "See [Source:people/bob] for details.".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let mut issues = Vec::new();
        check_broken_citations(&page, &all_slugs, &mut issues);
        assert!(!issues.is_empty());
        assert_eq!(issues[0].rule, "broken-citation");
    }

    // ── Auto-fix tests ─────────────────────────────────────────

    #[test]
    fn test_fix_llm_preamble_removes_preamble_line() {
        let content = "Of course, here is the info.\nReal content here.";
        let fixed = fix_llm_preamble(content);
        assert!(!fixed.contains("Of course"));
        assert!(fixed.contains("Real content here."));
    }

    #[test]
    fn test_fix_llm_preamble_removes_mid_text_preamble() {
        let content = "Some real content\nI've created a summary.\nMore content";
        let fixed = fix_llm_preamble(content);
        assert!(!fixed.contains("I've created"));
        assert!(fixed.contains("Some real content"));
        assert!(fixed.contains("More content"));
    }

    #[test]
    fn test_fix_llm_preamble_preserves_clean_content() {
        let content = "This is a clean page.\nNo preamble here.";
        let fixed = fix_llm_preamble(content);
        assert_eq!(fixed, content);
    }

    #[test]
    fn test_fix_llm_preamble_preserves_trailing_newline() {
        let content = "Certainly, here it is.\nActual content.\n";
        let fixed = fix_llm_preamble(content);
        assert!(fixed.ends_with('\n'));
        assert!(!fixed.contains("Certainly"));
    }

    #[test]
    fn test_fix_code_fence_residue_closes_unclosed() {
        let content = "```python\nprint('hello')\n";
        let fixed = fix_code_fence_residue(content);
        // Should have a closing fence at the end
        assert!(fixed.ends_with("```\n"));
        // Count fences -- should be even now
        let count = fixed
            .lines()
            .filter(|l| l.trim().starts_with("```"))
            .count();
        assert_eq!(count % 2, 0);
    }

    #[test]
    fn test_fix_code_fence_residue_no_change_if_even() {
        let content = "```python\nprint('hello')\n```\n";
        let fixed = fix_code_fence_residue(content);
        assert_eq!(fixed, content);
    }

    #[test]
    fn test_fix_code_fence_residue_appends_newline_before_fence() {
        let content = "```js\nconsole.log('hi')"; // no trailing newline
        let fixed = fix_code_fence_residue(content);
        assert!(fixed.contains("```\n"));
        // The original line should still be there
        assert!(fixed.contains("console.log"));
    }

    #[test]
    fn test_lint_page_with_fix_returns_fixed_content() {
        let page = Page {
            id: 0,
            slug: "test/fix".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "Of course, here is the info.\nReal content.".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let all_slugs = HashSet::new();
        let opts_with_fix = LintOpts {
            fix: true,
            dry_run: false,
        };
        let opts_no_fix = LintOpts {
            fix: false,
            dry_run: false,
        };
        let engine = SqliteEngine::with_config(":memory:", crate::config::Config::default());

        // With fix enabled, should return fixed content
        let result = lint_page(&engine, &page, &all_slugs, &opts_with_fix);
        assert!(result.fixed_content.is_some());
        assert!(!result.fixed_content.as_ref().unwrap().contains("Of course"));
        assert!(result
            .fixed_content
            .as_ref()
            .unwrap()
            .contains("Real content"));

        // Without fix, should not return fixed content
        let result = lint_page(&engine, &page, &all_slugs, &opts_no_fix);
        assert!(result.fixed_content.is_none());
    }

    #[test]
    fn test_lint_page_placeholder_date_not_auto_fixed() {
        let page = Page {
            id: 0,
            slug: "test/nofix".to_string(),
            title: "Test".to_string(),
            page_type: PageType::Person,
            compiled_truth: "Event on YYYY-MM-DD was important.".to_string(),
            timeline: None,
            frontmatter: None,
            created_at: String::new(),
            updated_at: String::new(),
            content_hash: None,
            deleted_at: None,
        };
        let all_slugs = HashSet::new();
        let opts = LintOpts {
            fix: true,
            dry_run: false,
        };
        let engine = SqliteEngine::with_config(":memory:", crate::config::Config::default());

        let result = lint_page(&engine, &page, &all_slugs, &opts);
        // placeholder-date is not auto-fixable, so no fixed content
        assert!(result.fixed_content.is_none());
        // But the issue should still be reported
        assert!(result.issues.iter().any(|i| i.rule == "placeholder-date"));
    }
}
