//! 存档工作流（`<project_root>/workflows/<name>.dwf.js` /
//! `<workflow_home>/workflows/<name>.dwf.js`）与参数声明。
//!
//! 文件头元数据块：
//! ```text
//! /* rclaw-workflow
//! name: pr-review
//! description: ...
//! when_to_use: ...
//! args:
//!   repo:
//!     type: string
//!     required: true
//! */
//! ```
//! 正文是普通脚本。YAML 解析失败 / 声明非法一律拒绝保存与加载。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::workflow_paths::write_atomic;

const META_PREFIX: &str = "/* rclaw-workflow";
const META_SUFFIX: &str = "*/";

/// 参数声明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgDecl {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub required: Option<bool>,
    #[serde(default)]
    pub default: Option<Value>,
}

/// 存档元数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedMeta {
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    #[serde(default)]
    pub args: BTreeMap<String, ArgDecl>,
}

/// 解析存档文件：返回（脚本正文，参数声明）。
pub fn parse_saved_script(text: &str) -> Result<(String, Value), String> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(META_PREFIX) {
        return Ok((text.to_string(), json!({})));
    }
    let end = trimmed
        .find(META_SUFFIX)
        .ok_or_else(|| "rclaw-workflow 元数据块没有闭合 */".to_string())?;
    let yaml = &trimmed[META_PREFIX.len()..end];
    let meta: SavedMeta =
        serde_yaml::from_str(yaml).map_err(|error| format!("元数据 YAML 解析失败: {error}"))?;
    validate_args_decl(&meta.args)?;
    let script = trimmed[end + META_SUFFIX.len()..].trim_start().to_string();
    let declared = serde_json::to_value(&meta.args).map_err(|error| error.to_string())?;
    Ok((script, declared))
}

/// 解析元数据（ListSavedWorkflows 用）。
pub fn parse_saved_meta(text: &str) -> Result<Option<SavedMeta>, String> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(META_PREFIX) {
        return Ok(None);
    }
    let end = trimmed
        .find(META_SUFFIX)
        .ok_or_else(|| "rclaw-workflow 元数据块没有闭合 */".to_string())?;
    let yaml = &trimmed[META_PREFIX.len()..end];
    let meta: SavedMeta =
        serde_yaml::from_str(yaml).map_err(|error| format!("元数据 YAML 解析失败: {error}"))?;
    Ok(Some(meta))
}

fn valid_arg_name(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// 参数声明校验（Save 时）。
pub fn validate_args_decl(args: &BTreeMap<String, ArgDecl>) -> Result<(), String> {
    for (name, decl) in args {
        if !valid_arg_name(name) {
            return Err(format!("参数名 '{name}' 非法（[A-Za-z_][A-Za-z0-9_]*）"));
        }
        if !matches!(decl.kind.as_str(), "string" | "number" | "boolean" | "json") {
            return Err(format!("参数 '{name}' 的 type 非法: {}", decl.kind));
        }
        if decl.required.unwrap_or(false) && decl.default.is_some() {
            return Err(format!("参数 '{name}' 不能同时 required 与 default"));
        }
        if let Some(default) = &decl.default {
            if !value_matches(default, &decl.kind) {
                return Err(format!("参数 '{name}' 的 default 与 type 不符"));
            }
        }
    }
    Ok(())
}

fn value_matches(value: &Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "json" => true,
        _ => false,
    }
}

/// Create 时按声明填充 / 校验参数。
pub fn apply_args(declared: &Value, provided: &Value) -> Result<Value, String> {
    let map = provided
        .as_object()
        .ok_or_else(|| "args 必须是 JSON 对象".to_string())?;
    let declarations = declared.as_object().cloned().unwrap_or_default();
    for key in map.keys() {
        if !declarations.contains_key(key) {
            return Err(format!("未知参数 '{key}'"));
        }
    }
    let mut out = serde_json::Map::new();
    for (name, decl) in declarations {
        let required = decl
            .get("required")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let default = decl.get("default");
        match map.get(&name) {
            Some(value) => {
                let kind = decl
                    .get("type")
                    .and_then(Value::as_str)
                    .ok_or_else(|| format!("参数 '{name}' 缺少 type"))?;
                if !value_matches(value, kind) {
                    return Err(format!("参数 '{name}' 类型不符（期望 {kind}）"));
                }
                out.insert(name, value.clone());
            }
            None => {
                if let Some(default) = default {
                    out.insert(name, default.clone());
                } else if required {
                    return Err(format!("缺少必填参数 '{name}'"));
                }
            }
        }
    }
    Ok(Value::Object(out))
}

/// 保存工作流文件（原子替换；调用方负责 overwwrite / expected_hash 语义）。
/// 写存档文件；返回 `(path, written)`——内容与现有文件完全一致时不落盘（§5.8「无变更不写」），
/// `written=false`。
pub fn write_saved(dir: &Path, meta: &SavedMeta, script: &str) -> Result<(PathBuf, bool), String> {
    let path = dir.join(format!("{}.dwf.js", meta.name));
    let yaml = serde_yaml::to_string(meta).map_err(|error| error.to_string())?;
    let content = format!("{META_PREFIX}\n{}*/\n\n{}\n", yaml, script.trim());
    if std::fs::read(&path).is_ok_and(|existing| existing == content.as_bytes()) {
        return Ok((path, false));
    }
    std::fs::create_dir_all(dir).map_err(|error| format!("创建存档目录失败: {error}"))?;
    write_atomic(&path, content.as_bytes())?;
    Ok((path, true))
}

/// 列出目录下的存档（按文件名排序）。
pub fn list_saved(dir: &Path, scope: &str) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("js") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        let Some(name) = stem.strip_suffix(".dwf") else {
            continue;
        };
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        match parse_saved_meta(&text) {
            Ok(Some(meta)) => out.push(json!({
                "name": meta.name,
                "description": meta.description,
                "when_to_use": meta.when_to_use,
                "scope": scope,
                "path": path.to_string_lossy(),
                "args": meta.args,
                "shadowed": false,
            })),
            Ok(None) => out.push(json!({
                "name": name,
                "description": "",
                "when_to_use": "",
                "scope": scope,
                "path": path.to_string_lossy(),
                "args": {},
                "shadowed": false,
            })),
            Err(error) => tracing::warn!("存档 {} 元数据解析失败: {error}", path.display()),
        }
    }
    out.sort_by(|left, right| {
        left.get("name")
            .and_then(Value::as_str)
            .cmp(&right.get("name").and_then(Value::as_str))
    });
    out
}

/// 脚本里的 `model("...")` / `model: "..."` 字面量（allowed_models 上限用）。
pub fn model_literals(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let patterns = [
        r#"\bmodel\s*\(\s*["']([^"']+)["']"#,
        r#"\bmodel\s*:\s*["']([^"']+)["']"#,
    ];
    for pattern in patterns {
        if let Ok(regex) = regex::Regex::new(pattern) {
            for capture in regex.captures_iter(script) {
                if let Some(model) = capture.get(1) {
                    let model = model.as_str().to_string();
                    if model.len() <= 200 && !out.contains(&model) {
                        out.push(model);
                    }
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out.truncate(16);
    out
}

/// 给每个 `hole(...)` 调用注入静态源码序号（第三参数）。
///
/// 运行时 `hole(name,prompt,site)` 把 site 传给宿主，hole_id 直接携带源码序号；
/// Fill 按序号替换调用点，分支跳过前面的同名 hole 也不会打错位置。
/// 快照/脚本 hash 都基于注入后的文本（作者原文只在 Save 时写盘）。
pub fn annotate_hole_sites(script: &str) -> (String, usize) {
    let sites = find_hole_calls(script);
    let mut out = script.to_string();
    for (index, site) in sites.iter().enumerate().rev() {
        // 已注入过序号（resume / fill 后的快照）：原位改写成当前序号——幂等，且补洞正文里
        // 新出现的 hole 会让后面的调用点重新编号。
        if let Some((arg_start, arg_end)) = site.site_arg {
            out.replace_range(arg_start..arg_end, &index.to_string());
            continue;
        }
        let insert_at = site.end.saturating_sub(1);
        // 尾随逗号 `hole("x",)`：插在逗号后而不是再造一个逗号。
        let before = &script[..insert_at];
        let trimmed = before.trim_end();
        let token = if trimmed.ends_with(',') {
            format!(" {index}")
        } else {
            format!(", {index}")
        };
        out.insert_str(insert_at, &token);
    }
    (out, sites.len())
}

/// 一个 hole 调用点（字节区间，左闭右开）。
#[derive(Debug, Clone)]
pub struct HoleCallSite {
    pub start: usize,
    pub end: usize,
    pub name: String,
    /// 末参数若是裸整数字面量（此前注入的源码序号），其字节区间。
    pub site_arg: Option<(usize, usize)>,
}

/// 调用括号内末尾的顶层裸整数参数（`hole("x", "p", 3)` 的 `3`）的字节区间。
fn trailing_site_arg(masked: &str, open: usize, close: usize) -> Option<(usize, usize)> {
    let inside = &masked[open + 1..close];
    let mut depth = 0i32;
    let mut last_comma: Option<usize> = None;
    for (offset, byte) in inside.bytes().enumerate() {
        match byte {
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => last_comma = Some(offset),
            _ => {}
        }
    }
    let after = last_comma? + 1;
    let tail = &inside[after..];
    let trimmed = tail.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let start = open + 1 + after + (tail.len() - tail.trim_start().len());
    Some((start, start + trimmed.len()))
}

/// 扫描脚本里所有 `hole(...)` 调用点（跳过字符串 / 注释）。
///
/// 运行时按调用顺序编号；Fill 用 `hole_id` 的序号定位同一调用点。tail hole 与
/// 分支级 / 嵌套 hole 用同一机制（替换该调用表达式为 async IIFE）。
pub fn find_hole_calls(script: &str) -> Vec<HoleCallSite> {
    let masked = super::workflow::mask_strings_and_comments(script);
    let bytes = masked.as_bytes();
    let mut out = Vec::new();
    let mut index = 0usize;
    while index + 4 <= bytes.len() {
        if &bytes[index..index + 4] == b"hole" {
            let before_ok = index == 0 || !is_ident(bytes[index - 1]);
            let after = index + 4;
            let after_ok = after >= bytes.len() || !is_ident(bytes[after]);
            if before_ok && after_ok {
                let mut cursor = after;
                while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
                    cursor += 1;
                }
                if bytes.get(cursor) == Some(&b'(') {
                    if let Some(close) = matching_paren(&masked, cursor) {
                        let name = first_string(&script[cursor + 1..close]).unwrap_or_default();
                        out.push(HoleCallSite {
                            start: index,
                            end: close + 1,
                            name,
                            site_arg: trailing_site_arg(&masked, cursor, close),
                        });
                        index = close + 1;
                        continue;
                    }
                }
            }
        }
        index += 1;
    }
    out
}

fn is_ident(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn matching_paren(masked: &str, open: usize) -> Option<usize> {
    let bytes = masked.as_bytes();
    let mut depth = 0i32;
    for (offset, byte) in bytes.iter().enumerate().skip(open) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

fn first_string(inside: &str) -> Option<String> {
    let trimmed = inside.trim_start();
    let quote = trimmed.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let mut out = String::new();
    let mut escaped = false;
    for ch in trimmed[quote.len_utf8()..].chars() {
        if escaped {
            out.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == quote {
            return Some(out);
        }
        out.push(ch);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_tail_and_branch_holes() {
        let script = r#"
const a = await agent("x").ask("hi");
if (a) { const v = await hole("branch", "p"); }
const tail = await hole("tail");
"#;
        let sites = find_hole_calls(script);
        assert_eq!(sites.len(), 2);
        assert_eq!(sites[0].name, "branch");
        assert_eq!(sites[1].name, "tail");
        assert!(script[sites[0].start..sites[0].end].starts_with("hole("));
    }

    #[test]
    fn args_validation_and_defaults() {
        let declared = json!({
            "topic": {"type": "string", "required": true},
            "depth": {"type": "number", "default": 2},
        });
        let args = apply_args(&declared, &json!({"topic": "x"})).unwrap();
        assert_eq!(args["topic"], "x");
        assert_eq!(args["depth"], 2);
        assert!(apply_args(&declared, &json!({})).is_err());
        assert!(apply_args(&declared, &json!({"topic": 1})).is_err());
        assert!(apply_args(&declared, &json!({"unknown": 1})).is_err());
    }
}
