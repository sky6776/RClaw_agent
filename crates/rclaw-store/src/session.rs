//! Session 持久化（`agent/state.sqlite`）。
//!
//! 会话消息、rewind 文件 checkpoint 与 usage 锚点都落在同一个 SQLite 库里，忠实保留
//! 全部「正确性红线」：
//! - 按 `channel + chat_id` 隔离（`session` 表唯一键，防串档）；
//! - 结构化字段（reasoning_content / tool_calls / tool_call_id）随 `Message` 的 serde JSON
//!   整体入列，满足 DeepSeek 思考模式「跨轮完整回传」要求；
//! - `append_tool_chain` 把 assistant(tool_calls) + tool_result 两条记录放进**同一个事务**，
//!   要么都进去、要么都不进（SQLite 事务是真正的崩溃原子，比旧 JSONL 的单次 write_all 更强）；
//!   需要时 rewind checkpoint 也同事务提交，杜绝「文件已改、链已落、checkpoint 丢」；
//! - 加载时仍做全历史工具链一致性修复（`repair_tool_chains`）与开头孤儿裁剪；
//! - **压缩非破坏**（参考 ZCode）：不删旧消息，只追加一条 compaction boundary 行
//!   （`meta.compaction = {head_until, tail_from}`），加载时按最后一个 boundary 拼出
//!   provider 可见的「活动视图」：`head ++ [boundary] ++ kept ++ after`。完整 transcript
//!   永远保留（`load_transcript` / `/export` / 搜索 / 交接可见）。
//!
//! 每条消息有稳定身份 `message.id`（= `Message::id`，全局单调递增）。所有「定位某条消息」的
//! 逻辑（压缩锚点、usage 锚点、rewind 分组）一律按 id。

use std::collections::HashMap;

use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Row};

use rclaw_core::{ContentBlock, Message, Role};

use crate::db::{now_ms, Db};
use crate::fs::Layout;
use crate::journal::JournalStore;
use crate::{Result, StoreError};

/// 会话存储：围绕 `agent/state.sqlite` 的会话 / 消息 / checkpoint / 锚点操作。
#[derive(Debug, Clone)]
pub struct SessionStore {
    db: Db,
    layout: Layout,
}

/// 会话列表条目（桌面版多会话管理 / `/export all` / 交接）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    pub channel: String,
    pub chat_id: String,
    pub archived: bool,
    /// Unix 毫秒。
    pub created_at: i64,
    /// Unix 毫秒；每次追加消息时刷新。
    pub updated_at: i64,
    pub message_count: u32,
}

/// [`SessionStore::fork_session`] 的产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFork {
    pub new_chat_id: String,
    /// 复制进新会话的消息条数（活动视图前缀，含作为普通行保留的摘要正文）。
    pub copied: usize,
    /// 分叉锚点（源会话的 message id；新分支包含它及之前的全部可见消息）。
    pub anchor_message_id: i64,
}

/// 会话血缘（kv `session-fork/<channel>/<chat_id>` 的解码形态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkLineage {
    pub parent_chat_id: String,
    pub anchor_message_id: i64,
}

/// fork 血缘的 kv 键。
fn fork_lineage_key(channel: &str, chat_id: &str) -> String {
    format!("session-fork/{channel}/{chat_id}")
}

/// 会话目标（/goal）的 kv 键。规则单一事实源在此：runtime 的 goal 读写与 store 的
/// 会话删除 / 全清级联清理用同一规则。
pub fn session_goal_key(channel: &str, chat_id: &str) -> String {
    format!("session-goal/{channel}/{chat_id}")
}

/// artifact 目录的会话键（与 runtime `turn::artifact` 同一规则，单一事实源在此）：
/// `[A-Za-z0-9._-]` 之外替换成 `_`（防 chat_id 里的路径分隔符逃出目录），取前 46 字符
/// 后拼 `-` + 完整原始键的 FNV-1a 64 位摘要（16 hex，总长 ≤63）——纯截断会把只差尾部
/// 的两个长会话 id 合并进同一目录，删其一就端掉另一家的落盘产物；摘要保证不同会话
/// 必不同键。子代理等无源会话场景由调用方传 `chat_id = ""`（归档为 `subagent-<channel>`）。
pub fn artifact_chat_key(channel: &str, chat_id: &str) -> String {
    let raw = if chat_id.is_empty() {
        format!("subagent-{channel}")
    } else {
        format!("{channel}-{chat_id}")
    };
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // FNV-1a 64：无依赖、跨版本稳定（std 的 DefaultHasher 不保证跨版本一致，
    // 磁盘键要的是「同一输入永远同一输出」）。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in raw.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let mut key = String::with_capacity(64);
    key.extend(sanitized.chars().take(46));
    key.push('-');
    key.push_str(&format!("{hash:016x}"));
    key
}

/// 目录名是否形如 [`artifact_chat_key`] 的产物（`<1..=46 个 [A-Za-z0-9._-]>-<16 hex>`）。
/// 全清时只删这种目录，用户放进 `artifacts/` 的别的东西不碰。
fn is_artifact_chat_dir_name(name: &str) -> bool {
    let Some((head, hash)) = name.rsplit_once('-') else {
        return false;
    };
    let head_len = head.chars().count();
    (1..=46).contains(&head_len)
        && head
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && hash.len() == 16
        && hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// 删除某会话的 artifact 目录（工具结果落盘句柄的实体；会话删除时级联清理）。
/// best-effort：目录不存在视为成功，删除失败只告警（artifact 只是上下文优化产物，
/// 不能让会话删除因此失败）。
fn remove_artifact_dir(layout: &crate::fs::Layout, channel: &str, chat_id: &str) {
    let dir = layout
        .base
        .join("artifacts")
        .join(artifact_chat_key(channel, chat_id));
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!("artifact 目录清理失败（{}）: {error}", dir.display());
        }
    }
}

/// 活动 / 归档会话计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionStats {
    pub active: u32,
    pub archived: u32,
}

/// rewind 文件 checkpoint 的元数据行（不含 `before` 正文；列表 / 校验链用）。
///
/// `before` 单条可达 2MB 且 checkpoint 从不清理，列最近分组或做 hash 校验链时不该把全部
/// 正文读进内存——回滚集真正需要的正文另按行 id 范围读取（[`SessionStore::load_file_checkpoints_from`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheckpointMeta {
    pub id: i64,
    pub group_id: i64,
    pub path: String,
    pub before_hash: Option<String>,
    pub after_hash: Option<String>,
    pub is_rewind: bool,
    /// 同事务 tool_result 消息行 id（v2；旧行 / 重锚标记为 None）。
    pub anchor_message_id: Option<i64>,
}

/// rewind 文件 checkpoint 记录（一条 = 某次工具写入前后的单个文件快照）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCheckpointRecord {
    /// 行 id（插入时分配；写入时忽略）。行 id 序即时间序，回滚集按它切分。
    pub id: i64,
    /// rewind 分组 id（纳秒单调；`/rewind <id>` 的输入；同一轮内一组工具写入共享）。
    pub group_id: i64,
    pub path: String,
    /// 写入前内容；`None` = 写入前不存在（回滚即删除）。
    pub before: Option<String>,
    /// 写入前内容 sha256 hex；`None` = 写入前不存在。
    pub before_hash: Option<String>,
    /// 写入后内容 sha256 hex；`None` = 写入后不存在（delete）。
    pub after_hash: Option<String>,
    /// `/rewind` 自身写下的「重锚」标记（不参与回滚集，只刷新最新 hash）。
    pub is_rewind: bool,
    /// 与本 checkpoint 同事务写入的 tool_result 消息行 id（v2 起）；`None` = 旧数据
    /// （迁移前行）或独立落库路径（cron / 取消清理的 flush、rewind 重锚标记）。
    /// 「fork + 文件回滚到锚点」按它判定 checkpoint 属于哪条消息之后。
    pub anchor_message_id: Option<i64>,
}

/// 工具调用侧表记录（`tool_call_meta.meta` 列的 JSON 形态；一条 = 一次 tool_use）。
///
/// 只放 `ToolUse` / `ToolResult` 块里没有、执行期才知道的事实：归属 Skill、起止、成败。
/// 入参与结果正文不复制——它们在消息行里，展开面板按需读
/// （[`SessionStore::load_tool_chain_detail`]）。字段全部 `default`：旧行 / 字段增减
/// 都能解出，不让面板因一条坏 meta 整体失败。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ToolCallMetaRecord {
    /// 配对 `ToolUse.id`（同一 assistant 行内唯一）。
    pub tool_use_id: String,
    /// 归属 Skill：Manifest 专用入口工具所属的 skill，或 `read` 命中 SKILL.md 时激活的
    /// skill；普通内置工具为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    /// 开始执行时刻（unix 毫秒）；并行批次里各条各自计时。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_ms: Option<i64>,
    /// 执行耗时（毫秒）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 结果是否为错误（与同链 `ToolResult.is_error` 一致；列表级状态徽标不必读载体行）。
    #[serde(default)]
    pub is_error: bool,
    /// 调用没有执行完：turn 被取消时链以 aborted 占位结果闭合（`is_error` 同为 true）。
    /// 面板据此显示「已中断」而不是「失败」。
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub aborted: bool,
}

impl ToolCallMetaRecord {
    /// 取消中断的调用：没有耗时 / skill，仅标记 aborted + is_error（与占位结果一致）。
    pub fn aborted(tool_use_id: impl Into<String>) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            is_error: true,
            aborted: true,
            ..Self::default()
        }
    }

    /// 按结果载体的 `ToolResult` 块生成最简记录（只有成败；熔断合成结果等没有执行期
    /// 计时的链用它，列表级徽标才不会把错误显示成完成）。
    pub fn from_result_blocks(blocks: &[ContentBlock]) -> Vec<Self> {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    is_error,
                    ..
                } => Some(Self {
                    tool_use_id: tool_use_id.clone(),
                    is_error: *is_error,
                    ..Self::default()
                }),
                _ => None,
            })
            .collect()
    }
}

/// 单条工具结果（展开面板用；从 tool_result 载体行的 `ToolResult` 块投影）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResultDetail {
    pub tool_use_id: String,
    pub content: String,
    pub is_error: bool,
}

/// 一条 assistant(tool_calls) 行对应的工具链详情（桌面 / 安卓「工具调用展开观察」）。
///
/// 事实源全在库里：入参 = assistant 行的 `ToolUse` 块；结果 = 紧随其后的 tool_result 载体
/// 行；skill / 耗时 / 成败 = `tool_call_meta`；文件写入前内容 = 锚在载体行上的
/// `file_checkpoint`（write / edit 的新旧对照）。重启 / 重装（库文件仍在）后同样可得。
#[derive(Debug, Clone, PartialEq)]
pub struct ToolChainDetail {
    /// assistant(tool_calls) 行本身（含完整 `ToolUse` 入参）。
    pub assistant: Message,
    /// 载体行里的结果（按块顺序）；载体缺失（fork 以 tool_use 行为锚、链写入中断）时为空。
    pub results: Vec<ToolResultDetail>,
    pub metas: Vec<ToolCallMetaRecord>,
    /// 本链写入的文件 checkpoint（含 `before` 正文），按行 id 升序。
    pub checkpoints: Vec<FileCheckpointRecord>,
}

/// usage 锚点：某条消息（`anchor_message_id`）处 provider 回报的真实 prompt token 总数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UsageAnchorRecord {
    pub total_tokens: usize,
    pub anchor_message_id: i64,
}

/// `search_messages` 的粗筛结果行（精确计分由调用方完成）。
#[derive(Debug, Clone, PartialEq)]
pub struct SearchRow {
    pub channel: String,
    pub chat_id: String,
    pub archived: bool,
    pub message: Message,
}

/// 会话历史快照：尾部可回放窗口 + 行级游标（前端增量续读的起点）。
///
/// 游标 = 读取时该会话的最大消息行 id（与窗口同一读快照，不含窗口裁掉的更早行——
/// 那些行 id < 游标，永远不会被增量重放，视为已消费的「窗口外历史」）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatSnapshot {
    pub messages: Vec<Message>,
    pub cursor: i64,
}

/// 会话历史行级增量：`after_id` 之后新落库的可回放行 + 消费游标。
///
/// 游标语义保证「id ≤ cursor 的行要么在本批、要么被过滤器明确消费、要么在更早的
/// 批次里」：批满 `limit` 条时游标 = 本批最后一行的 id（后面可能还有，`more` 提示
/// 调用方续拉）；不满时游标 = 该会话当时的最大行 id（一次性消费到头，含被过滤器
/// 筛掉的行）。rclaw 的消息行只追加不更新，增量天然只有 appended 一种，无需
/// upsert / tombstone（zcode 协议层的三事件在这里塌缩成一个「按游标续读」）。
#[derive(Debug, Clone, PartialEq)]
pub struct ChatDelta {
    pub messages: Vec<Message>,
    pub cursor: i64,
    pub more: bool,
}

/// compaction boundary 元数据（`message.meta` 列的 JSON 形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionMeta {
    head_until: Option<i64>,
    tail_from: i64,
}

impl CompactionMeta {
    fn to_json(self) -> String {
        serde_json::json!({
            "compaction": { "head_until": self.head_until, "tail_from": self.tail_from }
        })
        .to_string()
    }

    fn parse(raw: &str) -> Option<CompactionMeta> {
        let value: serde_json::Value = serde_json::from_str(raw).ok()?;
        let c = value.get("compaction")?;
        Some(CompactionMeta {
            head_until: c.get("head_until").and_then(|v| v.as_i64()),
            tail_from: c.get("tail_from")?.as_i64()?,
        })
    }
}

impl SessionStore {
    /// 打开（必要时创建）`layout.db_file`。生产装配必须用本方法并处理错误。
    pub fn open(layout: Layout) -> Result<Self> {
        let db = Db::open(&layout.db_file)?;
        Ok(SessionStore { db, layout })
    }

    /// 测试 / 原型便捷构造：打开失败直接 panic。生产路径请用 [`Self::open`]。
    pub fn new(layout: Layout) -> Self {
        let path = layout.db_file.clone();
        Self::open(layout).unwrap_or_else(|e| panic!("打开状态库失败（{}）: {e}", path.display()))
    }

    /// 由已打开的库构造（与 [`JournalStore`] 共享同一连接）。
    pub fn from_db(db: Db, layout: Layout) -> Self {
        SessionStore { db, layout }
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 共享同一连接的 journal / kv 门面。
    pub fn journal(&self) -> JournalStore {
        JournalStore::from_db(self.db.clone())
    }

    /// 底层库句柄（桌面版统计等需要直接查询时使用）。
    pub fn db(&self) -> &Db {
        &self.db
    }

    // ---------------------------------------------------------------------
    // 写入
    // ---------------------------------------------------------------------

    /// 追加一条消息，返回分配的 `message.id`。若 `ts` 为 0，自动盖当前时间戳。
    pub fn append(&self, channel: &str, chat_id: &str, message: &Message) -> Result<i64> {
        self.db.transaction(|tx| {
            let sid = ensure_session(tx, channel, chat_id)?;
            insert_message(tx, sid, message, None)
        })
    }

    /// 原子追加「assistant(tool_calls) + tool_result」两条记录，返回两者的 id。
    pub fn append_tool_chain(
        &self,
        channel: &str,
        chat_id: &str,
        asst: &Message,
        tool_result: &Message,
    ) -> Result<(i64, i64)> {
        self.append_tool_chain_with_checkpoints(channel, chat_id, asst, tool_result, 0, &[])
    }

    /// 工具链两条 + 本轮 rewind checkpoint 在**同一事务**提交。
    ///
    /// `checkpoints` 为空时等价于 [`Self::append_tool_chain`]。
    pub fn append_tool_chain_with_checkpoints(
        &self,
        channel: &str,
        chat_id: &str,
        asst: &Message,
        tool_result: &Message,
        group_id: i64,
        checkpoints: &[FileCheckpointRecord],
    ) -> Result<(i64, i64)> {
        self.append_tool_chain_full(
            channel,
            chat_id,
            asst,
            tool_result,
            group_id,
            checkpoints,
            &[],
        )
    }

    /// 工具链两条 + rewind checkpoint + 工具调用侧表记录在**同一事务**提交。
    ///
    /// `metas` 锚定 assistant(tool_calls) 行 id（入参所在行 = 面板条目的身份）；
    /// 同 id 重复时后者覆盖，侧表永远不让链写入失败。
    #[allow(clippy::too_many_arguments)]
    pub fn append_tool_chain_full(
        &self,
        channel: &str,
        chat_id: &str,
        asst: &Message,
        tool_result: &Message,
        group_id: i64,
        checkpoints: &[FileCheckpointRecord],
        metas: &[ToolCallMetaRecord],
    ) -> Result<(i64, i64)> {
        self.db.transaction(|tx| {
            let sid = ensure_session(tx, channel, chat_id)?;
            let a = insert_message(tx, sid, asst, None)?;
            let t = insert_message(tx, sid, tool_result, None)?;
            // checkpoint 锚定同事务的 tool_result 消息行（v2）——按消息锚点回滚文件
            // 的事实源。
            insert_checkpoints(tx, sid, group_id, checkpoints, Some(t))?;
            insert_tool_call_metas(tx, sid, a, metas)?;
            Ok((a, t))
        })
    }

    /// 追加一条 compaction boundary（非破坏性压缩），返回其 id。
    ///
    /// - `summary`：独立形态的摘要消息（`role=user`，正文为摘要块）；
    /// - `head_until`：受保护头部最后一条消息的 id（无受保护头部则 `None`）；
    /// - `tail_from`：尾窗第一条保留消息的 id。
    ///
    /// 之后 [`Self::load_active`] 的视图 = `id <= head_until` ++ `[boundary]` ++
    /// `tail_from <= id < boundary.id`（普通行） ++ `id > boundary.id`。被摘要的中段与
    /// 更早的 boundary 行仍留在库里，但不再出现在视图中。
    ///
    /// 锚点在同一事务内校验（视图是裸 id 区间拼接，错锚点会静默丢掉整段保留窗口或让已
    /// 摘要的中段复活）：`tail_from` / `head_until` 必须是本会话的普通消息；已有 boundary
    /// 时 `tail_from` 不得回退到上一 boundary 的尾窗之前、受保护头部不得扩大。
    pub fn append_compaction(
        &self,
        channel: &str,
        chat_id: &str,
        summary: &Message,
        head_until: Option<i64>,
        tail_from: i64,
    ) -> Result<i64> {
        if head_until.is_some_and(|h| h >= tail_from) {
            return Err(StoreError::Other(format!(
                "compaction 锚点非法：head_until={head_until:?} >= tail_from={tail_from}"
            )));
        }
        let meta = CompactionMeta {
            head_until,
            tail_from,
        }
        .to_json();
        self.db.transaction(|tx| {
            let Some(sid) = session_id(tx, channel, chat_id)? else {
                return Err(StoreError::Other(format!(
                    "compaction 锚点非法：会话 {channel}/{chat_id} 不存在"
                )));
            };
            validate_compaction_anchors(tx, sid, head_until, tail_from)?;
            insert_message(tx, sid, summary, Some(&meta))
        })
    }

    /// 往分叉出的子会话追加一条「分叉告知」行（模型可见，前端不回放）。
    ///
    /// `role=user` + `meta={"fork_notice":…}`：`load_history`（模型上下文）不按 role/meta
    /// 过滤，模型读得到；回放过滤（`REPLAYABLE_FILTER_SQL`，meta IS NULL）把它排除——
    /// 前端历史 / 快照 / 增量都不显示，也不参与首条消息派生标题。仿 ZCode
    /// `persistSyntheticUserNotice`：告诉模型这是分叉出的新分支，别把父会话后续的
    /// 事当成自己经历过。`files_note` 是文件回滚结果的一句话描述（运行时按实情拼）。
    pub fn append_fork_notice(
        &self,
        channel: &str,
        chat_id: &str,
        parent_chat_id: &str,
        anchor_message_id: i64,
        files_note: &str,
    ) -> Result<i64> {
        let meta = serde_json::json!({
            "fork_notice": { "parent": parent_chat_id, "anchor": anchor_message_id }
        })
        .to_string();
        let text = format!(
            "【会话分叉】本会话自父会话 `{parent_chat_id}` 的消息 #{anchor_message_id} 分叉而来，\
             上面的历史继承自父会话。{files_note}\n\
             请从本分叉继续：不要假设分叉点之后的消息在本会话发生过。"
        );
        self.db.transaction(|tx| {
            let Some(sid) = session_id(tx, channel, chat_id)? else {
                return Err(StoreError::Other(format!(
                    "分叉告知写入失败：会话 {channel}/{chat_id} 不存在"
                )));
            };
            insert_message(tx, sid, &Message::plain(Role::User, text), Some(&meta))
        })
    }

    // ---------------------------------------------------------------------
    // 读取
    // ---------------------------------------------------------------------

    /// 活动视图（按最后一个 compaction boundary 拼接）+ 工具链一致性修复，不按条数 / 体积裁剪。
    ///
    /// 拼接要读 boundary 与三四段消息行，放在一个读快照里（`Db::read_snapshot`）：另一进程
    /// 在语句之间提交的 boundary 不会撕裂视图。
    pub fn load_active(&self, channel: &str, chat_id: &str) -> Result<Vec<Message>> {
        let raw = self.db.read_snapshot(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            load_active_raw(conn, sid, None)
        })?;
        let mut msgs = parse_rows(raw);
        while msgs.first().is_some_and(|m| m.is_tool_result_carrier()) {
            msgs.remove(0);
        }
        Ok(repair_tool_chains(msgs))
    }

    /// [`Self::load_active`] 的别名（runtime 语义压缩流程沿用此名）。
    pub fn load_all(&self, channel: &str, chat_id: &str) -> Result<Vec<Message>> {
        self.load_active(channel, chat_id)
    }

    /// 读取历史：活动视图裁剪到最近 `max_msgs` 条、跳过开头孤儿 tool_result，
    /// 再按 `max_bytes`（序列化后）做二次裁剪，最后做工具链一致性修复。
    ///
    /// 条数裁剪下推到 SQL（只取活动视图末尾 `max_msgs` 行）：长会话不必整表读出、解析再
    /// 丢掉前面绝大部分。结果与先读全量再裁相同（都是「活动视图最后 `max_msgs` 条」），
    /// 唯一差别是无法解析的坏行：SQL 按行计数，坏行被跳过后不会再往前多补一条。
    /// 与 [`Self::load_active`] 一样在一个读快照里拼接。
    pub fn load_history(
        &self,
        channel: &str,
        chat_id: &str,
        max_msgs: usize,
        max_bytes: usize,
    ) -> Result<Vec<Message>> {
        let raw = self.db.read_snapshot(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            load_active_raw(conn, sid, Some(max_msgs.max(1)))
        })?;
        Ok(trim_history(parse_rows(raw), max_msgs, max_bytes))
    }

    /// 完整 transcript：该会话全部行按 id 升序（含被摘要的中段与所有 boundary），不做修复。
    pub fn load_transcript(&self, channel: &str, chat_id: &str) -> Result<Vec<Message>> {
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            query_messages(
                conn,
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY id",
                params![sid],
            )
        })
    }

    /// 完整 transcript 的末尾：最近 `max_msgs` 条【可回放】行（id 升序），再按 `max_bytes`
    /// （逐条序列化后累计）从最旧一端裁、至少留一条。给「恢复转录」这类面向人的回放用——
    /// 压缩是非破坏性的，回放不该拿 provider 可见的压缩视图当聊天记录（压缩点处会无提示地
    /// 缺一段），所以按 transcript 取、含被摘要的中段。
    ///
    /// 「可回放」在 SQL 层就筛掉回放用不到的行：compaction boundary 行（`meta IS NOT NULL`，
    /// 正文是摘要）、`role=tool` 行与带 tool_result 块的 user 行（工具结果载体）——否则一个
    /// 工具密集的会话（每次迭代两行载体）会让 `max_msgs` 大半被它们占掉，回放只剩寥寥几条
    /// 对话。不做工具链修复（回放只渲染文本与工具名，不喂模型）。
    ///
    /// JSON 函数只对合法的 `data` 求值（`CASE` 保证求值顺序，`AND` 项可能被规划器重排）：
    /// 手改坏的一行只被跳过（与 `parse_message` 对坏行的处理一致），不让整条查询报错把
    /// 面板的历史回放打断；`content` 元素里非对象的也不喂给 `json_extract`。
    pub fn load_transcript_tail(
        &self,
        channel: &str,
        chat_id: &str,
        max_msgs: usize,
        max_bytes: usize,
    ) -> Result<Vec<Message>> {
        let msgs = self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            transcript_tail_on(conn, sid, max_msgs)
        })?;
        Ok(trim_tail_by_bytes(msgs, max_bytes))
    }

    /// 历史快照 + 行级游标：尾部窗口（与 [`Self::load_transcript_tail`] 同口径）与
    /// 「该会话当前最大消息行 id」取自同一读快照。给桌面端首开 / 重建视图用：
    /// 拿 `cursor` 作 `last_seen_row_id`，之后用 [`Self::load_chat_delta`] 按游标续读，
    /// 视图 = 窗口投影 + 其后增量，重连不重拉全量。
    pub fn load_chat_snapshot(
        &self,
        channel: &str,
        chat_id: &str,
        max_msgs: usize,
        max_bytes: usize,
    ) -> Result<ChatSnapshot> {
        self.db.read_snapshot(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(ChatSnapshot {
                    messages: Vec::new(),
                    cursor: 0,
                });
            };
            let messages = trim_tail_by_bytes(transcript_tail_on(conn, sid, max_msgs)?, max_bytes);
            let cursor = max_message_id(conn, sid)?;
            Ok(ChatSnapshot { messages, cursor })
        })
    }

    /// 行级增量：`after_id` 之后落库的可回放行（id 升序，最多 `limit` 条）+ 消费游标
    /// （语义见 [`ChatDelta`]）。行集与游标取自同一读快照：并发写入的行 id 更大，
    /// 下一次续读自然覆盖，不重不漏。
    ///
    /// 与快照窗口不同，这里【不做字节裁剪】——被裁掉的行游标已声明消费，丢了就永远
    /// 补不回来；截断只按条数（`more`），由调用方续拉。页满与否按【原始 SQL 行】计
    /// （坏 JSON 行被跳过也占页额度），否则一页坏行会让游标越页推进、跳过其后全部好行。
    pub fn load_chat_delta(
        &self,
        channel: &str,
        chat_id: &str,
        after_id: i64,
        limit: usize,
    ) -> Result<ChatDelta> {
        self.db.read_snapshot(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(ChatDelta {
                    messages: Vec::new(),
                    cursor: 0,
                    more: false,
                });
            };
            let mut stmt = conn.prepare(&format!(
                "SELECT id, data FROM message \
                 WHERE session_id = ?1 AND id > ?2 AND {REPLAYABLE_FILTER_SQL} \
                 ORDER BY id LIMIT ?3"
            ))?;
            let mut rows = stmt.query(params![sid, after_id, limit.max(1) as i64])?;
            let mut messages = Vec::new();
            let mut raw_count = 0usize;
            let mut last_raw_id = after_id;
            while let Some(row) = rows.next()? {
                let id: i64 = row.get(0)?;
                let data: String = row.get(1)?;
                raw_count += 1;
                last_raw_id = id;
                if let Some(m) = parse_message(id, &data) {
                    messages.push(m);
                }
            }
            let full = raw_count >= limit.max(1);
            let cursor = match raw_count {
                // 批满：游标只能推进到本批最后一行（之后可能还有，`more` 让调用方续拉）；
                // 不满：一次消费到头（含被过滤器筛掉的行），游标直达当前最大行 id。
                0 => max_message_id(conn, sid)?,
                _ if full => last_raw_id,
                _ => max_message_id(conn, sid)?,
            };
            Ok(ChatDelta {
                messages,
                cursor,
                more: full,
            })
        })
    }

    /// 完整 transcript 连同每行的 `meta`（compaction boundary 行为 `Some`）。
    fn load_transcript_with_meta(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> Result<Vec<(Message, Option<String>)>> {
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            let mut stmt = conn.prepare_cached(
                "SELECT id, data, meta FROM message WHERE session_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![sid], |row| {
                Ok((row_to_message(row)?, row.get::<_, Option<String>>(2)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (message, meta) = row?;
                if let Some(message) = message {
                    out.push((message, meta));
                }
            }
            Ok(out)
        })
    }

    /// 最早的若干条【真实】`role=user` 消息（桌面版抽标题）。
    ///
    /// 在 SQL 层就排除 compaction boundary 行（`meta IS NOT NULL`，正文是摘要）和
    /// tool_result 载体（`role=user` 但内容块全是工具结果）：否则 `limit` 会被它们占满，
    /// 一个长会话压缩几次、开头几轮全是工具调用之后就抽不到标题。
    pub fn first_user_messages(
        &self,
        channel: &str,
        chat_id: &str,
        limit: usize,
    ) -> Result<Vec<Message>> {
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            query_messages(
                conn,
                "SELECT id, data FROM message WHERE session_id = ?1 AND role = 'user' \
                 AND meta IS NULL \
                 AND NOT EXISTS (SELECT 1 FROM json_each(message.data, '$.content') AS b \
                                 WHERE json_extract(b.value, '$.type') = 'tool_result') \
                 ORDER BY id LIMIT ?2",
                params![sid, limit as i64],
            )
        })
    }

    // ---------------------------------------------------------------------
    // 会话管理
    // ---------------------------------------------------------------------

    pub fn session_exists(&self, channel: &str, chat_id: &str) -> Result<bool> {
        self.db
            .with(|conn| Ok(session_id(conn, channel, chat_id)?.is_some()))
    }

    /// 列出会话（`channel = None` 为全部），按 `updated_at` 降序。
    pub fn list_sessions(&self, channel: Option<&str>) -> Result<Vec<SessionSummary>> {
        self.db.with(|conn| {
            let sql = "SELECT s.channel, s.chat_id, s.archived_at, s.created_at, s.updated_at, \
                       (SELECT count(*) FROM message m WHERE m.session_id = s.id) \
                       FROM session s WHERE (?1 IS NULL OR s.channel = ?1) \
                       ORDER BY s.updated_at DESC, s.id DESC";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(params![channel], row_to_summary)?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// 单个会话的摘要行（按唯一键直查，不扫整张 session 表）。
    pub fn session_summary(&self, channel: &str, chat_id: &str) -> Result<Option<SessionSummary>> {
        self.db.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT s.channel, s.chat_id, s.archived_at, s.created_at, s.updated_at, \
                     (SELECT count(*) FROM message m WHERE m.session_id = s.id) \
                     FROM session s WHERE s.channel = ?1 AND s.chat_id = ?2",
                    params![channel, chat_id],
                    row_to_summary,
                )
                .optional()?)
        })
    }

    /// 归档 / 取消归档。返回会话是否存在（不存在视为无事可做，调用方按幂等处理）。
    pub fn set_archived(&self, channel: &str, chat_id: &str, archived: bool) -> Result<bool> {
        let now = now_ms();
        self.db.with(|conn| {
            let n = conn.execute(
                "UPDATE session SET archived_at = ?3 WHERE channel = ?1 AND chat_id = ?2",
                params![channel, chat_id, archived.then_some(now)],
            )?;
            Ok(n > 0)
        })
    }

    /// 删除会话（消息 / checkpoint 级联；usage 锚点、会话目标与 fork 血缘 kv 一并删除，
    /// artifact 目录级联清理）。返回会话是否存在。
    pub fn delete_session(&self, channel: &str, chat_id: &str) -> Result<bool> {
        let existed = self.db.transaction(|tx| {
            tx.execute(
                "DELETE FROM usage_anchor WHERE channel = ?1 AND chat_id = ?2",
                params![channel, chat_id],
            )?;
            // 会话级 kv（/goal 目标、fork 血缘）随会话删除：目标活不过它的会话；血缘
            // 行留着会让 chat_id 复用时「新会话被当成旧分叉」。
            tx.execute(
                "DELETE FROM kv WHERE key IN (?1, ?2)",
                params![
                    session_goal_key(channel, chat_id),
                    fork_lineage_key(channel, chat_id)
                ],
            )?;
            let n = tx.execute(
                "DELETE FROM session WHERE channel = ?1 AND chat_id = ?2",
                params![channel, chat_id],
            )?;
            Ok(n > 0)
        })?;
        if existed {
            remove_artifact_dir(&self.layout, channel, chat_id);
        }
        Ok(existed)
    }

    /// 从任意消息锚点 fork 出【新会话】（ZCode `forkStableConversationAtMessage` 同源语义，
    /// 第十二轮）：把源会话【活动视图】里 id ≤ 锚点的消息原样复制进一个新 chat_id 的会话，
    /// 原会话一个字节都不动（append-only；运行中的 turn 继续往原会话追加）。
    ///
    /// - 锚点必须是活动视图里的消息行——被压缩摘要折叠掉的消息不可作锚（它已不在
    ///   「用户所见历史」里，与 ZCode rewind 的 Unavailable 策略同判），明确报错。
    /// - 视图里的 compaction boundary 行按【普通行】复制（摘要正文保留，旧 meta 锚点
    ///   引用的是源会话的 id，带过去即坏；其余 meta 行——如 fork 告知行——原样保留）——
    ///   新会话的可见历史 = fork 时刻源会话的可见历史，不再折叠。
    /// - 血缘写进同一事务的 kv：`session-fork/<channel>/<new_chat_id>` =
    ///   `{parent, anchor}`（查回用 [`Self::fork_parent`]）。
    /// - `new_chat_id` 由调用方给出（桌面命令/斜杠命令负责生成与字符集校验）；已存在
    ///   则报错——fork 是显式动作，不静默覆盖别人的历史。
    /// - 消息体里引用源会话 artifact（工具结果落盘句柄）的路径统一改写到新会话自己的
    ///   artifact 目录（事务提交后整目录复制）——否则父会话删除会连带删掉新分支还在
    ///   引用的全文，转录里只剩预览。
    /// - 整个复制在一个事务里：中途失败新会话一个字节都不留。
    pub fn fork_session(
        &self,
        channel: &str,
        chat_id: &str,
        anchor_message_id: i64,
        new_chat_id: &str,
    ) -> Result<SessionFork> {
        let now = now_ms();
        // 句柄重写前缀：与落盘侧（runtime spill）同构造——Path::join 归一尾部分隔符
        // （base = "agent/" 时手拼字符串会出现 `agent//artifacts`，与落盘路径
        // `agent/artifacts` 对不上，重写永远不命中）+ 正斜杠 + 相对 base 按进程
        // CWD 展开（spill 的 absolute_base 同一假设）+ 尾斜杠（消息里路径必带文件名）。
        let source_prefix = self.artifacts_dir_prefix(channel, chat_id);
        let target_prefix = self.artifacts_dir_prefix(channel, new_chat_id);
        let fork = self.db.transaction(|tx| {
            if session_id(tx, channel, new_chat_id)?.is_some() {
                return Err(StoreError::Other(format!(
                    "会话 {channel}/{new_chat_id} 已存在，fork 拒绝覆盖"
                )));
            }
            let Some(sid) = session_id(tx, channel, chat_id)? else {
                return Err(StoreError::Other(format!(
                    "源会话 {channel}/{chat_id} 不存在"
                )));
            };
            // 复制源 = 活动视图前缀：锚点必须在视图里，被压缩折叠的消息不带走。
            let prefix: Vec<(i64, String)> = {
                let view = load_active_raw(tx, sid, None)?;
                let Some(anchor_pos) = view
                    .iter()
                    .position(|(id, _)| *id == anchor_message_id)
                else {
                    return Err(StoreError::Other(format!(
                        "锚点消息 {anchor_message_id} 不在会话 {channel}/{chat_id} 的活动视图里\
                         （可能已被压缩折叠进摘要）。请换一条仍可见的消息"
                    )));
                };
                // 视图顺序 = 会话顺序（head ++ [boundary] ++ kept ++ after）；按锚点在视图
                // 中的【位置】取前缀——boundary 行的 id 最大，按 id 大小截断会把 boundary
                // 连同它之后的尾窗错切掉。
                view[..=anchor_pos].to_vec()
            };
            // 锚点存在性（视图定位已保证命中）：前缀末行必须是锚点本身。
            if prefix.last().is_none_or(|(id, _)| *id != anchor_message_id) {
                return Err(StoreError::Other(format!(
                    "锚点消息 {anchor_message_id} 不在会话 {channel}/{chat_id} 的转录里"
                )));
            }
            tx.execute(
                "INSERT INTO session(channel, chat_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                params![channel, new_chat_id, now],
            )?;
            let new_sid = session_id(tx, channel, new_chat_id)?.ok_or_else(|| {
                StoreError::Other(format!("会话行创建后不可见: {channel}/{new_chat_id}"))
            })?;
            // role/ts/meta 从源行随取（data 里的 ts 是序列化冗余，列值才是事实源）。
            let mut stmt = tx.prepare_cached(
                "SELECT role, ts, meta FROM message WHERE session_id = ?1 AND id = ?2",
            )?;
            for (id, data) in &prefix {
                let (role, ts, meta): (String, i64, Option<String>) =
                    stmt.query_row(params![sid, id], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                    })?;
                // artifact 句柄重写：命中源会话目录前缀的路径改指新会话目录（事务提交后
                // 整目录复制补上实体文件）；不含句柄的行原样复制。
                let data = if data.contains(source_prefix.as_str()) {
                    data.replace(source_prefix.as_str(), &target_prefix)
                } else {
                    data.clone()
                };
                // boundary 行降级为普通行（旧 meta 锚点引用源会话 id，带过去即坏）；
                // 其余 meta（如 fork 告知行）原样保留——降级成 NULL 会让它以普通用户
                // 消息的形态混进前端回放。
                let meta = match meta {
                    Some(raw) if CompactionMeta::parse(&raw).is_some() => None,
                    other => other,
                };
                tx.execute(
                    "INSERT INTO message(session_id, role, ts, data, meta) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![new_sid, role, ts, data, meta],
                )?;
                // 工具调用侧表随行复制（键含行 id，重映射到新行）：新分支的面板同样能
                // 看到 skill 归属 / 耗时 / 成败。checkpoint 不复制（既定：分叉即回滚点）。
                let new_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT OR REPLACE INTO tool_call_meta(session_id, message_id, tool_use_id, meta) \
                     SELECT ?1, ?2, tool_use_id, meta FROM tool_call_meta \
                     WHERE session_id = ?3 AND message_id = ?4",
                    params![new_sid, new_id, sid, id],
                )?;
            }
            let lineage = serde_json::json!({
                "parent": chat_id,
                "anchor": anchor_message_id,
            })
            .to_string();
            tx.execute(
                "INSERT INTO kv(key, value, updated_at) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![fork_lineage_key(channel, new_chat_id), lineage, now],
            )?;
            Ok(SessionFork {
                new_chat_id: new_chat_id.to_string(),
                copied: prefix.len(),
                anchor_message_id,
            })
        })?;
        // 事务提交成功后才复制 artifact 目录（复制失败不回滚 fork：句柄丢失是可感知、
        // 可重新执行工具的小损失，会话分叉本身不该因此失败）。
        self.copy_artifact_dir(channel, chat_id, new_chat_id);
        Ok(fork)
    }

    /// fork 时把源会话的 artifact 目录整份复制到新会话键下（配合消息体里的路径重写，
    /// 新分支的落盘句柄自持，父会话删除不再牵连）。best-effort：源目录不存在静默跳过，
    /// 复制失败只告警。
    fn copy_artifact_dir(&self, channel: &str, source_chat: &str, target_chat: &str) {
        let source = self
            .layout
            .base
            .join("artifacts")
            .join(artifact_chat_key(channel, source_chat));
        let target = self
            .layout
            .base
            .join("artifacts")
            .join(artifact_chat_key(channel, target_chat));
        if !source.is_dir() {
            return;
        }
        if let Err(error) = std::fs::create_dir_all(&target) {
            tracing::warn!("artifact 目录复制失败（{}）: {error}", target.display());
            return;
        }
        let entries = match std::fs::read_dir(&source) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!("artifact 目录读取失败（{}）: {error}", source.display());
                return;
            }
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            // 只复制 artifact 实体（`<id>-<tool>.txt`）：atomic_write 并发写入中的
            // `.<名>.<nonce>.tmp` sidecar / 崩溃残留的孤儿 tmp 以 `.` 开头，半截内容
            // 不该被当成新分支的句柄实体带走。
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if let Err(error) = std::fs::copy(entry.path(), target.join(entry.file_name())) {
                tracing::warn!(
                    "artifact 文件复制失败（{}）: {error}",
                    entry.path().display()
                );
            }
        }
    }

    /// 消息体里 artifact 句柄的目录前缀（带尾斜杠、正斜杠形态）：与 runtime spill 的
    /// display 路径同构造——Path::join 归一 base 尾部分隔符（手拼字符串会出现
    /// `agent//artifacts`，与落盘路径对不上、重写永不命中），相对 base 按进程 CWD 展开。
    fn artifacts_dir_prefix(&self, channel: &str, chat_id: &str) -> String {
        let base = &self.layout.base;
        let base = if base.is_absolute() {
            base.clone()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(base))
                .unwrap_or_else(|_| base.clone())
        };
        format!(
            "{}/",
            base.join("artifacts")
                .join(artifact_chat_key(channel, chat_id))
                .to_string_lossy()
                .replace('\\', "/")
        )
    }

    /// fork 出的会话的父会话（非 fork 会话 / 血缘损坏 → None）。
    pub fn fork_parent(&self, channel: &str, chat_id: &str) -> Result<Option<ForkLineage>> {
        let raw = self.db.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT value FROM kv WHERE key = ?1",
                    params![fork_lineage_key(channel, chat_id)],
                    |row| row.get::<_, String>(0),
                )
                .optional()?)
        })?;
        let Some(raw) = raw else {
            return Ok(None);
        };
        let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
            StoreError::Other(format!("fork 血缘不可解析: {channel}/{chat_id}: {e}"))
        })?;
        Ok(Some(ForkLineage {
            parent_chat_id: value
                .get("parent")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
            anchor_message_id: value
                .get("anchor")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or_default(),
        }))
    }

    /// 最近若干条【真实用户消息】（id + 时间 + 文本预览）：`/fork` 无参列表的锚点候选。
    ///
    /// 与 `first_user_messages` 同一筛选（排除 boundary 行与 tool_result 载体），方向相反
    /// （新在前）。文本块拼接后取前 120 字符做预览。
    pub fn recent_user_anchors(
        &self,
        channel: &str,
        chat_id: &str,
        limit: usize,
    ) -> Result<Vec<(i64, i64, String)>> {
        let rows = self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            // 与 `recent_window` 同一套 JSON 防护：手改坏的行用 json_valid 挡在 SQL 层
            // （否则 json_each 对非法 JSON 直接让整条查询报错，/fork 列表整体失效）；
            // content 不是数组时 json_each 给出标量行，CASE 不去 json_extract。
            let mut stmt = conn.prepare_cached(
                "SELECT id, ts, data FROM message WHERE session_id = ?1 AND role = 'user' \
                 AND meta IS NULL \
                 AND CASE WHEN json_valid(message.data) \
                          THEN NOT EXISTS (SELECT 1 FROM json_each(message.data, '$.content') AS b \
                                           WHERE CASE WHEN b.type = 'object' \
                                                      THEN json_extract(b.value, '$.type') END = 'tool_result') \
                          ELSE 0 END \
                 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![sid, limit.max(1) as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })?;
        Ok(rows
            .into_iter()
            .map(|(id, ts, data)| {
                let preview = parse_message(id, &data)
                    .map(|m| {
                        m.content
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<String>()
                    })
                    .unwrap_or_default();
                let preview: String = preview.trim().chars().take(120).collect();
                (id, ts, preview)
            })
            .collect())
    }

    /// `/clear`：清空全部会话级数据（会话 / 消息 / checkpoint / usage 锚点）并 `VACUUM`。
    /// journal / kv 不受影响。
    ///
    /// 删除事务提交后数据对所有读者立即不可见；`secure_delete=ON` 把【主库】里的删除页
    /// 清零，WAL 文件里的旧页面则要等 [`Db::vacuum`] 末尾的 TRUNCATE checkpoint 回写后才
    /// 消失——所以「磁盘上不留痕」是尽力而为：VACUUM / checkpoint 失败（BUSY / 磁盘满）
    /// 不应让调用方误报「清空失败」，仅告警。大库上 VACUUM 需要整库重写，调用方应放到
    /// `spawn_blocking`。
    pub fn clear_all_sessions(&self) -> Result<()> {
        self.db.transaction(|tx| {
            tx.execute_batch(
                "DELETE FROM tool_call_meta; DELETE FROM file_checkpoint; DELETE FROM message; \
                 DELETE FROM session; DELETE FROM usage_anchor;",
            )?;
            // 会话全清，会话级 kv（/goal 目标、fork 血缘）一并删除——否则 /clear 后旧
            // 目标还挂在 kv 里，goal reminder 会把它注进「全新」的会话；血缘行会让复用
            // chat_id 的新会话被当成旧分叉。plan / workflow 等运行时 journal 与其 kv
            // 不属会话，保留。
            tx.execute(
                "DELETE FROM kv WHERE key LIKE 'session-goal/%' OR key LIKE 'session-fork/%'",
                [],
            )?;
            Ok(())
        })?;
        if let Err(e) = self.db.vacuum() {
            tracing::warn!("state.sqlite VACUUM 失败（数据已删除，仅文件体积未回收）: {e}");
        }
        // 会话全清，工具结果落盘的 artifact 目录一并清空（best-effort）。只删形如
        // `<会话键>-<16 hex>` 的会话目录（[`artifact_chat_key`] 的产物）：根目录是用户可
        // 配置的 agent 根下的子目录，整棵 remove_dir_all 会把用户放进去的别的东西也抹掉。
        let artifacts_root = self.layout.base.join("artifacts");
        let entries = match std::fs::read_dir(&artifacts_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                tracing::warn!(
                    "artifacts 根目录读取失败（{}）: {error}",
                    artifacts_root.display()
                );
                return Ok(());
            }
        };
        for entry in entries.flatten() {
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if !is_artifact_chat_dir_name(&entry.file_name().to_string_lossy()) {
                continue;
            }
            if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(
                    "artifact 目录清理失败（{}）: {error}",
                    entry.path().display()
                );
            }
        }
        Ok(())
    }

    /// 活动 / 归档会话计数。
    pub fn session_stats(&self) -> Result<SessionStats> {
        self.db.with(|conn| {
            let (active, archived): (i64, i64) = conn.query_row(
                "SELECT sum(archived_at IS NULL), sum(archived_at IS NOT NULL) FROM session",
                [],
                |row| {
                    Ok((
                        row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                        row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                    ))
                },
            )?;
            Ok(SessionStats {
                active: active.max(0) as u32,
                archived: archived.max(0) as u32,
            })
        })
    }

    /// 逐会话遍历消息（transcript 顺序），供统计类调用方聚合而不必整库加载。
    ///
    /// 每个会话单独持锁一次：全库扫描耗时与库体积成正比，不能让运行中 turn 的追加
    /// 等整个扫描结束（连接是单把 `Mutex`）。回调里不要再调 store 方法。
    pub fn scan_messages(
        &self,
        channel: Option<&str>,
        mut visit: impl FnMut(&SessionSummary, &Message),
    ) -> Result<()> {
        for summary in self.list_sessions(channel)? {
            for message in self.load_transcript(&summary.channel, &summary.chat_id)? {
                visit(&summary, &message);
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------------
    // 导出 / 搜索
    // ---------------------------------------------------------------------

    /// 把完整 transcript 导出为 JSONL：每行 = `Message` 的 serde JSON（含 `id`）；
    /// compaction boundary 行额外带 `meta`（`{"compaction": {head_until, tail_from}}`），
    /// 导出文件据此可区分摘要行与普通 user 消息、重建活动视图。
    pub fn export_transcript_jsonl(&self, channel: &str, chat_id: &str) -> Result<String> {
        let mut out = String::new();
        for (msg, meta) in self.load_transcript_with_meta(channel, chat_id)? {
            let mut value = serde_json::to_value(&msg)?;
            if let Some(meta) = meta
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            {
                if let Some(object) = value.as_object_mut() {
                    object.insert("meta".to_string(), meta);
                }
            }
            out.push_str(&serde_json::to_string(&value)?);
            out.push('\n');
        }
        Ok(out)
    }

    /// 全库消息粗筛：文本块（`content[].type == "text"`）里同时包含全部 `terms` 的普通消息行
    /// （Unicode 大小写不敏感，经连接级 `rclaw_lower`），按 id 降序（新消息优先），最多
    /// `max_rows` 行。只看解码后的文本块，不看 JSON 原文——`"`/`\` 等被转义的词、JSON 骨架
    /// 字段名（`text` / `role`）、工具参数与 base64 媒体都不会误命中；boundary 行与
    /// `role=tool` 结果不参与。精确计分（词频、片段）由调用方在返回的消息上完成，
    /// 口径同为 `str::to_lowercase`，粗筛结果是精筛的超集。
    pub fn search_messages(&self, terms: &[String], max_rows: usize) -> Result<Vec<SearchRow>> {
        let terms: Vec<String> = terms
            .iter()
            .map(|t| t.trim().to_lowercase())
            .filter(|t| !t.is_empty())
            .collect();
        if terms.is_empty() {
            return Ok(Vec::new());
        }
        let mut sql = String::from(
            "SELECT s.channel, s.chat_id, s.archived_at, m.id, m.data FROM message m \
             JOIN session s ON s.id = m.session_id WHERE m.meta IS NULL AND m.role <> 'tool'",
        );
        for i in 0..terms.len() {
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM json_each(m.data, '$.content') AS b \
                 WHERE json_extract(b.value, '$.type') = 'text' \
                 AND instr(rclaw_lower(json_extract(b.value, '$.text')), ?{}) > 0)",
                i + 1
            ));
        }
        // 新消息优先：候选上限截断时保留最近的命中。
        sql.push_str(&format!(" ORDER BY m.id DESC LIMIT ?{}", terms.len() + 1));
        self.db.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut binds: Vec<rusqlite::types::Value> = terms
                .iter()
                .map(|t| rusqlite::types::Value::Text(t.clone()))
                .collect();
            binds.push(rusqlite::types::Value::Integer(max_rows.max(1) as i64));
            let rows = stmt.query_map(rusqlite::params_from_iter(binds), |row| {
                let channel: String = row.get(0)?;
                let chat_id: String = row.get(1)?;
                let archived = row.get::<_, Option<i64>>(2)?.is_some();
                let id: i64 = row.get(3)?;
                let data: String = row.get(4)?;
                Ok((channel, chat_id, archived, id, data))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (channel, chat_id, archived, id, data) = row?;
                let Some(message) = parse_message(id, &data) else {
                    continue;
                };
                out.push(SearchRow {
                    channel,
                    chat_id,
                    archived,
                    message,
                });
            }
            Ok(out)
        })
    }

    // ---------------------------------------------------------------------
    // rewind 文件 checkpoint
    // ---------------------------------------------------------------------

    /// 追加一组 checkpoint（同一事务；工具链落库失败后的补写 / `/rewind` 重锚标记使用）。
    ///
    /// 不刷新会话 `updated_at`（它的语义是「最后一条消息的时间」，纯 checkpoint 写入不算
    /// 对话活动，不应把会话顶到列表最前）。
    pub fn append_file_checkpoints(
        &self,
        channel: &str,
        chat_id: &str,
        group_id: i64,
        checkpoints: &[FileCheckpointRecord],
    ) -> Result<()> {
        if checkpoints.is_empty() {
            return Ok(());
        }
        self.db.transaction(|tx| {
            let sid = ensure_session_untouched(tx, channel, chat_id)?;
            // 独立落库路径（cron / 取消清理的 flush）：无同事务消息可锚，anchor 为 None。
            insert_checkpoints(tx, sid, group_id, checkpoints, None)
        })
    }

    /// 该会话全部 checkpoint（含 `before` 正文），按行 id 升序（= 时间序）。
    pub fn load_file_checkpoints(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> Result<Vec<FileCheckpointRecord>> {
        self.load_file_checkpoints_from(channel, chat_id, 0)
    }

    /// 行 id `>= min_id` 的 checkpoint（含 `before` 正文），按行 id 升序。回滚集只需要
    /// 从选点行起的正文，更早的 checkpoint 不必读进内存。
    pub fn load_file_checkpoints_from(
        &self,
        channel: &str,
        chat_id: &str,
        min_id: i64,
    ) -> Result<Vec<FileCheckpointRecord>> {
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            let mut stmt = conn.prepare(
                "SELECT id, group_id, path, before, before_hash, after_hash, is_rewind, \
                 anchor_message_id FROM file_checkpoint WHERE session_id = ?1 AND id >= ?2 \
                 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![sid, min_id], |row| {
                Ok(FileCheckpointRecord {
                    id: row.get(0)?,
                    group_id: row.get(1)?,
                    path: row.get(2)?,
                    before: row.get(3)?,
                    before_hash: row.get(4)?,
                    after_hash: row.get(5)?,
                    is_rewind: row.get::<_, i64>(6)? != 0,
                    anchor_message_id: row.get(7)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    /// 行 id 在 `[min_id, max_id]` 内的 assistant(tool_calls) 行的工具调用侧表记录，
    /// 按行 id 归组（面板快照 / 增量批次的列表级充实：skill 徽标、耗时、成败）。
    /// 坏 meta 行告警跳过。
    pub fn load_tool_call_metas(
        &self,
        channel: &str,
        chat_id: &str,
        min_id: i64,
        max_id: i64,
    ) -> Result<HashMap<i64, Vec<ToolCallMetaRecord>>> {
        if min_id > max_id {
            return Ok(HashMap::new());
        }
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(HashMap::new());
            };
            let mut stmt = conn.prepare_cached(
                "SELECT message_id, meta FROM tool_call_meta \
                 WHERE session_id = ?1 AND message_id BETWEEN ?2 AND ?3 ORDER BY rowid",
            )?;
            let rows = stmt.query_map(params![sid, min_id, max_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut out: HashMap<i64, Vec<ToolCallMetaRecord>> = HashMap::new();
            for row in rows {
                let (message_id, raw) = row?;
                if let Some(meta) = parse_tool_call_meta(message_id, &raw) {
                    out.entry(message_id).or_default().push(meta);
                }
            }
            Ok(out)
        })
    }

    /// 一条 assistant(tool_calls) 行的工具链详情（展开面板）：入参行 + 紧随其后的
    /// tool_result 载体 + 侧表 + 锚在载体上的文件 checkpoint。
    ///
    /// `None` = 该行不存在 / 不属于该会话 / 不是含工具调用的 assistant 行。载体缺失
    /// （fork 以 tool_use 行为锚复制、链写入中断）时 `results` 为空而非报错。
    pub fn load_tool_chain_detail(
        &self,
        channel: &str,
        chat_id: &str,
        assistant_id: i64,
    ) -> Result<Option<ToolChainDetail>> {
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(None);
            };
            let assistant = conn
                .query_row(
                    "SELECT id, data FROM message WHERE session_id = ?1 AND id = ?2",
                    params![sid, assistant_id],
                    row_to_message,
                )
                .optional()?
                .flatten();
            let Some(assistant) = assistant else {
                return Ok(None);
            };
            if !assistant.has_tool_calls() {
                return Ok(None);
            }
            // 载体 = 会话内紧随其后的一行（工具链同事务写入，t == a + 1 在会话内成立；
            // 跨会话并发落库会让全局 id 不连续，所以按「本会话下一行」取）。
            let carrier = conn
                .query_row(
                    "SELECT id, data FROM message WHERE session_id = ?1 AND id > ?2 \
                     ORDER BY id LIMIT 1",
                    params![sid, assistant_id],
                    row_to_message,
                )
                .optional()?
                .flatten()
                .filter(|m| m.is_tool_result_carrier());
            let results = carrier
                .as_ref()
                .map(|m| {
                    m.content
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } => Some(ToolResultDetail {
                                tool_use_id: tool_use_id.clone(),
                                content: content.clone(),
                                is_error: *is_error,
                            }),
                            _ => None,
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut stmt = conn.prepare_cached(
                "SELECT meta FROM tool_call_meta WHERE session_id = ?1 AND message_id = ?2 \
                 ORDER BY rowid",
            )?;
            let metas = stmt
                .query_map(params![sid, assistant_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?
                .iter()
                .filter_map(|raw| parse_tool_call_meta(assistant_id, raw))
                .collect();
            let checkpoints = match carrier.as_ref() {
                Some(carrier) => {
                    let mut stmt = conn.prepare_cached(
                        "SELECT id, group_id, path, before, before_hash, after_hash, is_rewind, \
                         anchor_message_id FROM file_checkpoint \
                         WHERE session_id = ?1 AND anchor_message_id = ?2 ORDER BY id",
                    )?;
                    let rows = stmt.query_map(params![sid, carrier.id], |row| {
                        Ok(FileCheckpointRecord {
                            id: row.get(0)?,
                            group_id: row.get(1)?,
                            path: row.get(2)?,
                            before: row.get(3)?,
                            before_hash: row.get(4)?,
                            after_hash: row.get(5)?,
                            is_rewind: row.get::<_, i64>(6)? != 0,
                            anchor_message_id: row.get(7)?,
                        })
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                }
                None => Vec::new(),
            };
            Ok(Some(ToolChainDetail {
                assistant,
                results,
                metas,
                checkpoints,
            }))
        })
    }

    /// 该会话全部 checkpoint 的元数据（不含 `before` 正文），按行 id 升序。
    pub fn load_file_checkpoint_meta(
        &self,
        channel: &str,
        chat_id: &str,
    ) -> Result<Vec<FileCheckpointMeta>> {
        self.db.with(|conn| {
            let Some(sid) = session_id(conn, channel, chat_id)? else {
                return Ok(Vec::new());
            };
            let mut stmt = conn.prepare(
                "SELECT id, group_id, path, before_hash, after_hash, is_rewind, \
                 anchor_message_id FROM file_checkpoint WHERE session_id = ?1 ORDER BY id",
            )?;
            let rows = stmt.query_map(params![sid], |row| {
                Ok(FileCheckpointMeta {
                    id: row.get(0)?,
                    group_id: row.get(1)?,
                    path: row.get(2)?,
                    before_hash: row.get(3)?,
                    after_hash: row.get(4)?,
                    is_rewind: row.get::<_, i64>(5)? != 0,
                    anchor_message_id: row.get(6)?,
                })
            })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row?);
            }
            Ok(out)
        })
    }

    // ---------------------------------------------------------------------
    // usage 锚点
    // ---------------------------------------------------------------------

    pub fn load_usage_anchor(
        &self,
        channel: &str,
        chat_id: &str,
        cron_job_id: &str,
    ) -> Result<Option<UsageAnchorRecord>> {
        self.db.with(|conn| {
            Ok(conn
                .query_row(
                    "SELECT total_tokens, anchor_message_id FROM usage_anchor \
                     WHERE channel = ?1 AND chat_id = ?2 AND cron_job_id = ?3",
                    params![channel, chat_id, cron_job_id],
                    |row| {
                        Ok(UsageAnchorRecord {
                            total_tokens: row.get::<_, i64>(0)?.max(0) as usize,
                            anchor_message_id: row.get(1)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn save_usage_anchor(
        &self,
        channel: &str,
        chat_id: &str,
        cron_job_id: &str,
        anchor: &UsageAnchorRecord,
    ) -> Result<()> {
        self.db.with(|conn| {
            conn.execute(
                "INSERT INTO usage_anchor(channel, chat_id, cron_job_id, total_tokens, \
                 anchor_message_id, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6) \
                 ON CONFLICT(channel, chat_id, cron_job_id) DO UPDATE SET \
                 total_tokens = excluded.total_tokens, \
                 anchor_message_id = excluded.anchor_message_id, \
                 updated_at = excluded.updated_at",
                params![
                    channel,
                    chat_id,
                    cron_job_id,
                    anchor.total_tokens as i64,
                    anchor.anchor_message_id,
                    now_ms()
                ],
            )?;
            Ok(())
        })
    }

    pub fn delete_usage_anchor(
        &self,
        channel: &str,
        chat_id: &str,
        cron_job_id: &str,
    ) -> Result<()> {
        self.db.with(|conn| {
            conn.execute(
                "DELETE FROM usage_anchor WHERE channel = ?1 AND chat_id = ?2 AND cron_job_id = ?3",
                params![channel, chat_id, cron_job_id],
            )?;
            Ok(())
        })
    }
}

// -------------------------------------------------------------------------
// SQL 辅助
// -------------------------------------------------------------------------

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn session_id(conn: &Connection, channel: &str, chat_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT id FROM session WHERE channel = ?1 AND chat_id = ?2",
            params![channel, chat_id],
            |row| row.get(0),
        )
        .optional()?)
}

/// `session` 摘要行的列映射（`channel, chat_id, archived_at, created_at, updated_at, count`）。
fn row_to_summary(row: &Row<'_>) -> rusqlite::Result<SessionSummary> {
    Ok(SessionSummary {
        channel: row.get(0)?,
        chat_id: row.get(1)?,
        archived: row.get::<_, Option<i64>>(2)?.is_some(),
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        message_count: row.get::<_, i64>(5)?.max(0) as u32,
    })
}

/// 取（或创建）会话行并刷新 `updated_at`（追加消息时使用）。
///
/// `updated_at` 取 `max(旧值, now)`：时钟回拨不会让会话在列表里倒退（对齐 ZCode
/// `max(time_updated, ?)`）。
fn ensure_session(conn: &Connection, channel: &str, chat_id: &str) -> Result<i64> {
    let now = now_ms();
    conn.execute(
        "INSERT INTO session(channel, chat_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) \
         ON CONFLICT(channel, chat_id) DO UPDATE SET \
         updated_at = max(session.updated_at, excluded.updated_at)",
        params![channel, chat_id, now],
    )?;
    session_id(conn, channel, chat_id)?
        .ok_or_else(|| StoreError::Other(format!("会话行创建后不可见: {channel}/{chat_id}")))
}

/// 取（或创建）会话行但不刷新 `updated_at`（纯 checkpoint 写入等非对话活动）。
fn ensure_session_untouched(conn: &Connection, channel: &str, chat_id: &str) -> Result<i64> {
    if let Some(sid) = session_id(conn, channel, chat_id)? {
        return Ok(sid);
    }
    let now = now_ms();
    conn.execute(
        "INSERT INTO session(channel, chat_id, created_at, updated_at) VALUES (?1, ?2, ?3, ?3) \
         ON CONFLICT(channel, chat_id) DO NOTHING",
        params![channel, chat_id, now],
    )?;
    session_id(conn, channel, chat_id)?
        .ok_or_else(|| StoreError::Other(format!("会话行创建后不可见: {channel}/{chat_id}")))
}

/// 最后一个 compaction boundary 行:`(id, 已解析 meta)`。
///
/// 只认 meta 真为 compaction 形态的行(`meta` 列还装其它种类,如 fork 告知行)——
/// 裸 `meta IS NOT NULL` 会把最新一条任意 meta 行误当 boundary:视图拼接退化成完整
/// 历史、boundary 锚点校验被静默跳过。
fn latest_boundary(conn: &Connection, sid: i64) -> Result<Option<(i64, CompactionMeta)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, meta FROM message WHERE session_id = ?1 AND meta IS NOT NULL \
         ORDER BY id DESC LIMIT 32",
    )?;
    let rows = stmt.query_map(params![sid], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, raw): (i64, String) = row?;
        if let Some(meta) = CompactionMeta::parse(&raw) {
            return Ok(Some((id, meta)));
        }
    }
    Ok(None)
}

/// 该 id 是否是本会话的一条普通消息行（非 compaction boundary）。
///
/// `meta` 列里非 compaction 形态的行（如 fork 告知行）也是模型可见的普通行，
/// 可作压缩锚点（head_until / tail_from 落在它上合法）；boundary 行不行——
/// 视图拼接按它切三段，锚到它会让保留窗口错切。
fn plain_message_exists(conn: &Connection, sid: i64, id: i64) -> Result<bool> {
    let meta: Option<Option<String>> = conn
        .query_row(
            "SELECT meta FROM message WHERE session_id = ?1 AND id = ?2",
            params![sid, id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(meta.is_some_and(|meta| match meta {
        None => true,
        Some(raw) => CompactionMeta::parse(&raw).is_none(),
    }))
}

/// `append_compaction` 的锚点校验（见其文档）。
fn validate_compaction_anchors(
    conn: &Connection,
    sid: i64,
    head_until: Option<i64>,
    tail_from: i64,
) -> Result<()> {
    if !plain_message_exists(conn, sid, tail_from)? {
        return Err(StoreError::Other(format!(
            "compaction 锚点非法：tail_from={tail_from} 不是本会话的普通消息"
        )));
    }
    if let Some(head_until) = head_until {
        if !plain_message_exists(conn, sid, head_until)? {
            return Err(StoreError::Other(format!(
                "compaction 锚点非法：head_until={head_until} 不是本会话的普通消息"
            )));
        }
    }
    if let Some((prev_id, prev)) = latest_boundary(conn, sid)? {
        if tail_from < prev.tail_from {
            return Err(StoreError::Other(format!(
                "compaction 锚点非法：tail_from={tail_from} 回退到上一 boundary（id={prev_id}）\
                 的尾窗起点 {} 之前，已摘要的中段会复活",
                prev.tail_from
            )));
        }
        if let Some(head_until) = head_until {
            if prev
                .head_until
                .is_none_or(|prev_head| head_until > prev_head)
            {
                return Err(StoreError::Other(format!(
                    "compaction 锚点非法：head_until={head_until} 超出上一 boundary（id={prev_id}）\
                     的受保护头部 {:?}",
                    prev.head_until
                )));
            }
        }
    }
    Ok(())
}

fn insert_message(
    conn: &Connection,
    session_id: i64,
    message: &Message,
    meta: Option<&str>,
) -> Result<i64> {
    let mut msg = message.clone();
    // id 以行主键为准，`data` 里不存。
    msg.id = 0;
    if msg.ts == 0 {
        msg.ts = Utc::now().timestamp();
    }
    let data = serde_json::to_string(&msg)?;
    conn.execute(
        "INSERT INTO message(session_id, role, ts, data, meta) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![session_id, role_name(&msg.role), msg.ts, data, meta],
    )?;
    Ok(conn.last_insert_rowid())
}

fn insert_checkpoints(
    conn: &Connection,
    session_id: i64,
    group_id: i64,
    checkpoints: &[FileCheckpointRecord],
    anchor_message_id: Option<i64>,
) -> Result<()> {
    if checkpoints.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT INTO file_checkpoint(session_id, group_id, path, before, before_hash, \
         after_hash, is_rewind, anchor_message_id) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for cp in checkpoints {
        // 单条记录可自带 group_id（`/rewind` 的重锚标记用独立分组）；0 则跟随参数。
        let gid = if cp.group_id != 0 {
            cp.group_id
        } else {
            group_id
        };
        // 记录自带 anchor 时用它，否则继承事务级（工具链同事务的 tool_result 消息 id）。
        // 重锚标记本身无锚：它走 `append_file_checkpoints`，事务级也是 None。
        let anchor = cp.anchor_message_id.or(anchor_message_id);
        stmt.execute(params![
            session_id,
            gid,
            cp.path,
            cp.before,
            cp.before_hash,
            cp.after_hash,
            cp.is_rewind as i64,
            anchor
        ])?;
    }
    Ok(())
}

/// 工具调用侧表写入（与工具链同事务）。`INSERT OR REPLACE`：同一行内 provider 重复
/// tool_use id 时后者覆盖，不让主链写入因侧表失败。
fn insert_tool_call_metas(
    conn: &Connection,
    session_id: i64,
    message_id: i64,
    metas: &[ToolCallMetaRecord],
) -> Result<()> {
    if metas.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare_cached(
        "INSERT OR REPLACE INTO tool_call_meta(session_id, message_id, tool_use_id, meta) \
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    for meta in metas {
        let raw = serde_json::to_string(meta)?;
        stmt.execute(params![session_id, message_id, meta.tool_use_id, raw])?;
    }
    Ok(())
}

fn parse_tool_call_meta(message_id: i64, raw: &str) -> Option<ToolCallMetaRecord> {
    match serde_json::from_str::<ToolCallMetaRecord>(raw) {
        Ok(meta) => Some(meta),
        Err(e) => {
            tracing::warn!("tool_call_meta 行解析失败，已跳过: message_id={message_id} err={e}");
            None
        }
    }
}

fn parse_message(id: i64, data: &str) -> Option<Message> {
    match serde_json::from_str::<Message>(data) {
        Ok(mut m) => {
            m.id = id;
            Some(m)
        }
        Err(e) => {
            tracing::warn!("message 行解析失败，已跳过: id={id} err={e}");
            None
        }
    }
}

/// 「可回放行」筛选（SQL 片段）：排除 compaction boundary（`meta IS NOT NULL`）、
/// `role=tool` 与 tool_result 载体。回放 / 面板历史 / 快照 / 增量共用同一口径，
/// 消费游标才成立——被它筛掉的行视为已消费，不进任何批次。
const REPLAYABLE_FILTER_SQL: &str = "meta IS NULL \
   AND role IN ('user', 'assistant') \
   AND CASE WHEN json_valid(message.data) \
            THEN NOT EXISTS (SELECT 1 FROM json_each(message.data, '$.content') AS b \
                             WHERE CASE WHEN b.type = 'object' \
                                        THEN json_extract(b.value, '$.type') END = 'tool_result') \
            ELSE 0 END";

/// 尾部窗口查询（连接上执行，不含字节裁剪）：最近 `max_msgs` 条可回放行，id 升序。
fn transcript_tail_on(conn: &Connection, session_id: i64, max_msgs: usize) -> Result<Vec<Message>> {
    query_messages(
        conn,
        &format!(
            "SELECT id, data FROM ( \
                 SELECT id, data FROM message WHERE session_id = ?1 AND {REPLAYABLE_FILTER_SQL} \
                 ORDER BY id DESC LIMIT ?2 \
             ) ORDER BY id"
        ),
        params![session_id, max_msgs.max(1) as i64],
    )
}

/// 该会话当前最大消息行 id（无行返回 0）。
fn max_message_id(conn: &Connection, session_id: i64) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT COALESCE(MAX(id), 0) FROM message WHERE session_id = ?1",
        params![session_id],
        |row| row.get(0),
    )?)
}

/// 体积从最新一端往回累计、一次扫过：每条只序列化一次（旧写法每删一条就把整段重新
/// 序列化，面板每次打开都是 O(n²) 的 4 MB 级工作量）。`serialized_size` 失败时返回
/// `usize::MAX` 当「装不下」，饱和加法让它不溢出。至少保留一条。
fn trim_tail_by_bytes(mut msgs: Vec<Message>, max_bytes: usize) -> Vec<Message> {
    let mut keep_from = msgs.len();
    let mut used = 0usize;
    while keep_from > 0 {
        let size = serialized_size(std::slice::from_ref(&msgs[keep_from - 1]));
        let total = used.saturating_add(size);
        if total > max_bytes && keep_from < msgs.len() {
            break;
        }
        used = total;
        keep_from -= 1;
    }
    msgs.drain(..keep_from);
    msgs
}

fn row_to_message(row: &Row<'_>) -> rusqlite::Result<Option<Message>> {
    let id: i64 = row.get(0)?;
    let data: String = row.get(1)?;
    Ok(parse_message(id, &data))
}

fn query_messages<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params, row_to_message)?;
    let mut out = Vec::new();
    for row in rows {
        if let Some(m) = row? {
            out.push(m);
        }
    }
    Ok(out)
}

/// 原始行 `(id, data)`：只做 I/O，JSON 解析留给 [`parse_rows`] 在连接锁外完成（活动视图
/// 可能有上千行，持锁解析会把同进程其它连接使用方——桌面轮询、turn 的落库——都串在后面）。
fn query_raw<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// 末尾 `limit` 行（按 id 升序返回）：`ORDER BY id DESC LIMIT` 后翻转。
fn query_raw_tail(
    conn: &Connection,
    sql_desc: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<(i64, String)>> {
    let mut rows = query_raw(conn, sql_desc, params)?;
    rows.reverse();
    Ok(rows)
}

/// 把原始行解析成消息（坏行告警跳过）。
fn parse_rows(rows: Vec<(i64, String)>) -> Vec<Message> {
    rows.into_iter()
        .filter_map(|(id, data)| parse_message(id, &data))
        .collect()
}

/// 活动视图的原始行：按最后一个 compaction boundary 拼接 `head ++ [B] ++ kept ++ after`。
///
/// `head` / `kept` 两段只取普通行（`meta IS NULL`）：更早的 boundary 行若落在区间内会被
/// 当成一条普通 user 消息混进视图——它的摘要会连同当前 boundary 一起进入 prompt，
/// 还会让运行时把它当「最新摘要」误判。`after` 段按定义不含 boundary（B 是最后一个）。
///
/// `limit = Some(n)`：调用方只要视图末尾 n 行（`load_history` 的条数裁剪）——没有 boundary
/// 时直接 `ORDER BY id DESC LIMIT n`；有 boundary 时先取 `after` 段末尾 n 行，够数就不再
/// 读前面三段。结果与读全量再截尾一致。
fn load_active_raw(
    conn: &Connection,
    sid: i64,
    limit: Option<usize>,
) -> Result<Vec<(i64, String)>> {
    let boundary = latest_boundary(conn, sid)?;
    let full_history = |conn: &Connection| -> Result<Vec<(i64, String)>> {
        match limit {
            Some(n) => query_raw_tail(
                conn,
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
                params![sid, n as i64],
            ),
            None => query_raw(
                conn,
                "SELECT id, data FROM message WHERE session_id = ?1 ORDER BY id",
                params![sid],
            ),
        }
    };
    let Some((boundary_id, meta)) = boundary else {
        return full_history(conn);
    };
    let after = match limit {
        Some(n) => query_raw_tail(
            conn,
            "SELECT id, data FROM message WHERE session_id = ?1 AND id > ?2 \
             ORDER BY id DESC LIMIT ?3",
            params![sid, boundary_id, n as i64],
        )?,
        None => query_raw(
            conn,
            "SELECT id, data FROM message WHERE session_id = ?1 AND id > ?2 ORDER BY id",
            params![sid, boundary_id],
        )?,
    };
    if limit.is_some_and(|n| after.len() >= n) {
        return Ok(after);
    }
    let mut view = Vec::new();
    if let Some(head_until) = meta.head_until {
        view.extend(query_raw(
            conn,
            "SELECT id, data FROM message WHERE session_id = ?1 AND id <= ?2 AND meta IS NULL \
             ORDER BY id",
            params![sid, head_until],
        )?);
    }
    view.extend(query_raw(
        conn,
        "SELECT id, data FROM message WHERE session_id = ?1 AND id = ?2",
        params![sid, boundary_id],
    )?);
    view.extend(query_raw(
        conn,
        "SELECT id, data FROM message WHERE session_id = ?1 AND id >= ?2 AND id < ?3 \
         AND meta IS NULL ORDER BY id",
        params![sid, meta.tail_from, boundary_id],
    )?);
    view.extend(after);
    Ok(view)
}

/// 条数 / 体积裁剪 + 孤儿清理 + 工具链修复（旧 JSONL 加载路径的同一套不变量）。
fn trim_history(mut msgs: Vec<Message>, max_msgs: usize, max_bytes: usize) -> Vec<Message> {
    // 1) 只保留最近 max_msgs 条。
    let cap = max_msgs.max(1);
    if msgs.len() > cap {
        msgs.drain(..msgs.len() - cap);
    }

    // 2) 跳过开头连续的孤儿 tool_result 载体。
    while msgs.first().is_some_and(|m| m.is_tool_result_carrier()) {
        msgs.remove(0);
    }

    // 3) 体积二次裁剪：从头移除最旧，直到序列化 <= max_bytes，
    //    每次移除后重新检查孤儿（移除 assistant(tool_calls) 可能产生新孤儿）。
    while serialized_size(&msgs) > max_bytes && msgs.len() > 1 {
        msgs.remove(0);
        while msgs.first().is_some_and(|m| m.is_tool_result_carrier()) {
            msgs.remove(0);
        }
    }

    // 4) 全历史工具链一致性修复：删除任何「无配对 tool_result 的 assistant tool_use」与
    //    「无前序 tool_calls 的孤儿 tool_result」，保证留下的工具链位置相邻且 id 全覆盖。
    repair_tool_chains(msgs)
}

/// 估算 Vec<Message> 序列化后的字节数。
fn serialized_size(msgs: &[Message]) -> usize {
    serde_json::to_string(msgs)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

// -------------------------------------------------------------------------
// 工具链一致性修复（P2：修复「中间损坏的 tool_result 留下非法历史」）
// -------------------------------------------------------------------------

/// 取 assistant 消息里所有 `ToolUse` 的 id（用于工具链配对校验）。
fn tool_use_ids(m: &Message) -> Vec<String> {
    m.content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

/// 校验 assistant 消息里【每个】 `ToolUse` 是否结构合法，与 `openai_compat::convert_messages`
/// 的 fail-loud 规则一一对应：
/// - `id` 非空：空 id 无法与后续 tool_result 配对（已由调用方另行检测）；
/// - `name` 非空：空 `name` 在 openai_compat 回放时会被拒绝（无法构成合法 tool_calls）；
/// - `input` 为 JSON 对象：`null`/数组/字符串/数字等非对象在回放时会被拒绝
///   （`arguments` 非法或为 `null`），或致工具执行参数错乱。
///
/// 任一 `ToolUse` 不合法，整条 assistant 工具链（含其连续 tool_result）都必须在
/// [`repair_tool_chains`] 中丢弃——否则 [`SessionStore::load_history`] 仍会返回这条坏数据，
/// openai_compat 每一轮构造请求都会失败，用户无法通过继续对话自愈。
fn has_invalid_tool_use(m: &Message) -> bool {
    m.content.iter().any(|b| match b {
        ContentBlock::ToolUse { id, name, input } => {
            id.is_empty() || name.is_empty() || !input.is_object()
        }
        _ => false,
    })
}

/// 取消息里所有「工具结果 id」，用于工具链配对校验。
///
/// **两种形态都抽取（修复 P2）**：
/// 1. `role=user` 里的 `ToolResult` 块 → 取其 `tool_use_id`；
/// 2. `role=tool` 消息 → 取其 `tool_call_id`（OpenAI 兼容协议里工具结果以独立
///    `role=tool` 消息回传，配对 id 存于 `tool_call_id` 字段，而非 content 块）。
///
/// 与 [`Message::is_tool_result_carrier`]（两种形态都判 true）配套：判定「是结果载体」
/// 与「结果 id 是哪些」必须对齐，否则 `repair_tool_chains` 会把 `role=tool` 当成载体、
/// 却取不到 id，配对校验形同虚设，孤儿 `role=tool` 仍可能留下致 400。
fn tool_result_ids(m: &Message) -> Vec<String> {
    // 严格按 role 分支：
    // - `Role::Tool`：配对 id 只认 `tool_call_id` 字段，content 里的 ToolResult 块【不算数】
    //   （OpenAI 兼容协议里 role=tool 的结果 id 存于 tool_call_id）。若 tool_call_id 为空
    //   → 返回空 Vec → repair 判坏链/孤儿丢弃。混合形态（role=tool 同时 content 带 ToolResult）
    //   也只取 tool_call_id，杜绝 content 干扰。
    // - 其它 role（主要是 `Role::User` 携带 ToolResult 块的「结果载体」）：只读 content 里的
    //   ToolResult，不读 tool_call_id（user 消息本就不该用 tool_call_id 配对）。
    //
    // 与 [`Message::is_tool_result_carrier`]（两种形态都判 true）配套：判定「是结果载体」与
    // 「结果 id 是哪些」必须对齐——畸形 role=tool（无 tool_call_id）虽是载体，却取不到合法 id，
    // repair 会据此判坏链，整条工具链丢弃，避免 convert_messages 回放报错。
    match m.role {
        Role::Tool => m
            .tool_call_id
            .as_ref()
            .filter(|s| !s.is_empty())
            .map(|id| vec![id.clone()])
            .unwrap_or_default(),
        _ => m
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
                _ => None,
            })
            .collect(),
    }
}

/// 全历史工具链一致性修复。
///
/// OpenAI 兼容协议的硬约束：一条带 `tool_calls` 的 assistant 消息，必须紧跟若干
/// `role=tool` 消息把【每个】call id 都回答掉，然后才能出现下一条 assistant；反之，
/// 一条 `role=tool`（本仓库里即 user 携带 `ToolResult` 的「结果载体」）若没有前序
/// `assistant(tool_calls)` 声明同名 id，就是孤儿。违反任一方向都会让下一轮请求 400。
///
/// 本函数对整段历史做配对校验，使留下的工具链【位置相邻且 id 全覆盖】：
/// - assistant(tool_calls)：后面必须紧跟【连续若干条】tool_result 载体；结构合法
///   （无外来 id / 重复 id / 坏 tool_use）但覆盖不全时，缺失结果按 ADR-0004 合成
///   aborted 占位补齐（见下），保留整条链；结构损坏才整链丢弃。
/// - tool_result 载体：只有作为前一个 assistant 的连续结果组成员时才保留，否则视为孤儿删除。
///
/// **两类来源的判据（ADR-0004，对既有 fail-loud 的有限修订）**：
/// 1. 「provider 坏响应 / 数据损坏」——tool_use 结构非法（空 id / 重复 id / 空 name /
///    非 object input）、结果载体含外来 id 或重复 id：无法安全归属，维持 fail-loud
///    整链丢弃（用户可通过继续对话自愈，坏数据不进下一轮请求）。
/// 2. 「本地中断导致的缺失 output」——assistant 的 tool_use 结构全部合法，但其后
///    缺少部分/全部结果载体（取消、崩溃、结果行被跳过等本地数据丢失）：合成
///    `[工具执行被中断]` 占位 tool_result 补齐配对。合成占位让压缩前的中间上下文
///    不再整链蒸发，模型下轮明确知道哪些调用没有结果、不会假设调用已完成。
///
/// 为什么是“若干条”而不是“一条”：OpenAI 兼容格式通常是一次 tool call 对应一条
/// `role=tool` 消息。一个 assistant 如果发起两个工具调用，合法历史就是
/// `assistant(tool_calls=[a,b]) -> role=tool(a) -> role=tool(b)`。
///
/// 算法是单次线性扫描：遇到 assistant(tool_calls) 时，从它后面连续收集工具结果，直到
/// 所有 call id 都被覆盖；若遇到非结果消息、缺 id、或出现不属于本 assistant 的结果 id，
/// 按上表分别处理（合成占位 / 整链丢弃），后续结果消息会在自己的分支里作为孤儿被丢弃。
pub fn repair_tool_chains(msgs: Vec<Message>) -> Vec<Message> {
    let mut repaired = Vec::with_capacity(msgs.len());
    let mut i = 0;

    while i < msgs.len() {
        let msg = &msgs[i];

        if msg.has_tool_calls() {
            let needed = tool_use_ids(msg);
            // 与 `openai_compat::convert_messages` 的 fail-loud 规则对齐，逐条校验本
            // assistant 的【每个】ToolUse，任一不合法即视为坏链：
            // 1) `id` 非空、且同一 assistant 内 tool_use id 不重复；
            // 2) `name` 非空；
            // 3) `input` 为 JSON 对象。
            // 检测到坏链时：跳过本 assistant（不 push），其后续连续 tool_result 会在下一轮
            // 循环里因「无合法前序 assistant」被作为孤儿一并删除，从而丢弃整条坏工具链。
            let is_bad_chain = needed.iter().any(|id| id.is_empty())
                || {
                    let unique: std::collections::HashSet<_> = needed.iter().collect();
                    unique.len() != needed.len()
                }
                || has_invalid_tool_use(msg);
            if is_bad_chain {
                i += 1;
                continue;
            }
            let mut answered: Vec<String> = Vec::new();
            let mut end = i + 1;
            let mut valid_chain = true;

            // 从 assistant 后面连续吃掉工具结果。每条结果都必须：
            // 1. 能提取出至少一个 id；
            // 2. 所有 id 都属于当前 assistant 声明的 tool call；
            // 3. 多条结果合并后最终覆盖全部 needed id。
            //
            // 一旦覆盖完成就停止收集；如果后面还有额外 tool_result，它会在下一轮循环里
            // 被当成孤儿删除，而不会污染这条已经完整的合法链。
            while end < msgs.len() && msgs[end].is_tool_result_carrier() {
                let ids = tool_result_ids(&msgs[end]);
                if ids.is_empty() || !ids.iter().all(|id| needed.contains(id)) {
                    valid_chain = false;
                    break;
                }
                // 同一条结果消息【内部】不得携带重复 id——convert_messages 会把它序列化成
                // 两条相同 tool_call_id 的 role=tool 消息，OpenAI 兼容接口通常 400。
                {
                    let unique: std::collections::HashSet<_> = ids.iter().collect();
                    if unique.len() != ids.len() {
                        valid_chain = false;
                        break;
                    }
                }
                // 任一结果 id 已出现在 answered 中 → 重复 tool_result，判坏链。
                if ids.iter().any(|id| answered.contains(id)) {
                    valid_chain = false;
                    break;
                }
                for id in ids {
                    answered.push(id);
                }
                end += 1;

                if needed.iter().all(|id| answered.contains(id)) {
                    break;
                }
            }

            if valid_chain {
                repaired.push(msg.clone());
                for result in &msgs[i + 1..end] {
                    repaired.push(result.clone());
                }
                // A2（ADR-0004）：链结构合法但结果覆盖不全 → 本地中断遗留的缺失输出，
                // 合成 aborted 占位补齐配对，保留整条链。结构损坏（valid_chain=false）维持 fail-loud。
                let missing = needed
                    .iter()
                    .filter(|id| !answered.contains(*id))
                    .cloned()
                    .collect::<Vec<_>>();
                if !missing.is_empty() {
                    tracing::warn!(
                        "工具链结果缺失 {} 个（本地中断遗留），已合成 aborted 占位: {}",
                        missing.len(),
                        missing.join(", ")
                    );
                    repaired.push(synthesize_aborted_tool_results(&missing));
                }
                i = end;
            } else {
                // 坏 assistant 只删除自身；它后面已经读到或尚未读到的结果消息继续进入
                // 下一轮循环，并会因为没有合法前序 assistant 而被作为孤儿删除。
                i += 1;
            }
            continue;
        }

        if msg.is_tool_result_carrier() {
            // 没有被上面的 assistant 分支消费掉的结果载体，一律是孤儿或多余结果。
            i += 1;
            continue;
        }

        repaired.push(msg.clone());
        i += 1;
    }

    repaired
}

/// A2：合成占位结果的正文（ADR-0004）。模型读到它即知道该调用没有结果，
/// 不应假设调用已完成；is_error=true 与占位语义一致。
pub const ABORTED_TOOL_RESULT_PLACEHOLDER: &str =
    "[工具执行被中断] 本条工具调用在上一个 turn 被本地中断（用户取消或进程崩溃），未取得执行结果；\
     不要假设该调用已完成，请先用工具核实相关状态后再决策。";

/// 为缺失的工具结果合成 aborted 占位载体（ADR-0004）。
///
/// 只在「assistant 结构合法但结果缺失」的本地中断场景调用；载体形态与运行时
/// 写入的 tool_result 完全一致（user 消息携带 ToolResult 块），可直接回放。
pub fn synthesize_aborted_tool_results(ids: &[String]) -> Message {
    Message {
        id: 0,
        role: Role::User,
        content: ids
            .iter()
            .map(|id| ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: ABORTED_TOOL_RESULT_PLACEHOLDER.to_string(),
                is_error: true,
            })
            .collect(),
        tool_call_id: None,
        ts: 0,
    }
}

/// 统计 transcript 中 assistant 消息里各工具的调用次数（insights 用）。
pub fn tool_use_counts(messages: &[Message]) -> HashMap<String, u32> {
    let mut counts = HashMap::new();
    for m in messages.iter().filter(|m| m.role == Role::Assistant) {
        for block in &m.content {
            if let ContentBlock::ToolUse { name, .. } = block {
                *counts.entry(name.clone()).or_insert(0) += 1;
            }
        }
    }
    counts
}

// -------------------------------------------------------------------------
// 顶层便捷函数（便于 runtime 直接调用）
// -------------------------------------------------------------------------

/// 追加一条消息，返回分配的 id。
pub fn append_message(
    store: &SessionStore,
    channel: &str,
    chat_id: &str,
    message: &Message,
) -> Result<i64> {
    store.append(channel, chat_id, message)
}

/// 原子追加工具调用链（assistant + tool_result），返回两条的 id。
pub fn append_tool_chain(
    store: &SessionStore,
    channel: &str,
    chat_id: &str,
    asst: &Message,
    tool_result: &Message,
) -> Result<(i64, i64)> {
    store.append_tool_chain(channel, chat_id, asst, tool_result)
}

/// 加载历史。
pub fn load_history(
    store: &SessionStore,
    channel: &str,
    chat_id: &str,
    max_msgs: usize,
    max_bytes: usize,
) -> Result<Vec<Message>> {
    store.load_history(channel, chat_id, max_msgs, max_bytes)
}

/// 删除单个会话。
pub fn clear_session(store: &SessionStore, channel: &str, chat_id: &str) -> Result<()> {
    store.delete_session(channel, chat_id).map(|_| ())
}

#[cfg(test)]
mod tests;
