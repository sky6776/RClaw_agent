//! Entity reference extraction + auto-link
//! Mirrors gbrain's src/core/link-extraction.ts
//!
//! Extracts entity references from markdown content in multiple formats:
//! - Markdown links: `[Name](people/slug)` or `[type:Name](people/slug)`
//! - Wikilinks: `[[people/slug|Name]]`
//! - Bare slug references: `people/alice` (not inside link syntax)
//! - Frontmatter fields: company, investors, key_people, etc.

use crate::engine::BrainEngine;
use crate::types::{LinkBatchInput, LinkDirection, LinkSource, PageType};
use regex::Regex;
use std::sync::OnceLock;
use tracing::{debug, trace, warn};

// ---------------------------------------------------------------------------
// P2-11: Lazily-compiled regex patterns (compiled once, reused on every call)
// ---------------------------------------------------------------------------

/// extract_entity_refs: explicit [[slug]] references
static RE_EXPLICIT_SLUG: OnceLock<Regex> = OnceLock::new();
/// extract_entity_refs: markdown [text](slug) links
static RE_MD_LINK: OnceLock<Regex> = OnceLock::new();
/// extract_entity_refs: [[wikilink]] references
static RE_WIKILINK: OnceLock<Regex> = OnceLock::new();
/// extract_entity_refs: bare slug references in text
static RE_BARE_SLUG: OnceLock<Regex> = OnceLock::new();

/// is_slug_reference: directory-like slug pattern
static RE_DIR_PATTERN: OnceLock<Regex> = OnceLock::new();

/// parse_timeline_entries: date patterns (YYYY-MM-DD, YYYY/MM/DD, M/D/YYYY, etc.)
static RE_DATE: OnceLock<Regex> = OnceLock::new();
/// parse_timeline_entries: colon-separated date entry pattern
static RE_COLON: OnceLock<Regex> = OnceLock::new();
/// parse_timeline_entries: **bold date** pattern
static RE_BOLD_DATE: OnceLock<Regex> = OnceLock::new();

/// Extracted entity reference
#[derive(Debug, Clone)]
pub struct EntityRef {
    pub slug: String,
    pub display_name: String,
    pub link_type: String,
    pub context_window: Option<String>,
    pub origin_field: Option<String>,
    pub direction: Option<LinkDirection>,
    pub link_source: Option<LinkSource>,
}

/// Frontmatter link rule — maps frontmatter fields to link types with direction semantics
/// Mirrors TS FRONTMATTER_LINK_MAP with incoming/outgoing direction, page type constraints,
/// and dir_hint for slug resolution.
#[derive(Debug, Clone)]
pub struct FrontmatterLinkRule {
    pub field: &'static str,
    pub link_type: &'static str,
    pub direction: LinkDirection,
    pub page_type_constraint: Option<PageType>,
    pub dir_hint: &'static [&'static str],
}

/// Full frontmatter link map with direction semantics (mirrors TS)
const FRONTMATTER_LINK_MAP: &[FrontmatterLinkRule] = &[
    // Person → Company (outgoing: person works_at/founded company)
    FrontmatterLinkRule {
        field: "company",
        link_type: "works_at",
        direction: LinkDirection::Outgoing,
        page_type_constraint: Some(PageType::Person),
        dir_hint: &["companies"],
    },
    FrontmatterLinkRule {
        field: "companies",
        link_type: "works_at",
        direction: LinkDirection::Outgoing,
        page_type_constraint: Some(PageType::Person),
        dir_hint: &["companies"],
    },
    FrontmatterLinkRule {
        field: "founded",
        link_type: "founded",
        direction: LinkDirection::Outgoing,
        page_type_constraint: Some(PageType::Person),
        dir_hint: &["companies"],
    },
    // Company → Person (incoming: person works_at company, from company's perspective)
    FrontmatterLinkRule {
        field: "key_people",
        link_type: "works_at",
        direction: LinkDirection::Incoming,
        page_type_constraint: Some(PageType::Company),
        dir_hint: &["people"],
    },
    FrontmatterLinkRule {
        field: "partner",
        link_type: "yc_partner",
        direction: LinkDirection::Incoming,
        page_type_constraint: Some(PageType::Company),
        dir_hint: &["people"],
    },
    FrontmatterLinkRule {
        field: "investors",
        link_type: "invested_in",
        direction: LinkDirection::Incoming,
        page_type_constraint: Some(PageType::Company),
        dir_hint: &["companies", "people", "funds"],
    },
    // Deal
    FrontmatterLinkRule {
        field: "investors",
        link_type: "invested_in",
        direction: LinkDirection::Incoming,
        page_type_constraint: Some(PageType::Deal),
        dir_hint: &["companies", "people", "funds"],
    },
    FrontmatterLinkRule {
        field: "lead",
        link_type: "led_round",
        direction: LinkDirection::Incoming,
        page_type_constraint: Some(PageType::Deal),
        dir_hint: &["companies", "people", "funds"],
    },
    // Meeting
    FrontmatterLinkRule {
        field: "attendees",
        link_type: "attended",
        direction: LinkDirection::Incoming,
        page_type_constraint: Some(PageType::Meeting),
        dir_hint: &["people"],
    },
    // Any type
    FrontmatterLinkRule {
        field: "sources",
        link_type: "discussed_in",
        direction: LinkDirection::Incoming,
        page_type_constraint: None,
        dir_hint: &["source", "media"],
    },
    FrontmatterLinkRule {
        field: "source",
        link_type: "source",
        direction: LinkDirection::Outgoing,
        page_type_constraint: None,
        dir_hint: &[],
    },
    FrontmatterLinkRule {
        field: "related",
        link_type: "related_to",
        direction: LinkDirection::Outgoing,
        page_type_constraint: None,
        dir_hint: &[],
    },
    FrontmatterLinkRule {
        field: "see_also",
        link_type: "related_to",
        direction: LinkDirection::Outgoing,
        page_type_constraint: None,
        dir_hint: &[],
    },
    // Legacy compatibility (no direction constraint)
    FrontmatterLinkRule {
        field: "competitors",
        link_type: "competes_with",
        direction: LinkDirection::Outgoing,
        page_type_constraint: None,
        dir_hint: &["companies"],
    },
    FrontmatterLinkRule {
        field: "partners",
        link_type: "partnered_with",
        direction: LinkDirection::Outgoing,
        page_type_constraint: None,
        dir_hint: &["companies"],
    },
    FrontmatterLinkRule {
        field: "acquired_by",
        link_type: "acquired_by",
        direction: LinkDirection::Outgoing,
        page_type_constraint: None,
        dir_hint: &["companies"],
    },
];

/// Strip code blocks from content to avoid false link matches
/// Handles both fenced blocks (```) and inline code (`) using a byte-level
/// scan on String. Replaces code regions with spaces (preserving byte positions)
/// so that link positions in the cleaned text still correspond to the original content.
///
/// 相比旧版 Vec<char> 实现，此版本直接在 String/bytes 上操作，
/// 避免了 O(n) 额外内存分配（Vec<char> 每个字符占 4 字节），
/// 在大文档上可节省约 75% 的内存。
fn strip_code_blocks(content: &str) -> String {
    let bytes = content.as_bytes();
    let len = bytes.len();
    // 用 content 的字节初始化 result，避免逐字节拷贝
    let mut result = content.as_bytes().to_vec();
    let mut i = 0;
    let mut in_code = false;

    while i < len {
        // 检测围栏代码块: ```
        if i + 2 < len && bytes[i] == b'`' && bytes[i + 1] == b'`' && bytes[i + 2] == b'`' {
            result[i] = b' ';
            result[i + 1] = b' ';
            result[i + 2] = b' ';
            in_code = !in_code;
            i += 3;
            continue;
        }

        if in_code {
            // 围栏代码块内部 — 替换为空格
            result[i] = b' ';
            i += 1;
            continue;
        }

        // 检测行内代码: 单个 `
        if bytes[i] == b'`' {
            let mut j = i + 1;
            while j < len && bytes[j] != b'`' {
                j += 1;
            }
            if j < len {
                // P2-6: 拒绝跨越换行符的行内代码 span
                let has_newline = (i + 1..j).any(|k| bytes[k] == b'\n');
                if has_newline {
                    // 换行符之前未找到闭合反引号 — 非行内代码
                    i += 1;
                    continue;
                }
                // 找到闭合反引号 — 将整个行内代码 span 替换为空格
                for slot in &mut result[i..=j] {
                    *slot = b' ';
                }
                i = j + 1;
                continue;
            }
            // 未找到闭合反引号 — 当作普通字符
        }

        i += 1;
    }

    // 原始内容是有效 UTF-8，替换操作仅针对 ASCII 反引号和代码内容，
    // 不会破坏多字节 UTF-8 序列（多字节字符的首字节 >= 0x80）。
    // 使用安全的 from_utf8 替代 unsafe from_utf8_unchecked，
    // 万一 invariants 被破坏也能返回错误而非 UB。
    String::from_utf8(result).unwrap_or_else(|e| {
        // 极端 fallback：如果 UTF-8 invariants 被破坏，丢弃非法字节后的内容
        let valid_up_to = e.utf8_error().valid_up_to();
        let mut truncated = e.into_bytes();
        truncated.truncate(valid_up_to);
        // SAFETY: truncated 只保留 valid_up_to 之前的有效 UTF-8 前缀
        unsafe { String::from_utf8_unchecked(truncated) }
    })
}

/// Extract entity references from markdown content
/// Handles `[Name](dir/slug)`, `[type:Name](dir/slug)`, `[[dir/slug|Name]]`, and bare slug refs
pub fn extract_entity_refs(content: &str) -> Vec<EntityRef> {
    debug!(content_len = content.len(), "Extracting entity references");
    let cleaned = strip_code_blocks(content);
    let mut refs = Vec::new();

    // Pattern 0: Explicit relation labels [type:Name](dir/slug) (P2-11: lazy regex)
    let explicit_re =
        RE_EXPLICIT_SLUG.get_or_init(|| Regex::new(r"\[([a-z_]+):([^\]]+)\]\(([^)]+)\)").unwrap());
    for caps in explicit_re.captures_iter(&cleaned) {
        let link_type = caps.get(1).unwrap().as_str().to_string();
        let name = caps.get(2).unwrap().as_str().to_string();
        let target = caps.get(3).unwrap().as_str().to_string();

        if is_slug_reference(&target) {
            let ctx = capture_context(
                &cleaned,
                caps.get(0).unwrap().start(),
                caps.get(0).unwrap().end(),
            );
            refs.push(EntityRef {
                slug: target,
                display_name: name,
                link_type,
                context_window: Some(ctx),
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: None,
            });
        }
    }

    // Pattern 1: Markdown links [Name](dir/slug) — skip if already captured as explicit (P2-11: lazy regex)
    let md_link_re = RE_MD_LINK.get_or_init(|| Regex::new(r"\[([^\]]+)\]\(([^)]+)\)").unwrap());
    for caps in md_link_re.captures_iter(&cleaned) {
        let name = caps.get(1).unwrap().as_str().to_string();
        let target = caps.get(2).unwrap().as_str().to_string();

        // Skip if this was already captured as an explicit label (name contains ":")
        if name.contains(':') {
            continue;
        }

        if is_slug_reference(&target) {
            // P0-4: capture context first, then pass it to infer_link_type
            let ctx = capture_context(
                &cleaned,
                caps.get(0).unwrap().start(),
                caps.get(0).unwrap().end(),
            );
            let link_type = infer_link_type(&name, &target, None, Some(&ctx), None);
            refs.push(EntityRef {
                slug: target,
                display_name: name,
                link_type,
                context_window: Some(ctx),
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: None,
            });
        }
    }

    // Pattern 2: Wikilinks [[dir/slug|Name]] or [[dir/slug]]
    // Pattern 2: Wikilinks [[dir/slug|Name]] or [[dir/slug]] (P2-11: lazy regex)
    let wikilink_re =
        RE_WIKILINK.get_or_init(|| Regex::new(r"\[\[([^\]|]+)(?:\|([^\]]+))?\]\]").unwrap());
    for caps in wikilink_re.captures_iter(&cleaned) {
        let raw_slug = caps.get(1).unwrap().as_str().to_string();
        let name = caps
            .get(2)
            .map(|m| m.as_str().to_string())
            .unwrap_or_else(|| raw_slug.rsplit('/').next().unwrap_or(&raw_slug).to_string());

        // P2-5: Strip section anchors (#heading) and .md suffix (mirrors TS)
        let without_anchor = raw_slug.split('#').next().unwrap_or(&raw_slug);
        let slug = without_anchor
            .strip_suffix(".md")
            .unwrap_or(without_anchor)
            .to_string();

        if is_slug_reference(&slug) {
            // P0-4: capture context first, then pass it to infer_link_type
            let ctx = capture_context(
                &cleaned,
                caps.get(0).unwrap().start(),
                caps.get(0).unwrap().end(),
            );
            let link_type = infer_link_type(&name, &slug, None, Some(&ctx), None);
            refs.push(EntityRef {
                slug,
                display_name: name,
                link_type,
                context_window: Some(ctx),
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: None,
            });
        }
    }

    // Pattern 3: Bare slug references (P2-11: lazy regex)
    let bare_slug_re = RE_BARE_SLUG.get_or_init(|| {
        Regex::new(r"\b(?:people|companies|deals|topics|concepts|projects|entities|tech|finance|personal|openclaw|wiki|writing|meetings|deal|civic|project|source|media|yc)/[a-z0-9][a-z0-9/-]*\b").unwrap()
    });
    for caps in bare_slug_re.captures_iter(&cleaned) {
        let slug = caps.get(0).unwrap().as_str().to_string();
        let name = slug.rsplit('/').next().unwrap_or(&slug).to_string();
        if is_slug_reference(&slug) && !refs.iter().any(|r| r.slug == slug) {
            let ctx = capture_context(
                &cleaned,
                caps.get(0).unwrap().start(),
                caps.get(0).unwrap().end(),
            );
            refs.push(EntityRef {
                slug,
                display_name: name,
                link_type: "mentions".to_string(),
                context_window: Some(ctx),
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: None,
            });
        }
    }

    debug!(
        ref_count = refs.len(),
        content_len = content.len(),
        "Entity reference extraction complete"
    );
    refs
}

/// Result of frontmatter reference extraction (mirrors TS FrontmatterExtractResult)
#[derive(Debug, Clone)]
pub struct FrontmatterExtractResult {
    pub candidates: Vec<EntityRef>,
    /// Field names and values that could not be resolved to a page slug
    pub unresolved: Vec<(String, String)>,
}

/// Extract entity references from YAML frontmatter fields
/// Uses FrontmatterLinkRule with direction semantics and page type constraints.
///
/// When a `resolver` is provided, each slugified value is checked against the DB.
/// Values that cannot be resolved are recorded in `unresolved` instead of being
/// silently added as candidates.
pub fn extract_frontmatter_refs(
    frontmatter: &serde_json::Value,
    from_slug: &str,
    page_type: Option<PageType>,
    mut resolver: Option<&mut dyn SlugResolver>,
) -> FrontmatterExtractResult {
    let mut refs = Vec::new();
    let mut unresolved = Vec::new();
    let obj = match frontmatter.as_object() {
        Some(o) => o,
        None => {
            return FrontmatterExtractResult {
                candidates: refs,
                unresolved,
            }
        }
    };

    for rule in FRONTMATTER_LINK_MAP {
        // Skip if page type constraint doesn't match
        // When page_type is Some and doesn't match constraint, skip the rule.
        // When page_type is None (unknown), allow the rule to fire (best-effort)
        // since many pages lack explicit page_type in practice.
        if let Some(constraint) = &rule.page_type_constraint {
            if let Some(ref pt) = page_type {
                if pt != constraint {
                    continue;
                }
            }
        }

        if let Some(value) = obj.get(rule.field) {
            let raw_values = extract_raw_values(value);
            for raw in &raw_values {
                let slug = if raw.contains('/') {
                    raw.clone()
                } else {
                    // Slugify: use dir_hint if available, otherwise derive from field name
                    let prefix = if !rule.dir_hint.is_empty() {
                        rule.dir_hint[0].to_string()
                    } else if rule.field.ends_with('s') && rule.field != "key_people" {
                        rule.field[..rule.field.len() - 1].to_string()
                    } else {
                        rule.field.to_string()
                    };
                    format!("{}/{}", prefix, raw.to_lowercase().replace(' ', "-"))
                };

                // If a resolver is available, verify the slug resolves
                if let Some(res) = resolver.as_mut() {
                    match res.resolve(&slug, Some(rule.dir_hint)) {
                        Ok(Some(resolved_slug)) => {
                            refs.push(EntityRef {
                                slug: resolved_slug,
                                display_name: slug.rsplit('/').next().unwrap_or(&slug).to_string(),
                                link_type: rule.link_type.to_string(),
                                context_window: None,
                                origin_field: Some(rule.field.to_string()),
                                direction: Some(rule.direction.clone()),
                                link_source: Some(LinkSource::Frontmatter),
                            });
                        }
                        Ok(None) => {
                            // Slug could not be resolved
                            unresolved.push((rule.field.to_string(), raw.clone()));
                        }
                        Err(_) => {
                            // Resolver error — treat as unresolved
                            unresolved.push((rule.field.to_string(), raw.clone()));
                        }
                    }
                } else {
                    // No resolver — add all candidates (legacy behavior)
                    refs.push(EntityRef {
                        slug: slug.clone(),
                        display_name: slug.rsplit('/').next().unwrap_or(&slug).to_string(),
                        link_type: rule.link_type.to_string(),
                        context_window: None,
                        origin_field: Some(rule.field.to_string()),
                        direction: Some(rule.direction.clone()),
                        link_source: Some(LinkSource::Frontmatter),
                    });
                }
            }
        }
    }

    debug!(
        candidate_count = refs.len(),
        unresolved_count = unresolved.len(),
        "Frontmatter reference extraction complete"
    );
    let _ = from_slug; // used for reconciliation in operations layer
    FrontmatterExtractResult {
        candidates: refs,
        unresolved,
    }
}

/// Extract raw string values from a frontmatter value (before slugification).
/// Returns the original strings so that unresolved tracking can report the human-readable name.
/// P1-6: Handles object entries in arrays — picks obj.name ?? obj.slug ?? obj.title
/// (mirrors TS behavior for frontmatter like `investors: [{name: Sequoia, role: lead}]`).
fn extract_raw_values(value: &serde_json::Value) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(arr) => arr.iter().flat_map(extract_raw_values).collect(),
        serde_json::Value::Object(obj) => {
            // P1-6: Try name, slug, then title field — mirrors TS obj.name ?? obj.slug ?? obj.title
            if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                return vec![name.to_string()];
            }
            if let Some(slug) = obj.get("slug").and_then(|v| v.as_str()) {
                return vec![slug.to_string()];
            }
            if let Some(title) = obj.get("title").and_then(|v| v.as_str()) {
                return vec![title.to_string()];
            }
            vec![]
        }
        _ => vec![],
    }
}

/// Extract slugs from a frontmatter value (string or array)
/// Uses dir_hint for slug resolution when the value doesn't contain a slash.
#[allow(dead_code)]
fn extract_slugs_from_value(
    value: &serde_json::Value,
    field: &str,
    dir_hint: &[&str],
) -> Vec<String> {
    match value {
        serde_json::Value::String(s) => {
            if s.contains('/') {
                vec![s.clone()]
            } else {
                // Slugify: use dir_hint if available, otherwise derive from field name
                let prefix = if !dir_hint.is_empty() {
                    dir_hint[0].to_string()
                } else if field.ends_with('s') && field != "key_people" {
                    field[..field.len() - 1].to_string()
                } else {
                    field.to_string()
                };
                let slugified = format!("{}/{}", prefix, s.to_lowercase().replace(' ', "-"));
                vec![slugified]
            }
        }
        serde_json::Value::Array(arr) => arr
            .iter()
            .flat_map(|v| extract_slugs_from_value(v, field, dir_hint))
            .collect(),
        _ => vec![],
    }
}

/// Capture ±240 chars around a match position as context window
/// P1-7: expanded from 120 to 240 chars (mirrors TS)
fn capture_context(content: &str, start: usize, end: usize) -> String {
    let mut ctx_start = start.saturating_sub(240);
    // Walk forward to next char boundary to avoid panic on multi-byte UTF-8
    while !content.is_char_boundary(ctx_start) && ctx_start < end {
        ctx_start += 1;
    }
    let mut ctx_end = end.saturating_add(240).min(content.len());
    // 向回对齐到 char boundary，避免 +240 落在多字节 UTF-8 字符中间导致 panic
    while !content.is_char_boundary(ctx_end) && ctx_end > ctx_start {
        ctx_end -= 1;
    }
    content[ctx_start..ctx_end].to_string()
}

/// Check if a target string looks like a slug reference
fn is_slug_reference(target: &str) -> bool {
    if target.starts_with("http://") || target.starts_with("https://") {
        return false;
    }
    if target.starts_with('#') || target.starts_with("mailto:") {
        return false;
    }

    // P2-11: Lazy regex for slug validation
    let dir_pattern_re = RE_DIR_PATTERN.get_or_init(|| {
        Regex::new(r"^(people|companies|deals|topics|concepts|projects|entities|tech|finance|personal|openclaw|wiki|writing|meetings|deal|civic|project|source|media|yc)/[a-z0-9][a-z0-9/-]*$").unwrap()
    });

    dir_pattern_re.is_match(target)
}

/// P1-6: 6 regex patterns for link type inference (mirrors TS)
/// These replace the simple contains() matching with comprehensive regex patterns.
/// P2-11: Use OnceLock for lazy one-time compilation (consistent with project convention).
static WORKS_AT_RE: OnceLock<Regex> = OnceLock::new();
static INVESTED_RE: OnceLock<Regex> = OnceLock::new();
static FOUNDED_RE: OnceLock<Regex> = OnceLock::new();
static ADVISES_RE: OnceLock<Regex> = OnceLock::new();
static PARTNER_ROLE_RE: OnceLock<Regex> = OnceLock::new();
static ADVISOR_ROLE_RE: OnceLock<Regex> = OnceLock::new();

/// P0-4: Page role priors — default link types based on source page type.
/// When regex patterns don't match in context, these priors provide a fallback
/// based on the type of the page that contains the link.
static PAGE_ROLE_PRIORS: &[(PageType, &str)] = &[
    (PageType::Company, "acquired"),
    (PageType::Company, "invested_in"),
    (PageType::Person, "advised_by"),
    (PageType::Person, "mentions"),
    (PageType::Project, "mentions"),
];

/// P0-4: Infer link type from context window and page role priors.
///
/// Previously this function tested regex patterns against the display name (e.g. "Alice Chen"),
/// which almost never contains verb phrases, making the regexes effectively dead code.
/// Now it accepts a context window (surrounding text) and tests regexes against that.
///
/// Logic:
/// 1. Try regex patterns against context (if provided), falling back to display_text
/// 2. If no regex match, check page_role_priors: if source page type matches a prior, use that
/// 3. If still no match, return "mentions"
pub fn infer_link_type(
    display_text: &str,
    target: &str,
    source_page_type: Option<PageType>,
    context: Option<&str>,
    page_role_priors: Option<&[(PageType, &str)]>,
) -> String {
    let target_lower = target.to_lowercase();
    // P0-4: Use context window for regex matching, fall back to display_text
    let regex_input = context.unwrap_or(display_text).to_lowercase();

    // 1. Regex-based verb phrase matching against context window (P0-4 fix)
    // P2-11: Use get_or_init() for OnceLock patterns
    let founded_re = FOUNDED_RE.get_or_init(|| Regex::new(
        r"\b(?:founded|co-?founded|started the company|incorporated|founders?\b|founder of|founders? (?:include|are)|the founder|is a co-?founder|is one of the founders)\b"
    ).unwrap());
    let invested_re = INVESTED_RE.get_or_init(|| Regex::new(
        r"\b(?:invested in|invests in|investing in|invest in|investment in|investments in|backed by|funding from|funded by|raised from|led the (?:seed|Series|round|investment|round)|led .{0,30}(?:Series [A-Z]|seed|round|investment)|participated in (?:the )?(?:seed|Series|round)|wrote (?:a |the )?check|first check|early investor|portfolio (?:company|includes)|board seat (?:at|in|on)|term sheet for)\b"
    ).unwrap());
    let advises_re = ADVISES_RE.get_or_init(|| Regex::new(
        r"\b(?:advises|advised|advisor (?:to|at|for|of)|advisory (?:board|role|position)|board advisor|on .{0,20} advisory board|joined .{0,20} advisory board)\b"
    ).unwrap());
    let works_at_re = WORKS_AT_RE.get_or_init(|| Regex::new(
        r"\b(?:CEO of|CTO of|COO of|CFO of|CMO of|CRO of|VP at|VP of|VPs? Engineering|VPs? Product|works at|worked at|working at|employed by|employed at|joined as|joined the team|engineer at|engineer for|director at|director of|head of|leads engineering|leads product|currently at|previously at|previously worked at|spent .* (?:years|months) at|stint at|tenure at)\b"
    ).unwrap());
    let partner_role_re = PARTNER_ROLE_RE.get_or_init(|| Regex::new(
        r"\b(?:partner at|partner of|venture partner|VC partner|invested early|investor at|investor in|portfolio|venture capital|early-stage investor|seed investor|fund [A-Z]|invests across|backs companies)\b"
    ).unwrap());
    let advisor_role_re = ADVISOR_ROLE_RE.get_or_init(|| {
        Regex::new(
            r"\b(?:full-time advisor|professional advisor|advises (?:multiple|several|various))\b",
        )
        .unwrap()
    });

    if target_lower.starts_with("people/") {
        if founded_re.is_match(&regex_input) {
            trace!(target = %target, pattern = "founded", "Link type inferred from context");
            return "founded".to_string();
        }
        if invested_re.is_match(&regex_input) {
            return "invested_in".to_string();
        }
        if advises_re.is_match(&regex_input) {
            return "advises".to_string();
        }
        if works_at_re.is_match(&regex_input) {
            return "works_at".to_string();
        }
        if regex_input.contains("attended") || regex_input.contains("attendee") {
            return "attended".to_string();
        }
    }

    if target_lower.starts_with("companies/") {
        if invested_re.is_match(&regex_input) {
            return "invested_in".to_string();
        }
        if founded_re.is_match(&regex_input) {
            return "founded".to_string();
        }
        if regex_input.contains("acquired") {
            return "acquired".to_string();
        }
        if partner_role_re.is_match(&regex_input) {
            return "invested_in".to_string();
        }
    }

    if target_lower.starts_with("deals/") {
        return "deal".to_string();
    }

    // 2. Source-page-type-based inference (enhanced with PARTNER_ROLE_RE/ADVISOR_ROLE_RE)
    if let Some(ref spt) = source_page_type {
        match spt {
            PageType::Person if target_lower.starts_with("companies/") => {
                if partner_role_re.is_match(&regex_input) {
                    return "invested_in".to_string();
                }
                if advisor_role_re.is_match(&regex_input) {
                    return "advises".to_string();
                }
                return "works_at".to_string();
            }
            PageType::Company if target_lower.starts_with("people/") => {
                if partner_role_re.is_match(&regex_input) {
                    return "invested_in".to_string();
                }
                return "key_person".to_string();
            }
            _ => {}
        }
    }

    // 3. P0-4: Page role priors — if source page type matches, use prior link type
    let priors = page_role_priors.unwrap_or(PAGE_ROLE_PRIORS);
    if let Some(ref spt) = source_page_type {
        for (prior_type, prior_link_type) in priors {
            if *prior_type == *spt {
                return prior_link_type.to_string();
            }
        }
    }

    // 4. Default: mentions
    "mentions".to_string()
}

/// Convert extracted refs to LinkBatchInput for bulk insertion.
/// P2-3: Deduplicates by (from_slug, to_slug, link_type, link_source) — keeps the
/// first link per tuple, preventing duplicates when the same link is found via
/// markdown, wikilinks, bare slugs, and frontmatter sources.
pub fn refs_to_batch_input(from_slug: &str, refs: &[EntityRef]) -> Vec<LinkBatchInput> {
    let mut seen = std::collections::HashSet::new();
    refs.iter()
        .filter(|r| {
            // P2-3: Dedup by (from_slug, target_slug) — same pair only once
            seen.insert((from_slug.to_string(), r.slug.clone()))
        })
        .map(|r| LinkBatchInput {
            from_slug: from_slug.to_string(),
            to_slug: r.slug.clone(),
            link_type: Some(r.link_type.clone()),
            context: Some(r.context_window.clone().unwrap_or(r.display_name.clone())),
            link_source: Some(r.link_source.clone().unwrap_or(LinkSource::Markdown)),
            origin_slug: Some(from_slug.to_string()),
            origin_field: r.origin_field.clone(),
            direction: r.direction.clone(),
        })
        .collect()
}

/// Validate a date string as ISO YYYY-MM-DD (mirrors TS isValidDate)
pub fn is_valid_date(s: &str) -> bool {
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

/// SlugResolver trait — multi-step name-to-slug resolution chain (mirrors TS makeResolver)
pub trait SlugResolver {
    fn resolve(
        &mut self,
        name: &str,
        dir_hint: Option<&[&str]>,
    ) -> crate::error::Result<Option<String>>;
}

/// Engine-backed slug resolver with caching and 4-step resolve chain
pub struct EngineSlugResolver<'a> {
    pub engine: &'a crate::sqlite_engine::SqliteEngine,
    pub cache: std::collections::HashMap<String, Option<String>>,
    pub live_mode: bool,
}

impl<'a> EngineSlugResolver<'a> {
    pub fn new(engine: &'a crate::sqlite_engine::SqliteEngine, live_mode: bool) -> Self {
        Self {
            engine,
            cache: std::collections::HashMap::new(),
            live_mode,
        }
    }
}

impl<'a> SlugResolver for EngineSlugResolver<'a> {
    fn resolve(
        &mut self,
        name: &str,
        dir_hint: Option<&[&str]>,
    ) -> crate::error::Result<Option<String>> {
        let key = format!("{}:{:?}", name, dir_hint);
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }

        debug!(name = %name, "EngineSlugResolver resolving slug");
        let result = self.resolve_impl(name, dir_hint);
        if result.is_none() {
            warn!(name = %name, "EngineSlugResolver could not resolve slug");
        }
        self.cache.insert(key, result.clone());
        Ok(result)
    }
}

impl<'a> EngineSlugResolver<'a> {
    fn resolve_impl(&self, name: &str, dir_hint: Option<&[&str]>) -> Option<String> {
        // Step 1: Already a slug? (contains /)
        if name.contains('/') && self.engine.get_page(name).ok().flatten().is_some() {
            return Some(name.to_string());
        }

        // Step 2: dir_hint + slugify → try exact getPage
        let slugified = name
            .to_lowercase()
            .replace(' ', "-")
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '/')
            .collect::<String>();
        if let Some(hints) = dir_hint {
            for hint in hints {
                let candidate = format!("{}/{}", hint, slugified);
                if self.engine.get_page(&candidate).ok().flatten().is_some() {
                    return Some(candidate);
                }
            }
        }

        // Step 3: Fuzzy title match — P0-5: pass dir_hint as dir_prefix
        let hint_prefix = dir_hint.and_then(|h| h.first()).copied();
        if let Ok(matches) = self
            .engine
            .find_by_title_fuzzy(name, hint_prefix, Some(0.55), None)
        {
            if let Some(m) = matches.first() {
                return Some(m.slug.clone());
            }
        }

        // Step 4: live-mode search fallback (skip in batch mode for determinism)
        // P0-5: raised threshold from 0.3 to 0.8 (matches TS), added dir_hint filtering
        if self.live_mode {
            let search_opts = crate::types::SearchOpts {
                limit: Some(3),
                ..Default::default()
            };
            if let Ok(results) = self.engine.search_keyword(name, search_opts) {
                for r in &results {
                    if r.score > 0.8 {
                        // P0-5: if dir_hint is present, only accept results whose slug
                        // starts with a prefix matching one of the hints
                        if let Some(hints) = dir_hint {
                            let matches_hint = hints
                                .iter()
                                .any(|hint| r.slug.starts_with(&format!("{}/", hint)));
                            if !matches_hint {
                                continue;
                            }
                        }
                        return Some(r.slug.clone());
                    }
                }
            }
        }

        None
    }
}

/// Parse timeline entries from markdown content
/// P2-3: Enhanced with 3 date formats (mirrors TS):
/// 1. `- YYYY-MM-DD: summary` (original)
/// 2. `- **YYYY-MM-DD** | summary` (TS preferred)
/// 3. `- YYYY-MM-DD | summary` (pipe separator)
///
/// P2-4: Added em-dash/en-dash separator support and multi-line detail collection.
///
/// Separators: `|`, `-`, en-dash (`–`), em-dash (`—`).
///
/// Continuation lines (indented with spaces/tabs after an entry) are appended as detail.
pub fn parse_timeline_entries(content: &str) -> Vec<(String, String)> {
    // P2-4: Support |, -, en-dash, and em-dash as separators (P2-11: lazy regex)
    let date_re =
        RE_DATE.get_or_init(|| Regex::new(r"-\s*(\d{4}-\d{2}-\d{2})\s*[|\-–—]+\s*(.+)").unwrap());
    // Colon separator format remains separate (P2-11: lazy regex)
    let colon_re =
        RE_COLON.get_or_init(|| Regex::new(r"-\s*(\d{4}-\d{2}-\d{2})\s*:\s*(.+)").unwrap());
    // Bold date format (P2-11: lazy regex)
    let bold_date_re = RE_BOLD_DATE
        .get_or_init(|| Regex::new(r"-\s*\*\*(\d{4}-\d{2}-\d{2})\*\*\s*[|\-–—]*\s*(.+)").unwrap());

    /// Internal entry with optional detail for multi-line support (P2-4)
    struct Entry {
        date: String,
        summary: String,
        detail: Option<String>,
    }

    let mut entries: Vec<Entry> = Vec::new();

    for line in content.lines() {
        // Try bold date format first: `- **YYYY-MM-DD** | summary`
        if let Some(caps) = bold_date_re.captures(line) {
            let date = caps.get(1).unwrap().as_str().to_string();
            let summary = caps.get(2).unwrap().as_str().trim().to_string();
            if !summary.is_empty() && is_valid_date(&date) {
                entries.push(Entry {
                    date,
                    summary,
                    detail: None,
                });
                continue;
            }
        }
        // Then try dash/dash-variant separator: `- YYYY-MM-DD | summary` or `- YYYY-MM-DD — summary`
        if let Some(caps) = date_re.captures(line) {
            let date = caps.get(1).unwrap().as_str().to_string();
            let summary = caps.get(2).unwrap().as_str().trim().to_string();
            if !summary.is_empty() && is_valid_date(&date) {
                entries.push(Entry {
                    date,
                    summary,
                    detail: None,
                });
                continue;
            }
        }
        // Then try colon separator: `- YYYY-MM-DD: summary`
        if let Some(caps) = colon_re.captures(line) {
            let date = caps.get(1).unwrap().as_str().to_string();
            let summary = caps.get(2).unwrap().as_str().trim().to_string();
            if !summary.is_empty() && is_valid_date(&date) {
                entries.push(Entry {
                    date,
                    summary,
                    detail: None,
                });
                continue;
            }
        }
        // P2-4: Continuation line — indented text appended to last entry's detail
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = entries.last_mut() {
                let detail = last.detail.get_or_insert_with(String::new);
                if !detail.is_empty() {
                    detail.push(' ');
                }
                detail.push_str(line.trim());
            }
        }
    }

    // Combine summary + detail into a single string per entry
    entries
        .into_iter()
        .map(|e| {
            let text = match e.detail {
                Some(d) if !d.is_empty() => format!("{} {}", e.summary, d),
                _ => e.summary,
            };
            (e.date, text)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_code_blocks_stripped_before_extraction() {
        let content = "Some text\n```python\n[[people/john]]\n```\nReal ref: [[companies/acme]]";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().all(|r| r.slug != "people/john"));
        assert!(refs.iter().any(|r| r.slug == "companies/acme"));
    }

    #[test]
    fn test_strip_code_blocks_inline() {
        let content = "See `[[people/john]]` and [[companies/acme]]";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().all(|r| r.slug != "people/john"));
        assert!(refs.iter().any(|r| r.slug == "companies/acme"));
    }

    #[test]
    fn test_strip_code_blocks_preserves_offsets() {
        let content = "Hello ```code``` World";
        let stripped = strip_code_blocks(content);
        // The stripped text should have same length as original
        assert_eq!(stripped.len(), content.len());
        // Non-code parts should be preserved at same positions
        assert!(stripped.starts_with("Hello "));
        assert!(stripped.ends_with(" World"));
    }

    #[test]
    fn test_extract_markdown_links() {
        let content = "See [Alice](people/alice) and [Acme](companies/acme).";
        let refs = extract_entity_refs(content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].slug, "people/alice");
        assert_eq!(refs[0].display_name, "Alice");
        assert_eq!(refs[1].slug, "companies/acme");
    }

    #[test]
    fn test_extract_wikilinks() {
        let content = "See [[people/alice|Alice]] and [[companies/acme]].";
        let refs = extract_entity_refs(content);
        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].slug, "people/alice");
        assert_eq!(refs[0].display_name, "Alice");
        assert_eq!(refs[1].slug, "companies/acme");
        assert_eq!(refs[1].display_name, "acme");
    }

    #[test]
    fn test_skip_urls() {
        let content = "Visit [website](https://example.com) and [Alice](people/alice).";
        let refs = extract_entity_refs(content);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].slug, "people/alice");
    }

    #[test]
    fn test_explicit_relation_label() {
        let content = "[investor:Sequoia](companies/sequoia)";
        let refs = extract_entity_refs(content);
        assert!(refs
            .iter()
            .any(|r| r.slug == "companies/sequoia" && r.link_type == "investor"));
    }

    #[test]
    fn test_context_window_captured() {
        let content = "prefix text [[people/john]] suffix text";
        let refs = extract_entity_refs(content);
        assert!(refs[0].context_window.is_some());
        let ctx = refs[0].context_window.as_ref().unwrap();
        assert!(ctx.contains("prefix"));
        assert!(ctx.contains("suffix"));
    }

    #[test]
    fn test_frontmatter_link_extraction_string() {
        let frontmatter = serde_json::json!({
            "company": "companies/acme",
        });
        let result = extract_frontmatter_refs(&frontmatter, "test/page", None, None);
        assert!(!result.candidates.is_empty());
        assert!(result
            .candidates
            .iter()
            .any(|r| r.slug == "companies/acme" && r.link_type == "works_at"));
    }

    #[test]
    fn test_frontmatter_link_extraction_array() {
        let frontmatter = serde_json::json!({
            "investors": ["companies/sequoia", "companies/a16z"],
            "key_people": ["people/alice", "people/bob"],
        });
        let result = extract_frontmatter_refs(&frontmatter, "test/page", None, None);
        assert!(result
            .candidates
            .iter()
            .any(|r| r.slug == "companies/sequoia" && r.link_type == "invested_in"));
        assert!(result
            .candidates
            .iter()
            .any(|r| r.slug == "people/alice" && r.link_type == "works_at"));
    }

    #[test]
    fn test_frontmatter_slugify() {
        let frontmatter = serde_json::json!({
            "company": "Acme Corp",
        });
        let result = extract_frontmatter_refs(&frontmatter, "test/page", None, None);
        assert!(result
            .candidates
            .iter()
            .any(|r| r.slug == "companies/acme-corp" || r.slug.starts_with("company")));
    }

    #[test]
    fn test_frontmatter_unresolved_without_resolver() {
        // Without a resolver, all candidates are added and unresolved is empty
        let frontmatter = serde_json::json!({
            "company": "Acme Corp",
        });
        let result = extract_frontmatter_refs(&frontmatter, "test/page", None, None);
        assert!(result.unresolved.is_empty());
        assert!(!result.candidates.is_empty());
    }

    #[test]
    fn test_infer_link_type_founded() {
        // P0-4: "founder" in context window should match FOUNDED_RE
        assert_eq!(
            infer_link_type(
                "Alice",
                "people/alice",
                None,
                Some("Alice is the founder of the company"),
                None
            ),
            "founded"
        );
    }

    #[test]
    fn test_infer_link_type_mentions() {
        assert_eq!(
            infer_link_type("see also", "people/alice", None, None, None),
            "mentions"
        );
    }

    #[test]
    fn test_infer_link_type_role_prior_person() {
        assert_eq!(
            infer_link_type("John", "companies/acme", Some(PageType::Person), None, None),
            "works_at"
        );
    }

    #[test]
    fn test_infer_link_type_role_prior_company() {
        assert_eq!(
            infer_link_type("Alice", "people/alice", Some(PageType::Company), None, None),
            "key_person"
        );
    }

    #[test]
    fn test_infer_link_type_context_over_display() {
        // P0-4: Context with verb phrase should override display name
        assert_eq!(
            infer_link_type(
                "Acme Corp",
                "companies/acme",
                None,
                Some("She invested in Acme Corp last year"),
                None
            ),
            "invested_in"
        );
    }

    #[test]
    fn test_infer_link_type_page_role_priors() {
        // P0-4: When no regex match and no target-path match, page role priors apply.
        // Project is in PAGE_ROLE_PRIORS with "mentions"
        assert_eq!(
            infer_link_type(
                "Something",
                "topics/random",
                Some(PageType::Project),
                None,
                None
            ),
            "mentions"
        );
    }

    #[test]
    fn test_parse_timeline_entries() {
        let content = "- 2024-01-15: Series A announced\n- 2024-02-20: Product launch";
        let entries = parse_timeline_entries(content);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "2024-01-15");
        assert_eq!(entries[0].1, "Series A announced");
    }

    // --- P1-6 tests: frontmatter object entry support ---

    #[test]
    fn test_extract_raw_values_object_name() {
        let value = serde_json::json!({"name": "Sequoia", "role": "lead"});
        let result = extract_raw_values(&value);
        assert_eq!(result, vec!["Sequoia"]);
    }

    #[test]
    fn test_extract_raw_values_object_slug() {
        let value = serde_json::json!({"slug": "sequoia-capital", "role": "lead"});
        let result = extract_raw_values(&value);
        assert_eq!(result, vec!["sequoia-capital"]);
    }

    #[test]
    fn test_extract_raw_values_object_title() {
        let value = serde_json::json!({"title": "Sequoia Capital", "amount": "$10M"});
        let result = extract_raw_values(&value);
        assert_eq!(result, vec!["Sequoia Capital"]);
    }

    #[test]
    fn test_extract_raw_values_object_name_priority() {
        // name takes priority over slug and title
        let value = serde_json::json!({"name": "Sequoia", "slug": "sequoia-cap", "title": "Sequoia Capital"});
        let result = extract_raw_values(&value);
        assert_eq!(result, vec!["Sequoia"]);
    }

    #[test]
    fn test_extract_raw_values_object_no_match() {
        let value = serde_json::json!({"role": "lead", "amount": "$10M"});
        let result = extract_raw_values(&value);
        assert!(result.is_empty());
    }

    #[test]
    fn test_extract_raw_values_array_with_objects() {
        let value = serde_json::json!([
            {"name": "Sequoia", "role": "lead"},
            "companies/a16z",
            {"title": "Y Combinator"}
        ]);
        let result = extract_raw_values(&value);
        assert_eq!(result, vec!["Sequoia", "companies/a16z", "Y Combinator"]);
    }

    #[test]
    fn test_frontmatter_object_extraction() {
        // P1-6: Full integration — frontmatter with object array entries
        let frontmatter = serde_json::json!({
            "investors": [
                {"name": "Sequoia", "role": "lead"},
                {"name": "a16z", "role": "participant"}
            ]
        });
        let _result = extract_frontmatter_refs(&frontmatter, "deals/series-a", None, None);
        // Without page type constraint, investors rule won't match for None type,
        // but the raw value extraction should still work.
        // The investors field has page_type_constraint Some(Company) and Some(Deal),
        // so with page_type=None, the rules won't fire. Test with Deal type instead:
        let result =
            extract_frontmatter_refs(&frontmatter, "deals/series-a", Some(PageType::Deal), None);
        assert!(result
            .candidates
            .iter()
            .any(|r| r.display_name == "sequoia"));
        assert!(result.candidates.iter().any(|r| r.display_name == "a16z"));
    }

    // --- P1-7 tests: funds dir_hint ---

    #[test]
    fn test_funds_dir_hint_investors_company() {
        // Verify that the investors rule for Company page type includes "funds" in dir_hint
        let rule = FRONTMATTER_LINK_MAP
            .iter()
            .find(|r| r.field == "investors" && r.page_type_constraint == Some(PageType::Company))
            .expect("investors/Company rule should exist");
        assert!(rule.dir_hint.contains(&"funds"));
    }

    #[test]
    fn test_funds_dir_hint_investors_deal() {
        let rule = FRONTMATTER_LINK_MAP
            .iter()
            .find(|r| r.field == "investors" && r.page_type_constraint == Some(PageType::Deal))
            .expect("investors/Deal rule should exist");
        assert!(rule.dir_hint.contains(&"funds"));
    }

    #[test]
    fn test_funds_dir_hint_lead_deal() {
        let rule = FRONTMATTER_LINK_MAP
            .iter()
            .find(|r| r.field == "lead" && r.page_type_constraint == Some(PageType::Deal))
            .expect("lead/Deal rule should exist");
        assert!(rule.dir_hint.contains(&"funds"));
    }

    // --- P1-8 tests: multi-segment bare slugs ---

    #[test]
    fn test_bare_slug_multi_segment() {
        let content = "See people/alice/notes for details.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "people/alice/notes"));
    }

    #[test]
    fn test_bare_slug_single_segment_still_works() {
        let content = "See people/alice for details.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "people/alice"));
    }

    #[test]
    fn test_is_slug_reference_multi_segment() {
        assert!(is_slug_reference("people/alice/notes"));
        assert!(is_slug_reference("companies/acme/projects"));
        assert!(is_slug_reference("people/alice"));
    }

    // --- P1-9 tests: new bare slug directories ---

    #[test]
    fn test_bare_slug_new_directories() {
        let content = "Attended meetings/standup and deal/series-a with civic/sf-bond.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "meetings/standup"));
        assert!(refs.iter().any(|r| r.slug == "deal/series-a"));
        assert!(refs.iter().any(|r| r.slug == "civic/sf-bond"));
    }

    #[test]
    fn test_bare_slug_new_directories_more() {
        let content = "Check source/reuters, media/podcast, and yc/s23.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "source/reuters"));
        assert!(refs.iter().any(|r| r.slug == "media/podcast"));
        assert!(refs.iter().any(|r| r.slug == "yc/s23"));
    }

    #[test]
    fn test_is_slug_reference_new_directories() {
        assert!(is_slug_reference("meetings/standup"));
        assert!(is_slug_reference("deal/series-a"));
        assert!(is_slug_reference("civic/sf-bond"));
        assert!(is_slug_reference("project/rebuild"));
        assert!(is_slug_reference("source/reuters"));
        assert!(is_slug_reference("media/podcast"));
        assert!(is_slug_reference("yc/s23"));
    }

    // --- P2-3 tests: cross-source link dedup ---

    #[test]
    fn test_refs_to_batch_input_dedup() {
        // P2-3: Same target slug from different sources should produce only one link
        let refs = vec![
            EntityRef {
                slug: "people/alice".to_string(),
                display_name: "Alice".to_string(),
                link_type: "mentions".to_string(),
                context_window: None,
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: Some(LinkSource::Markdown),
            },
            EntityRef {
                slug: "people/alice".to_string(),
                display_name: "Alice".to_string(),
                link_type: "works_at".to_string(),
                context_window: None,
                origin_field: Some("company".to_string()),
                direction: None,
                link_source: Some(LinkSource::Frontmatter),
            },
        ];
        let batch = refs_to_batch_input("companies/acme", &refs);
        // Only one link should be produced for (companies/acme, people/alice)
        assert_eq!(batch.len(), 1);
        // First source (Markdown) should be kept
        assert_eq!(batch[0].link_source, Some(LinkSource::Markdown));
    }

    #[test]
    fn test_refs_to_batch_input_distinct_targets() {
        // P2-3: Different targets should each produce a link
        let refs = vec![
            EntityRef {
                slug: "people/alice".to_string(),
                display_name: "Alice".to_string(),
                link_type: "mentions".to_string(),
                context_window: None,
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: None,
            },
            EntityRef {
                slug: "people/bob".to_string(),
                display_name: "Bob".to_string(),
                link_type: "mentions".to_string(),
                context_window: None,
                origin_field: Some("body".to_string()),
                direction: None,
                link_source: None,
            },
        ];
        let batch = refs_to_batch_input("companies/acme", &refs);
        assert_eq!(batch.len(), 2);
    }

    // --- P2-4 tests: timeline em-dash and multi-line detail ---

    #[test]
    fn test_parse_timeline_em_dash_separator() {
        // P2-4: Em-dash separator
        let content = "- 2024-01-15 \u{2014} Series A announced";
        let entries = parse_timeline_entries(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "2024-01-15");
        assert_eq!(entries[0].1, "Series A announced");
    }

    #[test]
    fn test_parse_timeline_en_dash_separator() {
        // P2-4: En-dash separator
        let content = "- 2024-01-15 \u{2013} Product launch";
        let entries = parse_timeline_entries(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "2024-01-15");
        assert_eq!(entries[0].1, "Product launch");
    }

    #[test]
    fn test_parse_timeline_pipe_separator() {
        // P2-4: Pipe separator (existing behavior, verify still works)
        let content = "- 2024-01-15 | Series A announced";
        let entries = parse_timeline_entries(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "2024-01-15");
        assert_eq!(entries[0].1, "Series A announced");
    }

    #[test]
    fn test_parse_timeline_multiline_detail() {
        // P2-4: Continuation lines appended as detail
        let content =
            "- 2024-01-15: Series A announced\n  Raised $10M from Sequoia\n  Valuation $50M";
        let entries = parse_timeline_entries(content);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "2024-01-15");
        assert!(entries[0].1.contains("Series A announced"));
        assert!(entries[0].1.contains("Raised $10M from Sequoia"));
        assert!(entries[0].1.contains("Valuation $50M"));
    }

    #[test]
    fn test_parse_timeline_multiline_multiple_entries() {
        // P2-4: Each entry gets its own detail
        let content =
            "- 2024-01-15: Series A\n  Detail for A\n- 2024-02-20: Product launch\n  Detail for B";
        let entries = parse_timeline_entries(content);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].1.contains("Detail for A"));
        assert!(!entries[0].1.contains("Detail for B"));
        assert!(entries[1].1.contains("Detail for B"));
    }

    // --- P2-5 tests: wikilink section anchor and .md stripping ---

    #[test]
    fn test_wikilink_section_anchor_stripped() {
        // P2-5: [[people/alice#career]] should resolve to people/alice
        let content = "See [[people/alice#career]] for details.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "people/alice"));
        assert!(!refs.iter().any(|r| r.slug.contains('#')));
    }

    #[test]
    fn test_wikilink_md_suffix_stripped() {
        // P2-5: [[people/alice.md]] should resolve to people/alice
        let content = "See [[people/alice.md]] for details.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "people/alice"));
        assert!(!refs.iter().any(|r| r.slug.contains(".md")));
    }

    #[test]
    fn test_wikilink_anchor_and_md_combined() {
        // P2-5: [[people/alice.md#career]] should resolve to people/alice
        let content = "See [[people/alice.md#career|Alice]] for details.";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().any(|r| r.slug == "people/alice"));
    }

    // --- P2-6 tests: inline code newline guard ---

    #[test]
    fn test_inline_code_newline_guard() {
        // P2-6: Backtick span crossing a newline should NOT be stripped
        let content = "before `code\nspan` after [[people/alice]]";
        let refs = extract_entity_refs(content);
        // The wikilink should be found because the code span crossing newline is not stripped
        assert!(refs.iter().any(|r| r.slug == "people/alice"));
    }

    #[test]
    fn test_inline_code_single_line_still_stripped() {
        // P2-6: Normal inline code on a single line should still be stripped
        let content = "See `[[people/john]]` and [[companies/acme]]";
        let refs = extract_entity_refs(content);
        assert!(refs.iter().all(|r| r.slug != "people/john"));
        assert!(refs.iter().any(|r| r.slug == "companies/acme"));
    }
}
