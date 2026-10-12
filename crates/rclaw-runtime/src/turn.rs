//! 长 Turn 用户插队 / 中断架构：`TurnManager` + 每会话活动 turn 任务。
//!
//! # 背景
//! 旧 `AgentLoop` 是「单 turn 串行」模型：从 inbound 取一条消息 → 完整跑完 `handle_turn`
//! → 才取下一条。长 turn（如股票 skill 调 Python 脚本）期间用户新输入只能进 inbound 队列，
//! 既进不了正在运行的 LLM 上下文，也中断不了正在执行的 skill / tool。
//!
//! # 本模块的解法
//! - 用户新输入先作为「控制事件」进入该会话的活动 turn，**不立即写 canonical session**。
//! - 全局 agent loop 不再跑完整 turn，只负责收消息 + [`TurnManager::route`]；
//!   真正的 LLM / tool 执行放进对应会话的活动 turn 任务。
//! - 普通消息的同一个 `(channel, chat_id)` 至多一个活动 turn（即 per-session 单写者），
//!   保证 session 写入顺序可控；带 job id 的 Cron 消息按 job 隔离，可并行执行后回复到同一会话。
//! - `/cancel` 在路由层优先识别：取消当前回复目标下的全部 running turn，不污染 inbound、
//!   不写 session。
//! - 只有在「协议安全点」才把插队补充注入当前 `messages` / session。
//! - 用可配置的全局信号量限制顶层 turn 并发；后台 review 不写 session、只使用受限工具集
//!   且带超时，避免阻塞下一轮用户 turn。
//!
//! # 取消实现策略（surgical）
//! 不改动 `Provider` / `Tool` trait 签名，而是在活动 turn 任务里用 `tokio::select!`
//! 把 LLM 调用 / 工具执行 / 信号量等待 future 与 `cancel.cancelled()` 同台竞争：
//! - 取消 LLM 流：select 走 cancel 分支 → `chat_stream` future 被 drop → reqwest 流被 drop →
//!   HTTP 连接关闭，`on_text` 不再被调用；门禁前缓冲的 partial 会被丢弃且不落盘。
//! - 取消工具执行：select 走 cancel 分支 → `execute_tools` future 被 drop → 内部 `Shell` 的
//!   子进程靠 `kill_on_drop(true)` 随 future drop 被 kill。
//!
//! 这样既满足文档「可取消」，又不波及 llm / tools 两个 crate。

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read as IoRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use futures::future::join_all;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use rclaw_core::{
    BusMessage, ChatRequest, ContentBlock, Message, MessageSource, Role, ToolSchema, CHANNEL_CLI,
    CHANNEL_NAPCAT, CHANNEL_SYSTEM, CHANNEL_WEIXIN,
};
use rclaw_llm::{LlmError, LlmErrorClass, Provider};
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_store::skills::build_summaries;
use rclaw_store::{redact_sensitive_shell_command, redact_sensitive_text, resolve_within_base};
use rclaw_tools::{
    MemoryTool, SkillListTool, SkillManageTool, SkillReadTool, SkillUsageStore, ToolRegistry,
    ToolResult,
};

use crate::context;
use crate::dynamic_tools::{SkillCatalog, SkillInvocation};
use crate::options::AgentOptions;
use crate::slash::SlashRegistry;

mod commands;
mod media;
mod run;
mod tool_execution;
use media::*;
pub(crate) use run::{
    LoopDecision, Run, RunAdapter, MAX_COMPLETION_GATE_REJECTIONS, REPEATED_TOOL_CALL_LIMIT,
};

mod state;
pub(crate) use media::{
    agent_result_reports_media_publish_attempt, claims_media_delivery, finalize_media_reply,
    is_media_control_line, media_channel_available, revalidated_pending_media_markers,
    skill_package_label, validated_media_markers_from_agent_result,
    validated_media_markers_from_tool_result,
};
pub(crate) use state::TurnState;
pub(crate) use tool_execution::{
    execute_tool_batch, is_candidate_patch_tool_allowed, is_subagent_blocked_tool,
    ToolExecutionContext, ToolExecutionPolicy,
};

mod compression;
mod in_turn;
mod output;
mod review;
pub mod rewind;

use compression::*;
use output::*;
use review::*;

pub(crate) use output::{
    build_loop_circuit_result, is_potential_skill_entry_path, load_package_skill,
    normalize_absolute_lexical_path, normalize_agent_tool_path, patch_runtime_source_channel,
    PackageSkillLoad, ToolExecutionMessages,
};

mod effect;
mod external_action;

use external_action::*;

mod gate;
mod history;
use gate::QuiescenceGate;
use history::HistoryLoader;

pub(crate) use effect::{
    shell_command_attempts_media_publisher, shell_command_runs_media_publisher,
    untrusted_publish_media_command_error,
};

/// 包级 Skill 说明是指令，不应绕过常规 read 的上下文预算无限载入。
const MAX_PACKAGE_SKILL_BYTES: usize = 256 * 1024;
/// 已有媒体门禁认可的 Skill 发布脚本；shell 与 Manifest 入口共用此列表。
pub(crate) const TRUSTED_MEDIA_PUBLISHER_SCRIPTS: &[&str] = &[
    "glm-master-skill/scripts/publish_media.py",
    "evaluate-fund/scripts/md2html.py",
    "glmv-stock-analyst/scripts/md2html.py",
    "glmv-stock-analyst/scripts/export_report.py",
];

// ---------------------------------------------------------------------------
// 会话键
// ---------------------------------------------------------------------------

/// 唯一确定一个活动 turn 的会话键。
///
/// 不同通道 / 不同会话天然解耦：`(cli, console)` 与 `(weixin, wx_user_123)` 互不影响，
/// 各自维护自己的活动 turn。带身份的 Cron 再按 `job_id` 解耦，使回复到同一外部会话的
/// 不同定时任务也能独立执行。`/cancel` 按 `(channel, chat_id)` 取消该回复目标下的全部任务。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TurnKey {
    pub channel: String,
    pub chat_id: String,
    /// 带身份的 Cron 消息按 job 分执行通道；普通消息与旧式无身份 Cron 为 `None`。
    pub cron_job_id: Option<String>,
}

impl TurnKey {
    /// 由一条总线消息得到它所属的会话键。
    pub fn from_msg(msg: &BusMessage) -> Self {
        TurnKey {
            channel: msg.channel.clone(),
            chat_id: msg.chat_id.clone(),
            cron_job_id: msg.cron_job_id().map(str::to_string),
        }
    }
}

/// Reject state-dependent duplicate/conflicting Skill writes before any call in the batch runs.
pub(crate) fn preflight_skill_manage_batch(
    calls: &[rclaw_core::ToolCall],
) -> HashMap<usize, String> {
    let mut mutations = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        if call.name != "skill_manage" {
            continue;
        }
        let Ok(input) = serde_json::from_str::<Value>(&call.input) else {
            continue;
        };
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let action = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if !name.is_empty() && !action.is_empty() {
            mutations.push((index, name.to_string(), action.to_string()));
        }
    }
    if mutations.len() == 1 && calls.iter().any(|call| call.name == "read") {
        let (index, name, action) = &mutations[0];
        return HashMap::from([(
            *index,
            format!(
                "Rejected skill_manage {name}:{action} before execution: read results from the same model batch are not visible to the model yet. Observe the read result, then submit exactly one skill_manage mutation in the next response."
            ),
        )]);
    }
    if mutations.len() < 2 {
        return HashMap::new();
    }
    let mut rejected = HashMap::new();
    let actions = mutations
        .iter()
        .map(|(_, name, action)| format!("{name}:{action}"))
        .collect::<Vec<_>>()
        .join(" -> ");
    let error = format!(
        "Rejected every skill_manage call in this model batch before execution: it contains multiple state-dependent Skill mutations ({actions}). Run exactly one skill_manage mutation, observe its result, then re-read and decide the next mutation. Never use create/delete as an activation or placeholder operation."
    );
    for (index, _, _) in mutations {
        rejected.insert(index, error.clone());
    }
    rejected
}

fn render_skills(layout: &Layout) -> String {
    let mut skills = build_summaries(layout);
    skills.sort_by(|a, b| {
        a.title
            .to_lowercase()
            .cmp(&b.title.to_lowercase())
            .then_with(|| a.path.cmp(&b.path))
    });

    if skills.is_empty() {
        return "当前没有可用的 skill。".to_string();
    }

    let mut output = format!("当前支持的 skill（共 {} 个）：\n", skills.len());
    for (index, skill) in skills.iter().enumerate() {
        let title = skill.title.split_whitespace().collect::<Vec<_>>().join(" ");
        let package = skill_package_label(layout, &skill.path, &title);
        let description = skill
            .description
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let description = if description.is_empty() {
            "未提供功能描述"
        } else {
            &description
        };
        output.push_str(&format!("\n{}. {}\n", index + 1, title));
        if let Some(package) = package {
            output.push_str(&format!("   所属技能包：{package}\n"));
        }
        output.push_str(&format!("   功能：{description}\n"));
    }
    output.trim_end().to_string()
}

/// 渲染当前注册的全部工具名称与说明（`/tools` 命令用）。
///
/// 直接复用运行时 `ToolRegistry` 的 schema，保证与 Agent 实际可调用的工具清单一致。
/// 按名称排序便于查阅；只显示工具名与说明，不暴露 JSON Schema 细节。
fn render_tools(schemas: &[ToolSchema]) -> String {
    if schemas.is_empty() {
        return "当前没有可用的工具。".to_string();
    }
    let mut sorted: Vec<&ToolSchema> = schemas.iter().collect();
    sorted.sort_by_key(|schema| schema.name.to_lowercase());
    let mut output = format!("当前支持的工具（共 {} 个）：\n", sorted.len());
    for (index, schema) in sorted.iter().enumerate() {
        let description = schema.description.trim();
        let description = if description.is_empty() {
            "未提供说明"
        } else {
            description
        };
        output.push_str(&format!(
            "\n{}. {}\n   {}\n",
            index + 1,
            schema.name,
            description
        ));
    }
    output.trim_end().to_string()
}

/// 把字节数格式化为人类可读的 B / KB / MB。
fn format_bytes(n: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if n >= MB {
        format!("{:.1} MB", n as f64 / MB as f64)
    } else if n >= KB {
        format!("{:.1} KB", n as f64 / KB as f64)
    } else {
        format!("{} B", n)
    }
}

/// 非 shell 工具调用的进度摘要（`name` 或 `name → 摘要`）：
/// 文件类工具带路径与行数增减，web_search 带检索词，其余仅工具名。
/// 桌面端按「 → 」切分后渲染成动作流水（图标 / 标签 / 路径 / +/- 行数）。
fn tool_call_brief(name: &str, input: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(input) else {
        return name.to_string();
    };
    let str_field = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    };
    let line_count = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .map(|s| s.lines().count())
            .unwrap_or(0)
    };
    match name {
        "read" | "list" | "grep" | "delete" => {
            let p = str_field("path");
            if p.is_empty() {
                name.to_string()
            } else {
                format!("{name} → {p}")
            }
        }
        "write" => {
            let p = str_field("path");
            if p.is_empty() {
                return name.to_string();
            }
            let added = line_count("content");
            format!("{name} → {p} +{added}")
        }
        "edit" => {
            let p = str_field("path");
            if p.is_empty() {
                return name.to_string();
            }
            let added = line_count("new_string");
            let removed = line_count("old_string");
            format!("{name} → {p} +{added} -{removed}")
        }
        "web_search" => {
            let q = str_field("search_query");
            if q.is_empty() {
                name.to_string()
            } else {
                // 单行化，避免检索词里的换行破坏进度消息的单行约定。
                let first_line = q.lines().next().unwrap_or("");
                format!("{name} → {first_line}")
            }
        }
        "web_browser" => {
            let brief = str_field("query");
            let brief = if brief.is_empty() {
                str_field("url")
            } else {
                brief
            };
            if brief.is_empty() {
                name.to_string()
            } else {
                format!("{} → {}", name, brief.lines().next().unwrap_or(""))
            }
        }
        _ => name.to_string(),
    }
}

/// `/log` 的用法提示。
fn log_usage() -> String {
    "/log 子命令：\n\
     /log                  列出日志文件（数量、名称、大小）\n\
     /log get <名称>       查看日志内容（超过 20KB 仅显示末尾）\n\
     /log del <名称>       删除单个日志\n\
     /log del all confirm  删除全部日志（需确认）"
        .to_string()
}

/// 把日志文件名解析到 logs_dir 内的路径，拒绝目录穿越（分隔符 / `..` / 绝对路径）。
fn resolve_log_name(logs_dir: &Path, name: &str) -> std::result::Result<PathBuf, String> {
    if name.is_empty() {
        return Err("缺少日志文件名。用法：/log get <名称>".to_string());
    }
    let clean = name.replace('\\', "/");
    if clean.contains('/') || clean.contains("..") || Path::new(name).is_absolute() {
        return Err(format!(
            "非法日志文件名「{name}」：仅允许 agent/logs 目录内的文件名。"
        ));
    }
    let path = logs_dir.join(name);
    // 拒绝符号链接：日志只应是 logs_dir 内的普通文件。/log get 会把文件复制进共享 media 沙箱
    // 并经当前媒体 channel 外发，若放行指向 logs_dir 之外（如 /etc/passwd）的符号链接，
    // 会被诱导读取并外泄沙箱外数据。resolve_log_name 仅做词法校验，这里补符号链接拦截。
    if matches!(std::fs::symlink_metadata(&path), Ok(meta) if meta.is_symlink()) {
        return Err(format!(
            "非法日志文件名「{name}」：不能是指向 logs 目录之外的符号链接。"
        ));
    }
    Ok(path)
}

/// 抽成纯函数便于单测。删除全部 memory + sessions 不可逆，故要求显式二次确认，避免误触。
fn clear_confirmed(content: &str) -> bool {
    let mut words = content
        .trim()
        .strip_prefix('/')
        .unwrap_or("")
        .split_whitespace();
    words.next(); // 跳过命令名 "clear"
    match words.next() {
        Some(w) => {
            let w = w.to_ascii_lowercase();
            w == "confirm" || w == "yes"
        }
        None => false,
    }
}

/// 安全闸：`dir` 必须是位于 `base` 下、且**非 reparse point** 的真实目录，否则拒绝清空。
///
/// 比对 `canonicalize(dir)`（跟随 dir 自身的 symlink/junction）与「`canonicalize(base)` + dir
/// 相对 base 的后缀」（dir 应当所在的物理位置）。二者【相等】才放行。
///
/// 为什么用相等而非 `starts_with`（P2 修复）：旧 `starts_with(base)` 只挡「junction 指向 base
/// 之外」；若 `agent/memory` 被替换成指向 `agent/skills`（base 之内）的 junction，
/// `canonicalize` 仍以 base 为前缀 → 改用相等判定后，任何 reparse point（无论指向 base 内外）
/// 都会因 `canonicalize(dir)` 偏离「base_c + 后缀」而被拒。Windows junction 是 reparse mount
/// point，[`std::fs::FileType::is_symlink`] 检测不到，但 `canonicalize` 会跟随它，故此法有效。
///
/// `dir` 不存在则放行（无可清内容、也无危险）。
fn ensure_dir_is_real(
    dir: &std::path::Path,
    base: &std::path::Path,
) -> std::result::Result<(), String> {
    if !dir.exists() {
        return Ok(());
    }
    let suffix = dir
        .strip_prefix(base)
        .map_err(|_| format!("{dir:?} 不在 agent base {base:?} 之下，已拒绝清空"))?;
    let base_c =
        std::fs::canonicalize(base).map_err(|e| format!("解析 base {base:?} 失败: {e}"))?;
    let expected = base_c.join(suffix); // dir 应当所在的物理位置（不跟随 dir 自身 reparse）
    let actual = std::fs::canonicalize(dir).map_err(|e| format!("解析路径 {dir:?} 失败: {e}"))?;
    if actual != expected {
        return Err(format!(
            "{dir:?} 是符号链接/junction（解析到 {actual:?}，预期应为 {expected:?}），已拒绝清空"
        ));
    }
    Ok(())
}

/// 纯函数：渲染 `/context` 回显（上下文窗口 / 已用 / 剩余 / 使用率）。
///
/// 抽成纯函数便于单测。窗口由 `opts.context_length` 决定（`0` = 未声明，按
/// `COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH` 即 128K 假设）；同时展示会话历史 token 估算
/// 与完整请求压力估算（历史 + system prompt + 工具 schema）。压缩按完整请求压力触发。
///
/// 尾注见 [`context_footer`]：按压缩启用状态与 token 阈值是否生效分三种情况，避免误导——尤其
/// token 阈值 = 0（不会自动摘要化旧历史）时不能显示「约 0 token」。
fn render_context_usage(history_used: usize, request_used: usize, opts: &AgentOptions) -> String {
    let capacity = if opts.context_length > 0 {
        opts.context_length
    } else {
        crate::options::COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH
    };
    let remaining = capacity.saturating_sub(request_used);
    let pct = if capacity > 0 {
        (request_used as f64 / capacity as f64 * 100.0).round() as usize
    } else {
        0
    };
    format!(
        "上下文窗口：{capacity} token\n\
         已用（历史估算）：{history_used} token\n\
         请求压力估算（历史 + system prompt + tools）：{request_used} token（{pct}%）\n\
         剩余：{remaining} token\n\
         \n\
         注：自动压缩按“请求压力估算”触发；历史估算单独展示，便于判断旧会话本身的体量。\n\
         {footer}",
        footer = context_footer(opts)
    )
}

/// 上下文占用的数值快照（供桌面版状态面板等外部调用方轮询展示）。
#[derive(Debug, Clone, Copy)]
pub struct ContextUsageSnapshot {
    /// 会话历史 token 估算（不含 system prompt / 工具 schema）。
    pub history_used: u64,
    /// 请求压力估算（历史 + system prompt + 动态上下文 + 工具 schema），
    /// 与压缩触发同口径；无真实 usage 锚点时即纯启发式估算。
    pub request_used: u64,
    /// 上下文窗口（`opts.context_length`，0 按 128K 假设，与 `/context` 一致）。
    pub context_length: u64,
}

/// [`TurnManager::handle_context`] 的无锚数值版：同一套估算器，但不需要 TurnManager
/// 句柄——输入全部是装配期可克隆的组件（桌面版 RunningAgent 都有）。历史读取失败时
/// 返回 `Err`（面板可显示错误而不是静默归零）。
///
/// 历史走只读变体 [`SessionStore::load_history_view`]：本函数供轮询调用方使用，会与
/// 活动 turn 的 load→append 并发，不能像 `/context` 那样触发物理压缩写回（裁剪口径相同，
/// 估算数值不受影响）。
#[allow(clippy::too_many_arguments)]
pub fn estimate_context_usage(
    session: &SessionStore,
    memory: &MemoryStore,
    layout: &Layout,
    tools: &ToolRegistry,
    opts: &AgentOptions,
    channels: &rclaw_core::RuntimeChannelInfo,
    search: Option<&rclaw_tools::WebSearchControl>,
    channel: &str,
    chat_id: &str,
) -> Result<ContextUsageSnapshot, String> {
    let history = session
        .load_history_view(channel, chat_id, opts.max_history, opts.max_history_bytes)
        .map_err(|e| format!("读取会话历史失败：{e}"))?;
    let history_used = estimate_history_tokens(&history) as u64;
    // 与 TurnManager::new 同源：system prompt 由同一构建器重建（含 web_search 描述段）。
    let system = context::build_system_prompt_for_tools(
        layout,
        memory,
        opts.recent_days,
        search,
        tools.schemas(),
    );
    let status = Run::new_with_base("检查当前上下文占用", opts.max_tool_iter, &layout.base);
    let catalog = SkillCatalog::new(layout);
    let mut request_messages = history;
    request_messages.push(context::inference_tail_message(
        layout,
        memory,
        opts.recent_days,
        &catalog,
        channel,
        chat_id,
        false,
        channels,
        opts.daemon,
        &status,
    ));
    let request_used =
        estimate_request_tokens(Some(&system), &request_messages, tools.schemas()) as u64;
    let context_length = if opts.context_length > 0 {
        opts.context_length
    } else {
        crate::options::COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH
    } as u64;
    Ok(ContextUsageSnapshot {
        history_used,
        request_used,
        context_length,
    })
}

/// `/context` 尾注：按压缩配置分三种情况，避免显示与系统实际行为不符。
///
/// - 压缩关闭（`session_compression_enabled = false`）：提示只做硬裁剪；
/// - token 阈值 > 0：显示 token 触发阈值（与「已用」同口径，故使用率与触发点直接可比）；
/// - token 阈值 = 0：token 触发已关闭，不会自动摘要化旧历史。显示禁用提示，而不是
///   误导性的「约 0 token」。
fn context_footer(opts: &AgentOptions) -> String {
    if !opts.session_compression_enabled {
        return "会话语义压缩已关闭：超过窗口时只做硬裁剪，不会摘要化旧历史。".to_string();
    }
    if opts.compression_threshold_tokens > 0 {
        return format!(
            "压缩触发阈值：约 {} token（超过即摘要化旧历史）。",
            opts.compression_threshold_tokens
        );
    }
    "压缩触发阈值未启用（token 阈值为 0）：不会自动摘要化旧历史。".to_string()
}

// ---------------------------------------------------------------------------
// 插队事件
// ---------------------------------------------------------------------------

/// 用户在 turn 运行期间投递的「控制事件」。
///
/// MVP 只支持「普通补充文本」一种（按到达顺序处理，不做复杂合并）；「替换任务」
/// 等价于 cancel + 新 turn，可后续再单独建模。
#[derive(Debug)]
enum Interjection {
    /// 同一会话在 turn 运行期间到达的完整消息。
    ///
    /// 只有真实用户消息会在协议安全点作为补充注入当前 `messages`；cron / heartbeat 等
    /// 内部消息必须保留原始 [`MessageSource`]，等当前 turn 结束后重路由，避免被误持久化
    /// 成普通 user 文本。注意只有相同 [`TurnKey`] 才会进入这里；不同 `(channel, chat_id)`，
    /// 或同一回复目标但不同 `cron_job_id`，都会直接启动自己的 active turn。
    AppendMessage(BusMessage),
}

/// 一个活动会话当前的活动 turn 句柄（存放在 [`TurnManager::active`] map 里）。
struct ActiveTurn {
    /// 取消令牌：触发后 turn 任务在最近的 select! 处中止。
    cancel: CancellationToken,
    /// 向该 turn 投递插队事件的发送端（路由层持克隆副本，不阻塞地 `try_send`）。
    event_tx: mpsc::Sender<Interjection>,
    // 注意：不存放 turn 任务的 `JoinHandle`。tokio 的 spawned 任务在句柄 drop 时自动
    // detach（继续在后台跑），而取消走 `CancellationToken`（不用 join.abort()），故无需持有句柄。
}

// ---------------------------------------------------------------------------
// TurnManager
// ---------------------------------------------------------------------------

/// 活动会话的活动 turn 管理器：路由、取消、插队、单 turn 执行。
///
/// 全局只有一个实例（由 `AgentLoop` 持有 `Arc<TurnManager>`）。所有依赖（provider /
/// 存储 / 工具 / 选项 / outbound / 斜杠）都是可共享 / 可克隆的，方便克隆进每个 turn 任务。
pub struct TurnManager {
    /// `(channel, chat_id, cron_job_id) → 活动 turn`。普通消息同一会话至多一个活动 turn；
    /// 不同 Cron job 即使回复目标相同，也使用独立活动 turn。
    active: Arc<Mutex<HashMap<TurnKey, ActiveTurn>>>,

    provider: Arc<dyn Provider>,
    session: SessionStore,
    memory: MemoryStore,
    layout: Layout,
    base_system_prompt: Arc<String>,
    tools: Arc<ToolRegistry>,
    opts: AgentOptions,
    outbound_tx: mpsc::Sender<BusMessage>,
    /// 无状态斜杠命令注册表（`/llmodel` / `/help` 等）；需要 runtime 状态的命令由
    /// `turn::commands` 分发。`None` 表示不启用无状态命令。
    slash: Option<Arc<SlashRegistry>>,

    /// Bot Mode：bot 注册表（persona 解析 + 花名册）。无 bot 时零行为差异。
    bots: Arc<crate::bots::BotRegistry>,
    /// Bot Mode：交接回执路由。turn 收尾时通知等待方；None = 未装配（测试/精简装配）。
    bot_router: Option<Arc<crate::bots::BotRouter>>,

    /// 接线期采集的通道能力与共享媒体沙箱（C3：替代 RCLAW_* 环境变量）。
    channels: rclaw_core::RuntimeChannelInfo,

    /// 顶层 turn 的全局执行并发上限。路由 / 中断能力不受影响；同一普通会话仍由
    /// `active` map 保持单活动 turn + 单写者。
    execution_limit: Arc<Semaphore>,

    /// 会话历史加载与自动压缩编排（防抖状态由模块内部持有）。
    history: HistoryLoader,

    /// post-turn review 的到期判定、执行、持久化、回报与空闲维护。
    reviews: Arc<ReviewLifecycle>,
    /// 静默门：互斥生命周期操作与新 turn / review 启动（C7）。
    gate: Arc<QuiescenceGate>,
}

impl TurnManager {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn Provider>,
        review_provider: Option<Arc<dyn Provider>>,
        session: SessionStore,
        memory: MemoryStore,
        layout: Layout,
        tools: Arc<ToolRegistry>,
        opts: AgentOptions,
        outbound_tx: mpsc::Sender<BusMessage>,
        slash: Option<Arc<SlashRegistry>>,
        channels: rclaw_core::RuntimeChannelInfo,
        bots: Arc<crate::bots::BotRegistry>,
        bot_router: Option<Arc<crate::bots::BotRouter>>,
    ) -> Self {
        let max_concurrent_turns = opts.max_concurrent_turns.clamp(1, Semaphore::MAX_PERMITS);
        // 系统提示按宿主三态生成：桌面/Termux 与注册表无关（零回归），APK 宿主按
        // 实际注册工具裁剪（不宣称不存在的 shell/CLI 能力）。
        let base_system_prompt = Arc::new(context::build_system_prompt_for_tools(
            &layout,
            &memory,
            opts.recent_days,
            slash
                .as_ref()
                .and_then(|s| s.web_search_control())
                .as_deref(),
            tools.schemas(),
        ));
        let execution_limit = Arc::new(Semaphore::new(max_concurrent_turns));
        let active = Arc::new(Mutex::new(HashMap::new()));
        let history =
            HistoryLoader::new(&session, &layout, &opts, &channels, execution_limit.clone());
        let gate = Arc::new(QuiescenceGate::new());
        let reviews = Arc::new(ReviewLifecycle::new(
            provider.clone(),
            review_provider,
            &memory,
            &layout,
            &opts,
            outbound_tx.clone(),
            &channels,
            gate.clone(),
            bots.clone(),
        ));
        TurnManager {
            active,
            provider,
            session,
            memory,
            layout,
            base_system_prompt,
            tools,
            opts,
            outbound_tx,
            slash,
            channels,
            bots,
            bot_router,
            execution_limit,
            history,
            reviews,
            gate,
        }
    }

    // ---- active map 访问：全部走同步辅助函数 ----
    //
    // 为什么不直接在 async 方法里 `self.active.lock()`：std 与 tokio 的 `MutexGuard` 都是
    // `!Send`（tokio 的 MutexGuard 只实现 Sync、不实现 Send；std 的同理）。一旦某个 async
    // 方法的 future 结构里出现 guard 字段，该 future 就变成 `!Send`，无法被 `tokio::spawn`
    // 跨线程执行（agent loop 的 run / 各 turn 任务都是 spawned）。把锁操作收进【同步】辅助函数，
    // guard 只活在同步调用栈上、绝不进入任何 async future 的状态，于是 future 仍是 `Send`。
    // 这些临界段都是纯同步操作（无 await），用 `std::sync::Mutex` 即可。

    /// 同步：取某会话活动 turn 的插队事件发送端（克隆），不持有锁返回。
    fn active_event_tx(&self, key: &TurnKey) -> Option<mpsc::Sender<Interjection>> {
        self.active
            .lock()
            .expect("active map 锁未被毒化")
            .get(key)
            .map(|t| t.event_tx.clone())
    }

    /// 同步：取消同一回复目标下的全部活动 turn。
    ///
    /// 返回 `(匹配任务数, 是否全部早已处于取消状态)`。Cron turn 会额外按 job id 分键，
    /// 因此用户侧 `/cancel` 不能再只查普通的 `cron_job_id=None` 键。
    fn active_cancel_session(&self, channel: &str, chat_id: &str) -> (usize, bool) {
        let mut active = self.active.lock().expect("active map 锁未被毒化");
        active.retain(|key, turn| {
            key.channel != channel || key.chat_id != chat_id || !turn.event_tx.is_closed()
        });

        let mut matched = 0usize;
        let mut all_already_cancelled = true;
        for (key, turn) in active.iter() {
            if key.channel == channel && key.chat_id == chat_id {
                matched += 1;
                if !turn.cancel.is_cancelled() {
                    all_already_cancelled = false;
                    turn.cancel.cancel();
                }
            }
        }
        (matched, matched > 0 && all_already_cancelled)
    }

    /// 同步：仅当 map 中的条目仍是同一条事件通道时移除它。
    ///
    /// 这是一个窄窗口兜底：turn 主体可能已经退出并关闭 `event_rx`，但外壳任务还没来得及
    /// 从 active map 自移除。此时路由层拿到的 `event_tx` 已关闭，必须清掉对应僵尸条目后
    /// 再启动新 turn；但不能误删已经被其它路径新建的活动 turn。正常 turn 收尾也复用它来
    /// 确保只删除自己的 active 条目。
    fn active_remove_if_same_event_tx(
        &self,
        key: &TurnKey,
        tx: &mpsc::Sender<Interjection>,
    ) -> bool {
        let mut active = self.active.lock().expect("active map 锁未被毒化");
        let same = active
            .get(key)
            .is_some_and(|turn| turn.event_tx.same_channel(tx));
        if same {
            active.remove(key);
        }
        same
    }

    /// 路由一条 inbound 消息（agent loop 的唯一调用入口）。
    ///
    /// 顺序：
    /// 1. `/cancel`（仅真实用户消息）→ 取消当前会话 running turn 或回送「无任务」提示；
    /// 2. 其它已知斜杠命令（`/skills` / `/llmodel` / `/help` 等）→ 路由层即时处理，不进 LLM / session；
    /// 3. 已有活动 turn → 把新输入作为插队事件投递给该 turn（在下一个安全点处理）；
    /// 4. 无活动 turn → 为该会话启动一个新 turn 任务。
    ///
    /// 注意：本方法在 agent loop 里被 `await` 串行调用，**不会并发**，因此「检查 active →
    /// 插入 active」之间无需担心另一个 route 介入；但仍需与 turn 任务的自移除互斥。
    pub async fn route(self: &Arc<Self>, msg: BusMessage) {
        if msg.source() == MessageSource::User {
            tracing::info!(
                channel = ?msg.channel,
                chat_id = ?msg.chat_id,
                input = ?msg.content,
                "用户原始输入"
            );
        }

        if commands::dispatch(self, &msg).await {
            return;
        }

        let key = TurnKey::from_msg(&msg);

        // 1) 已有活动 turn → 投递插队事件（同步取发送端，再用 try_send 不阻塞路由）。
        //
        // 这里命中的一定是【同一个 TurnKey】。不同 `(channel, chat_id)`，或同一回复目标下
        // 不同 `cron_job_id`，不会看见这个 active 条目，会直接走下面的 `spawn_turn`。
        // 对于同 key 的用户 / heartbeat / 旧式无 job id Cron，仍不能并行另开 turn：它们可能
        // 共享 session 与用户可见回复顺序，因此先进入事件队列，之后按原始 MessageSource
        // 重路由成新 turn。
        //
        // 若撞上「event_rx 已关闭但 active 条目尚未清理」的窄窗口，清掉同一条旧通道后重试，
        // 避免把消息投给已结束的 turn 或在 spawn_turn 的双重检查里丢失。
        loop {
            if let Some(tx) = self.active_event_tx(&key) {
                // 用 clone 投递：通道关闭时 msg 仍完整，可在清理 stale 条目后继续使用。
                match tx.try_send(Interjection::AppendMessage(msg.clone())) {
                    Ok(()) => return,
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        // MVP 不阻塞路由：插队队列满时丢弃并告警（极端情况，队列容量 32 已很大）。
                        tracing::warn!(
                            "turn 插队队列已满，丢弃补充输入 ({}:{})",
                            key.channel,
                            key.chat_id
                        );
                        return;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        // 窄窗口：turn 任务已退出但还没来得及从 active 移除。
                        if self.active_remove_if_same_event_tx(&key, &tx) {
                            tracing::debug!(
                                "turn 事件通道已关闭，已移除僵尸 active 条目后重试 ({}:{})",
                                key.channel,
                                key.chat_id
                            );
                            continue;
                        }
                        // 条目已被其它路径替换，重试读取最新 active。
                        continue;
                    }
                }
            } else {
                // 2) 无活动 turn → 启动新 turn 任务。
                if !self.spawn_turn(msg.clone()) {
                    // 静默门被生命周期操作持有时拒绝启动：回送短暂提示（罕见窗口）。
                    if msg.source() == MessageSource::User {
                        let _ = self
                            .outbound_tx
                            .send(BusMessage::new(
                                msg.channel,
                                msg.chat_id,
                                "⏳ 系统正在执行清理 / 维护操作，请稍后重试。",
                            ))
                            .await;
                    } else {
                        tracing::warn!(
                            "静默门被生命周期操作持有，非用户来源的残留消息被丢弃 ({}:{})",
                            msg.channel,
                            msg.chat_id
                        );
                        // 被丢弃的交接消息：按失败通知等待方，而不是挂到超时。
                        if let Some(router) = &self.bot_router {
                            router.notify(
                                &msg.channel,
                                &msg.chat_id,
                                msg.handoff_id(),
                                crate::bots::HandoffOutcome::Failed(
                                    "系统正在执行清理 / 维护操作，交接未投递".into(),
                                ),
                            );
                        }
                    }
                }
                return;
            };
        }
    }

    /// 同步：该会话是否有活动 turn（/compact 等重写会话的操作用）。
    pub(crate) fn has_active_turn(&self, channel: &str, chat_id: &str) -> bool {
        self.active
            .lock()
            .expect("active map 锁未被毒化")
            .keys()
            .any(|key| key.channel == channel && key.chat_id == chat_id)
    }

    /// `/compact`：手动立即压缩当前会话（ZCode Layer 1 manual `/compact` 的移植面）。
    ///
    /// 与自动压缩共用同一套摘要 / 确定性 fallback / session 重写管线，但忽略阈值与
    /// 防抖（用户显式意图优先）；有活动 turn 时拒绝（避免与 turn 的原子追加交错）。
    async fn handle_compact(&self, channel: &str, chat_id: &str) -> String {
        if self.has_active_turn(channel, chat_id) {
            return "⏳ 该会话有正在运行的任务，/compact 已中止：请先 /cancel 或等其结束后重试。"
                .to_string();
        }
        let persona = self.bots.persona_for_chat(channel, chat_id);
        let turn_memory = persona
            .as_ref()
            .map(|p| p.memory.clone())
            .unwrap_or_else(|| self.memory.clone());
        let provider = persona
            .as_ref()
            .and_then(|p| p.provider.clone())
            .unwrap_or_else(|| self.provider.clone());
        let full = match self.session.load_all(channel, chat_id) {
            Ok(history) => history,
            Err(e) => return format!("读取会话历史失败：{e}"),
        };
        let keep_recent = self.opts.compression_keep_recent.max(8);
        if full.len() <= keep_recent + 2 {
            return format!("会话只有 {} 条消息，没有可压缩的窗口。", full.len());
        }
        let msg = BusMessage::new(channel.to_string(), chat_id.to_string(), String::new());
        let cancel = CancellationToken::new();
        let sink = crate::turn::history::CompressionSink {
            memory: turn_memory,
            provider,
        };
        match self
            .history
            .summarize(&full, &msg, &cancel, keep_recent, &sink)
            .await
        {
            SummaryLoad::Completed {
                summary,
                compressed,
                used_fallback,
            } => {
                let key = TurnKey {
                    channel: channel.to_string(),
                    chat_id: chat_id.to_string(),
                    cron_job_id: None,
                };
                // 与自动压缩同源：fallback 记冷却、低效记防抖计数。
                self.history.record_compression_outcome(
                    &key,
                    history_serialized_bytes(&full),
                    history_serialized_bytes(&compressed),
                    full.len(),
                    used_fallback,
                );
                match self.session.rewrite(channel, chat_id, &compressed) {
                    Ok(()) => {
                        if let Err(e) =
                            sink.memory
                                .write_session_summary(channel, chat_id, &summary)
                        {
                            tracing::warn!("/compact 摘要文件写入失败（不影响 session）: {e}");
                        }
                        format!(
                            "✅ 会话已压缩：{} → {} 条{}。摘要已写入会话记忆。",
                            full.len(),
                            compressed.len(),
                            if used_fallback {
                                "（LLM 摘要不可用，使用确定性 fallback）"
                            } else {
                                ""
                            }
                        )
                    }
                    Err(e) => format!("压缩结果写入失败（原会话保留）：{e}"),
                }
            }
            SummaryLoad::Cancelled => "压缩已取消。".to_string(),
            SummaryLoad::IneffectivePlan => {
                "没有可压缩的窗口（历史太短或已全部是摘要），会话保持不变。".to_string()
            }
            SummaryLoad::Failed => {
                "压缩摘要生成失败（鉴权/网络类错误），会话保持不变；请稍后重试。".to_string()
            }
        }
    }

    /// `/rewind [message_id]`：列出可回滚点或按 messageId 恢复文件
    /// （Layer 5 选点回滚；恢复前 hash 校验，外部改动整组拒绝）。
    fn handle_rewind(&self, channel: &str, chat_id: &str, content: &str) -> String {
        let mut parts = content.split_whitespace();
        let _command = parts.next();
        match parts.next() {
            None => {
                let groups = rewind::list_groups(&self.layout, channel, chat_id);
                if groups.is_empty() {
                    return "当前会话没有可回滚的文件 checkpoint（仅 write/edit/delete 会被记录）。"
                        .to_string();
                }
                let mut out = format!("可回滚点（最近 {} 组，新在前）：", groups.len());
                for (message_id, files) in &groups {
                    out.push_str(&format!("\n\n- 消息 {message_id}（{} 个文件）", files.len()));
                    for (path, _) in files {
                        out.push_str(&format!("\n  - {path}"));
                    }
                }
                out.push_str(
                    "\n\n回滚：/rewind <message_id>（恢复前校验文件 hash，被外部改动过则整组拒绝）",
                );
                out
            }
            Some(id_text) => {
                if self.has_active_turn(channel, chat_id) {
                    return "⏳ 有任务正在运行，/rewind 已中止：请先 /cancel 或等其结束后重试。"
                        .to_string();
                }
                let Ok(message_id) = id_text.parse::<i64>() else {
                    return format!("非法 messageId「{id_text}」：应为 /rewind 列表中的数字。");
                };
                match rewind::restore_to_message(&self.layout, channel, chat_id, message_id) {
                    Ok(report) => report,
                    Err(error) => format!("⚠️ {error}"),
                }
            }
        }
    }

    /// `/context` 的内容生成：当前模型上下文窗口 + 历史估算 + 请求压力估算 + 剩余 + 使用率。
    ///
    /// 复用 `SessionStore::load_history`（与 turn 同样的 max_history / max_history_bytes 裁剪窗口）
    /// 与 `estimate_request_tokens`（与压缩触发口径一致）。窗口取 `AgentOptions::context_length`，
    /// `0`（未声明）按 `COMPRESSION_DEFAULT_ASSUMED_CONTEXT_LENGTH`（128K）假设。
    ///
    /// # 两条口径说明（避免 /context 显示与系统实际行为产生误解）
    ///
    /// 1. **「历史估算」不含系统提示 / 工具 schema**：它只用于判断旧会话本身体量。
    ///    自动压缩按「请求压力估算」触发，后者包含历史、system prompt 与工具 schema。
    /// 2. **窗口取启动时的 `context_length`**：与运行期压缩系统实际使用的窗口一致（二者都来自
    ///    `AgentOptions::context_length`，启动期固定）。`/llmodel` 中途切模型的 `context_length`
    ///    只持久化到 rclaw.toml、下次启动才重派生，故 /context 显示的窗口在当前会话内不会随
    ///    `/llmodel` 变动——这与压缩预算「启动期固定」的行为对齐，并不自相矛盾。
    fn handle_context(&self, channel: &str, chat_id: &str) -> String {
        // Bot Mode：bot 会话的 /context 用 bot 自己的提示与记忆口径估算。
        let persona = self.bots.persona_for_chat(channel, chat_id);
        let turn_memory = persona
            .as_ref()
            .map(|p| p.memory.clone())
            .unwrap_or_else(|| self.memory.clone());
        let history = match self.session.load_history(
            channel,
            chat_id,
            self.opts.max_history,
            self.opts.max_history_bytes,
        ) {
            Ok(msgs) => msgs,
            Err(e) => return format!("读取会话历史失败：{e}"),
        };
        let history_used = estimate_history_tokens(&history);
        let system = match &persona {
            Some(p) => (*p.base_prompt).clone(),
            None => (*self.base_system_prompt).clone(),
        };
        let status = Run::new_with_base(
            "检查当前上下文占用",
            self.opts.max_tool_iter,
            &self.layout.base,
        );
        let mut request_messages = history.clone();
        // /context 只做一次用量估算：一次性目录视图，等同改造前的按次扫描。
        let catalog = SkillCatalog::new(&self.layout);
        request_messages.push(context::inference_tail_message(
            &self.layout,
            &turn_memory,
            self.opts.recent_days,
            &catalog,
            channel,
            chat_id,
            false,
            &self.channels,
            self.opts.daemon,
            &status,
        ));
        let request_used = self.history.anchored_estimate(
            &TurnKey {
                channel: channel.to_string(),
                chat_id: chat_id.to_string(),
                cron_job_id: None,
            },
            &system,
            &request_messages,
            self.tools.schemas(),
        );
        render_context_usage(history_used, request_used, &self.opts)
    }

    fn handle_skills_command(&self, content: &str) -> String {
        let mut parts = content.split_whitespace();
        let _command = parts.next();
        let action = parts.next().unwrap_or("list").to_ascii_lowercase();
        let name = parts.next();
        let mutating = matches!(
            action.as_str(),
            "pin" | "unpin" | "adopt" | "restore" | "curate"
        );
        // 静默门替代原 active-map 抱锁：关门 = 无 turn / review 在跑，且持门期间
        // 新 turn 无法启动（与 spawn_turn 互斥）。无 await，仅短同步生命周期操作。
        let _quiescent_guard = if mutating {
            match self.gate.try_acquire() {
                Some(guard) => Some(guard),
                None => {
                    return "Skill 生命周期操作已中止：仍有前台任务或后台 review 在运行，请等待其结束后重试。"
                        .to_string()
                }
            }
        } else {
            None
        };
        let curator = self.reviews.skill_curator();
        let store = curator.store();
        match action.as_str() {
            "list" => render_skills(&self.layout),
            "help" => "用法：/skills [list|status [name]|pin <name>|unpin <name>|adopt <name>|restore <name>|curate]。\nadopt 会把 user-owned Skill 明确转为 agent-owned；只有 agent-owned 且未 pinned 的 Skill 会被后台自动维护。".to_string(),
            "status" => match store.records() {
                Ok(records) if records.is_empty() => name.map_or_else(
                    || "当前没有 Skill 生命周期记录。".to_string(),
                    |name| format!("没有名为 '{name}' 的 Skill 生命周期记录。"),
                ),
                Ok(records) => {
                    let mut lines = vec!["Skill 生命周期状态：".to_string()];
                    for (skill_name, record) in records {
                        if name.is_some_and(|requested| requested != skill_name) {
                            continue;
                        }
                        lines.push(format!(
                            "- {skill_name}: owner={}, state={}, pinned={}, use/view/patch={}/{}/{}, last_used={}, last_viewed={}, last_patched={}, last_lifecycle={}",
                            record.owner.map(|owner| owner.to_string()).unwrap_or_else(|| "unknown".to_string()),
                            record.state,
                            record.pinned,
                            record.use_count,
                            record.view_count,
                            record.patch_count,
                            record.last_used_at.as_deref().unwrap_or("never"),
                            record.last_viewed_at.as_deref().unwrap_or("never"),
                            record.last_patched_at.as_deref().unwrap_or("never"),
                            record.last_lifecycle_at.as_deref().unwrap_or("never"),
                        ));
                    }
                    if lines.len() == 1 && name.is_some() {
                        format!("没有名为 '{}' 的 Skill 生命周期记录。", name.unwrap_or(""))
                    } else {
                        lines.join("\n")
                    }
                }
                Err(error) => format!("读取 Skill 生命周期状态失败：{error}"),
            },
            "pin" | "unpin" => {
                let Some(name) = name else {
                    return format!("用法：/skills {action} <name>");
                };
                if let Err(error) = store.live_skill_dir(name) {
                    return format!("Skill 不存在、不可寻址或不是普通目录：{name}（{error}）");
                }
                let pinned = action == "pin";
                match store.set_pinned(name, pinned) {
                    Ok(()) => format!("Skill '{name}' 已{}。", if pinned { "固定" } else { "取消固定" }),
                    Err(error) => format!("更新 Skill pin 状态失败：{error}"),
                }
            }
            "adopt" => {
                let Some(name) = name else {
                    return "用法：/skills adopt <name>".to_string();
                };
                if let Err(error) = store.live_skill_dir(name) {
                    return format!("Skill 不存在、不可寻址或不是普通目录：{name}（{error}）");
                }
                match store.adopt(name) {
                    Ok(()) => format!("Skill '{name}' 已转为 agent-owned，可由后台自进化维护。"),
                    Err(error) => format!("Skill adopt 失败：{error}"),
                }
            }
            "restore" => {
                let Some(name) = name else {
                    return "用法：/skills restore <name>".to_string();
                };
                match curator.restore(name) {
                    Ok(path) => format!("Skill '{name}' 已恢复：{}", path.display()),
                    Err(error) => format!("Skill restore 失败：{error}"),
                }
            }
            "curate" => match curator.run_now() {
                Ok(report) => {
                    let mut output = report.summary();
                    if !report.errors.is_empty() {
                        output.push_str("\n错误：\n- ");
                        output.push_str(&report.errors.join("\n- "));
                    }
                    output
                }
                Err(error) => format!("Skill Curator 运行失败：{error}"),
            },
            other => format!("未知 /skills 子命令：{other}。输入 /skills help 查看用法。"),
        }
    }

    /// `/clear`：清空当前会话 + 删除 agent/memory、agent/sessions 下全部文件（不可逆）。
    ///
    /// 四道闸（按序）：
    /// 1. `/clear confirm`（或 `/clear yes`）二次确认——未确认只回显将删除的内容；
    /// 2. 静默门确认没有活动 turn——否则它会在清空后把已加载的旧历史/记忆 append 或 rewrite
    ///    写回，造成「已清空但状态复活」；
    /// 3. 静默门确认没有后台 review 在途——auto_memory/auto_evolve 触发的 review 是 detach
    ///    任务、不进 `active`；若不拦，它会基于清空前的 messages 把记忆写回（P2）；
    /// 4. [`Self::clear_agent_state`] 的 symlink/junction 安全闸 + 实际删除。
    fn handle_clear(&self, content: &str) -> String {
        if !clear_confirmed(content) {
            return "⚠️ /clear 会【永久删除】以下内容，不可恢复：\n\
                    • 全部会话历史（agent/sessions/，含当前会话）\n\
                    • 全部长期记忆（agent/memory/：MEMORY.md、daily/、session_summaries/）\n\
                    \n确认请输入：/clear confirm"
                .to_string();
        }
        // 静默门关门：拿到 = 无 turn / review 在跑，且持门期间新 turn 无法启动，
        // 与删除动作之间不存在竞态窗口（C7 替代原 check-then-act）。
        let Some(_quiescent) = self.gate.try_acquire() else {
            let (turns, reviews) = self.gate.running();
            if turns > 0 {
                return "⏳ 有任务正在运行，/clear 已中止：清空后该 turn 会把旧历史 / 记忆写回，\
                        导致「已清空但状态复活」。\n请先 /cancel 或等其结束后重试：/clear confirm"
                    .to_string();
            }
            if reviews > 0 {
                return "⏳ 后台记忆 / 自进化 review 仍在运行，/clear 已中止：它会基于清空前的对话把记忆写回。\n\
                        请稍后重试：/clear confirm"
                    .to_string();
            }
            return "⏳ 系统正在执行清理 / 维护操作，请稍后重试：/clear confirm".to_string();
        };
        match self.clear_agent_state() {
            Ok(summary) => format!("✅ 已清空 agent 本地状态：\n{summary}"),
            Err(e) => format!("⚠️ 清空失败（可能部分文件被占用，可重试 /clear confirm）：{e}"),
        }
    }

    /// 删除 sessions/ 与 memory/ 目录下的全部内容，返回人可读的清理摘要。
    ///
    /// 复用 [`rclaw_store::fs::clear_dir_contents`]（保留目录本身，删除文件 + 子目录）。
    /// 删除后系统自愈：`read_long_term` 对缺失 MEMORY.md 返回空、写入路径经 `ensure_parent_dir`
    /// 自动重建所需文件，故无需手动 re-init。遇到首个无法删除的条目即返回错误（已删的保持已删）。
    ///
    /// 先经 [`ensure_dir_is_real`] 安全闸：拒绝 sessions/memory 是 reparse point（symlink/junction），
    /// 即便它指向 agent base 之内（防 memory → skills 这类 junction 把目标目录一并清掉）。
    fn clear_agent_state(&self) -> std::result::Result<String, String> {
        ensure_dir_is_real(&self.layout.session_dir, &self.layout.base)?;
        ensure_dir_is_real(&self.layout.memory_dir, &self.layout.base)?;
        let sessions = rclaw_store::fs::clear_dir_contents(&self.layout.session_dir)
            .map_err(|e| e.to_string())?;
        let memory = rclaw_store::fs::clear_dir_contents(&self.layout.memory_dir)
            .map_err(|e| e.to_string())?;
        Ok(format!(
            "• sessions/：删除 {sessions} 个会话文件（含当前会话）\n\
             • memory/：删除 {memory} 个顶层条目（MEMORY.md、daily/、session_summaries/）\n\
             \n记忆与历史已重置；下一次写入会自动重建所需文件。"
        ))
    }

    /// `/log`：查看 / 发送 / 删除 agent/logs 下的日志文件。
    ///
    /// 子命令：
    /// - 无参 / `list`：列出日志文件的数量、名称与大小；
    /// - `get <name>`：CLI 回显日志内容（超 20KB 仅显示末尾）；支持媒体的外部 channel 以【文件】形式发送；
    /// - `del <name>`：删除单个日志；
    /// - `del all confirm`：删除全部日志（需二次确认）。
    ///
    /// 仅在 agent/logs 内操作，文件名不得含路径分隔符或 `..`，防目录穿越。
    /// 删除当日正在写入的日志后，下一次日志事件会自动重建文件（见 logging.rs 的
    /// `reopen_if_missing`）。
    fn handle_log(&self, channel: &str, content: &str) -> String {
        let mut parts = content
            .trim()
            .strip_prefix('/')
            .unwrap_or("")
            .split_whitespace();
        parts.next(); // 跳过 "log"
        let sub = parts.next().unwrap_or("");
        match sub.to_ascii_lowercase().as_str() {
            "" | "list" | "ls" => self.log_list(),
            "get" | "cat" | "show" => self.log_get(channel, parts.next().unwrap_or("")),
            "del" | "delete" | "rm" | "remove" => {
                let target = parts.next().unwrap_or("");
                let extra = parts.next().unwrap_or("");
                if target.eq_ignore_ascii_case("all") {
                    if extra.eq_ignore_ascii_case("confirm") || extra.eq_ignore_ascii_case("yes") {
                        self.log_delete_all()
                    } else {
                        "⚠️ 将删除 agent/logs 下【全部】日志文件，不可恢复。\n确认请输入：/log del all confirm"
                            .to_string()
                    }
                } else {
                    self.log_delete_one(target)
                }
            }
            _ => log_usage(),
        }
    }

    fn log_list(&self) -> String {
        let dir = &self.layout.logs_dir;
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return format!("日志目录 {} 不存在（尚未产生日志）。", dir.display());
            }
            Err(e) => return format!("读取日志目录失败：{e}"),
        };
        let mut files: Vec<(String, u64)> = Vec::new();
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                let name = entry.file_name().to_string_lossy().into_owned();
                files.push((name, size));
            }
        }
        if files.is_empty() {
            return "当前没有日志文件。".to_string();
        }
        files.sort_by(|a, b| a.0.cmp(&b.0));
        let total: u64 = files.iter().map(|(_, size)| size).sum();
        let mut out = format!(
            "日志文件（共 {} 个，合计 {}）：\n",
            files.len(),
            format_bytes(total)
        );
        for (name, size) in &files {
            out.push_str(&format!("• {name} ({})\n", format_bytes(*size)));
        }
        out.push_str("\n用法：/log get <名称> 查看；/log del <名称> 删除单个；/log del all confirm 删除全部。");
        out
    }

    fn log_get(&self, channel: &str, name: &str) -> String {
        const MAX_BYTES: usize = 20_000;
        let path = match self.log_resolve(name) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let file_name = path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = match std::fs::metadata(&path) {
            Ok(m) => m.len(),
            Err(e) => return format!("读取日志失败：{e}"),
        };

        // 支持媒体的外部 channel：把日志复制进共享沙箱并按真实来源生成 marker。
        // enabled 只表示媒体能力，不选择其它目录；CLI/文本 channel 走下方文本回显。
        if media_channel_available(channel, &self.channels) {
            let media_dir = self.channels.media_dir.as_deref().map(PathBuf::from);
            if let Some(marker) = media_dir
                .as_ref()
                .and_then(|dir| log_media_marker(channel, dir, &path, &file_name, size))
            {
                return format!("📄 已准备日志附件 {file_name}（{}）。", format_bytes(size))
                    + "\n"
                    + &marker;
            }
            // media 目录不可用或复制失败 → 回退到文本回显。
        }

        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return format!("读取日志失败：{e}"),
        };
        if bytes.len() <= MAX_BYTES {
            let body = String::from_utf8_lossy(&bytes);
            format!("📄 {file_name}（{}）：\n\n{}", format_bytes(size), body)
        } else {
            let tail = &bytes[bytes.len() - MAX_BYTES..];
            let body = String::from_utf8_lossy(tail);
            format!(
                "📄 {file_name}（{}，仅显示末尾 {}）：\n\n…(前略)…\n{}",
                format_bytes(size),
                format_bytes(MAX_BYTES as u64),
                body
            )
        }
    }

    fn log_delete_one(&self, name: &str) -> String {
        if name.is_empty() {
            return "用法：/log del <日志文件名>（名称见 /log）".to_string();
        }
        let path = match self.log_resolve(name) {
            Ok(p) => p,
            Err(e) => return e,
        };
        if !path.exists() {
            return format!("日志文件不存在：{name}");
        }
        match std::fs::remove_file(&path) {
            Ok(()) => format!("✅ 已删除日志：{name}"),
            Err(e) => format!("删除失败：{e}"),
        }
    }

    fn log_delete_all(&self) -> String {
        let dir = &self.layout.logs_dir;
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return "日志目录不存在，无需删除。".to_string();
            }
            Err(e) => return format!("读取日志目录失败：{e}"),
        };
        let mut deleted = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            match std::fs::remove_file(&path) {
                Ok(()) => deleted += 1,
                Err(_) => failed.push(name),
            }
        }
        let mut out = format!("✅ 已删除 {deleted} 个日志文件。");
        if !failed.is_empty() {
            out.push_str(&format!(
                "\n⚠️ 以下 {} 个未能删除（可能被占用）：{}",
                failed.len(),
                failed.join("、")
            ));
        }
        out
    }

    /// 把日志文件名解析到 logs_dir 内的路径，拒绝目录穿越（分隔符 / `..` / 绝对路径）。
    fn log_resolve(&self, name: &str) -> std::result::Result<PathBuf, String> {
        resolve_log_name(&self.layout.logs_dir, name)
    }

    /// `/cancel` 的路由层处理。
    ///
    /// - 有 running turn：触发同一 `(channel, chat_id)` 下全部 `CancellationToken`，并立即回送
    ///   「已请求停止」。active 条目必须保留到各 turn 真正退出时再自移除。
    /// - 无 running turn：直接回送「当前没有正在运行的任务」，不创建新 turn、不写 session。
    async fn route_cancel(self: &Arc<Self>, key: TurnKey) {
        let (matched, all_already_cancelled) =
            self.active_cancel_session(&key.channel, &key.chat_id);
        match matched {
            0 => {
                let _ = self
                    .outbound_tx
                    .send(BusMessage::new(
                        key.channel,
                        key.chat_id,
                        "当前没有正在运行的任务。".to_string(),
                    ))
                    .await;
            }
            count => {
                let response = if count == 1 {
                    if all_already_cancelled {
                        "已请求停止当前任务，请稍候。".to_string()
                    } else {
                        "已请求停止当前任务。".to_string()
                    }
                } else if all_already_cancelled {
                    format!("已请求停止当前会话的 {count} 个任务，请稍候。")
                } else {
                    format!("已请求停止当前会话的 {count} 个任务。")
                };
                let _ = self
                    .outbound_tx
                    .send(BusMessage::new(key.channel, key.chat_id, response))
                    .await;
            }
        }
    }

    /// 为「无活动 turn」的会话启动一个新 turn 任务（**同步**：见上方 active map 访问说明）。
    ///
    /// 同步持锁完成 `check + spawn + insert`（spawn 本身不 await），保证 turn 任务的自移除
    /// 不会抢在我们插入前把条目删掉，避免「插入已完成 turn 的僵尸条目」的窄窗口竞争。
    /// 返回 false 表示静默门被生命周期操作持有、无法启动（调用方回送提示）。
    fn spawn_turn(self: &Arc<Self>, msg: BusMessage) -> bool {
        if !self.gate.try_begin_turn() {
            return false;
        }
        let key = TurnKey::from_msg(&msg);

        loop {
            let mut active = self.active.lock().expect("active map 锁未被毒化");
            // 双重检查：事件通道关闭重路由等路径可能已在极短时间内建出同 key turn。
            if let Some(existing) = active.get(&key) {
                let tx = existing.event_tx.clone();
                drop(active);
                match tx.try_send(Interjection::AppendMessage(msg.clone())) {
                    Ok(()) => {
                        // 未真正启动新 turn：释放本次占用的槽位。
                        self.gate.end_turn();
                        return true;
                    }
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::warn!(
                            "turn 插队队列已满，丢弃补充输入 ({}:{})",
                            key.channel,
                            key.chat_id
                        );
                        self.gate.end_turn();
                        return true;
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        if self.active_remove_if_same_event_tx(&key, &tx) {
                            tracing::debug!(
                                "spawn_turn 发现关闭的 active 事件通道，已移除僵尸条目后重试 ({}:{})",
                                key.channel,
                                key.chat_id
                            );
                            continue;
                        }
                        // 条目已被替换，重试投递到最新 active。
                        continue;
                    }
                }
            }

            let cancel = CancellationToken::new();
            let (event_tx, event_rx) = mpsc::channel::<Interjection>(32);
            let mgr = self.clone();
            let cancel_for_task = cancel.clone();
            let event_tx_for_task = event_tx.clone();
            // turn 任务的真正执行
            // spawn 返回的 JoinHandle 立即 drop：tokio spawned 任务在句柄 drop 时继续运行
            // （detach），取消改由 `cancel` 控制，故无需保存句柄。
            tokio::spawn(async move {
                mgr.run_turn(msg, cancel_for_task, event_rx, event_tx_for_task)
                    .await;
            });
            active.insert(key, ActiveTurn { cancel, event_tx });
            return true;
        }
    }

    /// 测试 / 观测用：是否所有会话都没有活动 turn（review 不计入——它不阻塞下一轮 turn）。
    pub fn is_idle(&self) -> bool {
        self.gate.running().0 == 0
    }

    /// 测试 / 观测用：等待直到前台 turn 槽位全部释放，替代轮询。
    pub async fn wait_turns_idle(&self) {
        self.gate.wait_turns_idle().await;
    }

    /// 测试 / 观测用：等待直到完全静默（turn 与 review 槽位全部释放）。
    pub async fn wait_quiescent(&self) {
        self.gate.wait_quiescent().await;
    }

    /// turn 任务的「外壳」：负责无论 turn 如何结束（正常 / 取消 / panic）都从 active map
    /// 移除自己，并把残留插队输入重新路由为新 turn（避免丢失用户输入）。
    ///
    /// 把主体放进子任务再 await 其 JoinHandle：即使主体 panic，本任务也能继续做清理，
    /// 不会在 active map 留下永远不会被移除的僵尸条目。
    async fn run_turn(
        self: Arc<Self>,
        msg: BusMessage,
        cancel: CancellationToken,
        event_rx: mpsc::Receiver<Interjection>,
        event_tx: mpsc::Sender<Interjection>,
    ) {
        let key = TurnKey::from_msg(&msg);

        // Bot Mode：bot 会话（bot-<name> / grp-<gid>-<member>）解析 persona；
        // 未命中（含全部普通会话）走主 persona，零行为差异。
        let persona = self.bots.persona_for_chat(&msg.channel, &msg.chat_id);
        let bot_ctx = crate::bots::BotTurnCtx {
            bot_name: persona.as_ref().map(|p| p.name.clone()),
            identity_name: persona
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| "RClaw".to_string()),
            identity_display: persona
                .as_ref()
                .map(|p| p.def.title.clone())
                .unwrap_or_else(|| "RClaw".to_string()),
            chat_key: crate::bots::chat_key(&msg.channel, &msg.chat_id),
            chain: msg.agent_chain().to_vec(),
            roster: self.bots.roster(),
        };
        // 每 bot 独立记忆：memory 工具经 task-local 路由到 bot 自己的 MemoryStore。
        let persona_memory = persona.as_ref().map(|p| p.memory.clone());

        // 子任务跑真正的 turn 主体，返回「未处理的插队输入」。
        let mgr = self.clone();
        let panic_report_target = msg.clone();
        let is_user_turn = msg.source() == MessageSource::User;
        // 取出当前消息的工具沙箱权限（napcat max QQ = Unrestricted），用 task-local 注入子任务：
        // per-task 隔离避免并发 turn 串档；读不到（如后台 review fork）默认回退 Sandboxed。
        let access = msg.tool_access();
        // Layer 2a（ZCode 移植）：顶层 turn 的取消令牌进 task-local，供前台子代理
        // 自动转后台建立「父取消 → 子取消」联动（detach 后由调用方解除）。
        let cancel_for_scope = cancel.clone();
        let chat_scope = Some((panic_report_target.channel.clone(), panic_report_target.chat_id.clone()));
        let inner: JoinHandle<Vec<BusMessage>> = tokio::spawn(async move {
            let fut = rewind::CURRENT_CHAT.scope(
                chat_scope,
                rclaw_tools::turn_state::scope_top_level(
                    persona_memory,
                    Some(cancel_for_scope),
                    rclaw_tools::access::scope_access(
                        access,
                        mgr.run_turn_inner(msg, cancel, event_rx, persona),
                    ),
                ),
            );
            crate::bots::scope_bot_ctx(bot_ctx, fut).await
        });
        let leftovers = match inner.await {
            Ok(v) => v,
            Err(join_err) => {
                tracing::error!("turn 任务异常结束（panic）: {join_err}");
                // 交接等待方不能一直挂到超时：panic 也算一次失败回执（仅 Agent 来源）。
                if let Some(router) = &self.bot_router {
                    router.notify(
                        &panic_report_target.channel,
                        &panic_report_target.chat_id,
                        panic_report_target.handoff_id(),
                        crate::bots::HandoffOutcome::Failed("任务异常终止：内部执行错误。".into()),
                    );
                }
                if panic_report_target.is_cron_task() {
                    let _ = self
                        .outbound_tx
                        .send(BusMessage::new(
                            panic_report_target.channel.clone(),
                            panic_report_target.chat_id.clone(),
                            cron_report(
                                &panic_report_target,
                                "任务异常终止：内部执行错误。",
                                false,
                            ),
                        ))
                        .await;
                }
                Vec::new()
            }
        };

        // 从 active 移除自己（turn 已结束）。若旧 active 已被路由层清理并替换成新 turn，
        // 这里不能按 key 无条件删除，否则会误删新 turn，破坏 per-session 单写者约束。
        if !self.active_remove_if_same_event_tx(&key, &event_tx) {
            tracing::debug!(
                "turn 结束清理时 active 已不存在或已被替换 ({}:{})",
                key.channel,
                key.chat_id
            );
        }
        // 无论 active 条目如何结束，本 turn 的静默门槽位必须释放。
        self.gate.end_turn();

        // 残留插队输入：按到达顺序重新路由为新 turn，确保用户输入不丢失
        // （例如补充正好在本 turn 收尾时到达、未被安全点处理到）。
        for leftover in leftovers {
            self.route(leftover).await;
        }

        // Curate only after the user turn has refreshed usage telemetry and any already-queued
        // leftovers have been re-routed. If a leftover starts another turn, the idle guard skips
        // this pass so the Curator cannot archive the exact dormant Skill that message will use.
        if is_user_turn {
            self.reviews.curate_if_idle();
        }
    }

    /// 构造轮内压缩上下文（`in_turn` 模块的共享环境）。
    ///
    /// `key` 由调用方持有（msg 的 TurnKey 是确定性派生，turn 生命周期内不变）；
    /// `schemas` 由调用方按当前快照收集（静态面 + 动态激活面）——每个迭代重建一次，
    /// 供 anchored 估算在锚点失配（压缩重写历史）时回退纯启发式使用。
    fn in_turn_ctx<'a>(
        &'a self,
        key: &'a TurnKey,
        msg: &'a BusMessage,
        cancel: &'a CancellationToken,
        system: &'a str,
        schemas: &'a [ToolSchema],
        sink: &'a crate::turn::history::CompressionSink,
    ) -> in_turn::InTurnContext<'a> {
        in_turn::InTurnContext {
            session: &self.session,
            history: &self.history,
            opts: &self.opts,
            key,
            msg,
            cancel,
            system,
            schemas,
            sink,
        }
    }

    /// turn 的真正主体，叠加取消与插队
    ///
    /// `persona`：Bot Mode 会话（bot-… / grp-…）解析出的 bot persona；None = 主 persona。
    /// 返回值：未被安全点处理掉的插队补充文本（交给上层重新路由）。
    async fn run_turn_inner(
        self: Arc<Self>,
        msg: BusMessage,
        cancel: CancellationToken,
        mut event_rx: mpsc::Receiver<Interjection>,
        persona: Option<Arc<crate::bots::BotPersona>>,
    ) -> Vec<BusMessage> {
        let is_cron = msg.is_cron_task();
        let cli = msg.channel == CHANNEL_CLI;
        tracing::info!(
            "处理消息来自 {}:{} (cron={is_cron}, bot={})",
            msg.channel,
            msg.chat_id,
            persona.as_ref().map(|p| p.name.as_str()).unwrap_or("")
        );

        // Bot Mode：bot turn 就地替换基础提示 / 记忆注入 / provider（会话、技能目录、
        // 文件沙箱共享主布局）。turn_memory 同时供 memory 工具的 task-local 路由使用
        // 已在 run_turn 外壳完成，这里只影响注入侧。
        let turn_memory = persona
            .as_ref()
            .map(|p| p.memory.clone())
            .unwrap_or_else(|| self.memory.clone());
        let turn_provider = persona
            .as_ref()
            .and_then(|p| p.provider.clone())
            .unwrap_or_else(|| self.provider.clone());

        // 静态 schema 只读；共享循环状态（含动态 Manifest 工具集）由 TurnState 拥有。
        // 工具面 = 注册表全量滤除用户禁用项（桌面版禁能开关从下一 turn 生效）；
        // 再滤除 /search dis 的 backend 直连工具（控制面执行闸之外的工具面同步）；
        // Bot Mode：再滤除该 bot 定义里 denied_tools 收窄的工具。
        let mut static_schema: Vec<ToolSchema> = self.tools.request_schemas();
        if let Some(control) = self.slash.as_ref().and_then(|s| s.web_search_control()) {
            static_schema.retain(|schema| {
                context::search_tool_backend(&schema.name)
                    .map(|backend| control.is_enabled(backend))
                    .unwrap_or(true)
            });
        }
        if let Some(denied) = persona.as_ref().map(|p| p.def.denied_tools.as_slice()) {
            if !denied.is_empty() {
                static_schema.retain(|schema| !denied.contains(&schema.name));
            }
        }
        let mut state = TurnState::new(&static_schema);
        // skills 目录扫描每 turn 一次：激活检测与动态上下文共享该缓存。
        let catalog = SkillCatalog::new(&self.layout);

        // 1) system 只使用启动期稳定快照。动态上下文作为末尾非持久化消息注入。
        // Bot Mode：bot 会话使用 bot 专属基础提示（身份 + persona + 协作协议）。
        let system = match &persona {
            Some(p) => (*p.base_prompt).clone(),
            None => (*self.base_system_prompt).clone(),
        };
        let mut run = Run::new_with_base(&msg.content, self.opts.max_tool_iter, &self.layout.base);
        let preflight_message = context::inference_tail_message(
            &self.layout,
            &turn_memory,
            self.opts.recent_days,
            &catalog,
            &msg.channel,
            &msg.chat_id,
            is_cron,
            &self.channels,
            self.opts.daemon,
            &run,
        );

        // 2) 加载历史（cron 任务不带历史）。压缩对各会话一律启用：bot 会话的
        // 摘要写进 bot 自己的记忆、用 bot 固定的 provider 生成（ADR-0006，
        // 对齐 Hermes「Bot Chat / 群成员会话即普通会话，同一压缩机制」）。
        let compression = crate::turn::history::CompressionSink {
            memory: turn_memory.clone(),
            provider: turn_provider.clone(),
        };
        // 轮内压缩（ZCode 移植 · Layer 1）复用同一 sink 构造：load 消费上一份，
        // 这份供 ReAct 循环内的 auto / reactive compact 使用。
        let in_turn_sink = crate::turn::history::CompressionSink {
            memory: turn_memory.clone(),
            provider: turn_provider.clone(),
        };
        let mut messages: Vec<Message> = if is_cron {
            Vec::new()
        } else {
            match self
                .history
                .load(
                    &msg,
                    &cancel,
                    &system,
                    &static_schema,
                    Some(&preflight_message),
                    Some(compression),
                )
                .await
            {
                Ok(HistoryLoad::Messages(m)) => m,
                Ok(HistoryLoad::Cancelled) => {
                    tracing::info!(
                        "turn 在会话压缩阶段被取消 ({}:{})",
                        msg.channel,
                        msg.chat_id
                    );
                    // 交接等待方按「取消」收尾，而不是挂到超时。
                    if let Some(router) = &self.bot_router {
                        router.notify(
                            &msg.channel,
                            &msg.chat_id,
                            msg.handoff_id(),
                            crate::bots::HandoffOutcome::Cancelled,
                        );
                    }
                    return drain_leftovers(&mut event_rx);
                }
                Err(e) => {
                    // 历史加载失败：回复错误，不进入 ReAct。交接等待方按失败收尾
                    // （Agent 消息的 turn 也会走到这里），而不是挂到超时。
                    let _ = self
                        .outbound_tx
                        .send(BusMessage::new(
                            msg.channel.clone(),
                            msg.chat_id.clone(),
                            format!("加载会话历史失败: {e}"),
                        ))
                        .await;
                    if let Some(router) = &self.bot_router {
                        router.notify(
                            &msg.channel,
                            &msg.chat_id,
                            msg.handoff_id(),
                            crate::bots::HandoffOutcome::Failed(format!("加载会话历史失败: {e}")),
                        );
                    }
                    return drain_leftovers(&mut event_rx);
                }
            }
        };

        // 3) 当前 user 消息：先持久化（cron 不持久化），再追加进本轮内存 messages。
        //    user 落盘失败必须中止本轮：否则后续 assistant / tool 记录仍可能被保存，
        //    历史会变成「只有回答没有问题」。
        let user_msg = Message::plain(Role::User, msg.content.clone());
        if !is_cron {
            if let Err(e) = self.session.append(&msg.channel, &msg.chat_id, &user_msg) {
                tracing::error!("user 消息持久化失败，中止本轮以避免历史残缺: {e}");
                let _ = self
                    .outbound_tx
                    .send(BusMessage::new(
                        msg.channel.clone(),
                        msg.chat_id.clone(),
                        // 详细错误（与「加载会话历史失败」同风格）：含 StoreError 的 IO/超限/路径原因。
                        format!("保存消息失败: {e}"),
                    ))
                    .await;
                if let Some(router) = &self.bot_router {
                    router.notify(
                        &msg.channel,
                        &msg.chat_id,
                        msg.handoff_id(),
                        crate::bots::HandoffOutcome::Failed(format!("保存消息失败: {e}")),
                    );
                }
                return drain_leftovers(&mut event_rx);
            }
        }
        messages.push(user_msg);

        // ---- 轮内上下文管理（ZCode 移植 · Layer 1）----
        // turn_start = 本轮 user 消息的下标：[..turn_start] 是加载期历史（与磁盘一致），
        // [turn_start..] 是本轮追加。ephemeral 跟踪本轮【不落盘】的临时上下文消息
        // （skill_context / 媒体附件），轮内压缩 rewrite session 时剔除、内存保留。
        let mut turn_start = messages.len();
        let mut ephemeral: Vec<usize> = Vec::new();
        // 轮内压缩共享的会话键（msg 的确定性派生，turn 生命周期内不变）。
        let turn_key = TurnKey::from_msg(&msg);

        // ---- ReAct 循环状态 ----
        // 共享状态袋（动态工具集、包级 skill 表、公告去重、媒体三件套、scope 标志、
        // skill_manage 粘性标志）全部归 TurnState；此处只留循环私有控制变量与轮次结果。
        let mut had_tool_iter = false;
        let mut react_iters = 0u32;
        let mut final_text: Option<String> = None;
        let mut final_reasoning: Option<String> = None;
        let mut completed_successfully = false;
        // 是否因 /cancel 而中止（决定走「取消清理」还是「正常收尾」）。
        let mut cancelled = false;
        // LLM 调用最终失败时携带的详细错误文案（重试耗尽 / 流式中途异常终止）。
        // None 表示没有 LLM 错误（成功，或成功但返回空文本）。
        let mut llm_error: Option<String> = None;
        // B1：工具执行期间被 /cancel 打断时，记录尚未持久化的 assistant(tool_calls)，
        // 清理阶段连同 aborted 占位结果一起落盘（ADR-0004），让模型下轮知道上次
        // 干到哪、哪些调用没有结果。
        let mut interrupted_chain: Option<Message> = None;
        // 非用户插队消息不能注入当前 LLM；保留原始 BusMessage，turn 结束后重路由。
        let mut deferred_messages: Vec<BusMessage> = Vec::new();

        // 全局信号量：限制可同时执行的顶层 turn。等待槽位期间也可被 /cancel 中断。
        // _permit 借用 self.execution_limit 并持有到函数末尾（释放执行槽位）；
        // 其余对 self 各字段的访问都是共享借用，互不冲突。
        let _permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                cancelled = true;
                tracing::info!("turn 在等待执行槽位时被取消 ({}:{})", msg.channel, msg.chat_id);
                None
            }
            p = self.execution_limit.acquire() => Some(p.expect("execution_limit semaphore 不会关闭")),
        };

        if !cancelled {
            // turn 开始：重置工具的 per-turn 状态（如 memory 工具的同 turn 失败熔断计数，
            // 避免计数跨 turn 累积）。
            self.tools.reset_turn_state();
            'react: for iter in 0..self.opts.max_tool_iter {
                react_iters = react_iters.saturating_add(1);
                run.begin_iteration(iter + 1);
                // 安全点 A：迭代顶部——处理「上一轮工具执行期间」到达的补充。
                // 此时 session tail 处于已闭合位置（[当前 user] 或 […完整 tool chain]），
                // 把补充作为独立 user 消息追加是协议安全的。
                if !is_cron {
                    let received_new_input = drain_append_interjections(
                        &self.session,
                        &mut messages,
                        &mut event_rx,
                        &mut deferred_messages,
                    )
                    .await;
                    if received_new_input && !state.validated_media_markers.is_empty() {
                        // marker 只是待随最终回复交给 channel 的发送意图，发布脚本成功并不
                        // 等于已经发送。用户在此期间可能取消、改名或改变交付范围，因此旧
                        // marker 必须失效。保留 seen 集合，避免 AgentStatus/AgentWait 重放
                        // 已完成快照时把旧 marker 复活；真实重发由发布器生成新 delivery id。
                        state.validated_media_markers.clear();
                        tracing::info!("收到发布后的用户补充，已废弃尚未出站的旧媒体 marker");
                    }
                    if cancel.is_cancelled() {
                        cancelled = true;
                        break 'react;
                    }
                }

                // 轮内 auto compact（ZCode 移植 · Layer 1b）：每次迭代检查一次预算。
                // 头区（[..turn_start]）超阈值就地基摘要 + rewrite session；本轮尾区
                // 永远保留，原始任务消息由尾窗锚点保护不被摘要化。估算 / 防抖 / 失败
                // 冷却与 load 期同一套状态。
                if !is_cron && self.opts.in_turn_compression_enabled {
                    let schemas_now: Vec<ToolSchema> = static_schema
                        .iter()
                        .chain(state.dynamic.active_schemas().iter())
                        .cloned()
                        .collect();
                    let ctx =
                        self.in_turn_ctx(&turn_key, &msg, &cancel, &system, &schemas_now, &in_turn_sink);
                    in_turn::maybe_auto_compress(
                        &ctx,
                        &mut messages,
                        &mut turn_start,
                        &mut ephemeral,
                    )
                    .await;
                }

                // 「working」状态：非 CLI / 非 system / 非 weixin / 非 cron，且仅首轮下发一次。
                if !is_cron
                    && self.opts.send_working_status
                    && should_emit_working_status(&msg.channel, &self.channels)
                    && iter == 0
                {
                    let _ = self.outbound_tx.try_send(
                        BusMessage::new(
                            msg.channel.clone(),
                            msg.chat_id.clone(),
                            "🟢 Agent工作中...".to_string(),
                        )
                        .with_intermediate(),
                    );
                }

                let stream_enabled = self.opts.stream_enabled;

                // LLM 调用（cancel-aware + 失败重试 / 指数退避）：
                // 连接失败 / 超时 / 错误响应（含 429、5xx）时最多重试 `llm_max_retries` 次，
                // 退避从 `llm_retry_base_delay` 起、每轮 ×2、上限 `llm_retry_max_delay`；
                // 调用期与退避期都受 /cancel 中断。
                //
                // 流式安全：SSE 片段只由 provider 组装，不在完成验证前展示；失败尝试不会
                // 留下用户可见 partial，因此 retryable 错误可以从全新请求安全重试。
                let max_retries = self.opts.llm_max_retries;
                let retry_base_delay = self.opts.llm_retry_base_delay;
                let retry_max_delay = self.opts.llm_retry_max_delay;
                let mut outcome_opt: Option<rclaw_core::ChatOutcome> = None;
                // 每迭代一次的 reactive compact 预算：超限压缩后若本迭代再次超限，
                // 不再重复压缩（由普通错误路径收尾），避免「压缩-超限」循环。
                let mut reactive_done = false;
                'retry: for attempt in 0..=max_retries {
                    if cancel.is_cancelled() {
                        cancelled = true;
                        break 'retry;
                    }

                    // SSE 片段在完成验证前不得展示给用户，否则一个随后被 result/process gate
                    // 拒绝的“外部动作已完成”声明已经泄露。provider 仍走 streaming 协议并组装
                    // 完整 outcome；通过门禁后统一展示最终文本。
                    let on_text: rclaw_llm::OnText = Arc::new(move |_text: &str| {});
                    // 推理增量流式出站（桌面「💭 思考过程」实时展示）：每片以「💭\n」前缀
                    // 单独推送，前端追加进当前思考块；轮末 emit_napcat_intermediate_if_safe
                    // 的整段 reasoning（过安全检查后）以同格式覆盖，作为权威快照。流式增量
                    // 是实时直播视图——单分片无法做整段安全检查，这是有意的取舍。
                    // 许可按能力位 reasoning_stream 判定：仅桌面声明 true；QQ/微信等通道
                    // 保持原有「按轮整段、过安全检查后转发」的方式，不逐分片出站。
                    let stream_reasoning = !is_cron
                        && self
                            .channels
                            .caps(&msg.channel)
                            .is_some_and(|caps| caps.reasoning_stream);
                    let on_reasoning: rclaw_llm::OnReasoning = {
                        let outbound_tx = self.outbound_tx.clone();
                        let msg = msg.clone();
                        Arc::new(move |delta: &str| {
                            if !stream_reasoning || delta.is_empty() {
                                return;
                            }
                            let _ = outbound_tx.try_send(
                                BusMessage::new(
                                    msg.channel.clone(),
                                    msg.chat_id.clone(),
                                    format!("💭\n{delta}"),
                                )
                                .with_intermediate(),
                            );
                        })
                    };
                    // 每个 retry attempt 都刷新动态上下文与 AgentStatus；上一次错误会出现在
                    // 下一次推理的末尾状态中，时间 / memory / environment 也不会复用旧快照。
                    // 轮内 microcompact（ZCode 移植 · Layer 1a）：请求视图本地剪枝——
                    // 旧媒体剥离、旧 tool result 摘要化、大 tool_use input 裁剪，保护窗口
                    // 内的最近消息原样保留。只改请求视图，不动内存/磁盘（下一轮 load 仍
                    // 能取回完整内容），长 turn 不再把巨大的工具输出一路带进每次请求。
                    let mut request_messages = if self.opts.in_turn_compression_enabled {
                        in_turn::microcompact_view(&messages, &self.opts)
                    } else {
                        messages.clone()
                    };
                    let mut tail_message = context::inference_tail_message(
                        &self.layout,
                        &turn_memory,
                        self.opts.recent_days,
                        &catalog,
                        &msg.channel,
                        &msg.chat_id,
                        is_cron,
                        &self.channels,
                        self.opts.daemon,
                        &run,
                    );
                    // M1：团队邮箱有未读的后台子 Agent 完成通知时，把「存在性」提示注入
                    // 本次推理尾部（非持久化、不唤醒）——父在下一个自然迭代即可看到结果
                    // 已就绪并自行决定何时用 TaskList 查看，不再「干等 or 漏看」二选一。
                    let team_inbox_unread =
                        crate::agent_tools::read_team_inbox_unread(&self.layout.base);
                    if team_inbox_unread > 0 {
                        tail_message.content.push(ContentBlock::text(format!(
                            "\n[System hint] 团队邮箱有 {team_inbox_unread} 条未读的后台子 Agent \
                             完成通知；请在合适的工具调用间隙用 TaskList 查看（不要中断当前工作）。\
                             若本 turn 收尾时仍有未读，系统会发送合批唤醒消息驱动收集轮次；\
                             提前用 TaskList 消费可免除该额外轮次。"
                        )));
                    }
                    request_messages.push(tail_message);
                    let req = ChatRequest {
                        system: Some(system.clone()),
                        messages: request_messages,
                        tools: static_schema
                            .iter()
                            .chain(state.dynamic.active_schemas().iter())
                            .cloned()
                            .collect(),
                        max_tokens: turn_provider.max_tokens(),
                        stream: stream_enabled,
                    };
                    // 克隆 provider Arc 进异步块，避免把整个 self move 进去（后续还要用 self）。
                    // Bot Mode：bot 固定了 provider 时这里就是 bot 的 provider。
                    let provider = turn_provider.clone();

                    let res = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => {
                            cancelled = true;
                            break 'retry;
                        }
                        r = async move {
                            if stream_enabled {
                                provider
                                    .chat_stream_reasoning(req, on_text, on_reasoning)
                                    .await
                            } else {
                                provider.chat(req).await
                            }
                        } => r,
                    };

                    match res {
                        Ok(oc) => {
                            // A1 usage 锚定：每次成功调用都刷新真实用量锚点（后到覆盖）。
                            // 下一轮历史加载用「锚点真实值 + 增量启发式」代替纯启发式，
                            // 避免估算漂移导致自动压缩触发时机失准。
                            if let Some(usage) = &oc.usage {
                                if let Some(anchor_message) = messages.last() {
                                    self.history.record_usage_anchor(
                                        &TurnKey::from_msg(&msg),
                                        anchor_message,
                                        usage.total(),
                                    );
                                }
                            }
                            outcome_opt = Some(oc);
                            break 'retry;
                        }
                        Err(e) => {
                            run.record_llm_error(&e);
                            // 轮内 reactive compact（ZCode 移植 · Layer 1c）：provider 明确
                            // 报上下文超限时（通常 NonRetryable、原样重试无意义），强制
                            // 压缩头区后原地重试一次。压缩失败则按原错误走普通路径。
                            if !reactive_done && in_turn::error_indicates_context_exceeded(&e) {
                                reactive_done = true;
                                let schemas_now: Vec<ToolSchema> = static_schema
                                    .iter()
                                    .chain(state.dynamic.active_schemas().iter())
                                    .cloned()
                                    .collect();
                                let ctx = self.in_turn_ctx(
                                    &turn_key,
                                    &msg,
                                    &cancel,
                                    &system,
                                    &schemas_now,
                                    &in_turn_sink,
                                );
                                if in_turn::reactive_compress(
                                    &ctx,
                                    &mut messages,
                                    &mut turn_start,
                                    &mut ephemeral,
                                )
                                .await
                                {
                                    continue 'retry;
                                }
                            }
                            let attempt_no = attempt + 1;
                            if e.is_retryable() && attempt < max_retries {
                                let backoff = std::cmp::min(
                                    retry_base_delay.saturating_mul(
                                        1u32.checked_shl(attempt).unwrap_or(u32::MAX),
                                    ),
                                    retry_max_delay,
                                );
                                tracing::warn!(
                                    "LLM 调用失败 ({attempt_no}/{}): {e}；{backoff:?} 后重试",
                                    max_retries + 1
                                );
                                // 退避期间同样受 /cancel 中断。
                                tokio::select! {
                                    biased;
                                    _ = cancel.cancelled() => {
                                        cancelled = true;
                                        break 'retry;
                                    }
                                    _ = tokio::time::sleep(backoff) => {}
                                }
                            } else {
                                let class = match e.class() {
                                    LlmErrorClass::Retryable => "retryable",
                                    LlmErrorClass::NonRetryable => "non_retryable",
                                    LlmErrorClass::Protocol => "protocol",
                                };
                                llm_error = Some(if e.is_retryable() {
                                    format!(
                                        "LLM 调用失败（已重试 {max_retries} 次仍无法成功，分类 {class}）：{e}"
                                    )
                                } else {
                                    format!("LLM 调用失败（分类 {class}，原样重试无效）：{e}")
                                });
                                tracing::warn!(
                                    "LLM 调用失败 ({attempt_no}/{}): {e}；分类={class}，放弃本轮",
                                    max_retries + 1
                                );
                                break 'retry;
                            }
                        }
                    }
                }

                let mut outcome = match outcome_opt {
                    Some(o) => o,
                    // 取消（cancelled=true）或 LLM 异常（cancelled=false）→ 统一跳到清理。
                    None => break 'react,
                };

                outcome.text = sanitize_memory_context_text(&outcome.text);
                if let Some(reasoning) = outcome.reasoning.as_mut() {
                    *reasoning = sanitize_memory_context_text(reasoning);
                }

                if !outcome.wants_tools() {
                    // 安全点 C：最终候选在 LLM 生成期间也可能收到用户补充。此时候选尚未
                    // 持久化或出站，可以安全丢弃并用最新输入重算；尤其不能让“不要发了”
                    // 到达后，仍把上一轮已暂存但尚未发送的媒体 marker 交给 channel。
                    if !is_cron
                        && drain_append_interjections(
                            &self.session,
                            &mut messages,
                            &mut event_rx,
                            &mut deferred_messages,
                        )
                        .await
                    {
                        state.validated_media_markers.clear();
                        if cancel.is_cancelled() {
                            cancelled = true;
                            break 'react;
                        }
                        continue 'react;
                    }
                    // 媒体认领闸门 + 完成闸门的统一策略由内核裁决；pending marker
                    // 原地复核（丢弃失效项），返回剥离 marker 后的最终回复。
                    let (finalized_reply, gate) = run.evaluate_final_gate(
                        &outcome.text,
                        outcome.reasoning.as_deref(),
                        &mut state.validated_media_markers,
                        &msg.channel,
                        &self.channels,
                        state.media_publish_attempted,
                    );
                    if gate.accepted() {
                        // 三层验证通过后才允许正常结束。
                        // 最终回复轮：outcome.text 即将作为正式回复出站，此处只发 reasoning。
                        emit_napcat_intermediate_if_safe(
                            &self.outbound_tx,
                            &msg,
                            &outcome,
                            is_cron,
                            false,
                            &self.channels,
                        );
                        final_text = Some(finalized_reply);
                        if had_tool_iter {
                            if let Some(r) = &outcome.reasoning {
                                final_reasoning = Some(r.clone());
                            }
                        }
                        break 'react;
                    }
                    if run.completion_rejections() <= MAX_COMPLETION_GATE_REJECTIONS
                        && iter + 1 < self.opts.max_tool_iter
                    {
                        tracing::warn!(
                            "完成验证拒绝第 {} 次候选回复：{}",
                            run.completion_rejections(),
                            gate.reason_text()
                        );
                        continue 'react;
                    }
                    llm_error = Some(format!("完成验证未通过：{}", gate.reason_text()));
                    break 'react;
                }

                // 工具调用迭代。
                // 工具调用轮：优先发送非空 outcome.text（普通过程消息格式），text 为空或被安全
                // 检查抑制时才回退到 reasoning（「💭 思考中...」格式）。同一轮只发送一次。
                emit_napcat_intermediate_if_safe(
                    &self.outbound_tx,
                    &msg,
                    &outcome,
                    is_cron,
                    true,
                    &self.channels,
                );
                had_tool_iter = true;
                let tool_names = outcome
                    .tool_calls
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::info!(
                    "工具调用迭代 {}：{} 个调用 [{}]",
                    iter + 1,
                    outcome.tool_calls.len(),
                    tool_names
                );

                // 非微信外部通道实时进度：每轮工具迭代推送一次，带工具名。
                // shell 工具：channel 仅显示 command 首行前 60 字符（区分 stock-analyst 各步在跑哪个脚本），
                // 完整 command（脱敏后）走 info 日志落盘，不再全推到 channel。
                // CLI 已在日志/流式输出里看到工具调用，不发 CLI 避免重复。
                if !is_cron
                    && self.opts.send_working_status
                    && should_emit_working_status(&msg.channel, &self.channels)
                    && preflight_skill_manage_batch(&outcome.tool_calls).is_empty()
                    && matches!(
                        run.clone().record_tool_calls(&outcome.tool_calls),
                        LoopDecision::Continue
                    )
                {
                    // 每个工具调用单独推送一条进度（桌面端按行渲染成动作流水）。
                    // shell 预览已是单行截断文本，非 shell 摘要同为单行，均无多行。
                    for call in &outcome.tool_calls {
                        let name = call.name.as_str();
                        let brief = if name == "shell" {
                            let full = serde_json::from_str::<serde_json::Value>(&call.input)
                                .ok()
                                .and_then(|v| {
                                    v.get("command")
                                        .and_then(|c| c.as_str())
                                        .map(redact_sensitive_shell_command)
                                });
                            match full {
                                Some(p) if !p.is_empty() => {
                                    // 完整 command（脱敏后）走 info 日志落盘，不再全推到 channel。
                                    tracing::info!(
                                        "shell 命令全文 (channel={}, chat_id={}, iter={}):\n{}",
                                        msg.channel,
                                        msg.chat_id,
                                        iter + 1,
                                        p
                                    );
                                    // channel 只显示首行的前 60 字符；首行超 60 字符或还有
                                    // 后续行时，用 "..." 表示未显示内容——同时避免媒体标记行
                                    // 被通道解析（只取首行即切断 [:napcat-media 等控制行）。
                                    let first_line = p.lines().next().unwrap_or("");
                                    let mut preview = String::new();
                                    let mut over = false;
                                    for (i, ch) in first_line.chars().enumerate() {
                                        if i < 60 {
                                            preview.push(ch);
                                        } else {
                                            over = true;
                                            break;
                                        }
                                    }
                                    if over || p.contains('\n') {
                                        preview.push_str("...");
                                    }
                                    format!("{name} → {preview}")
                                }
                                _ => name.to_string(),
                            }
                        } else {
                            tool_call_brief(name, &call.input)
                        };
                        let content = format!("正在调用工具({}):{}", iter + 1, brief);
                        let _ = self.outbound_tx.try_send(BusMessage::new(
                            msg.channel.clone(),
                            msg.chat_id.clone(),
                            content,
                        ));
                    }
                }

                // 安全点 B：tool_calls 已返回但尚未执行。若此时有补充到达（流期间累积）：
                //   - 不执行旧 tool_calls；
                //   - 不持久化 assistant(tool_calls)（尚未写入，天然不写）；
                //   - 把补充加入 messages 后 `continue` 重新请求 LLM。
                // 这正是文档「tool_calls 返回后、工具执行前发送补充消息 → 不执行旧 tool_calls、
                // 重新请求 LLM」的要求。
                if !is_cron
                    && drain_append_interjections(
                        &self.session,
                        &mut messages,
                        &mut event_rx,
                        &mut deferred_messages,
                    )
                    .await
                {
                    // 这里已经消费了补充消息，下一轮顶部不会再次看到它；因此必须在
                    // 本安全点同步废弃此前工具留下的待发送 marker。否则用户此时说
                    // “不要发了”，旧 marker 仍会在新回复收尾时被自动附加并真正外发。
                    state.validated_media_markers.clear();
                    continue 'react;
                }

                // 构造 assistant(tool_calls)。若任一 tool_use 入参非法，放弃本轮、不持久化。
                let assistant_msg = match build_assistant_message(&outcome) {
                    Ok(mut m) => {
                        // Layer 5 选点回滚：预分配持久 messageId（纳秒单调，append 保留
                        // 非 0 ts），本迭代 write/edit/delete 的 checkpoint 以它归属。
                        m.ts = rewind::next_message_id();
                        m
                    }
                    Err(e) => {
                        tracing::warn!("工具链入参非法，拒绝持久化本轮: {e}");
                        break 'react;
                    }
                };
                // 只有协议合法且即将实际处理的调用才计入重复指纹；被安全点 B 的新反馈
                // 淘汰的旧调用不属于循环，不应消耗熔断预算。
                let loop_decision = run.record_tool_calls(&outcome.tool_calls);
                if let LoopDecision::Trip {
                    fingerprints,
                    hard_stop,
                } = loop_decision
                {
                    if hard_stop {
                        llm_error = Some(format!(
                            "工具循环熔断：同一调用在改道提示后仍重复出现（{}）。\
                         请换个思路：拆小任务、改用不同工具或参数，或补充必要信息后重试。",
                            fingerprints.join(", ")
                        ));
                        break 'react;
                    }
                    let tool_result = build_loop_circuit_result(&outcome, &fingerprints);
                    run.record_tool_results(&outcome, &tool_result);
                    if !is_cron {
                        if let Err(e) = self.session.append_tool_chain(
                            &msg.channel,
                            &msg.chat_id,
                            &assistant_msg,
                            &tool_result,
                        ) {
                            tracing::error!("循环熔断工具链持久化失败 ({e})，放弃本轮");
                            break 'react;
                        }
                    }
                    messages.push(assistant_msg);
                    messages.push(tool_result);
                    continue 'react;
                }
                // 工具执行（cancel-aware）：取消时 drop `execute_tools` future，
                // 内部 `Shell` 子进程靠 `kill_on_drop(true)` 被 kill；此时 assistant(tool_calls)
                // 尚未落盘，B1 在清理阶段把它与 aborted 占位结果一起补写进历史（ADR-0004）。
                let tool_exec = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => {
                        interrupted_chain = Some(assistant_msg);
                        cancelled = true;
                        break 'react;
                    }
                    e = self.execute_tools(&outcome, &msg, &mut state, &catalog) => e,
                };
                run.set_active_manifest_tools(state.dynamic.active_names());
                if state.media_scope_changed {
                    // TaskList 已在工具内部消费团队补充，下一轮安全点不会再次看到它；
                    // 与用户插队和子 Agent 自动邮箱读取保持一致，旧媒体意图必须失效
                    // （seen 保留在 TurnState：同一完成快照无法把已撤销 marker 重新带回）。
                    state.handle_media_scope_change();
                }
                // 批次新鲜 marker 统一经 seen 去重后认领；顺序与拆分前一致：
                // 先废弃旧意图，再接纳本批 marker。
                for marker in &tool_exec.validated_media_markers {
                    state.accept_validated_marker(marker);
                }
                run.record_tool_results(&outcome, &tool_exec.tool_result);

                // 原子持久化 assistant(tool_calls) + tool_result（单次 write_all）。
                if !is_cron {
                    if let Err(e) = self.session.append_tool_chain(
                        &msg.channel,
                        &msg.chat_id,
                        &assistant_msg,
                        &tool_exec.tool_result,
                    ) {
                        tracing::error!("工具链原子持久化失败 ({e})，放弃本轮");
                        break 'react;
                    }
                }
                // Layer 5：本迭代捕获的文件 checkpoint 以 assistant messageId 落盘。
                rewind::flush(
                    &self.layout,
                    &msg.channel,
                    &msg.chat_id,
                    assistant_msg.ts,
                    std::mem::take(&mut state.pending_file_checkpoints),
                );

                messages.push(assistant_msg);
                messages.push(tool_exec.tool_result);
                // skill_context / 媒体附件是本轮临时上下文（不落盘）：记录下标，
                // 轮内压缩 rewrite session 时剔除、内存保留供模型继续参考。
                if let Some(skill_context_msg) = tool_exec.skill_context_message {
                    messages.push(skill_context_msg);
                    ephemeral.push(messages.len() - 1);
                }
                if let Some(media_msg) = tool_exec.media_message {
                    if turn_provider.supports_multimodal() {
                        messages.push(media_msg);
                        ephemeral.push(messages.len() - 1);
                    } else {
                        tracing::info!("当前模型不支持多模态输入，跳过 read 工具返回的媒体附件");
                        messages.push(multimodal_unsupported_message());
                        ephemeral.push(messages.len() - 1);
                    }
                }
            }
        }

        if !cancelled && final_text.is_none() && llm_error.is_none() {
            llm_error = Some(format!(
                "达到 ReAct 迭代上限 {}，任务未通过完成验证。",
                self.opts.max_tool_iter
            ));
        }

        // ---- 统一清理：取消 / 正常 / 异常在此分叉 ----
        if cancelled {
            // 取消：绝不保存半截 LLM 输出。工具执行被打断时，B1（ADR-0004）把
            // assistant(tool_calls) 与 aborted 占位结果补写为一条【闭合】工具链
            // ——模型下轮知道哪些调用没有结果、不会重复已做的工作；
            // 其余情况不留任何未闭合链。
            if !is_cron {
                if let Some(assistant_msg) = interrupted_chain.take() {
                    rewind::flush(
                        &self.layout,
                        &msg.channel,
                        &msg.chat_id,
                        assistant_msg.ts,
                        std::mem::take(&mut state.pending_file_checkpoints),
                    );
                    let ids = assistant_msg
                        .content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    if !ids.is_empty() {
                        let carrier = rclaw_store::synthesize_aborted_tool_results(&ids);
                        if let Err(e) = self.session.append_tool_chain(
                            &msg.channel,
                            &msg.chat_id,
                            &assistant_msg,
                            &carrier,
                        ) {
                            tracing::warn!("中断工具链持久化失败: {e}");
                        }
                    }
                }
                // 写一条简短 assistant 确认，保证会话历史可回放、合法（user 之后有 assistant）。
                let confirm = Message::plain(Role::Assistant, "（已停止当前任务）");
                if let Err(e) = self.session.append(&msg.channel, &msg.chat_id, &confirm) {
                    tracing::warn!("取消确认持久化失败: {e}");
                }
            }
            // 路由层对 /cancel 已即时回送确认，这里不再重复 push outbound，避免刷屏。
            // 交接等待方按「取消」收尾，而不是挂到超时。仅 Agent 来源：用户 turn 的
            // 取消与交接等待无关（交接消息会被延迟重路由成自己的 Agent turn）。
            if let Some(router) = &self.bot_router {
                router.notify(
                    &msg.channel,
                    &msg.chat_id,
                    msg.handoff_id(),
                    crate::bots::HandoffOutcome::Cancelled,
                );
            }
        } else if let Some(text) = final_text.filter(|t| !t.is_empty()) {
            // 正常结束：持久化最终 assistant + 推送 outbound（与旧 handle_turn 一致）。
            let reply_text = if is_cron {
                cron_report(&msg, &text, true)
            } else {
                text.clone()
            };
            if !is_cron {
                let final_msg = if had_tool_iter {
                    // 结构化（含 reasoning），满足思考模式跨轮回传。
                    let mut blocks = vec![ContentBlock::text(text.clone())];
                    if let Some(r) = &final_reasoning {
                        blocks.push(ContentBlock::reasoning(r));
                    }
                    Message {
                        role: Role::Assistant,
                        content: blocks,
                        tool_call_id: None,
                        ts: 0,
                    }
                } else {
                    Message::plain(Role::Assistant, &text)
                };
                if let Err(e) = self.session.append(&msg.channel, &msg.chat_id, &final_msg) {
                    tracing::warn!("最终回复持久化失败: {e}");
                }
                messages.push(final_msg);
            }
            completed_successfully = true;

            // CLI 的 streaming provider 输出经过完成门禁缓冲；验证通过后才一次性写 stdout。
            // 非 CLI 通道仍经 outbound 投递。
            let skip_outbound = self.opts.stream_enabled && cli;
            if skip_outbound {
                let mut out = std::io::stdout();
                let _ = out.write_all(reply_text.as_bytes());
                let _ = out.write_all(b"\n");
                let _ = out.write_all(b"> ");
                let _ = out.flush();
            } else {
                tracing::info!(
                    channel = %msg.channel,
                    chat_id = %msg.chat_id,
                    bytes = reply_text.len(),
                    "最终回复已写入 outbound（后续投递由通道转发链负责）"
                );
                let _ = self
                    .outbound_tx
                    .send(BusMessage::new(
                        msg.channel.clone(),
                        msg.chat_id.clone(),
                        reply_text,
                    ))
                    .await;
            }
            // 交接回执：按 Agent 消息的关联 id 配给等待方（用户 turn 的回复
            // 不属于任何交接，跳过——否则会顶掉排队中交接的答案）。
            if let Some(router) = &self.bot_router {
                router.notify(
                    &msg.channel,
                    &msg.chat_id,
                    msg.handoff_id(),
                    crate::bots::HandoffOutcome::Replied(text),
                );
            }
        } else {
            // 空 / 错误回复。LLM 调用失败时回送详细错误（重试耗尽 / 流式中断）；
            // LLM 成功但返回空文本时（llm_error 为 None）回送兜底文案。
            let body = llm_error
                .clone()
                .unwrap_or_else(|| "Sorry, I encountered an error.".to_string());
            let body = if is_cron {
                cron_report(&msg, &body, false)
            } else {
                body
            };
            let _ = self
                .outbound_tx
                .send(BusMessage::new(
                    msg.channel.clone(),
                    msg.chat_id.clone(),
                    body.clone(),
                ))
                .await;
            if let Some(router) = &self.bot_router {
                router.notify(
                    &msg.channel,
                    &msg.chat_id,
                    msg.handoff_id(),
                    crate::bots::HandoffOutcome::Failed(body),
                );
            }
        }

        // ---- 技能结果记账与自进化信号（确定性，零 LLM 成本） ----
        //
        // 归因口径：本轮读取过入口（read SKILL.md）的顶层技能，按整轮最终成败记一次
        // outcome（供可靠性标注与 Curator 提前归档）；失败轮与用户纠正语气另记信号，
        // 供后台 review fork 优先消费（信号驱动进化，替代按节奏盲扫）。取消轮中性，不记账。
        {
            let used_skills: Vec<String> = state
                .announced_skill_paths
                .iter()
                .filter_map(|path| top_level_skill_name(&self.layout, path))
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if !used_skills.is_empty() && !cancelled {
                let usage = SkillUsageStore::new(self.layout.skills_dir.clone());
                for skill in &used_skills {
                    if let Err(error) = usage.record_outcome(skill, completed_successfully) {
                        tracing::warn!(skill = skill, %error, "技能结果记账失败");
                    }
                }
            }
            let source = format!("{}:{}", msg.channel, msg.chat_id);
            if !completed_successfully && !cancelled {
                if let Some(error) = &llm_error {
                    rclaw_tools::record_signal(
                        &self.layout.skills_dir,
                        rclaw_tools::classify_failure_signal(error),
                        &used_skills,
                        error,
                        &source,
                    );
                }
            } else if completed_successfully
                && msg.source() == MessageSource::User
                && rclaw_tools::looks_like_user_correction(&msg.content)
            {
                // 纠正语气出现在【成功收尾】的轮：上一轮行为被用户否定，经验应落进
                // 技能。归因到本轮实际读取的技能（纠正轮通常会重读涉事技能）。
                rclaw_tools::record_signal(
                    &self.layout.skills_dir,
                    "user_correction",
                    &used_skills,
                    &msg.content,
                    &source,
                );
            }
        }

        // Bot Mode：bot 会话（用户直聊与队友交接都算该 bot 的真实轮次）的记忆评审
        // 写 bot 自己的记忆（ReviewLifecycle 内部解析 persona）；技能进化保持主
        // persona 专属（技能库共享，ADR-0006）。
        if completed_successfully
            && matches!(msg.source(), MessageSource::User | MessageSource::Agent)
        {
            self.reviews.post_turn(
                msg.clone(),
                messages.clone(),
                react_iters,
                state.skill_manage_succeeded,
                state.memory_write_succeeded,
            );
        }

        // 返回未在安全点处理掉的插队输入，交上层重新路由为新 turn。
        deferred_messages.extend(drain_leftovers(&mut event_rx));
        deferred_messages
    }

    /// 执行 outcome 里的全部工具调用，组装成 user(tool_result) 消息（迁移自旧 handle_turn）。
    async fn execute_tools(
        &self,
        outcome: &rclaw_core::ChatOutcome,
        msg: &BusMessage,
        state: &mut TurnState,
        catalog: &SkillCatalog,
    ) -> ToolExecutionMessages {
        execute_tool_batch(
            outcome,
            ToolExecutionContext {
                registry: &self.tools,
                layout: &self.layout,
                channels: &self.channels,
                state,
                catalog,
                policy: ToolExecutionPolicy::Foreground {
                    message: msg,
                    outbound: &self.outbound_tx,
                    send_working_status: self.opts.send_working_status,
                },
            },
        )
        .await
    }

    #[cfg(test)]
    async fn run_post_turn_review(
        &self,
        msg: &BusMessage,
        messages: &[Message],
        auto_memory: bool,
        auto_evolve: bool,
    ) {
        self.reviews
            .run_post_turn_review(msg, messages, auto_memory, auto_evolve, None)
            .await;
    }

    #[cfg(test)]
    async fn announce_skill_invocations(
        &self,
        msg: &BusMessage,
        tool_calls: &[rclaw_core::ToolCall],
        catalog: &SkillCatalog,
        announced_skill_paths: &mut HashSet<PathBuf>,
    ) -> Vec<SkillInvocation> {
        tool_execution::announce_skill_invocations(
            &ToolExecutionPolicy::Foreground {
                message: msg,
                outbound: &self.outbound_tx,
                send_working_status: self.opts.send_working_status,
            },
            &self.channels,
            tool_calls,
            catalog,
            announced_skill_paths,
        )
        .await
    }
}

/// 判断当前 turn 已激活的动态入口是否被 Manifest 明确声明为媒体发布器。
pub(crate) fn top_level_skill_name(layout: &Layout, skill_path: &Path) -> Option<String> {
    let canonical_root = layout.skills_dir.canonicalize().ok()?;
    let canonical_path = skill_path.canonicalize().ok()?;
    let relative = canonical_path.strip_prefix(canonical_root).ok()?;
    let mut components = relative.components();
    let name = components.next()?.as_os_str().to_str()?;
    (components.next().is_some()).then(|| name.to_string())
}

// ===========================================================================
// 测试
// ===========================================================================
#[cfg(test)]
mod tests;
