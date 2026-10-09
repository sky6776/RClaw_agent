//! Bot Mode：多智能体协作的核心运行时（ADR-0006）。
//!
//! 组成：
//! - [`BotPersona`] / [`BotRegistry`]：bot 的运行期形态（定义 + 独立记忆 + 基础提示 +
//!   可选固定 provider）。注册表支持热重载（桌面版保存 bot 定义后调用 reload）。
//! - [`BotRouter`]：智能体间交接的投递与回执。把带归属前缀的 Agent 来源消息投到目标
//!   bot 的收件箱会话，注册 oneshot 回执等待；turn 收尾时由 TurnManager 通知回执。
//! - [`MessageAgentTool`]：暴露给模型的 `message_agent` 工具（仅主 turn / bot turn，
//!   子代理被 `is_subagent_blocked_tool` 拒绝）。
//! - [`BotTurnCtx`]：task-local 的当前 turn 身份（bot 名 / 交接链 / 回复键），
//!   `message_agent` 与群聊编排据此判定发送者与防环。
//!
//! 防环三板斧（对齐 Hermes 的有界协作）：
//! 1. 交接链深度上限 [`HANDOFF_MAX_CHAIN`]；
//! 2. 链上重复出现目标 → `handoff_cycle`；
//! 3. 目标会话的 turn 正阻塞在交接等待上 → `target_waiting`（等价 Hermes 的 turn lock
//!    拒绝，但即时失败而非等锁超时）。

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use rclaw_core::BusMessage;
use rclaw_llm::Provider;
use rclaw_store::fs::Layout;
use rclaw_store::memory::MemoryStore;
use rclaw_store::{BotDef, BotStore};
use rclaw_tools::{Tool, ToolResult, WebSearchControl};

use crate::context;

/// Bot Mode 交接消息正文上限（字符）。
pub const HANDOFF_MESSAGE_MAX_CHARS: usize = 16_000;
/// 交接链深度上限（链上发送者数量，含主 persona；目标排除在外）。
pub const HANDOFF_MAX_CHAIN: usize = 3;
/// 等待交接回复的超时（秒）。
pub const HANDOFF_REPLY_TIMEOUT_SECS: u64 = 600;

/// bot 正文转播消毒：把伪装成 harness 注入帧的控制语法改写为 member-quoted
/// 形态（对齐 Hermes 的 control-frame relabeling）——bot 引用框架语法（运行时
/// 上下文 / 记忆 / 房间围栏、System 行、交接与回传前缀、压缩摘要标记）不能
/// 冒充 harness 真实注入。真实用户行不改写（与 Hermes 同取舍）。
///
/// 这是精确子串黑名单改写，属纵深防御的一层（主防线是围栏 + System note），
/// 并非安全边界——空白/大小写变体不在覆盖范围。群聊转播（desktop groups.rs）
/// 与 1:1 交接（dispatch / MessageAgentTool 回传）共用同一套。
pub fn sanitize_control_frames(text: &str) -> String {
    // 幂等：先还原上一轮的改写再统一改写——多级转发（A 交接 B、B 回传、A 再
    // 转 C）时正文里已带 member-quoted 标记，直接叠加会嵌套成
    // [member-quoted [member-quoted …]]（展示噪音）。
    let text = text
        .replace("[member-quoted Message from 🤖]", "Message from 🤖")
        .replace("[member-quoted Reply from 🤖]", "Reply from 🤖")
        .replace("[member-quoted ", "")
        .replace("<member-quoted ", "<");
    text.replace("<rclaw-runtime-context", "<member-quoted rclaw-runtime-context")
        .replace(
            "</rclaw-runtime-context>",
            "[member-quoted /rclaw-runtime-context]",
        )
        .replace("<memory-context", "<member-quoted memory-context")
        .replace("</memory-context>", "[member-quoted /memory-context]")
        .replace("<room-transcript", "<member-quoted room-transcript")
        .replace("</room-transcript>", "[member-quoted /room-transcript]")
        .replace("<agent-status>", "<member-quoted agent-status>")
        .replace("</agent-status>", "[member-quoted /agent-status]")
        .replace("[System note:", "[member-quoted System note:")
        .replace("[System meta:", "[member-quoted System meta:")
        .replace("[System hint]", "[member-quoted System hint]")
        .replace("[CONTEXT COMPACTION", "[member-quoted CONTEXT COMPACTION")
        .replace(
            "--- END OF CONTEXT SUMMARY",
            "--- [member-quoted END OF CONTEXT SUMMARY",
        )
        .replace("Message from 🤖", "[member-quoted Message from 🤖]")
        .replace("Reply from 🤖", "[member-quoted Reply from 🤖]")
}
/// 交接回复回传上限（字符，正文上限 + 2000 余量，对齐 Hermes 的
/// REPLY_COMPLETION_CHARS 思想）：超长回复截断回传，全文留在对方会话可查——
/// 否则一条巨型回复会原样撑爆发送方的上下文。
pub const HANDOFF_REPLY_MAX_CHARS: usize = HANDOFF_MESSAGE_MAX_CHARS + 2_000;

/// provider 解析工厂：名字 → 可用 provider（bootstrap 读配置构建）。
type ProviderFactory = Arc<dyn Fn(&str) -> Option<Arc<dyn Provider>> + Send + Sync>;

/// 交接等待队列元素：(交接关联 id, 回执发送端)。
type WaiterEntry = (u64, oneshot::Sender<HandoffOutcome>);

/// bot 收件箱会话的 chat_id 前缀（channel 固定为 desktop）。
pub const BOT_CHANNEL: &str = "desktop";
/// 收件箱 chat_id：`bot-<name>`。
pub fn bot_inbox_chat_id(name: &str) -> String {
    format!("bot-{name}")
}
/// 群成员会话 chat_id：`grp-<gid>-<member>`。
pub fn group_member_chat_id(group_id: &str, member: &str) -> String {
    format!("grp-{group_id}-{member}")
}

// ---------------------------------------------------------------------------
// turn 上下文（task-local）
// ---------------------------------------------------------------------------

/// 当前 turn 的 Bot Mode 身份。主 persona 与 bot turn 都有（主 persona 也能交接）。
#[derive(Debug, Clone)]
pub struct BotTurnCtx {
    /// bot 名；None = 主 persona（RClaw）。
    pub bot_name: Option<String>,
    /// 链上身份名（"RClaw" 或 bot 名）。
    pub identity_name: String,
    /// 显示名（归属前缀用）。
    pub identity_display: String,
    /// 回复键 `"<channel>:<chat_id>"`。
    pub chat_key: String,
    /// 到达本 turn 前的交接链（不含自己）。
    pub chain: Vec<String>,
    /// turn 开始时的花名册快照（主 persona 的动态注记用；bot 的协议已在静态提示里）。
    pub roster: String,
}

tokio::task_local! {
    static BOT_CTX: BotTurnCtx;
}

/// 在 bot 上下文里运行一个 future（run_turn 外壳调用，对每个顶层 turn 恒定生效）。
pub async fn scope_bot_ctx<F: std::future::Future>(ctx: BotTurnCtx, fut: F) -> F::Output {
    BOT_CTX.scope(ctx, fut).await
}

/// 读取当前 turn 的 Bot Mode 上下文（子代理 / 无上下文环境返回 None）。
pub fn current_bot_ctx() -> Option<BotTurnCtx> {
    BOT_CTX.try_with(Clone::clone).ok()
}

// ---------------------------------------------------------------------------
// 失败码（类型化，对齐 Hermes bot_failure_reasons 的封闭枚举思路）
// ---------------------------------------------------------------------------

/// 交接失败码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandoffError {
    UnknownAgent(String),
    MessageTooLong {
        size: usize,
        max: usize,
    },
    SelfHandoff,
    HandoffCycle,
    DepthExceeded {
        depth: usize,
        max: usize,
    },
    /// 目标会话的 turn 正在等待别的交接回复，无法接收新交接。
    TargetWaiting,
    Timeout,
    Failed(String),
    Cancelled,
}

impl std::fmt::Display for HandoffError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HandoffError::UnknownAgent(name) => {
                write!(f, "unknown_agent: 不存在名为 {name} 的智能体")
            }
            HandoffError::MessageTooLong { size, max } => {
                write!(f, "message_too_long: 消息 {size} 字符超过上限 {max}")
            }
            HandoffError::SelfHandoff => write!(f, "self_handoff: 不能给自己发交接消息"),
            HandoffError::HandoffCycle => {
                write!(f, "handoff_cycle: 目标已在当前交接链上，拒绝形成环路")
            }
            HandoffError::DepthExceeded { depth, max } => {
                write!(f, "depth_exceeded: 交接链深度 {depth} 超过上限 {max}")
            }
            HandoffError::TargetWaiting => {
                write!(f, "target_waiting: 目标正阻塞在其它交接的等待上，稍后再试")
            }
            HandoffError::Timeout => write!(
                f,
                "timeout: 等待回复超时（消息可能已被对方处理，不要盲目重发；可到对方会话查看结果）"
            ),
            HandoffError::Failed(detail) => write!(f, "failed: {detail}"),
            HandoffError::Cancelled => write!(f, "cancelled: 对方回合被取消"),
        }
    }
}

/// 交接回执。
#[derive(Debug, Clone)]
pub enum HandoffOutcome {
    Replied(String),
    Failed(String),
    Cancelled,
}

// ---------------------------------------------------------------------------
// BotPersona / BotRegistry
// ---------------------------------------------------------------------------

/// bot 的运行期形态。
#[derive(Clone)]
pub struct BotPersona {
    pub name: String,
    pub def: Arc<BotDef>,
    /// 独立记忆（MEMORY / USER / daily 都在 `agent/bots/<name>/` 下）。
    pub memory: MemoryStore,
    /// bot 专属基础提示（身份 + Bot Mode 协议）。
    pub base_prompt: Arc<String>,
    /// 固定 provider；None = 跟随主 provider。
    pub provider: Option<Arc<dyn Provider>>,
}

/// bot 注册表：名字 → 运行期形态；支持热重载。
pub struct BotRegistry {
    store: BotStore,
    base_layout: Layout,
    inner: Mutex<HashMap<String, Arc<BotPersona>>>,
    /// 每 bot 的 MemoryStore 在重载间保持同一实例：不同实例对同一 MEMORY.md
    /// 各持一把互斥锁做读改写，热重载恰逢在途 bot turn 时会丢一次写入。
    memories: Mutex<HashMap<String, MemoryStore>>,
    provider_factory: ProviderFactory,
    memory_char_limit: usize,
    user_char_limit: usize,
}

impl BotRegistry {
    /// `provider_factory` 把 provider 名解析为可用 provider（bootstrap 传入，读配置构建）。
    pub fn new(
        base_layout: &Layout,
        provider_factory: ProviderFactory,
        search: Option<&WebSearchControl>,
        memory_char_limit: usize,
        user_char_limit: usize,
    ) -> Self {
        let registry = BotRegistry {
            store: BotStore::new(&base_layout.base),
            base_layout: base_layout.clone(),
            inner: Mutex::new(HashMap::new()),
            memories: Mutex::new(HashMap::new()),
            provider_factory,
            memory_char_limit,
            user_char_limit,
        };
        registry.reload(search);
        registry
    }

    /// 重读 bot 定义并重建全部 persona（提示 / 记忆 / provider 一并刷新）。
    pub fn reload(&self, search: Option<&WebSearchControl>) {
        let defs = self.store.list();
        let roster = roster_text(&defs);
        let mut map = HashMap::new();
        for (name, def) in defs {
            let def = Arc::new(def);
            let memory = {
                let mut memories = self
                    .memories
                    .lock()
                    .expect("BotRegistry.memories 锁未被毒化");
                memories
                    .entry(name.clone())
                    .or_insert_with(|| {
                        MemoryStore::new(Layout::new(
                            self.base_layout.base.join("bots").join(&name),
                        ))
                        .with_char_limits(self.memory_char_limit, self.user_char_limit)
                    })
                    .clone()
            };
            let provider = def
                .provider
                .as_deref()
                .and_then(|name| (self.provider_factory)(name));
            if def.provider.is_some() && provider.is_none() {
                tracing::warn!(
                    bot = %name,
                    provider = ?def.provider,
                    "bot 固定的 provider 不存在，回退主 provider"
                );
            }
            let base_prompt = Arc::new(context::build_bot_system_prompt(
                &self.base_layout,
                &name,
                &def,
                &roster,
                search,
            ));
            map.insert(
                name.clone(),
                Arc::new(BotPersona {
                    name: name.clone(),
                    def,
                    memory,
                    base_prompt,
                    provider,
                }),
            );
        }
        tracing::info!("Bot 注册表已加载（{} 个 bot）", map.len());
        // 清理已删除 bot 的记忆实例（随旧 persona 一并释放）。
        self.memories
            .lock()
            .expect("BotRegistry.memories 锁未被毒化")
            .retain(|name, _| map.contains_key(name));
        *self.inner.lock().expect("BotRegistry 锁未被毒化") = map;
    }

    /// 按名取 persona。
    pub fn get(&self, name: &str) -> Option<Arc<BotPersona>> {
        self.inner
            .lock()
            .expect("BotRegistry 锁未被毒化")
            .get(name)
            .cloned()
    }

    /// 全部 bot（按名排序）。
    pub fn all(&self) -> Vec<Arc<BotPersona>> {
        let mut v: Vec<Arc<BotPersona>> = self
            .inner
            .lock()
            .expect("BotRegistry 锁未被毒化")
            .values()
            .cloned()
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// 花名册文本（含主 persona 行），注入 bot 提示与群聊成员轮。
    pub fn roster(&self) -> String {
        let defs: Vec<(String, BotDef)> = self
            .inner
            .lock()
            .expect("BotRegistry 锁未被毒化")
            .iter()
            .map(|(k, v)| (k.clone(), (*v.def).clone()))
            .collect();
        roster_text(&defs)
    }

    /// 由会话标识解析 persona：仅 desktop 通道；`bot-<name>`（收件箱）与
    /// `grp-<gid>-<member>`（群成员会话）两种前缀。未命中返回 None（主 persona）。
    pub fn persona_for_chat(&self, channel: &str, chat_id: &str) -> Option<Arc<BotPersona>> {
        if channel != BOT_CHANNEL {
            return None;
        }
        if let Some(name) = chat_id.strip_prefix("bot-") {
            return self.get(name);
        }
        if let Some(rest) = chat_id.strip_prefix("grp-") {
            // 群 id 固定 8 位十六进制；第 9 个字符是 '-'，其后全部是成员名（可含 '-'）。
            let bytes = rest.as_bytes();
            if bytes.len() > 9 && bytes[..8].iter().all(u8::is_ascii_hexdigit) && bytes[8] == b'-' {
                return self.get(&rest[9..]);
            }
            return None;
        }
        None
    }
}

/// 花名册渲染（静态部分；主 persona 恒在首位）。
fn roster_text(defs: &[(String, BotDef)]) -> String {
    let mut s =
        String::from("- `@RClaw` — 主控：与用户直接对话的默认智能体，负责任务分解与最终交付\n");
    for (name, def) in defs {
        let desc = if def.description.is_empty() {
            def.title.clone()
        } else {
            def.description.clone()
        };
        s.push_str(&format!("- `@{name}` — {}：{}\n", def.title, desc));
    }
    s
}

// ---------------------------------------------------------------------------
// BotRouter
// ---------------------------------------------------------------------------

/// 交接投递与回执路由。
pub struct BotRouter {
    inbound_tx: mpsc::Sender<BusMessage>,
    registry: Arc<BotRegistry>,
    /// 交接关联 id 计数器。
    next_handoff_id: AtomicU64,
    /// 目标会话 → (交接 id, 回执发送端) 队列。按 id 配对：turn 收尾携带该轮
    /// Agent 消息的 id，精确命中自己的等待者——用户 turn 的回复、并发投递的
    /// 其它交接都不会错配（FIFO 在「先注册后发送」的窗口内存在交错）。
    waiters: Mutex<HashMap<String, VecDeque<WaiterEntry>>>,
    /// 其 turn 正阻塞在交接等待上的会话键（防死锁的即时失败集）。
    dispatching: Mutex<HashSet<String>>,
}

/// 统一的会话键（waiters / dispatching / BotTurnCtx.chat_key 共用）。
pub fn chat_key(channel: &str, chat_id: &str) -> String {
    format!("{channel}:{chat_id}")
}

impl BotRouter {
    pub fn new(inbound_tx: mpsc::Sender<BusMessage>, registry: Arc<BotRegistry>) -> Self {
        BotRouter {
            inbound_tx,
            registry,
            next_handoff_id: AtomicU64::new(1),
            waiters: Mutex::new(HashMap::new()),
            dispatching: Mutex::new(HashSet::new()),
        }
    }

    /// 标记一个会话的 turn 正在等待交接回复（tool 在 dispatch 前调用，await 结束后清除）。
    pub fn mark_dispatching(&self, key: &str) {
        self.dispatching
            .lock()
            .expect("BotRouter.dispatching 锁未被毒化")
            .insert(key.to_string());
    }

    /// 清除等待标记（与 mark 配对；键不存在时静默）。
    pub fn clear_dispatching(&self, key: &str) {
        self.dispatching
            .lock()
            .expect("BotRouter.dispatching 锁未被毒化")
            .remove(key);
    }

    /// 校验并投递一次交接；成功返回回执接收端。
    ///
    /// `sender_name` 为发送者链上身份（"RClaw" / bot 名 / "group:<gid>"），
    /// `sender_display` 为归属前缀里的显示名。`chain` 为**含发送者**的交接链
    /// （防环与深度判定；群聊成员轮为空链——编排方不是链上智能体）。
    /// `target_chat_id` 通常为 [`bot_inbox_chat_id`]（私聊交接）或
    /// [`group_member_chat_id`]（群聊成员轮）。
    pub async fn dispatch(
        &self,
        target_bot: &str,
        target_chat_id: &str,
        sender_name: &str,
        sender_display: &str,
        chain: Vec<String>,
        text: &str,
    ) -> Result<oneshot::Receiver<HandoffOutcome>, HandoffError> {
        let persona = self
            .registry
            .get(target_bot)
            .ok_or_else(|| HandoffError::UnknownAgent(target_bot.to_string()))?;

        let size = text.chars().count();
        if size > HANDOFF_MESSAGE_MAX_CHARS {
            return Err(HandoffError::MessageTooLong {
                size,
                max: HANDOFF_MESSAGE_MAX_CHARS,
            });
        }
        if chain.len() > HANDOFF_MAX_CHAIN {
            return Err(HandoffError::DepthExceeded {
                depth: chain.len(),
                max: HANDOFF_MAX_CHAIN,
            });
        }
        if target_bot == sender_name {
            return Err(HandoffError::SelfHandoff);
        }
        if chain.iter().any(|c| c == target_bot) {
            return Err(HandoffError::HandoffCycle);
        }

        let target_key = chat_key(BOT_CHANNEL, target_chat_id);
        if self
            .dispatching
            .lock()
            .expect("BotRouter.dispatching 锁未被毒化")
            .contains(&target_key)
        {
            return Err(HandoffError::TargetWaiting);
        }

        // 注册回执等待（先注册后投递：目标 turn 可能在 send 返回前就完成）。
        let handoff_id = self.next_handoff_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("BotRouter.waiters 锁未被毒化")
            .entry(target_key.clone())
            .or_default()
            .push_back((handoff_id, tx));

        // 交接正文是转播数据：控制帧语法先消毒再进对方会话（与群聊转播同一套）。
        let content = format!(
            "Message from 🤖 {sender_display} (@{sender_name}):\n{}",
            sanitize_control_frames(text)
        );
        let msg = BusMessage::new_agent(BOT_CHANNEL, target_chat_id, chain, handoff_id, content);
        if let Err(e) = self.inbound_tx.send(msg).await {
            // 投递失败：清掉刚注册的等待者，避免占用队列。
            self.drop_waiter(&target_key, handoff_id);
            return Err(HandoffError::Failed(format!("inbound 总线投递失败: {e}")));
        }
        tracing::info!(
            bot = %persona.name,
            target_chat = %target_chat_id,
            "Bot Mode 交接已投递"
        );
        Ok(rx)
    }

    /// turn 收尾回调：把结果配给该轮 Agent 消息关联 id 对应的等待者。
    ///
    /// 无关联 id（`None`，用户 turn 的回复）或未知 id 静默返回——用户 turn 的
    /// 回复不得被交接等待者消费（等价于按来源门控：只有 `new_agent` 会设置 id）。
    /// 按 id 精确移除；接收端已 drop（调用方超时 / 取消）则静默丢弃，队列中的
    /// 其它等待者不受影响。
    pub fn notify(
        &self,
        channel: &str,
        chat_id: &str,
        handoff_id: Option<u64>,
        outcome: HandoffOutcome,
    ) {
        let Some(handoff_id) = handoff_id else {
            return;
        };
        let key = chat_key(channel, chat_id);
        let waiter = {
            let mut map = self.waiters.lock().expect("BotRouter.waiters 锁未被毒化");
            let Some(q) = map.get_mut(&key) else {
                return;
            };
            let Some(i) = q.iter().position(|(id, _)| *id == handoff_id) else {
                return;
            };
            let Some((_, tx)) = q.remove(i) else {
                return;
            };
            // 空队列顺手清键，避免 map 随会话数无限增长。
            if q.is_empty() {
                map.remove(&key);
            }
            tx
        };
        let _ = waiter.send(outcome);
    }

    /// 群聊编排 / 任意调用方等待一条已投递交接的回复（带超时）。
    pub async fn wait_reply(
        &self,
        rx: oneshot::Receiver<HandoffOutcome>,
    ) -> Result<String, HandoffError> {
        match tokio::time::timeout(Duration::from_secs(HANDOFF_REPLY_TIMEOUT_SECS), rx).await {
            Ok(Ok(HandoffOutcome::Replied(text))) => Ok(text),
            Ok(Ok(HandoffOutcome::Failed(e))) => Err(HandoffError::Failed(e)),
            Ok(Ok(HandoffOutcome::Cancelled)) => Err(HandoffError::Cancelled),
            Ok(Err(_recv_closed)) => Err(HandoffError::Failed(
                "回执通道被关闭（等待者已被移除）".to_string(),
            )),
            Err(_elapsed) => Err(HandoffError::Timeout),
        }
    }

    fn drop_waiter(&self, key: &str, handoff_id: u64) {
        let mut map = self.waiters.lock().expect("BotRouter.waiters 锁未被毒化");
        if let Some(q) = map.get_mut(key) {
            if let Some(i) = q.iter().position(|(id, _)| *id == handoff_id) {
                q.remove(i);
            }
            if q.is_empty() {
                map.remove(key);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// message_agent 工具
// ---------------------------------------------------------------------------

/// 智能体间交接工具。仅在主 turn / bot turn 的工具面出现（子代理被
/// `is_subagent_blocked_tool` 拒绝；群聊成员轮经 BotRouter 直达，不经本工具）。
pub struct MessageAgentTool {
    registry: Arc<BotRegistry>,
    router: Arc<OnceLock<Arc<BotRouter>>>,
}

impl MessageAgentTool {
    /// `router` 为延迟回填句柄（bootstrap 先注册工具，二进制装配完 inbound 后 set）。
    pub fn new(registry: Arc<BotRegistry>, router: Arc<OnceLock<Arc<BotRouter>>>) -> Self {
        MessageAgentTool { registry, router }
    }
}

#[async_trait]
impl Tool for MessageAgentTool {
    fn name(&self) -> &str {
        "message_agent"
    }

    fn description(&self) -> &str {
        "Send a direct message to another agent (bot) and wait for its reply. The message is delivered to the target agent's inbox session and runs as that agent's next turn. Rules: compose the message yourself in your own words — never forward the user's words verbatim and never reveal private 1:1 conversation content; avoid bare acknowledgements; one substantive message per handoff. The reply returns as a tool result attributed to the sender."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Target bot name from the roster (without @)."
                },
                "message": {
                    "type": "string",
                    "description": "The message body you composed for the target agent. Self-contained: include all context the target needs."
                }
            },
            "required": ["agent", "message"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(ctx) = current_bot_ctx() else {
            return ToolResult::err(
                "message_agent 仅在主会话与 bot 会话中可用（子代理与后台任务不可交接）",
            );
        };
        let Some(target) = input
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .map(|s| s.trim_start_matches('@').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
        else {
            return ToolResult::err("agent is required for message_agent");
        };
        let Some(text) = input
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
        else {
            return ToolResult::err("message is required for message_agent");
        };

        let Some(router) = self.router.get().cloned() else {
            return ToolResult::err("BotRouter 尚未就绪（运行时未装配完成）");
        };

        let mut chain = ctx.chain.clone();
        chain.push(ctx.identity_name.clone());
        if chain.len() > HANDOFF_MAX_CHAIN {
            return ToolResult::err(
                HandoffError::DepthExceeded {
                    depth: chain.len(),
                    max: HANDOFF_MAX_CHAIN,
                }
                .to_string(),
            );
        }

        let target_chat = bot_inbox_chat_id(&target);
        router.mark_dispatching(&ctx.chat_key);
        // 守卫保证「等待中」标记必然清除：turn 被取消时工具 future 会在 await 处
        // 被 drop，函数体里的 clear 不会执行，唯有 Drop 守卫兜底——否则该会话会被
        // target_waiting 永久拒绝。
        struct DispatchingGuard<'a> {
            router: &'a BotRouter,
            key: String,
        }
        impl Drop for DispatchingGuard<'_> {
            fn drop(&mut self) {
                self.router.clear_dispatching(&self.key);
            }
        }
        let _guard = DispatchingGuard {
            router: router.as_ref(),
            key: ctx.chat_key.clone(),
        };
        let rx = match router
            .dispatch(
                &target,
                &target_chat,
                &ctx.identity_name,
                &ctx.identity_display,
                chain,
                text,
            )
            .await
        {
            Ok(rx) => rx,
            Err(e) => return ToolResult::err(e.to_string()),
        };
        let outcome = router.wait_reply(rx).await;
        drop(_guard);

        match outcome {
            Ok(reply) => {
                let display = self
                    .registry
                    .get(&target)
                    .map(|p| p.def.title.clone())
                    .unwrap_or_else(|| target.clone());
                // 回传回复同样是转播数据：消毒后再截断（截断可能切尾，先消毒
                // 保证改写标记完整）。
                let reply = sanitize_control_frames(&reply);
                let reply =
                    if reply.chars().count() > HANDOFF_REPLY_MAX_CHARS {
                        format!(
                        "{}\n…（回复超过 {HANDOFF_REPLY_MAX_CHARS} 字符已截断，全文在对方会话中）",
                        reply.chars().take(HANDOFF_REPLY_MAX_CHARS).collect::<String>()
                    )
                    } else {
                        reply
                    };
                ToolResult::ok(format!("Reply from 🤖 {display} (@{target}):\n{reply}"))
            }
            Err(e) => ToolResult::err(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn persona(name: &str) -> Arc<BotPersona> {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        Arc::new(BotPersona {
            name: name.to_string(),
            def: Arc::new(BotDef {
                title: format!("{name}标题"),
                description: String::new(),
                persona: "测试".into(),
                provider: None,
                denied_tools: vec![],
            }),
            memory: MemoryStore::new(layout),
            base_prompt: Arc::new(String::new()),
            provider: None,
        })
    }

    fn registry_with(bots: Vec<(&str, Arc<BotPersona>)>) -> Arc<BotRegistry> {
        // 测试专用构造：绕过文件系统，直接塞 inner map。
        let dir = tempfile::tempdir().unwrap();
        let base_layout = Layout::new(dir.path());
        let inner: HashMap<String, Arc<BotPersona>> =
            bots.into_iter().map(|(n, p)| (n.to_string(), p)).collect();
        Arc::new(BotRegistry {
            store: BotStore::new(&base_layout.base),
            base_layout,
            inner: Mutex::new(inner),
            memories: Mutex::new(HashMap::new()),
            provider_factory: Arc::new(|_| None),
            memory_char_limit: 2200,
            user_char_limit: 1375,
        })
    }

    #[tokio::test]
    async fn dispatch_validates_target_and_message() {
        let (tx, _rx) = mpsc::channel(4);
        let registry = registry_with(vec![("alpha", persona("alpha"))]);
        let router = Arc::new(BotRouter::new(tx, registry));

        // 未知目标。
        assert!(matches!(
            router
                .dispatch(
                    "ghost",
                    "bot-ghost",
                    "RClaw",
                    "主控",
                    vec!["RClaw".into()],
                    "hi"
                )
                .await,
            Err(HandoffError::UnknownAgent(_))
        ));
        // 自交。
        assert!(matches!(
            router
                .dispatch(
                    "alpha",
                    "bot-alpha",
                    "alpha",
                    "甲",
                    vec!["alpha".into()],
                    "hi"
                )
                .await,
            Err(HandoffError::SelfHandoff)
        ));
        // 环路：目标在链上但不是直接发送者（B 收到 A 的交接后再回到 A）。
        assert!(matches!(
            router
                .dispatch(
                    "alpha",
                    "bot-alpha",
                    "beta",
                    "乙",
                    vec!["RClaw".into(), "alpha".into(), "beta".into()],
                    "hi"
                )
                .await,
            Err(HandoffError::HandoffCycle)
        ));
        // 超长。
        let big = "x".repeat(HANDOFF_MESSAGE_MAX_CHARS + 1);
        assert!(matches!(
            router
                .dispatch(
                    "alpha",
                    "bot-alpha",
                    "RClaw",
                    "主控",
                    vec!["RClaw".into()],
                    &big
                )
                .await,
            Err(HandoffError::MessageTooLong { .. })
        ));
        // 深度超限：链上发送者（含当前发送方）数量超过上限。
        assert!(matches!(
            router
                .dispatch(
                    "alpha",
                    "bot-alpha",
                    "delta",
                    "丙",
                    vec![
                        "RClaw".into(),
                        "beta".into(),
                        "gamma".into(),
                        "delta".into()
                    ],
                    "hi"
                )
                .await,
            Err(HandoffError::DepthExceeded { .. })
        ));
    }

    #[tokio::test]
    async fn target_waiting_and_reply_pairing() {
        let (tx, _rx) = mpsc::channel(4);
        let registry = registry_with(vec![("alpha", persona("alpha"))]);
        let router = Arc::new(BotRouter::new(tx, registry));

        // 正常投递 → 等待者注册。
        let rx = router
            .dispatch(
                "alpha",
                "bot-alpha",
                "RClaw",
                "主控",
                vec!["RClaw".into()],
                "hi",
            )
            .await
            .unwrap();
        // alpha 的 turn 标记为等待中 → 再向 alpha 交接被拒。
        router.mark_dispatching(&chat_key(BOT_CHANNEL, "bot-alpha"));
        assert!(matches!(
            router
                .dispatch(
                    "alpha",
                    "bot-alpha",
                    "beta",
                    "乙",
                    vec!["beta".into()],
                    "again"
                )
                .await,
            Err(HandoffError::TargetWaiting)
        ));
        router.clear_dispatching(&chat_key(BOT_CHANNEL, "bot-alpha"));

        // id 关联配对：每个等待者只收到自己那条交接的回复（乱序到达也不错配）。
        let rx2 = router
            .dispatch(
                "alpha",
                "bot-alpha",
                "beta",
                "乙",
                vec!["beta".into()],
                "2nd",
            )
            .await
            .unwrap();
        // 两条等待都在队列：取各自的关联 id（front = 先注册的 rx）。
        let (id1, id2) = {
            let map = router.waiters.lock().unwrap();
            let q = map.get(&chat_key(BOT_CHANNEL, "bot-alpha")).unwrap();
            (
                q.front().map(|(id, _)| *id).unwrap(),
                q.back().map(|(id, _)| *id).unwrap(),
            )
        };
        assert_ne!(id1, id2);
        // rx2 的 turn 先完成：按 id 精确配对，不会顶掉 rx；未知 id 与无 id（用户
        // turn 的回复）静默，不消费任何等待者。
        router.notify(
            BOT_CHANNEL,
            "bot-alpha",
            Some(id2),
            HandoffOutcome::Replied("二".into()),
        );
        router.notify(
            BOT_CHANNEL,
            "bot-alpha",
            Some(999_999),
            HandoffOutcome::Replied("孤儿".into()),
        );
        router.notify(
            BOT_CHANNEL,
            "bot-alpha",
            None,
            HandoffOutcome::Replied("用户回复".into()),
        );
        router.notify(
            BOT_CHANNEL,
            "bot-alpha",
            Some(id1),
            HandoffOutcome::Replied("一".into()),
        );
        assert_eq!(router.wait_reply(rx).await.unwrap(), "一");
        assert_eq!(router.wait_reply(rx2).await.unwrap(), "二");
    }

    #[test]
    fn chat_id_helpers() {
        assert_eq!(bot_inbox_chat_id("alpha"), "bot-alpha");
        assert_eq!(
            group_member_chat_id("abcd1234", "beta"),
            "grp-abcd1234-beta"
        );
    }

    #[test]
    fn persona_for_chat_resolves_inbox_only() {
        let registry = registry_with(vec![("alpha", persona("alpha"))]);
        assert!(registry.persona_for_chat("desktop", "bot-alpha").is_some());
        assert!(registry.persona_for_chat("desktop", "bot-ghost").is_none());
        assert!(registry.persona_for_chat("cli", "bot-alpha").is_none());
        assert!(registry
            .persona_for_chat("desktop", "normal-chat")
            .is_none());
        // 群成员会话：固定 8 位群 id + '-' + 成员名（成员名可含 '-'）。
        assert!(registry
            .persona_for_chat("desktop", "grp-abcd1234-alpha")
            .is_some());
        assert!(registry
            .persona_for_chat("desktop", "grp-abcd1234-alpha-2")
            .is_none());
        assert!(registry
            .persona_for_chat("desktop", "grp-abcd1234-ghost")
            .is_none());
        // 非 8 位群 id 段不按群会话解析。
        assert!(registry
            .persona_for_chat("desktop", "grp-short-alpha")
            .is_none());
    }
}
