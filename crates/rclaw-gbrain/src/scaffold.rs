//! Deterministic output builders — prevents LLM-hallucinated URLs.
//! Mirrors gbrain's src/core/output/scaffold.ts
//!
//! Design invariant: "LLM picks WHAT to write. Code builds WHERE and HOW."

/// Error type for scaffold input validation
#[derive(Debug, thiserror::Error)]
pub enum ScaffoldError {
    #[error("Invalid handle: {0} (expected @handle or handle with 1-15 chars)")]
    InvalidHandle(String),
    #[error("Invalid tweet ID: {0} (expected 1-20 digits)")]
    InvalidTweetId(String),
    #[error("Invalid message ID: {0}")]
    InvalidMessageId(String),
    #[error("Invalid slug: {0}")]
    InvalidSlug(String),
    #[error("Invalid date: {0} (expected YYYY-MM-DD)")]
    InvalidDate(String),
    #[error("Input is empty")]
    Empty(String),
}

/// Build a tweet citation: `[Source: [X/handle, YYYY-MM-DD](https://x.com/handle/status/id)]`
pub fn tweet_citation(
    handle: &str,
    tweet_id: &str,
    date_iso: Option<&str>,
) -> Result<String, ScaffoldError> {
    let clean_handle = handle.trim_start_matches('@');
    if clean_handle.is_empty()
        || clean_handle.len() > 15
        || !clean_handle
            .chars()
            .all(|c| c.is_alphanumeric() || c == '_')
    {
        return Err(ScaffoldError::InvalidHandle(handle.to_string()));
    }
    if tweet_id.is_empty() || tweet_id.len() > 20 || !tweet_id.chars().all(|c| c.is_ascii_digit()) {
        return Err(ScaffoldError::InvalidTweetId(tweet_id.to_string()));
    }
    let date = date_iso.unwrap_or_default();
    if !date.is_empty() && !is_iso_date(date) {
        return Err(ScaffoldError::InvalidDate(date.to_string()));
    }
    // Conditionally include date to avoid malformed ", " when date is empty
    let date_part = if date.is_empty() {
        String::new()
    } else {
        format!(", {}", date)
    };
    Ok(format!(
        "[Source: [X/{}{}](https://x.com/{}/status/{})]",
        clean_handle, date_part, clean_handle, tweet_id
    ))
}

/// Build an email citation: `[Source: email "subject", YYYY-MM-DD](https://mail.google.com/mail/u/0/#inbox/messageId)`
pub fn email_citation(
    _account: &str,
    message_id: &str,
    subject: &str,
    date_iso: Option<&str>,
) -> Result<String, ScaffoldError> {
    if message_id.is_empty() || message_id.len() > 60 {
        return Err(ScaffoldError::InvalidMessageId(message_id.to_string()));
    }
    let date = date_iso.unwrap_or("");
    if !date.is_empty() && !is_iso_date(date) {
        return Err(ScaffoldError::InvalidDate(date.to_string()));
    }
    // Sanitize message_id to prevent markdown injection via [ ] ( ) characters
    let safe_message_id = sanitize_label(message_id);
    Ok(format!(
        "[Source: email \"{}\", {}](https://mail.google.com/mail/u/0/#inbox/{})",
        sanitize_label(subject),
        date,
        safe_message_id
    ))
}

/// Build an entity link: `[Display Name](../../dir/slug.md)`
pub fn entity_link(
    slug: &str,
    display_text: &str,
    relative_prefix: Option<&str>,
) -> Result<String, ScaffoldError> {
    if slug.is_empty() || !is_valid_slug(slug) {
        return Err(ScaffoldError::InvalidSlug(slug.to_string()));
    }
    if display_text.is_empty() {
        return Err(ScaffoldError::Empty("display_text".to_string()));
    }
    let prefix = relative_prefix.unwrap_or("../..");
    let safe_display = sanitize_label(display_text);
    // Slugs always end with .md in wikilinks for portability
    let md_slug = if slug.ends_with(".md") {
        slug.to_string()
    } else {
        format!("{}.md", slug)
    };
    Ok(format!("[{}]({}/{})", safe_display, prefix, md_slug))
}

/// Build a timeline line: `- **YYYY-MM-DD** | Summary [Source: ...]`
pub fn timeline_line(
    date_iso: &str,
    summary: &str,
    source_citation: Option<&str>,
) -> Result<String, ScaffoldError> {
    if !is_iso_date(date_iso) {
        return Err(ScaffoldError::InvalidDate(date_iso.to_string()));
    }
    if summary.is_empty() {
        return Err(ScaffoldError::Empty("summary".to_string()));
    }
    let safe_summary = sanitize_label(summary);
    if let Some(src) = source_citation {
        if src.is_empty() {
            Ok(format!("- **{}** | {}", date_iso, safe_summary))
        } else {
            Ok(format!(
                "- **{}** | {} {}",
                date_iso,
                safe_summary,
                sanitize_label(src)
            ))
        }
    } else {
        Ok(format!("- **{}** | {}", date_iso, safe_summary))
    }
}

/// Sanitize a label for safe markdown inclusion
fn sanitize_label(s: &str) -> String {
    let s = s.trim();
    let s = s.chars().take(200).collect::<String>();
    s.replace('\n', " ").replace('[', "(").replace(']', ")")
}

fn is_iso_date(s: &str) -> bool {
    s.len() == 10
        && s.starts_with(|c: char| c.is_ascii_digit())
        && s.chars().nth(4) == Some('-')
        && s.chars().nth(7) == Some('-')
        && chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").is_ok()
}

use std::sync::OnceLock;

static SLUG_RE: OnceLock<regex::Regex> = OnceLock::new();

fn is_valid_slug(s: &str) -> bool {
    let re = SLUG_RE.get_or_init(|| {
        regex::Regex::new(r"^[a-z0-9][a-z0-9\-]*(/[a-z0-9][a-z0-9\-]*)*$").unwrap()
    });
    re.is_match(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tweet_citation() {
        let c = tweet_citation("alice", "1234567890", Some("2024-01-15")).unwrap();
        assert!(c.contains("[Source: [X/alice, 2024-01-15]"));
        assert!(c.contains("x.com/alice/status/1234567890"));
    }

    #[test]
    fn test_tweet_citation_handle_with_at() {
        let c = tweet_citation("@alice", "123", None).unwrap();
        assert!(c.contains("X/alice"));
    }

    #[test]
    fn test_tweet_citation_invalid() {
        assert!(tweet_citation("", "123", None).is_err());
    }

    #[test]
    fn test_entity_link() {
        let link = entity_link("people/alice", "Alice Smith", None).unwrap();
        assert!(link.contains("[Alice Smith]"));
        assert!(link.contains("people/alice.md"));
    }

    #[test]
    fn test_timeline_line() {
        let line = timeline_line("2024-01-15", "Met with Alice", None).unwrap();
        assert_eq!(line, "- **2024-01-15** | Met with Alice");
    }
}
