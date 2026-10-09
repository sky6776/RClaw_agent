//! Output validators — validate content before writing to the brain
//! Mirrors gbrain's src/core/validators.ts
//!
//! Validators check content integrity before it's committed:
//! - Back-link validator: ensure link targets exist
//! - Citation validator: check citation format
//! - Source citation validator: check [Source:slug] references point to existing pages
//! - Back-link symmetry validator: check outbound links have corresponding inbound links
//! - Link validator: check slug format
//! - Triple-hr validator: check --- separators

use crate::engine::BrainEngine;
use crate::sqlite_engine::SqliteEngine;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::OnceLock;
use tracing::{debug, info};

static WIKILINK_RE: OnceLock<Regex> = OnceLock::new();
static CITE_RE: OnceLock<Regex> = OnceLock::new();
static VALID_SLUG_RE: OnceLock<Regex> = OnceLock::new();
static MD_LINK_RE: OnceLock<Regex> = OnceLock::new();
static SOURCE_CITE_RE: OnceLock<Regex> = OnceLock::new();
static REF_RE: OnceLock<Regex> = OnceLock::new();

/// Validation severity
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Error,
    Warning,
    Info,
}

/// A single validation issue
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub rule: String,
    pub severity: Severity,
    pub message: String,
    pub location: Option<String>,
    pub fix_hint: Option<String>,
}

/// Result of validating content
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub slug: String,
    pub issues: Vec<ValidationIssue>,
    pub is_valid: bool,
}

impl ValidationResult {
    pub fn new(slug: &str) -> Self {
        Self {
            slug: slug.to_string(),
            issues: Vec::new(),
            is_valid: true,
        }
    }

    pub fn add_issue(&mut self, issue: ValidationIssue) {
        if issue.severity == Severity::Error {
            self.is_valid = false;
        }
        self.issues.push(issue);
    }

    pub fn errors(&self) -> Vec<&ValidationIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Error)
            .collect()
    }

    pub fn warnings(&self) -> Vec<&ValidationIssue> {
        self.issues
            .iter()
            .filter(|i| i.severity == Severity::Warning)
            .collect()
    }
}

/// Validate that wiki-link targets exist in the brain
pub fn validate_back_links(engine: &SqliteEngine, slug: &str, content: &str) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    // Extract all [[wiki-links]]
    let re = WIKILINK_RE.get_or_init(|| Regex::new(r"\[\[([^\]]+)\]\]").unwrap());
    let all_slugs: HashSet<String> = engine
        .get_all_slugs()
        .unwrap_or_default()
        .into_iter()
        .collect();

    for cap in re.captures_iter(content) {
        let target = cap.get(1).unwrap().as_str().to_string();
        if !all_slugs.contains(&target) {
            result.add_issue(ValidationIssue {
                rule: "back-link".to_string(),
                severity: Severity::Warning,
                message: format!("Link target '{}' does not exist", target),
                location: Some(format!("[[{}]]", target)),
                fix_hint: Some(format!("Create page '{}' or fix the link", target)),
            });
        }
    }

    debug!(slug = %slug, issue_count = result.issues.len(), "Back-link validation complete");
    result
}

/// Validate citation format (e.g., [1], [source:xxx])
pub fn validate_citations(slug: &str, content: &str) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    // Check for malformed citations: [number] without corresponding reference
    // P2-11: Regex patterns use OnceLock for lazy one-time compilation.
    let cite_re = CITE_RE.get_or_init(|| Regex::new(r"\[(\d+)\]").unwrap());
    let ref_re = REF_RE.get_or_init(|| {
        regex::RegexBuilder::new(r"^\[(\d+)\]:\s+.+")
            .multi_line(true)
            .build()
            .unwrap()
    });

    let cited_numbers: HashSet<String> = cite_re
        .captures_iter(content)
        .filter_map(|c| c.get(1).map(|m| m.as_str().to_string()))
        .collect();

    let ref_numbers: HashSet<String> = ref_re
        .captures_iter(content)
        .filter_map(|c: regex::Captures<'_>| c.get(1).map(|m| m.as_str().to_string()))
        .collect();

    for num in &cited_numbers {
        if !ref_numbers.contains(num) {
            result.add_issue(ValidationIssue {
                rule: "citation".to_string(),
                severity: Severity::Warning,
                message: format!(
                    "Citation [{}] has no corresponding reference definition",
                    num
                ),
                location: Some(format!("[{}]", num)),
                fix_hint: Some(format!("Add reference definition: [{}]: <url>", num)),
            });
        }
    }

    debug!(slug = %slug, issue_count = result.issues.len(), "Citation validation complete");
    result
}

/// Validate slug format (prefix/name, lowercase, alphanumeric + hyphens)
pub fn validate_slug(slug: &str) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    if slug.is_empty() {
        result.add_issue(ValidationIssue {
            rule: "slug".to_string(),
            severity: Severity::Error,
            message: "Slug cannot be empty".to_string(),
            location: None,
            fix_hint: None,
        });
        return result;
    }

    if slug.contains(' ') {
        result.add_issue(ValidationIssue {
            rule: "slug".to_string(),
            severity: Severity::Error,
            message: format!("Slug '{}' contains spaces", slug),
            location: None,
            fix_hint: Some("Replace spaces with hyphens".to_string()),
        });
    }

    if slug != slug.to_lowercase() {
        debug!(slug = %slug, "Slug validation: not lowercase");
        result.add_issue(ValidationIssue {
            rule: "slug".to_string(),
            severity: Severity::Warning,
            message: format!("Slug '{}' should be lowercase", slug),
            location: None,
            fix_hint: Some(format!("Use '{}' instead", slug.to_lowercase())),
        });
    }

    // Check for invalid characters
    let valid_re = VALID_SLUG_RE.get_or_init(|| Regex::new(r"^[a-z0-9/\-]+$").unwrap());
    if !valid_re.is_match(slug) {
        result.add_issue(ValidationIssue {
            rule: "slug".to_string(),
            severity: Severity::Error,
            message: format!("Slug '{}' contains invalid characters (only a-z, 0-9, /, - allowed)", slug),
            location: None,
            fix_hint: Some("Remove special characters and use only lowercase alphanumeric, hyphens, and slashes".to_string()),
        });
    }

    result
}

/// Validate triple-hr (---) separators in frontmatter
pub fn validate_triple_hr(slug: &str, content: &str) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    let count = content.matches("---").count();

    // If there's frontmatter, there should be exactly 2 separators (opening and closing)
    if content.starts_with("---") && count < 2 {
        result.add_issue(ValidationIssue {
            rule: "triple-hr".to_string(),
            severity: Severity::Error,
            message: "Frontmatter opening --- found but no closing ---".to_string(),
            location: Some("line 1".to_string()),
            fix_hint: Some("Add closing --- after frontmatter".to_string()),
        });
    }

    // Check for stray --- in body (not frontmatter)
    if count > 2 {
        let lines: Vec<&str> = content.lines().collect();
        let mut separator_count = 0;

        for (i, line) in lines.iter().enumerate() {
            if line.trim() == "---" {
                separator_count += 1;
                if separator_count > 2 {
                    result.add_issue(ValidationIssue {
                        rule: "triple-hr".to_string(),
                        severity: Severity::Info,
                        message: format!("Stray --- separator at line {}", i + 1),
                        location: Some(format!("line {}", i + 1)),
                        fix_hint: None,
                    });
                }
            }
        }
    }

    debug!(slug = %slug, stray_count = result.issues.len(), "Triple-hr validation complete");
    result
}

/// Validate that markdown [text](slug) link targets exist in the brain
/// Mirrors TS output/validators/link.ts — checks dangling markdown links
pub fn validate_links(engine: &SqliteEngine, slug: &str, content: &str) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    // Extract all markdown links [text](target)
    let md_link_re = MD_LINK_RE.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
    let all_slugs: HashSet<String> = engine
        .get_all_slugs()
        .unwrap_or_default()
        .into_iter()
        .collect();

    for cap in md_link_re.captures_iter(content) {
        let target = cap.get(2).unwrap().as_str().to_string();
        // Skip external URLs (http/https) and anchors (#)
        if target.starts_with("http://")
            || target.starts_with("https://")
            || target.starts_with("#")
        {
            continue;
        }
        // Strip anchor suffix if present (e.g., "people/alice#section")
        let clean_target = if let Some(pos) = target.find('#') {
            &target[..pos]
        } else {
            &target
        };
        if !clean_target.is_empty() && !all_slugs.contains(clean_target) {
            result.add_issue(ValidationIssue {
                rule: "link".to_string(),
                severity: Severity::Warning,
                message: format!("Markdown link target '{}' does not exist", clean_target),
                location: Some(cap.get(0).unwrap().as_str().to_string()),
                fix_hint: Some(format!("Create page '{}' or fix the link", clean_target)),
            });
        }
    }

    debug!(slug = %slug, issue_count = result.issues.len(), "Link validation complete");
    result
}

/// P2-4: Validate that [Source:slug] references point to existing source pages.
/// Mirrors TS citationValidator — checks `[Source:xxx]` citations where xxx should
/// be a valid page slug in the brain.
pub fn validate_source_citations(
    engine: &SqliteEngine,
    slug: &str,
    content: &str,
) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    let re = SOURCE_CITE_RE.get_or_init(|| Regex::new(r"\[Source:([^\]]+)\]").unwrap());
    let all_slugs: HashSet<String> = engine
        .get_all_slugs()
        .unwrap_or_default()
        .into_iter()
        .collect();

    for cap in re.captures_iter(content) {
        let target = cap.get(1).unwrap().as_str().trim().to_string();
        if !target.is_empty() && !all_slugs.contains(&target) {
            result.add_issue(ValidationIssue {
                rule: "source-citation".to_string(),
                severity: Severity::Warning,
                message: format!("Source citation '{}' references non-existent page", target),
                location: Some(cap.get(0).unwrap().as_str().to_string()),
                fix_hint: Some(format!("Create page '{}' or fix the citation", target)),
            });
        }
    }

    debug!(slug = %slug, issue_count = result.issues.len(), "Source citation validation complete");
    result
}

/// P2-5: Validate back-link symmetry — check that pages with outbound links
/// have corresponding inbound links. Mirrors TS backLinkValidator.
/// For each outbound link from this slug, verifies that the target page
/// has a link record pointing back (either from_slug or to_slug direction).
pub fn validate_back_link_symmetry(engine: &SqliteEngine, slug: &str) -> ValidationResult {
    let mut result = ValidationResult::new(slug);

    let outbound_links = engine.get_links(slug).unwrap_or_default();

    for link in &outbound_links {
        // Check if the target page has any link record involving this slug
        let target_links = engine.get_links(&link.to_slug).unwrap_or_default();
        let has_symmetry = target_links
            .iter()
            .any(|tl| tl.from_slug == slug || tl.to_slug == slug);

        if !has_symmetry {
            result.add_issue(ValidationIssue {
                rule: "back-link-symmetry".to_string(),
                severity: Severity::Info,
                message: format!(
                    "Outbound link to '{}' has no corresponding inbound link",
                    link.to_slug
                ),
                location: Some(format!("{} -> {}", link.from_slug, link.to_slug)),
                fix_hint: Some(format!(
                    "Add a link from '{}' back to '{}'",
                    link.to_slug, slug
                )),
            });
        }
    }

    debug!(slug = %slug, issue_count = result.issues.len(), "Back-link symmetry validation complete");
    result
}

/// Run all validators on content
pub fn validate_all(engine: &SqliteEngine, slug: &str, content: &str) -> ValidationResult {
    info!(slug = %slug, "Running all validators");
    let mut result = validate_slug(slug);

    let back_link_result = validate_back_links(engine, slug, content);
    result.issues.extend(back_link_result.issues);

    let link_result = validate_links(engine, slug, content);
    result.issues.extend(link_result.issues);

    let citation_result = validate_citations(slug, content);
    result.issues.extend(citation_result.issues);

    // P2-4: Source citation validation (checks [Source:slug] refs exist in DB)
    let source_citation_result = validate_source_citations(engine, slug, content);
    result.issues.extend(source_citation_result.issues);

    // P2-5: Back-link symmetry validation (checks outbound links have inbound counterparts)
    let symmetry_result = validate_back_link_symmetry(engine, slug);
    result.issues.extend(symmetry_result.issues);

    let hr_result = validate_triple_hr(slug, content);
    result.issues.extend(hr_result.issues);

    if result.issues.iter().any(|i| i.severity == Severity::Error) {
        result.is_valid = false;
    }

    info!(slug = %slug, total_issues = result.issues.len(), is_valid = result.is_valid, "All validators complete");
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::BrainEngine;
    use crate::types::{PageInput, PageType};

    #[test]
    fn test_validate_slug_valid() {
        let result = validate_slug("people/alice-smith");
        assert!(result.is_valid);
        assert!(result.issues.is_empty());
    }

    #[test]
    fn test_validate_slug_spaces() {
        let result = validate_slug("people/alice smith");
        assert!(!result.is_valid);
        assert!(result.issues.iter().any(|i| i.message.contains("spaces")));
    }

    #[test]
    fn test_validate_slug_uppercase() {
        let result = validate_slug("People/Alice");
        assert!(result
            .issues
            .iter()
            .any(|i| i.message.contains("lowercase")));
    }

    #[test]
    fn test_validate_slug_special_chars() {
        let result = validate_slug("people/alice@smith");
        assert!(!result.is_valid);
    }

    #[test]
    fn test_validate_slug_empty() {
        let result = validate_slug("");
        assert!(!result.is_valid);
    }

    #[test]
    fn test_validate_triple_hr_valid() {
        let content = "---\ntitle: Test\n---\nBody content";
        let result = validate_triple_hr("test/page", content);
        assert!(result.is_valid);
    }

    #[test]
    fn test_validate_triple_hr_unclosed() {
        let content = "---\ntitle: Test\nBody content";
        let result = validate_triple_hr("test/page", content);
        assert!(!result.is_valid);
    }

    #[test]
    fn test_validate_citations_orphan() {
        let content = "See [1] for details.\n\nSome text";
        let result = validate_citations("test/page", content);
        assert!(result.issues.iter().any(|i| i.rule == "citation"));
    }

    #[test]
    fn test_validation_result_helpers() {
        let mut result = ValidationResult::new("test");
        assert!(result.is_valid);
        assert!(result.errors().is_empty());

        result.add_issue(ValidationIssue {
            rule: "test".to_string(),
            severity: Severity::Error,
            message: "error".to_string(),
            location: None,
            fix_hint: None,
        });
        assert!(!result.is_valid);
        assert_eq!(result.errors().len(), 1);

        result.add_issue(ValidationIssue {
            rule: "test".to_string(),
            severity: Severity::Warning,
            message: "warning".to_string(),
            location: None,
            fix_hint: None,
        });
        assert_eq!(result.warnings().len(), 1);
    }

    #[test]
    fn test_validate_source_citations_missing() {
        let content = "According to [Source:people/bob], the project started in 2020.";
        let engine = SqliteEngine::with_config(":memory:", crate::config::Config::default());
        let result = validate_source_citations(&engine, "test/page", content);
        assert!(result.issues.iter().any(|i| i.rule == "source-citation"));
    }

    #[test]
    fn test_validate_back_link_symmetry_no_symmetry() {
        // Create an in-memory engine with a page and a one-way link
        let mut engine = SqliteEngine::with_config(":memory:", crate::config::Config::default());
        engine.connect().unwrap();
        engine.init_schema().unwrap();
        engine
            .put_page(
                "people/alice",
                PageInput {
                    page_type: PageType::Person,
                    title: "Alice".to_string(),
                    compiled_truth: "Knows Bob".to_string(),
                    timeline: None,
                    frontmatter: None,
                    content_hash: None,
                },
            )
            .unwrap();
        engine
            .put_page(
                "people/bob",
                PageInput {
                    page_type: PageType::Person,
                    title: "Bob".to_string(),
                    compiled_truth: "Known by Alice".to_string(),
                    timeline: None,
                    frontmatter: None,
                    content_hash: None,
                },
            )
            .unwrap();
        engine
            .add_link(
                "people/alice",
                "people/bob",
                None,
                Some("knows"),
                Some("markdown"),
                None,
                None,
            )
            .unwrap();

        let result = validate_back_link_symmetry(&engine, "people/alice");
        // Alice -> Bob exists, but Bob -> Alice does not
        assert!(result.issues.iter().any(|i| i.rule == "back-link-symmetry"));
    }
}
