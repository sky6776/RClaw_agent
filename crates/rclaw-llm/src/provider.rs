//! 高层 Provider trait 与基于 `ChatProtocol` 的通用实现。
//!
//! `ProtocolProvider<P>` 把「具体协议（请求构造/响应解析）」与「HTTP 客户端」
//! 组合起来，实现 `Provider`：agent loop 只认 `Provider`，完全不感知 wire format。
//!
//! 流式读取使用 `reqwest` 的 `bytes_stream()`，逐行切分 SSE，交给协议层
//! [`StreamParser`](crate::protocol::StreamParser) 解析与累积。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use secrecy::ExposeSecret;

use rclaw_core::{ChatOutcome, ChatRequest};

use crate::config::{ProtocolKind, ProviderConfig};
use crate::error::{redact_secrets, LlmError, Result};
use crate::openai_compat::OpenAiCompat;
use crate::protocol::ChatProtocol;

// ---------------------------------------------------------------------------
// Provider trait —— agent loop 的唯一调用入口
// ---------------------------------------------------------------------------

/// 文本增量回调类型（`Arc` 包装，便于跨异步任务/线程持有与复用）。
pub type OnText = Arc<dyn Fn(&str) + Send + Sync>;

/// 推理（reasoning）增量回调类型：SSE 流中每个 reasoning 分片触发一次。
pub type OnReasoning = Arc<dyn Fn(&str) + Send + Sync>;

/// LLM provider 高层接口（provider 无关）。
///
/// `chat_stream` 的 `on_text` 回调用于实时把文本增量输出（如 CLI 逐字打印）；
/// 最终返回聚合好的完整结果（含 reasoning / tool_calls / stop）。
/// 这对应一次流式 LLM 调用（prompt + 历史 + 工具 + 文本回调）：
/// `cb` ↔ `on_text`，`resp` ↔ 返回值。
#[async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;

    /// 该 provider 配置的「单轮最大输出 token」默认值。
    ///
    /// agent loop 在构造 `ChatRequest` 时用它填充 `req.max_tokens`，使协议层写入 body 的
    /// 上限与配置一致；库调用方仍可在调用前覆盖 `req.max_tokens` 实现按轮限流。
    /// 默认实现返回 4096；持有 `ProviderConfig` 的实现（如 [`ProtocolProvider`]）应覆盖为
    /// `cfg.max_tokens`，让用户在配置里调整的值真正生效。
    fn max_tokens(&self) -> u32 {
        4096
    }

    /// 当前 provider 选中的模型是否支持多模态输入。
    fn supports_multimodal(&self) -> bool {
        false
    }

    /// 当前 provider 使用的配置快照（不可变；用于界面展示 bot/会话默认模型）。
    fn config_snapshot(&self) -> Option<ProviderConfig> {
        None
    }

    /// 当前 live 身份 `(provider 名, 模型名)`。
    ///
    /// `name()` 在可热切换实现里是启动时的名字（trait 返回 `&str`，无法在锁内返回）；
    /// 工作流的 allowed_models、状态条等需要"现在是谁"的地方应优先用本方法。
    fn current_identity(&self) -> Option<(String, String)> {
        None
    }

    /// 返回一份配置冻结的 provider 快照。
    ///
    /// 可热切换实现（[`crate::SwappableProvider`]）用它把一次 turn 的能力、输出预算、
    /// 工具链与重试固定在同一份配置上：turn 中途 `/llmodel` 切换只影响下一 turn。
    /// 不支持热切换的实现返回 `None`，调用方回退到自身。
    fn snapshot(&self) -> Option<Arc<dyn Provider>> {
        None
    }

    /// 非流式：一次性返回完整结果。
    async fn chat(&self, req: ChatRequest) -> Result<ChatOutcome>;

    /// 流式：边收边通过 `on_text` 输出文本增量，结束后返回聚合结果。
    async fn chat_stream(&self, req: ChatRequest, on_text: OnText) -> Result<ChatOutcome>;

    /// 流式（含推理增量）：同 [`Provider::chat_stream`]，另通过 `on_reasoning` 实时输出
    /// reasoning 分片（思考模型的「💭 思考中」流式展示用）。不需要推理增量的调用方
    /// 继续用 [`Provider::chat_stream`] 即可——默认实现委托过去、丢弃 reasoning 增量；
    /// 支持增量解析的协议实现（如 openai_compat 的 reasoning_content 字段）应覆写本方法。
    async fn chat_stream_reasoning(
        &self,
        req: ChatRequest,
        on_text: OnText,
        on_reasoning: OnReasoning,
    ) -> Result<ChatOutcome> {
        let _ = on_reasoning;
        self.chat_stream(req, on_text).await
    }
}

// ---------------------------------------------------------------------------
// 通用实现：协议 + reqwest 客户端
// ---------------------------------------------------------------------------

/// 任意 `ChatProtocol` + 配置 + reqwest 客户端 → `Provider`。
pub struct ProtocolProvider<P: ChatProtocol> {
    cfg: ProviderConfig,
    protocol: P,
    client: reqwest::Client,
}

impl<P: ChatProtocol> ProtocolProvider<P> {
    pub fn new(cfg: ProviderConfig, protocol: P) -> Result<Self> {
        // 最终配置预检：模型/档位/端点/api_key 全部在构造期验证。调用方（保存、
        // 热切换）因此可以「先构造成功再落盘」，不会出现首请求或重启才失败。
        crate::config::validate_provider_config(&cfg)?;
        if cfg.request_timeout_secs == 0 {
            return Err(LlmError::Config(
                "request_timeout_secs 必须大于 0".to_string(),
            ));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.request_timeout_secs))
            .build()
            .map_err(|error| crate::error::transport_error(&error))?;
        let provider = ProtocolProvider {
            cfg,
            protocol,
            client,
        };
        // 构造期就验证鉴权头（非法 key 字符/头名会在首个请求才炸）；保存前预检才能
        // 保证「成功返回的切换 = 重启后也能构造」。
        provider.build_headers()?;
        Ok(provider)
    }

    fn build_headers(&self) -> Result<HeaderMap> {
        let mut map = build_header_map(&self.protocol.auth_headers(&self.cfg))?;
        // 鉴权头可能使用任意自定义头名；统一标记 sensitive，避免日志/中间件回显。
        for (_, value) in map.iter_mut() {
            value.set_sensitive(true);
        }
        Ok(map)
    }
}

#[async_trait]
impl<P: ChatProtocol + Send + Sync> Provider for ProtocolProvider<P> {
    fn config_snapshot(&self) -> Option<ProviderConfig> {
        Some(self.cfg.clone())
    }

    fn name(&self) -> &str {
        &self.cfg.name
    }

    /// 覆盖默认实现：返回 provider 配置里的 `max_tokens`，让用户在 rclaw.toml 等处
    /// 调整的上限对 agent loop 真正生效（agent loop 会据此填充 `req.max_tokens`）。
    fn max_tokens(&self) -> u32 {
        self.cfg.max_tokens
    }

    fn supports_multimodal(&self) -> bool {
        self.cfg.supports_multimodal()
    }

    async fn chat(&self, req: ChatRequest) -> Result<ChatOutcome> {
        let body = self.protocol.build_request_body(&self.cfg, &req, false)?;
        let url = self.protocol.endpoint(&self.cfg);
        let call_id = next_llm_call_id();
        tracing::info!(
            call_id = %call_id,
            provider = ?self.cfg.name,
            model = ?self.cfg.default_model,
            url = ?url,
            mode = "non-stream",
            "LLM 请求开始"
        );

        let resp = self
            .client
            .post(&url)
            .headers(self.build_headers()?)
            .json(&body)
            .send()
            .await
            .map_err(|error| crate::error::transport_error(&error))?;

        let status = resp.status();
        if !status.is_success() {
            let text = read_error_body(resp).await;
            // 上游正文可能回显提示词/工具数据/凭据：边界内只保留状态码与受控错误码。
            return Err(LlmError::BadStatus {
                status: status.as_u16(),
                body: upstream_error_summary(status.as_u16(), &text),
            });
        }
        let bytes = read_body_capped(resp, MAX_RESPONSE_BYTES).await?;
        let outcome = self
            .protocol
            .parse_full_response(&bytes, &self.cfg.dialect, self.cfg.max_tool_calls)
            .map_err(|error| redact_llm_error(error, self.cfg.api_key.expose_secret()))?;
        log_final_output(&self.cfg, &call_id, "non-stream", &outcome);
        Ok(outcome)
    }

    async fn chat_stream(&self, req: ChatRequest, on_text: OnText) -> Result<ChatOutcome> {
        self.chat_stream_reasoning(req, on_text, Arc::new(|_| {}))
            .await
    }

    async fn chat_stream_reasoning(
        &self,
        req: ChatRequest,
        on_text: OnText,
        on_reasoning: OnReasoning,
    ) -> Result<ChatOutcome> {
        let body = self.protocol.build_request_body(&self.cfg, &req, true)?;
        let url = self.protocol.endpoint(&self.cfg);
        let call_id = next_llm_call_id();
        tracing::info!(
            call_id = %call_id,
            provider = ?self.cfg.name,
            model = ?self.cfg.default_model,
            url = ?url,
            mode = "stream",
            "LLM 请求开始"
        );

        let resp = self
            .client
            .post(&url)
            .headers(self.build_headers()?)
            .json(&body)
            .send()
            .await
            .map_err(|error| crate::error::transport_error(&error))?;

        let status = resp.status();
        if !status.is_success() {
            let text = read_error_body(resp).await;
            // 上游正文可能回显提示词/工具数据/凭据：边界内只保留状态码与受控错误码。
            return Err(LlmError::BadStatus {
                status: status.as_u16(),
                body: upstream_error_summary(status.as_u16(), &text),
            });
        }

        let mut parser = self
            .protocol
            .make_stream_parser(self.cfg.dialect.clone(), self.cfg.max_tool_calls);
        let mut stream = resp.bytes_stream();
        // 原始字节缓冲：跨 chunk 累积「半行」以及被切断的多字节字符。
        //
        // 关键修复：绝不能对单个 HTTP chunk 调 `from_utf8_lossy`。SSE 的 chunk 边界
        // 可能正好落在一个中文 / emoji（UTF-8 多字节序列）中间，lossy 会【立刻】把残缺
        // 字节替换成 U+FFFD——信息此时已丢失，之后哪怕按 '\n' 把行拼回来也救不回该字符。
        // 正确做法：累积原始字节，只在拿到完整行（以 '\n' 结尾）后再解码。
        // 因为 '\n' = 0x0A，绝不会是 UTF-8 的续接字节（0x80–0xBF）或起始字节（≥0xC0），
        // 所以任意两个 '\n' 之间的字节一定是完整的 UTF-8 序列（假设服务端流合法），可安全解码。
        let mut buf: Vec<u8> = Vec::new();
        let mut saw_termination = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| crate::error::transport_error(&error))?;
            buf.extend_from_slice(&chunk);

            // 处理所有完整行（内联，避免 dyn Fn 跨函数强转的生命周期问题）。
            while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
                // 取出完整行（含 '\n'），剩余字节留在 buf 里等待下一个 chunk 补齐。
                let line_bytes: Vec<u8> = buf.drain(..=nl).collect();
                // 完整行解码：理论上必为合法 UTF-8；极端情况下（服务端发了非法字节）
                // 用 lossy 兜底以免 panic。注意此时 lossy 只会作用于「本就损坏」的字节，
                // 绝不会破坏任何跨 chunk 的合法多字节字符。
                let raw = String::from_utf8_lossy(&line_bytes);
                let line = raw.trim_end_matches(['\n', '\r']);
                if line.is_empty() {
                    continue;
                }
                // 只关心 "data:" 行。
                let payload = match line.strip_prefix("data: ") {
                    Some(p) => p,
                    None => match line.strip_prefix("data:") {
                        Some(p) => p.trim_start(),
                        None => continue, // event: / id: / 注释 等忽略
                    },
                };

                if parser.is_done_marker(payload) {
                    saw_termination = true;
                    continue;
                }
                let delta = parser
                    .feed(payload)
                    .map_err(|error| redact_llm_error(error, self.cfg.api_key.expose_secret()))?;
                if let Some(t) = &delta.text {
                    on_text(t);
                }
                if let Some(r) = &delta.reasoning {
                    on_reasoning(r);
                }
                if delta.finished.is_some() {
                    saw_termination = true;
                }
            }
            // 完整行已全部消费，buf 里只剩「半行」：半行超过上限说明对端在持续推送不含
            // '\n' 的字节（异常网关 / 忽略 stream:true 吐出超大非 SSE 体），不能让它一直
            // 累积到 total timeout 到期——Android 低内存进程会先被 OOM killer 杀掉。
            if buf.len() > MAX_SSE_LINE_BYTES {
                return Err(LlmError::StreamAborted(format!(
                    "SSE 单行超过 {MAX_SSE_LINE_BYTES} 字节上限，已中止读取"
                )));
            }
        }

        // EOF：必须曾收到 [DONE] 或 finish_reason，否则视为异常断开，拒绝持久化。
        if !saw_termination {
            return Err(LlmError::StreamAborted("SSE EOF 未收到终止事件".into()));
        }

        // 最后一道状态校验：挡住「只有 [DONE] 却没有 finish_reason」这类不完整流——
        // 否则未校验的工具槽位会被补成空 id/name 或 {} 入参的 ToolCall。
        parser
            .finish()
            .map_err(|error| redact_llm_error(error, self.cfg.api_key.expose_secret()))?;
        let outcome = parser.into_outcome();
        log_final_output(&self.cfg, &call_id, "stream", &outcome);
        Ok(outcome)
    }
}

/// 非流式响应体的最大字节数。reqwest 的 `timeout` 只限时间不限大小，端点 / 前置网关异常时
/// `bytes()` 会把任意大的响应体整个读进内存（带宽 × 300s），Android 上即 OOM 被杀。
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;
/// SSE 流中单行（两个 '\n' 之间）的最大字节数；正常 SSE 事件只有 KB 级。
const MAX_SSE_LINE_BYTES: usize = 16 * 1024 * 1024;
/// 非 2xx 错误体只取前 1 MiB（调用方最终只保留 500 字符）。
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;

/// 带上限地读取整个响应体；超限立即中止并返回 `BadResponse`。
async fn read_body_capped(resp: reqwest::Response, max: usize) -> Result<Vec<u8>> {
    let mut stream = resp.bytes_stream();
    let mut out: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| crate::error::transport_error(&error))?;
        if out.len().saturating_add(chunk.len()) > max {
            return Err(LlmError::BadResponse(format!(
                "响应体超过 {max} 字节上限，已中止读取"
            )));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// 读取非 2xx 的错误体（带上限、读失败按空串处理，与旧 `text().unwrap_or_default()` 等价）。
async fn read_error_body(resp: reqwest::Response) -> String {
    match read_body_capped(resp, MAX_ERROR_BODY_BYTES).await {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(_) => String::new(),
    }
}

// ---------------------------------------------------------------------------
// 工厂：按协议种类构造一个 boxed Provider
// ---------------------------------------------------------------------------

/// 按配置里的协议种类构造 `Provider`。
///
/// 新增协议时在这里加一个 `match` 分支即可，其余链路无需改动。
pub fn build_provider(cfg: ProviderConfig) -> Result<Box<dyn Provider>> {
    match cfg.protocol {
        ProtocolKind::OpenAiCompat => {
            Ok(Box::new(ProtocolProvider::new(cfg, OpenAiCompat::new())?))
        }
    }
}

// ---------------------------------------------------------------------------
// 辅助
// ---------------------------------------------------------------------------

static LLM_CALL_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static LLM_PROCESS_LOG_ID: OnceLock<String> = OnceLock::new();

/// 为一次 provider 调用生成进程内单调、跨同日进程重启也可区分的日志关联 ID。
fn next_llm_call_id() -> String {
    let process_log_id = LLM_PROCESS_LOG_ID.get_or_init(|| {
        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{}-{started_at}", std::process::id())
    });
    let sequence = LLM_CALL_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{process_log_id}-{sequence}")
}

/// 记录一次 LLM 调用的可诊断元数据。模型输出和工具参数可能复述刚读取的凭证，
/// 因此日志只保留长度、工具名和结束原因，不落盘正文、reasoning 或工具入参。
fn log_final_output(cfg: &ProviderConfig, call_id: &str, mode: &str, outcome: &ChatOutcome) {
    let tool_names = outcome
        .tool_calls
        .iter()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>();
    tracing::info!(
        call_id = %call_id,
        provider = ?cfg.name,
        model = ?cfg.default_model,
        mode,
        output_chars = outcome.text.chars().count(),
        reasoning_chars = outcome
            .reasoning
            .as_deref()
            .map(|reasoning| reasoning.chars().count())
            .unwrap_or(0),
        tool_call_count = outcome.tool_calls.len(),
        tool_names = ?tool_names,
        finish_reason = ?outcome.stop,
        usage = ?outcome.usage.map(|u| (u.prompt_tokens, u.completion_tokens, u.total())),
        "LLM 最终输出"
    );
}

/// 把 `(name, value)` 列表构造成 [`HeaderMap`]，遇到非法 header 时【直接报错】而非丢弃。
///
/// 修复「非法鉴权头被静默丢弃」（P3）：旧实现用 `if let (Ok(name), Ok(val)) = ...` 把
/// 解析失败的 header 跳过，于是 `auth = apikey:...` 配成非法 header 名、或 header 值含非法
/// 字节时，请求会【无鉴权头】发出，只在远端返回 401 时才暴露，排查困难。现在改为返回
/// [`Result`]，并在错误信息里指明【是哪个 header】非法，让配置/请求构造阶段就地失败。
/// 流式错误里可能带上游正文/密钥回显：先替换本 provider 的实际 key，再做通用脱敏。
fn redact_llm_error(error: LlmError, secret: &str) -> LlmError {
    match error {
        LlmError::StreamAborted(message) => {
            LlmError::StreamAborted(redact_secrets(&message.replace(secret, "<redacted>")))
        }
        LlmError::BadStatus { status, body } => LlmError::BadStatus {
            status,
            body: redact_secrets(&body.replace(secret, "<redacted>")),
        },
        LlmError::BadResponse(message) => {
            LlmError::BadResponse(redact_secrets(&message.replace(secret, "<redacted>")))
        }
        LlmError::Config(message) => {
            LlmError::Config(redact_secrets(&message.replace(secret, "<redacted>")))
        }
        other => other,
    }
}

fn build_header_map(headers: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (k, v) in headers {
        let name = HeaderName::from_bytes(k.as_bytes()).map_err(|_| {
            LlmError::Config("非法 header 名（无法解析，只允许 HTTP token 字符）".to_string())
        })?;
        let mut val = HeaderValue::from_str(v).map_err(|_| {
            LlmError::Config(
                "header 的值含非法字节（HeaderValue 仅允许可见 ASCII 与 tab）".to_string(),
            )
        })?;
        // 鉴权头标记 sensitive：Debug/日志/中间件不会打印明文 value。
        if name == reqwest::header::AUTHORIZATION
            || k.to_ascii_lowercase().contains("key")
            || k.to_ascii_lowercase().contains("auth")
        {
            val.set_sensitive(true);
        }
        map.append(name, val);
    }
    Ok(map)
}

/// 把字符串截断到 `max` 字节以内（末尾加 `...`），用于错误体摘要。
///
/// 必须在 UTF-8 字符边界处截断：直接 `&s[..max]` 当 `max` 落在多字节字符（中文/emoji）
/// 中间时会 panic。这里把 `max` 回退到最近的字符边界再切片。错误体常含非 ASCII 文本，
/// 不做边界处理会在运行期 panic。
/// 把上游错误正文收敛为「状态码之外的结构化错误码 + 有限分类标记」：正文
/// （可能含提示词、工具数据、任意格式凭据）只在本函数内读取，绝不进入错误文案。
fn upstream_error_summary(status: u16, text: &str) -> String {
    // 可重试状态（408/425/429/5xx）优先按状态分类：正文里的鉴权/超限字样通常是
    // 限流或网关附言，不能据此把瞬时失败判成配置错误；正文完全省略。
    if matches!(status, 408 | 425 | 429) || (500..=599).contains(&status) {
        return "（瞬时错误正文已省略）".to_string();
    }
    let mut parts: Vec<String> = Vec::new();
    let mut decoded_message = String::new();
    if let Ok(root) = serde_json::from_str::<serde_json::Value>(text) {
        let error = &root["error"];
        for key in ["code", "type"] {
            let value = error
                .get(key)
                .and_then(|value| value.as_str())
                .or_else(|| root.get(key).and_then(|value| value.as_str()));
            if let Some(code) = value.and_then(crate::error::safe_error_code) {
                for tag in crate::error::error_code_tags(&code) {
                    if !parts.iter().any(|part| part.ends_with(tag)) {
                        parts.push(tag.to_string());
                    }
                }
                parts.push(format!("{key}={code}"));
            }
        }
        // message 可能是 Unicode 转义的中文：解析后的值参与分类，但原文不外泄。
        if let Some(message) = error
            .get("message")
            .and_then(|value| value.as_str())
            .or_else(|| root.get("message").and_then(|value| value.as_str()))
        {
            decoded_message.push_str(message);
        }
    }
    let mut scan = text.to_string();
    if !decoded_message.is_empty() {
        scan.push(' ');
        scan.push_str(&decoded_message);
    }
    for tag in crate::error::upstream_error_tags(&scan) {
        if !parts.iter().any(|part| part.ends_with(tag)) {
            parts.push(tag.to_string());
        }
    }
    if parts.is_empty() {
        "（未提供可识别错误码，正文已省略）".to_string()
    } else {
        parts.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llm_call_ids_are_unique_and_process_scoped() {
        let first = next_llm_call_id();
        let second = next_llm_call_id();
        assert_ne!(first, second);
        assert_eq!(
            first.rsplit_once('-').unwrap().0,
            second.rsplit_once('-').unwrap().0
        );
    }

    #[test]
    fn upstream_error_summary_drops_free_form_body() {
        let summary = upstream_error_summary(
            400,
            r#"{"error":{"code":"invalid_api_key","type":"authentication_error","message":"sk-secret prompt text"}}"#,
        );
        assert!(summary.contains("code=invalid_api_key"));
        assert!(summary.contains("type=authentication_error"));
        assert!(!summary.contains("sk-secret"));
        assert!(!summary.contains("prompt text"));
        // 非 JSON 网关错误页：正文整体省略。
        assert!(!upstream_error_summary(500, "<html>token=secret</html>").contains("secret"));
        // 只有 message 的鉴权错误：保留分类标记，但不透出 message 原文。
        let auth_only = upstream_error_summary(
            400,
            r#"{"error":{"message":"invalid api key sk-live-secret"}}"#,
        );
        assert!(auth_only.contains("authentication_error"), "{auth_only}");
        assert!(!auth_only.contains("sk-live-secret"), "{auth_only}");
        // 未知 code 不在白名单：不透出任意远端文本。
        let unknown = upstream_error_summary(400, r#"{"error":{"code":"sk-very-secret"}}"#);
        assert!(!unknown.contains("sk-very-secret"), "{unknown}");
        // 瞬时状态只按状态分类：正文/白名单鉴权码都不得把 429 判成鉴权失败。
        let transient = upstream_error_summary(
            429,
            r#"{"error":{"code":"invalid_api_key","message":"unauthorized"}}"#,
        );
        assert!(!transient.contains("authentication"), "{transient}");
        assert!(!transient.contains("invalid_api_key"), "{transient}");
        // 中文超限提示可能是 JSON Unicode 转义：解码后仍要产出分类标签。
        let unicode = upstream_error_summary(
            400,
            r#"{"error":{"message":"\u4e0a\u4e0b\u6587\u957f\u5ea6\u8d85\u9650"}}"#,
        );
        assert!(unicode.contains("context_length_exceeded"), "{unicode}");
        // 只有 permission_error 码、没有文本：按码映射到鉴权分类。
        let permission = upstream_error_summary(400, r#"{"error":{"type":"permission_error"}}"#);
        assert!(permission.contains("authentication_error"), "{permission}");
        // 与下游分类器保持超集：request too large / API key is invalid 组合。
        let too_large = upstream_error_summary(400, r#"{"error":{"message":"Request too large"}}"#);
        assert!(too_large.contains("context_length_exceeded"), "{too_large}");
        let key_invalid =
            upstream_error_summary(400, r#"{"error":{"message":"API key is invalid"}}"#);
        assert!(
            key_invalid.contains("authentication_error"),
            "{key_invalid}"
        );
    }

    /// P3：合法 header 列表应正确构造 HeaderMap。
    #[test]
    fn build_header_map_accepts_valid() {
        let map = build_header_map(&[
            ("Content-Type".into(), "application/json".into()),
            ("X-Custom".into(), "value".into()),
        ])
        .unwrap();
        assert_eq!(map.get("content-type").unwrap(), "application/json");
        assert_eq!(map.get("x-custom").unwrap(), "value");
    }

    /// P3：非法 header 名（含空格等）必须报错，而非旧实现那样静默丢弃→请求无鉴权。
    #[test]
    fn build_header_map_rejects_bad_name() {
        let r = build_header_map(&[("Bad Header".into(), "v".into())]);
        assert!(r.is_err(), "非法 header 名应被拒绝");
    }

    /// P3：header 值含非法字节（裸换行）必须报错。
    #[test]
    fn build_header_map_rejects_bad_value() {
        let r = build_header_map(&[("X-Test".into(), "bad\nvalue".into())]);
        assert!(r.is_err(), "非法 header 值应被拒绝");
    }
}
