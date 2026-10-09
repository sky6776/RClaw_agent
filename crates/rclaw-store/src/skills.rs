//! Skills 摘要抽取。
//!
//! 技能加载：扫描 `agent/skills/*.md`、`agent/skills/*/SKILL.md`
//! 以及包格式 `agent/skills/<package>/skills/*.md`，
//! 抽取每个 skill 的标题（首行 `# Title` 或 front matter `name`）
//! 与描述（首段或 front matter `description`），
//! 拼成供系统提示引用的摘要。

use std::fs;
use std::path::Path;

use crate::fs::Layout;

/// 单个 skill 的摘要项。
#[derive(Debug, Clone)]
pub struct SkillSummary {
    pub title: String,
    pub description: String,
    pub path: String,
}

/// 扫描 skills 目录，返回摘要列表。
///
/// 单个文件读取失败或格式不全不会阻断整体，但会区分 NotFound 与其它错误：
/// - `NotFound`：目录不存在、文件被删，静默跳过（无 skill 正常情况）；
/// - 其它错误（权限、磁盘故障、非法 UTF-8）：打 warn 日志后继续处理剩余文件，
///   与 SOUL/USER、长期记忆的错误处理策略保持一致。
pub fn build_summaries(layout: &Layout) -> Vec<SkillSummary> {
    build_summaries_in(&layout.skills_dir)
}

/// 扫描指定 skills 目录，返回摘要列表（供 `SkillListTool` 等无 [`Layout`] 的调用方复用）。
///
/// 错误策略与 [`build_summaries`] 一致：`NotFound` 静默跳过，其它 IO 错误 warn 后继续处理剩余项。
pub fn build_summaries_in(skills_dir: &Path) -> Vec<SkillSummary> {
    let dir = skills_dir;
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // skills 目录尚未创建 → 正常，无 skill。
            return Vec::new();
        }
        Err(e) => {
            tracing::warn!(
                "skills 目录读取失败，系统提示中不含技能摘要: {e} path={}",
                dir.display()
            );
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    "skill 目录项读取失败，已跳过该单项（其它 skill 继续处理）: {e} path={}",
                    dir.display()
                );
                continue;
            }
        };
        for path in skill_entry_paths(&entry.path()) {
            match fs::read_to_string(&path) {
                Ok(text) => {
                    if let Some(s) = parse_one(&text, &path) {
                        out.push(s);
                    } else {
                        tracing::warn!("skill 文件解析失败，已跳过: path={}", path.display());
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // 文件在 read_dir 后被删除 → 正常跳过。
                }
                Err(e) => {
                    tracing::warn!("skill 文件读取失败，已跳过: {e} path={}", path.display());
                }
            }
        }
    }
    out
}

/// 索引里单条描述的最大字符数：描述是从 SKILL.md 首段抽的，可能很长；
/// 不截断会随技能数量线性撑大每轮系统提示（Hermes SKILL_PROMPT_DESC_LIMIT 同款）。
const INDEX_DESC_MAX_CHARS: usize = 60;
/// 可靠性标注的最小结果样本数：低于该样本量不标注，避免小样本噪声误导选型。
pub const INDEX_RELIABILITY_MIN_SAMPLES: u64 = 3;

/// 索引行附带的可靠性样本：键为顶层技能目录名（skills/&lt;name&gt;/SKILL.md 的 &lt;name&gt;），
/// 值为 (成功次数, 总次数)。来源是 SkillUsageStore 的结果遥测。
pub type SkillStatSamples = std::collections::BTreeMap<String, (u64, u64)>;

/// 从摘要路径提取顶层技能目录名（skills/&lt;name&gt;/SKILL.md → &lt;name&gt;）。
///
/// 平铺 .md 与包内技能（&lt;pkg&gt;/skills/...）返回 `None`——它们不参与可靠性标注：
/// 包内技能的使用遥测按包名归因（见 runtime 的 top_level_skill_name），若在这里按
/// 内层目录名反查，会与同名顶层技能的样本串数据。包内路径的特征是含 ≥2 个
/// `skills` 路径段（skills 根 + 包内 skills 子目录），据此排除。
pub fn top_level_skill_dir_name(path: &str) -> Option<String> {
    let normalized = path.replace('\\', "/");
    let mut segments = normalized.rsplit('/');
    let file = segments.next()?;
    if !file.eq_ignore_ascii_case("SKILL.md") {
        return None;
    }
    let name = segments.next()?;
    if name.is_empty() || name.eq_ignore_ascii_case("skills") {
        return None;
    }
    // 顶层技能：skills 根之上不应再出现第二个 skills 段（那是包内 skills 子目录）。
    let skills_roots = segments
        .filter(|seg| seg.eq_ignore_ascii_case("skills"))
        .count();
    if skills_roots != 1 {
        return None;
    }
    Some(name.to_string())
}

/// 把摘要列表格式化成系统提示里的一段文本。
pub fn summaries_to_text(items: &[SkillSummary]) -> String {
    summaries_to_text_with_stats(items, &SkillStatSamples::default())
}

/// 同 [`summaries_to_text`]，但样本充足的技能行附带可靠性标注
/// （`近N次:S成F败`），让模型在选型时能感知「该技能近期持续失败」。
pub fn summaries_to_text_with_stats(items: &[SkillSummary], stats: &SkillStatSamples) -> String {
    let mut buf = String::new();
    for s in items {
        let desc = truncate_chars(&s.description, INDEX_DESC_MAX_CHARS);
        let reliability = top_level_skill_dir_name(&s.path)
            .and_then(|name| stats.get(&name))
            .filter(|(_, total)| *total >= INDEX_RELIABILITY_MIN_SAMPLES)
            .map(|(success, total)| format!("（近{total}次：{success}成{}败）", total - success));
        match reliability {
            Some(tag) => buf.push_str(&format!(
                "- **{}**: {} {} (read with: read {})\n",
                s.title, desc, tag, s.path
            )),
            None => buf.push_str(&format!(
                "- **{}**: {} (read with: read {})\n",
                s.title, desc, s.path
            )),
        }
    }
    buf
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn is_markdown(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("md")
        && path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'))
}

fn skill_entry_paths(path: &Path) -> Vec<std::path::PathBuf> {
    if is_markdown(path) {
        return vec![path.to_path_buf()];
    }
    // 技能包必须是目录。普通文件（如 skill_manage 写入的 .evolution.log、.DS_Store、
    // 编辑器临时文件等运行期产物）既不是 markdown 也不是目录，直接跳过；否则会 fallthrough
    // 到 package_skill_paths，对「<文件>/skills」做 read_dir 从而触发
    // "Not a directory (os error 20)" 告警。
    if !path.is_dir() {
        return Vec::new();
    }
    let skill_md = path.join("SKILL.md");
    if skill_md.is_file() {
        return vec![skill_md];
    }
    package_skill_paths(path)
}

fn package_skill_paths(path: &Path) -> Vec<std::path::PathBuf> {
    let dir = path.join("skills");
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            tracing::warn!("skill 包目录读取失败，已跳过: {e} path={}", dir.display());
            return Vec::new();
        }
    };

    let mut out = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                tracing::warn!(
                    "skill 包目录项读取失败，已跳过该项: {e} path={}",
                    dir.display()
                );
                continue;
            }
        };
        let p = entry.path();
        if is_markdown(&p) {
            out.push(p);
            continue;
        }
        let nested = p.join("SKILL.md");
        if nested.is_file() {
            out.push(nested);
        }
    }
    out.sort();
    out
}

/// 从一个 skill 文件文本解析出标题 + 描述。
fn parse_one(text: &str, path: &Path) -> Option<SkillSummary> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(s) = parse_front_matter_skill(text, path) {
        return Some(s);
    }

    let fallback_text = body_after_front_matter(text).unwrap_or(text);
    let mut lines = fallback_text.lines();
    let first = loop {
        let line = lines.next()?;
        let t = line.trim();
        if !t.is_empty() {
            break t;
        }
    };
    let title = first.trim_start_matches('#').trim().to_string();
    if title.is_empty() || title == "---" {
        return None;
    }

    let mut desc = String::new();
    let mut desc_started = false;
    for line in lines {
        let t = line.trim();
        // Markdown 标题后通常有一个空行：描述开始前跳过空行，开始后以空行结束首段。
        if t.is_empty() {
            if desc_started {
                break;
            }
            continue;
        }
        if t.starts_with("##") {
            break;
        }
        desc_started = true;
        if !desc.is_empty() {
            desc.push(' ');
        }
        desc.push_str(t);
    }

    Some(SkillSummary {
        title,
        description: desc,
        path: path.to_string_lossy().replace('\\', "/"),
    })
}

fn parse_front_matter_skill(text: &str, path: &Path) -> Option<SkillSummary> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut title = None;
    let mut desc = String::new();
    let mut in_desc = false;
    for line in lines {
        let t = line.trim_end();
        if t.trim() == "---" {
            break;
        }
        if let Some(v) = t.strip_prefix("name:") {
            title = Some(parse_inline_yaml_scalar(v));
            in_desc = false;
            continue;
        }
        if let Some(v) = t.strip_prefix("description:") {
            let v = v.trim();
            if !v.is_empty() && !is_yaml_block_scalar_header(v) {
                desc.push_str(&parse_inline_yaml_scalar(v));
            }
            in_desc = true;
            continue;
        }
        if in_desc {
            if line.starts_with(' ') || line.starts_with('\t') || line.trim().is_empty() {
                let part = line.trim();
                if !part.is_empty() {
                    if !desc.is_empty() {
                        desc.push(' ');
                    }
                    desc.push_str(part);
                }
            } else {
                in_desc = false;
            }
        }
    }

    let title = title?.trim().to_string();
    if title.is_empty() {
        return None;
    }
    Some(SkillSummary {
        title,
        description: desc.trim().to_string(),
        path: path.to_string_lossy().replace('\\', "/"),
    })
}

/// 解析 frontmatter 的简单行内标量。
///
/// RClaw 不需要完整 YAML AST，但 `name` / `description` 常用单双引号和行尾注释。双引号优先
/// 按 JSON 字符串解码常见转义，失败时保守地只去掉配对引号；YAML 单引号内的 `''` 表示一个
/// `'`。
fn parse_inline_yaml_scalar(value: &str) -> String {
    let value = strip_yaml_inline_comment(value.trim()).trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        return serde_json::from_str::<String>(value)
            .unwrap_or_else(|_| value[1..value.len() - 1].to_string());
    }
    if value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'') {
        return value[1..value.len() - 1].replace("''", "'");
    }
    value.to_string()
}

/// 移除简单 YAML 标量末尾的注释，同时保留引号内以及未被空白分隔的 `#`。
fn strip_yaml_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        match quote {
            Some(b'\'') => {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            Some(b'"') => {
                if bytes[index] == b'\\' {
                    index += 2;
                    continue;
                }
                if bytes[index] == b'"' {
                    quote = None;
                }
            }
            Some(_) => unreachable!(),
            None => match bytes[index] {
                b'\'' | b'"' => quote = Some(bytes[index]),
                b'#' if index == 0 || bytes[index - 1].is_ascii_whitespace() => {
                    return value[..index].trim_end();
                }
                _ => {}
            },
        }
        index += 1;
    }
    value.trim_end()
}

/// 判断 `description:` 后是否为 YAML 块标量头。
///
/// 支持 `>` / `|`、chomping 修饰符（`>-`、`|+`）、缩进指示符（如 `>2-`），以及其后的
/// YAML 注释。这里只识别头部，不区分 folded/literal，因为摘要最终统一折叠成单行。
fn is_yaml_block_scalar_header(value: &str) -> bool {
    let mut parts = value.splitn(2, char::is_whitespace);
    let indicator = parts.next().unwrap_or("");
    let trailing = parts.next().unwrap_or("").trim_start();
    let mut chars = indicator.chars();
    let Some(style) = chars.next() else {
        return false;
    };
    matches!(style, '>' | '|')
        && chars.all(|ch| matches!(ch, '+' | '-') || ch.is_ascii_digit())
        && (trailing.is_empty() || trailing.starts_with('#'))
}

fn body_after_front_matter(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut offset = 0usize;
    for line in text.lines() {
        let line_len = line.len();
        let eol_len = text
            .get(offset + line_len..)
            .map(|tail| {
                if tail.starts_with("\r\n") {
                    2
                } else if tail.starts_with('\n') {
                    1
                } else {
                    0
                }
            })
            .unwrap_or(0);
        let next_offset = offset + line_len + eol_len;
        if offset > 0 && line.trim() == "---" {
            return text.get(next_offset..);
        }
        offset = next_offset;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{init_layout, Layout};

    #[test]
    fn index_description_is_truncated() {
        let long_desc = "x".repeat(200);
        let items = vec![SkillSummary {
            title: "demo".into(),
            description: long_desc.clone(),
            path: "skills/demo/SKILL.md".into(),
        }];
        let text = summaries_to_text(&items);
        assert!(!text.contains(&long_desc), "长描述不得整段进索引");
        assert!(text.contains('…'), "截断应有省略号: {text}");
        assert!(text.len() < long_desc.len());
    }

    #[test]
    fn top_level_skill_dir_name_matches_only_dir_skills() {
        assert_eq!(
            top_level_skill_dir_name("skills/glmv-caption/SKILL.md"),
            Some("glmv-caption".to_string())
        );
        assert_eq!(
            top_level_skill_dir_name("L:/x/agent/skills/glmv-caption/SKILL.md"),
            Some("glmv-caption".to_string())
        );
        assert_eq!(top_level_skill_dir_name("skills/foo.md"), None);
        assert_eq!(top_level_skill_dir_name("skills/pkg/skills/inner.md"), None);
        // 包内目录型技能：按包名归因（外层 top_level_skill_name），不能按内层名反查，
        // 否则会与同名顶层技能的样本串数据。
        assert_eq!(
            top_level_skill_dir_name("skills/pkg/skills/inner/SKILL.md"),
            None
        );
    }

    #[test]
    fn reliability_tag_requires_min_samples() {
        let items = vec![SkillSummary {
            title: "demo".into(),
            description: "desc".into(),
            path: "skills/demo/SKILL.md".into(),
        }];
        // 样本不足（2 < 3）：不标注。
        let mut stats = SkillStatSamples::new();
        stats.insert("demo".to_string(), (1, 2));
        let text = summaries_to_text_with_stats(&items, &stats);
        assert!(!text.contains("近2次"), "小样本不得标注: {text}");
        // 样本充足：标注成功/失败计数。
        stats.insert("demo".to_string(), (7, 9));
        let text = summaries_to_text_with_stats(&items, &stats);
        assert!(text.contains("近9次：7成2败"), "{text}");
        // 无样本技能不受影响。
        let items2 = vec![SkillSummary {
            title: "other".into(),
            description: "desc".into(),
            path: "skills/other/SKILL.md".into(),
        }];
        let text = summaries_to_text_with_stats(&items2, &stats);
        assert!(text.contains("(read with: read"), "{text}");
        assert!(!text.contains("近"), "{text}");
    }

    #[test]
    fn package_skills_dir_is_scanned() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        init_layout(&layout).unwrap();
        let package_skills = layout.skills_dir.join("ai-berkshire").join("skills");
        std::fs::create_dir_all(&package_skills).unwrap();
        std::fs::write(
            package_skills.join("investment-research.md"),
            "# 投资研究\n\n四大师综合分析框架。\n",
        )
        .unwrap();

        let summaries = build_summaries(&layout);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "投资研究");
        assert_eq!(summaries[0].description, "四大师综合分析框架。");
        assert!(summaries[0]
            .path
            .ends_with("agent/skills/ai-berkshire/skills/investment-research.md"));
    }

    #[test]
    fn stray_file_in_skills_dir_is_skipped() {
        // 复现：skill_manage 会在 skills 目录写入普通文件 .evolution.log，
        // 加载器不应把它当成技能包目录去 read_dir（否则 "Not a directory" 告警）。
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        std::fs::create_dir_all(&skills).unwrap();
        std::fs::write(
            skills.join(".evolution.log"),
            "[2026-07-12] created skill x\n",
        )
        .unwrap();
        // 同时放一个正常包，确保它仍被扫描。
        let pkg = skills.join("ai-berkshire").join("skills");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("investment-research.md"), "# 投资研究\n\n框架。\n").unwrap();

        let summaries = build_summaries_in(&skills);
        assert_eq!(summaries.len(), 1, "stray file should be skipped");
        assert_eq!(summaries[0].title, "投资研究");
    }

    #[test]
    fn skill_front_matter_name_and_folded_description_are_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        let skill_dir = skills.join("glm5v-multimodal");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: glm5v-multimodal\ndescription: >\n  Analyze images, videos, and documents.\n  Use the configured GLM script.\nmetadata:\n  ignored: true\n---\n\n# Body title\n\nBody text.\n",
        )
        .unwrap();

        let summaries = build_summaries_in(&skills);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "glm5v-multimodal");
        assert_eq!(
            summaries[0].description,
            "Analyze images, videos, and documents. Use the configured GLM script."
        );
        assert!(summaries[0]
            .path
            .ends_with("skills/glm5v-multimodal/SKILL.md"));
    }

    #[test]
    fn skill_front_matter_indented_plain_description_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        let skill_dir = skills.join("glmv-caption");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: glmv-caption\ndescription:\n  Generate captions for images and videos.\n  Ask first when an attachment has no explicit goal.\n---\n\n# Caption\n",
        )
        .unwrap();

        let summaries = build_summaries_in(&skills);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].title, "glmv-caption");
        assert_eq!(
            summaries[0].description,
            "Generate captions for images and videos. Ask first when an attachment has no explicit goal."
        );
    }

    #[test]
    fn skill_front_matter_literal_description_is_parsed_as_summary_text() {
        let summary = parse_one(
            "---\nname: glm-master-skill\ndescription: |\n  Route GLM capabilities.\n  Read the selected skill before acting.\n---\n\n# Master\n",
            Path::new("skills/glm-master-skill/SKILL.md"),
        )
        .unwrap();

        assert_eq!(summary.title, "glm-master-skill");
        assert_eq!(
            summary.description,
            "Route GLM capabilities. Read the selected skill before acting."
        );
    }

    #[test]
    fn skill_front_matter_single_quoted_scalars_are_unquoted() {
        let summary = parse_one(
            "---\nname: 'quoted-skill'\ndescription: 'It''s useful.'\n---\n",
            Path::new("skills/quoted-skill/SKILL.md"),
        )
        .unwrap();

        assert_eq!(summary.title, "quoted-skill");
        assert_eq!(summary.description, "It's useful.");
    }

    #[test]
    fn skill_front_matter_chomped_block_header_is_not_in_description() {
        let summary = parse_one(
            "---\nname: block-skill\ndescription: >- # folded summary\n  First line.\n  Second line.\n---\n",
            Path::new("skills/block-skill/SKILL.md"),
        )
        .unwrap();

        assert_eq!(summary.title, "block-skill");
        assert_eq!(summary.description, "First line. Second line.");
    }

    #[test]
    fn skill_front_matter_inline_comments_respect_scalar_boundaries() {
        let summary = parse_one(
            "---\nname: plain-skill # internal note\ndescription: 'Keeps # inside.' # external note\n---\n",
            Path::new("skills/plain-skill/SKILL.md"),
        )
        .unwrap();

        assert_eq!(summary.title, "plain-skill");
        assert_eq!(summary.description, "Keeps # inside.");

        let summary = parse_one(
            "---\nname: anchor-skill\ndescription: https://example.com/#section\n---\n",
            Path::new("skills/anchor-skill/SKILL.md"),
        )
        .unwrap();
        assert_eq!(summary.description, "https://example.com/#section");
    }
}
