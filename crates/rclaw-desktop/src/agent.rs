//! agent loop 装配与前端事件桥。
//!
//! 桌面版是一个「虚拟通道」（channel="desktop"）：inbound 由 Tauri command 写入
//! 总线，outbound 经 Tauri 事件推给 Webview。流式安全语义与 CLI 一致——SSE 片段
//! 在完成闸门通过前不外发，GUI 只展示 outbound 已有的状态消息（工作中 / 工具
//! 调用进度），不暴露未过闸门的原始片段。

use std::path::PathBuf;
use std::sync::Arc;

use serde::Serialize;
use tauri::{Emitter, Manager};
use tokio::sync::mpsc;

use rclaw_core::{BusMessage, ChannelCaps, RuntimeChannelInfo, ToolAccess};
use rclaw_runtime::{AgentLoop, CronService, Heartbeat};

#[cfg(not(all(target_os = "android", feature = "apk-host")))]
use crate::home;
use crate::state::{AppState, RunningAgent};

/// 桌面虚拟通道名（非 CHANNEL_CLI：保证最终回复走 outbound 而非 stdout）。
pub const CHANNEL_DESKTOP: &str = "desktop";

/// 前端事件载荷（`bus` 事件）。
#[derive(Debug, Clone, Serialize)]
pub struct BusPayload {
    pub channel: String,
    pub chat_id: String,
    pub content: String,
    /// 中间过程消息（工具轮 text / 思考流式增量）：前端据此不解除会话忙碌态、
    /// 思考块做流式合并。
    pub intermediate: bool,
}

/// 前端事件载荷（`agent` 生命周期事件）。
#[derive(Debug, Clone, Serialize)]
pub struct AgentEvent {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 装配并启动完整 agent loop；完成事件经 `agent` 事件通知前端。
///
/// 每进程只调用一次（启动时配置存在，或向导完成后）。
pub async fn start_agent(app: tauri::AppHandle, home: PathBuf) -> Result<(), String> {
    {
        let started = app
            .state::<AppState>()
            .agent
            .read()
            .expect("AppState.agent 锁未被毒化")
            .is_some();
        if started {
            return Ok(());
        }
    }

    let _ = app.emit(
        "agent",
        AgentEvent {
            state: "starting".into(),
            error: None,
        },
    );

    let result = start_agent_inner(&app, home).await;
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    if let Err(error) = &result {
        crate::android_boot_log(&format!("start_agent 失败: {error}"));
    }
    // 启动结果同步进 AppState：前端轮询 get_status 时兜底（事件可能在
    // 监听器就绪前发出而被错过，轮询是唯一可靠的恢复通道）。
    app.state::<AppState>()
        .set_start_error(result.as_ref().err().cloned());
    match result {
        Ok(()) => {
            let _ = app.emit(
                "agent",
                AgentEvent {
                    state: "ready".into(),
                    error: None,
                },
            );
            Ok(())
        }
        Err(e) => {
            tracing::error!("agent 启动失败: {e}");
            let _ = app.emit(
                "agent",
                AgentEvent {
                    state: "error".into(),
                    error: Some(e.clone()),
                },
            );
            Err(e)
        }
    }
}

#[cfg(not(all(target_os = "android", feature = "apk-host")))]
async fn start_agent_inner(app: &tauri::AppHandle, home: PathBuf) -> Result<(), String> {
    // 工具工作区外访问的弹窗确认钩子（进程级只装一次；完全访问模式下消息直接
    // 带 Unrestricted，不会走到该钩子）。
    install_permission_hook(app.clone());

    // 配置：从用户数据目录加载；agent 存储固定在数据目录下（与 CWD 解耦）。
    let mut cfg = rclaw_cli::config::Config::load_from_dir(&home)
        .map_err(|e| format!("加载配置失败: {e}"))?;
    cfg.agent.base_dir = home.join("agent").to_string_lossy().to_string();

    let layout =
        rclaw_cli::bootstrap::prepare_storage(&cfg).map_err(|e| format!("初始化存储失败: {e}"))?;
    // tracing 全局订阅者每进程只能设置一次；启动路径（配置存在）与向导路径互斥，
    // 恰好各调用一次。再次调用返回 Err 但不影响已初始化的订阅者。
    let _ = rclaw_cli::logging::init_tracing(&cfg.log, &layout.logs_dir);

    // 桌面虚拟通道能力：working_status / intermediate_output 让「工作中 / 工具调用
    // 进度」状态消息出站；media 打开媒体 marker 通路（附件经 media_message 注入）。
    let media_dir = layout.base.join("media");
    let channel_info = RuntimeChannelInfo {
        media_dir: Some(media_dir.to_string_lossy().replace('\\', "/")),
        channels: vec![ChannelCaps {
            name: CHANNEL_DESKTOP.into(),
            media: true,
            working_status: true,
            intermediate_output: true,
            reasoning_stream: true,
        }],
    };

    // 核心组件与 CLI 同一装配路径；/llmodel 持久化写回数据目录的 rclaw.toml。
    // CDP 浏览器（桌面）：拟人化 browser_* 工具面（协议与安卓共用，见
    // rclaw_tools::cdp_browser；CLI/Termux 不注册，无浏览器工具）。截图落工作区 browser/。
    // transport 经 commands::browser_transport_or_init 单源获取：装配重试时
    // 命令层（「打开浏览器」）与工具注册始终同一实例。
    let browser_transport =
        crate::commands::browser_transport_or_init(layout.base.join("browser"));
    let core = rclaw_cli::bootstrap::assemble_core_with(
        &cfg,
        &layout,
        &channel_info,
        false,
        home::config_path(&home),
        |tools| {
            rclaw_tools::cdp_browser::register(tools, browser_transport);
        },
    )
    .await
    .map_err(|e| {
        crate::android_boot_log(&format!("stage7 assemble_core 失败: {e}"));
        format!("装配核心组件失败: {e}")
    })?;

    // 应用用户禁用工具偏好（桌面工具面板开关的持久化）。
    for name in read_disabled_tools(&home) {
        core.tools.set_enabled(&name, false);
    }

    let (inbound_tx, inbound_rx) = mpsc::channel::<BusMessage>(64);
    let (outbound_tx, outbound_rx) = mpsc::channel::<BusMessage>(64);

    // Bot Mode（ADR-0006）：inbound 总线就绪后装配 BotRouter，回填给 message_agent 工具。
    let bot_router = Arc::new(rclaw_runtime::BotRouter::new(
        inbound_tx.clone(),
        core.bots.clone(),
    ));
    let _ = core.bot_router_slot.set(bot_router.clone());
    // 后台完成通知的批量唤醒（ZCode 移植 · Layer 4）：inbound 总线就绪后接线，
    // 与 CLI main.rs 同一装配点；不接线则桌面端父 turn 结束后无人驱动收集轮次。
    core.agent_runtime
        .background_wake()
        .attach(inbound_tx.clone());

    // 会话存储克隆一份给状态层（多会话列表 / 历史 / 归档），本体进 agent loop。
    let session_for_state = core.session.clone();
    // 工具注册表与布局克隆给状态层（工具面板 / 自进化面板）。
    let tools_for_state = core.tools.clone();
    let layout_for_state = layout.clone();
    // 上下文占用估算输入（状态面板）：move 进 AgentLoop 前克隆。
    let memory_for_state = core.memory.clone();
    let opts_for_state = core.opts.clone();
    let channels_for_state = channel_info.clone();
    let web_search_for_state = core.web_search_control.clone();
    let agent = AgentLoop::new(
        core.provider.clone(),
        core.review_provider.clone(),
        core.session,
        core.memory,
        layout.clone(),
        core.tools,
        core.opts,
        inbound_rx,
        outbound_tx,
        Some(core.slash),
        channel_info,
        core.bots.clone(),
        Some(bot_router.clone()),
    );
    // 删会话时清运行期状态用的句柄：必须在 agent 被 move 进任务前取走。
    let session_lifecycle = agent.session_lifecycle();
    // 统一分叉入口句柄：同样必须在 agent 被 move 进任务前取走。
    let fork_handle = agent.fork_handle();
    tauri::async_runtime::spawn(agent.run());

    // cron + heartbeat 后台服务（与 CLI 相同语义）。
    tauri::async_runtime::spawn(
        CronService::new(
            core.cron.clone(),
            inbound_tx.clone(),
            cfg.cron.check_interval_sec,
        )
        .run(),
    );
    tauri::async_runtime::spawn(
        Heartbeat::new(layout, inbound_tx.clone(), cfg.heartbeat.interval_sec).run(),
    );

    // outbound → Webview 事件桥。
    tauri::async_runtime::spawn(outbound_bridge(app.clone(), outbound_rx));

    let model = cfg
        .llm
        .providers
        .iter()
        .find(|p| p.name == cfg.llm.provider)
        .and_then(|p| p.model.clone());
    let llm_providers: Vec<String> = cfg.llm.providers.iter().map(|p| p.name.clone()).collect();
    let running = RunningAgent {
        inbound_tx,
        cron: core.cron,
        session: session_for_state,
        session_lifecycle,
        fork: fork_handle,
        agent_runtime: core.agent_runtime,
        tools: tools_for_state,
        layout: layout_for_state,
        // 上下文占用估算输入：与 agent loop 同源的克隆（move 前取走）。
        memory: memory_for_state,
        opts: opts_for_state,
        channels: channels_for_state,
        web_search: web_search_for_state,
        provider_name: cfg.llm.provider.clone(),
        model,
        bots: core.bots,
        bot_router,
        llm_providers,
    };
    *app.state::<AppState>()
        .agent
        .write()
        .expect("AppState.agent 锁未被毒化") = Some(running);
    tracing::info!("RClaw 桌面版 agent 已就绪");
    Ok(())
}

/// outbound 总线消费端：把每条回复 / 状态消息推给 Webview。
async fn outbound_bridge(app: tauri::AppHandle, mut rx: mpsc::Receiver<BusMessage>) {
    while let Some(msg) = rx.recv().await {
        if msg.channel != CHANNEL_DESKTOP {
            continue;
        }
        // Bot Mode：群成员会话（grp-…）是编排内部会话，不推给前端。
        if msg.chat_id.starts_with("grp-") {
            continue;
        }
        let payload = BusPayload {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            content: msg.content.clone(),
            intermediate: msg.is_intermediate(),
        };
        if let Err(e) = app.emit("bus", payload) {
            tracing::warn!("bus 事件推送失败: {e}");
        }
    }
}

/// 注册工具工作区外路径确认钩子：默认权限下 shell / 文件工具碰到 agent 工作区
/// 之外的路径时，经 [`permissions::ask`] 弹窗询问用户；允许才放行（每次授权按
/// （操作, 物理落点）缓存在工具层）。完全访问模式的消息直接带 `Unrestricted`，不经过此钩子。
fn install_permission_hook(app: tauri::AppHandle) {
    let hook: rclaw_tools::access::PathConfirmHook = Arc::new(move |request| {
        Box::pin(crate::permissions::ask(app.clone(), request))
    });
    if !rclaw_tools::access::set_path_confirm_hook(hook) {
        tracing::debug!("工具权限确认钩子已注册过，跳过");
    }
}

/// 向 inbound 总线投递一条用户消息 / 斜杠命令（chat_id 决定所属会话）。
pub async fn send(app: &tauri::AppHandle, chat_id: String, content: String) -> Result<(), String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let sender = app
        .state::<AppState>()
        .inbound()
        .map_err(|e| format!("send: {e}"))?;
    let mut msg = BusMessage::new(CHANNEL_DESKTOP.to_string(), chat_id.clone(), content);
    // 完全访问模式：工具沙箱对桌面通道消息放开（任意路径免确认）。
    if read_permission_mode(&app.state::<AppState>().home) == PERMISSION_MODE_FULL {
        msg = msg.with_tool_access(ToolAccess::Unrestricted);
    }
    // 先置忙碌再投递：/cancel 等快回合可能在 send 返回前就跑完全程并发出
    // 收尾 outbound（运行时任务在另一线程），事后置位会卡忙碌至自愈超时。
    crate::android_status::turn_started(&chat_id);
    if let Err(e) = sender.send(msg).await {
        crate::android_status::turn_aborted(&chat_id);
        return Err(format!("投递消息失败: {e}"));
    }
    Ok(())
}

/// 会话 id 白名单：字母 / 数字 / `-` / `_`，1..=64 字符。
/// chat_id 会进入 `/export` 的 percent-encoded 导出文件名，也是 state.sqlite 的会话键，
/// 收紧字符集避免路径注入与键歧义。
pub fn is_valid_chat_id(chat_id: &str) -> bool {
    !chat_id.is_empty()
        && chat_id.len() <= 64
        && chat_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// ---------------------------------------------------------------------------
// 界面偏好（`home/desktop_prefs.json`）：工具禁能开关、标题栏循环标语等
// 与 rclaw.toml 无关的桌面 UI 偏好。读-改-写保真其它键。
// ---------------------------------------------------------------------------

/// 偏好文件路径。
///
/// 放在 `home` 根下而非 `home/agent/`：`agent/` 是模型文件工具的工作区根（工作区内写入
/// 免确认，见 rclaw-tools access.rs），而本文件承载 `permission_mode` / `disabled_tools`——
/// 沙箱策略若存放在被它约束的目录里，模型（被网页/技能内容提示注入即可）写一句
/// `{"permission_mode":"full"}` 就能单方面关闭「工作区外需确认」的沙箱。
pub fn prefs_path(home: &std::path::Path) -> PathBuf {
    home.join("desktop_prefs.json")
}

/// 旧版路径 `agent/desktop_prefs.json` 里允许带入新文件的键：只迁界面外观类偏好。
///
/// `permission_mode` / `disabled_tools` **绝不**从旧路径带入：旧路径在模型可写的工作区里，
/// 原样搬运等于把「写一句 `{"permission_mode":"full"}` 关沙箱」的口子原封不动留在迁移这一步
/// （模型只要抢在新文件生成前写出旧文件即可）。这两项按默认值重新开始，用户在「权限」页重设。
const LEGACY_PREFS_MIGRATED_KEYS: [&str; 4] = ["theme", "slogans", "slogan_interval_secs", "chat_titles"];

/// 旧版路径 `agent/desktop_prefs.json` 一次性迁移到新路径（best-effort）。
///
/// 新文件已存在则不动。否则只拷贝 [`LEGACY_PREFS_MIGRATED_KEYS`]，用 `create_new` 落成新文件
/// （与并发的 [`update_prefs`] 写入相撞时让位，不覆盖刚写好的真实偏好），然后删除旧文件；
/// 新文件一经落成旧路径便不再被读取，之后模型在工作区里再写出同名文件也不会被当作偏好。
fn migrate_legacy_prefs(home: &std::path::Path) {
    use std::io::Write;

    let current = prefs_path(home);
    if current.exists() {
        return;
    }
    let legacy = home.join("agent").join("desktop_prefs.json");
    if !legacy.is_file() {
        return;
    }
    let legacy_value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&legacy).unwrap_or_default())
            .unwrap_or(serde_json::json!({}));
    let mut migrated = serde_json::Map::new();
    if let Some(object) = legacy_value.as_object() {
        for key in LEGACY_PREFS_MIGRATED_KEYS {
            if let Some(value) = object.get(key) {
                migrated.insert(key.to_string(), value.clone());
            }
        }
    }
    let materialized = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&current)
        .and_then(|mut file| {
            file.write_all(serde_json::Value::Object(migrated).to_string().as_bytes())
        });
    match materialized {
        Ok(()) => {}
        // 并发写入方已落成新文件：以它为准。
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => {
            // 新文件没建出来 → 旧文件保留、下次读取重试；建出来但没写完 → 下次按已迁移处理，
            // 只丢外观偏好（沙箱键本就不迁移），不值得为此再引入覆盖式原子写与并发写入方相撞。
            tracing::warn!(error = %error, "桌面偏好迁移写入失败（仅影响主题 / 标语 / 会话标题）");
            return;
        }
    }
    if let Err(error) = std::fs::remove_file(&legacy) {
        tracing::warn!(error = %error, "删除旧版桌面偏好文件失败");
    }
}

/// 读取整份偏好（文件缺失 / 损坏 / 非对象按空对象处理）。
fn read_prefs(home: &std::path::Path) -> serde_json::Value {
    migrate_legacy_prefs(home);
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(prefs_path(home)).unwrap_or_default())
            .unwrap_or(serde_json::json!({}));
    // 合法但非对象的 JSON（`[]`、`"x"`、`1`）：下游 `value["key"] = ...` 的 IndexMut 对非对象
    // 直接 panic，所有偏好写入命令会在 tauri 任务里炸掉、IPC 永不 resolve。按空对象重建。
    if value.is_object() {
        value
    } else {
        serde_json::json!({})
    }
}

/// 写回整份偏好（调用方负责先读后改，避免覆盖其它键）。
/// 原子写：`std::fs::write` 先截断再写，安卓看门狗 / OOM 随时可能杀进程，落在窗口内
/// 整份偏好（含工具禁用、权限模式）会被清零。
fn write_prefs(home: &std::path::Path, value: &serde_json::Value) -> Result<(), String> {
    rclaw_store::atomic_write(prefs_path(home), value.to_string().as_bytes())
        .map_err(|e| format!("写入偏好失败: {e}"))
}

/// 读-改-写串行化：Tauri 命令并发执行，两条命令各自 read → 改一个键 → write 会互相
/// 覆盖对方刚写入的键（快速连点两个工具开关只剩一个生效）。所有写入方都经此函数。
static PREFS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn update_prefs(
    home: &std::path::Path,
    mutate: impl FnOnce(&mut serde_json::Value),
) -> Result<(), String> {
    let _guard = PREFS_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut value = read_prefs(home);
    mutate(&mut value);
    write_prefs(home, &value)
}

/// 读取被禁用的工具名列表（文件缺失 / 损坏按空处理）。
pub fn read_disabled_tools(home: &std::path::Path) -> Vec<String> {
    read_prefs(home)
        .get("disabled_tools")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// 写回被禁用的工具名列表（保留偏好文件中的其它键）。
pub fn write_disabled_tools(home: &std::path::Path, disabled: &[String]) -> Result<(), String> {
    update_prefs(home, |value| {
        value["disabled_tools"] = serde_json::json!(disabled);
    })
}

/// 读取自定义会话名映射（chat_id -> 标题；文件缺失 / 损坏按空处理）。
pub fn read_chat_titles(home: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    read_prefs(home)
        .get("chat_titles")
        .and_then(|t| t.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(id, title)| title.as_str().map(|t| (id.clone(), t.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// 写入 / 清除自定义会话名（标题去空白后为空 = 删除该键，恢复自动派生标题；
/// 读-改-写保留偏好文件中的其它键）。归档 / 恢复只改会话标记、chat_id 不变，
/// 自定义标题天然跟随会话。
pub fn write_chat_title(home: &std::path::Path, chat_id: &str, title: &str) -> Result<(), String> {
    let title = title.trim();
    update_prefs(home, |value| {
        if title.is_empty() {
            if let Some(map) = value.get_mut("chat_titles").and_then(|t| t.as_object_mut()) {
                map.remove(chat_id);
            }
        } else {
            if value
                .get("chat_titles")
                .and_then(|t| t.as_object())
                .is_none()
            {
                value["chat_titles"] = serde_json::json!({});
            }
            value["chat_titles"][chat_id] = serde_json::json!(title);
        }
    })
}

/// 标题栏标语默认文案（`slogans` 未配置 / 被清空时使用）。
pub const DEFAULT_SLOGANS: [&str; 4] = [
    "Powered by AI",
    "代码很单纯，复杂的是人",
    "质疑Rust，了解Rust，学习Rust，爱上Rust",
    "勿催，如有必要，我会胡吊说",
];

const SLOGANS_MAX_ITEMS: usize = 30;
const SLOGAN_MAX_CHARS: usize = 120;

/// 轮播周期缺省值与合法区间（秒）。
pub const DEFAULT_SLOGAN_INTERVAL_SECS: u64 = 5;
const SLOGAN_INTERVAL_MIN_SECS: u64 = 1;
const SLOGAN_INTERVAL_MAX_SECS: u64 = 3600;

/// 标题栏标语轮播偏好（界面级，存 desktop_prefs.json，不进 rclaw.toml）。
#[derive(Debug, Clone, PartialEq)]
pub struct SloganPrefs {
    pub slogans: Vec<String>,
    pub interval_secs: u64,
}

/// 读取标语轮播偏好；未配置或为空时标语回退默认文案、周期回退默认值。
pub fn read_slogan_prefs(home: &std::path::Path) -> SloganPrefs {
    let prefs = read_prefs(home);
    let stored: Vec<String> = prefs
        .get("slogans")
        .and_then(|s| s.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let slogans = if stored.is_empty() {
        DEFAULT_SLOGANS.iter().map(|s| s.to_string()).collect()
    } else {
        stored
    };
    let interval_secs = prefs
        .get("slogan_interval_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_SLOGAN_INTERVAL_SECS)
        .clamp(SLOGAN_INTERVAL_MIN_SECS, SLOGAN_INTERVAL_MAX_SECS);
    SloganPrefs {
        slogans,
        interval_secs,
    }
}

/// 写回标语轮播偏好（标语 trim 去空行、限条数与单条长度；周期限区间；
/// 读-改-写保留偏好文件中的其它键）。
pub fn write_slogan_prefs(
    home: &std::path::Path,
    slogans: &[String],
    interval_secs: u64,
) -> Result<(), String> {
    let cleaned: Vec<String> = slogans
        .iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .take(SLOGANS_MAX_ITEMS)
        .collect();
    if let Some(too_long) = cleaned
        .iter()
        .find(|s| s.chars().count() > SLOGAN_MAX_CHARS)
    {
        return Err(format!(
            "单条标语过长（上限 {SLOGAN_MAX_CHARS} 字符）: {}",
            too_long.chars().take(30).collect::<String>()
        ));
    }
    if !(SLOGAN_INTERVAL_MIN_SECS..=SLOGAN_INTERVAL_MAX_SECS).contains(&interval_secs) {
        return Err(format!(
            "轮播周期须在 {SLOGAN_INTERVAL_MIN_SECS}-{SLOGAN_INTERVAL_MAX_SECS} 秒之间: {interval_secs}"
        ));
    }
    update_prefs(home, |value| {
        value["slogans"] = serde_json::json!(cleaned);
        value["slogan_interval_secs"] = serde_json::json!(interval_secs);
    })
}

/// 读取界面主题；未配置 / 非法值回退 `dark`。
/// 界面主题全集（与 UI 的 THEMES 一致；base 分浅深两系由前端决定）。
pub const THEMES: [&str; 6] = ["dark", "light", "starry", "forest", "sakura", "cream"];

pub fn read_theme(home: &std::path::Path) -> String {
    read_prefs(home)
        .get("theme")
        .and_then(|t| t.as_str())
        .filter(|t| THEMES.contains(t))
        .unwrap_or("dark")
        .to_string()
}

/// 写回界面主题（仅接受已知主题；读-改-写保留偏好文件中的其它键）。
pub fn write_theme(home: &std::path::Path, theme: &str) -> Result<(), String> {
    if !THEMES.contains(&theme) {
        return Err(format!("未知主题: {theme}（支持 {}）", THEMES.join(" / ")));
    }
    update_prefs(home, |value| {
        value["theme"] = serde_json::json!(theme);
    })
}

// ---------------------------------------------------------------------------
// 工具权限模式（desktop_prefs.json 的 permission_mode）：
//   default = 默认权限——文件/shell 工具仅限 agent 工作区，之外需用户弹窗授权；
//   full    = 完全访问——任意路径免确认（消息带 ToolAccess::Unrestricted）。
// ---------------------------------------------------------------------------

/// 默认权限模式值。
pub const PERMISSION_MODE_DEFAULT: &str = "default";
/// 完全访问模式值。
pub const PERMISSION_MODE_FULL: &str = "full";

/// 读取权限模式；未配置 / 非法值回退默认权限。
pub fn read_permission_mode(home: &std::path::Path) -> String {
    read_prefs(home)
        .get("permission_mode")
        .and_then(|t| t.as_str())
        .filter(|t| *t == PERMISSION_MODE_DEFAULT || *t == PERMISSION_MODE_FULL)
        .unwrap_or(PERMISSION_MODE_DEFAULT)
        .to_string()
}

/// 写回权限模式（仅接受 default / full；读-改-写保留偏好文件中的其它键）。
pub fn write_permission_mode(home: &std::path::Path, mode: &str) -> Result<(), String> {
    if mode != PERMISSION_MODE_DEFAULT && mode != PERMISSION_MODE_FULL {
        return Err(format!(
            "未知权限模式: {mode}（仅支持 {PERMISSION_MODE_DEFAULT} / {PERMISSION_MODE_FULL}）"
        ));
    }
    update_prefs(home, |value| {
        value["permission_mode"] = serde_json::json!(mode);
    })
}


// ---------------------------------------------------------------------------
// APK 宿主（android + apk-host）：GUI 与前台服务共享单运行时。
//
// 装配核心 `assemble_apk_agent` 无 tauri 依赖：服务（nativeStart，无 GUI）与
// GUI（start_agent_inner）都经它装配，RunningAgent 进进程级全局槽；GUI 启动时
// 采用槽内既有运行时（含 napcat/weixin 通道任务），outbound 桥延迟到 GUI 就绪
// 才接（无头期的 desktop 通道消息不存在——GUI 未开就没有 desktop 会话）。
// ---------------------------------------------------------------------------

#[cfg(all(target_os = "android", feature = "apk-host"))]
async fn start_agent_inner(app: &tauri::AppHandle, _home: PathBuf) -> Result<(), String> {
    install_permission_hook(app.clone());
    let context = rclaw_android::host_context()
        .ok_or("Android 宿主上下文未初始化（nativeInit 未调用）")?;
    let mut slot = crate::android_host::apk_runtime_slot().lock().await;
    let pending_outbound = match slot.as_mut() {
        Some(runtime) => runtime.pending_outbound.take(),
        None => {
            // GUI 可能早于服务装配：启动 JSON 只是 nativeInit 的最小上下文，
            // toml 由 resolve_android_config 从 filesDir/rclaw.toml 回读。
            let launch = rclaw_android::AndroidConfig::minimal(
                &context.files_dir.to_string_lossy(),
            );
            let config_json =
                serde_json::to_string(&launch).map_err(|e| e.to_string())?;
            let (running, _gui_attached) = match assemble_apk_agent(&config_json, Some(app)).await
            {
                Ok(result) => result,
                Err(error) => {
                    #[cfg(all(target_os = "android", feature = "apk-host"))]
                    crate::android_boot_log(&format!("GUI 装配失败: {error}"));
                    return Err(error);
                }
            };
            *slot = Some(crate::android_host::ApkRuntime {
                agent: running,
                pending_outbound: None,
            });
            // GUI 在场装配时桥已直接接上。
            None
        }
    };
    let runtime = slot.as_ref().expect("槽内运行时已就绪");
    // 无头期装配的运行时：GUI 现在接管 desktop 通道 outbound。
    if let Some(rx) = pending_outbound {
        tauri::async_runtime::spawn(outbound_bridge(app.clone(), rx));
    }
    *app.state::<AppState>()
        .agent
        .write()
        .expect("AppState.agent 锁未被毒化") = Some(clone_running_agent(&runtime.agent));
    tracing::info!("APK agent 已就绪（GUI 采用共享运行时）");
    Ok(())
}

/// RunningAgent 的浅克隆（内部全部是 Arc / 克隆型句柄）。
#[cfg(all(target_os = "android", feature = "apk-host"))]
fn clone_running_agent(running: &RunningAgent) -> RunningAgent {
    RunningAgent {
        inbound_tx: running.inbound_tx.clone(),
        cron: running.cron.clone(),
        session: running.session.clone(),
        session_lifecycle: running.session_lifecycle.clone(),
        fork: running.fork.clone(),
        agent_runtime: running.agent_runtime.clone(),
        tools: running.tools.clone(),
        layout: running.layout.clone(),
        memory: running.memory.clone(),
        opts: running.opts.clone(),
        channels: running.channels.clone(),
        web_search: running.web_search.clone(),
        provider_name: running.provider_name.clone(),
        model: running.model.clone(),
        bots: running.bots.clone(),
        bot_router: running.bot_router.clone(),
        llm_providers: running.llm_providers.clone(),
    }
}

/// APK 装配：Android 配置解析 + host_glue 环境/闸门/通道/gbrain + 共用
/// assemble_core_with（gbrain 进程内注册；shell 保留——沙箱内 toybox sh/tar
/// 真机可用，见 HANDOFF §2.11）+ AgentLoop/Cron/Heartbeat。
#[cfg(all(target_os = "android", feature = "apk-host"))]
pub(crate) async fn assemble_apk_agent(
    config_json: &str,
    app: Option<&tauri::AppHandle>,
) -> Result<(RunningAgent, Option<mpsc::Receiver<BusMessage>>), String> {
    use rclaw_channels::Channel as _;
    // 装配在途即判活（nativeStart 秒回，Java 看门狗 60s 后首查；装配期间
    // 不判死，见 rclaw-android::begin_assembly）。守卫覆盖所有提前返回。
    rclaw_android::begin_assembly();
    let _assembly_guard = {
        struct Guard;
        impl Drop for Guard {
            fn drop(&mut self) {
                rclaw_android::end_assembly();
            }
        }
        Guard
    };
    let launch: rclaw_android::AndroidConfig = serde_json::from_str(config_json)
        .map_err(|e| format!("解析 Android 配置 JSON 失败: {e}"))?;
    let mut resolved = rclaw_android::resolve_android_config(launch)
        .map_err(|e| format!("Android 配置解析失败: {e}"))?;
    let home = resolved.files_dir.clone();

    let prepared = rclaw_android::prepare_environment(&mut resolved)
        .map_err(|e| format!("APK 环境准备失败: {e}"))?;
    let layout = prepared.layout;
    let _ = rclaw_cli::logging::init_tracing(&resolved.config.log, &layout.logs_dir);
    rclaw_android::install_skill_gate(&layout, &resolved)
        .map_err(|e| format!("技能审批闸门安装失败: {e}"))?;

    // 通道能力 = 桌面虚拟通道 + 外部通道（napcat/weixin）。
    let channels = rclaw_android::build_channels(&resolved, &prepared.media_dir);
    let mut caps = vec![ChannelCaps {
        name: CHANNEL_DESKTOP.into(),
        media: true,
        working_status: true,
        intermediate_output: true,
        reasoning_stream: true,
    }];
    caps.extend(channels.info.channels.iter().cloned());
    let channel_info = RuntimeChannelInfo {
        media_dir: channels.info.media_dir.clone(),
        channels: caps,
    };

    let (gbrain, gbrain_call_timeout) = rclaw_android::prepare_gbrain(&mut resolved)
        .map_err(|e| format!("gbrain 准备失败: {e}"))?;
    let gbrain_health = gbrain.as_ref().map(|tools| tools.health_flag());
    let cfg = resolved.config.clone();

    let core = rclaw_cli::bootstrap::assemble_core_with(
        &cfg,
        &layout,
        &channel_info,
        false,
        home.join("rclaw.toml"),
        |tools| {
            if let Some(gbrain) = gbrain.as_ref() {
                let added = gbrain.extend_registry_with_timeout(tools, gbrain_call_timeout);
                tracing::info!(added, "APK host 已注册进程内 gbrain 工具");
            }
            // 注册拟人化 CDP browser_* 工具面（AndroidTransport 长驻调试
            // WebView；桌面同款协议，见 rclaw_tools::cdp_browser）。
            rclaw_android::browser::register(tools);
        },
    )
    .await
    .map_err(|e| {
        crate::android_boot_log(&format!("stage7 assemble_core 失败: {e}"));
        format!("装配核心组件失败: {e}")
    })?;

    // 界面偏好（工具禁用）与桌面同一文件语义（filesDir/agent/desktop_prefs.json）。
    for name in read_disabled_tools(&home) {
        core.tools.set_enabled(&name, false);
    }

    // 通知空闲态：安装估算输入并启动 30s 刷新（GUI 与无头服务两路都在这里，
    // 幂等）。放在禁用工具回放之后，估算看到的工具面与实际一致。
    crate::android_status::install_idle_inputs(
        core.session.clone(),
        core.memory.clone(),
        layout.clone(),
        core.tools.clone(),
        core.opts.clone(),
        channel_info.clone(),
        core.web_search_control.clone(),
    );

    let (inbound_tx, inbound_rx) = mpsc::channel::<BusMessage>(64);
    let (outbound_tx, outbound_rx) = mpsc::channel::<BusMessage>(64);

    // 状态抽头：AgentLoop 的全部出站（desktop/微信/QQ）先过 android_status
    // （忙碌文案 / 完成提醒 / last_chat——此前只有 desktop 桥喂状态机，无头
    // 服务模式下的外部通道 turn 完全不产生通知状态），再转发进 dispatcher。
    let (status_tx, mut status_rx) = mpsc::channel::<BusMessage>(64);
    let dispatcher_tx = outbound_tx.clone();
    tokio::spawn(async move {
        while let Some(msg) = status_rx.recv().await {
            // grp- 内部编排会话不参与状态机（与 outbound_bridge 同过滤）。
            if !msg.chat_id.starts_with("grp-") {
                crate::android_status::on_outbound(
                    &msg.chat_id,
                    &msg.content,
                    msg.is_intermediate(),
                );
            }
            if dispatcher_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // 外部通道接线（无头 QQ/微信：GUI 未打开时也在跑）；desktop 虚拟通道
    // 单独路由——GUI 在场立即接 outbound 桥，无头期把接收端留给 GUI 采用时再接。
    let mut dispatcher = rclaw_channels::OutboundDispatcher::new(outbound_rx);
    let (desktop_tx, desktop_rx) = mpsc::channel::<BusMessage>(64);
    dispatcher.route_to(CHANNEL_DESKTOP, desktop_tx);
    let pending_outbound = match app {
        Some(app) => {
            tauri::async_runtime::spawn(outbound_bridge(app.clone(), desktop_rx));
            None
        }
        None => Some(desktop_rx),
    };
    if let Some(weixin) = channels.weixin {
        let (tx, rx) = mpsc::channel::<BusMessage>(16);
        dispatcher.route_to(rclaw_core::CHANNEL_WEIXIN, tx);
        tokio::spawn(weixin.run(inbound_tx.clone(), rx));
    }
    if let Some(napcat) = channels.napcat {
        let (tx, rx) = mpsc::channel::<BusMessage>(16);
        dispatcher.route_to(rclaw_core::CHANNEL_NAPCAT, tx);
        tokio::spawn(napcat.run(inbound_tx.clone(), rx));
    }
    tokio::spawn(dispatcher.run());

    let bot_router = Arc::new(rclaw_runtime::BotRouter::new(
        inbound_tx.clone(),
        core.bots.clone(),
    ));
    let _ = core.bot_router_slot.set(bot_router.clone());
    // 后台完成通知的批量唤醒（Layer 4）：Android 路径同样接线。
    core.agent_runtime
        .background_wake()
        .attach(inbound_tx.clone());

    let session_for_state = core.session.clone();
    let tools_for_state = core.tools.clone();
    let layout_for_state = layout.clone();
    let memory_for_state = core.memory.clone();
    let opts_for_state = core.opts.clone();
    let channels_for_state = channel_info.clone();
    let web_search_for_state = core.web_search_control.clone();
    let agent = AgentLoop::new(
        core.provider.clone(),
        core.review_provider.clone(),
        core.session,
        core.memory,
        layout.clone(),
        core.tools,
        core.opts,
        inbound_rx,
        status_tx,
        Some(core.slash),
        channel_info,
        core.bots.clone(),
        Some(bot_router.clone()),
    );
    let session_lifecycle = agent.session_lifecycle();
    let fork_handle = agent.fork_handle();
    let agent_task = tokio::spawn(agent.run());
    tokio::spawn(
        CronService::new(
            core.cron.clone(),
            inbound_tx.clone(),
            cfg.cron.check_interval_sec,
        )
        .run(),
    );
    tokio::spawn(
        Heartbeat::new(layout, inbound_tx.clone(), cfg.heartbeat.interval_sec).run(),
    );

    // 喂 watchdog 的健康信号：装配成功置位 + 30s 心跳（v0.2.0 服务
    // checkNativeRuntime 每 60s 查 is_healthy；不喂则进程被周期性杀掉重启）。
    // 心跳绑定 agent loop 存活：loop 退出（inbound 全部关闭）或 panic 后停止
    // 喂心跳，150s 内看门狗判死、杀进程由闹钟按退避重启——此前心跳只证明
    // tokio runtime 活着，agent 死了也一直「健康」。
    // 注意：gbrain 失活不再联动进程级健康信号——gbrain 工具调用超时只降级
    // 该工具（调用返回 unhealthy 错误 + 下方观察循环停机告警）；若在这里把
    // V2 标志翻 false，看门狗 60s 内杀进程，表现为「发消息后应用闪退」。
    rclaw_android::set_runtime_healthy(true);
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if agent_task.is_finished() {
                tracing::error!("Agent loop 已退出，停止喂看门狗心跳（等待进程级恢复）");
                crate::android_boot_log("agent loop 已退出，心跳停喂");
                break;
            }
            rclaw_android::mark_v2_heartbeat();
        }
    });

    // gbrain 健康观察（失活告警并停机；进程级 watchdog 负责恢复）。
    if let Some(health) = gbrain_health {
        let gbrain = gbrain.expect("health flag 与句柄同源");
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                if !health.load(std::sync::atomic::Ordering::Relaxed) {
                    tracing::error!("进程内 gbrain 已失去健康状态");
                    gbrain.shutdown();
                    break;
                }
            }
        });
    }

    let model = cfg
        .llm
        .providers
        .iter()
        .find(|p| p.name == cfg.llm.provider)
        .and_then(|p| p.model.clone());
    let llm_providers: Vec<String> = cfg.llm.providers.iter().map(|p| p.name.clone()).collect();
    Ok((
        RunningAgent {
            inbound_tx,
            cron: core.cron,
            session: session_for_state,
            session_lifecycle,
            fork: fork_handle,
            agent_runtime: core.agent_runtime,
            tools: tools_for_state,
            layout: layout_for_state,
            memory: memory_for_state,
            opts: opts_for_state,
            channels: channels_for_state,
            web_search: web_search_for_state,
            provider_name: cfg.llm.provider.clone(),
            model,
            bots: core.bots,
            bot_router,
            llm_providers,
        },
        pending_outbound,
    ))
}

#[cfg(test)]
mod prefs_tests {
    use super::*;

    #[test]
    fn slogans_default_when_missing_then_roundtrip_preserves_disabled_tools() {
        let dir = tempfile::tempdir().unwrap();
        // 未配置 → 默认文案 + 默认周期。
        let prefs = read_slogan_prefs(dir.path());
        assert_eq!(prefs.slogans, DEFAULT_SLOGANS.to_vec());
        assert_eq!(prefs.interval_secs, DEFAULT_SLOGAN_INTERVAL_SECS);

        // 写入标语后再写工具禁能，两键必须共存（读-改-写回归点）。
        write_slogan_prefs(dir.path(), &["a".into(), "b".into()], 3).unwrap();
        write_disabled_tools(dir.path(), &["shell".into()]).unwrap();
        assert_eq!(read_disabled_tools(dir.path()), vec!["shell".to_string()]);
        let prefs = read_slogan_prefs(dir.path());
        assert_eq!(prefs.slogans, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(prefs.interval_secs, 3);

        // 反向：再写标语不丢 disabled_tools 与周期。
        write_slogan_prefs(dir.path(), &["c".into()], 3).unwrap();
        assert_eq!(read_disabled_tools(dir.path()), vec!["shell".to_string()]);
        let prefs = read_slogan_prefs(dir.path());
        assert_eq!(prefs.slogans, vec!["c".to_string()]);
        assert_eq!(prefs.interval_secs, 3);
    }

    #[test]
    fn chat_titles_roundtrip_reset_and_coexist_with_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        // 未配置 → 空映射。
        assert!(read_chat_titles(dir.path()).is_empty());

        // 写入两个标题后再写工具禁能，两键必须共存（读-改-写回归点）。
        write_chat_title(dir.path(), "main", "自定义标题").unwrap();
        write_chat_title(dir.path(), "c-1", "研究").unwrap();
        write_disabled_tools(dir.path(), &["shell".into()]).unwrap();
        assert_eq!(read_disabled_tools(dir.path()), vec!["shell".to_string()]);
        let titles = read_chat_titles(dir.path());
        assert_eq!(titles.get("main").map(String::as_str), Some("自定义标题"));
        assert_eq!(titles.get("c-1").map(String::as_str), Some("研究"));

        // 标题 trim 落盘；空 / 纯空白标题 = 删除该键（恢复自动派生标题）。
        write_chat_title(dir.path(), "c-2", "  空格标题  ").unwrap();
        assert_eq!(
            read_chat_titles(dir.path()).get("c-2").map(String::as_str),
            Some("空格标题")
        );
        write_chat_title(dir.path(), "main", "   ").unwrap();
        let titles = read_chat_titles(dir.path());
        assert!(!titles.contains_key("main"));
        assert_eq!(titles.get("c-1").map(String::as_str), Some("研究"));
        assert_eq!(titles.get("c-2").map(String::as_str), Some("空格标题"));
    }

    #[test]
    fn slogan_prefs_clean_clamps_and_rejects_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        // 超长标语被拒，文件未写入。
        write_slogan_prefs(
            dir.path(),
            &["  hi  ".into(), String::new(), "x".repeat(200)],
            5,
        )
        .unwrap_err();
        assert_eq!(
            read_slogan_prefs(dir.path()),
            SloganPrefs {
                slogans: DEFAULT_SLOGANS.iter().map(|s| s.to_string()).collect(),
                interval_secs: DEFAULT_SLOGAN_INTERVAL_SECS,
            }
        );
        // 超界周期被拒。
        assert!(write_slogan_prefs(dir.path(), &["a".into()], 0).is_err());
        assert!(write_slogan_prefs(dir.path(), &["a".into()], 3601).is_err());

        // trim 去空行照常；周期落界内照常。
        write_slogan_prefs(dir.path(), &["  hi  ".into(), String::new()], 2).unwrap();
        let prefs = read_slogan_prefs(dir.path());
        assert_eq!(prefs.slogans, vec!["hi".to_string()]);
        assert_eq!(prefs.interval_secs, 2);

        // 清空标语 → 读侧回退默认文案（周期保留用户值）。
        write_slogan_prefs(dir.path(), &[], 9).unwrap();
        let prefs = read_slogan_prefs(dir.path());
        assert_eq!(prefs.slogans, DEFAULT_SLOGANS.to_vec());
        assert_eq!(prefs.interval_secs, 9);

        // 读取侧钳制：手工塞入超界值按边界处理。
        let mut value = read_prefs(dir.path());
        value["slogan_interval_secs"] = serde_json::json!(99_999);
        write_prefs(dir.path(), &value).unwrap();
        assert_eq!(
            read_slogan_prefs(dir.path()).interval_secs,
            SLOGAN_INTERVAL_MAX_SECS
        );
    }

    #[test]
    fn theme_defaults_roundtrip_and_coexists_with_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        // 未配置 / 非法值 → dark。
        assert_eq!(read_theme(dir.path()), "dark");
        let mut value = read_prefs(dir.path());
        value["theme"] = serde_json::json!("banana");
        write_prefs(dir.path(), &value).unwrap();
        assert_eq!(read_theme(dir.path()), "dark");

        // roundtrip；与既有键（标语、工具禁能）共存。
        write_slogan_prefs(dir.path(), &["a".into()], 3).unwrap();
        write_disabled_tools(dir.path(), &["shell".into()]).unwrap();
        write_theme(dir.path(), "light").unwrap();
        assert_eq!(read_theme(dir.path()), "light");
        assert_eq!(read_disabled_tools(dir.path()), vec!["shell".to_string()]);
        assert_eq!(read_slogan_prefs(dir.path()).slogans, vec!["a".to_string()]);

        // 非法主题被拒，文件未写入（light 保持）。
        assert!(write_theme(dir.path(), "solarized").is_err());
        assert_eq!(read_theme(dir.path()), "light");
        // 切回 dark 照常。
        write_theme(dir.path(), "dark").unwrap();
        assert_eq!(read_theme(dir.path()), "dark");
    }

    #[test]
    fn legacy_prefs_migration_drops_sandbox_keys_and_is_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let legacy = dir.path().join("agent").join("desktop_prefs.json");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        // 模型在工作区里写出的「旧偏好」：带沙箱键 + 外观键。
        std::fs::write(
            &legacy,
            r#"{"permission_mode":"full","disabled_tools":["shell"],"theme":"light","chat_titles":{"main":"t"}}"#,
        )
        .unwrap();

        // 外观键迁入；沙箱键回到默认；旧文件被删除、新文件落成。
        assert_eq!(read_theme(dir.path()), "light");
        assert_eq!(
            read_chat_titles(dir.path()).get("main").map(String::as_str),
            Some("t")
        );
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_DEFAULT);
        assert!(read_disabled_tools(dir.path()).is_empty());
        assert!(!legacy.exists());
        assert!(prefs_path(dir.path()).is_file());

        // 新文件已存在后，工作区里再出现同名文件不再被读取。
        std::fs::write(&legacy, r#"{"permission_mode":"full","theme":"dark"}"#).unwrap();
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_DEFAULT);
        assert_eq!(read_theme(dir.path()), "light");
        assert!(legacy.exists());
    }

    #[test]
    fn permission_mode_defaults_roundtrip_and_coexists() {
        let dir = tempfile::tempdir().unwrap();
        // 未配置 / 非法值 → 默认权限。
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_DEFAULT);
        let mut value = read_prefs(dir.path());
        value["permission_mode"] = serde_json::json!("yolo");
        write_prefs(dir.path(), &value).unwrap();
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_DEFAULT);

        // roundtrip；与既有键共存。
        write_disabled_tools(dir.path(), &["shell".into()]).unwrap();
        write_permission_mode(dir.path(), PERMISSION_MODE_FULL).unwrap();
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_FULL);
        assert_eq!(read_disabled_tools(dir.path()), vec!["shell".to_string()]);

        // 非法模式被拒，原值保持。
        assert!(write_permission_mode(dir.path(), "sudo").is_err());
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_FULL);
        // 切回默认照常。
        write_permission_mode(dir.path(), PERMISSION_MODE_DEFAULT).unwrap();
        assert_eq!(read_permission_mode(dir.path()), PERMISSION_MODE_DEFAULT);
    }
}
