//! Tree-sitter-based AST code chunker.
//!
//! Mirrors gbrain's src/core/chunkers/code.ts but uses Rust tree-sitter bindings
//! instead of WASM. Provides richer metadata than the regex-based `index_code()`
//! including parent scope paths, qualified symbol names, doc comments, and
//! call-site edge extraction from the AST.

use crate::code_index::CodeIndex;
use crate::types::{ChunkInput, ChunkSource, CodeEdgeInput, CodeSymbol};
use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;
use tracing::{debug, warn};
use tree_sitter::{Node, Parser, Tree};

// ── Target token count for small-sibling merging ──────────────────────
const TARGET_TOKEN_COUNT: i32 = 300;
/// Merge adjacent chunks under this fraction of target token count.
const SMALL_MERGE_FRACTION: f64 = 0.15;

// ── Language detection ────────────────────────────────────────────────

/// Map file extension (without dot) to canonical language name.
fn detect_language(slug: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(lang) = explicit.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(normalize_lang(lang));
    }
    let ext = slug.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    match ext.to_lowercase().as_str() {
        "rs" => Some("rust".to_string()),
        "ts" => Some("typescript".to_string()),
        "tsx" => Some("tsx".to_string()),
        "js" | "mjs" | "cjs" => Some("javascript".to_string()),
        "jsx" => Some("jsx".to_string()),
        "py" => Some("python".to_string()),
        "go" => Some("go".to_string()),
        "java" => Some("java".to_string()),
        "c" => Some("c".to_string()),
        "cpp" | "cc" | "cxx" | "hpp" => Some("cpp".to_string()),
        _ => None,
    }
}

fn normalize_lang(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "rs" | "rust" => "rust",
        "ts" | "typescript" => "typescript",
        "tsx" => "tsx",
        "js" | "jsx" | "javascript" | "mjs" | "cjs" => "javascript",
        "py" | "python" => "python",
        "go" | "golang" => "go",
        "java" => "java",
        "c" => "c",
        "cpp" | "c++" | "cc" | "cxx" => "cpp",
        other => other,
    }
    .to_string()
}

// ── Grammar initialization (lazy OnceLock) ────────────────────────────

struct GrammarSet {
    rust: tree_sitter::Language,
    typescript: tree_sitter::Language,
    tsx: tree_sitter::Language,
    javascript: tree_sitter::Language,
    python: tree_sitter::Language,
    go: tree_sitter::Language,
    java: tree_sitter::Language,
    c: tree_sitter::Language,
    cpp: tree_sitter::Language,
}

static GRAMMARS: OnceLock<GrammarSet> = OnceLock::new();

fn grammars() -> &'static GrammarSet {
    GRAMMARS.get_or_init(|| GrammarSet {
        rust: tree_sitter_rust::LANGUAGE.into(),
        typescript: tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        tsx: tree_sitter_typescript::LANGUAGE_TSX.into(),
        javascript: tree_sitter_javascript::LANGUAGE.into(),
        python: tree_sitter_python::LANGUAGE.into(),
        go: tree_sitter_go::LANGUAGE.into(),
        java: tree_sitter_java::LANGUAGE.into(),
        c: tree_sitter_c::LANGUAGE.into(),
        cpp: tree_sitter_cpp::LANGUAGE.into(),
    })
}

fn get_language(name: &str) -> Option<tree_sitter::Language> {
    let g = grammars();
    match name {
        "rust" => Some(g.rust.clone()),
        "typescript" => Some(g.typescript.clone()),
        "tsx" => Some(g.tsx.clone()),
        "javascript" | "jsx" => Some(g.javascript.clone()),
        "python" => Some(g.python.clone()),
        "go" => Some(g.go.clone()),
        "java" => Some(g.java.clone()),
        "c" => Some(g.c.clone()),
        "cpp" => Some(g.cpp.clone()),
        _ => None,
    }
}

// ── Top-level type definitions ────────────────────────────────────────

/// Returns the set of AST node types that count as semantic top-level units
/// for the given language. Mirrors TS TOP_LEVEL_TYPES.
fn top_level_types(lang: &str) -> &'static HashSet<&'static str> {
    static RUST: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static TYPESCRIPT: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static JAVASCRIPT: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static PYTHON: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static GO: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static JAVA: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static C: OnceLock<HashSet<&'static str>> = OnceLock::new();
    static CPP: OnceLock<HashSet<&'static str>> = OnceLock::new();

    match lang {
        "rust" => RUST.get_or_init(|| {
            HashSet::from([
                "function_item",
                "impl_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "mod_item",
                "type_item",
                "const_item",
                "static_item",
                "use_declaration",
            ])
        }),
        "typescript" | "tsx" => TYPESCRIPT.get_or_init(|| {
            HashSet::from([
                "function_declaration",
                "class_declaration",
                "abstract_class_declaration",
                "interface_declaration",
                "type_alias_declaration",
                "enum_declaration",
                "lexical_declaration",
                "variable_declaration",
                "export_statement",
                "method_definition",
                "arrow_function",
                "generator_function_declaration",
            ])
        }),
        "javascript" | "jsx" => JAVASCRIPT.get_or_init(|| {
            HashSet::from([
                "function_declaration",
                "class_declaration",
                "lexical_declaration",
                "variable_declaration",
                "export_statement",
                "method_definition",
                "arrow_function",
                "generator_function_declaration",
            ])
        }),
        "python" => PYTHON.get_or_init(|| {
            HashSet::from([
                "function_definition",
                "class_definition",
                "import_statement",
                "import_from_statement",
                "assignment",
                "decorated_definition",
            ])
        }),
        "go" => GO.get_or_init(|| {
            HashSet::from([
                "function_declaration",
                "method_declaration",
                "type_declaration",
                "import_declaration",
            ])
        }),
        "java" => JAVA.get_or_init(|| {
            HashSet::from([
                "class_declaration",
                "interface_declaration",
                "record_declaration",
                "method_declaration",
                "constructor_declaration",
                "field_declaration",
                "enum_declaration",
            ])
        }),
        "c" => C.get_or_init(|| {
            HashSet::from([
                "function_definition",
                "struct_specifier",
                "enum_specifier",
                "type_definition",
            ])
        }),
        "cpp" => CPP.get_or_init(|| {
            HashSet::from([
                "function_definition",
                "struct_specifier",
                "enum_specifier",
                "type_definition",
                "class_specifier",
                "namespace_definition",
                "template_declaration",
            ])
        }),
        _ => {
            // Return an empty static set for unsupported languages
            static EMPTY: OnceLock<HashSet<&'static str>> = OnceLock::new();
            EMPTY.get_or_init(HashSet::new)
        }
    }
}

// ── Nested emit configuration ─────────────────────────────────────────

/// For nestable parent types, emit a parent scope-header chunk plus
/// per-child chunks with parent_symbol_path populated.
struct NestedEmitConfig {
    parent_types: &'static [&'static str],
    child_types: &'static [&'static str],
}

fn nested_emit_config(lang: &str) -> Option<NestedEmitConfig> {
    match lang {
        "rust" => Some(NestedEmitConfig {
            parent_types: &[
                "impl_item",
                "struct_item",
                "enum_item",
                "trait_item",
                "mod_item",
            ],
            child_types: &[
                "function_item",
                "const_item",
                "static_item",
                "type_item",
                "struct_item",
                "enum_item",
                "trait_item",
            ],
        }),
        "typescript" | "tsx" | "javascript" | "jsx" => Some(NestedEmitConfig {
            parent_types: &[
                "class_declaration",
                "abstract_class_declaration",
                "interface_declaration",
                "enum_declaration",
            ],
            child_types: &[
                "method_definition",
                "function_declaration",
                "lexical_declaration",
                "variable_declaration",
                "arrow_function",
                "constructor_definition",
            ],
        }),
        "python" => Some(NestedEmitConfig {
            parent_types: &["class_definition"],
            child_types: &["function_definition", "decorated_definition", "assignment"],
        }),
        "go" => Some(NestedEmitConfig {
            parent_types: &["type_declaration"],
            child_types: &["method_declaration", "function_declaration"],
        }),
        "java" => Some(NestedEmitConfig {
            parent_types: &[
                "class_declaration",
                "interface_declaration",
                "record_declaration",
                "enum_declaration",
            ],
            child_types: &[
                "method_declaration",
                "constructor_declaration",
                "field_declaration",
            ],
        }),
        "cpp" => Some(NestedEmitConfig {
            parent_types: &[
                "class_specifier",
                "struct_specifier",
                "namespace_definition",
            ],
            child_types: &[
                "function_definition",
                "template_declaration",
                "type_definition",
            ],
        }),
        "c" => None,
        _ => None,
    }
}

// ── Qualified name separator ──────────────────────────────────────────

/// Returns the separator used for building qualified names in a language.
/// Rust uses `::`, most others use `.`.
fn qualified_name_separator(lang: &str) -> &'static str {
    match lang {
        "rust" => "::",
        _ => ".",
    }
}

// ── Intermediate chunk record ─────────────────────────────────────────

#[derive(Debug, Clone)]
struct RawChunk {
    chunk_text: String,
    symbol_name: String,
    symbol_name_qualified: String,
    symbol_type: String,
    start_line: i32,
    end_line: i32,
    parent_symbol_path: Option<String>,
    doc_comment: Option<String>,
}

// ── Main public API ───────────────────────────────────────────────────

/// AST-based code chunking using tree-sitter.
///
/// Returns a `CodeIndex` with richer metadata than the regex-based
/// `index_code()`, including parent scope paths, qualified symbol names,
/// doc comments, and AST-extracted call edges.
///
/// Falls back to `index_code()` if tree-sitter parsing fails or the
/// language is unsupported.
pub fn chunk_code_tree_sitter(
    slug: &str,
    code: &str,
    language: Option<&str>,
    start_index: i32,
) -> CodeIndex {
    let lang = match detect_language(slug, language) {
        Some(l) => l,
        None => {
            debug!(slug = %slug, "No tree-sitter grammar for language, falling back to regex");
            return crate::code_index::index_code(slug, code, language, start_index);
        }
    };

    let ts_lang = match get_language(&lang) {
        Some(l) => l,
        None => {
            debug!(lang = %lang, "No tree-sitter grammar available, falling back to regex");
            return crate::code_index::index_code(slug, code, language, start_index);
        }
    };

    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        warn!(lang = %lang, "Failed to set tree-sitter language, falling back to regex");
        return crate::code_index::index_code(slug, code, language, start_index);
    }

    let tree = match parser.parse(code, None) {
        Some(t) => t,
        None => {
            warn!(lang = %lang, "Tree-sitter parsing failed, falling back to regex");
            return crate::code_index::index_code(slug, code, language, start_index);
        }
    };

    let path = extract_path_from_slug(slug);
    let raw_chunks = extract_chunks_from_ast(&tree, code, &lang, &path);

    // Apply small sibling merging
    let merged = merge_small_siblings(raw_chunks, &lang);

    // Build ChunkInput, CodeSymbol, and edges
    build_code_index(slug, code, &lang, &merged, start_index)
}

/// Extract the file path portion from a slug (e.g. "code/src/engine.rs" -> "src/engine.rs").
fn extract_path_from_slug(slug: &str) -> String {
    // Slugs are like "code/path/to/file.ext" — take everything after the first /
    slug.split_once('/')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| slug.to_string())
}

// ── AST traversal and chunk extraction ────────────────────────────────

fn extract_chunks_from_ast(tree: &Tree, code: &str, lang: &str, path: &str) -> Vec<RawChunk> {
    let root = tree.root_node();
    let top_types = top_level_types(lang);
    let nested_config = nested_emit_config(lang);
    let sep = qualified_name_separator(lang);

    let mut chunks = Vec::new();
    let mut chunk_idx = 0usize;
    let lines: Vec<&str> = code.lines().collect();

    walk_node(
        root,
        code,
        lang,
        path,
        top_types,
        nested_config.as_ref(),
        sep,
        &lines,
        None, // parent qualified name
        &mut chunks,
        &mut chunk_idx,
    );

    chunks
}

#[allow(clippy::too_many_arguments)]
fn walk_node(
    node: Node,
    code: &str,
    lang: &str,
    path: &str,
    top_types: &HashSet<&'static str>,
    nested_config: Option<&NestedEmitConfig>,
    sep: &str,
    lines: &[&str],
    parent_qname: Option<&str>,
    chunks: &mut Vec<RawChunk>,
    chunk_idx: &mut usize,
) {
    let kind = node.kind();

    // Check if this is a top-level semantic unit
    if top_types.contains(kind) {
        let symbol_name = extract_symbol_name(node, code).unwrap_or_else(|| kind.to_string());
        let qname = build_qualified_name(parent_qname, &symbol_name, sep);

        // Check if this is a nestable parent with children to emit separately
        let is_nested_parent = nested_config
            .map(|cfg| cfg.parent_types.contains(&kind))
            .unwrap_or(false);

        if is_nested_parent {
            // Emit a parent scope-header chunk: declaration line + member digest list
            let header_text = build_parent_header(node, code, lang, path, &qname, lines);
            let start_line = (node.start_position().row + 1) as i32;
            let end_line = (node.end_position().row + 1) as i32;
            let doc_comment = extract_doc_comment(node, code, lang);

            chunks.push(RawChunk {
                chunk_text: header_text,
                symbol_name: symbol_name.clone(),
                symbol_name_qualified: qname.clone(),
                symbol_type: normalize_symbol_type(kind, lang),
                start_line,
                end_line,
                parent_symbol_path: parent_qname.map(|s| s.to_string()),
                doc_comment,
            });
            *chunk_idx += 1;

            // Walk children and emit per-child chunks with parent_symbol_path.
            // Members are often inside container nodes like declaration_list, class_body,
            // block, etc. We need to descend through these to find the actual members.
            let child_config = nested_config.unwrap();
            walk_nested_children(
                node,
                code,
                lang,
                path,
                top_types,
                nested_config,
                sep,
                lines,
                &qname,
                chunks,
                chunk_idx,
                child_config,
            );
            return;
        }

        // Non-nested or leaf node: emit the full body as a chunk
        let chunk_text = build_chunk_text(node, code, lang, path, &qname, lines);
        let start_line = (node.start_position().row + 1) as i32;
        let end_line = (node.end_position().row + 1) as i32;
        let doc_comment = extract_doc_comment(node, code, lang);

        chunks.push(RawChunk {
            chunk_text,
            symbol_name,
            symbol_name_qualified: qname,
            symbol_type: normalize_symbol_type(kind, lang),
            start_line,
            end_line,
            parent_symbol_path: parent_qname.map(|s| s.to_string()),
            doc_comment,
        });
        *chunk_idx += 1;
        return;
    }

    // Not a top-level type: recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk_node(
            child,
            code,
            lang,
            path,
            top_types,
            nested_config,
            sep,
            lines,
            parent_qname,
            chunks,
            chunk_idx,
        );
    }
}

/// Walk children of a nested parent (class/impl/trait/interface) to find
/// member methods/functions. Members may be nested inside container nodes
/// like `declaration_list`, `class_body`, `block`, etc.
#[allow(clippy::too_many_arguments)]
fn walk_nested_children(
    node: Node,
    code: &str,
    lang: &str,
    path: &str,
    top_types: &HashSet<&'static str>,
    nested_config: Option<&NestedEmitConfig>,
    sep: &str,
    lines: &[&str],
    parent_qname: &str,
    chunks: &mut Vec<RawChunk>,
    chunk_idx: &mut usize,
    child_config: &NestedEmitConfig,
) {
    // Container node types that hold members but are not members themselves
    let container_types: HashSet<&str> = HashSet::from([
        "declaration_list",       // Rust impl/trait body
        "field_declaration_list", // Rust struct body
        "class_body",             // TypeScript/Java class body
        "block",                  // Python class body
        "declaration",            // Go type spec body
        "enum_body",              // Java enum body
        "record_body",            // Java record body
        "namespace_body",         // C++ namespace body
    ]);

    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        let child_kind = child.kind();

        // If this is a direct member, walk it
        if child_config.child_types.contains(&child_kind) || top_types.contains(child_kind) {
            walk_node(
                child,
                code,
                lang,
                path,
                top_types,
                nested_config,
                sep,
                lines,
                Some(parent_qname),
                chunks,
                chunk_idx,
            );
            continue;
        }

        // If this is a container node, descend into it to find members
        if container_types.contains(child_kind) {
            walk_nested_children(
                child,
                code,
                lang,
                path,
                top_types,
                nested_config,
                sep,
                lines,
                parent_qname,
                chunks,
                chunk_idx,
                child_config,
            );
            continue;
        }

        // Also check if the child is itself a nested parent (e.g. nested class)
        if nested_config
            .map(|cfg| cfg.parent_types.contains(&child_kind))
            .unwrap_or(false)
        {
            walk_node(
                child,
                code,
                lang,
                path,
                top_types,
                nested_config,
                sep,
                lines,
                Some(parent_qname),
                chunks,
                chunk_idx,
            );
        }
    }
}

// ── Symbol name extraction ────────────────────────────────────────────

/// Extract the symbol name from an AST node using the "name" field.
fn extract_symbol_name(node: Node, code: &str) -> Option<String> {
    // Try the "name" field first
    if let Some(name_node) = node.child_by_field_name("name") {
        if let Ok(text) = name_node.utf8_text(code.as_bytes()) {
            return Some(text.to_string());
        }
    }

    // Fallback: for some node types the name is in specific child positions
    match node.kind() {
        "impl_item" => {
            // impl Type or impl Trait for Type — try "trait" then "type" field
            if let Some(trait_node) = node.child_by_field_name("trait") {
                if let Ok(text) = trait_node.utf8_text(code.as_bytes()) {
                    return Some(format!("impl {}", text.trim_start_matches("for ").trim()));
                }
            }
            if let Some(type_node) = node.child_by_field_name("type") {
                if let Ok(text) = type_node.utf8_text(code.as_bytes()) {
                    return Some(format!("impl {}", text));
                }
            }
            // Try to get the type from child nodes
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_identifier" {
                    if let Ok(text) = child.utf8_text(code.as_bytes()) {
                        return Some(format!("impl {}", text));
                    }
                }
            }
        }
        "variable_declaration" | "lexical_declaration" => {
            // Try to find the variable declarator
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "variable_declarator" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Ok(text) = name_node.utf8_text(code.as_bytes()) {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }
        "type_declaration" => {
            // Go type declarations: type Name struct/interface
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "type_spec" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Ok(text) = name_node.utf8_text(code.as_bytes()) {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }
        "decorated_definition" => {
            // Python decorated definitions: the actual definition is a child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if child.kind() == "function_definition" || child.kind() == "class_definition" {
                    if let Some(name_node) = child.child_by_field_name("name") {
                        if let Ok(text) = name_node.utf8_text(code.as_bytes()) {
                            return Some(text.to_string());
                        }
                    }
                }
            }
        }
        "export_statement" => {
            // The exported item is a child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let child_name = extract_symbol_name(child, code);
                if child_name.is_some() {
                    return child_name;
                }
            }
        }
        "template_declaration" => {
            // C++ template: the actual declaration is a child
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                let child_name = extract_symbol_name(child, code);
                if child_name.is_some() {
                    return child_name;
                }
            }
        }
        _ => {}
    }

    None
}

// ── Qualified name building ───────────────────────────────────────────

fn build_qualified_name(parent: Option<&str>, name: &str, sep: &str) -> String {
    match parent {
        Some(p) => format!("{}{}{}", p, sep, name),
        None => name.to_string(),
    }
}

// ── Symbol type normalization ─────────────────────────────────────────

fn normalize_symbol_type(kind: &str, _lang: &str) -> String {
    match kind {
        "function_item"
        | "function_declaration"
        | "function_definition"
        | "arrow_function"
        | "generator_function_declaration" => "function".to_string(),
        "method_definition" | "method_declaration" => "method".to_string(),
        "constructor_declaration" | "constructor_definition" => "constructor".to_string(),
        "impl_item" => "impl".to_string(),
        "struct_item" | "struct_specifier" => "struct".to_string(),
        "enum_item" | "enum_declaration" | "enum_specifier" => "enum".to_string(),
        "trait_item" => "trait".to_string(),
        "interface_declaration" => "interface".to_string(),
        "class_declaration" | "abstract_class_declaration" | "class_specifier" => {
            "class".to_string()
        }
        "record_declaration" => "record".to_string(),
        "mod_item" => "module".to_string(),
        "type_item" | "type_alias_declaration" | "type_definition" | "type_declaration" => {
            "type".to_string()
        }
        "const_item" => "const".to_string(),
        "static_item" => "static".to_string(),
        "use_declaration" | "import_declaration" | "import_statement" | "import_from_statement" => {
            "import".to_string()
        }
        "lexical_declaration" | "variable_declaration" => "variable".to_string(),
        "field_declaration" => "field".to_string(),
        "assignment" => "assignment".to_string(),
        "decorated_definition" => "decorated".to_string(),
        "namespace_definition" => "namespace".to_string(),
        _ => {
            // For unknown types, return the raw kind if it looks reasonable
            if kind.len() < 30 {
                kind.to_string()
            } else {
                "unknown".to_string()
            }
        }
    }
}

// ── Parent header building ────────────────────────────────────────────

/// Build a scope-header chunk for a parent node (class/impl/trait/interface).
/// Contains the declaration line + a digest list of members, NOT the full body.
fn build_parent_header(
    node: Node,
    code: &str,
    lang: &str,
    path: &str,
    qname: &str,
    lines: &[&str],
) -> String {
    let start_row = node.start_position().row;
    let end_row = node.end_position().row;

    // Extract declaration line(s) — everything up to and including the opening brace/colon
    let decl_end = find_declaration_end(node, code, lang);
    let decl_lines: Vec<&str> = lines
        .iter()
        .take(decl_end.min(lines.len()))
        .skip(start_row)
        .copied()
        .collect();

    // Build member digest
    let symbol_type = normalize_symbol_type(node.kind(), lang);
    let members = extract_member_digest(node, code, lang);

    let lang_label = language_label(lang);
    let start_line = start_row + 1;
    let end_line = end_row + 1;
    let header = format!(
        "[{}] {}:{}-{} {} {}",
        lang_label, path, start_line, end_line, symbol_type, qname
    );

    let mut result = header;
    result.push('\n');
    for line in &decl_lines {
        result.push_str(line);
        result.push('\n');
    }
    if !members.is_empty() {
        result.push_str("// Members:\n");
        for member in &members {
            result.push_str(&format!("//   {} {}\n", member.0, member.1));
        }
    }

    result
}

/// Find the line index (0-based, exclusive) where the declaration part ends.
/// For brace-delimited languages this is the line containing the opening `{`.
fn find_declaration_end(node: Node, code: &str, lang: &str) -> usize {
    if lang == "python" {
        // Python: declaration ends at the first line with increased indentation
        let start_row = node.start_position().row;
        let lines: Vec<&str> = code.lines().collect();
        if start_row + 1 < lines.len() {
            return start_row + 2; // Include the def/class line + one more
        }
        return start_row + 1;
    }

    // Find opening brace in the node text
    if let Ok(text) = node.utf8_text(code.as_bytes()) {
        if let Some(pos) = text.find('{') {
            let lines_before_brace = text[..pos].lines().count();
            return node.start_position().row + lines_before_brace + 1;
        }
    }

    // Fallback: just the first line
    node.start_position().row + 1
}

/// Extract a digest of members from a parent node (names + types).
fn extract_member_digest(node: Node, code: &str, lang: &str) -> Vec<(String, String)> {
    let mut members = Vec::new();
    let config = nested_emit_config(lang);

    if let Some(cfg) = config {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if cfg.child_types.contains(&child.kind()) {
                let name =
                    extract_symbol_name(child, code).unwrap_or_else(|| child.kind().to_string());
                let sym_type = normalize_symbol_type(child.kind(), lang);
                members.push((sym_type, name));
            }
        }
    }

    members
}

// ── Chunk text building ───────────────────────────────────────────────

/// Build the text for a leaf chunk with a structured header line.
fn build_chunk_text(
    node: Node,
    _code: &str,
    lang: &str,
    path: &str,
    qname: &str,
    lines: &[&str],
) -> String {
    let start_row = node.start_position().row;
    let end_row = node.end_position().row;

    let symbol_type = normalize_symbol_type(node.kind(), lang);
    let lang_label = language_label(lang);
    let start_line = start_row + 1;
    let end_line = end_row + 1;

    // Build header line
    let header = format!(
        "[{}] {}:{}-{} {} {}",
        lang_label, path, start_line, end_line, symbol_type, qname
    );

    // Extract the actual code text
    let body_lines: Vec<&str> = lines
        .iter()
        .take(end_row + 1)
        .skip(start_row)
        .copied()
        .collect();
    let body = body_lines.join("\n");

    format!("{}\n{}", header, body)
}

fn language_label(lang: &str) -> &'static str {
    match lang {
        "rust" => "Rust",
        "typescript" | "tsx" => "TypeScript",
        "javascript" | "jsx" => "JavaScript",
        "python" => "Python",
        "go" => "Go",
        "java" => "Java",
        "c" => "C",
        "cpp" => "C++",
        _ => "Code",
    }
}

// ── Doc comment extraction ────────────────────────────────────────────

/// Extract doc comments above a symbol declaration.
fn extract_doc_comment(node: Node, code: &str, lang: &str) -> Option<String> {
    let start_row = node.start_position().row;
    if start_row == 0 {
        return None;
    }

    let lines: Vec<&str> = code.lines().collect();
    let mut comment_lines = Vec::new();

    match lang {
        "rust" => {
            // Walk backwards looking for /// comments
            let mut row = start_row;
            while row > 0 {
                row -= 1;
                let line = lines.get(row).map(|l| l.trim()).unwrap_or("");
                if line.starts_with("///") {
                    let content = line.trim_start_matches("///").trim_start();
                    comment_lines.push(content.to_string());
                } else if line.starts_with("//!") {
                    let content = line.trim_start_matches("//!").trim_start();
                    comment_lines.push(content.to_string());
                } else if line.is_empty() {
                    // Allow one empty line between comments
                    if row > 0 {
                        let prev = lines.get(row - 1).map(|l| l.trim()).unwrap_or("");
                        if prev.starts_with("///") || prev.starts_with("//!") {
                            continue;
                        }
                    }
                    break;
                } else {
                    break;
                }
            }
        }
        "typescript" | "tsx" | "javascript" | "jsx" | "java" | "go" => {
            // Walk backwards looking for /** */ or // comments
            let mut row = start_row;
            while row > 0 {
                row -= 1;
                let line = lines.get(row).map(|l| l.trim()).unwrap_or("");
                if line.starts_with("/**") || line.starts_with(" *") || line.starts_with("*") {
                    let content = line
                        .trim_start_matches("/**")
                        .trim_start_matches(" *")
                        .trim_start_matches('*')
                        .trim_start_matches('/')
                        .trim();
                    comment_lines.push(content.to_string());
                } else if line.starts_with("//") {
                    let content = line.trim_start_matches('/').trim_start();
                    comment_lines.push(content.to_string());
                } else if line.is_empty() {
                    if row > 0 {
                        let prev = lines.get(row - 1).map(|l| l.trim()).unwrap_or("");
                        if prev.starts_with("/**")
                            || prev.starts_with(" *")
                            || prev.starts_with("//")
                        {
                            continue;
                        }
                    }
                    break;
                } else {
                    break;
                }
            }
        }
        "python" => {
            // Walk backwards looking for """ or # comments
            let mut row = start_row;
            // Check for triple-quoted docstring right after the declaration
            // Python docstrings are typically the first statement in the body
            let body_row = start_row + 1;
            if body_row < lines.len() {
                let body_line = lines[body_row].trim();
                if body_line.starts_with("\"\"\"") || body_line.starts_with("'''") {
                    let quote = if body_line.starts_with("\"\"\"") {
                        "\"\"\""
                    } else {
                        "'''"
                    };
                    // Single-line docstring
                    if let Some(end) = body_line[3..].find(quote) {
                        let content = &body_line[3..3 + end];
                        comment_lines.push(content.to_string());
                        // Reverse since we're collecting bottom-up
                        comment_lines.reverse();
                        if comment_lines.is_empty() {
                            return None;
                        }
                        return Some(comment_lines.join("\n"));
                    }
                    // Multi-line docstring
                    let mut doc_lines = Vec::new();
                    let first = body_line.trim_start_matches(quote);
                    if !first.is_empty() {
                        doc_lines.push(first.to_string());
                    }
                    for line in lines.iter().take(lines.len()).skip(body_row + 1) {
                        let l = line.trim();
                        if l.contains(quote) {
                            let before = l.split_once(quote).map(|(b, _)| b).unwrap_or("");
                            if !before.is_empty() {
                                doc_lines.push(before.to_string());
                            }
                            break;
                        }
                        doc_lines.push(l.to_string());
                    }
                    return if doc_lines.is_empty() {
                        None
                    } else {
                        Some(doc_lines.join("\n"))
                    };
                }
            }

            // Walk backwards for # comments
            while row > 0 {
                row -= 1;
                let line = lines.get(row).map(|l| l.trim()).unwrap_or("");
                if line.starts_with('#') {
                    let content = line.trim_start_matches('#').trim_start();
                    comment_lines.push(content.to_string());
                } else if line.is_empty() {
                    if row > 0 {
                        let prev = lines.get(row - 1).map(|l| l.trim()).unwrap_or("");
                        if prev.starts_with('#') {
                            continue;
                        }
                    }
                    break;
                } else {
                    break;
                }
            }
        }
        _ => return None,
    }

    // Comments were collected bottom-up, so reverse
    comment_lines.reverse();
    if comment_lines.is_empty() {
        None
    } else {
        Some(comment_lines.join("\n"))
    }
}

// ── Small sibling merging ─────────────────────────────────────────────

/// Merge adjacent chunks under 15% of target token count.
/// Keep definitions addressable for code_def/search filters.
fn merge_small_siblings(chunks: Vec<RawChunk>, _lang: &str) -> Vec<RawChunk> {
    if chunks.is_empty() {
        return chunks;
    }

    let threshold = (TARGET_TOKEN_COUNT as f64 * SMALL_MERGE_FRACTION) as i32;
    let mut result: Vec<RawChunk> = Vec::new();

    let mut i = 0;
    while i < chunks.len() {
        let current = &chunks[i];

        // Never merge chunks with parent_symbol_path or primary definitions.
        if current.parent_symbol_path.is_some() || is_primary_semantic_symbol(&current.symbol_type)
        {
            result.push(current.clone());
            i += 1;
            continue;
        }

        let token_count = current.chunk_text.split_whitespace().count() as i32;

        // If current chunk is small, try to merge with the next small chunk
        if token_count < threshold && i + 1 < chunks.len() {
            let next = &chunks[i + 1];

            // Don't merge across parent boundaries or primary definitions.
            if next.parent_symbol_path.is_some() || is_primary_semantic_symbol(&next.symbol_type) {
                result.push(current.clone());
                i += 1;
                continue;
            }

            let next_tokens = next.chunk_text.split_whitespace().count() as i32;

            if next_tokens < threshold {
                // Merge current + next
                let merged_text = format!("{}\n\n{}", current.chunk_text, next.chunk_text);
                let merged = RawChunk {
                    chunk_text: merged_text,
                    symbol_name: format!("{},{}", current.symbol_name, next.symbol_name),
                    symbol_name_qualified: format!(
                        "{},{}",
                        current.symbol_name_qualified, next.symbol_name_qualified
                    ),
                    symbol_type: "merged".to_string(),
                    start_line: current.start_line,
                    end_line: next.end_line,
                    parent_symbol_path: None,
                    doc_comment: current.doc_comment.clone(),
                };
                result.push(merged);
                i += 2;
                continue;
            }
        }

        result.push(current.clone());
        i += 1;
    }

    // Second pass: continue merging if we created new small siblings
    // (limit to 3 passes to avoid infinite loops)
    for _ in 0..3 {
        if result.len() <= 1 {
            break;
        }
        let prev_len = result.len();
        result = merge_pass(result);
        if result.len() == prev_len {
            break;
        }
    }

    result
}

fn merge_pass(chunks: Vec<RawChunk>) -> Vec<RawChunk> {
    let threshold = (TARGET_TOKEN_COUNT as f64 * SMALL_MERGE_FRACTION) as i32;
    let mut result: Vec<RawChunk> = Vec::new();

    let mut i = 0;
    while i < chunks.len() {
        let current = &chunks[i];

        if current.parent_symbol_path.is_some() || is_primary_semantic_symbol(&current.symbol_type)
        {
            result.push(current.clone());
            i += 1;
            continue;
        }

        let token_count = current.chunk_text.split_whitespace().count() as i32;

        if token_count < threshold && i + 1 < chunks.len() {
            let next = &chunks[i + 1];

            if next.parent_symbol_path.is_some() || is_primary_semantic_symbol(&next.symbol_type) {
                result.push(current.clone());
                i += 1;
                continue;
            }

            let next_tokens = next.chunk_text.split_whitespace().count() as i32;

            if next_tokens < threshold {
                let merged_text = format!("{}\n\n{}", current.chunk_text, next.chunk_text);
                let merged = RawChunk {
                    chunk_text: merged_text,
                    symbol_name: format!("{},{}", current.symbol_name, next.symbol_name),
                    symbol_name_qualified: format!(
                        "{},{}",
                        current.symbol_name_qualified, next.symbol_name_qualified
                    ),
                    symbol_type: "merged".to_string(),
                    start_line: current.start_line,
                    end_line: next.end_line,
                    parent_symbol_path: None,
                    doc_comment: current.doc_comment.clone(),
                };
                result.push(merged);
                i += 2;
                continue;
            }
        }

        result.push(current.clone());
        i += 1;
    }

    result
}

fn is_primary_semantic_symbol(symbol_type: &str) -> bool {
    matches!(
        symbol_type,
        "function" | "method" | "class" | "struct" | "interface" | "enum" | "trait" | "impl"
    )
}

// ── CodeIndex building ────────────────────────────────────────────────

fn build_code_index(
    slug: &str,
    code: &str,
    lang: &str,
    raw_chunks: &[RawChunk],
    start_index: i32,
) -> CodeIndex {
    let mut chunks = Vec::new();
    let mut symbols = Vec::new();

    if raw_chunks.is_empty() {
        // No semantic units found — emit the whole file as one chunk
        let token_count = code.split_whitespace().count() as i32;
        chunks.push(ChunkInput {
            chunk_index: start_index,
            chunk_text: code.to_string(),
            source: ChunkSource::FencedCode,
            token_count,
            embedding: None,
            model: None,
            language: Some(lang.to_string()),
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

    for (offset, raw) in raw_chunks.iter().enumerate() {
        let chunk_index = start_index + offset as i32;
        let token_count = raw.chunk_text.split_whitespace().count() as i32;

        chunks.push(ChunkInput {
            chunk_index,
            chunk_text: raw.chunk_text.clone(),
            source: ChunkSource::FencedCode,
            token_count,
            embedding: None,
            model: None,
            language: Some(lang.to_string()),
            symbol_name: Some(raw.symbol_name_qualified.clone()),
            symbol_type: Some(raw.symbol_type.clone()),
            start_line: Some(raw.start_line),
            end_line: Some(raw.end_line),
            parent_symbol_path: raw.parent_symbol_path.clone(),
            symbol_name_qualified: Some(raw.symbol_name_qualified.clone()),
            doc_comment: raw.doc_comment.clone(),
        });

        // Only emit CodeSymbol for non-merged chunks
        if raw.symbol_type != "merged" {
            symbols.push(CodeSymbol {
                name: raw.symbol_name.clone(),
                qualified_name: raw.symbol_name_qualified.clone(),
                symbol_type: raw.symbol_type.clone(),
                language: lang.to_string(),
                start_line: raw.start_line,
                end_line: raw.end_line,
                parent_symbol: raw.parent_symbol_path.clone(),
            });
        }
    }

    // Extract edges from AST call expressions + fallback to regex-based inference
    // Use the symbols list (which has non-merged symbol spans) for edge resolution
    let ast_edges = extract_ast_edges(slug, code, lang, &symbols);
    let edges = if ast_edges.is_empty() {
        // Fallback: use regex-based edge inference
        infer_edges_from_raw_chunks(slug, raw_chunks)
    } else {
        ast_edges
    };

    CodeIndex {
        chunks,
        symbols,
        edges,
    }
}

// ── AST-based edge extraction ─────────────────────────────────────────

/// Extract call-site edges from the AST by walking call_expression nodes.
fn extract_ast_edges(
    slug: &str,
    code: &str,
    lang: &str,
    symbols: &[CodeSymbol],
) -> Vec<CodeEdgeInput> {
    let ts_lang = match get_language(lang) {
        Some(l) => l,
        None => return Vec::new(),
    };

    let mut parser = Parser::new();
    if parser.set_language(&ts_lang).is_err() {
        return Vec::new();
    }

    let tree = match parser.parse(code, None) {
        Some(t) => t,
        None => return Vec::new(),
    };

    // Build a map from line number to qualified symbol name
    let mut line_to_symbol: HashMap<usize, String> = HashMap::new();
    for sym in symbols {
        for line in sym.start_line..=sym.end_line {
            line_to_symbol.insert(line as usize, sym.qualified_name.clone());
        }
    }

    // Build a set of known qualified names for resolution
    let known_symbols: HashSet<String> = symbols.iter().map(|s| s.qualified_name.clone()).collect();

    // Also index by simple name
    let name_to_qualified: HashMap<String, String> = symbols
        .iter()
        .map(|s| {
            let simple = s
                .qualified_name
                .rsplit(['.', ':'])
                .next()
                .unwrap_or(&s.qualified_name)
                .to_string();
            (simple, s.qualified_name.clone())
        })
        .collect();

    // Collect all call expressions from the AST
    let calls = collect_call_expressions(tree.root_node(), code);

    let mut edges = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();

    for (call_name, call_line) in calls {
        // Resolve caller: find which symbol contains this line
        let caller = line_to_symbol.get(&call_line).cloned().unwrap_or_default();

        // Resolve callee: try exact match, then simple name
        let callee = if known_symbols.contains(&call_name) {
            call_name.clone()
        } else if let Some(qualified) = name_to_qualified.get(&call_name) {
            qualified.clone()
        } else {
            // Try the last segment of a dotted/path name
            let simple = call_name.rsplit(['.', ':']).next().unwrap_or(&call_name);
            if let Some(qualified) = name_to_qualified.get(simple) {
                qualified.clone()
            } else {
                continue; // Can't resolve the callee
            }
        };

        // Skip self-references
        if caller == callee {
            continue;
        }

        let key = (caller.clone(), callee.clone());
        if seen.insert(key.clone()) {
            edges.push(CodeEdgeInput {
                from_slug: slug.to_string(),
                from_symbol: caller.clone(),
                to_slug: slug.to_string(),
                to_symbol: callee.clone(),
                edge_type: "calls".to_string(),
                confidence: 0.9,
                context: None,
                from_chunk_id: None,
                to_chunk_id: None,
                from_symbol_qualified: Some(caller),
                to_symbol_qualified: Some(callee),
            });
        }
    }

    edges
}

/// Collect all call expressions from the AST, returning (callee_name, line_number).
fn collect_call_expressions(node: Node, code: &str) -> Vec<(String, usize)> {
    let mut calls = Vec::new();

    // Handle call_expression nodes
    if node.kind() == "call_expression" {
        // The callee is the first child (or "function" field)
        if let Some(func_node) = node.child_by_field_name("function") {
            if let Ok(text) = func_node.utf8_text(code.as_bytes()) {
                let callee = simplify_call_name(text);
                if !is_builtin_or_keyword(&callee) {
                    calls.push((callee, node.start_position().row + 1));
                }
            }
        } else if let Some(child) = node.child(0) {
            if let Ok(text) = child.utf8_text(code.as_bytes()) {
                let callee = simplify_call_name(text);
                if !is_builtin_or_keyword(&callee) {
                    calls.push((callee, node.start_position().row + 1));
                }
            }
        }
    }

    // Recurse into children
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        calls.extend(collect_call_expressions(child, code));
    }

    calls
}

/// Simplify a call expression name by removing arguments, generics, etc.
fn simplify_call_name(name: &str) -> String {
    // Remove generic parameters: foo::<T>() -> foo
    let name = if let Some(pos) = name.find("::<") {
        &name[..pos]
    } else if let Some(pos) = name.find('<') {
        &name[..pos]
    } else {
        name
    };

    // Remove parentheses: foo() -> foo
    let name = if let Some(pos) = name.find('(') {
        &name[..pos]
    } else {
        name
    };

    // For method chains, keep the last meaningful segment: self.a.b() -> b
    // But keep dotted paths for resolution: obj.method -> obj.method
    name.trim().to_string()
}

fn is_builtin_or_keyword(name: &str) -> bool {
    let builtins = [
        "if",
        "for",
        "while",
        "match",
        "switch",
        "catch",
        "return",
        "sizeof",
        "Some",
        "Ok",
        "Err",
        "None",
        "True",
        "False",
        "vec!",
        "vec",
        "println!",
        "println",
        "format!",
        "format",
        "print",
        "panic!",
        "assert",
        "assert_eq",
        "assert_ne",
        "todo",
        "unimplemented",
        "unreachable",
        "eprintln",
        "dbg",
        "panic",
        "super",
        "Self",
        "self",
        "console",
        "println",
        "System",
        "String",
        "new",
        "drop",
        "clone",
        "copy",
        "default",
        "from",
        "into",
        "as_ref",
        "as_mut",
        "to_string",
        "len",
        "is_empty",
        "push",
        "pop",
        "get",
        "set",
        "insert",
        "remove",
        "contains",
        "iter",
        "map",
        "filter",
        "collect",
        "unwrap",
        "expect",
        "is_ok",
        "is_err",
        "is_some",
        "is_none",
    ];
    builtins.contains(&name)
}

// ── Regex-based edge inference (fallback) ─────────────────────────────

/// Regex-based edge inference as a fallback when AST-based extraction
/// yields no edges. Operates directly on RawChunk data.
fn infer_edges_from_raw_chunks(slug: &str, raw_chunks: &[RawChunk]) -> Vec<CodeEdgeInput> {
    // Build symbol lookup: simple name -> qualified name
    let mut symbol_lookup: HashMap<String, String> = HashMap::new();
    for raw in raw_chunks {
        if raw.symbol_type == "merged" {
            continue;
        }
        symbol_lookup.insert(raw.symbol_name.clone(), raw.symbol_name_qualified.clone());
        symbol_lookup.insert(
            raw.symbol_name_qualified.clone(),
            raw.symbol_name_qualified.clone(),
        );
    }

    let mut edges = Vec::new();
    let mut seen = HashSet::new();

    for raw in raw_chunks {
        if raw.symbol_type == "merged" {
            continue;
        }
        let tokens = call_tokens_from_body(&raw.chunk_text);
        for token in tokens {
            if token == raw.symbol_name || token == raw.symbol_name_qualified {
                continue;
            }
            let lookup = symbol_lookup.get(&token).or_else(|| {
                token
                    .rsplit('.')
                    .next()
                    .and_then(|name| symbol_lookup.get(name))
            });
            if let Some(target) = lookup {
                let key = format!("{}->{}", raw.symbol_name_qualified, target);
                if seen.insert(key) {
                    edges.push(CodeEdgeInput {
                        from_slug: slug.to_string(),
                        from_symbol: raw.symbol_name_qualified.clone(),
                        to_slug: slug.to_string(),
                        to_symbol: target.clone(),
                        edge_type: "calls".to_string(),
                        confidence: 0.85,
                        context: Some(format!(
                            "{} references {}",
                            raw.symbol_name_qualified, target
                        )),
                        from_chunk_id: None,
                        to_chunk_id: None,
                        from_symbol_qualified: Some(raw.symbol_name_qualified.clone()),
                        to_symbol_qualified: Some(target.clone()),
                    });
                }
            }
        }
    }

    edges
}

/// Extract call tokens from code body using regex.
/// Simplified version of code_index::call_tokens.
fn call_tokens_from_body(body: &str) -> HashSet<String> {
    use regex::Regex;
    static CALL_RE: OnceLock<Regex> = OnceLock::new();
    let re = CALL_RE.get_or_init(|| {
        Regex::new(r"(?P<name>[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)?)\s*\(").unwrap()
    });

    re.captures_iter(body)
        .filter_map(|caps| caps.name("name").map(|m| m.as_str().to_string()))
        .filter(|name| !is_builtin_or_keyword(name))
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rust_language_from_slug() {
        assert_eq!(
            detect_language("code/lib.rs", None),
            Some("rust".to_string())
        );
        assert_eq!(
            detect_language("code/mod.ts", None),
            Some("typescript".to_string())
        );
        assert_eq!(
            detect_language("code/app.tsx", None),
            Some("tsx".to_string())
        );
        assert_eq!(
            detect_language("code/main.py", None),
            Some("python".to_string())
        );
        assert_eq!(
            detect_language("code/main.go", None),
            Some("go".to_string())
        );
        assert_eq!(
            detect_language("code/Main.java", None),
            Some("java".to_string())
        );
        assert_eq!(detect_language("code/main.c", None), Some("c".to_string()));
        assert_eq!(
            detect_language("code/main.cpp", None),
            Some("cpp".to_string())
        );
        assert_eq!(detect_language("code/unknown.txt", None), None);
    }

    #[test]
    fn respects_explicit_language() {
        assert_eq!(
            detect_language("code/unknown.txt", Some("rust")),
            Some("rust".to_string())
        );
    }

    #[test]
    fn chunks_rust_functions() {
        let code = r#"
/// Doc for alpha.
/// This function does something important.
pub fn alpha() -> i32 {
    let result = beta() + 1;
    let more = result * 2;
    let extra = more + 3;
    let final_val = extra - 1;
    println!("The result is {}", final_val);
    let another = final_val * 3;
    let yet_another = another + 7;
    yet_another
}

/// Doc for beta.
fn beta() -> i32 {
    let x = 42;
    let y = x + 10;
    let z = y * 2;
    let w = z - 5;
    let v = w / 3;
    let u = v + 100;
    let t = u * 4;
    t
}
"#;
        let indexed = chunk_code_tree_sitter("code/lib.rs", code, None, 0);
        // With sufficiently large functions, they should not be merged
        assert!(
            indexed.symbols.iter().any(|s| s.name == "alpha"),
            "Should find alpha symbol, got: {:?}",
            indexed.symbols.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert!(
            indexed.symbols.iter().any(|s| s.name == "beta"),
            "Should find beta symbol"
        );
        // Check doc comment extraction
        assert!(
            indexed.chunks.iter().any(|c| c.doc_comment.is_some()),
            "Should extract doc comment for alpha"
        );
    }

    #[test]
    fn chunks_rust_impl_with_methods() {
        let code = r#"
pub struct User {
    name: String,
    email: String,
    age: u32,
}

impl User {
    /// Create a new user instance
    pub fn new(name: String, email: String, age: u32) -> Self {
        let validated_name = name.trim().to_string();
        let validated_email = email.trim().to_string();
        Self { name: validated_name, email: validated_email, age }
    }

    /// Greet the user
    pub fn greet(&self) -> String {
        let greeting = format!("Hello, {}!", self.name);
        let details = format!("You are {} years old.", self.age);
        format!("{} {}", greeting, details)
    }

    /// Update the user email
    pub fn update_email(&mut self, new_email: String) {
        let validated = new_email.trim().to_string();
        self.email = validated;
    }
}
"#;
        let indexed = chunk_code_tree_sitter("code/user.rs", code, None, 0);
        // Method chunks should have parent_symbol_path set
        let method_chunks: Vec<_> = indexed
            .chunks
            .iter()
            .filter(|c| c.parent_symbol_path.is_some())
            .collect();

        assert!(
            method_chunks.len() >= 2,
            "Should have at least 2 method chunks with parent_symbol_path, got {}",
            method_chunks.len()
        );

        // Check that methods have the parent path set
        for mc in &method_chunks {
            assert!(
                mc.parent_symbol_path.as_deref() == Some("impl User")
                    || mc.parent_symbol_path.as_deref() == Some("User"),
                "Method parent_symbol_path should reference User, got {:?}",
                mc.parent_symbol_path
            );
        }

        // Check qualified names include the impl path
        assert!(
            indexed
                .symbols
                .iter()
                .any(|s| s.qualified_name.contains("User") && s.qualified_name.contains("new")),
            "Should have User::new qualified name, got: {:?}",
            indexed
                .symbols
                .iter()
                .map(|s| &s.qualified_name)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunks_python_class_with_methods() {
        let code = r#"
class Greeter:
    """A simple greeter class."""

    def hello(self):
        """Say hello to someone."""
        greeting = self.world()
        message = f"Hello {greeting}"
        print(message)
        extra_info = "from Greeter"
        return message

    def world(self):
        """Say world."""
        result = "World"
        upper = result.upper()
        return upper
"#;
        let indexed = chunk_code_tree_sitter("code/greeter.py", code, None, 0);
        // Check Greeter class is found
        let has_greeter = indexed.symbols.iter().any(|s| s.name == "Greeter")
            || indexed.chunks.iter().any(|c| {
                c.symbol_name
                    .as_ref()
                    .is_some_and(|n| n.contains("Greeter"))
            });
        assert!(has_greeter, "Should find Greeter class");

        // Check Greeter.hello is found (methods should be emitted with parent path)
        let has_hello = indexed
            .symbols
            .iter()
            .any(|s| s.name == "hello" && s.parent_symbol.as_deref() == Some("Greeter"))
            || indexed.chunks.iter().any(|c| {
                c.parent_symbol_path.as_deref() == Some("Greeter")
                    && c.symbol_name.as_ref().is_some_and(|n| n.contains("hello"))
            });
        assert!(
            has_hello,
            "Should find Greeter.hello, got: {:?}",
            indexed
                .symbols
                .iter()
                .map(|s| format!("{}:{}", s.name, s.qualified_name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn chunks_typescript_functions() {
        let code = r#"
export function searchKeyword(query: string): SearchResult[] {
    const normalized = normalizeQuery(query);
    const results = doSearch(normalized);
    return results.filter(isRelevant);
}

function doSearch(query: string): SearchResult[] {
    return [];
}

function normalizeQuery(q: string): string {
    return q.toLowerCase().trim();
}

function isRelevant(r: SearchResult): boolean {
    return r.score > 0.5;
}
"#;
        let indexed = chunk_code_tree_sitter("code/search.ts", code, None, 0);
        // Check that at least one symbol or chunk references searchKeyword
        let found_search = indexed.symbols.iter().any(|s| s.name == "searchKeyword")
            || indexed.chunks.iter().any(|c| {
                c.symbol_name
                    .as_ref()
                    .is_some_and(|n| n.contains("searchKeyword"))
            });
        assert!(found_search, "Should find searchKeyword");
    }

    #[test]
    fn falls_back_to_regex_for_unsupported_language() {
        let code = "some code here";
        let indexed = chunk_code_tree_sitter("code/file.xyz", code, None, 0);
        // Should still produce output via regex fallback
        assert!(!indexed.chunks.is_empty());
    }

    #[test]
    fn empty_code_produces_single_file_chunk() {
        let code = "";
        let indexed = chunk_code_tree_sitter("code/lib.rs", code, None, 0);
        // Empty code should produce a single file chunk
        assert_eq!(indexed.chunks.len(), 1);
        assert_eq!(indexed.chunks[0].symbol_type.as_deref(), Some("file"));
    }

    #[test]
    fn small_sibling_merging() {
        let code = r#"
const A: i32 = 1;
const B: i32 = 2;
const C: i32 = 3;

pub fn main_function() -> i32 {
    let total = A + B + C + 100 + 200 + 300 + 400 + 500 + 600;
    let result = total * 2 + 50;
    let final_val = result - 10;
    final_val
}
"#;
        let indexed = chunk_code_tree_sitter("code/lib.rs", code, None, 0);
        // Small const items should be merged; main_function should stand alone
        let has_merged = indexed
            .chunks
            .iter()
            .any(|c| c.symbol_type.as_deref() == Some("merged"));
        let has_main = indexed.symbols.iter().any(|s| s.name == "main_function")
            || indexed
                .chunks
                .iter()
                .any(|c| c.symbol_name.as_deref() == Some("main_function"));
        assert!(has_merged, "Should merge small const siblings");
        assert!(has_main, "Should find main_function");
    }

    #[test]
    fn extracts_call_edges() {
        let code = r#"
fn alpha() -> i32 {
    let x = beta();
    let y = x + 1;
    let z = y * 2;
    let w = z + 10;
    let v = w - 5;
    let extra = v * 3;
    let more = extra + 7;
    let result = more - 2;
    result
}

fn beta() -> i32 {
    let y = gamma();
    let z = y * 2;
    let w = z + 10;
    let v = w - 5;
    let extra = v * 3;
    let more = extra + 7;
    let result = more - 2;
    result
}

fn gamma() -> i32 {
    let result = 42;
    let doubled = result * 2;
    let tripled = doubled + 10;
    let final_val = tripled - 5;
    let extra = final_val * 3;
    let more = extra + 7;
    let outcome = more - 2;
    outcome
}
"#;
        let indexed = chunk_code_tree_sitter("code/lib.rs", code, None, 0);
        // Should detect at least one call edge (alpha->beta or beta->gamma)
        let has_call_edge = indexed.edges.iter().any(|e| {
            (e.from_symbol.contains("alpha") && e.to_symbol.contains("beta"))
                || (e.from_symbol.contains("beta") && e.to_symbol.contains("gamma"))
        });
        assert!(
            has_call_edge,
            "Should detect call edges, got: {:?}",
            indexed
                .edges
                .iter()
                .map(|e| format!("{}->{}", e.from_symbol, e.to_symbol))
                .collect::<Vec<_>>()
        );
    }
}
