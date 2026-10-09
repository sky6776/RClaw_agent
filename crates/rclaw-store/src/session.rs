//! Session 持久化（JSONL）。
//!
//! session JSONL 读写，忠实保留全部「正确性红线」：
//! - 按 `channel + chat_id` 隔离（百分号编码文件名，防串档）；
//! - 结构化字段（reasoning_content / tool_calls / tool_call_id）随会话持久化，
//!   满足 DeepSeek 思考模式「跨轮完整回传」要求；
//! - `append_tool_chain` 把 assistant(tool_calls) + tool_result 两条记录拼成
//!   **单次 write_all**，缩小「写一条、第二条失败」造成的半条工具链窗口
//!   （从源头规避，无需 ftruncate 回滚兜底）。
//!   注意：单次 `write_all` 并非内核级崩溃原子——进程在第一条落盘后、第二条落盘前
//!   崩溃，仍可能只留下 assistant(tool_calls)。因此 `load_history` 还会在最后一步
//!   丢弃尾部「无配对 tool_result 的 assistant tool_use」，作为崩溃残留的兜底修复；
//! - `load_history` 用 ring buffer 裁剪到 `max_msgs`，并跳过开头孤儿 tool_result，
//!   避免 OpenAI 兼容 API 因缺前序 tool_calls 返回 400；
//! - 超体积时从头裁剪并重新检查孤儿。

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

use chrono::Utc;

use rclaw_core::{ContentBlock, Message, Role};

use crate::fs::Layout;
use crate::{ensure_parent_dir, Result};

/// 会话存储：所有操作围绕一个 [`Layout`] 计算路径。
#[derive(Debug, Clone)]
pub struct SessionStore {
    layout: Layout,
}

impl SessionStore {
    pub fn new(layout: Layout) -> Self {
        SessionStore { layout }
    }

    /// 由 channel + chat_id 得到 session 文件路径。
    fn path(&self, channel: &str, chat_id: &str) -> PathBuf {
        self.layout.session_path(channel, chat_id)
    }

    /// 会话文件路径（桌面版多会话管理：归档 = 移动该文件）。
    pub fn session_path_pub(&self, channel: &str, chat_id: &str) -> PathBuf {
        self.path(channel, chat_id)
    }

    /// 会话文件根目录（`agent/sessions`；桌面版按 channel 前缀扫描列表）。
    pub fn sessions_dir(&self) -> PathBuf {
        self.layout.session_dir.clone()
    }

    /// 追加一条消息（结构化）。若 `ts` 为 0，自动盖当前时间戳。
    pub fn append(&self, channel: &str, chat_id: &str, message: &Message) -> Result<()> {
        let mut msg = message.clone();
        if msg.ts == 0 {
            msg.ts = Utc::now().timestamp();
        }
        let line = serde_json::to_string(&msg)?;
        append_lines(&self.path(channel, chat_id), &[line])
    }

    /// 原子追加「assistant(tool_calls) + user(tool_result)」两条记录。
    ///
    /// 把两条 JSONL 行拼成一个缓冲区，单次 `write_all` 落盘：
    /// 要么两条都进去，要么都不进，不会留下半条工具调用链。
    pub fn append_tool_chain(
        &self,
        channel: &str,
        chat_id: &str,
        asst: &Message,
        tool_result: &Message,
    ) -> Result<()> {
        let mut a = asst.clone();
        let mut t = tool_result.clone();
        if a.ts == 0 {
            a.ts = Utc::now().timestamp();
        }
        if t.ts == 0 {
            t.ts = Utc::now().timestamp();
        }
        let line1 = serde_json::to_string(&a)?;
        let line2 = serde_json::to_string(&t)?;
        // 拼成一次写入：\n 分隔，结尾换行。
        append_lines(&self.path(channel, chat_id), &[line1, line2])
    }

    /// 读取历史：裁剪到最近 `max_msgs` 条，并跳过开头孤儿 tool_result，
    /// 再按 `max_bytes`（序列化后）做二次裁剪。
    ///
    /// 仅限与 turn 循环串行调用（文件膨胀时会把压缩结果写回文件）；
    /// 并发 / 只读场景（如桌面版会话列表）用 [`Self::load_history_view`]。
    pub fn load_history(
        &self,
        channel: &str,
        chat_id: &str,
        max_msgs: usize,
        max_bytes: usize,
    ) -> Result<Vec<Message>> {
        self.load_history_inner(channel, chat_id, max_msgs, max_bytes, true)
    }

    /// [`Self::load_history`] 的只读变体：同样的裁剪与工具链修复，但不触发物理压缩写回。
    ///
    /// turn 循环自身的 load→append 是串行的，压缩写回安全；桌面版等新调用方会在
    /// turn 进行中并发读取同一会话——此时原子写回会与 turn 的追加竞争、丢失写回
    /// 窗口内追加的行。只读视图不做写回，压缩留待该会话下一个 turn 串行执行。
    pub fn load_history_view(
        &self,
        channel: &str,
        chat_id: &str,
        max_msgs: usize,
        max_bytes: usize,
    ) -> Result<Vec<Message>> {
        self.load_history_inner(channel, chat_id, max_msgs, max_bytes, false)
    }

    fn load_history_inner(
        &self,
        channel: &str,
        chat_id: &str,
        max_msgs: usize,
        max_bytes: usize,
        compact: bool,
    ) -> Result<Vec<Message>> {
        let path = self.path(channel, chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }

        // 1) 流式读取，以固定容量 ring buffer 保留最近 max_msgs 条。
        //    长期运行后 session 文件持续增长，全量解析 O(总历史) 会造成内存峰值和
        //    延迟；用 VecDeque 在读取时即限制容量，仅保留最近 max_msgs 条，
        //    内存为 O(max_msgs) 而非 O(总历史行数)。
        //    解析失败的行跳过（防单行损坏丢全部历史）。
        //    同时统计物理行数（含坏行），用于后续判定是否需要压缩文件——
        //    大量坏 JSON 行的文件若不按物理行判定压缩，将永远扫描并刷 warn。
        let f = fs::File::open(&path)?;
        let reader = BufReader::new(f);
        let cap = max_msgs.max(1);
        let mut ring: VecDeque<Message> = VecDeque::with_capacity(cap);
        let mut physical_lines: usize = 0; // 所有非空物理行数（含解析失败的行）
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    // 非 UTF-8 行：无法被 serde_json 解析，跳过并计入物理行。
                    // 不传播错误——旧实现用 ? 会让整个 load_history 因一个坏行失败。
                    tracing::warn!("session 行非 UTF-8，已跳过: {e}");
                    physical_lines += 1;
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            physical_lines += 1;
            match serde_json::from_str::<Message>(trimmed) {
                Ok(m) => {
                    // ring buffer 满了就丢弃最旧的消息，保持容量 <= max_msgs。
                    if ring.len() >= cap {
                        ring.pop_front();
                    }
                    ring.push_back(m);
                }
                Err(_) => {
                    tracing::warn!("session 行解析失败，已跳过: {:.80}", trimmed);
                }
            }
        }
        // reader.lines() 以所有权形式消费了 BufReader<File>，for 循环结束后 Lines
        // 迭代器随 loop 退出被 drop，内部的 File 句柄随之关闭，compaction 可安全写入。
        if ring.is_empty() {
            // 全损坏 session 清理：文件中有非空物理行但全部解析失败（全坏 JSON 或
            // 全非 UTF-8），若不处理，之后每轮都会全量扫描并刷 warn。
            // 超过阈值时用 atomic_write 清空文件，避免无限扫描。
            if physical_lines > 0 && physical_lines > max_msgs.max(1) * 2 {
                if let Err(e) = crate::fs::atomic_write(&path, b"") {
                    tracing::warn!(
                        "全损坏 session 清空失败（不影响本轮返回，下次 load 重试）: {e} path={}",
                        path.display()
                    );
                } else {
                    tracing::warn!(
                        "全损坏 session 已清空（{} 物理行全部解析失败），原数据已丢失 path={}",
                        physical_lines,
                        path.display()
                    );
                }
            }
            return Ok(Vec::new());
        }

        // 2) ring buffer 内已是最近 max_msgs 条，直接转为 Vec 供后续修复。
        let mut msgs: Vec<Message> = ring.into_iter().collect();

        // 3) 跳过开头连续的孤儿 tool_result 载体。
        while msgs.first().is_some_and(|m| m.is_tool_result_carrier()) {
            msgs.remove(0);
        }

        // 4) 体积二次裁剪：从头移除最旧，直到序列化 <= max_bytes，
        //    每次移除后重新检查孤儿（移除 assistant(tool_calls) 可能产生新孤儿）。
        while serialized_size(&msgs) > max_bytes && msgs.len() > 1 {
            msgs.remove(0);
            while msgs.first().is_some_and(|m| m.is_tool_result_carrier()) {
                msgs.remove(0);
            }
        }

        // 5) 全历史工具链一致性修复（不再只修尾部）。
        //    旧实现仅丢弃【尾部】无配对 tool_result 的 assistant tool_use；但若一条
        //    【中间】的 tool_result 行损坏被第 1 步跳过，其前面的 assistant(tool_calls)
        //    会留在历史里、后面又跟着新消息——回传给 OpenAI 兼容 API 时出现「有 tool_calls
        //    却无对应 role=tool」必然 400。`repair_tool_chains` 对整段历史做配对校验，
        //    删除任何「无配对 tool_result 的 assistant tool_use」与「无前序 tool_calls 的
        //    孤儿 tool_result」，保证留下的工具链位置相邻且 id 全覆盖（详见函数注释）。
        let repaired = repair_tool_chains(msgs);

        // 6) 压缩：当历史文件物理行数远大于加载后保留的消息数时（bloat >= 2x），
        //    用原子写把修复后的消息写回文件，裁剪掉已被淘汰的旧消息和坏行。
        //    使用 physical_lines（含坏行/非 UTF-8）而非仅成功解析的消息数——
        //    大量坏行的文件同样需要在压缩中清理，否则每轮全量扫描并刷 warn。
        //    Ring buffer 限定了内存，但每个 turn 仍从文件头逐行扫描全量历史——
        //    不压缩的话 O(总历史行数) 的延迟会随会话持续增长。
        //    压缩阈值设为 max_msgs * 2：若只有一个 turn 的轻微溢出没必要频繁重写，
        //    显著膨胀时才做 compaction（使用 crash-safe 的 atomic_write）。
        //    只读视图（compact=false）跳过写回：并发读取时写回会与 turn 的追加竞争。
        let compaction_threshold = max_msgs.max(1) * 2;
        if compact && physical_lines > compaction_threshold {
            let mut buf = String::new();
            for msg in &repaired {
                let line = serde_json::to_string(msg)?;
                buf.push_str(&line);
                buf.push('\n');
            }
            // atomic_write 是 crash-safe 的：先写 sidecar .tmp → fsync → rename。
            // 此时 reader 已 drop，同一 turn 的 append 还未开始，不存在并发写冲突。
            // 即使这里失败（ENOSPC / 权限），我们仍返回已修复的消息给调用方——
            // 压缩是优化而非正确性依赖，下次 load 时再尝试。
            if let Err(e) = crate::fs::atomic_write(&path, buf.as_bytes()) {
                tracing::warn!(
                    "session 历史压缩失败（不影响本轮加载，下次 load 重试）: {e} path={}",
                    path.display()
                );
            } else {
                tracing::debug!(
                    "session 历史已压缩: {} 物理行 → {} 条 (保留最近 {} 条) path={}",
                    physical_lines,
                    repaired.len(),
                    max_msgs,
                    path.display()
                );
            }
        }

        Ok(repaired)
    }

    /// 读取当前 session 的全部可解析历史，并做工具链一致性修复。
    ///
    /// 与 [`load_history`] 不同，本方法不按 `max_msgs`/`max_bytes` 裁剪，也不触发物理压缩；
    /// 供语义压缩流程先拿到完整上下文，再生成摘要并调用 [`rewrite`] 整体替换。
    pub fn load_all(&self, channel: &str, chat_id: &str) -> Result<Vec<Message>> {
        let path = self.path(channel, chat_id);
        if !path.exists() {
            return Ok(Vec::new());
        }

        let f = fs::File::open(&path)?;
        let reader = BufReader::new(f);
        let mut msgs = Vec::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!("session 行非 UTF-8，已跳过: {e}");
                    continue;
                }
            };
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            match serde_json::from_str::<Message>(trimmed) {
                Ok(m) => msgs.push(m),
                Err(_) => tracing::warn!("session 行解析失败，已跳过: {:.80}", trimmed),
            }
        }

        while msgs.first().is_some_and(|m| m.is_tool_result_carrier()) {
            msgs.remove(0);
        }
        Ok(repair_tool_chains(msgs))
    }

    /// 整体重写当前 session。
    ///
    /// 用于会话语义压缩：把旧历史替换为「压缩摘要 + 受保护的近端消息」。
    /// 写入采用 [`crate::fs::atomic_write`]，避免留下半截 JSONL。
    pub fn rewrite(&self, channel: &str, chat_id: &str, messages: &[Message]) -> Result<()> {
        let path = self.path(channel, chat_id);
        let mut buf = String::new();
        for msg in messages {
            let line = serde_json::to_string(msg)?;
            buf.push_str(&line);
            buf.push('\n');
        }
        crate::fs::atomic_write(&path, buf.as_bytes())
    }

    /// 清空会话（删除文件）。
    pub fn clear(&self, channel: &str, chat_id: &str) -> Result<()> {
        let path = self.path(channel, chat_id);
        if path.exists() {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

/// 把若干「已序列化好的 JSONL 行」以追加模式一次性写入文件。
///
/// 单次 write_all 保证原子性（对工具链两行尤其重要）。
fn append_lines(path: &PathBuf, lines: &[String]) -> Result<()> {
    ensure_parent_dir(path)?;
    // 把所有行（含各自结尾换行）先拼进【同一个缓冲区】，再做单次 write_all。
    //
    // 这是 `append_tool_chain` 真正原子性的保证：assistant(tool_calls) 与 tool_result
    // 两条 JSONL 行被一次性落盘——要么两条都写进去、要么都不写。
    // 旧实现对每行和换行分别 write_all（两行 = 4 次写入），写完第一条后若发生
    // I/O 错误或进程崩溃，就会留下「有 assistant tool_call 却没有对应 tool result」
    // 的半条工具链，污染后续请求历史，甚至触发 OpenAI 兼容 API 因缺前序 tool_calls
    // 而返回 400。单缓冲单次写入从源头消除这个窗口。
    let mut buf: Vec<u8> = Vec::new();
    for line in lines {
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(&buf)?;
    // 传播 fsync 错误：`ENOSPC` / `EIO` 时让调用方感知写入未真正落盘，而不是谎报成功。
    // 与 `rclaw_store::fs::atomic_write` 保持一致的持久化语义。
    f.sync_all()?;
    Ok(())
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
    // P2：按 role 分支提取结果 id，避免「role=tool 缺 tool_call_id、但 content 里有
    // ToolResult」的畸形旧会话被误判为已配对。
    //
    // 旧实现【先】从 content 里抽 ToolResult，【再】给 Role::Tool 追加 tool_call_id。于是对一条
    // `role=tool` 且【缺 tool_call_id】、但 content 里塞了 ToolResult 的畸形消息，会从 content
    // 抽出 tool_use_id，让 [`repair_tool_chains`] 误以为该 call 已被回答；然而
    // `openai_compat::convert_messages` 回放时 role=tool 仍因缺 tool_call_id 报错，导致每一轮
    // 请求都失败、用户无法自愈。
    //
    // 现在严格按 role 分支：
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
fn repair_tool_chains(msgs: Vec<Message>) -> Vec<Message> {
    let mut repaired = Vec::with_capacity(msgs.len());
    let mut i = 0;

    while i < msgs.len() {
        let msg = &msgs[i];

        if msg.has_tool_calls() {
            let needed = tool_use_ids(msg);
            // P2：与 `openai_compat::convert_messages` 的 fail-loud 规则对齐，逐条校验本
            // assistant 的【每个】ToolUse，任一不合法即视为坏链：
            // 1) `id` 非空、且同一 assistant 内 tool_use id 不重复
            //    （重复 id 会让配对歧义——`needed.iter().all(|id| answered.contains(id))`
            //    在 needed 含重复 id 时，只要 answered 有该 id 一次就会错误地认为所有同名
            //    tool_use 都已回答；旧会话或手工编辑的 JSONL 可能携带这种非法结构，回放 400）；
            // 2) `name` 非空（空 name 无法构成合法 tool_calls，openai_compat 回放时整轮失败）；
            // 3) `input` 为 JSON 对象（null/数组/字符串等非对象 → arguments 非法 → 回放 400）。
            //
            // 旧 JSONL 或手工编辑的 session 一旦包含这类坏 tool_use，openai_compat 已会在
            // 构造请求时拒绝整轮；但若不在 repair 阶段丢弃，load_history 仍会返回它，导致
            // 【每一轮】请求都失败，用户无法通过继续对话自愈。检测到坏链时：跳过本 assistant
            // （不 push），其后续连续 tool_result 会在下一轮循环里因「无合法前序 assistant」
            // 被作为孤儿一并删除（见下方 is_tool_result_carrier 分支），从而丢弃整条坏工具链。
            let is_bad_chain = needed.iter().any(|id| id.is_empty())
                || {
                    let unique: std::collections::HashSet<_> = needed.iter().collect();
                    unique.len() != needed.len()
                }
                || has_invalid_tool_use(msg);
            if is_bad_chain {
                // assistant 携带空 id / 重复 id / 空 name / 非 object input → 视为坏链丢弃。
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
                // P2：同一条结果消息【内部】不得携带重复 id（例如一条 User 消息里塞了两个
                // 相同 tool_use_id 的 ToolResult）。旧实现只检查 ids 是否已出现在 answered
                // （跨结果重复），没检查 ids 自身的内部重复——后者会被判为有效链，
                // convert_messages 随后会把它序列化成两条相同 tool_call_id 的 role=tool 消息，
                // OpenAI 兼容接口通常 400。这里用 HashSet 做局部去重校验，判坏链、整条丢弃。
                {
                    let unique: std::collections::HashSet<_> = ids.iter().collect();
                    if unique.len() != ids.len() {
                        valid_chain = false;
                        break;
                    }
                }
                // 任一结果 id 已出现在 answered 中 → 重复 tool_result。
                // OpenAI 兼容历史要求每个 tool_call_id 恰好一条结果消息；
                // 重复结果（如 tool(a), tool(a), tool(b)）在重放时会配对歧义甚至 400。
                // 这里判坏链，整条 assistant 链丢弃。
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
                // 合成 aborted 占位补齐配对，保留整条链（此前是整链丢弃 → 压缩前
                // 上下文整段蒸发）。结构损坏（valid_chain=false）维持 fail-loud。
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

// -------------------------------------------------------------------------
// 顶层便捷函数（便于 runtime 直接调用）
// -------------------------------------------------------------------------

/// 追加一条消息。
pub fn append_message(
    store: &SessionStore,
    channel: &str,
    chat_id: &str,
    message: &Message,
) -> Result<()> {
    store.append(channel, chat_id, message)
}

/// 原子追加工具调用链（assistant + tool_result）。
pub fn append_tool_chain(
    store: &SessionStore,
    channel: &str,
    chat_id: &str,
    asst: &Message,
    tool_result: &Message,
) -> Result<()> {
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

/// 清空会话。
pub fn clear_session(store: &SessionStore, channel: &str, chat_id: &str) -> Result<()> {
    store.clear(channel, chat_id)
}

#[cfg(test)]
mod tests;
