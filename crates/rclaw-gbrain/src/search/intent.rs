//! Query intent classification
//! Mirrors gbrain's src/core/search/intent.ts
//!
//! Classifies queries into entity, temporal, event, or general intent
//! to auto-select detail level for search results.
//!
//! P2-11: All regex patterns use OnceLock for lazy one-time compilation.

use crate::types::DetailLevel;
use regex::Regex;
use std::sync::OnceLock;

/// Query intent type
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    Entity,
    Temporal,
    Event,
    General,
}

/// Classified query intent with detail hint
#[derive(Debug, Clone)]
pub struct QueryIntent {
    pub intent: Intent,
    pub detail_hint: DetailLevel,
    pub confidence: f64,
}

// ---------------------------------------------------------------------------
// P2-11: Lazily-compiled regex patterns (compiled once, reused on every call)
// ---------------------------------------------------------------------------

/// Helper: compile a batch of pattern strings into Regex objects, stored in OnceLock.
fn get_or_init_patterns<'a>(lock: &'a OnceLock<Vec<Regex>>, patterns: &[&str]) -> &'a Vec<Regex> {
    lock.get_or_init(|| patterns.iter().filter_map(|p| Regex::new(p).ok()).collect())
}

// Full-context patterns (checked FIRST — mirrors TS)
static FULL_CONTEXT_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
const FULL_CONTEXT_PATTERNS_STR: &[&str] = &[
    r"(?i)\beverything\b",
    r"(?i)\ball (about|info|information|details)\b",
    r"(?i)\bfull (history|context|picture|story|details)\b",
    r"(?i)\bcomprehensive\b",
    r"(?i)\bdeep dive\b",
    r"(?i)\bgive me everything\b",
    r"(?i)\bcomplete (overview|picture|history)\b",
    r"(?i)\bthe whole (story|picture|history)\b",
];

// Temporal patterns (checked SECOND — mirrors TS order)
static TEMPORAL_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
const TEMPORAL_PATTERNS_STR: &[&str] = &[
    r"(?i)\bwhen\b",
    r"(?i)\blatest\b",
    r"(?i)\brecent\b",
    r"(?i)\bhistory\b",
    r"(?i)\btimeline\b",
    r"(?i)\b\d{4}\b",
    r"(?i)\blast (met|meeting|call)\b",
    r"(?i)\bmeeting notes\b",
    r"(?i)\bwhat'?s new\b",
    r"(?i)\bupdates (on|from|about)\b",
    r"(?i)\bhow long (ago|since)\b",
    r"(?i)\blast (week|month|quarter|year)\b",
    r"(?i)\b\d{4}-\d{2}\b",
];

// Event patterns (checked THIRD)
// P1-8: Expanded to match TS intent.ts patterns
static EVENT_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
const EVENT_PATTERNS_STR: &[&str] = &[
    r"(?i)\bmeeting\b",
    r"(?i)\blaunch\b",
    r"(?i)\bannounce\b",
    r"(?i)\bevent\b",
    r"(?i)\bfunding\b",
    r"(?i)\bacquisition\b",
    r"(?i)\bannounced\b",
    r"(?i)\bannouncement\b",
    r"(?i)\braised \$\d+\b",
    r"(?i)\bfundraise\b",
    r"(?i)\bipo\b",
    r"(?i)\bmerger\b",
    r"(?i)\bmerged\b",
    r"(?i)\bmerges\b",
    r"(?i)\bnews\b",
];

// Entity patterns (checked FOURTH — after temporal, mirrors TS order)
// P1-8: Expanded to match TS intent.ts patterns
static ENTITY_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
const ENTITY_PATTERNS_STR: &[&str] = &[
    r"(?i)\bwho (is|are|was|were)\b",
    r"(?i)\bwhat (is|are|was|were)\b",
    r"(?i)\btell me about\b",
    r"(?i)\bfind\b",
    r"(?i)\blookup\b",
    r"(?i)\bwhat does\b",
    r"(?i)\bdescribe\b",
    r"(?i)\bsummary\b",
    r"(?i)\bsummarize\b",
    r"(?i)\boverview\b",
    r"(?i)\bbackground\b",
    r"(?i)\bprofile\b",
];

/// Check if any pattern in a lazily-compiled set matches the query.
fn any_match(lock: &OnceLock<Vec<Regex>>, patterns: &[&str], query: &str) -> bool {
    get_or_init_patterns(lock, patterns)
        .iter()
        .any(|re| re.is_match(query))
}

/// Classify a query's intent
pub fn classify_intent(query: &str) -> QueryIntent {
    let q = query.to_lowercase();

    // Check full-context patterns FIRST (mirrors TS)
    if any_match(&FULL_CONTEXT_PATTERNS, FULL_CONTEXT_PATTERNS_STR, &q) {
        return QueryIntent {
            intent: Intent::Temporal,
            detail_hint: DetailLevel::High,
            confidence: 0.9,
        };
    }

    // Check temporal patterns SECOND (before entity — mirrors TS order)
    if any_match(&TEMPORAL_PATTERNS, TEMPORAL_PATTERNS_STR, &q) {
        return QueryIntent {
            intent: Intent::Temporal,
            detail_hint: DetailLevel::High,
            confidence: 0.75,
        };
    }

    // Check event patterns THIRD
    if any_match(&EVENT_PATTERNS, EVENT_PATTERNS_STR, &q) {
        return QueryIntent {
            intent: Intent::Event,
            detail_hint: DetailLevel::High,
            confidence: 0.7,
        };
    }

    // Check entity patterns FOURTH (after temporal — mirrors TS order)
    if any_match(&ENTITY_PATTERNS, ENTITY_PATTERNS_STR, &q) {
        return QueryIntent {
            intent: Intent::Entity,
            detail_hint: DetailLevel::Low,
            confidence: 0.8,
        };
    }

    // Default: general
    QueryIntent {
        intent: Intent::General,
        detail_hint: DetailLevel::Medium,
        confidence: 0.5,
    }
}

/// Map intent to detail level (mirrors TS behavior)
/// P2-7: General intent returns None (let engine use default) instead of Medium
pub fn detail_for_intent(intent: &Intent) -> Option<DetailLevel> {
    match intent {
        Intent::Entity => Some(DetailLevel::Low),
        Intent::Temporal => Some(DetailLevel::High),
        Intent::Event => Some(DetailLevel::High),
        Intent::General => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_entity_intent() {
        let result = classify_intent("who is Alice");
        assert_eq!(result.intent, Intent::Entity);
        assert_eq!(result.detail_hint, DetailLevel::Low);
    }

    #[test]
    fn test_temporal_intent() {
        let result = classify_intent("when was the latest funding round");
        assert_eq!(result.intent, Intent::Temporal);
    }

    #[test]
    fn test_event_intent() {
        let result = classify_intent("acquisition of Acme Corp");
        assert_eq!(result.intent, Intent::Event);
    }

    #[test]
    fn test_general_intent() {
        let result = classify_intent("random search terms");
        assert_eq!(result.intent, Intent::General);
    }

    #[test]
    fn test_detail_for_intent() {
        assert_eq!(detail_for_intent(&Intent::Entity), Some(DetailLevel::Low));
        assert_eq!(
            detail_for_intent(&Intent::Temporal),
            Some(DetailLevel::High)
        );
        assert_eq!(detail_for_intent(&Intent::Event), Some(DetailLevel::High));
        assert_eq!(detail_for_intent(&Intent::General), None);
    }
}
