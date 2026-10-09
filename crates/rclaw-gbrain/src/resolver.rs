//! Resolver trait and registry for slug resolution
//! Mirrors gbrain's src/core/resolver.ts
//!
//! P0: DB-backed 4-step slug resolver with fuzzy + keyword fallback + cache
//!
//! Two modes:
//! - Batch: pre-load all slugs into memory, fast lookup (no DB queries)
//! - Live: query database each time, guaranteed freshness, fuzzy + keyword fallback

use crate::engine::BrainEngine;
use crate::sqlite_engine::SqliteEngine;
use crate::types::{PageType, SearchOpts};
use std::collections::HashMap;
use tracing::{debug, trace};

/// Resolver mode — controls how resolvers access data
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverMode {
    /// Pre-load all slugs into memory, fast lookup
    Batch,
    /// Query database each time, guaranteed freshness
    Live,
}

/// Resolver cost tier
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolverCost {
    Free,
    RateLimited,
    Paid,
}

/// A resolved reference
#[derive(Debug, Clone)]
pub struct ResolvedRef {
    pub slug: String,
    pub confidence: f64,
    pub resolver_name: String,
    pub source: String,
    pub cost_estimate: f64,
}

/// Resolver trait — resolves a partial reference to one or more slugs
pub trait Resolver: Send + Sync {
    /// Unique identifier for this resolver
    fn id(&self) -> &str;

    /// Name of this resolver (for debugging/logging)
    fn name(&self) -> &str;

    /// Cost tier of this resolver
    fn cost(&self) -> ResolverCost;

    /// Backend identifier (e.g., "brain-local", "x-api")
    fn backend(&self) -> &str;

    /// Whether this resolver is available given the current context
    fn available(&self, ctx: &ResolverContext) -> bool;

    /// Resolve a partial reference to candidate slugs
    fn resolve(&self, partial: &str, hint: Option<ResolverHint>) -> Vec<ResolvedRef>;
}

/// Context for resolver availability checks
#[derive(Debug, Clone)]
pub struct ResolverContext {
    pub remote: bool,
    pub has_api_key: bool,
    pub deadline: Option<std::time::Instant>,
}

/// Hint for resolver context
#[derive(Debug, Clone)]
pub struct ResolverHint {
    pub page_type: Option<PageType>,
    pub source_slug: Option<String>,
    pub field: Option<String>,
}

/// Registry of resolvers, chained in priority order
pub struct ResolverRegistry {
    resolvers: Vec<Box<dyn Resolver>>,
}

impl ResolverRegistry {
    pub fn new() -> Self {
        Self {
            resolvers: Vec::new(),
        }
    }

    /// Add a resolver (lower index = higher priority)
    pub fn add(&mut self, resolver: Box<dyn Resolver>) {
        self.resolvers.push(resolver);
    }

    /// Resolve a partial reference by chaining all resolvers
    ///
    /// Returns candidates from all resolvers, deduplicated by slug
    /// (keeping the highest-confidence match).
    pub fn resolve(&self, partial: &str, hint: Option<ResolverHint>) -> Vec<ResolvedRef> {
        debug!(partial = %partial, "ResolverRegistry resolving reference");
        let mut best: HashMap<String, ResolvedRef> = HashMap::new();

        for resolver in &self.resolvers {
            let ctx = ResolverContext {
                remote: false,
                has_api_key: false,
                deadline: None,
            };
            if !resolver.available(&ctx) {
                continue;
            }
            let candidates = resolver.resolve(partial, hint.clone());
            for candidate in candidates {
                best.entry(candidate.slug.clone())
                    .and_modify(|existing| {
                        if candidate.confidence > existing.confidence {
                            *existing = candidate.clone();
                        }
                    })
                    .or_insert(candidate);
            }
        }

        // Sort by confidence descending
        let mut results: Vec<ResolvedRef> = best.into_values().collect();
        results.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        debug!(partial = %partial, result_count = results.len(), "ResolverRegistry resolve complete");
        results
    }

    /// Resolve to a single best match (highest confidence)
    pub fn resolve_one(&self, partial: &str, hint: Option<ResolverHint>) -> Option<ResolvedRef> {
        let results = self.resolve(partial, hint);
        results.into_iter().next()
    }

    /// List registered resolvers
    pub fn resolver_names(&self) -> Vec<&str> {
        self.resolvers.iter().map(|r| r.name()).collect()
    }
}

impl Default for ResolverRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// P0: DB-backed Slug Resolver — 4-step resolution chain with fuzzy + keyword fallback + cache
/// Mirrors TS makeResolver():
/// 1. Exact match: input is already a valid slug → get_page
/// 2. Slugify + dir hint: normalize then exact match with dir prefix
/// 3. Fuzzy match: find_by_title_fuzzy (threshold 0.55)
/// 4. (Live only) Keyword search fallback (score >= 0.8)
///    M33: SlugResolver 拥有 SqliteEngine 的所有权而非借用引用。
///
/// 设计决策：当前选择拥有所有权是因为：
/// - Batch 模式需要长期持有 engine 以支持缓存查询，生命周期管理更简单
/// - Resolver 通常独占使用一个 engine 实例，共享收益不大
/// - 未来如果需要多 resolver 共享 engine，可改为 `Arc<SqliteEngine>` 引用计数
pub struct SlugResolver {
    engine: SqliteEngine,
    mode: ResolverMode,
    cache: HashMap<String, Option<String>>,
}

impl SlugResolver {
    pub fn new(engine: SqliteEngine, mode: ResolverMode) -> Self {
        Self {
            engine,
            mode,
            cache: HashMap::new(),
        }
    }

    /// 4-step resolution chain (mirrors TS makeResolver):
    /// Returns a single resolved slug, or None if unresolvable.
    pub fn resolve_slug(&mut self, name: &str, dir_hint: &[&str]) -> Option<String> {
        let cache_key = format!("{}\0{}", name, dir_hint.join(","));
        if let Some(cached) = self.cache.get(&cache_key) {
            return cached.clone();
        }

        // Step 1: Exact match — input is already a valid slug
        let trimmed = name.trim();
        if is_valid_slug(trimmed) && self.engine.get_page(trimmed).ok().flatten().is_some() {
            debug!(name = %name, step = "exact", "Slug resolved");
            self.cache.insert(cache_key, Some(trimmed.to_string()));
            return Some(trimmed.to_string());
        }

        // Step 2: Slugify + dir hint
        let slugified = slugify(trimmed);
        for hint in dir_hint {
            if hint.is_empty() {
                continue;
            }
            let candidate = format!("{}/{}", hint, slugified);
            if self.engine.get_page(&candidate).ok().flatten().is_some() {
                debug!(name = %name, candidate = %candidate, step = "slugify+hint", "Slug resolved");
                self.cache.insert(cache_key, Some(candidate.clone()));
                return Some(candidate);
            }
        }
        // Also try slugified without dir hint
        if self.engine.get_page(&slugified).ok().flatten().is_some() {
            debug!(name = %name, candidate = %slugified, step = "slugify", "Slug resolved");
            self.cache.insert(cache_key, Some(slugified.clone()));
            return Some(slugified);
        }

        // Step 3: Fuzzy match (find_by_title_fuzzy, threshold 0.55)
        let search_hints: Vec<Option<&str>> = if dir_hint.is_empty() {
            vec![None]
        } else {
            dir_hint.iter().map(|h| Some(*h)).collect()
        };
        for hint in search_hints {
            if let Ok(matches) = self
                .engine
                .find_by_title_fuzzy(name, hint, Some(0.55), Some(1))
            {
                if let Some(match_result) = matches.first() {
                    debug!(
                        name = %name,
                        slug = %match_result.slug,
                        score = match_result.score,
                        step = "fuzzy",
                        "Slug resolved"
                    );
                    self.cache
                        .insert(cache_key, Some(match_result.slug.clone()));
                    return Some(match_result.slug.clone());
                }
            }
        }

        // Step 4: (Live only) Keyword search fallback (score >= 0.8)
        if self.mode == ResolverMode::Live {
            if let Ok(results) = self.engine.search_keyword(
                name,
                SearchOpts {
                    limit: Some(3),
                    ..Default::default()
                },
            ) {
                // Find top result with score >= 0.8
                if let Some(top) = results.iter().find(|r| r.score >= 0.8) {
                    // If dir_hint provided, prefer results matching the hint
                    if !dir_hint.is_empty() {
                        if let Some(matched) = results.iter().find(|r| {
                            dir_hint
                                .iter()
                                .any(|h| r.slug.starts_with(&format!("{}/", h)))
                        }) {
                            debug!(
                                name = %name,
                                slug = %matched.slug,
                                step = "keyword",
                                "Slug resolved"
                            );
                            self.cache.insert(cache_key, Some(matched.slug.clone()));
                            return Some(matched.slug.clone());
                        }
                    } else {
                        debug!(
                            name = %name,
                            slug = %top.slug,
                            step = "keyword",
                            "Slug resolved"
                        );
                        self.cache.insert(cache_key, Some(top.slug.clone()));
                        return Some(top.slug.clone());
                    }
                }
            }
        }

        debug!(name = %name, "Slug unresolved");
        self.cache.insert(cache_key, None);
        None
    }
}

/// Check if a string looks like a valid slug (prefix/name[/sub...] format or just name)
/// P1-8: Now allows multi-segment paths (e.g. people/alice/notes) to match validate_page_slug.
fn is_valid_slug(s: &str) -> bool {
    if s.is_empty() {
        return false;
    }
    // Must be lowercase, alphanumeric + hyphens + slash separators
    let lower = s.to_lowercase();
    if lower != s {
        return false; // Not lowercase
    }
    // Check each character
    for c in s.chars() {
        if !c.is_alphanumeric() && c != '-' && c != '/' {
            return false;
        }
    }
    // If contains slash, must be prefix/name[/sub...] format
    if s.contains('/') {
        let parts: Vec<&str> = s.split('/').collect();
        if parts.len() < 2 {
            return false;
        }
        // No empty segments
        if parts.iter().any(|p| p.is_empty()) {
            return false;
        }
    }
    true
}

/// Slugify a string: lowercase, replace spaces with hyphens, strip special chars
fn slugify(s: &str) -> String {
    s.to_lowercase()
        .replace(' ', "-")
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '/')
        .collect()
}

/// Title resolver — resolves by matching against page titles
pub struct TitleResolver {
    /// Map of title → slug
    titles: HashMap<String, String>,
}

impl TitleResolver {
    pub fn new(entries: Vec<(String, String)>) -> Self {
        Self {
            titles: entries.into_iter().collect(),
        }
    }
}

impl Resolver for TitleResolver {
    fn id(&self) -> &str {
        "title-resolver"
    }

    fn name(&self) -> &str {
        "title"
    }

    fn cost(&self) -> ResolverCost {
        ResolverCost::Free
    }

    fn backend(&self) -> &str {
        "brain-local"
    }

    fn available(&self, _ctx: &ResolverContext) -> bool {
        true
    }

    fn resolve(&self, partial: &str, _hint: Option<ResolverHint>) -> Vec<ResolvedRef> {
        let mut results = Vec::new();
        let partial_lower = partial.to_lowercase();
        let mut match_count = 0usize;

        for (title, slug) in &self.titles {
            let title_lower = title.to_lowercase();

            if title_lower == partial_lower {
                match_count += 1;
                results.push(ResolvedRef {
                    slug: slug.clone(),
                    confidence: 0.9,
                    resolver_name: self.name().to_string(),
                    source: "brain-local".to_string(),
                    cost_estimate: 0.0,
                });
            } else if title_lower.contains(&partial_lower) {
                match_count += 1;
                results.push(ResolvedRef {
                    slug: slug.clone(),
                    confidence: 0.4,
                    resolver_name: self.name().to_string(),
                    source: "brain-local".to_string(),
                    cost_estimate: 0.0,
                });
            }
        }

        results.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        results.truncate(10);
        trace!(partial = %partial, match_count, "TitleResolver resolve complete");
        results
    }
}

/// Slug Registry — collision detection and auto-disambiguation
/// Mirrors TS output/slug-registry.ts
///
/// When creating pages, use SlugRegistry to:
/// - Detect slug collisions (slug already exists)
/// - Auto-disambiguate by appending numeric suffix
/// - Check if a slug is free (not yet used)
/// - Suggest disambiguators for colliding slugs
pub struct SlugRegistry<'a> {
    engine: &'a SqliteEngine,
}

impl<'a> SlugRegistry<'a> {
    pub fn new(engine: &'a SqliteEngine) -> Self {
        Self { engine }
    }

    /// Create a slug, handling collisions via auto-disambiguation
    ///
    /// If the desired slug already exists:
    /// - `on_collision::Error` → return None
    /// - `on_collision::AutoDisambiguate` → append numeric suffix (e.g., "people/alice-2")
    /// - `on_collision::Overwrite` → return the desired slug as-is
    pub fn create(
        &self,
        desired_slug: &str,
        _display_name: &str,
        on_collision: CollisionStrategy,
        max_disambiguator: usize,
    ) -> Option<String> {
        // Check if desired slug is free
        if self.is_free(desired_slug) {
            debug!(slug = %desired_slug, "Slug is free, using as-is");
            return Some(desired_slug.to_string());
        }

        match on_collision {
            CollisionStrategy::Error => {
                debug!(slug = %desired_slug, "Slug collision, strategy=Error");
                None
            }
            CollisionStrategy::AutoDisambiguate => {
                // Try suffixes -2, -3, ... up to max_disambiguator
                for i in 2..=max_disambiguator {
                    let candidate = format!("{}-{}", desired_slug, i);
                    if self.is_free(&candidate) {
                        debug!(
                            slug = %desired_slug,
                            candidate = %candidate,
                            "Slug collision resolved via auto-disambiguation"
                        );
                        return Some(candidate);
                    }
                }
                debug!(
                    slug = %desired_slug,
                    max = max_disambiguator,
                    "Slug collision: no free disambiguated slug found"
                );
                None
            }
            CollisionStrategy::Overwrite => {
                debug!(slug = %desired_slug, "Slug collision, strategy=Overwrite");
                Some(desired_slug.to_string())
            }
        }
    }

    /// Check if a slug is free (not yet used by any page)
    pub fn is_free(&self, slug: &str) -> bool {
        match self.engine.get_page(slug) {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    /// Suggest disambiguators for a colliding slug
    ///
    /// Returns up to `n` suggested slug variants that are free.
    /// E.g., for "people/alice" → ["people/alice-2", "people/alice-3"]
    pub fn suggest_disambiguators(&self, slug: &str, n: usize) -> Vec<String> {
        let mut suggestions = Vec::new();
        for i in 2..(n + 10) {
            // Search a bit wider than n
            let candidate = format!("{}-{}", slug, i);
            if self.is_free(&candidate) {
                suggestions.push(candidate);
                if suggestions.len() >= n {
                    break;
                }
            }
        }
        suggestions
    }
}

/// Strategy for handling slug collisions
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionStrategy {
    /// Return error on collision
    Error,
    /// Auto-append numeric suffix
    AutoDisambiguate,
    /// Overwrite existing page
    Overwrite,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_valid_slug() {
        assert!(is_valid_slug("people/alice"));
        assert!(is_valid_slug("alice"));
        assert!(!is_valid_slug("Alice Smith")); // uppercase + space
        assert!(!is_valid_slug("")); // empty
        assert!(!is_valid_slug("people/")); // empty name part
    }

    #[test]
    fn test_slugify() {
        assert_eq!(slugify("Alice Smith"), "alice-smith");
        assert_eq!(slugify("ACME Corp!"), "acme-corp");
    }

    #[test]
    fn test_resolver_cost() {
        let titles = TitleResolver::new(vec![]);
        assert_eq!(titles.cost(), ResolverCost::Free);
        assert_eq!(titles.backend(), "brain-local");
    }

    #[test]
    fn test_registry_chaining() {
        let mut registry = ResolverRegistry::new();
        registry.add(Box::new(TitleResolver::new(vec![(
            "Alice Smith".to_string(),
            "people/alice".to_string(),
        )])));

        let results = registry.resolve("alice", None);
        assert!(!results.is_empty());
    }

    #[test]
    fn test_registry_resolve_one() {
        let mut registry = ResolverRegistry::new();
        registry.add(Box::new(TitleResolver::new(vec![(
            "Alice Smith".to_string(),
            "people/alice".to_string(),
        )])));

        let result = registry.resolve_one("Alice Smith", None);
        assert!(result.is_some());
        assert_eq!(result.unwrap().slug, "people/alice");
    }
}
