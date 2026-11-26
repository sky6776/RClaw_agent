//! OpenAI 兼容协议实现。
//!
//! 一套实现覆盖 DeepSeek / 智谱 GLM 兼容端点 / Kimi / OpenRouter / Groq …
//! 即「`/chat/completions` + Bearer + `tool_calls` + `reasoning_content`」家族。
//!
//! 三块核心逻辑（忠实实现，并强化为 Rust 的明确错误）：
//! 1. [`build_request_body`]：规范化消息/工具 → OpenAI wire（对应
//!    `convert_messages_openai_compat` / `convert_tools_openai_compat`）。
//! 2. [`parse_full_response`]：非流式响应解析 + 工具调用校验（对应
//!    `parse_openai_compat_response` / `parse_tool_calls`）。
//! 3. [`OpenAiSseParser`]：流式 SSE 增量解析 + 状态化校验（对应 `parse_sse_event`）。

use secrecy::ExposeSecret;
use serde_json::{json, Value};
use std::collections::HashSet;

use rclaw_core::{
    ChatOutcome, ChatRequest, ContentBlock, FinishReason, MediaKind, Message, Role, TokenUsage,
    ToolCall,
};

use crate::config::{
    AuthScheme, Dialect, MaxTokensKey, MediaWire, ProviderConfig, ProviderFlavor, ReasoningField,
};
use crate::error::{LlmError, Result};
use crate::protocol::{ChatProtocol, StreamDelta, StreamParser};

/// OpenAI 兼容协议（无内部状态，可单例复用）。
pub struct OpenAiCompat;

impl OpenAiCompat {
    pub fn new() -> Self {
        OpenAiCompat
    }
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self::new()
    }
}

// 推理字段名（依方言）。返回 `Option`：
// - `Some(field)`：方言暴露推理内容，请求/响应该字段读写 `field`；
// - `None`：方言配置为【不暴露】思考内容（`ReasoningField::None`）——请求构造【不写】
//   任何 reasoning 字段、响应解析也【不读】任何 reasoning 字段。
//
// **修复 P2**：旧实现把 `None` 也映射成 `"reasoning_content"`，导致 `reasoning_field =
// "none"` 实际没生效——后续请求仍会把持久化的 reasoning_content 发给【不支持】该字段
// 的 provider，响应解析也仍读取该字段。改成 `Option` 后，调用方在 `None` 时彻底跳过
// reasoning 的序列化与解析（见 `convert_messages` / `parse_full_response` / SSE `feed`）。
fn reasoning_field_name(d: &Dialect) -> Option<&'static str> {
    match d.reasoning_field {
        ReasoningField::ReasoningContent => Some("reasoning_content"),
        ReasoningField::Reasoning => Some("reasoning"),
        ReasoningField::None => None,
    }
}

impl ChatProtocol for OpenAiCompat {
    fn build_request_body(
        &self,
        cfg: &ProviderConfig,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<Value> {
        let mut body = json!({});
        body["model"] = json!(cfg.default_model);

        // 限制输出 token 的字段名依方言选择。
        let mt_key = match cfg.dialect.max_tokens_key {
            MaxTokensKey::MaxTokens => "max_tokens",
            MaxTokensKey::MaxCompletionTokens => "max_completion_tokens",
        };
        // 用【请求携带的 max_tokens】写入 body，而非写死的 `cfg.max_tokens`。
        // 这样 `ChatRequest.max_tokens` 才名副其实——agent loop 调用前会从 provider 配置
        // 取值填入（见 `Provider::max_tokens()`），库调用方也可按需覆盖每轮上限。
        // 旧实现忽略 `req.max_tokens` 而总用 `cfg.max_tokens`，使该字段形同虚设、误导 API。
        body[mt_key] = json!(req.max_tokens);

        // DeepSeek 官方规则：请求携带 tools 且**思考开启**时，历史 assistant 的
        // reasoning_content 必须完整回传，否则远端 400。关闭思考（或 effort=none）
        // 时本就没有 reasoning，工具续轮必须放行。旧会话（别的 provider /
        // reasoning_field=reasoning / 关闭思考时期）可能没有该字段；在本地 fail loud，
        // 提示新建会话，而不是把一个必然 400 的请求发出去。
        let deepseek_thinking_active = cfg.reasoning.thinking != Some(false)
            && cfg.reasoning.effort.as_deref() != Some("none");
        if cfg.flavor == ProviderFlavor::DeepSeek
            && deepseek_thinking_active
            && !req.tools.is_empty()
        {
            ensure_deepseek_tool_reasoning(&req.messages, &cfg.dialect)?;
        }
        body["messages"] = Value::Array(convert_messages(
            req.system.as_deref(),
            &req.messages,
            &cfg.dialect,
            cfg.supports_multimodal(),
            cfg.media_wire,
        )?);

        if !req.tools.is_empty() {
            body["tools"] = Value::Array(convert_tools(&req.tools));
            if cfg.dialect.supports_tool_choice {
                body["tool_choice"] = json!("auto");
            }
        }

        if stream {
            body["stream"] = json!(true);
            // 请求服务端在流末尾回传真实 usage（独立 chunk，choices 为空数组）。
            // zhipu / DeepSeek / OpenRouter 等 OpenAI 兼容端点均支持该字段；运行时用它
            // 锚定 token 估算（A1）。不支持的端点通常忽略未知字段，风险可接受。
            body["stream_options"] = json!({ "include_usage": true });
            // 智谱将「响应流式输出」与「工具调用参数流式输出」拆成两个开关。
            // 仅在智谱请求实际携带工具时发送专用字段，避免把未知参数透传给
            // DeepSeek 等其它 OpenAI 兼容 provider。
            if !req.tools.is_empty() && supports_zhipu_tool_stream(cfg) {
                body["tool_stream"] = json!(true);
            }
        }

        // glm-5.3-flash 官方推荐参数（采样 + 思考），流式/非流式都生效。
        apply_reasoning_params(cfg, &mut body);
        Ok(body)
    }

    fn endpoint(&self, cfg: &ProviderConfig) -> String {
        format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'))
    }

    fn auth_headers(&self, cfg: &ProviderConfig) -> Vec<(String, String)> {
        let mut h = vec![("Content-Type".to_string(), "application/json".to_string())];
        match &cfg.auth {
            AuthScheme::Bearer => {
                h.push((
                    "Authorization".into(),
                    format!("Bearer {}", cfg.api_key.expose_secret()),
                ));
            }
            AuthScheme::ApiKey { header } => {
                h.push((header.clone(), cfg.api_key.expose_secret().to_string()));
            }
        }
        h
    }

    fn parse_full_response(
        &self,
        body: &[u8],
        dialect: &Dialect,
        max_tool_calls: usize,
    ) -> Result<ChatOutcome> {
        let root: Value = serde_json::from_slice(body)?;
        let choices = root.get("choices").and_then(|c| c.as_array());
        let choice0 = choices.and_then(|a| a.first());

        // 1) finish_reason 必须先于 message 校验：上游繁忙/中止的终帧可能不带 message，
        //    若先要求 message 会被误判成协议错误，永远走不到可重试分类。
        let mut tool_use = false;
        let mut seen_finish = false;
        let mut stop = FinishReason::Stop;
        if let Some(fr) = choice0
            .and_then(|c| c.get("finish_reason"))
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            seen_finish = true;
            // DeepSeek 等供应商在服务端资源不足/主动中止时返回这两个结束原因：
            // 请求本身没问题，归为可重试的上游繁忙，而不是协议错误（整轮失败且不重试）。
            if upstream_busy_finish_reason(fr, dialect) {
                return Err(LlmError::UpstreamBusy(
                    "上游结束原因表示资源暂不可用（可重试）".to_string(),
                ));
            }
            match FinishReason::from_openai(fr) {
                FinishReason::ToolCalls => {
                    tool_use = true;
                    stop = FinishReason::ToolCalls;
                }
                FinishReason::Stop => {}
                other => {
                    return Err(LlmError::BadResponse(format!(
                        "abnormal finish_reason: {other:?}"
                    )))
                }
            }
        }
        if !seen_finish {
            // 缺失 finish_reason：拒绝把无明确结束原因的响应持久化为正常答案（P2）。
            return Err(LlmError::BadResponse(
                "finish_reason 缺失：拒绝把无明确结束原因（stop/tool_calls）的响应持久化为正常答案"
                    .into(),
            ));
        }

        let message = choice0
            .and_then(|c| c.get("message"))
            .ok_or_else(|| LlmError::BadResponse("missing choices[0].message".into()))?;

        // 2) 正文 / 推理内容。
        let text = message
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        // 推理内容：仅当方言【暴露】推理字段时才读取（P2）。
        // `reasoning_field_name` 返回 None（reasoning_field = none）时彻底跳过，
        // 避免从「不支持该字段的 provider」的响应里误读 reasoning_content。
        let reasoning = reasoning_field_name(dialect).and_then(|field| {
            message
                .get(field)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        });

        // 3) 工具调用（含 count/字段完整性校验）。
        let tool_calls = parse_tool_calls(message.get("tool_calls"), max_tool_calls)?;

        // 4) 工具调用与 finish_reason 一致性校验。
        // 4a) finish_reason=tool_calls 但无实际调用 → 拒绝。
        if tool_use && tool_calls.is_empty() {
            return Err(LlmError::BadResponse(
                "finish_reason=tool_calls but call_count=0".into(),
            ));
        }
        // 4b) finish_reason≠tool_calls 但携带非空 tool_calls → 拒绝。
        // 某些 provider 会在 `finish_reason="stop"` 的同时返回未清理的 tool_calls 字段；
        // ChatOutcome::wants_tools() 仅检查 tool_calls.is_empty()，会导致 agent_loop
        // 在协议层不该执行工具时仍然进入工具执行路径，产生错误工具调用或错误历史持久化。
        if !tool_use && !tool_calls.is_empty() {
            return Err(LlmError::BadResponse(format!(
                "finish_reason={stop:?} but message.tool_calls is non-empty ({} calls)",
                tool_calls.len()
            )));
        }

        Ok(ChatOutcome {
            text,
            reasoning,
            tool_calls,
            stop: Some(stop),
            usage: root.get("usage").and_then(TokenUsage::from_openai),
        })
    }

    fn make_stream_parser(&self, dialect: Dialect, max_tool_calls: usize) -> Box<dyn StreamParser> {
        Box::new(OpenAiSseParser::new(dialect, max_tool_calls))
    }
}

/// 是否为「上游繁忙/主动中止」类的结束原因。
///
/// 仅当方言开启 `classify_upstream_busy`（DeepSeek）时生效：其它 OpenAI 兼容后端若
/// 复用同名结束原因，语义可能不同，不能被重新分类。DeepSeek 在服务端资源不足或主动
/// 中止请求时返回 `insufficient_system_resource` / `aborted`；请求本身没有协议问题，
/// 原样重试可能成功，因此归入 `LlmError::UpstreamBusy`（Retryable）。
fn upstream_busy_finish_reason(fr: &str, dialect: &Dialect) -> bool {
    dialect.classify_upstream_busy && matches!(fr, "insufficient_system_resource" | "aborted")
}

/// 是否为智谱官方 provider：常规配置以 provider 名 `zhipu` 标识；同时识别官方域名，
/// 兼容用户自定义 provider 名。智谱专用请求字段（`tool_stream`、glm-5.3-flash 推荐
/// 参数等）只发给官方端点，避免透传未知参数给其它 OpenAI 兼容 provider。
fn is_zhipu_provider(cfg: &ProviderConfig) -> bool {
    // 只认官方域名：provider 名可以被任意代理复用，按名字放行会把 `tool_stream` /
    // `clear_thinking` 等智谱专有字段发给别的服务（包括被命名为 zhipu 的 DeepSeek 端点）。
    reqwest::Url::parse(&cfg.base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
        .is_some_and(|host| {
            host.eq_ignore_ascii_case("bigmodel.cn")
                || host.to_ascii_lowercase().ends_with(".bigmodel.cn")
        })
}

/// 判断当前智谱 provider / 模型是否支持工具调用参数流式输出。
///
/// 模型范围按智谱 `tool_stream` 参数文档限制，避免热切换到不支持该字段的模型后，
/// 把智谱专用参数继续发送给远端。glm-5.3-flash 官方建议流式调用同开 `tool_stream`。
fn supports_zhipu_tool_stream(cfg: &ProviderConfig) -> bool {
    if !is_zhipu_provider(cfg) {
        return false;
    }

    let model = cfg.default_model.trim().to_ascii_lowercase();
    matches!(
        model.as_str(),
        "glm-5.3"
            | "glm-5.3-flash"
            | "glm-5.2"
            | "glm-5.1"
            | "glm-5-turbo"
            | "glm-5"
            | "glm-4.7"
            | "glm-4.6"
    ) || model.starts_with("glm-4.7-")
        || model.starts_with("glm-4.6-")
}

/// 注入思考参数。
///
/// 优先级（高 → 低）：
/// 1. `[[llm.providers]]` 显式配置的 `thinking` / `reasoning_effort`；
/// 2. glm-5.3-flash 的官方推荐默认（`temperature:1`、`top_p:0.95`、
///    `reasoning_effort:max`、`thinking:{type:enabled, clear_thinking:false}`）；
/// 3. 不发送 —— 由服务端默认决定。
///
/// `clear_thinking` 是智谱专有字段：只有 `ProviderFlavor::Zhipu` 才注入，避免发给
/// DeepSeek 等其它 OpenAI 兼容端点。
fn apply_reasoning_params(cfg: &ProviderConfig, body: &mut Value) {
    // 1) 显式配置优先。
    if let Some(enabled) = cfg.reasoning.thinking {
        if enabled {
            if is_zhipu_provider(cfg) {
                body["thinking"] = json!({ "type": "enabled", "clear_thinking": false });
            } else {
                body["thinking"] = json!({ "type": "enabled" });
            }
        } else {
            body["thinking"] = json!({ "type": "disabled" });
        }
    }
    if let Some(effort) = cfg.reasoning.effort.as_deref().filter(|s| !s.is_empty()) {
        body["reasoning_effort"] = json!(effort);
    }

    // 2) glm-5.3-flash 官方推荐默认：只补齐【未显式配置】的字段。
    if is_zhipu_provider(cfg)
        && cfg
            .default_model
            .trim()
            .eq_ignore_ascii_case("glm-5.3-flash")
    {
        let Some(obj) = body.as_object_mut() else {
            return;
        };
        obj.entry("temperature").or_insert_with(|| json!(1));
        obj.entry("top_p").or_insert_with(|| json!(0.95));
        obj.entry("reasoning_effort")
            .or_insert_with(|| json!("max"));
        obj.entry("thinking")
            .or_insert_with(|| json!({ "type": "enabled", "clear_thinking": false }));
    }
}

// ---------------------------------------------------------------------------
// 请求体构造：规范化 → OpenAI wire
// ---------------------------------------------------------------------------

/// 把 system + 规范化消息列表转成 OpenAI messages 数组。
///
/// **可失败（修复「缺失 tool_call_id 变空串」P3）**：返回 `Result`。任何要序列化为
/// `role=tool` 的消息，其 `tool_call_id` 必须存在且非空——否则会生成一条【无效的】
/// OpenAI 消息（空 `tool_call_id`），后端要么 400、要么把工具结果配对到错误的调用上。
/// 旧实现用 `unwrap_or_default()` 把缺失的 id 静默填成空串，让坏数据一路到达远端；
/// 现在改为在本地【直接拒绝】并指出是哪条消息缺 id。
fn convert_messages(
    system: Option<&str>,
    messages: &[Message],
    dialect: &Dialect,
    supports_multimodal: bool,
    media_wire: MediaWire,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();

    if let Some(sys) = system.filter(|s| !s.is_empty()) {
        out.push(json!({ "role": "system", "content": sys }));
    }

    let rf = reasoning_field_name(dialect);
    for (idx, m) in messages.iter().enumerate() {
        match m.role {
            Role::System => {
                out.push(json!({ "role": "system", "content": join_text(m) }));
            }
            Role::Assistant => {
                let text = join_text(m);
                let mut obj = json!({ "role": "assistant", "content": text });
                // 仅当方言【暴露】推理字段（rf = Some）时，才回传 assistant 的 reasoning。
                // reasoning_field = none 时彻底不发 reasoning_content（P2）——
                // 否则历史里持久化的 reasoning 会被发给不支持该字段的 provider。
                if let (Some(field), Some(r)) = (rf, join_reasoning(m)) {
                    obj[field] = Value::String(r);
                }
                // 回放 session 里的 assistant tool_use 块。
                //
                // **修复 P2「过滤坏 tool_use 会留下孤儿结果」**：旧实现用 `filter_map`
                // 静默丢弃非法 `ToolUse`（空 id/name 或非对象 input），但这不安全——
                // 【后续】同链路的 tool_result 仍会被序列化成 `role=tool`，形成【没有前序
                // tool_calls】的孤儿结果，OpenAI 兼容接口必然 400；另外旧实现放行 `input=null`，
                // 会序列化成 `arguments:"null"`，下游同样 400。
                //
                // 现在改为：遇到非法 `ToolUse` 直接 `return Err`（fail loud），让上层（session
                // 装载/repair 或 agent loop）感知并处理【整条工具链】（丢弃或修复），从根源上
                // 杜绝「局部过滤 → 孤儿 role=tool」与「arguments:null」。坏数据只应来自旧会话或
                // 手工编辑的 JSONL——新响应解析（parse_tool_calls / SSE）已保证 id/name 非空、
                // input 为 JSON 对象，不会进入此分支。
                let mut tcs: Vec<Value> = Vec::new();
                for b in &m.content {
                    let ContentBlock::ToolUse { id, name, input } = b else {
                        continue;
                    };
                    // id/name 必须非空：空 id/name 无法构成合法 tool_calls。
                    // 若静默跳过本 tool_use，其后续 tool_result 会变成孤儿 role=tool（无前序 tool_calls）→ 400。
                    if id.is_empty() || name.is_empty() {
                        return Err(LlmError::BadResponse(format!(
                            "messages[{idx}] 的 tool_use id/name 为空，拒绝回放（否则后续 tool_result 会成孤儿致 400）"
                        )));
                    }
                    // input 必须是 JSON 对象：
                    //  - null：旧实现放行 → arguments 序列化为 "null" → 下游 400；
                    //  - 数组/字符串/数字等非对象：arguments 非对象 → 下游 400 或工具执行 panic；
                    //  - 字符串：会被 input_to_arguments_string 原样当 arguments 文本，可能非法 JSON。
                    // 三者一律拒绝，要求在 session repair 阶段清洗（丢弃整条工具链）。
                    if !input.is_object() {
                        return Err(LlmError::BadResponse(format!(
                            "messages[{idx}] 的 tool_use input 非 JSON 对象，拒绝回放（否则 arguments 非对象/为 null 致 400）: type={}",
                            if input.is_null() { "null" }
                            else if input.is_array() { "array" }
                            else if input.is_string() { "string" }
                            else { "other" }
                        )));
                    }
                    tcs.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            // input 已校验为 JSON 对象，序列化为合法 arguments 文本。
                            "arguments": input_to_arguments_string(input),
                        }
                    }));
                }
                if !tcs.is_empty() {
                    obj["tool_calls"] = Value::Array(tcs);
                }
                out.push(obj);
            }
            Role::User => {
                // 先输出 tool_result 对应的 role=tool 消息，再输出文本 user 消息。
                for b in &m.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } = b
                    {
                        // tool_use_id 必须非空：空 id 会产生无效的 role=tool 消息。
                        if tool_use_id.is_empty() {
                            return Err(LlmError::BadResponse(format!(
                                "messages[{idx}] 的 tool_result 缺少 tool_use_id，拒绝生成空 tool_call_id（会致 400/配对错误）"
                            )));
                        }
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_use_id,
                            "content": content,
                        }));
                    }
                }
                if let Some(content) = user_visible_content(m, supports_multimodal, media_wire) {
                    out.push(json!({ "role": "user", "content": content }));
                }
            }
            Role::Tool => {
                // 规范化的 role=tool 消息（部分流程会直接用此形态）。
                // 缺失/空的 tool_call_id 必须拒绝，而非序列化成空串（P3）。
                let id = m.tool_call_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| {
                    LlmError::BadResponse(format!(
                        "messages[{idx}] role=tool 缺少 tool_call_id，拒绝生成空 tool_call_id（会致 400/配对错误）"
                    ))
                })?;
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": join_text(m),
                }));
            }
        }
    }
    Ok(out)
}

/// 工具描述：Anthropic 风格 `input_schema` → OpenAI `function.parameters`。
fn convert_tools(tools: &[rclaw_core::ToolSchema]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema.clone(),
                }
            })
        })
        .collect()
}

fn join_text(m: &Message) -> String {
    m.content
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("")
}

/// User 消息里真正发给模型的可见内容。
///
/// - 只有文本时保持旧行为：`content` 是字符串，兼容纯文本模型；
/// - 当前模型支持多模态且含多模态资源时，转成对应 provider 方言的 content array；
/// - 当前模型不支持多模态时，过滤 MediaUrl，只发送同条消息里的文本；
/// - ToolResult 块另行转成 `role=tool`，这里跳过。
///
/// 方言差异（[`MediaWire`]）：智谱/OpenAI 用 `image_url`/`video_url`/`file_url`；
/// DeepSeek 只认 `image_url` 与 `file(file_data)`，且没有 video part——不支持的
/// 形态必须降级成文本提示，绝不能把未知 part 透传给远端（会 400 或静默丢弃）。
fn user_visible_content(
    m: &Message,
    supports_multimodal: bool,
    media_wire: MediaWire,
) -> Option<Value> {
    let media_count = m
        .content
        .iter()
        .filter(|block| matches!(block, ContentBlock::MediaUrl { .. }))
        .count();
    if media_count > 0 && !supports_multimodal {
        // 纯文本模型：媒体块会被剥离，但必须给模型一个明确提示，避免它以为看到了附件。
        let mut text = join_text(m);
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!(
            "[{media_count} attachment(s) omitted: current model is text-only]"
        ));
        return Some(Value::String(text));
    }
    if media_count == 0 {
        let text = join_text(m);
        return (!text.is_empty()).then_some(Value::String(text));
    }

    let mut blocks = Vec::new();
    for b in &m.content {
        match b {
            ContentBlock::Text { text } if !text.is_empty() => {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            ContentBlock::MediaUrl { kind, url } => match (media_wire, kind) {
                (MediaWire::DeepSeek, MediaKind::File) => match parse_data_uri(url) {
                    DataUriPayload::Valid(payload) => {
                        blocks.push(json!({
                            "type": "file",
                            "file": { "file_data": payload }
                        }));
                    }
                    DataUriPayload::TooLarge => {
                        tracing::warn!("DeepSeek file_data 超过 20 MiB 原始字节上限，已降级");
                        blocks.push(json!({
                            "type": "text",
                            "text": "[file attachment omitted: payload exceeds 20 MiB]"
                        }));
                    }
                    DataUriPayload::Invalid => {
                        tracing::warn!("DeepSeek file_data 不是合法 base64 data URI，已降级");
                        blocks.push(json!({
                            "type": "text",
                            "text": "[file attachment omitted: invalid data URI]"
                        }));
                    }
                    DataUriPayload::NotDataUri => {
                        tracing::warn!(
                            "DeepSeek 不支持远程 file URL，已降级为文本提示（需 file_id/file_data）"
                        );
                        let label = if url.starts_with("http://") || url.starts_with("https://") {
                            safe_url_label(url)
                        } else {
                            "<unsupported-uri>".to_string()
                        };
                        blocks.push(json!({
                            "type": "text",
                            "text": format!(
                                "[file attachment url: {label} — DeepSeek 需要 file_id/file_data，未内联]"
                            )
                        }));
                    }
                },
                (MediaWire::DeepSeek, MediaKind::Video) => {
                    tracing::warn!("DeepSeek 不支持视频输入，已降级为文本提示");
                    blocks.push(json!({
                        "type": "text",
                        "text": "[video attachment omitted: provider does not accept video input]"
                    }));
                }
                (_, kind) => {
                    let ty = kind.openai_block_type();
                    let mut block = json!({ "type": ty });
                    block[ty] = json!({ "url": url });
                    blocks.push(block);
                }
            },
            _ => {}
        }
    }

    (!blocks.is_empty()).then_some(Value::Array(blocks))
}

/// data URI 解析结果。
enum DataUriPayload<'a> {
    NotDataUri,
    Valid(&'a str),
    TooLarge,
    Invalid,
}

/// DeepSeek `file_data` 的原始字节上限（20 MiB）。
const MAX_DEEPSEEK_FILE_RAW_BYTES: usize = 20 * 1024 * 1024;

/// 严格解析 `data:<mime>;base64,<payload>`。
///
/// - MIME 必须是 `type/subtype` 形式（各段非空、token 字符），metadata 只允许
///   `;base64`（且必须在末尾）；
/// - 载荷必须是标准 base64（长度 4 的倍数、字符集合法、padding ≤ 2 且只在末尾、
///   最后一组未使用的填充位为 0）；
/// - 解码后原始字节 ≤ 20 MiB。
///
/// 任何不满足的形态都不发送 `file_data`（远端会 400 或把垃圾当文件）。
fn parse_data_uri(url: &str) -> DataUriPayload<'_> {
    let Some(rest) = url.strip_prefix("data:") else {
        return DataUriPayload::NotDataUri;
    };
    let Some((meta, payload)) = rest.split_once(',') else {
        return DataUriPayload::Invalid;
    };
    let parts = meta.split(';').collect::<Vec<_>>();
    let mime = parts.first().copied().unwrap_or("");
    let mut mime_parts = mime.split('/');
    let (Some(kind), Some(subtype), None) =
        (mime_parts.next(), mime_parts.next(), mime_parts.next())
    else {
        return DataUriPayload::Invalid;
    };
    let token = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'+' | b'-'))
    };
    if !token(kind) || !token(subtype) {
        return DataUriPayload::Invalid;
    }
    // 只接受 `mime;base64`（base64 必须是最后一段）；charset 等参数不解析也不透传。
    if parts.len() != 2 || !parts[1].eq_ignore_ascii_case("base64") {
        return DataUriPayload::Invalid;
    }
    if payload.is_empty() || payload.len() % 4 != 0 {
        return DataUriPayload::Invalid;
    }
    let padding = payload
        .bytes()
        .rev()
        .take_while(|byte| *byte == b'=')
        .count();
    if padding > 2 {
        return DataUriPayload::Invalid;
    }
    let body = &payload[..payload.len() - padding];
    if body
        .bytes()
        .any(|byte| !(byte.is_ascii_alphanumeric() || byte == b'+' || byte == b'/'))
    {
        return DataUriPayload::Invalid;
    }
    // 规范 base64：最后一组未使用的填充位必须为 0（否则严格解码器会拒绝）。
    if padding > 0 {
        let last = payload.as_bytes()[payload.len() - padding - 1];
        let Some(value) = base64_value(last) else {
            return DataUriPayload::Invalid;
        };
        let mask = if padding == 1 { 0b11 } else { 0b1111 };
        if value & mask != 0 {
            return DataUriPayload::Invalid;
        }
    }
    let decoded = payload.len() / 4 * 3 - padding;
    if decoded > MAX_DEEPSEEK_FILE_RAW_BYTES {
        return DataUriPayload::TooLarge;
    }
    DataUriPayload::Valid(payload)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// 降级提示里的 URL 标签：去掉 query / fragment / 凭据，避免签名 URL 被写进日志或
/// 会话正文。解析失败时只回一个固定占位。
fn safe_url_label(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(mut parsed) if matches!(parsed.scheme(), "http" | "https") => {
            parsed.set_query(None);
            parsed.set_fragment(None);
            if !parsed.username().is_empty() || parsed.password().is_some() {
                let _ = parsed.set_username("");
                let _ = parsed.set_password(None);
            }
            parsed.to_string()
        }
        Err(_) => "<invalid-url>".to_string(),
        _ => "<unsupported-url>".to_string(),
    }
}

/// DeepSeek 带 tools 时的历史 reasoning 校验。
///
/// 官方规则：请求里一旦带 tools，之前所有 assistant 轮的 `reasoning_content` 都必须
/// 完整回传，否则 400。这里只校验**参与工具链**的 assistant 消息（含 ToolUse）——
/// 它们缺失 reasoning 必然触发远端 400；对应的 tool_result 也会因配对而放大问题。
/// 缺失时返回 `LlmError::Config`（NonRetryable，不重试），并给出可执行的恢复路径。
fn ensure_deepseek_tool_reasoning(messages: &[Message], dialect: &Dialect) -> Result<()> {
    // DeepSeek 只认 `reasoning_content`；`reasoning` 是别的厂商字段名，回传后不会被
    // 当作 reasoning，等同缺失。
    if dialect.reasoning_field != ReasoningField::ReasoningContent {
        return Err(LlmError::Config(
            "DeepSeek 开启思考且带工具时必须回传 reasoning_content；当前 provider 的 \
             reasoning_field 不是 reasoning_content，请改为 reasoning_content（或关闭思考）"
                .into(),
        ));
    }
    for (index, message) in messages.iter().enumerate() {
        if message.role != Role::Assistant {
            continue;
        }
        let has_tool_use = message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
        if !has_tool_use {
            continue;
        }
        let has_reasoning = message.content.iter().any(|block| {
            matches!(
                block,
                ContentBlock::Reasoning { reasoning_content } if !reasoning_content.trim().is_empty()
            )
        });
        if !has_reasoning {
            return Err(LlmError::Config(format!(
                "DeepSeek 要求带工具调用的历史完整回传 reasoning_content；messages[{index}] \
                 缺少该字段（通常来自其它 provider 的旧会话）。请新建会话，或先切回非 \
                 DeepSeek 模型处理该会话。"
            )));
        }
    }
    Ok(())
}

fn join_reasoning(m: &Message) -> Option<String> {
    let r: String = m
        .content
        .iter()
        .filter_map(|b| match b {
            ContentBlock::Reasoning { reasoning_content } => Some(reasoning_content.as_str()),
            _ => None,
        })
        .collect();
    if r.is_empty() {
        None
    } else {
        Some(r)
    }
}

/// ToolUse.input（JSON 值）→ OpenAI `arguments`（字符串）。
/// 若 input 本身就是字符串则原样使用，否则序列化为 JSON 串。
fn input_to_arguments_string(input: &Value) -> String {
    match input {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// 非流式响应：tool_calls 解析 + 校验
// ---------------------------------------------------------------------------

fn parse_tool_calls(val: Option<&Value>, max_tool_calls: usize) -> Result<Vec<ToolCall>> {
    let arr = match val {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(a) if a.is_array() => a.as_array().unwrap(),
        _ => return Err(LlmError::BadResponse("tool_calls 不是数组".into())),
    };

    // 超过本地能力即拒绝整轮，绝不静默截断工具链。
    if arr.len() > max_tool_calls {
        return Err(LlmError::BadResponse(format!(
            "tool_calls 数量 {} 超过上限 {max_tool_calls}（可在 rclaw.toml [llm] max_tool_calls 调大）",
            arr.len()
        )));
    }

    let mut out = Vec::with_capacity(arr.len());
    // 同一响应内的 tool_call id 必须唯一：重复 id 会导致 agent 执行两次工具并持久化
    // 重复 tool_call_id 的结果，下一轮回放给 OpenAI 兼容接口时配对歧义甚至 400。
    let mut seen_ids: HashSet<&str> = HashSet::with_capacity(arr.len());
    for tc in arr {
        let func = tc
            .get("function")
            .ok_or_else(|| LlmError::BadResponse("tool_call 缺少 function".into()))?;
        let name = func
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LlmError::BadResponse("tool_call 缺少 function.name".into()))?;
        // id 必须由服务端给出，禁止本地伪造（伪造会破坏 tool_call_id 配对）。
        let id = tc
            .get("id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| LlmError::BadResponse("tool_call 缺少 id".into()))?;
        // 拒绝同一响应内的重复 tool_call id：agent 会执行两次工具并持久化重复
        // tool_call_id 的结果，下一轮回放给 OpenAI 兼容接口时配对会产生歧义甚至 400。
        if !seen_ids.insert(id) {
            return Err(LlmError::BadResponse(format!(
                "同一响应内 tool_call id 重复（tool_calls[{}]）",
                out.len()
            )));
        }
        let input = match func.get("arguments") {
            Some(Value::String(s)) => s.clone(),
            Some(other) => other.to_string(),
            None => return Err(LlmError::BadResponse("tool_call 缺少 arguments".into())),
        };
        // 校验 arguments 是合法 JSON：空串/纯空白规整为 "{}"（无参工具的合法表示），
        // 非空但无法解析则视为 provider 坏响应，【整轮拒绝】——
        // 否则畸形的 arguments 会在下游被静默当成 {} 执行，把坏响应当成正常工具错误持久化。
        let input = normalize_arguments(&input)?;
        out.push(ToolCall {
            id: id.to_string(),
            name: name.to_string(),
            input,
        });
    }
    Ok(out)
}

/// 把 provider 给出的 `arguments` 原始文本规整为「可被下游安全 `from_str` 的合法 JSON 文本」。
///
/// - 空串/纯空白 → `"{}"`：无参工具的合法表示（部分 provider 对零参工具回传空 arguments）；
/// - 非空 → 必须能解析为合法 JSON **且为对象**（工具 schema 均为 object，数组/字符串/数字等
///   非对象参数会导致下游 `cron_add` 的对象写入 panic 或把非法参数持久化进工具链历史）；
///   否则返回 [`LlmError::BadResponse`]，由上层【整轮拒绝】。
///
/// 诊断文本只带字段位置、类型与长度，绝不回显 original 参数内容（上游可能回显凭据）。
///
/// 这样下游（agent loop、持久化）拿到的 `ToolCall.input` 一定是合法 JSON 对象，
/// 杜绝「畸形 arguments 被静默当成 `{}` 执行、并把坏响应当成正常工具错误写进会话」。
fn normalize_arguments(raw: &str) -> std::result::Result<String, LlmError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok("{}".to_string());
    }
    // 要求解析结果为 Object：工具调用参数必须是键值对，数组/字符串/数字等非对象
    // 会导致下游（如 cron_add 的 input["channel"] = ... 对象写入）panic。
    let v: Value = serde_json::from_str(trimmed).map_err(|_| {
        LlmError::BadResponse(format!(
            "tool_call arguments 非合法 JSON（{} 字节）",
            raw.len()
        ))
    })?;
    if !v.is_object() {
        return Err(LlmError::BadResponse(format!(
            "tool_call arguments 非 JSON 对象（类型={}）",
            if v.is_array() {
                "array"
            } else if v.is_string() {
                "string"
            } else if v.is_number() {
                "number"
            } else if v.is_boolean() {
                "boolean"
            } else {
                "null"
            }
        )));
    }
    // 校验通过：原样返回（保留 provider 给的文本，便于原样回传）。
    Ok(raw.to_string())
}

// ---------------------------------------------------------------------------
// 流式 SSE 解析器（状态化）
// ---------------------------------------------------------------------------

/// 单个工具调用槽位的累积状态。
#[derive(Default)]
struct ToolSlot {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

/// OpenAI 兼容 SSE 解析器。
///
/// 内部累积 text / reasoning / 工具调用槽位，并对正确性红线做严格校验：
/// - `index` 越界，或流结束后索引仍不连续 → 流损坏；
/// - `finish_reason=tool_calls` 时槽位不完整或为空 → 流损坏；
/// - `length` / `content_filter` 等异常结束 → 流损坏。
struct OpenAiSseParser {
    dialect: Dialect,
    max_tool_calls: usize,
    text: String,
    reasoning: String,
    // 工具调用分片可能乱序到达，因此槽位允许暂时为空；流结束时再要求
    // `0..=max_index` 全部存在且完整。
    slots: Vec<Option<ToolSlot>>,
    stop: Option<FinishReason>,
    /// 流末尾 `include_usage` chunk 上报的真实 token 用量（后到覆盖先到）。
    usage: Option<TokenUsage>,
}

impl OpenAiSseParser {
    fn new(dialect: Dialect, max_tool_calls: usize) -> Self {
        OpenAiSseParser {
            dialect,
            max_tool_calls,
            text: String::new(),
            reasoning: String::new(),
            slots: Vec::new(),
            stop: None,
            usage: None,
        }
    }

    fn reasoning_field(&self) -> Option<&'static str> {
        reasoning_field_name(&self.dialect)
    }

    /// 在工具调用流结束后统一校验稀疏槽位，确保任何不完整响应都不会进入执行阶段。
    fn validate_tool_calls(&self) -> Result<()> {
        if self.slots.is_empty() {
            return Err(LlmError::BadResponse(
                "finish=tool_calls 但无 tool_call".into(),
            ));
        }

        let mut seen: HashSet<&str> = HashSet::with_capacity(self.slots.len());
        for (i, slot) in self.slots.iter().enumerate() {
            let s = slot.as_ref().ok_or_else(|| {
                LlmError::BadResponse(format!("流结束但 tool_call index 不连续：缺少 index={i}"))
            })?;
            if s.id.is_none() || s.name.is_none() {
                return Err(LlmError::BadResponse(format!(
                    "流结束但 tool_call[{i}] 不完整 (id={}, name={})",
                    s.id.is_some(),
                    s.name.is_some()
                )));
            }
            let id = s.id.as_deref().expect("上方已校验 tool_call id");
            if !seen.insert(id) {
                return Err(LlmError::BadResponse(format!(
                    "流式 tool_call 跨槽位重复 id（tool_call[{i}]）"
                )));
            }
            normalize_arguments(&s.args).map_err(|_| {
                LlmError::BadResponse(format!(
                    "流结束但 tool_call[{i}] arguments 非合法 JSON（{} 字节）",
                    s.args.len()
                ))
            })?;
        }
        Ok(())
    }
}

/// 把流内 `error` 对象收敛为受控错误码（code/type）与分类标记，绝不透出 message。
fn stream_error_summary(error: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(object) = error.as_object() {
        // 先归一 code/type，再判断瞬时优先级：大小写/空白不影响结论，且任一字段
        // 明确瞬时时，另一个字段（如 context_length_exceeded）不得覆盖它。
        let mut transient: Option<(String, String)> = None;
        for key in ["code", "type"] {
            let Some(raw) = object.get(key).and_then(Value::as_str) else {
                continue;
            };
            let normalized = raw.trim().to_ascii_lowercase();
            if transient.is_none() && crate::error::error_code_is_transient(&normalized) {
                transient = Some((key.to_string(), normalized));
            }
        }
        if let Some((key, code)) = transient {
            return format!("流内错误事件（瞬时错误，{key}={code}）");
        }
        for key in ["code", "type"] {
            if let Some(code) = object
                .get(key)
                .and_then(Value::as_str)
                .and_then(crate::error::safe_error_code)
            {
                for tag in crate::error::error_code_tags(&code) {
                    if !parts.iter().any(|part| part.ends_with(tag)) {
                        parts.push(tag.to_string());
                    }
                }
                parts.push(format!("{key}={code}"));
            }
        }
        // message 只用于内部提取鉴权/超限分类，原文不外泄。
        if let Some(message) = object.get("message").and_then(Value::as_str) {
            for tag in crate::error::upstream_error_tags(message) {
                if !parts.iter().any(|part| part.ends_with(tag)) {
                    parts.push(tag.to_string());
                }
            }
        }
    }
    if parts.is_empty() {
        "流内错误事件（未提供可识别错误码，正文已省略）".to_string()
    } else {
        format!("流内错误事件（{}）", parts.join(", "))
    }
}

impl StreamParser for OpenAiSseParser {
    fn is_done_marker(&self, payload: &str) -> bool {
        payload.trim() == "[DONE]"
    }

    fn feed(&mut self, payload: &str) -> Result<StreamDelta> {
        let root: Value = serde_json::from_str(payload)
            .map_err(|_| LlmError::BadResponse("SSE 事件 JSON 解析失败".into()))?;

        let mut delta = StreamDelta::default();

        // 真实 usage：`include_usage` chunk 在流末尾以 choices 为空数组的独立事件到达；
        // 部分兼容端点也会把 usage 附在最后一个 choices chunk 上。先记先到、后到覆盖，
        // 必须在 choices 缺失/为空的提前返回之前解析。
        if let Some(usage) = root.get("usage").and_then(TokenUsage::from_openai) {
            self.usage = Some(usage);
        }

        // error 必须先于 choices 判断：部分端点把 error 与空 choices 数组放在同一帧，
        // 先看 choices 会把鉴权/超限信号当空帧丢掉。
        if let Some(error) = root.get("error").filter(|value| !value.is_null()) {
            return Err(LlmError::StreamAborted(stream_error_summary(error)));
        }
        let choices = match root.get("choices").and_then(|c| c.as_array()) {
            // 无 choices：心跳/usage 空事件，安全跳过。
            None => return Ok(delta),
            Some(c) => c,
        };

        let choice0 = match choices.first() {
            Some(c) => c,
            None => return Ok(delta),
        };

        // 结束原因必须先于 delta 处理：上游资源不足/中止的终帧可能同时带不完整的
        // tool_call 分片，先解析 delta 会先返回 BadResponse，永远走不到可重试分类。
        if let Some(fr) = choice0
            .get("finish_reason")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            match FinishReason::from_openai(fr) {
                FinishReason::Stop => {
                    self.stop = Some(FinishReason::Stop);
                    delta.finished = Some(FinishReason::Stop);
                }
                FinishReason::ToolCalls => {
                    // 这里只记录逻辑结束原因。索引连续性、字段完整性和 arguments JSON
                    // 校验统一推迟到 `finish()`，确保所有已到达的 SSE 分片都先完成聚合。
                    self.stop = Some(FinishReason::ToolCalls);
                    delta.finished = Some(FinishReason::ToolCalls);
                }
                _ if upstream_busy_finish_reason(fr, &self.dialect) => {
                    return Err(LlmError::UpstreamBusy(
                        "上游结束原因表示资源暂不可用（可重试）".to_string(),
                    ));
                }
                other => return Err(LlmError::BadResponse(format!("异常结束: {other:?}"))),
            }
        }

        if let Some(d) = choice0.get("delta") {
            // 文本增量：累积 + 回调。
            if let Some(c) = d
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            {
                self.text.push_str(c);
                delta.text = Some(c.to_string());
            }
            // 推理增量：累积（思考模式，跨轮回传）。仅当方言【暴露】推理字段时读取（P2）——
            // reasoning_field = none 时彻底不累积、不回传，避免从不支持该字段的 provider 误读。
            if let Some(field) = self.reasoning_field() {
                if let Some(r) = d
                    .get(field)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                {
                    self.reasoning.push_str(r);
                    delta.reasoning = Some(r.to_string());
                }
            }
            // 工具调用增量（按 index 分片）。
            if let Some(tcs) = d.get("tool_calls").and_then(|v| v.as_array()) {
                for tc in tcs {
                    // index 必须显式存在，不允许默认 0：缺失 index 的片段若被拼到
                    // 第 0 个槽位会把第二个工具调用的参数/name 合并到第一个工具，
                    // 导致执行错误的工具或持久化错误的历史记录。
                    let idx = tc
                        .get("index")
                        .and_then(|v| v.as_u64())
                        .and_then(|n| usize::try_from(n).ok())
                        .ok_or_else(|| {
                            LlmError::BadResponse(
                                "tool_call chunk missing valid `index` field".into(),
                            )
                        })?;

                    // 越界：超出本地能力 → 流损坏。
                    if idx >= self.max_tool_calls {
                        return Err(LlmError::BadResponse(format!(
                            "tool_call index {idx} 越界：单轮工具调用数超过上限 {}（可在 rclaw.toml [llm] max_tool_calls 调大）",
                            self.max_tool_calls
                        )));
                    }
                    // 工具调用分片可乱序到达：先扩展为稀疏槽位，缺失索引留待流结束时
                    // 统一校验。这样 index=1 先于 index=0 不会被误判为流损坏，但若
                    // index=0 最终始终未到达，整轮仍会被拒绝且不会执行任何工具。
                    if idx >= self.slots.len() {
                        self.slots.resize_with(idx + 1, || None);
                    }
                    let slot = self.slots[idx].get_or_insert_with(ToolSlot::default);
                    // 同槽一致性校验：同一槽位后续 chunk 不能改 id/name。
                    // 若 provider 在流中为同一 index 先后发送不同 id，说明流已损坏——
                    // 保留任意一个都会产生歧义工具链。
                    if let Some(id) = tc
                        .get("id")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        match &slot.id {
                            Some(existing) if existing != id => {
                                return Err(LlmError::BadResponse(format!(
                                    "tool_call[{idx}] 的 id 在流中发生变化"
                                )));
                            }
                            _ => slot.id = Some(id.to_string()),
                        }
                    }
                    if let Some(f) = tc.get("function") {
                        if let Some(n) = f
                            .get("name")
                            .and_then(|v| v.as_str())
                            .filter(|s| !s.is_empty())
                        {
                            match &slot.name {
                                Some(existing) if existing != n => {
                                    return Err(LlmError::BadResponse(format!(
                                        "tool_call[{idx}] 的 name 在流中发生变化"
                                    )));
                                }
                                _ => slot.name = Some(n.to_string()),
                            }
                        }
                        match f.get("arguments") {
                            Some(Value::String(a)) if !a.is_empty() => {
                                // arguments 是增量字符串，需拼接。
                                slot.args.push_str(a);
                            }
                            // 与非流式路径（parse_tool_call 的 `Some(other) => other.to_string()`）
                            // 对齐：部分兼容网关把 arguments 直接给成 JSON 对象。此前 as_str()
                            // 对对象返回 None → 参数被静默丢弃，空串又被 normalize_arguments
                            // 视为合法的 "{}"，工具最终以空参数执行并把结果持久化进会话。
                            Some(other) if !other.is_null() && !other.is_string() => {
                                slot.args.push_str(&other.to_string());
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        Ok(delta)
    }

    fn finish(&mut self) -> Result<()> {
        // 流以 [DONE] 结束时，必须已收到【明确】的 finish_reason（stop 或 tool_calls）。
        //
        // **修复 P2**：旧实现只在「累积了工具调用槽位」时才检查 stop，纯文本流若全程没收到
        // finish_reason（仅以 [DONE] 收尾），stop 仍为 None，却会被 `into_outcome` 当成
        // 正常文本答案持久化——把截断/不完整的 provider 响应写进会话。现在无论是否有工具
        // 调用槽位，都要求 stop 为明确的 Stop/ToolCalls，否则拒绝。
        if !matches!(
            self.stop,
            Some(FinishReason::Stop) | Some(FinishReason::ToolCalls)
        ) {
            return Err(LlmError::BadResponse(
                "流以 [DONE] 结束但未收到明确 finish_reason（stop/tool_calls），\
                 拒绝把不完整响应持久化为正常答案"
                    .into(),
            ));
        }
        // 工具调用槽位的最终完整性校验：只有全部通过后，provider 才会调用
        // `into_outcome`，运行时也才可能执行这些工具。
        match self.stop {
            Some(FinishReason::ToolCalls) => self.validate_tool_calls()?,
            Some(FinishReason::Stop) if !self.slots.is_empty() => {
                return Err(LlmError::BadResponse(
                    "存在 tool_call 槽位但流未以 finish_reason=tool_calls 正常结束".into(),
                ));
            }
            Some(FinishReason::Stop) => {}
            _ => unreachable!("上方已校验流结束原因"),
        }
        Ok(())
    }

    fn into_outcome(self: Box<Self>) -> ChatOutcome {
        let reasoning = if self.reasoning.is_empty() {
            None
        } else {
            Some(self.reasoning)
        };
        let tool_calls = self
            .slots
            .into_iter()
            .map(|slot| {
                // `StreamParser` 契约要求调用方先执行 `finish()`；provider 已遵守该契约。
                // 若未来调用方绕过最终校验，缺失槽位应 fail loud，而非静默丢弃工具调用。
                let s = slot.expect("into_outcome 前必须通过 tool_call 完整性校验");
                ToolCall {
                    id: s.id.unwrap_or_default(),
                    name: s.name.unwrap_or_default(),
                    // 写回规范化后的 arguments：空串/纯空白 → 无参工具的合法 `{}`，
                    // 非空合法 JSON 原样保留。流式校验（feed/finish）已保证到达此处时 args
                    // 要么空/空白、要么合法 JSON，故 `unwrap_or_else` 仅作防御兜底。
                    input: normalize_arguments(&s.args).unwrap_or_else(|_| "{}".to_string()),
                }
            })
            .collect();
        ChatOutcome {
            text: self.text,
            reasoning,
            tool_calls,
            stop: self.stop,
            usage: self.usage,
        }
    }
}

// ---------------------------------------------------------------------------
// 单测：覆盖请求构造与响应/流式解析的关键路径（不触网）。
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests;
