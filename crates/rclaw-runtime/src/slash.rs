//! 斜杠命令模块。
//!
//! 在 agent loop 处理每条【用户】消息的最开头拦截：若正文以 `/` 开头则派发给对应命令，
//! 命中即返回回复文本并跳过 LLM；未命中（非斜杠）则原样放行进入正常对话。
//!
//! 设计目标：
//! - **通道无关**：CLI、微信等任意通道发的 `/cmd` 都会被处理，回复按原 `channel`/`chat_id`
//!   经 outbound 回送，agent_loop 中已据此提前 return（不走 LLM、不污染会话历史）。
//! - **可扩展**：不依赖 turn 状态的命令在 [`SlashRegistry::try_handle`] 增加分支；需要
//!   workspace、session 或活动 turn 状态的命令（如 `/skills`、`/tools`、`/context`）由
//!   turn 的命令分发器在进入 LLM 前处理。
//!
//! 当前命令：
//! - `/llmodel [模型名]`：热切换 [[llm.providers]] 的 model / base_url，并按模型写入
//!   `context_length`（glm-5.3=1M、glm-5.3-flash=1M），同步写回 rclaw.toml。
//! - `/search [provider]`：把指定 web_search 方式移到优先级首位。
//! - `/search dis|en <provider>`：运行期禁用/启用方式并同步写回 rclaw.toml。
//! - `/searchfallback [on|off]`：开关 `web_search` 的主→辅 fallback（关闭后只用主搜索源，
//!   失败也不回退），同步写回 rclaw.toml。
//! - `/webbrowser [google|bing|baidu]`：查看/切换 `web_browser` 无头浏览器检索的
//!   首选搜索引擎，热切换并同步写回 rclaw.toml 的 `[web_browser].engine`。
//! - `/skills`：由 turn 命令分发器列出当前 workspace 可发现的 skill 名称、所属技能包与功能。
//! - `/tools`：由 turn 命令分发器列出当前注册的全部工具名称与说明。
//! - `/log`：由 turn 命令分发器查看 / 发送 / 删除 agent/logs 下的日志文件。
//! - `/help`：列出可用命令。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rclaw_llm::SwappableProvider;
use rclaw_tools::{web_browser, BackendId, WebSearchControl};

/// 斜杠命令处理结果。
pub enum CommandOutcome {
    /// 命中命令；附带要回送给用户的文本（agent_loop 据此直接 outbound 回复）。
    Handled(String),
    /// 非斜杠命令，放行进入正常 LLM 对话。
    NotACommand,
}

/// 已知模型规格：规范模型名 + 切换后应使用的 base_url + 上下文窗口（token）。
struct ModelSpec {
    model: &'static str,
    base_url: &'static str,
    context_length: usize,
}

/// 已知模型清单（按用户输入的小写名匹配）。
///
/// 规则与 rclaw.toml 现状一致：
/// - `glm-5.3`（编码模型，1M 上下文）→ `…/api/coding/paas/v4`
/// - `glm-5.3-flash`（原生多模态模型，1M 上下文）→ `…/api/coding/paas/v4`
///
/// `context_length` 用于派生会话压缩 token 预算（见 `rclaw_runtime::options::
/// derive_compression_token_budgets`），随模型一起写回 rclaw.toml，重启后生效。
///
/// 新增模型只需在此 match 增加一个分支。
fn resolve_model_spec(input: &str) -> Option<ModelSpec> {
    match input.trim().to_ascii_lowercase().as_str() {
        "glm-5.3-flash" => Some(ModelSpec {
            model: "glm-5.3-flash",
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            // token 计数用十进制（行业惯例），不是 1024 进制的字节数。
            context_length: 1_000_000,
        }),
        "glm-5.3" => Some(ModelSpec {
            model: "glm-5.3",
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            context_length: 1_000_000,
        }),
        // 旧名兼容：glm-5.2 → glm-5.3（升级改名后 rclaw.toml 与用户习惯仍可用旧名）。
        "glm-5.2" => Some(ModelSpec {
            model: "glm-5.3",
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            context_length: 1_000_000,
        }),
        // 旧名兼容：GLM-5V-Turbo → glm-5.3-flash（多模态位接替者，存量习惯仍可用旧名切换）。
        "glm-5v-turbo" => Some(ModelSpec {
            model: "glm-5.3-flash",
            base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
            context_length: 1_000_000,
        }),
        _ => None,
    }
}

/// 斜杠命令注册表。持有一份 provider 热切换句柄、rclaw.toml 路径与当前选中的 provider 名。
pub struct SlashRegistry {
    provider: Arc<SwappableProvider>,
    /// rclaw.toml 路径（与 `Config::load` 读取的相同）。
    config_path: PathBuf,
    /// 当前选中的 `[[llm.providers]]` 名（`[llm].provider`）——持久化时定位要改的 provider 段。
    selected_provider: String,
    /// web_search 顺序、手动启禁、健康状态及 fallback 开关的共享控制面。
    web_search_control: Option<Arc<WebSearchControl>>,
    /// 串行化运行时切换与 TOML 写回，避免多个会话的斜杠命令相互覆盖。
    config_mutation: Mutex<()>,
}

impl SlashRegistry {
    pub fn new(
        provider: Arc<SwappableProvider>,
        config_path: PathBuf,
        selected_provider: String,
    ) -> Self {
        SlashRegistry {
            provider,
            config_path,
            selected_provider,
            web_search_control: None,
            config_mutation: Mutex::new(()),
        }
    }

    /// 注入 web_search 控制面，启用顺序、启禁与 fallback 的热切换。
    pub fn with_web_search_control(mut self, control: Arc<WebSearchControl>) -> Self {
        self.web_search_control = Some(control);
        self
    }

    /// web_search 控制面句柄（turn 组装工具面 / 系统提示时做禁能联动用）。
    pub fn web_search_control(&self) -> Option<Arc<WebSearchControl>> {
        self.web_search_control.clone()
    }

    /// 尝试把一条消息正文当作斜杠命令处理。
    ///
    /// - 非 `/` 开头 → [`CommandOutcome::NotACommand`]，放行进 LLM；
    /// - `/` 后是已知命令名（`llmodel` / `search` / `searchfallback` / `webbrowser` /
    ///   `help`）→ 执行命令并返回回复；
    /// - `/` 后是未知名（如 `/etc/hosts`、`/typo`）→ 也放行进 LLM。
    ///   RClaw 常处理文件路径 / shell 片段，未知名若一律报错会误伤用户想发给模型的内容；
    ///   因此只有明确命中的命令才拦截，用 `/help` 发现可用命令。
    pub fn try_handle(&self, content: &str) -> CommandOutcome {
        let trimmed = content.trim();
        let Some(rest) = trimmed.strip_prefix('/') else {
            return CommandOutcome::NotACommand;
        };
        let rest = rest.trim_start();
        // 拆出命令名（首个空白前）与剩余参数。
        let (name, args) = match rest.split_once(char::is_whitespace) {
            Some((n, a)) => (n, a.trim()),
            None => (rest, ""),
        };
        match name.to_ascii_lowercase().as_str() {
            "llmodel" => CommandOutcome::Handled(self.handle_llmodel(args)),
            "search" => CommandOutcome::Handled(self.handle_search(args)),
            "searchfallback" => CommandOutcome::Handled(self.handle_searchfallback(args)),
            "webbrowser" => CommandOutcome::Handled(self.handle_webbrowser(args)),
            "help" => CommandOutcome::Handled(self.help_text()),
            _ => CommandOutcome::NotACommand,
        }
    }

    /// `/llmodel [模型名]`：无参回显当前模型；有参则热切换并持久化。
    fn handle_llmodel(&self, args: &str) -> String {
        // 只取首个 token 作为模型名，忽略多余参数，容错性更好。
        let input = args.split_whitespace().next().unwrap_or("");
        if input.is_empty() {
            let (model, base_url) = match self.provider.current_model() {
                Ok(current) => current,
                Err(e) => return format!("读取当前模型失败：{e}"),
            };
            return format!(
                "当前模型：{model}\nbase_url：{base_url}\n\n可用模型：glm-5.3、glm-5.3-flash\n用法：/llmodel <模型名>"
            );
        }
        let Some(spec) = resolve_model_spec(input) else {
            return format!(
                "未知模型「{input}」。可用模型：glm-5.3、glm-5.3-flash\n用法：/llmodel <模型名>"
            );
        };
        let model = spec.model.to_string();
        let base_url = spec.base_url.to_string();
        let context_length = spec.context_length;
        let _mutation = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return "配置修改锁已损坏，未切换模型。".into(),
        };

        // 1) 热切换 live provider（仅改 model / base_url，保留 api_key / auth / dialect 等）。
        //    构造失败时 switch_model 内部什么都不改。
        if let Err(e) = self.provider.switch_model(model.clone(), base_url.clone()) {
            return format!("切换模型失败（未改动）：{e}");
        }

        // 2) 持久化到 rclaw.toml：改选中 provider 的 model / base_url / context_length
        //    三项，其余内容（含注释与键序）保真（C6：toml_edit 注释保真写入）。
        //    context_length 仅用于下次启动派生压缩 token 预算（运行期预算已在启动时烘进
        //    AgentOptions，本会话不重派生）。
        match rclaw_store::toml_file::RclawToml::new(&self.config_path).set_provider_fields(
            &self.selected_provider,
            &model,
            &base_url,
            context_length,
        ) {
            Ok(()) => format!(
                "✅ 已切换模型：{model}\nbase_url：{base_url}\ncontext_length：{context_length}"
            ),
            Err(e) => {
                format!("⚠️ 运行时已切换为 {model}，但写入 rclaw.toml 失败（下次启动会回退）：{e}")
            }
        }
    }

    /// `/search` 显示状态；`/search <provider>` 把方式移到首位；
    /// `/search dis|en <provider>` 持久禁用/启用。禁用不改变顺序，重新启用后回到原位。
    fn handle_search(&self, args: &str) -> String {
        let Some(control) = self.web_search_control.as_ref() else {
            return "web_search 未启用，无法调整搜索优先级。".to_string();
        };
        let tokens = args.split_whitespace().collect::<Vec<_>>();
        if tokens.is_empty() || tokens[0].eq_ignore_ascii_case("status") {
            return self.search_status(control);
        }

        if matches!(tokens[0].to_ascii_lowercase().as_str(), "dis" | "en") {
            if tokens.len() != 2 {
                return "用法：/search dis|en <zhipu|tavily|doubao-custom|doubao-global>"
                    .to_string();
            }
            let Some(id) = BackendId::parse(tokens[1]) else {
                return search_unknown_provider(tokens[1]);
            };
            if !control.is_configured(id) {
                return format!("搜索方式 {id} 未配置");
            }
            let enabled = tokens[0].eq_ignore_ascii_case("en");
            let _mutation = match self.config_mutation.lock() {
                Ok(guard) => guard,
                Err(_) => return "配置修改锁已损坏，未修改搜索方式。".into(),
            };
            let id_table = match id {
                BackendId::Zhipu => "zhipu",
                BackendId::Tavily => "tavily",
                BackendId::DoubaoCustom => "doubao_custom",
                BackendId::DoubaoGlobal => "doubao_global",
            };
            if let Err(error) = rclaw_store::toml_file::RclawToml::new(&self.config_path)
                .set_search_provider_enabled(id_table, enabled)
            {
                return format!("写入 rclaw.toml 失败，运行时状态未改动：{error}");
            }
            // 已在锁内确认 provider 配置存在，此处不会失败。
            control
                .set_enabled(id, enabled)
                .expect("已配置的 web_search provider 必须可修改启用状态");
            tracing::info!(
                provider = id.as_str(),
                enabled,
                "用户通过 /search 修改搜索方式启用状态"
            );
            return format!(
                "✅ {} 已{}。\n{}",
                id.label(),
                if enabled {
                    "启用并清除阻塞状态"
                } else {
                    "禁用"
                },
                self.search_status(control)
            );
        }

        if tokens.len() != 1 {
            return "用法：/search <zhipu|tavily|doubao-custom|doubao-global>".into();
        }
        let Some(id) = BackendId::parse(tokens[0]) else {
            return search_unknown_provider(tokens[0]);
        };
        if !control.is_configured(id) {
            return format!("搜索方式 {id} 未配置");
        }
        let _mutation = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return "配置修改锁已损坏，未调整搜索顺序。".into(),
        };
        let mut order = control.order();
        order.retain(|item| *item != id);
        order.insert(0, id);
        let values = order.iter().map(|id| id.as_str()).collect::<Vec<_>>();
        if let Err(error) =
            rclaw_store::toml_file::RclawToml::new(&self.config_path).set_search_order(&values)
        {
            return format!("写入 rclaw.toml 失败，运行时顺序未改动：{error}");
        }
        control
            .move_to_front(id)
            .expect("已配置的 web_search provider 必须可调整顺序");
        tracing::info!(
            provider = id.as_str(),
            order = ?order.iter().map(|item| item.as_str()).collect::<Vec<_>>(),
            "用户通过 /search 切换主搜索方式"
        );
        format!(
            "✅ 已将 {} 设为首选搜索方式。\n{}",
            id.label(),
            self.search_status(control)
        )
    }

    /// `/searchfallback <on|off>`：开关 web_search 的主→辅 fallback。无参回显当前状态。
    /// 关闭后 web_search 只用当前主搜索源（`/search` 设定的那个），主路径失败/不足时
    /// 也不会回退到另一个 provider。web_search 未启用时提示不可用。
    fn handle_searchfallback(&self, args: &str) -> String {
        let Some(control) = self.web_search_control.as_ref() else {
            return "web_search 未启用，无法调整 fallback。".to_string();
        };
        let input = args.split_whitespace().next().unwrap_or("");
        if input.is_empty() {
            return format!(
                "web_search fallback 当前：{}\n用法：/searchfallback <on|off>",
                self.search_fallback_status(control)
            );
        }
        let enabled = match input.to_ascii_lowercase().as_str() {
            "on" | "true" | "1" | "开" | "开启" => true,
            "off" | "false" | "0" | "关" | "关闭" => false,
            _ => {
                return format!(
                    "未知参数「{input}」。支持：on / off\n用法：/searchfallback <on|off>"
                );
            }
        };
        let _mutation = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return "配置修改锁已损坏，未修改 fallback。".into(),
        };
        if let Err(error) =
            rclaw_store::toml_file::RclawToml::new(&self.config_path).set_search_fallback(enabled)
        {
            return format!("写入 rclaw.toml 失败，运行时 fallback 未改动：{error}");
        }
        control.set_fallback_enabled(enabled);
        tracing::info!(enabled, "用户通过 /searchfallback 修改 fallback 状态");
        format!(
            "✅ web_search fallback 已{}",
            self.search_fallback_status(control)
        )
    }

    /// `/webbrowser [google|bing|baidu]`：查看/切换 web_browser 无头浏览器检索
    /// 的首选搜索引擎。无参回显当前引擎与降级链；有参热切换（进程级槽位）
    /// 并同步写回 rclaw.toml 的 `[web_browser].engine`（段缺失时创建）。
    /// 写盘失败时运行时状态不改动，与 /search 的语义一致。
    fn handle_webbrowser(&self, args: &str) -> String {
        let input = args.split_whitespace().next().unwrap_or("");
        if input.is_empty() {
            let primary = web_browser::preferred_engine();
            let chain = web_browser::fallback_chain(primary)
                .iter()
                .map(|engine| engine.label())
                .collect::<Vec<_>>()
                .join(" → ");
            return format!(
                "web_browser 首选搜索引擎：{}（{}）\n无结果自动降级：{}\n用法：/webbrowser <google|bing|baidu>",
                primary.label(),
                primary.as_str(),
                chain
            );
        }
        let Some(engine) = web_browser::SearchEngine::parse(input) else {
            return format!("未知搜索引擎「{input}」。支持：google / bing / baidu");
        };
        let _mutation = match self.config_mutation.lock() {
            Ok(guard) => guard,
            Err(_) => return "配置修改锁已损坏，未切换搜索引擎。".into(),
        };
        if let Err(error) = rclaw_store::toml_file::RclawToml::new(&self.config_path)
            .set_web_browser_engine(engine.as_str())
        {
            return format!("写入 rclaw.toml 失败，运行时引擎未改动：{error}");
        }
        web_browser::set_preferred_engine(engine);
        tracing::info!(engine = engine.as_str(), "用户通过 /webbrowser 切换首选搜索引擎");
        format!(
            "✅ web_browser 首选搜索引擎已切换为 {}（{}）；无结果时自动降级：{}",
            engine.label(),
            engine.as_str(),
            web_browser::fallback_chain(engine)
                .iter()
                .skip(1)
                .map(|next| next.label())
                .collect::<Vec<_>>()
                .join(" → ")
        )
    }

    /// `/help`：列出可用命令。`/search` 行附带当前 web_search 顺序与 fallback 开关状态，
    /// 让用户一眼看出当前主搜索方式及自动降级是否开启。
    fn help_text(&self) -> String {
        let search_status = match self.web_search_control.as_ref() {
            None => "（web_search 未启用）".to_string(),
            Some(control) => format!(
                "（{}）",
                self.search_status(control)
                    .lines()
                    .next()
                    .unwrap_or("状态未知")
            ),
        };
        format!(
            "可用斜杠命令：\n\
             /llmodel <模型名>  切换 LLM 模型（热切换，同步更新 rclaw.toml）\n\
             /search [方式]       显示状态或把指定方式移到首位 {search_status}\n\
             /search dis|en <方式>  禁用/启用 zhipu、tavily、doubao-custom、doubao-global\n\
             /searchfallback <on|off>  开关 web_search fallback（关闭后只用主搜索源，失败也不回退）；状态见上方 /search 行\n\
             /webbrowser [引擎]   查看/切换 web_browser 无头浏览器检索的首选引擎（google / bing / baidu，热切换并同步 rclaw.toml）\n\
             /skills            显示当前支持的所有 skill 名称、所属技能包及功能\n\
             /tools             显示当前支持的所有工具名称及说明\n\
             /context           显示当前会话上下文用量（窗口/已用/剩余/使用率）\n\
             /clear [confirm]   清空当前会话并删除 agent/memory、agent/sessions 全部文件（需 /clear confirm）\n\
             /log [子命令]      查看 / 发送 / 删除 agent/logs 日志文件（list / get / del）\n\
             /cancel            停止当前会话正在运行的任务（路由层即时生效，不进入 LLM）\n\
             /help              显示本帮助\n\
             \n已知模型：glm-5.3、glm-5.3-flash"
        )
    }

    fn search_fallback_status(&self, control: &WebSearchControl) -> String {
        if !control.fallback_enabled() {
            return "关闭（只用主搜索源）".to_string();
        }
        "开启".to_string()
    }

    fn search_status(&self, control: &WebSearchControl) -> String {
        let statuses = control.statuses();
        let mut output = format!(
            "web_search 顺序与状态（fallback {}）：",
            self.search_fallback_status(control)
        );
        for (index, status) in statuses.iter().enumerate() {
            let enabled = if status.enabled {
                "enabled"
            } else {
                "disabled"
            };
            let primary = if status.primary { " / primary" } else { "" };
            output.push_str(&format!(
                "\n{}. {}  {} / {}{}",
                index + 1,
                status.id.as_str(),
                enabled,
                status.health.label(),
                primary
            ));
        }
        output.push_str("\n用法：/search <方式>；/search dis|en <方式>；/searchfallback on|off");
        output
    }
}

fn search_unknown_provider(input: &str) -> String {
    format!("未知搜索方式「{input}」。支持：zhipu / tavily / doubao-custom / doubao-global")
}

#[cfg(test)]
mod tests;
