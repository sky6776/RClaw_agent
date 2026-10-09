//! 规范化消息模型 —— RClaw 全 crate 的「唯一真相来源」。
//!
//! 设计要点：
//! - 采用「Anthropic 风格 content block」作为内部表示，因为它能无损表达
//!   text / reasoning / tool_use / tool_result 的有序序列，并可以双向映射到
//!   OpenAI、Anthropic、智谱等任意 provider 的 wire format。
//! - 这些类型派生 `Serialize/Deserialize`，**直接作为 session JSONL 的存储格式**，
//!   避免 cJSON 与结构体之间反复手工搬运。
//! - LLM 协议层（rclaw-llm）负责把这里的规范化结构翻译成具体 provider 的请求/响应，
//!   因此本模块对「哪个 provider」一无所知。

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 角色枚举
// ---------------------------------------------------------------------------

/// 一条消息的角色。序列化字面量与 OpenAI 兼容协议一致：
/// `system` / `user` / `assistant` / `tool`。
///
/// 为什么需要 `Tool`：OpenAI 兼容协议里，工具执行结果以独立的
/// `role=tool` 消息回传（携带 `tool_call_id`），与 `user` 消息区分开。
/// 而内部 content-block 模型用 `ToolResult` block 表达工具结果，
/// 转换层会决定是放进 `role=tool` 还是 `role=user` 消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

// ---------------------------------------------------------------------------
// 内容块
// ---------------------------------------------------------------------------

/// 一条消息的内容由若干「内容块」有序组成。
///
/// 使用 `#[serde(tag = "type")]` 内部标签，序列化结果与 content block
/// 格式一一对应，例如：
/// - `{"type":"text","text":"..."}`
/// - `{"type":"media_url","kind":"image","url":"data:image/png;base64,..."}`
/// - `{"type":"reasoning","reasoning_content":"..."}`
/// - `{"type":"tool_use","id":"call_x","name":"read","input":{...}}`
/// - `{"type":"tool_result","tool_use_id":"call_x","content":"..."}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    /// 普通文本。
    #[serde(rename = "text")]
    Text { text: String },

    /// 多模态输入资源。
    ///
    /// 内部统一表示为 media_url，协议层再映射到 OpenAI/智谱兼容格式里的
    /// `image_url` / `video_url` / `file_url`。`url` 可以是 HTTPS URL，也可以是
    /// `data:<mime>;base64,...` 形式的小型本地资源。
    #[serde(rename = "media_url")]
    MediaUrl { kind: MediaKind, url: String },

    /// 思考/推理内容（DeepSeek 思考模式、智谱 GLM 思考模式等）。
    ///
    /// 关键约束：发生工具调用的轮次，后续请求必须**完整回传** assistant 的
    /// reasoning_content，否则 DeepSeek 等会返回 400。因此它必须能被持久化、
    /// 并在下一轮重新拼回请求。详见 llm 层 `convert_messages` 与 store 层 session。
    #[serde(rename = "reasoning")]
    Reasoning { reasoning_content: String },

    /// assistant 请求执行某个工具（对应 OpenAI 的 `tool_calls[i]`）。
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        /// 工具入参，结构化 JSON 对象。
        input: serde_json::Value,
    },

    /// 工具执行结果（对应 OpenAI 的 `role=tool` 消息正文）。
    #[serde(rename = "tool_result")]
    ToolResult {
        /// 与对应 `ToolUse.id` 配对，provider 据此把结果接回正确的工具调用。
        tool_use_id: String,
        /// 结果正文（通常是字符串）。
        content: String,
        /// 是否为错误结果。OpenAI 兼容协议不强制该字段，仅作内部标记。
        #[serde(default, skip_serializing_if = "is_false")]
        is_error: bool,
    },
}

/// 多模态资源类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
    File,
}

impl MediaKind {
    /// OpenAI/智谱兼容 content block 的 `type` 字段。
    pub fn openai_block_type(self) -> &'static str {
        match self {
            MediaKind::Image => "image_url",
            MediaKind::Video => "video_url",
            MediaKind::File => "file_url",
        }
    }
}

/// 工具函数：用于 `skip_serializing_if`，省略值为 false 的 `is_error` 字段。
fn is_false(b: &bool) -> bool {
    !*b
}

impl ContentBlock {
    /// 构造一个普通文本块。
    pub fn text<S: Into<String>>(s: S) -> Self {
        ContentBlock::Text { text: s.into() }
    }

    /// 构造一个推理内容块。
    pub fn reasoning<S: Into<String>>(s: S) -> Self {
        ContentBlock::Reasoning {
            reasoning_content: s.into(),
        }
    }

    /// 构造一个多模态资源块。
    pub fn media_url(kind: MediaKind, url: impl Into<String>) -> Self {
        ContentBlock::MediaUrl {
            kind,
            url: url.into(),
        }
    }

    /// 若该块为文本块，返回其文本切片。
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text),
            _ => None,
        }
    }

    /// 若该块为多模态资源块，返回其类型和 URL。
    pub fn as_media_url(&self) -> Option<(MediaKind, &str)> {
        match self {
            ContentBlock::MediaUrl { kind, url } => Some((*kind, url)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 消息
// ---------------------------------------------------------------------------

/// 一条规范化消息。
///
/// `content` 既能从 JSON 数组反序列化（结构化消息），也能从纯字符串反序列化
/// （等价于单个文本块）。后者用于兼容「纯文本消息」的简洁写法。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,

    /// 内容块序列。自定义 (de)serialize 以兼容「字符串或数组」两种来源。
    #[serde(default, with = "content_blocks")]
    pub content: Vec<ContentBlock>,

    /// `role=tool` 消息携带的工具调用 ID，用于配对。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,

    /// Unix 秒级时间戳，持久化时自动写入。
    #[serde(default, skip_serializing_if = "is_zero_i64")]
    pub ts: i64,
}

impl Message {
    /// 构造一条纯文本消息（content 为单个 Text 块）。
    pub fn plain(role: Role, text: impl Into<String>) -> Self {
        Message {
            role,
            content: vec![ContentBlock::text(text)],
            tool_call_id: None,
            ts: 0,
        }
    }

    /// 是否为「工具结果载体」消息。
    ///
    /// 用于历史裁剪时识别「孤儿 tool_result」——若裁剪把对应的 assistant(tool_calls)
    /// 切掉，留下来的 tool_result 会让 OpenAI 兼容 API 因缺少前序 tool_calls 而返回 400，
    /// 必须跳过。
    ///
    /// **两种形态都算（修复 P2）**：
    /// 1. `role=user` 且 content 含 `ToolResult` 块——内部 content-block 模型的常见形态
    ///    （工具执行结果塞进一条 user 消息）；
    /// 2. `role=tool` 的消息——OpenAI 兼容协议里工具结果以独立 `role=tool` 消息回传
    ///    （本仓库部分流程直接用此形态，见 `openai_compat::convert_messages`）。
    ///
    /// 旧实现只认第 1 种，导致历史里若已存在 `role=tool` 消息，session 修复（孤儿裁剪）
    /// 会删掉其前序 assistant(tool_calls) 却【留下孤儿 role=tool】，下一轮请求仍会 400。
    pub fn is_tool_result_carrier(&self) -> bool {
        match self.role {
            // 形态 1：user 消息携带 ToolResult 块。
            Role::User => self
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. })),
            // 形态 2：role=tool 消息本身就是工具结果载体（配对 id 在 tool_call_id 字段）。
            // 纳入孤儿判定，使裁剪/修复逻辑对两种形态一致（详见 store::session::tool_result_ids）。
            Role::Tool => true,
            _ => false,
        }
    }

    /// 是否为含工具调用的 assistant 消息。
    pub fn has_tool_calls(&self) -> bool {
        self.role == Role::Assistant
            && self
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }
}

fn is_zero_i64(v: &i64) -> bool {
    *v == 0
}

/// `content` 字段的柔性 (de)serialize：
/// - 序列化：始终输出数组。
/// - 反序列化：接受数组、字符串（转为单文本块）、或缺失/空（转为空数组）。
mod content_blocks {
    use super::ContentBlock;
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};

    /// 序列化为数组形式：逐个块交给 ContentBlock 自身的 serde 表示。
    pub fn serialize<S>(blocks: &Vec<ContentBlock>, s: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut seq = s.serialize_seq(Some(blocks.len()))?;
        for b in blocks {
            seq.serialize_element(b)?;
        }
        seq.end()
    }

    /// 反序列化：兼容数组 / 字符串 / 空。
    pub fn deserialize<'de, D>(d: D) -> Result<Vec<ContentBlock>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Option::<serde_json::Value>::deserialize(d)?.unwrap_or_default();
        match value {
            serde_json::Value::Null => Ok(Vec::new()),
            serde_json::Value::String(s) => Ok(vec![ContentBlock::text(s)]),
            serde_json::Value::Array(arr) => arr
                .into_iter()
                .map(|v| {
                    // 数组元素：若为字符串则当文本块；否则按结构化块解析。
                    match v {
                        serde_json::Value::String(s) => Ok(ContentBlock::text(s)),
                        other => serde_json::from_value::<ContentBlock>(other)
                            .map_err(serde::de::Error::custom),
                    }
                })
                .collect(),
            // 兼容少数实现把整条 content 放成对象的情况：包成单元素数组。
            other @ (serde_json::Value::Object(_)
            | serde_json::Value::Bool(_)
            | serde_json::Value::Number(_)) => {
                Ok(vec![serde_json::from_value::<ContentBlock>(other)
                    .map_err(serde::de::Error::custom)?])
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 工具调用与工具描述
// ---------------------------------------------------------------------------

/// LLM 一次返回的工具调用。
///
/// 注意 `input` 是**字符串形式的 JSON**（与 OpenAI `function.arguments` 一致），
/// 而非结构化对象；这样能原样回传，也便于 tools 层再 `serde_json::from_str`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// 工具入参的 JSON 字符串（可能是 `"{}"`）。
    pub input: String,
}

/// 工具的规范化描述（Anthropic 风格 `input_schema`）。
///
/// 协议层负责把它翻译成具体 provider 的工具格式，例如 OpenAI 的
/// `{type:"function",function:{name,description,parameters}}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    /// JSON Schema 对象，描述工具入参。
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// 完成原因
// ---------------------------------------------------------------------------

/// 本轮生成的结束原因。对应 OpenAI 兼容协议的 `finish_reason`。
///
/// 只有 `Stop` 与 `ToolCalls` 是「正常结束」，其它都视作异常，不应被持久化
/// （SSE 与非流式两条路径都做同样的判断）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// 正常完成。
    Stop,
    /// 模型请求执行工具调用。
    ToolCalls,
    /// 因 max_tokens 截断，内容不完整。
    Length,
    /// 被内容安全过滤拦截。
    ContentFilter,
    /// 其它/未知字面量。
    Other(String),
}

impl FinishReason {
    /// 是否为可接受的正常结束（Stop / ToolCalls）。
    pub fn is_ok(&self) -> bool {
        matches!(self, FinishReason::Stop | FinishReason::ToolCalls)
    }

    /// 是否表示「需要执行工具」。
    pub fn is_tool_calls(&self) -> bool {
        matches!(self, FinishReason::ToolCalls)
    }

    /// 从 OpenAI 兼容协议的字面量解析。
    pub fn from_openai(s: &str) -> Self {
        match s {
            "stop" => FinishReason::Stop,
            "tool_calls" => FinishReason::ToolCalls,
            "length" => FinishReason::Length,
            "content_filter" => FinishReason::ContentFilter,
            other => FinishReason::Other(other.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// 一轮 LLM 请求 / 响应
// ---------------------------------------------------------------------------

/// 发给 LLM 的一轮请求（规范化、provider 无关）。
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// 系统提示词（若为空则不发 system 消息）。
    pub system: Option<String>,
    /// 历史消息（不含 system，system 单独传）。
    pub messages: Vec<Message>,
    /// 可用工具描述；为空表示本轮不带工具。
    pub tools: Vec<ToolSchema>,
    /// 单轮最大输出 token。
    pub max_tokens: u32,
    /// 是否流式（SSE）。
    pub stream: bool,
}

impl ChatRequest {
    /// 便捷构造。
    pub fn new(system: Option<String>, messages: Vec<Message>) -> Self {
        ChatRequest {
            system,
            messages,
            tools: Vec::new(),
            max_tokens: 4096,
            stream: false,
        }
    }
}

/// LLM 一轮的完整结果（流式时由事件聚合而成，非流式时一次性返回）。
#[derive(Debug, Clone, Default)]
pub struct ChatOutcome {
    /// 累积的普通文本。
    pub text: String,
    /// 累积的推理内容（思考模式）。
    pub reasoning: Option<String>,
    /// 工具调用列表（按 index 顺序）。
    pub tool_calls: Vec<ToolCall>,
    /// 结束原因。
    pub stop: Option<FinishReason>,
    /// Provider 上报的真实 token 用量（OpenAI 兼容 `usage` 对象）。
    ///
    /// provider 未返回（旧端点 / 未开 `stream_options.include_usage`）时为 `None`，
    /// 上层退回纯启发式估算。
    pub usage: Option<TokenUsage>,
}

/// Provider 上报的真实 token 用量（OpenAI 兼容 `usage` 对象的规范化子集）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
}

impl TokenUsage {
    /// 从 OpenAI 兼容 `usage` 对象解析；字段缺失按 0 处理。
    ///
    /// 对象不含任何 usage 键（如空对象 `{}`）时返回 `None`，避免把心跳事件上的
    /// 空 usage 当成真实用量。
    pub fn from_openai(value: &serde_json::Value) -> Option<Self> {
        if !value.is_object() {
            return None;
        }
        let has_any = ["prompt_tokens", "completion_tokens", "total_tokens"]
            .iter()
            .any(|key| value.get(*key).is_some());
        if !has_any {
            return None;
        }
        let get = |key: &str| {
            value
                .get(key)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0)
        };
        Some(TokenUsage {
            prompt_tokens: get("prompt_tokens"),
            completion_tokens: get("completion_tokens"),
            total_tokens: get("total_tokens"),
        })
    }

    /// 总 token；`total_tokens` 缺失（0）时用 prompt+completion 兜底。
    pub fn total(&self) -> u64 {
        if self.total_tokens > 0 {
            self.total_tokens
        } else {
            self.prompt_tokens.saturating_add(self.completion_tokens)
        }
    }
}

impl ChatOutcome {
    /// 是否请求了工具调用。
    pub fn wants_tools(&self) -> bool {
        self.stop.as_ref().is_some_and(|f| f.is_tool_calls()) || !self.tool_calls.is_empty()
    }
}

// ---------------------------------------------------------------------------
// 流式事件
// ---------------------------------------------------------------------------

/// SSE 流式过程中产生的规范化事件（provider 无关）。
///
/// 协议层把各家 provider 的增量（OpenAI 的 `delta.*`、Anthropic 的
/// `content_block_delta` 等）统一翻译成本枚举，供上层实时输出与聚合。
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// 文本增量。
    Text(String),
    /// 推理内容增量（思考模式）。
    Reasoning(String),
    /// 工具调用增量。
    ///
    /// `index` 标识属于本轮第几个工具调用；多个增量按 index 累积，
    /// `id`/`name` 通常只在首包出现，`arguments` 为分片需拼接。
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments: String,
    },
    /// 流正常结束。
    Finish(FinishReason),
    /// 流内错误（如 API 返回 `error` 对象）。
    Error(String),
}

// ===========================================================================
// 单元测试：聚焦于「会话存储与协议转换都依赖的序列化语义」。
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    /// 纯文本消息应能从「字符串 content」反序列化为单文本块。
    #[test]
    fn message_deserialize_string_content() {
        let json = r#"{"role":"user","content":"今天天气"}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content.len(), 1);
        assert_eq!(m.content[0].as_text().unwrap(), "今天天气");
    }

    /// 结构化消息（content 数组含 tool_use）应正确反序列化。
    #[test]
    fn message_deserialize_array_content_with_tool_use() {
        let json = r#"{"role":"assistant","content":[
            {"type":"text","text":"hello"},
            {"type":"tool_use","id":"call_1","name":"read","input":{"path":"agent/x"}}
        ]}"#;
        let m: Message = serde_json::from_str(json).unwrap();
        assert_eq!(m.role, Role::Assistant);
        assert_eq!(m.content.len(), 2);
        assert!(matches!(m.content[0], ContentBlock::Text { .. }));
        assert!(matches!(m.content[1], ContentBlock::ToolUse { .. }));
    }

    /// 多模态资源块应能序列化/反序列化，供会话与协议层共享。
    #[test]
    fn media_url_block_roundtrip() {
        let m = Message {
            role: Role::User,
            content: vec![
                ContentBlock::media_url(MediaKind::Image, "data:image/png;base64,abc"),
                ContentBlock::text("看这张图"),
            ],
            tool_call_id: None,
            ts: 0,
        };
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["content"][0]["type"], "media_url");
        assert_eq!(v["content"][0]["kind"], "image");
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(m, back);
    }

    /// tool_result 载体识别（孤儿裁剪依赖此判定）。
    #[test]
    fn detects_tool_result_carrier() {
        let m = Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "ok".into(),
                is_error: false,
            }],
            tool_call_id: None,
            ts: 0,
        };
        assert!(m.is_tool_result_carrier());
        assert!(!Message::plain(Role::User, "hi").is_tool_result_carrier());
    }

    /// 序列化始终输出数组；且 round-trip 等价。
    #[test]
    fn message_serialize_always_array_and_roundtrip() {
        let m = Message::plain(Role::Assistant, "reply");
        let v = serde_json::to_value(&m).unwrap();
        assert!(v["content"].is_array(), "content 必须序列化为数组");
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(m, back);
    }

    /// FinishReason 解析与判定。
    #[test]
    fn finish_reason_from_openai() {
        assert!(FinishReason::from_openai("stop").is_ok());
        assert!(FinishReason::from_openai("tool_calls").is_tool_calls());
        assert!(!FinishReason::from_openai("length").is_ok());
        assert!(matches!(
            FinishReason::from_openai("foo"),
            FinishReason::Other(_)
        ));
    }
}
