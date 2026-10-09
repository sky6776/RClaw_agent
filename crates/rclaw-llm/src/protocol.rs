//! 协议层抽象。
//!
//! `ChatProtocol` 回答「**怎么把规范化请求变成具体 provider 的 HTTP 请求，
//! 以及怎么把响应/流式事件翻回规范化结构**」。它本身不做 HTTP，
//! 便于单测覆盖与多协议复用。
//!
//! 当前唯一实现是 [`crate::openai_compat::OpenAiCompat`]，
//! 一套实现覆盖 DeepSeek / 智谱兼容端点 / Kimi / OpenRouter 等。
//! 新增全新协议（如 Anthropic Messages）时实现本 trait 即可。

use rclaw_core::{ChatOutcome, ChatRequest, FinishReason};

use crate::config::{Dialect, ProviderConfig};
use crate::error::Result;

/// 非流式与请求构造的协议契约。
pub trait ChatProtocol: Send + Sync {
    /// 规范化请求 → 具体 provider 的 JSON 请求体。
    /// `stream` 为 true 时在体里加 `"stream": true`。
    fn build_request_body(
        &self,
        cfg: &ProviderConfig,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<serde_json::Value>;

    /// 完整的请求 URL（含 `/chat/completions` 之类路径）。
    fn endpoint(&self, cfg: &ProviderConfig) -> String;

    /// 鉴权头列表（如 `Authorization: Bearer ...`）。
    fn auth_headers(&self, cfg: &ProviderConfig) -> Vec<(String, String)>;

    /// 解析非流式完整响应体 → 规范化结果。
    fn parse_full_response(
        &self,
        body: &[u8],
        dialect: &Dialect,
        max_tool_calls: usize,
    ) -> Result<ChatOutcome>;

    /// 构造一个流式解析器（自持累积状态，是流式结果的唯一真相来源）。
    fn make_stream_parser(&self, dialect: Dialect, max_tool_calls: usize) -> Box<dyn StreamParser>;
}

/// 一次 SSE `data:` 事件解析出的增量。
#[derive(Debug, Default)]
pub struct StreamDelta {
    /// 文本增量（供实时输出到 CLI）。
    pub text: Option<String>,
    /// 推理内容增量（思考模式）。
    pub reasoning: Option<String>,
    /// 收到结束原因；异常结束（length/content_filter 等）会被 feed 转成 `Err`。
    pub finished: Option<FinishReason>,
}

/// 流式增量解析器（状态化、自累积）。
///
/// 设计要点：解析器是流式结果的**唯一真相来源**——它内部累积文本、推理、
/// 工具调用，并负责所有正确性红线校验：
/// - tool_call `index` 可乱序到达；结束后仍不连续或越界 → 视为流损坏，返回 `Err`；
/// - `finish_reason=tool_calls` 但槽位不完整 / 为空 → 返回 `Err`；
/// - `length` / `content_filter` 等异常结束 → 返回 `Err`。
///
/// 这样上层 provider 只需驱动 I/O 并把文本增量回调出去，无需重复聚合逻辑。
pub trait StreamParser: Send {
    /// 喂入一条 `data: ` 后的 payload（不含前缀，不含 `[DONE]`）。
    fn feed(&mut self, payload: &str) -> Result<StreamDelta>;

    /// 判断某 payload 是否为流结束标记（如 `[DONE]`）。
    fn is_done_marker(&self, payload: &str) -> bool;

    /// 流结束前的最终校验（在 `into_outcome` 之前由 provider 调用）。
    ///
    /// 默认实现为 no-op；具体协议可在此对「流以 `[DONE]` 结束但从未收到
    /// `finish_reason`」这类不完整状态做最后一道防线——例如校验累积的工具调用槽位
    /// 完整且 stop 合法，否则返回 `Err` 拒绝产出 outcome。
    ///
    /// 返回 `Ok(())` 表示状态自洽，可安全取结果。
    fn finish(&mut self) -> Result<()> {
        Ok(())
    }

    /// 取出累积的最终结果（流正常结束后调用）。
    fn into_outcome(self: Box<Self>) -> ChatOutcome;
}
