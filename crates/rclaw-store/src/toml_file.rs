//! rclaw.toml 的唯一写入方（C6，ADR-0002）。
//!
//! 用户配置含大量手写注释，serde 整体重写会全部抹掉；本模块用 `toml_edit` 做
//! 格式/注释保真的文档手术，暴露类型化意图方法，替代 slash.rs 里的手写行编辑器。
//!
//! 写入协议：读入 → `DocumentMut` 变更 → 临时文件 + 原子 rename（`atomic_write`）。
//! 跨命令的串行化由调用方（SlashRegistry 的 `config_mutation` 锁）负责——每次调用
//! 自身是原子的单次改写。

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use crate::fs::atomic_write_private;

/// 进程内所有 rclaw.toml 写回共用一把锁：配置面板、导入、`/search`、模型切换都在
/// 同一把锁下做「比较期望内容 + 原子写」，避免从旧快照互相覆盖。
static TOML_WRITE_LOCK: Mutex<()> = Mutex::new(());

fn lock_writes() -> Result<MutexGuard<'static, ()>, String> {
    TOML_WRITE_LOCK
        .lock()
        .map_err(|_| "rclaw.toml 写入锁已损坏".to_string())
}

/// CAS 冲突的共享判定：调用方据此区分「别人改过」与真实写失败。
pub fn is_cas_conflict(error: &str) -> bool {
    error.contains("已被其他窗口修改")
}

/// 跨进程写锁：同目录 `<config>.lock` 上持系统级独占文件锁。
///
/// 进程内 mutex 只能挡住同进程写者；桌面应用与 CLI 可能同时指向同一个 rclaw.toml，
/// 两边的「比较基线 → 原子写」若交错，后写者会用旧全文覆盖前者的修改。锁文件用
/// `create(true)` 打开（**永不删除**，避免「回收残留锁时删掉别人新锁」的竞争），
/// 由操作系统在进程退出时自动释放。最多等 5 秒，超时报错而不是无限阻塞。
struct CrossProcessLock {
    file: std::fs::File,
}

impl CrossProcessLock {
    fn acquire(target: &Path) -> Result<Self, String> {
        let mut os = target.as_os_str().to_os_string();
        os.push(".lock");
        let path = PathBuf::from(os);
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| {
                format!(
                    "无法打开 rclaw.toml 跨进程写锁（{}）: {error}",
                    path.display()
                )
            })?;
        // 正常写入是毫秒级；有界等待避免 UI/CLI 线程被死锁进程拖住。
        for _ in 0..250 {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(Self { file }),
                // Windows 的竞争错误是 ERROR_LOCK_VIOLATION（33），std 归到
                // Uncategorized；只按 WouldBlock 判断会把它当成真实 I/O 错误。
                Err(error)
                    if error.kind() == std::io::ErrorKind::WouldBlock
                        || error.raw_os_error() == fs2::lock_contended_error().raw_os_error() =>
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(format!(
                        "无法锁定 rclaw.toml 跨进程写锁（{}）: {error}",
                        path.display()
                    ));
                }
            }
        }
        Err("等待 rclaw.toml 跨进程写锁超时（另一个进程正在保存配置），未保存".to_string())
    }
}

impl Drop for CrossProcessLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.file);
    }
}

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
        let (baseline, mut doc) = self.load_with_text()?;
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
                    .map(|name| name.trim() == provider_name.trim())
                    .unwrap_or(false)
            })
            .ok_or_else(|| format!("rclaw.toml 中未找到 provider「{provider_name}」"))?;
        set_table_item_preserving_decor(table, "model", toml_edit::value(model));
        set_table_item_preserving_decor(table, "base_url", toml_edit::value(base_url));
        set_table_item_preserving_decor(table, "context_length", toml_edit::value(context_length));
        self.save_expecting(&doc, Some(&baseline))
    }

    /// 目标 provider 在配置里缺失时追加一个 `[[llm.providers]]` 条目（模型选择器
    /// 自动补端点/能力参数，`api_key` 留空等用户填写）。
    ///
    /// 同时支持两种合法表达（serde 反序列化一视同仁）：数组表
    /// `[[llm.providers]]` 与内联数组 `providers = [{ ... }]`；追加沿用原表达，
    /// 不改写既有条目/凭据，也不动数组现有的注释装饰。
    ///
    /// 返回 `Ok(true)` = 本次真的写入了新条目；`Ok(false)` = 同名条目已存在（并发
    /// 窗口里刚被别的写入者加上），未改文件。写入走 `save_expecting`（跨进程锁 +
    /// CAS），与配置面板/导入/模型切换共享同一套「比较后原子写」协议。
    pub fn ensure_provider_entry(&self, entry: &NewProviderEntry<'_>) -> Result<bool, String> {
        let context_length = i64::try_from(entry.context_length)
            .map_err(|_| "context_length 超出 TOML 整数范围".to_string())?;
        let max_tokens = i64::from(entry.max_tokens);
        let (baseline, mut doc) = self.load_with_text()?;
        if provider_name_in_doc(&doc, entry.name) {
            return Ok(false);
        }
        if doc.get("llm").is_none() {
            doc.insert("llm", toml_edit::Item::Table(explicit_table()));
        }
        match doc.get_mut("llm") {
            Some(toml_edit::Item::Table(llm)) => {
                if !llm.contains_key("providers") {
                    llm.insert(
                        "providers",
                        toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()),
                    );
                }
                match llm.get_mut("providers") {
                    Some(toml_edit::Item::ArrayOfTables(providers)) => {
                        providers.push(new_provider_table(entry, context_length, max_tokens));
                    }
                    Some(toml_edit::Item::Value(toml_edit::Value::Array(providers))) => {
                        push_inline_provider(providers, entry, context_length, max_tokens)?;
                    }
                    Some(_) => {
                        return Err(
                            "llm.providers 不是数组表/内联数组，无法添加 provider".to_string()
                        )
                    }
                    None => {
                        return Err(
                            "llm.providers 不是数组表/内联数组，无法添加 provider".to_string()
                        )
                    }
                }
            }
            Some(toml_edit::Item::Value(value)) if value.as_inline_table().is_some() => {
                let llm = value.as_inline_table_mut().expect("已确认是内联表");
                if llm.get("providers").is_none() {
                    llm.insert(
                        "providers",
                        toml_edit::Value::Array(toml_edit::Array::new()),
                    );
                }
                match llm.get_mut("providers") {
                    Some(toml_edit::Value::Array(providers)) => {
                        push_inline_provider(providers, entry, context_length, max_tokens)?;
                    }
                    _ => return Err("llm.providers 不是内联数组，无法添加 provider".to_string()),
                }
            }
            _ => return Err("[llm] 不是表/内联表，无法添加 provider".to_string()),
        }
        self.save_expecting(&doc, Some(&baseline))?;
        Ok(true)
    }

    /// 模型选择器的一次性整组参数写回。
    ///
    /// - 写入目标 provider 条目的 model / base_url / context_length / max_tokens /
    ///   thinking / reasoning_effort / media_wire / reasoning_field / max_tokens_key；
    /// - `select_provider = true` 时同时把 `[llm].provider` 改为该条目（跨 provider 切换）；
    /// - 检测到 `[llm] model = "..."` 全局覆盖时**拒绝写入**：该键会盖掉条目里的
    ///   model，切换看似成功、重启后却回退——必须让用户先删掉它，而不是静默失败；
    /// - `expected_text` 非空时做 compare-and-swap：文件在这次切换开始后被配置面板
    ///   或另一进程改过就拒绝提交，避免用旧快照覆盖别人的修改。
    /// - 全部改动一次 `save`，避免半套参数落盘的中间态。
    pub fn apply_model_switch(
        &self,
        provider_name: &str,
        params: &ProviderParamUpdate<'_>,
        select_provider: bool,
        expected_text: Option<&str>,
    ) -> Result<(), String> {
        let context_length = i64::try_from(params.context_length)
            .map_err(|_| "context_length 超出 TOML 整数范围".to_string())?;
        // 有 expected 时从【同一份快照文本】解析文档；比较与写在 save_expecting 的
        // 全局锁内完成，配置面板/导入无法在两步之间插入。
        let mut doc = match expected_text {
            Some(text) => text
                .parse::<toml_edit::DocumentMut>()
                .map_err(|error| format!("rclaw.toml 解析失败: {}", toml_parse_summary(&error)))?,
            None => self.load()?,
        };
        if let Some(global_model) = llm_global_model_override(&doc) {
            return Err(format!(
                "[llm] model = \"{global_model}\" 是全局覆盖，模型切换不会生效；请先删除该行后重试"
            ));
        }
        if select_provider {
            let llm = doc
                .get_mut("llm")
                .and_then(|item| item.as_table_mut())
                .ok_or_else(|| "rclaw.toml 中未找到 [llm] 段".to_string())?;
            set_table_item_preserving_decor(llm, "provider", toml_edit::value(provider_name));
        }
        let table = provider_table_mut(&mut doc, provider_name)?;
        set_table_item_preserving_decor(table, "model", toml_edit::value(params.model));
        set_table_item_preserving_decor(table, "base_url", toml_edit::value(params.base_url));
        set_table_item_preserving_decor(table, "context_length", toml_edit::value(context_length));
        set_table_item_preserving_decor(
            table,
            "max_tokens",
            toml_edit::value(i64::from(params.max_tokens)),
        );
        set_table_item_preserving_decor(table, "thinking", toml_edit::value(params.thinking));
        set_table_item_preserving_decor(
            table,
            "reasoning_effort",
            toml_edit::value(params.reasoning_effort),
        );
        set_table_item_preserving_decor(table, "media_wire", toml_edit::value(params.media_wire));
        set_table_item_preserving_decor(
            table,
            "reasoning_field",
            toml_edit::value(params.reasoning_field),
        );
        set_table_item_preserving_decor(
            table,
            "max_tokens_key",
            toml_edit::value(params.max_tokens_key),
        );
        self.save_expecting(&doc, expected_text)
    }

    /// 只改思考参数（会话页选择器用）：`thinking` 必写；`effort=None` 表示删除该键
    /// （关闭思考时不能留下旧的 effort，否则服务端可能仍按旧档位思考）。
    pub fn set_reasoning_fields(
        &self,
        provider_name: &str,
        thinking: &str,
        effort: Option<&str>,
        expected_text: Option<&str>,
    ) -> Result<(), String> {
        let mut doc = match expected_text {
            Some(text) => text
                .parse::<toml_edit::DocumentMut>()
                .map_err(|error| format!("rclaw.toml 解析失败: {}", toml_parse_summary(&error)))?,
            None => self.load()?,
        };
        let table = provider_table_mut(&mut doc, provider_name)?;
        set_table_item_preserving_decor(table, "thinking", toml_edit::value(thinking));
        match effort {
            Some(effort) => {
                set_table_item_preserving_decor(
                    table,
                    "reasoning_effort",
                    toml_edit::value(effort),
                );
            }
            None => {
                table.remove("reasoning_effort");
            }
        }
        self.save_expecting(&doc, expected_text)
    }

    /// 把 `[web_search].order` 改写为字符串数组（段缺该行时补上）。
    pub fn set_search_order(&self, order: &[&str]) -> Result<(), String> {
        let (baseline, mut doc) = self.load_with_text()?;
        let table = doc
            .get_mut("web_search")
            .and_then(|web_search| web_search.as_table_mut())
            .ok_or_else(|| "rclaw.toml 中未找到 [web_search] 段".to_string())?;
        let values = order
            .iter()
            .map(|value| toml_edit::Value::from(*value))
            .collect::<toml_edit::Array>();
        set_table_item_preserving_decor(table, "order", toml_edit::value(values));
        self.save_expecting(&doc, Some(&baseline))
    }

    /// 手动启禁某个搜索 provider：只改 `[web_search.<id>].enabled`，其余调优参数保真。
    pub fn set_search_provider_enabled(&self, id: &str, enabled: bool) -> Result<(), String> {
        let (baseline, mut doc) = self.load_with_text()?;
        let table = doc
            .get_mut("web_search")
            .and_then(|web_search| web_search.as_table_mut())
            .and_then(|web_search| web_search.get_mut(id))
            .and_then(|table| table.as_table_mut())
            .ok_or_else(|| format!("rclaw.toml 中未找到 [web_search.{id}] 段"))?;
        set_table_item_preserving_decor(table, "enabled", toml_edit::value(enabled));
        self.save_expecting(&doc, Some(&baseline))
    }

    /// 改写 `[web_search].fallback`（段缺该行时补上）。
    pub fn set_search_fallback(&self, enabled: bool) -> Result<(), String> {
        let (baseline, mut doc) = self.load_with_text()?;
        let table = doc
            .get_mut("web_search")
            .and_then(|web_search| web_search.as_table_mut())
            .ok_or_else(|| "rclaw.toml 中未找到 [web_search] 段".to_string())?;
        set_table_item_preserving_decor(table, "fallback", toml_edit::value(enabled));
        self.save_expecting(&doc, Some(&baseline))
    }

    /// 从一段 TOML 文本读取 `[llm].provider`（不落盘、不加锁）。
    ///
    /// 供模型/思考切换在提交前核对“磁盘上实际选中的 provider”是否仍是 live 身份，
    /// 避免配置面板改过选中项后，成功提示与重启结果不一致。
    pub fn selected_provider_in_text(&self, text: &str) -> Result<Option<String>, String> {
        let doc = text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| format!("rclaw.toml 解析失败: {}", toml_parse_summary(&error)))?;
        // 缺省/空白 provider 与启动语义一致：有效选中项是默认 DeepSeek。
        let name = doc
            .get("llm")
            .and_then(|item| item.as_table())
            .and_then(|llm| llm.get("provider"))
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or("deepseek")
            .to_string();
        Ok(Some(name))
    }

    /// 面板保存的类型化赋值动作（见 [`Self::apply_edits_preview`]）。
    pub fn apply_edits_preview(&self, edits: &[(String, EditValue)]) -> Result<String, String> {
        self.apply_edits_preview_with_baseline(edits)
            .map(|(_, preview)| preview)
    }

    /// 同上，但额外返回预览所基于的原文：调用方在真正落盘时把它作为 CAS 基线，
    /// 防止预览到保存之间被模型/搜索写入覆盖。
    pub fn apply_edits_preview_with_baseline(
        &self,
        edits: &[(String, EditValue)],
    ) -> Result<(String, String), String> {
        let (baseline, mut doc) = self.load_with_text()?;
        for (path, value) in edits {
            apply_dotted(&mut doc, path, value)?;
        }
        Ok((baseline, doc.to_string()))
    }

    /// 同 [`Self::apply_edits_preview_with_baseline`]，但文件不存在时以空文档为基线
    /// 生成完整预览。首次保存路径用它在写盘前一次准备好整份内容，避免先落一个
    /// 空配置文件再二次写入（中间态会被「已配置」判断误认）。
    pub fn apply_edits_preview_creating(
        &self,
        edits: &[(String, EditValue)],
    ) -> Result<(Option<String>, String), String> {
        let (baseline, mut doc) = match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let doc = text.parse::<toml_edit::DocumentMut>().map_err(|error| {
                    format!("rclaw.toml 解析失败: {}", toml_parse_summary(&error))
                })?;
                (Some(text), doc)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                (None, toml_edit::DocumentMut::new())
            }
            Err(error) => {
                return Err(format!(
                    "读取 rclaw.toml 失败（{}）: {error}",
                    self.path.display()
                ));
            }
        };
        for (path, value) in edits {
            apply_dotted(&mut doc, path, value)?;
        }
        Ok((baseline, doc.to_string()))
    }

    /// 把改写后的文本落盘（原子写 + CRLF 风格保持）；与 apply_edits_preview
    /// 搭配使用，中间可插入调用方的校验。
    pub fn write_text(&self, text: &str) -> Result<(), String> {
        self.write_text_expecting(text, None)
    }

    /// 带 CAS 基线的整文件写回：文件在预览后被其它写入者改过就拒绝。
    pub fn write_text_expecting(
        &self,
        text: &str,
        expected_text: Option<&str>,
    ) -> Result<(), String> {
        let _guard = lock_writes()?;
        let _cross = CrossProcessLock::acquire(&self.path)?;
        if let Some(expected) = expected_text {
            match std::fs::read_to_string(&self.path) {
                Ok(current) => {
                    if current != expected {
                        return Err("rclaw.toml 已被其他窗口修改，请刷新后重试（未保存）".into());
                    }
                }
                // 基线存在（哪怕是空文件）但文件消失了：和内容变化一样是并发冲突，
                // 绝不按「首次创建」复活。
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Err("rclaw.toml 已被其他窗口修改，请刷新后重试（未保存）".into());
                }
                Err(error) => {
                    return Err(format!(
                        "读取 rclaw.toml 失败（{}）: {error}",
                        self.path.display()
                    ));
                }
            }
        }
        let mut out = text.to_string();
        if self.source_was_crlf() {
            out = out.replace("\r\n", "\n").replace('\n', "\r\n");
        }
        atomic_write_private(&self.path, out.as_bytes())
            .map_err(|error| format!("写入 rclaw.toml 失败（{}）: {error}", self.path.display()))
    }

    /// 首次创建：整份内容一次原子发布；目标已存在（含同进程/跨进程抢先创建）时
    /// 返回 CAS 冲突，绝不覆盖。
    pub fn write_text_creating(&self, text: &str) -> Result<(), String> {
        let _guard = lock_writes()?;
        let _cross = CrossProcessLock::acquire(&self.path)?;
        match crate::fs::atomic_create_private(&self.path, text.as_bytes()) {
            Ok(()) => Ok(()),
            Err(error) if error.to_string().contains("已存在") => {
                Err("rclaw.toml 已被其他窗口修改，请刷新后重试（未保存）".into())
            }
            Err(error) => Err(format!(
                "创建 rclaw.toml 失败（{}）: {error}",
                self.path.display()
            )),
        }
    }

    fn load(&self) -> Result<toml_edit::DocumentMut, String> {
        self.load_with_text().map(|(_, doc)| doc)
    }

    /// 读取原文并解析成文档（同一份字节），供需要 CAS 基线的调用方使用。
    fn load_with_text(&self) -> Result<(String, toml_edit::DocumentMut), String> {
        let text = std::fs::read_to_string(&self.path)
            .map_err(|error| format!("读取 rclaw.toml 失败（{}）: {error}", self.path.display()))?;
        let doc = text.parse::<toml_edit::DocumentMut>().map_err(|error| {
            // toml_edit 的错误 Display 可能带源码片段（含 api_key 行）；只保留首行
            // （消息 + 行列），避免把凭据带进会话/日志。
            format!("rclaw.toml 解析失败: {}", toml_parse_summary(&error))
        })?;
        Ok((text, doc))
    }

    /// 在全局写锁内「比较期望文本 → 原子写」。expected 非空时，文件被其它写入者
    /// （配置面板/导入/另一进程）改过就拒绝，保证比较与写在同一个临界区。
    fn save_unlocked(
        &self,
        doc: &toml_edit::DocumentMut,
        expected_text: Option<&str>,
    ) -> Result<(), String> {
        if let Some(expected) = expected_text {
            let current = std::fs::read_to_string(&self.path).map_err(|error| {
                format!("读取 rclaw.toml 失败（{}）: {error}", self.path.display())
            })?;
            if current != expected {
                return Err("rclaw.toml 已被其他窗口修改，请刷新后重试".into());
            }
        }
        let mut out = doc.to_string();
        // 原文件为 CRLF 时，toml_edit 新增键的行尾默认是 LF，会造成混合换行；
        // 统一还原为纯 CRLF（拆分前手写编辑器承诺逐字节保留换行风格）。
        if self.source_was_crlf() {
            out = out.replace("\r\n", "\n").replace('\n', "\r\n");
        }
        atomic_write_private(&self.path, out.as_bytes())
            .map_err(|error| format!("写入 rclaw.toml 失败（{}）: {error}", self.path.display()))
    }

    /// 带 CAS 的写回：doc 必须由 `expected_text` 解析而来；比较与写在同一锁内。
    fn save_expecting(
        &self,
        doc: &toml_edit::DocumentMut,
        expected_text: Option<&str>,
    ) -> Result<(), String> {
        let _guard = lock_writes()?;
        let _cross = CrossProcessLock::acquire(&self.path)?;
        self.save_unlocked(doc, expected_text)
    }

    /// 原文件是否使用 CRLF 行尾。
    fn source_was_crlf(&self) -> bool {
        std::fs::read_to_string(&self.path)
            .map(|text| text.contains("\r\n"))
            .unwrap_or(false)
    }
}

/// TOML 解析错误摘要：保留位置与消息，去掉含源码行的片段（可能带 key）。
fn toml_parse_summary(error: &toml_edit::TomlError) -> String {
    let mut parts = Vec::new();
    for line in error.to_string().lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('|') {
            continue;
        }
        if let Some((before, _)) = trimmed.split_once('|') {
            if before.trim().parse::<usize>().is_ok() {
                continue;
            }
        }
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    if parts.is_empty() {
        "语法错误".to_string()
    } else {
        parts.join("; ")
    }
}

/// 模型选择器整组切换要写回的 provider 参数（字段与 `[[llm.providers]]` 一一对应）。
pub struct ProviderParamUpdate<'a> {
    pub model: &'a str,
    pub base_url: &'a str,
    pub context_length: usize,
    pub max_tokens: u32,
    pub thinking: &'a str,
    pub reasoning_effort: &'a str,
    pub media_wire: &'a str,
    pub reasoning_field: &'a str,
    pub max_tokens_key: &'a str,
}

/// 模型选择器发现目标 provider 缺失时要新增的 `[[llm.providers]]` 条目。
///
/// `api_key` 不在这里给值：自动补的是端点/模型/能力参数，凭据必须由用户手填。
pub struct NewProviderEntry<'a> {
    pub name: &'a str,
    pub base_url: &'a str,
    pub model: &'a str,
    pub context_length: usize,
    pub max_tokens: u32,
    pub thinking: &'a str,
    pub reasoning_effort: &'a str,
    pub media_wire: &'a str,
    pub reasoning_field: &'a str,
    pub max_tokens_key: &'a str,
    /// 能力族（`zhipu` / `deepseek`）；`None` 表示按 base_url 自动判定。
    pub flavor: Option<&'a str>,
}

/// 文本里是否已定义同名 provider 条目（不做任何写入）。
pub fn provider_exists_in_text(text: &str, provider_name: &str) -> Result<bool, String> {
    let doc = parse_doc(text)?;
    Ok(provider_name_in_doc(&doc, provider_name))
}

/// 同名条目是否已有非空 `api_key`（只返回布尔值，不把凭据带出配置层）。
///
/// 条目缺失也算 `false`：调用方应先补条目，再提示用户填 key。
pub fn provider_has_api_key_in_text(text: &str, provider_name: &str) -> Result<bool, String> {
    let doc = parse_doc(text)?;
    Ok(provider_api_key_present_in_doc(&doc, provider_name))
}

fn parse_doc(text: &str) -> Result<toml_edit::DocumentMut, String> {
    text.parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("rclaw.toml 解析失败: {}", toml_parse_summary(&error)))
}

/// `llm.providers` 的两种合法表达（与 `Config` 反序列化的 `Vec<ProviderEntry>`
/// 一致）：数组表 `[[llm.providers]]` 或内联数组 `providers = [{ ... }]`。
enum ProvidersRef<'a> {
    Tables(&'a toml_edit::ArrayOfTables),
    Inline(&'a toml_edit::Array),
}

fn providers_ref(doc: &toml_edit::DocumentMut) -> Option<ProvidersRef<'_>> {
    let providers = match doc.get("llm")? {
        toml_edit::Item::Table(llm) => llm.get("providers")?,
        toml_edit::Item::Value(llm) => {
            // 内联表 `llm = { providers = [...] }`：值只能是内联数组。
            return match llm.as_inline_table()?.get("providers")? {
                toml_edit::Value::Array(array) => Some(ProvidersRef::Inline(array)),
                _ => None,
            };
        }
        _ => return None,
    };
    match providers {
        toml_edit::Item::ArrayOfTables(tables) => Some(ProvidersRef::Tables(tables)),
        toml_edit::Item::Value(toml_edit::Value::Array(array)) => Some(ProvidersRef::Inline(array)),
        _ => None,
    }
}

fn name_matches(value: Option<&str>, provider_name: &str) -> bool {
    value
        .map(|name| name.trim() == provider_name.trim())
        .unwrap_or(false)
}

fn provider_name_in_doc(doc: &toml_edit::DocumentMut, provider_name: &str) -> bool {
    match providers_ref(doc) {
        Some(ProvidersRef::Tables(tables)) => tables.iter().any(|table| {
            name_matches(
                table.get("name").and_then(|name| name.as_str()),
                provider_name,
            )
        }),
        Some(ProvidersRef::Inline(array)) => array.iter().any(|value| {
            value
                .as_inline_table()
                .map(|table| {
                    name_matches(
                        table.get("name").and_then(|name| name.as_str()),
                        provider_name,
                    )
                })
                .unwrap_or(false)
        }),
        None => false,
    }
}

fn provider_api_key_present_in_doc(doc: &toml_edit::DocumentMut, provider_name: &str) -> bool {
    match providers_ref(doc) {
        Some(ProvidersRef::Tables(tables)) => tables
            .iter()
            .find(|table| {
                name_matches(
                    table.get("name").and_then(|name| name.as_str()),
                    provider_name,
                )
            })
            .and_then(|table| table.get("api_key"))
            .and_then(|value| value.as_str())
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false),
        Some(ProvidersRef::Inline(array)) => array
            .iter()
            .filter_map(|value| value.as_inline_table())
            .find(|table| {
                name_matches(
                    table.get("name").and_then(|name| name.as_str()),
                    provider_name,
                )
            })
            .and_then(|table| table.get("api_key"))
            .and_then(|value| value.as_str())
            .map(|key| !key.trim().is_empty())
            .unwrap_or(false),
        None => false,
    }
}

/// 新增条目要写入的字段（数组表与内联表共用，避免两处字段漂移）。
fn provider_field_values(
    entry: &NewProviderEntry<'_>,
    context_length: i64,
    max_tokens: i64,
) -> Vec<(&'static str, toml_edit::Value)> {
    let mut fields = vec![
        ("name", toml_edit::Value::from(entry.name)),
        ("base_url", toml_edit::Value::from(entry.base_url)),
        ("model", toml_edit::Value::from(entry.model)),
        // 空 api_key 让构造期明确报「缺少 api_key」，而不是用别人的凭据发请求。
        ("api_key", toml_edit::Value::from("")),
        ("context_length", toml_edit::Value::from(context_length)),
        ("max_tokens", toml_edit::Value::from(max_tokens)),
        ("thinking", toml_edit::Value::from(entry.thinking)),
        (
            "reasoning_effort",
            toml_edit::Value::from(entry.reasoning_effort),
        ),
        ("media_wire", toml_edit::Value::from(entry.media_wire)),
        (
            "reasoning_field",
            toml_edit::Value::from(entry.reasoning_field),
        ),
        (
            "max_tokens_key",
            toml_edit::Value::from(entry.max_tokens_key),
        ),
    ];
    if let Some(flavor) = entry.flavor {
        fields.push(("flavor", toml_edit::Value::from(flavor)));
    }
    fields
}

fn new_provider_table(
    entry: &NewProviderEntry<'_>,
    context_length: i64,
    max_tokens: i64,
) -> toml_edit::Table {
    let mut table = toml_edit::Table::new();
    for (key, value) in provider_field_values(entry, context_length, max_tokens) {
        table.insert(key, toml_edit::Item::Value(value));
    }
    // 固定文案：条目名来自配置，可能含换行/控制字符，不拼进原始装饰。
    table
        .decor_mut()
        .set_prefix("# 由模型选择器自动添加：请先填写 api_key，再重新选择该模型\n");
    table
}

fn push_inline_provider(
    providers: &mut toml_edit::Array,
    entry: &NewProviderEntry<'_>,
    context_length: i64,
    max_tokens: i64,
) -> Result<(), String> {
    if providers
        .iter()
        .any(|value| value.as_inline_table().is_none())
    {
        return Err("llm.providers 内联数组包含非内联表值，无法添加 provider".to_string());
    }
    let mut table = toml_edit::InlineTable::new();
    for (key, value) in provider_field_values(entry, context_length, max_tokens) {
        table.insert(key, value);
    }
    providers.push(toml_edit::Value::InlineTable(table));
    Ok(())
}

/// 读取 `[llm] model` 全局覆盖（trim 后非空才算存在）。
fn llm_global_model_override(doc: &toml_edit::DocumentMut) -> Option<String> {
    doc.get("llm")
        .and_then(|item| item.as_table())
        .and_then(|llm| llm.get("model"))
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .map(str::to_string)
}

/// 按 `name` 定位 `[[llm.providers]]` 中的条目表。
fn provider_table_mut<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    provider_name: &str,
) -> Result<&'a mut toml_edit::Table, String> {
    let providers = doc
        .get_mut("llm")
        .and_then(|llm| llm.as_table_mut())
        .and_then(|llm| llm.get_mut("providers"))
        .and_then(|providers| providers.as_array_of_tables_mut())
        .ok_or_else(|| "rclaw.toml 中未找到 [[llm.providers]] 段".to_string())?;
    providers
        .iter_mut()
        .find(|table| {
            table
                .get("name")
                .and_then(|name| name.as_str())
                .map(|name| name.trim() == provider_name.trim())
                .unwrap_or(false)
        })
        .ok_or_else(|| format!("rclaw.toml 中未找到 provider「{provider_name}」"))
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
/// 路径末段是键名；下钻同时支持常规表/数组表与内联表/内联数组（`llm = { ... }`、
/// `providers = [{ ... }]` 都是合法且可加载的写法，设置页必须能填其中的 key）；
/// 数组段下一段按条目的 `name` 字段匹配；缺失的中间表会创建（显式表头，注释与
/// 既有键序保真）。
fn apply_dotted(
    doc: &mut toml_edit::DocumentMut,
    path: &str,
    value: &EditValue,
) -> Result<(), String> {
    let mut segments: Vec<&str> = path.split('.').collect();
    let Some(key) = segments.pop() else {
        return Err(format!("配置路径为空: {path}"));
    };
    // 与读路径一致：空段（`a..b`）不是合法寻址，不能把它当成某个空名条目。
    if segments.is_empty() || key.is_empty() || segments.iter().any(|seg| seg.is_empty()) {
        return Err(format!("配置路径至少要包含段名与键名: {path}"));
    }
    match descend_edit_container(doc, &segments)? {
        EditContainer::Table(table) => {
            set_table_item_preserving_decor(table, key, edit_value_to_item(value));
        }
        EditContainer::Inline(table) => {
            table.insert(key, edit_value_to_value(value));
        }
    }
    Ok(())
}

/// 下钻终点的两种容器：常规表（含数组表条目）或内联表（含内联数组条目）。
enum EditContainer<'a> {
    Table(&'a mut toml_edit::Table),
    Inline(&'a mut toml_edit::InlineTable),
}

/// 沿段名下行到目标容器；数组段（数组表或内联数组）的下一段按条目 `name=` 匹配。
/// 缺失的中间表自动创建，与旧实现保持一致。
fn descend_edit_container<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    segments: &[&str],
) -> Result<EditContainer<'a>, String> {
    let mut container = EditContainer::Table(doc.as_table_mut());
    let mut index = 0;
    while index < segments.len() {
        let seg = segments[index];
        match container {
            EditContainer::Table(table) => {
                if !table.contains_key(seg) {
                    table.insert(seg, toml_edit::Item::Table(explicit_table()));
                }
                let item = table
                    .get_mut(seg)
                    .ok_or_else(|| format!("配置缺少段 [{seg}]"))?;
                match item {
                    toml_edit::Item::Table(sub) => {
                        container = EditContainer::Table(sub);
                        index += 1;
                    }
                    toml_edit::Item::ArrayOfTables(entries) => {
                        let name = *segments.get(index + 1).ok_or_else(|| {
                            format!("路径在数组段 [{seg}] 后缺少按 name 匹配的条目名")
                        })?;
                        let sub = entries
                            .iter_mut()
                            .find(|entry| {
                                entry
                                    .get("name")
                                    .and_then(|name| name.as_str())
                                    .map(|value| value.trim() == name.trim())
                                    .unwrap_or(false)
                            })
                            .ok_or_else(|| {
                                format!("[{seg}] 数组中未找到 name=\"{name}\" 的条目")
                            })?;
                        container = EditContainer::Table(sub);
                        index += 2;
                    }
                    toml_edit::Item::Value(value) => {
                        container = edit_container_from_value(value, seg, segments, &mut index)?
                    }
                    toml_edit::Item::None => {
                        return Err(format!("段 [{seg}] 已是非表值，无法继续下钻"))
                    }
                }
            }
            EditContainer::Inline(table) => {
                let value = table
                    .get_mut(seg)
                    .ok_or_else(|| format!("配置缺少键 {seg}"))?;
                container = edit_container_from_value(value, seg, segments, &mut index)?;
            }
        }
    }
    Ok(container)
}

/// 从内联表/内联数组值继续下钻；`index` 会推进到下一段（数组多消费一个 name）。
fn edit_container_from_value<'a>(
    value: &'a mut toml_edit::Value,
    seg: &str,
    segments: &[&str],
    index: &mut usize,
) -> Result<EditContainer<'a>, String> {
    match value {
        toml_edit::Value::InlineTable(table) => {
            *index += 1;
            Ok(EditContainer::Inline(table))
        }
        toml_edit::Value::Array(entries) => {
            let name = *segments
                .get(*index + 1)
                .ok_or_else(|| format!("路径在数组段 [{seg}] 后缺少按 name 匹配的条目名"))?;
            let entry = entries
                .iter_mut()
                .filter_map(|value| value.as_inline_table_mut())
                .find(|entry| {
                    entry
                        .get("name")
                        .and_then(|name| name.as_str())
                        .map(|value| value.trim() == name.trim())
                        .unwrap_or(false)
                })
                .ok_or_else(|| format!("[{seg}] 数组中未找到 name=\"{name}\" 的条目"))?;
            *index += 2;
            Ok(EditContainer::Inline(entry))
        }
        _ => Err(format!("段 [{seg}] 已是非表值，无法继续下钻")),
    }
}

fn explicit_table() -> toml_edit::Table {
    let mut t = toml_edit::Table::new();
    t.set_implicit(false);
    t
}

fn edit_value_to_value(value: &EditValue) -> toml_edit::Value {
    match value {
        EditValue::Bool(b) => toml_edit::Value::from(*b),
        EditValue::Int(i) => toml_edit::Value::from(*i),
        EditValue::Float(f) => toml_edit::Value::from(*f),
        EditValue::Str(s) => toml_edit::Value::from(s.as_str()),
        EditValue::StrArray(items) => toml_edit::Value::Array(
            items
                .iter()
                .map(|s| toml_edit::Value::from(s.as_str()))
                .collect::<toml_edit::Array>(),
        ),
    }
}

fn edit_value_to_item(value: &EditValue) -> toml_edit::Item {
    toml_edit::Item::Value(edit_value_to_value(value))
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
    fn ensure_provider_entry_appends_missing_provider_once() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "# 顶部注释\n[llm]\nprovider = \"zhipu\"\n\n[[llm.providers]]\nname = \"zhipu\"\nmodel = \"glm-5.3\"\nbase_url = \"https://open.bigmodel.cn/api/coding/paas/v4\"\napi_key = \"secret\"\n",
        );
        let entry = NewProviderEntry {
            name: "deepseek",
            base_url: "https://api.deepseek.com/v1",
            model: "deepseek-flash",
            context_length: 1_048_576,
            max_tokens: 384_000,
            thinking: "enabled",
            reasoning_effort: "max",
            media_wire: "deepseek",
            reasoning_field: "reasoning_content",
            max_tokens_key: "max_tokens",
            flavor: Some("deepseek"),
        };
        assert!(toml.ensure_provider_entry(&entry).unwrap());
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 顶部注释"), "注释必须保真");
        assert!(text.contains("name = \"deepseek\""));
        assert!(text.contains("api_key = \"\""), "自动补的条目 key 必须留空");
        assert!(text.contains("context_length = 1048576"));
        assert!(text.contains("max_tokens = 384000"));
        assert!(text.contains("flavor = \"deepseek\""));
        assert!(text.contains("请先填写 api_key"));
        assert!(provider_exists_in_text(&text, "deepseek").unwrap());
        assert!(!provider_has_api_key_in_text(&text, "deepseek").unwrap());
        assert!(provider_has_api_key_in_text(&text, "zhipu").unwrap());

        // 幂等：同名条目已存在时不重复追加，也不改文件。
        assert!(!toml.ensure_provider_entry(&entry).unwrap());
        let again = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(again.matches("name = \"deepseek\"").count(), 1);
    }

    #[test]
    fn ensure_provider_entry_creates_llm_section() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), "# 空配置\n");
        let entry = NewProviderEntry {
            name: "deepseek",
            base_url: "https://api.deepseek.com/v1",
            model: "deepseek-flash",
            context_length: 1_048_576,
            max_tokens: 384_000,
            thinking: "enabled",
            reasoning_effort: "max",
            media_wire: "deepseek",
            reasoning_field: "reasoning_content",
            max_tokens_key: "max_tokens",
            flavor: None,
        };
        assert!(toml.ensure_provider_entry(&entry).unwrap());
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 空配置"));
        assert!(text.contains("[[llm.providers]]"), "{text}");
        assert!(text.contains("name = \"deepseek\""), "{text}");
        assert!(!text.contains("flavor ="), "{text}");
    }

    #[test]
    fn ensure_provider_entry_appends_to_inline_providers() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), "[llm]\nproviders = []\n");
        let entry = NewProviderEntry {
            name: "deepseek",
            base_url: "https://api.deepseek.com/v1",
            model: "deepseek-flash",
            context_length: 1_048_576,
            max_tokens: 384_000,
            thinking: "enabled",
            reasoning_effort: "max",
            media_wire: "deepseek",
            reasoning_field: "reasoning_content",
            max_tokens_key: "max_tokens",
            flavor: Some("deepseek"),
        };
        assert!(toml.ensure_provider_entry(&entry).unwrap());
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        // 沿用内联数组表达，不改写成数组表（避免丢装饰/语法形态）。
        assert!(text.contains("providers = ["), "{text}");
        assert!(text.contains("name = \"deepseek\""), "{text}");
        assert!(!text.contains("[[llm.providers]]"), "{text}");
        assert!(provider_exists_in_text(&text, "deepseek").unwrap());
    }

    #[test]
    fn ensure_provider_entry_keeps_inline_array_comments() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[llm]\nproviders = [\n  # 手写注释：以后加代理条目\n]\n",
        );
        let entry = NewProviderEntry {
            name: "deepseek",
            base_url: "https://api.deepseek.com/v1",
            model: "deepseek-flash",
            context_length: 1_048_576,
            max_tokens: 384_000,
            thinking: "enabled",
            reasoning_effort: "max",
            media_wire: "deepseek",
            reasoning_field: "reasoning_content",
            max_tokens_key: "max_tokens",
            flavor: Some("deepseek"),
        };
        assert!(toml.ensure_provider_entry(&entry).unwrap());
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 手写注释：以后加代理条目"), "{text}");
        assert!(text.contains("name = \"deepseek\""), "{text}");
        // 注释保留后仍必须是可解析的 TOML。
        assert!(text.parse::<toml_edit::DocumentMut>().is_ok(), "{text}");
    }

    #[test]
    fn inline_provider_entries_are_found_and_checked() {
        let text = "[llm]\nprovider = \"deepseek\"\nproviders = [\n  { name = \"zhipu\", base_url = \"https://open.bigmodel.cn/api/coding/paas/v4\", model = \"glm-5.3\", api_key = \"secret\" },\n  { name = \"deepseek\", base_url = \"https://api.deepseek.com/v1\", model = \"deepseek-flash\", api_key = \"\" },\n]\n";
        assert!(provider_exists_in_text(text, "zhipu").unwrap());
        assert!(provider_exists_in_text(text, "deepseek").unwrap());
        assert!(!provider_exists_in_text(text, "other").unwrap());
        assert!(provider_has_api_key_in_text(text, "zhipu").unwrap());
        assert!(!provider_has_api_key_in_text(text, "deepseek").unwrap());

        // 已存在的内联条目不会再被追加。
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), text);
        let entry = NewProviderEntry {
            name: "deepseek",
            base_url: "https://api.deepseek.com/v1",
            model: "deepseek-flash",
            context_length: 1_048_576,
            max_tokens: 384_000,
            thinking: "enabled",
            reasoning_effort: "max",
            media_wire: "deepseek",
            reasoning_field: "reasoning_content",
            max_tokens_key: "max_tokens",
            flavor: Some("deepseek"),
        };
        assert!(!toml.ensure_provider_entry(&entry).unwrap());
        let unchanged = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(unchanged.matches("name = \"deepseek\"").count(), 1);
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
    fn apply_edits_preview_supports_inline_providers_and_inline_llm() {
        // 常规 [llm] + 内联 providers 数组：设置页能按 name 填 api_key。
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "[llm]\nprovider = \"deepseek\"\nproviders = [\n  { name = \"zhipu\", model = \"glm-5.3\", api_key = \"secret\" },\n  { name = \"deepseek\", model = \"deepseek-flash\", api_key = \"\" },\n]\n",
        );
        let out = toml
            .apply_edits_preview(&[
                (
                    "llm.providers.deepseek.api_key".into(),
                    EditValue::Str("test-key".into()),
                ),
                ("llm.provider".into(), EditValue::Str("zhipu".into())),
            ])
            .unwrap();
        assert!(out.contains("providers = ["), "必须保留内联数组形态: {out}");
        assert!(out.contains("api_key = \"test-key\""), "{out}");
        assert!(
            out.contains("api_key = \"secret\""),
            "其它条目不得被改: {out}"
        );
        assert!(out.contains("provider = \"zhipu\""), "{out}");

        // 整段 llm 内联表。
        let dir2 = tempfile::tempdir().unwrap();
        let toml2 = write_config(
            dir2.path(),
            "llm = { provider = \"deepseek\", providers = [{ name = \"deepseek\", model = \"deepseek-flash\", api_key = \"\" }] }\n",
        );
        let out2 = toml2
            .apply_edits_preview(&[(
                "llm.providers.deepseek.api_key".into(),
                EditValue::Str("test-key-2".into()),
            )])
            .unwrap();
        assert!(out2.contains("api_key = \"test-key-2\""), "{out2}");
        assert!(out2.starts_with("llm = {"), "{out2}");
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
        // 裸键名（无段）与空段（`a..b`）都是错误。
        assert!(toml
            .apply_edits_preview(&[("base_dir".into(), EditValue::Str("x".into()))])
            .is_err());
        assert!(toml
            .apply_edits_preview(&[("llm.providers..model".into(), EditValue::Str("x".into()))])
            .is_err());
    }

    #[test]
    fn empty_file_baseline_deleted_is_conflict_not_recreate() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(dir.path(), "");
        std::fs::remove_file(dir.path().join("rclaw.toml")).unwrap();
        let error = toml
            .write_text_expecting(
                "[log]
console = true
",
                Some(""),
            )
            .unwrap_err();
        assert!(is_cas_conflict(&error), "{error}");
        assert!(!dir.path().join("rclaw.toml").exists());
    }

    #[test]
    fn creating_refuses_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let toml = write_config(
            dir.path(),
            "existing = true
",
        );
        let error = toml
            .write_text_creating(
                "new = true
",
            )
            .unwrap_err();
        assert!(is_cas_conflict(&error), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap(),
            "existing = true
"
        );
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
