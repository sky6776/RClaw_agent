//! 配置面板文档模型：把仓库根的 `rclaw.toml.example` 解析为「段 → 键 →
//! 文档注释 / 默认值 / 值类型」的结构，作为桌面「配置」栏目的唯一文档来源。
//!
//! 解析按行进行（toml_edit 拿不到被注释掉的键与注释的归属关系）：
//! - 键上方连续的纯注释行 + 行内尾注 = 该键的文档；
//! - 紧跟在键行之后（中间无空行）的注释行也归入该键的文档；
//! - `# key = value` 形式的注释键按「默认值项」收录（示例未显式配置、用默认）；
//! - `# [[section]]` 形式开启「注释示例段」（如 DeepSeek provider、MCP server
//!   示例）：嵌套段渲染为只读文档；无点号的顶层注释段（如 `# [mcp]`）仍解析
//!   出可编辑的默认键。
//!
//! 保存寻址与 [`rclaw_store::RclawToml::apply_edits_preview`] 同一语义：点分
//! 下钻，数组表段（`[[llm.providers]]`）按条目 `name=` 匹配。

use serde::Serialize;

/// 仓库根示例文件（面板文档的唯一来源；与发行包一起编译进二进制）。
pub const EXAMPLE: &str = include_str!("../../../rclaw.toml.example");

#[derive(Debug, Clone, Serialize)]
pub struct ConfigDocSection {
    /// 段头原文（`[agent]` / `[[llm.providers]]`）。
    pub header: String,
    /// 点分路径（`agent` / `llm.providers.zhipu`，数组段以 name= 回填）。
    pub path: String,
    /// 展示名（`agent` / `llm.providers · zhipu`）。
    pub title: String,
    /// 段级文档注释行。
    pub doc: Vec<String>,
    /// 注释示例段（DeepSeek / MCP server 示例等）：只读文档展示，无键。
    pub example_only: bool,
    pub items: Vec<ConfigDocItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigDocItem {
    pub key: String,
    /// 保存寻址路径（`agent.max_concurrent_turns`）。
    pub path: String,
    /// 该键的完整文档（功能 / 生效方式 / 取值含义）。
    pub doc: Vec<String>,
    /// 示例默认值的 TOML 原文（`6`、`true`、`"zhipu"`、`["a", "b"]`）。
    pub default_raw: String,
    /// bool / int / float / str / str_array。
    pub kind: String,
    /// 示例中该键被注释（未显式配置，生效的是默认值）。
    pub commented: bool,
    /// 密钥类输入（UI 打码展示）。
    pub secret: bool,
    /// 枚举型键的合法取值（非空时 UI 渲染为下拉框；空 = 自由输入）。见 [`options_of`]。
    pub options: Vec<String>,
    /// 用户 rclaw.toml 中的当前值（None = 未配置，用默认）。
    pub current: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigDoc {
    /// 文件顶部（任何段之前）的整体说明注释。
    pub preamble: Vec<String>,
    pub sections: Vec<ConfigDocSection>,
}

/// 解析示例文本为文档模型。
pub fn parse_example(text: &str) -> ConfigDoc {
    let mut sections: Vec<ConfigDocSection> = Vec::new();
    let mut preamble: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    // 键行之后、空行之前的注释行归入上一个键。
    let mut trailing_allowed = false;
    // 注释示例段状态：(段, 是否解析键)。
    let mut commented_section: Option<(ConfigDocSection, bool)> = None;

    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim().is_empty() {
            trailing_allowed = false;
            continue;
        }
        let stripped = line.trim_start();
        if let Some(comment) = stripped.strip_prefix('#') {
            let body = comment.trim_start();
            if is_commented_header(body) {
                // 新注释段头先收束上一个注释段（[mcp] 顶层注释段后跟
                // `# [mcp.servers.*]` 示例段，各自成段而不是把后者的键
                // 并进前者）。`# [mcp]`（无点号）→ 可编辑默认键；
                // 嵌套注释段 → 只读文档。
                if let Some((prev, prev_parse_keys)) = commented_section.take() {
                    if prev_parse_keys || !prev.doc.is_empty() || !prev.items.is_empty() {
                        sections.push(prev);
                    }
                }
                let path = header_path(body);
                let parse_keys = !path.contains('.');
                let mut section = new_section(body, &path, &pending);
                section.example_only = !parse_keys;
                commented_section = Some((section, parse_keys));
                pending.clear();
                trailing_allowed = false;
                continue;
            }
            // 注释键 `# key = value`（默认值项）。
            if let Some((key, value, inline)) = parse_keyed(body) {
                match &mut commented_section {
                    Some((section, true)) => {
                        push_item(section, &key, &value, inline, true, &mut pending)
                    }
                    // doc-blob 注释段（DeepSeek / MCP server 示例）：键行原样进文档。
                    Some((section, false)) => {
                        let line_doc = match inline {
                            Some(i) => format!("{key} = {value}  # {i}"),
                            None => format!("{key} = {value}"),
                        };
                        section.doc.push(line_doc);
                    }
                    None => {
                        if let Some(section) = sections.last_mut() {
                            push_item(section, &key, &value, inline, true, &mut pending);
                        }
                    }
                }
                trailing_allowed = true;
                continue;
            }
            // 纯注释：紧随键行（无空行）→ 归上一个键；否则累积给下一个目标。
            let doc_line = body.to_string();
            let mut appended = false;
            if trailing_allowed {
                if let Some((section, true)) = commented_section.as_mut() {
                    if let Some(item) = section.items.last_mut() {
                        item.doc.push(doc_line.clone());
                        appended = true;
                    }
                } else if let Some(section) = sections.last_mut() {
                    if let Some(item) = section.items.last_mut() {
                        item.doc.push(doc_line.clone());
                        appended = true;
                    }
                }
            }
            if !appended {
                match &mut commented_section {
                    Some((section, _)) => section.doc.push(doc_line),
                    None => {
                        // 首个段之前的注释是文件整体说明（preamble）。
                        if sections.is_empty() {
                            preamble.push(doc_line);
                        } else {
                            pending.push(doc_line);
                        }
                    }
                }
            }
            continue;
        }
        // 非注释行：先结束注释示例段。
        if let Some((section, parse_keys)) = commented_section.take() {
            if parse_keys || !section.doc.is_empty() || !section.items.is_empty() {
                sections.push(section);
            }
        }
        trailing_allowed = false;
        if let Some(path) = section_header(stripped) {
            sections.push(new_section(stripped, &path, &pending));
            pending.clear();
            continue;
        }
        if let Some((key, value, inline)) = parse_keyed(stripped) {
            if let Some(section) = sections.last_mut() {
                // 数组段首键 name= 回填段 path/title。
                if section.header.starts_with("[[") && key == "name" {
                    let name = value.trim_matches('"').to_string();
                    section.path = format!("{}.{}", section.path, name);
                    section.title = format!("{} · {}", section.title, name);
                }
                push_item(section, &key, &value, inline, false, &mut pending);
                trailing_allowed = true;
            }
            continue;
        }
    }
    if let Some((section, _)) = commented_section.take() {
        if !section.doc.is_empty() || !section.items.is_empty() {
            sections.push(section);
        }
    }
    ConfigDoc { preamble, sections }
}

fn new_section(header: &str, path: &str, pending: &[String]) -> ConfigDocSection {
    let title = path.split('.').next_back().unwrap_or(path).to_string();
    ConfigDocSection {
        header: header.to_string(),
        path: path.to_string(),
        title,
        doc: pending.to_vec(),
        example_only: false,
        items: Vec::new(),
    }
}

fn push_item(
    section: &mut ConfigDocSection,
    key: &str,
    value: &str,
    inline: Option<String>,
    commented: bool,
    pending: &mut Vec<String>,
) {
    let mut doc = std::mem::take(pending);
    if let Some(inline) = inline {
        doc.push(inline);
    }
    let path = format!("{}.{}", section.path, key);
    section.items.push(ConfigDocItem {
        key: key.to_string(),
        doc,
        default_raw: value.trim().to_string(),
        kind: kind_of(value.trim()).to_string(),
        commented,
        secret: is_secret(key),
        options: options_of(&path),
        path,
        current: None,
    });
}

/// 枚举型键的合法取值表（按保存寻址路径）。
///
/// 与各键的 serde 枚举保持一致（`rclaw_cli::config::Config::parse_str` 在保存前仍会
/// 校验，这里只决定 UI 是否渲染下拉框以及选项顺序）。
fn options_of(path: &str) -> Vec<String> {
    let options: &[&str] = match path {
        // `rclaw_runtime::options::WorkflowActivation`（snake_case）。
        "agent.workflow_activation" => &["always", "command"],
        _ => &[],
    };
    options.iter().map(|s| s.to_string()).collect()
}

/// `key = value # inline` → (key, value, inline)。
fn parse_keyed(body: &str) -> Option<(String, String, Option<String>)> {
    let (key, rest) = body.split_once('=')?;
    let key = key.trim();
    if key.is_empty()
        || !key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return None;
    }
    let rest = rest.trim();
    if rest.is_empty() {
        return None;
    }
    // 行内注释：首个 ` # `（字符串值里极少含该形态）。
    let (value, inline) = match rest.find(" # ") {
        Some(pos) => (
            rest[..pos].trim().to_string(),
            Some(rest[pos + 3..].trim().to_string()),
        ),
        None => (rest.to_string(), None),
    };
    // 右侧必须形如 TOML 值（字符串/数组/内联表/数字/布尔），否则视为注释
    // 散文里的公式（如 `effective_window = provider.context_length - ...`），
    // 不收为键。
    if !looks_like_toml_value(&value) {
        return None;
    }
    Some((key.to_string(), value, inline))
}

/// 值形态粗判：引号 / 数组 / 内联表 / 数字 / 布尔。
fn looks_like_toml_value(value: &str) -> bool {
    let v = value.trim();
    if v == "true" || v == "false" {
        return true;
    }
    matches!(
        v.chars().next(),
        Some('"') | Some('\'') | Some('[') | Some('{') | Some('0'..='9') | Some('+') | Some('-')
    )
}

/// 段头 `[a.b]` / `[[a.b]]` 的点分路径。
fn section_header(line: &str) -> Option<String> {
    if !(line.starts_with('[')) || !line.ends_with(']') {
        return None;
    }
    let inner = line
        .trim_start_matches("[[")
        .trim_end_matches("]]")
        .trim_start_matches('[')
        .trim_end_matches(']');
    if inner.is_empty()
        || !inner
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        return None;
    }
    Some(inner.to_string())
}

/// 注释段头判定（`# [mcp]` / `# [[llm.providers]]` 的 body）。
fn is_commented_header(body: &str) -> bool {
    section_header(body).is_some()
}

fn header_path(body: &str) -> String {
    section_header(body).unwrap_or_default()
}

fn kind_of(raw: &str) -> &'static str {
    if raw == "true" || raw == "false" {
        return "bool";
    }
    if raw.starts_with('[') {
        return "str_array";
    }
    if raw.starts_with('"') {
        return "str";
    }
    if raw.parse::<i64>().is_ok() {
        return "int";
    }
    if raw.parse::<f64>().is_ok() {
        return "float";
    }
    "str"
}

fn is_secret(key: &str) -> bool {
    matches!(key, "api_key" | "api_keys" | "token" | "access_token")
        || key.ends_with("_key")
        || key.ends_with("_keys")
        || key.ends_with("_token")
        || key.ends_with("_secret")
}

/// 在用户 rclaw.toml（toml_edit 文档）里解析点分路径的当前值。
/// 与保存寻址同语义：数组表段按条目 `name=` 匹配。
pub fn resolve_current<'a>(
    doc: &'a toml_edit::DocumentMut,
    path: &str,
) -> Option<&'a toml_edit::Item> {
    let mut segments: Vec<&str> = path.split('.').collect();
    let key = segments.pop()?;
    let mut table: &toml_edit::Table = doc.as_table();
    let mut iter = segments.iter().copied();
    while let Some(seg) = iter.next() {
        let item = table.get(seg)?;
        if let Some(arr) = item.as_array_of_tables() {
            let name = iter.next()?;
            table = arr
                .iter()
                .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))?;
        } else {
            table = item.as_table()?;
        }
    }
    table.get(key).filter(|v| !v.is_none())
}

/// 标量值的干净 UI 文本。字符串走 `as_str()`（toml_edit 已剥离引号与 decor）；
/// 其余标量必须按类型取值——`Value::to_string()` 会带上 decor（前导空白与
/// 行内注释），导致 " true" 这样的值让布尔复选框判不中、数字输入框显示为空。
fn scalar_display(value: &toml_edit::Value) -> Option<String> {
    if let Some(s) = value.as_str() {
        return Some(s.to_string());
    }
    if let Some(b) = value.as_bool() {
        return Some(b.to_string());
    }
    if let Some(i) = value.as_integer() {
        return Some(i.to_string());
    }
    if let Some(f) = value.as_float() {
        return Some(f.to_string());
    }
    value.as_datetime().map(|dt| dt.to_string())
}

/// 当前值的 UI 显示形态：字符串去引号、数组转逗号分隔（元素按标量类型取值），
/// 其余标量按类型取干净文本（内联表兜底原样序列化）。
pub fn resolve_current_display(doc: &toml_edit::DocumentMut, path: &str) -> Option<String> {
    let item = resolve_current(doc, path)?;
    let value = item.as_value()?;
    if let Some(arr) = value.as_array() {
        return Some(
            arr.iter()
                .filter_map(scalar_display)
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    Some(scalar_display(value).unwrap_or_else(|| value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_example_into_sections_and_docs() {
        let doc = parse_example(EXAMPLE);
        assert!(
            doc.preamble.iter().any(|l| l.contains("RClaw 配置")),
            "顶部说明应进 preamble: {:?}",
            doc.preamble
        );
        // 顶层段齐备。
        for p in [
            "agent",
            "llm",
            "llm.providers.zhipu",
            "web_search",
            "web_search.tavily",
            "web_search.zhipu",
            "napcat",
            "weixin",
            "wxbot",
            "log",
            "mcp",
            "mcp.servers.gbrain",
        ] {
            assert!(
                doc.sections.iter().any(|s| s.path == p),
                "缺段 {p}: {:?}",
                doc.sections
                    .iter()
                    .map(|s| s.path.as_str())
                    .collect::<Vec<_>>()
            );
        }
        let agent = doc
            .sections
            .iter()
            .find(|s| s.path == "agent")
            .expect("[agent] 段");
        let base = agent.items.iter().find(|i| i.key == "base_dir").unwrap();
        assert!(!base.commented);
        assert_eq!(base.kind, "str");
        assert!(
            base.doc.iter().any(|l| l.contains("独立的记忆")),
            "键文档应搬过来: {:?}",
            base.doc
        );
        // auto_memory 是显式键（未注释）。
        let am = agent.items.iter().find(|i| i.key == "auto_memory").unwrap();
        assert!(!am.commented);
        // workflow_activation：枚举型 str 键，带下拉选项，文档（含 `"command" = …` 形态的
        // 说明行）归到本键而不是被误收为键。
        let activation = agent
            .items
            .iter()
            .find(|i| i.key == "workflow_activation")
            .expect("agent.workflow_activation 应被收录");
        assert!(!activation.commented);
        assert_eq!(activation.kind, "str");
        assert_eq!(activation.default_raw, "\"always\"");
        assert_eq!(activation.options, vec!["always", "command"]);
        assert!(
            activation.doc.iter().any(|l| l.contains("/workflow")),
            "启用方式说明应归到 workflow_activation: {:?}",
            activation.doc
        );
        assert!(
            !agent.items.iter().any(|i| i.key.starts_with('"')),
            "带引号的说明行不得被误判为键: {:?}",
            agent
                .items
                .iter()
                .map(|i| i.key.as_str())
                .collect::<Vec<_>>()
        );
        assert!(base.options.is_empty(), "非枚举键无下拉选项");
        // 散文里的键名/公式不得被误判为键（deny_unknown_fields 下它永远存不进去）。
        assert!(
            !agent.items.iter().any(|i| i.key == "compression_threshold_tokens"),
            "散文提及的 compression_threshold_tokens 是注释文字，不得成为独立键（该键未在示例中显式出现）: {:?}",
            agent.items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>()
        );
        // 注释键（默认值项）：napcat 的 allow_from（值形态合法的注释键收录为可编辑项）。
        let napcat = doc
            .sections
            .iter()
            .find(|s| s.path == "napcat")
            .expect("[napcat] 段");
        let allow = napcat
            .items
            .iter()
            .find(|i| i.key == "allow_from")
            .expect("allow_from 应作为注释默认键收录");
        assert!(allow.commented);
        assert_eq!(allow.kind, "str");
        // log 的注释键（dir / file_name）齐备。
        let log = doc.sections.iter().find(|s| s.path == "log").unwrap();
        for key in ["dir", "file_name"] {
            let item = log
                .items
                .iter()
                .find(|i| i.key == key)
                .unwrap_or_else(|| panic!("log.{key} 注释键应被收录"));
            assert!(item.commented, "log.{key} 应为注释键");
        }
        // provider 数组段按 name 回填路径，键可寻址且 api_key 打码。
        let zhipu = doc
            .sections
            .iter()
            .find(|s| s.path == "llm.providers.zhipu")
            .expect("zhipu provider 段");
        assert!(zhipu
            .items
            .iter()
            .any(|i| i.path == "llm.providers.zhipu.api_key" && i.secret));
        // gbrain env 的密钥键被收录（is_secret 只识别小写 _key 后缀，环境变量风格
        // 的大写 _KEY 不打码——既有面板行为，非本测试约定）。
        let gbrain_env = doc
            .sections
            .iter()
            .find(|s| s.path == "mcp.servers.gbrain.env")
            .expect("gbrain env 段");
        assert!(gbrain_env
            .items
            .iter()
            .any(|i| i.key == "GBRAIN_EMBEDDING_API_KEY"));
        // [mcp] 全局段只有 3 个默认键（server 子段各自成段，键不得并入）。
        let mcp = doc
            .sections
            .iter()
            .find(|s| s.path == "mcp")
            .expect("[mcp] 段");
        assert_eq!(
            mcp.items.len(),
            3,
            "[mcp] 只含全局默认键: {:?}",
            mcp.items.iter().map(|i| i.key.as_str()).collect::<Vec<_>>()
        );
        assert!(mcp.items.iter().any(|i| i.key == "startup_timeout_secs"));
        // web_search 段的 read_top 键可寻址、非注释、带行内注释（抽一个标量键验证文档化）。
        let web_search = doc
            .sections
            .iter()
            .find(|s| s.path == "web_search")
            .expect("[web_search] 段");
        let read_top = web_search
            .items
            .iter()
            .find(|i| i.key == "read_top")
            .expect("web_search.read_top 应被收录");
        assert!(!read_top.commented);
        assert_eq!(read_top.kind, "int");
        assert!(
            read_top.doc.iter().any(|l| l.contains("补读")),
            "read_top 行内注释应进文档: {:?}",
            read_top.doc
        );
    }

    #[test]
    fn wxbot_secrets_are_masked() {
        // [wxbot] 段的两个凭据（api_token / webhook_secret）必须被识别为密钥，
        // 配置面板才会用密码框展示（is_secret 的 _token/_secret 后缀识别）。
        assert!(is_secret("api_token"));
        assert!(is_secret("webhook_secret"));
        assert!(is_secret("access_token"));
        assert!(!is_secret("api_base"));
        assert!(!is_secret("webhook_listen"));
        assert!(!is_secret("allow_from"));

        let doc = parse_example(EXAMPLE);
        let wxbot = doc
            .sections
            .iter()
            .find(|s| s.path == "wxbot")
            .expect("[wxbot] 段");
        for key in ["api_token", "webhook_secret"] {
            let item = wxbot
                .items
                .iter()
                .find(|i| i.key == key)
                .unwrap_or_else(|| panic!("wxbot.{key} 应被收录"));
            assert!(item.secret, "wxbot.{key} 应打码（密码框展示）");
        }
    }

    #[test]
    fn resolve_current_walks_arrays_by_name() {
        let text = "[llm]\nprovider = \"zhipu\"\n\n[[llm.providers]]\nname = \"zhipu\"\nmodel = \"glm-5.3\"\n\n[[llm.providers]]\nname = \"ds\"\nmodel = \"x\"\n";
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(
            resolve_current_display(&doc, "llm.providers.zhipu.model"),
            Some("glm-5.3".to_string()),
            "字符串当前值应去引号"
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.provider"),
            Some("zhipu".to_string())
        );
        assert!(resolve_current(&doc, "llm.providers.nope.model").is_none());
        assert!(resolve_current_display(&doc, "agent.auto_memory").is_none());
    }

    #[test]
    fn keyed_parse_splits_inline_comment() {
        let (k, v, inline) =
            parse_keyed("max_tool_calls = 256          # 单轮允许的最大工具调用数；默认 256")
                .unwrap();
        assert_eq!(k, "max_tool_calls");
        assert_eq!(v, "256");
        assert!(inline.unwrap().contains("单轮允许"));
    }

    /// 回归：布尔 / 整数 / 浮点当前值必须取干净文本，不得带 toml_edit decor
    /// （前导空白、行内注释）——否则 UI 布尔复选框判不中、数字输入框显示为空。
    #[test]
    fn resolve_current_display_strips_decor_from_scalars() {
        let text = "[llm]\n\
                    provider = \"zhipu\"   # 行内注释\n\
                    stream = true\n\
                    max_tokens = 128000\n\
                    request_timeout_secs = 300      # 单次 LLM 请求总超时\n\
                    ratio = 0.5\n\
                    tags = [\"a\", \"b\"]\n\
                    nums = [1, 2]\n";
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        assert_eq!(
            resolve_current_display(&doc, "llm.provider"),
            Some("zhipu".to_string())
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.stream"),
            Some("true".to_string())
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.max_tokens"),
            Some("128000".to_string())
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.request_timeout_secs"),
            Some("300".to_string())
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.ratio"),
            Some("0.5".to_string())
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.tags"),
            Some("a, b".to_string())
        );
        assert_eq!(
            resolve_current_display(&doc, "llm.nums"),
            Some("1, 2".to_string())
        );
    }
}
