use std::collections::HashSet;

use super::types::{
    canonical_url_key, truncate_chars, BackendId, SearchHit, SearchResponse, MAX_OUTPUT_CHARS,
};

pub fn merge_response(
    target: &mut Vec<SearchHit>,
    seen_urls: &mut HashSet<String>,
    response: SearchResponse,
) -> Option<String> {
    for mut hit in response.hits {
        if let Some(url) = hit.url.as_deref() {
            let key = canonical_url_key(url);
            if !seen_urls.insert(key) {
                continue;
            }
        }
        hit.provider = response.provider;
        hit.rank = target.len();
        target.push(hit);
    }
    response.summary
}

pub fn render_search_results(
    query: &str,
    hits: &[SearchHit],
    provider_summaries: &[(BackendId, String)],
    notices: &[String],
) -> String {
    let mut output = String::new();
    if !notices.is_empty() {
        output.push_str("[web_search notices]\n");
        for notice in notices {
            output.push_str("- ");
            output.push_str(notice);
            output.push('\n');
        }
        output.push('\n');
    }
    output.push_str("# Web search\n");
    output.push_str("query: ");
    output.push_str(query);
    output.push('\n');

    for (provider, summary) in provider_summaries {
        output.push_str("\n## ");
        output.push_str(provider.label());
        output.push_str(" provider summary\n");
        append_limited(&mut output, summary, 8_000);
        output.push('\n');
    }

    for (index, hit) in hits.iter().enumerate() {
        output.push_str("\n## ");
        output.push_str(&(index + 1).to_string());
        output.push_str(". ");
        output.push_str(if hit.title.trim().is_empty() {
            "(untitled)"
        } else {
            hit.title.trim()
        });
        output.push('\n');
        output.push_str("provider: ");
        output.push_str(hit.provider.as_str());
        output.push('\n');
        if let Some(url) = hit.url.as_deref() {
            output.push_str("url: ");
            output.push_str(url);
            output.push('\n');
        }
        if let Some(snippet) = hit
            .snippet
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.push_str("snippet: ");
            append_limited(&mut output, snippet, 1_500);
            output.push('\n');
        }
        if let Some(content) = hit
            .content
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        {
            output.push_str("content:\n");
            append_limited(&mut output, content, 6_000);
            output.push('\n');
        }
        if output.chars().count() >= MAX_OUTPUT_CHARS {
            break;
        }
    }
    limit_output(output)
}

pub fn render_read_result(
    url: &str,
    provider: BackendId,
    content: &str,
    notices: &[String],
) -> String {
    let mut output = String::new();
    for notice in notices {
        output.push_str("[notice] ");
        output.push_str(notice);
        output.push('\n');
    }
    output.push_str("url: ");
    output.push_str(url);
    output.push('\n');
    output.push_str("provider: ");
    output.push_str(provider.as_str());
    output.push_str("\n\n");
    append_limited(&mut output, content, MAX_OUTPUT_CHARS);
    limit_output(output)
}

fn append_limited(output: &mut String, value: &str, limit: usize) {
    let remaining = MAX_OUTPUT_CHARS.saturating_sub(output.chars().count());
    let limit = remaining.min(limit);
    if limit == 0 {
        return;
    }
    output.push_str(&truncate_chars(value, limit));
}

pub fn limit_output(value: String) -> String {
    truncate_chars(&value, MAX_OUTPUT_CHARS)
}
