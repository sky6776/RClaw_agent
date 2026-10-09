//! Deterministic code indexing.
//!
//! This is intentionally lightweight: it extracts common symbol declarations
//! across Rust/TS/JS/Python/Go/Java/C/C++-like code without pulling in tree-sitter.
//! The schema/API mirrors the fuller TS code graph so a tree-sitter backend can
//! replace this parser later without changing callers.

use crate::types::{ChunkInput, ChunkSource, CodeEdgeInput, CodeSymbol};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

#[derive(Debug, Clone)]
pub struct CodeIndex {
    pub chunks: Vec<ChunkInput>,
    pub symbols: Vec<CodeSymbol>,
    pub edges: Vec<CodeEdgeInput>,
}

#[derive(Debug, Clone)]
struct SymbolSpan {
    symbol: CodeSymbol,
    body: String,
    chunk_index: i32,
}

pub fn infer_language(slug: &str, explicit: Option<&str>) -> String {
    if let Some(lang) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return normalize_language(lang);
    }
    let ext = slug.rsplit_once('.').map(|(_, ext)| ext).unwrap_or("");
    normalize_language(ext)
}

pub fn index_code(slug: &str, code: &str, language: Option<&str>, start_index: i32) -> CodeIndex {
    let language = infer_language(slug, language);
    let spans = extract_symbol_spans(code, &language);
    let mut chunks = Vec::new();
    let mut symbols = Vec::new();

    if spans.is_empty() {
        let token_count = code.split_whitespace().count() as i32;
        chunks.push(ChunkInput {
            chunk_index: start_index,
            chunk_text: code.to_string(),
            source: ChunkSource::FencedCode,
            token_count,
            embedding: None,
            model: None,
            language: Some(language),
            symbol_name: None,
            symbol_type: Some("file".to_string()),
            start_line: Some(1),
            end_line: Some(code.lines().count().max(1) as i32),
            parent_symbol_path: None,
            symbol_name_qualified: None,
            doc_comment: None,
        });
        return CodeIndex {
            chunks,
            symbols,
            edges: Vec::new(),
        };
    }

    let mut span_records = Vec::new();
    for (offset, mut span) in spans.into_iter().enumerate() {
        let chunk_index = start_index + offset as i32;
        span.chunk_index = chunk_index;
        let token_count = span.body.split_whitespace().count() as i32;
        chunks.push(ChunkInput {
            chunk_index,
            chunk_text: span.body.clone(),
            source: ChunkSource::FencedCode,
            token_count,
            embedding: None,
            model: None,
            language: Some(span.symbol.language.clone()),
            symbol_name: Some(span.symbol.qualified_name.clone()),
            symbol_type: Some(span.symbol.symbol_type.clone()),
            start_line: Some(span.symbol.start_line),
            end_line: Some(span.symbol.end_line),
            parent_symbol_path: None,
            symbol_name_qualified: None,
            doc_comment: None,
        });
        symbols.push(span.symbol.clone());
        span_records.push(span);
    }

    let edges = infer_edges(slug, &span_records);
    CodeIndex {
        chunks,
        symbols,
        edges,
    }
}

fn normalize_language(value: &str) -> String {
    match value.trim().trim_start_matches('.').to_lowercase().as_str() {
        "rs" | "rust" => "rust".to_string(),
        "ts" | "tsx" | "typescript" => "typescript".to_string(),
        "js" | "jsx" | "javascript" | "mjs" | "cjs" => "javascript".to_string(),
        "py" | "python" => "python".to_string(),
        "go" | "golang" => "go".to_string(),
        "java" => "java".to_string(),
        "kt" | "kotlin" => "kotlin".to_string(),
        "cs" | "csharp" => "csharp".to_string(),
        "cpp" | "cc" | "cxx" | "hpp" | "h" | "c++" => "cpp".to_string(),
        "c" => "c".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "text".to_string(),
    }
}

fn extract_symbol_spans(code: &str, language: &str) -> Vec<SymbolSpan> {
    let declarations = find_declarations(code, language);
    if declarations.is_empty() {
        return Vec::new();
    }
    let lines: Vec<&str> = code.lines().collect();
    let mut spans = Vec::new();
    for (idx, decl) in declarations.iter().enumerate() {
        let next_line = declarations
            .get(idx + 1)
            .map(|d| d.line)
            .unwrap_or_else(|| lines.len().max(1));
        let end_line = find_end_line(&lines, decl.line, next_line, language);
        let start_idx = decl.line.saturating_sub(1);
        let end_idx = end_line.min(lines.len()).max(start_idx + 1);
        let body = lines[start_idx..end_idx].join("\n");
        let qualified_name = if let Some(parent) = &decl.parent {
            format!("{}.{}", parent, decl.name)
        } else {
            decl.name.clone()
        };
        spans.push(SymbolSpan {
            symbol: CodeSymbol {
                name: decl.name.clone(),
                qualified_name,
                symbol_type: decl.symbol_type.clone(),
                language: language.to_string(),
                start_line: decl.line as i32,
                end_line: end_line as i32,
                parent_symbol: decl.parent.clone(),
            },
            body,
            chunk_index: 0,
        });
    }
    spans
}

#[derive(Debug, Clone)]
struct Declaration {
    name: String,
    symbol_type: String,
    parent: Option<String>,
    line: usize,
}

fn find_declarations(code: &str, language: &str) -> Vec<Declaration> {
    let mut declarations = Vec::new();
    let mut parent_stack: Vec<(String, usize)> = Vec::new();

    for (idx, line) in code.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.chars().take_while(|c| c.is_whitespace()).count();
        while parent_stack
            .last()
            .map(|(_, parent_indent)| indent <= *parent_indent)
            .unwrap_or(false)
        {
            parent_stack.pop();
        }
        if let Some((name, symbol_type)) = capture_declaration(trimmed, language) {
            let is_container = matches!(
                symbol_type.as_str(),
                "class" | "struct" | "enum" | "trait" | "impl" | "interface"
            );
            let parent = parent_stack.last().map(|(name, _)| name.clone());
            declarations.push(Declaration {
                name: name.clone(),
                symbol_type,
                parent,
                line: line_no,
            });
            if is_container {
                parent_stack.push((name, indent));
            }
        }
    }

    declarations
}

fn capture_declaration(line: &str, language: &str) -> Option<(String, String)> {
    let first_word = line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or("");
    if matches!(
        first_word,
        "return" | "if" | "for" | "while" | "switch" | "catch" | "sizeof"
    ) {
        return None;
    }
    let specs = declaration_specs();
    let keys = match language {
        "python" => &["python"][..],
        "rust" => &["rust", "c_like"][..],
        "go" => &["go", "c_like"][..],
        "c" => &["c_like"][..],
        "cpp" => &["c_like"][..],
        "typescript" | "javascript" => &["ts", "c_like"][..],
        _ => &["rust", "ts", "python", "go", "c_like"][..],
    };
    for key in keys {
        if let Some(patterns) = specs.get(*key) {
            for (symbol_type, re) in patterns {
                if let Some(caps) = re.captures(line) {
                    if let Some(name) = caps.name("name") {
                        return Some((name.as_str().to_string(), (*symbol_type).to_string()));
                    }
                }
            }
        }
    }
    None
}

fn declaration_specs() -> &'static HashMap<&'static str, Vec<(&'static str, Regex)>> {
    static SPECS: OnceLock<HashMap<&'static str, Vec<(&'static str, Regex)>>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert(
            "rust",
            vec![
                ("function", Regex::new(r"^(?:pub\s+)?(?:async\s+)?fn\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("struct", Regex::new(r"^(?:pub\s+)?struct\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("enum", Regex::new(r"^(?:pub\s+)?enum\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("trait", Regex::new(r"^(?:pub\s+)?trait\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("impl", Regex::new(r"^impl(?:<[^>]+>)?\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
            ],
        );
        m.insert(
            "ts",
            vec![
                ("class", Regex::new(r"^(?:export\s+)?(?:default\s+)?class\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)").unwrap()),
                ("interface", Regex::new(r"^(?:export\s+)?interface\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)").unwrap()),
                ("function", Regex::new(r"^(?:export\s+)?(?:async\s+)?function\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)").unwrap()),
                ("function", Regex::new(r"^(?:export\s+)?(?:const|let|var)\s+(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*(?:async\s*)?\(").unwrap()),
                ("method", Regex::new(r"^(?:public\s+|private\s+|protected\s+|static\s+|async\s+)*(?P<name>[A-Za-z_$][A-Za-z0-9_$]*)\s*\([^)]*\)\s*(?::[^{]+)?\{").unwrap()),
            ],
        );
        m.insert(
            "python",
            vec![
                ("class", Regex::new(r"^class\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("function", Regex::new(r"^(?:async\s+)?def\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
            ],
        );
        m.insert(
            "go",
            vec![
                ("function", Regex::new(r"^func\s+(?:\([^)]*\)\s*)?(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("struct", Regex::new(r"^type\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s+struct").unwrap()),
                ("interface", Regex::new(r"^type\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s+interface").unwrap()),
            ],
        );
        m.insert(
            "c_like",
            vec![
                ("class", Regex::new(r"^(?:public\s+|private\s+|protected\s+)?class\s+(?P<name>[A-Za-z_][A-Za-z0-9_]*)").unwrap()),
                ("function", Regex::new(r"^(?:[A-Za-z_][A-Za-z0-9_<>\[\]\*&:\s]+\s+)+(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\([^;]*\)\s*\{?").unwrap()),
            ],
        );
        m
    })
}

fn find_end_line(
    lines: &[&str],
    start_line: usize,
    next_decl_line: usize,
    language: &str,
) -> usize {
    if language == "python" {
        let start_indent = lines
            .get(start_line.saturating_sub(1))
            .map(|line| line.chars().take_while(|c| c.is_whitespace()).count())
            .unwrap_or(0);
        for idx in start_line..next_decl_line.saturating_sub(1) {
            let line = lines.get(idx).copied().unwrap_or("");
            if !line.trim().is_empty()
                && line.chars().take_while(|c| c.is_whitespace()).count() <= start_indent
            {
                return idx;
            }
        }
        return next_decl_line.saturating_sub(1).max(start_line);
    }

    let mut depth = 0_i32;
    let mut saw_open = false;
    let start_idx = start_line.saturating_sub(1);
    let end_idx = lines.len().min(next_decl_line.max(start_line));
    for (idx, line) in lines.iter().enumerate().take(end_idx).skip(start_idx) {
        for ch in line.chars() {
            match ch {
                '{' => {
                    depth += 1;
                    saw_open = true;
                }
                '}' => depth -= 1,
                _ => {}
            }
        }
        if saw_open && depth <= 0 {
            return idx + 1;
        }
    }
    next_decl_line.saturating_sub(1).max(start_line)
}

fn infer_edges(slug: &str, spans: &[SymbolSpan]) -> Vec<CodeEdgeInput> {
    let mut symbol_lookup: HashMap<String, String> = HashMap::new();
    for span in spans {
        symbol_lookup.insert(span.symbol.name.clone(), span.symbol.qualified_name.clone());
        symbol_lookup.insert(
            span.symbol.qualified_name.clone(),
            span.symbol.qualified_name.clone(),
        );
    }

    let mut edges = Vec::new();
    let mut seen = HashSet::new();
    for span in spans {
        let tokens = call_tokens(&span.body);
        for token in tokens {
            if token == span.symbol.name || token == span.symbol.qualified_name {
                continue;
            }
            let lookup = symbol_lookup.get(&token).or_else(|| {
                token
                    .rsplit('.')
                    .next()
                    .and_then(|name| symbol_lookup.get(name))
            });
            if let Some(target) = lookup {
                let key = format!("{}->{}", span.symbol.qualified_name, target);
                if seen.insert(key) {
                    edges.push(CodeEdgeInput {
                        from_slug: slug.to_string(),
                        from_symbol: span.symbol.qualified_name.clone(),
                        to_slug: slug.to_string(),
                        to_symbol: target.clone(),
                        edge_type: "calls".to_string(),
                        confidence: 0.85,
                        context: Some(format!(
                            "{} references {}",
                            span.symbol.qualified_name, target
                        )),
                        from_chunk_id: None,
                        to_chunk_id: None,
                        from_symbol_qualified: None,
                        to_symbol_qualified: None,
                    });
                }
            }
        }
    }
    edges
}

fn call_tokens(body: &str) -> HashSet<String> {
    static CALL_RE: OnceLock<Regex> = OnceLock::new();
    let re = CALL_RE.get_or_init(|| {
        Regex::new(r"(?P<name>[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)?)\s*\(").unwrap()
    });
    let keywords = [
        "if", "for", "while", "match", "switch", "catch", "return", "sizeof", "Some", "Ok", "Err",
        "vec", "println", "format",
    ];
    let keyword_set: HashSet<&str> = keywords.into_iter().collect();
    re.captures_iter(body)
        .filter_map(|caps| caps.name("name").map(|m| m.as_str().to_string()))
        .filter(|name| !keyword_set.contains(name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_rust_symbols_and_edges() {
        let code = r#"
pub fn alpha() {
    beta();
}

fn beta() {
}
"#;
        let indexed = index_code("code/lib.rs", code, None, 0);
        assert_eq!(indexed.symbols.len(), 2);
        assert!(indexed.symbols.iter().any(|s| s.name == "alpha"));
        assert!(indexed
            .edges
            .iter()
            .any(|e| e.from_symbol == "alpha" && e.to_symbol == "beta"));
    }

    #[test]
    fn indexes_python_class_methods() {
        let code = r#"
class Greeter:
    def hello(self):
        self.world()

    def world(self):
        pass
"#;
        let indexed = index_code("code/greeter.py", code, None, 0);
        assert!(indexed
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Greeter.hello"));
        assert!(indexed
            .symbols
            .iter()
            .any(|s| s.qualified_name == "Greeter.world"));
    }

    #[test]
    fn indexes_c_functions_and_edges() {
        let code = r#"
int alpha(void) {
    return beta();
}

int beta(void) {
    return 1;
}
"#;
        let indexed = index_code("code/main.c", code, None, 0);
        assert!(indexed.symbols.iter().any(|s| s.name == "alpha"));
        assert!(indexed.symbols.iter().any(|s| s.name == "beta"));
        assert!(indexed
            .edges
            .iter()
            .any(|e| e.from_symbol == "alpha" && e.to_symbol == "beta"));
    }
}
