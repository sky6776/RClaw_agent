//! rclaw.toml 的唯一写入方（C6，ADR-0002）。
//!
//! 用户配置含大量手写注释，serde 整体重写会全部抹掉；本模块用 `toml_edit` 做
//! 格式/注释保真的文档手术，暴露类型化意图方法，替代 slash.rs 里的手写行编辑器。
//!
//! 写入协议：读入 → `DocumentMut` 变更 → 临时文件 + 原子 rename（`atomic_write`）。
//! 跨命令的串行化由调用方（SlashRegistry 的 `config_mutation` 锁）负责——每次调用
//! 自身是原子的单次改写。

use std::path::{Path, PathBuf};

use crate::fs::atomic_write;

/// rclaw.toml 文件模块：路径唯一来源 + 注释保真的意图方法。
pub struct RclawToml {
    path: PathBuf,
}

impl RclawToml {
    /// 以文件路径构造（路径唯一来源：与 `Config::load` 读取的相同）。
    pub fn new(path: impl AsRef<Path>) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
        }
    }

    /// 定位 `name == provider_name` 的 `[[llm.providers]]` 段，改写其
    /// `model` / `base_url` / `context_length` 三项；段缺其中任意项时补上。
    /// 其余内容（含注释与键序）保真。
    pub fn set_provider_fields(
        &self,
        provider_name: &str,
        model: &str,
        base_url: &str,
        context_length: usize,
    ) -> Result<(), String> {
        let context_length = i64::try_from(context_length)
            .map_err(|_| "context_length 超出 TOML 整数范围".to_string())?;
        let mut doc = self.load()?;
        let providers = doc
            .get_mut("llm")
            .and_then(|llm| llm.as_table_mut())
            .and_then(|llm| llm.get_mut("providers"))
            .and_then(|providers| providers.as_array_of_tables_mut())
            .ok_or_else(|| "rclaw.toml 中未找到 [[llm.providers]] 段".to_string())?;
        let table = providers
            .iter_mut()
            .find(|table| {
                table
                    .get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name == provider_name)
                    .unwrap_or(false)
            })
            .ok_or_else(|| format!("rclaw.toml 中未找到 provider「{provider_name}」"))?;
        set_table_item_preserving_decor(table, "model", toml_edit::value(model));
        set_table_item_preserving_decor(table, "base_url", toml_edit::value(base_url));
        set_table_item_preserving_decor(table, "context_length", toml_edit::value(context_length));
        self.save(&doc)
    }

    /// 把 `[web_search].order` 改写为字符串数组（段缺该行时补上）。
    pub fn set_search_order(&self, order: &[&str]) -> Result<(), String> {
        let mut doc = self.load()?;
        let table = doc
            .get_mut("web_search")
            .and_then(|web_search| web_search.as_table_mut())
            .ok_or_else(|| "rclaw.toml 中未找到 [web_search] 段".to_string())?;
        let values = order
            .iter()
            .map(|value| toml_edit::Value::from(*value))
            .collect::<toml_edit::Array>();
        set_table_item_preserving_decor(table, "order", toml_edit::value(values));
        self.save(&doc)
    }

    /// 手动启禁某个搜索 provider：只改 `[web_search.<id>].enabled`，其余调优参数保真。
    pub fn set_search_provider_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let mut doc = self.load()?;
        let table = doc
            .get_mut("web_search")
            .and_then(|web_search| web_search.as_table_mut())
            .and_then(|web_search| web_search.get_mut(id))
            .and_then(|table| table.as_table_mut())
            .ok_or_else(|| format!("rclaw.toml 中未找到 [web_search.{id}] 段"))?;
        set_table_item_preserving_decor(table, "enabled", toml_edit::value(enabled));
        self.save(&doc)
    }

    /// 改写 `[web_search].fallback`（段缺该行时补上）。
    pub fn set_search_fallback(&self, enabled: bool) -> Result<(), String> {
        let mut doc = self.load()?;
        let table = doc
            .get_mut("web_search")
            .and_then(|web_search| web_search.as_table_mut())
            .ok_or_else(|| "rclaw.toml 中未找到 [web_search] 段".to_string())?;
        set_table_item_preserving_decor(table, "fallback", toml_edit::value(enabled));
        self.save(&doc)
    }

    /// 改写 `[web_browser].engine`（段缺失时创建——该段整体可选，用户没写过
    /// 也能直接用 /webbrowser 设首选引擎；已存在但非表值时报错）。
    pub fn set_web_browser_engine(&self, engine: &str) -> Result<(), String> {
        let mut doc = self.load()?;
        {
            let web_browser = doc
                .entry("web_browser")
                .or_insert(toml_edit::Item::Table(explicit_table()));
            let table = web_browser
                .as_table_mut()
                .ok_or_else(|| "rclaw.toml 中 [web_browser] 已是非表值，无法写入".to_string())?;
            set_table_item_preserving_decor(table, "engine", toml_edit::value(engine));
        }
        self.save(&doc)
    }

    /// 面板保存的类型化赋值动作（见 [`Self::apply_edits_preview`]）。
    pub fn apply_edits_preview(&self, edits: &[(String, EditValue)]) -> Result<String, String> {
        let mut doc = self.load()?;
        for (path, value) in edits {
            apply_dotted(&mut doc, path, value)?;
        }
        Ok(doc.to_string())
    }

    /// 把改写后的文本落盘（原子写 + CRLF 风格保持）；与 apply_edits_preview
    /// 搭配使用，中间可插入调用方的校验。
    pub fn write_text(&self, text: &str) -> Result<(), String> {
        let mut out = text.to_string();
        if self.source_was_crlf() {
            out = out.replace("\r\n", "\n").replace('\n', "\r\n");
        }
        atomic_write(&self.path, out.as_bytes())
            .map_err(|error| format!("写入 rclaw.toml 失败（{}）: {error}", self.path.display()))
    }

    fn load(&self) -> Result<toml_edit::DocumentMut, String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| format!("读取 rclaw.toml 失败（{}）: {error}", self.path.display()))?;
        text.parse::<toml_edit::DocumentMut>()
            .map_err(|error| format!("rclaw.toml 解析失败: {error}"))
    }

    fn save(&self, doc: &toml_edit::DocumentMut) -> Result<(), String> {
        let mut out = doc.to_string();
        // 原文件为 CRLF 时，toml_edit 新增键的行尾默认是 LF，会造成混合换行；
        // 统一还原为纯 CRLF（拆分前手写编辑器承诺逐字节保留换行风格）。
        if self.source_was_crlf() {
            out = out.replace("\r\n", "\n").replace('\n', "\r\n");
        }
        atomic_write(&self.path, out.as_bytes())
            .map_err(|error| format!("写入 rclaw.toml 失败（{}）: {error}", self.path.display()))
    }

    /// 原文件是否使用 CRLF 行尾。
    fn source_was_crlf(&self) -> bool {
        std::fs::read_to_string(&self.path)
            .map(|text| text.contains("\r\n"))
            .unwrap_or(false)
    }
}

/// 面板保存的类型化配置值（点路径赋值的右值）。
#[derive(Debug, Clone, PartialEq)]
pub enum EditValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    StrArray(Vec<String>),
}

/// 应用一条点路径赋值（如 `web_search.tavily.timeout_secs`、`llm.providers.zhipu.model`）。
/// 路径末段是键名；下钻中遇到数组表时，下一段按条目的 `name` 字段匹配；
/// 缺失的中间表会创建（显式表头，注释与既有键序保真）。
fn apply_dotted(
    doc: &mut toml_edit::DocumentMut,
    path: &str,
    value: &EditValue,
) -> Result<(), String> {
    let mut segments: Vec<&str> = path.split('.').collect();
    let Some(key) = segments.pop() else {
        return Err(format!("配置路径为空: {path}"));
    };
    if segments.is_empty() || key.is_empty() {
        return Err(format!("配置路径至少要包含段名与键名: {path}"));
    }
    let table = descend_table(doc.as_table_mut(), &segments)?;
    set_table_item_preserving_decor(table, key, edit_value_to_item(value));
    Ok(())
}

/// 沿段名下行到目标表；数组表段（`[[llm.providers]]`）按 `name=` 匹配条目。
/// 对 `item` 做 match 分臂消费 + 尾递归（NLL 借用规则不允许跨分支持有衍生可变借用）。
fn descend_table<'a>(
    table: &'a mut toml_edit::Table,
    segments: &[&str],
) -> Result<&'a mut toml_edit::Table, String> {
    let Some(seg) = segments.first().copied() else {
        return Ok(table);
    };
    let rest = &segments[1..];
    let item = table
        .entry(seg)
        .or_insert_with(|| toml_edit::Item::Table(explicit_table()));
    if item.is_none() {
        *item = toml_edit::Item::Table(explicit_table());
    }
    match item {
        toml_edit::Item::ArrayOfTables(arr) => {
            let name = rest
                .first()
                .copied()
                .ok_or_else(|| format!("路径在数组段 [{seg}] 后缺少按 name 匹配的条目名"))?;
            let entry = arr
                .iter_mut()
                .find(|t| t.get("name").and_then(|n| n.as_str()) == Some(name))
                .ok_or_else(|| format!("[{seg}] 数组中未找到 name=\"{name}\" 的条目"))?;
            descend_table(entry, &rest[1..])
        }
        other => match other.as_table_mut() {
            Some(sub) => descend_table(sub, rest),
            None => Err(format!("段 [{seg}] 已是非表值，无法继续下钻")),
        },
    }
}

fn explicit_table() -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    t.set_implicit(false);
    t
}

fn edit_value_to_item(value: &EditValue) -> toml_edit::Item {
    match value {
        EditValue::Bool(b) => toml_edit::value(*b),
        EditValue::Int(i) => toml_edit::value(*i),
        EditValue::Float(f) => toml_edit::value(*f),
        EditValue::Str(s) => toml_edit::value(s.as_str()),
        EditValue::StrArray(items) => toml_edit::value(
            items
                .iter()
                .map(|s| toml_edit::Value::from(s.as_str()))
                .collect::<toml_edit::Array>(),
        ),
    }
}

/// 覆盖已有值时同时保留 table key 的原始格式，以及 value 上承载的空白/行内注释。
/// `Table::insert` 会对已存在的 key 调用 `Key::fmt()`，从而清掉 key decor；这里直接
/// 替换现有 Item，只在新增键时才走 insert。
fn set_table_item_preserving_decor(
    table: &mut toml_edit::Table,
    key: &str,
    mut item: toml_edit::Item,
) {
    if let Some(existing) = table.get_mut(key) {
        let decor = existing.as_value().map(|value| value.decor().clone());
        if let (Some(decor), Some(new)) = (decor, item.as_value_mut()) {
            *new.decor_mut() = decor;
        }
        *existing = item;
    } else {
        table.insert(key, item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(dir: &Path, text: &str) -> RclawToml {
        let path = dir.join("rclaw.toml");
        std::fs::write(&path, text).unwrap();
        RclawToml::new(path)
    }

    #[test]
    fn set_provider_fields_updates_only_target_provider_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "# 顶部注释\n[[llm.providers]]\nname = \"deepseek\"\nmodel = \"old\"\nbase_url = \"https://old\"\ncontext_length = 1000\n\n[[llm.providers]]\nname = \"zhipu\"\nmodel = \"glm-5.3\"\n",
        );
        toml.set_provider_fields("deepseek", "new-model", "https://new", 2000)
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 顶部注释"), "注释必须保真");
        assert!(text.contains("model = \"new-model\""));
        assert!(text.contains("base_url = \"https://new\""));
        assert!(text.contains("context_length = 2000"));
        assert!(
            text.contains("name = \"zhipu\""),
            "未命中 provider 不得被改"
        );
        assert!(text.contains("model = \"glm-5.3\""));
        assert!(!text.contains("\"old\""), "旧值必须被替换");
    }

    #[test]
    fn updates_keep_inline_comments_and_key_spacing() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[web_search]\norder    = [\"zhipu\"] # preferred order\nfallback = false # fallback policy\n\n[web_search.tavily]\nenabled = false # manual gate\n",
        );
        toml.set_search_order(&["tavily"]).unwrap();
        toml.set_search_fallback(true).unwrap();
        toml.set_search_provider_enabled("tavily", true).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("order    = [\"tavily\"] # preferred order"));
        assert!(text.contains("fallback = true # fallback policy"));
        assert!(text.contains("enabled = true # manual gate"));
    }

    #[test]
    fn set_provider_fields_inserts_missing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[[llm.providers]]\nname = \"deepseek\"\nmodel = \"old\"\n",
        );
        toml.set_provider_fields("deepseek", "m", "https://u", 128000)
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("base_url = \"https://u\""));
        assert!(text.contains("context_length = 128000"));
    }

    #[test]
    fn set_provider_fields_missing_provider_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[[llm.providers]]\nname = \"deepseek\"\nmodel = \"old\"\n",
        );
        assert!(toml.set_provider_fields("zhipu", "m", "u", 0).is_err());
    }

    #[test]
    fn set_search_order_replaces_and_inserts() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), "[web_search]\nenabled = true\n");
        toml.set_search_order(&["tavily", "zhipu"]).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("order = [\"tavily\", \"zhipu\"]"), "{text}");
        // 已有 order 行：再写覆盖不重复。
        toml.set_search_order(&["zhipu"]).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(text.matches("order =").count(), 1);
        assert!(text.contains("order = [\"zhipu\"]"));
    }

    #[test]
    fn set_search_provider_enabled_and_fallback_keep_neighbors() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "# 段注释\n[web_search]\nfallback = false\n\n[web_search.tavily]\napi_key = \"k\"\nenabled = false\nmax_results = 5\n",
        );
        toml.set_search_provider_enabled("tavily", true).unwrap();
        toml.set_search_fallback(true).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 段注释"), "注释必须保真");
        assert!(text.contains("api_key = \"k\""), "同段其它键必须保真");
        assert!(text.contains("max_results = 5"));
        assert!(text.contains("enabled = true"));
        assert!(text.contains("fallback = true"));
    }

    #[test]
    fn missing_section_is_error() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), "[llm]\nprovider = \"x\"\n");
        assert!(toml.set_search_order(&["zhipu"]).is_err());
        assert!(toml.set_search_fallback(true).is_err());
    }

    /// [web_browser] 段整体可选：缺失时直接创建写入；已有段更新引擎且邻居保真。
    #[test]
    fn set_web_browser_engine_creates_missing_section_and_updates_existing() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), "# 顶部\n[napcat]\nenabled = false\n");
        toml.set_web_browser_engine("google").unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 顶部"), "注释必须保真");
        assert!(text.contains("[web_browser]"), "{text}");
        assert!(text.contains("engine = \"google\""), "{text}");
        assert!(text.contains("enabled = false"), "邻居段必须保真");

        // 已有段：覆盖引擎，段内其它键保留。
        let dir2 = tempfile::tempdir().unwrap();
        let toml2 = write_config(
            dir2.path(),
            "[web_browser]\nengine = \"bing\" # 注释\n",
        );
        toml2.set_web_browser_engine("baidu").unwrap();
        let text2 = std::fs::read_to_string(dir2.path().join("rclaw.toml")).unwrap();
        assert!(text2.contains("engine = \"baidu\" # 注释"), "{text2}");
        assert_eq!(text2.matches("engine =").count(), 1);

        // 非表值：报错不写入。
        let dir3 = tempfile::tempdir().unwrap();
        let toml3 = write_config(dir3.path(), "web_browser = \"oops\"\n");
        assert!(toml3.set_web_browser_engine("bing").is_err());
    }

    /// CRLF 文件改写后必须保持纯 CRLF（拆分前的手写编辑器明确承诺逐字节保留换行风格）。
    #[test]
    fn crlf_files_stay_pure_crlf_after_edit() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[[llm.providers]]\r\nname = \"zhipu\"\r\nmodel = \"glm-5.3\"\r\n",
        );
        toml.set_provider_fields("zhipu", "m", "https://u", 1)
            .unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(
            !text.contains('\n') || text.matches("\r\n").count() == text.matches('\n').count(),
            "改写后不得出现混合换行（LF 与 CRLF 并存）"
        );
        assert!(text.contains("base_url = \"https://u\""));
    }

    /// 面板点路径赋值：已有键覆盖保注释、缺失键补建、数组表按 name 寻址。
    #[test]
    fn apply_edits_preview_covers_nested_keys_and_preserves_comments() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "# 顶部注释\n[agent]\nbase_dir = \"agent\" # 数据根\n\n[llm]\nprovider = \"zhipu\"\n\n[[llm.providers]]\nname = \"zhipu\"\nmodel = \"glm-5.3\"\n\n[[llm.providers]]\nname = \"deepseek\"\nmodel = \"deepseek-v4-pro\"\n",
        );
        let out = toml
            .apply_edits_preview(&[
                (
                    "agent.auto_memory".into(),
                    EditValue::Bool(true), // 文件中没有的默认键 → 新增
                ),
                (
                    "llm.providers.zhipu.model".into(),
                    EditValue::Str("glm-5.3-flash".into()), // 只改 zhipu 条目
                ),
                ("agent.base_dir".into(), EditValue::Str("agent2".into())),
            ])
            .unwrap();
        assert!(out.contains("# 顶部注释"), "注释必须保真");
        assert!(out.contains("base_dir = \"agent2\""));
        assert!(out.contains("auto_memory = true"));
        assert!(out.contains("model = \"glm-5.3-flash\""));
        assert!(
            out.contains("model = \"deepseek-v4-pro\""),
            "未命中条目不得被改"
        );
        // 校验通过后落盘。
        toml.write_text(&out).unwrap();
        let saved = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(saved.contains("auto_memory = true"));
    }

    #[test]
    fn apply_edits_preview_creates_missing_section_and_rejects_bad_paths() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[agent]\nbase_dir = \"agent\"\n\n[[llm.providers]]\nname = \"zhipu\"\nmodel = \"m\"\n",
        );
        // 缺失的 [log] 段自动创建（显式表头）。
        let out = toml
            .apply_edits_preview(&[("log.console".into(), EditValue::Bool(true))])
            .unwrap();
        assert!(out.contains("[log]"));
        assert!(out.contains("console = true"));
        // 数组条目按 name 匹配：不存在的 name 是错误。
        let err = toml.apply_edits_preview(&[(
            "llm.providers.nope.model".into(),
            EditValue::Str("m".into()),
        )]);
        assert!(err.is_err());
        // 裸键名（无段）是错误。
        assert!(toml
            .apply_edits_preview(&[("base_dir".into(), EditValue::Str("x".into()))])
            .is_err());
    }

    #[test]
    fn apply_edits_preview_handles_arrays_and_inline_comments() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[web_search]\norder = [\"zhipu\"] # preferred order\n\n[web_search.tavily]\nenabled = false # manual gate\napi_keys = [\"a\"]\n",
        );
        let out = toml
            .apply_edits_preview(&[
                (
                    "web_search.order".into(),
                    EditValue::StrArray(vec!["tavily".into(), "zhipu".into()]),
                ),
                ("web_search.tavily.enabled".into(), EditValue::Bool(true)),
                (
                    "web_search.tavily.api_keys".into(),
                    EditValue::StrArray(vec!["k1".into(), "k2".into()]),
                ),
            ])
            .unwrap();
        assert!(out.contains("order = [\"tavily\", \"zhipu\"] # preferred order"));
        assert!(out.contains("enabled = true # manual gate"));
        assert!(out.contains("api_keys = [\"k1\", \"k2\"]"));
    }
}
