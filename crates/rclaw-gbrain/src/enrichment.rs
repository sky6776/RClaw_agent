//! Enrichment service — entity detection, auto-enrich, tier auto-upgrade
//! Mirrors gbrain's src/core/enrichment.ts
//!
//! The enrichment pipeline:
//! 1. Detect entities in page content (mentions of known slugs)
//! 2. Auto-create stub pages for new entities
//! 3. Add backlinks from entity pages to source pages
//! 4. Add timeline entries for significant events
//! 5. Auto-upgrade tiers based on mention count and source quality

use crate::engine::BrainEngine;
use crate::error::Result;
use crate::link_extraction::extract_entity_refs;
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use tracing::{debug, info, trace, warn};

/// Tier classification for pages (mirrors TS enrichment tier rules)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Hash)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Core entities: 8+ mentions, or meeting/voice-note source
    Tier1,
    /// Supporting entities: 3-7 mentions + 2+ sources
    Tier2,
    /// Peripheral: everything else
    Tier3,
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Tier::Tier1 => write!(f, "tier1"),
            Tier::Tier2 => write!(f, "tier2"),
            Tier::Tier3 => write!(f, "tier3"),
        }
    }
}

impl Tier {
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "tier1" | "1" => Self::Tier1,
            "tier2" | "2" => Self::Tier2,
            _ => Self::Tier3,
        }
    }
}

/// Entity candidate detected in text
#[derive(Debug, Clone)]
pub struct EntityCandidate {
    pub name: String,
    pub entity_type: EntityType,
    pub context: String,
}

/// Entity type detected from text patterns
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityType {
    Person,
    Company,
    Unknown,
}

/// Auto-enrich result
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnrichResult {
    pub slug: String,
    pub entities_detected: usize,
    pub stubs_created: usize,
    pub backlinks_added: usize,
}

/// Enrichment result for a single page (legacy)
#[derive(Debug, Clone)]
pub struct EnrichmentResult {
    pub slug: String,
    pub mention_count: usize,
    pub tier: Tier,
    pub suggested_tags: Vec<String>,
    pub suggested_links: Vec<SuggestedLink>,
}

/// A suggested link
#[derive(Debug, Clone)]
pub struct SuggestedLink {
    pub to_slug: String,
    pub link_type: String,
    pub reason: String,
    pub confidence: f64,
}

/// Detect entity candidates in text using capitalization patterns
/// Matches 2-4 consecutive capitalized words, classifying as person or company.
/// P2-11: All regex patterns use OnceLock for lazy one-time compilation.
pub fn extract_entity_candidates(text: &str) -> Vec<EntityCandidate> {
    static CAPS_RE: OnceLock<Regex> = OnceLock::new();
    static COMPANY_SUFFIXES_RE: OnceLock<Regex> = OnceLock::new();
    let re =
        CAPS_RE.get_or_init(|| Regex::new(r"\b([A-Z][a-z]+(?:\s+[A-Z][a-z]+){1,3})\b").unwrap());
    let company_suffixes = COMPANY_SUFFIXES_RE.get_or_init(|| {
        Regex::new(r"(?i)\b(Inc|Corp|Ltd|LLC|Labs?|Tech|AI|Capital|Ventures?|Fund)\b").unwrap()
    });

    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for caps in re.captures_iter(text) {
        let name = caps.get(1).unwrap().as_str().to_string();
        if seen.contains(&name) {
            continue;
        }
        seen.insert(name.clone());

        let entity_type = if company_suffixes.is_match(&name) {
            EntityType::Company
        } else {
            // Heuristic: 2 words → likely person, 3+ words → could be either
            let word_count = name.split_whitespace().count();
            if word_count == 2 {
                EntityType::Person
            } else {
                EntityType::Unknown
            }
        };

        // Capture context window (±80 chars)
        let start = caps.get(0).unwrap().start();
        let mut ctx_start = start.saturating_sub(80);
        // Walk forward to next char boundary to avoid panic on multi-byte UTF-8
        while !text.is_char_boundary(ctx_start) && ctx_start < caps.get(0).unwrap().end() {
            ctx_start += 1;
        }
        let mut ctx_end = caps
            .get(0)
            .unwrap()
            .end()
            .saturating_add(80)
            .min(text.len());
        // Walk backward to nearest char boundary to avoid panic on multi-byte UTF-8
        while !text.is_char_boundary(ctx_end) && ctx_end > ctx_start {
            ctx_end -= 1;
        }
        let context = text[ctx_start..ctx_end].to_string();

        candidates.push(EntityCandidate {
            name,
            entity_type,
            context,
        });
    }

    candidates.truncate(20);
    debug!(
        candidate_count = candidates.len(),
        "Entity candidate extraction complete"
    );
    candidates
}

/// Tier auto-upgrade rules (mirrors TS):
/// - 8+ mentions → Tier1
/// - Meeting or voice-note → Tier1
/// - 3-7 mentions + 2+ sources → Tier2
/// - Default → Tier3
pub fn compute_tier(
    mention_count: usize,
    source_count: usize,
    page_type: Option<PageType>,
) -> Tier {
    let tier = compute_tier_impl(mention_count, source_count, page_type);
    debug!(mention_count, source_count, tier = %tier, "Tier computed");
    tier
}

fn compute_tier_impl(
    mention_count: usize,
    source_count: usize,
    page_type: Option<PageType>,
) -> Tier {
    // Meeting or voice-note → Tier1
    if matches!(page_type, Some(PageType::Meeting) | Some(PageType::Note)) {
        return Tier::Tier1;
    }

    // 8+ mentions → Tier1
    if mention_count >= 8 {
        return Tier::Tier1;
    }

    // 3-7 mentions + 2+ sources → Tier2
    if mention_count >= 3 && source_count >= 2 {
        return Tier::Tier2;
    }

    // Default → Tier3
    Tier::Tier3
}

/// Enrichment service
pub struct EnrichmentService<'a> {
    engine: &'a SqliteEngine,
}

impl<'a> EnrichmentService<'a> {
    pub fn new(engine: &'a SqliteEngine) -> Self {
        Self { engine }
    }

    /// Count how many times a slug is mentioned across the brain.
    /// R3-09: Deduplicates backlinks and keyword search results to avoid
    /// double-counting pages that appear in both sources.
    pub fn count_mentions(&self, slug: &str) -> Result<usize> {
        let backlinks = self.engine.get_backlinks(slug)?;

        let search_slug = slug.replace('-', " ");
        let search_results = self.engine.search_keyword(
            &search_slug,
            SearchOpts {
                limit: Some(100),
                ..Default::default()
            },
        )?;

        // R3-09: Use a single set to deduplicate — a page with both a backlink
        // AND a keyword search hit should only be counted once.
        let mut mentioning_pages = std::collections::HashSet::new();
        // Add backlink sources
        for link in &backlinks {
            if link.from_slug != slug {
                mentioning_pages.insert(link.from_slug.clone());
            }
        }
        // Add keyword search hits
        for result in &search_results {
            if result.slug != slug {
                mentioning_pages.insert(result.slug.clone());
            }
        }

        let count = mentioning_pages.len();
        debug!(slug = %slug, mention_count = count, "Mention count computed");
        Ok(count)
    }

    /// Suggest a tier classification based on mention count (legacy)
    pub fn suggest_tier(mention_count: usize) -> Tier {
        compute_tier(mention_count, 0, None)
    }

    /// Enrich a single page: compute tier, suggest tags and links
    pub fn enrich(&self, slug: &str) -> Result<EnrichmentResult> {
        debug!(slug = %slug, "Enriching page");

        let mention_count = self.count_mentions(slug)?;

        let page = self.engine.get_page(slug)?;
        // Use compute_tier with source_count and page_type instead of legacy suggest_tier
        // which hardcodes source_count=0 and page_type=None, making Tier2 unreachable
        let source_count = page
            .as_ref()
            .map(|p| {
                // Count backlinks as a proxy for source count
                self.engine
                    .get_backlinks(&p.slug)
                    .map(|l| l.len())
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let page_type = page.as_ref().map(|p| p.page_type.clone());
        let tier = compute_tier(mention_count, source_count, page_type);
        let suggested_tags = page
            .as_ref()
            .map(|p| self.suggest_tags_from_content(&p.compiled_truth, &p.page_type))
            .unwrap_or_default();

        let suggested_links = page
            .as_ref()
            .map(|p| self.suggest_links_from_content(slug, &p.compiled_truth))
            .unwrap_or_default();

        Ok(EnrichmentResult {
            slug: slug.to_string(),
            mention_count,
            tier,
            suggested_tags,
            suggested_links,
        })
    }

    /// Auto-enrich: detect entities, create stubs, add backlinks
    pub fn auto_enrich(&self, slug: &str, content: &str) -> Result<EnrichResult> {
        info!(slug = %slug, "Starting auto_enrich");
        let mut result = EnrichResult {
            slug: slug.to_string(),
            entities_detected: 0,
            stubs_created: 0,
            backlinks_added: 0,
        };

        // Detect entities using existing link extraction
        let refs = extract_entity_refs(content);
        result.entities_detected = refs.len();

        // Load config once — auto_link gates both stub creation and backlink addition
        let config = crate::config::Config::load()
            .map_err(|e| crate::error::GBrainError::Config(e.to_string()))?;
        if !config.auto_link {
            debug!(slug = %slug, "auto_link disabled, skipping auto_enrich");
            return Ok(result);
        }

        for entity_ref in &refs {
            // Create stub if page doesn't exist
            let existing = self.engine.get_page(&entity_ref.slug)?;
            if existing.is_none() {
                let stub_content = format!(
                    "# {}\n\n*Auto-generated stub from reference in [[{}]]*\n",
                    entity_ref.display_name, slug
                );
                let ops = crate::operations::Operations::with_config(
                    self.engine,
                    crate::operations::OpContext {
                        remote: true,
                        ..Default::default()
                    },
                    config.clone(),
                );
                match ops.put_page(
                    &entity_ref.slug,
                    &entity_ref.display_name,
                    &stub_content,
                    None,
                    None,
                ) {
                    Ok(_) => {
                        result.stubs_created += 1;
                        info!(stub_slug = %entity_ref.slug, source = %slug, "Created stub page");
                    }
                    Err(e) => {
                        warn!(stub_slug = %entity_ref.slug, error = %e, "Failed to create stub");
                    }
                }
            }

            // Add backlink (skip if already exists to prevent duplicates on re-enrichment)
            let existing_links = self.engine.get_links(&entity_ref.slug)?;
            let already_linked = existing_links
                .iter()
                .any(|l| l.to_slug == slug && l.link_type == "mentioned_in");
            if !already_linked {
                let backlink = LinkBatchInput {
                    from_slug: entity_ref.slug.clone(),
                    to_slug: slug.to_string(),
                    link_type: Some("mentioned_in".to_string()),
                    context: Some(entity_ref.display_name.clone()),
                    link_source: Some(LinkSource::Frontmatter),
                    origin_slug: Some(slug.to_string()),
                    origin_field: None,
                    direction: Some(LinkDirection::Incoming),
                };
                match self.engine.add_links_batch(&[backlink]) {
                    Ok(n) => result.backlinks_added += n,
                    Err(e) => warn!(error = %e, "Failed to add backlink"),
                }
            }
        }

        info!(slug = %slug, entities_detected = result.entities_detected, stubs_created = result.stubs_created, backlinks_added = result.backlinks_added, "Auto-enrich complete");
        Ok(result)
    }

    /// Enrich all pages and return results
    pub fn enrich_all(&self) -> Result<Vec<EnrichmentResult>> {
        let slugs = self.engine.get_all_slugs()?;
        let mut results = Vec::new();

        for slug in &slugs {
            match self.enrich(slug) {
                Ok(result) => results.push(result),
                Err(e) => {
                    warn!(slug = %slug, error = %e, "Failed to enrich page");
                }
            }
        }

        info!(count = results.len(), "Enrichment complete");
        Ok(results)
    }

    /// Suggest tags based on content and page type
    /// P2-11: Regex patterns use OnceLock for lazy one-time compilation.
    fn suggest_tags_from_content(&self, content: &str, page_type: &PageType) -> Vec<String> {
        static YEAR_RE: OnceLock<Regex> = OnceLock::new();
        let year_pattern = YEAR_RE.get_or_init(|| Regex::new(r"\b(20\d{2})\b").unwrap());

        let mut tags = Vec::new();
        tags.push(page_type.to_string());

        let content_lower = content.to_lowercase();

        for cap in year_pattern.captures_iter(&content_lower) {
            let year = cap.get(1).unwrap().as_str();
            tags.push(format!("year-{}", year));
        }

        let status_keywords = [
            ("active", "status-active"),
            ("inactive", "status-inactive"),
            ("archived", "status-archived"),
            ("draft", "status-draft"),
        ];
        for (keyword, tag) in &status_keywords {
            if content_lower.contains(keyword) {
                tags.push(tag.to_string());
            }
        }

        tags.sort();
        tags.dedup();
        tags.truncate(10);
        tags
    }

    /// Suggest links based on content analysis
    /// P2-11: Regex patterns use OnceLock for lazy one-time compilation.
    fn suggest_links_from_content(&self, _from_slug: &str, content: &str) -> Vec<SuggestedLink> {
        static WIKILINK_RE: OnceLock<Regex> = OnceLock::new();
        static MD_LINK_RE: OnceLock<Regex> = OnceLock::new();
        let mut links = Vec::new();

        let wikilink_re = WIKILINK_RE.get_or_init(|| Regex::new(r"\[\[([^\]]+)\]\]").unwrap());
        for cap in wikilink_re.captures_iter(content) {
            let target = cap.get(1).unwrap().as_str().to_string();
            links.push(SuggestedLink {
                to_slug: target,
                link_type: "mentions".to_string(),
                reason: "wikilink in content".to_string(),
                confidence: 0.9,
            });
        }

        let md_link_re = MD_LINK_RE.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
        for cap in md_link_re.captures_iter(content) {
            let text = cap.get(1).unwrap().as_str();
            let target = cap.get(2).unwrap().as_str().to_string();

            if target.starts_with("http") {
                continue;
            }

            links.push(SuggestedLink {
                to_slug: target,
                link_type: "mentions".to_string(),
                reason: format!("markdown link '{}' in content", text),
                confidence: 0.8,
            });
        }

        let mut seen = std::collections::HashSet::new();
        links.retain(|l| seen.insert(l.to_slug.clone()));
        links.truncate(20);
        trace!(link_count = links.len(), "Suggested links from content");
        links
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_tier_tier1_high_mentions() {
        assert_eq!(compute_tier(8, 1, None), Tier::Tier1);
        assert_eq!(compute_tier(15, 5, None), Tier::Tier1);
    }

    #[test]
    fn test_compute_tier_tier1_meeting() {
        assert_eq!(compute_tier(1, 0, Some(PageType::Meeting)), Tier::Tier1);
    }

    #[test]
    fn test_compute_tier_tier2() {
        assert_eq!(compute_tier(3, 2, None), Tier::Tier2);
        assert_eq!(compute_tier(7, 3, None), Tier::Tier2);
    }

    #[test]
    fn test_compute_tier_tier3() {
        assert_eq!(compute_tier(1, 0, None), Tier::Tier3);
        assert_eq!(compute_tier(2, 1, None), Tier::Tier3);
    }

    #[test]
    fn test_extract_entity_candidates() {
        let text = "Alice Smith met with Sequoia Capital yesterday.";
        let candidates = extract_entity_candidates(text);
        assert!(!candidates.is_empty());
        assert!(candidates
            .iter()
            .any(|c| c.name == "Alice Smith" && c.entity_type == EntityType::Person));
        assert!(candidates
            .iter()
            .any(|c| c.name.contains("Sequoia") && c.entity_type == EntityType::Company));
    }

    #[test]
    fn test_suggest_tier_legacy() {
        assert_eq!(EnrichmentService::suggest_tier(25), Tier::Tier1);
        assert_eq!(EnrichmentService::suggest_tier(3), Tier::Tier3);
    }

    #[test]
    fn test_tier_display() {
        assert_eq!(format!("{}", Tier::Tier1), "tier1");
        assert_eq!(format!("{}", Tier::Tier2), "tier2");
        assert_eq!(format!("{}", Tier::Tier3), "tier3");
    }

    #[test]
    fn test_tier_from_str() {
        assert_eq!(Tier::from_str_lossy("tier1"), Tier::Tier1);
        assert_eq!(Tier::from_str_lossy("tier2"), Tier::Tier2);
        assert_eq!(Tier::from_str_lossy("tier3"), Tier::Tier3);
    }
}
