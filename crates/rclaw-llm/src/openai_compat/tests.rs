use super::*;
use rclaw_core::{ContentBlock, ToolSchema};

fn cfg() -> ProviderConfig {
    ProviderConfig::deepseek(secrecy::Secret::new(String::from("sk-test")))
}

fn cfg_multimodal() -> ProviderConfig {
    let mut cfg = cfg();
    cfg.default_model = "glm-5.3-flash".into();
    cfg
}

/// 钉死 DeepSeek 实际请求 URL：配置只存 base（不含 `/chat/completions`），
/// 后缀由 [`OpenAiCompat::endpoint`] 拼接，拼出的完整 URL 必须与上游 CClaw 的
/// `AGENT_LLM_API_URL`（`"https://api.deepseek.com/v1/chat/completions"`）逐字节一致。
/// 一旦有人误把 `/chat/completions` 也写进 `base_url`（导致双拼），本测试立刻失败。
#[test]
fn deepseek_endpoint_equals_full_upstream_url() {
    let url = OpenAiCompat::new().endpoint(&cfg());
    assert_eq!(url, "https://api.deepseek.com/v1/chat/completions");
}

#[test]
fn builds_system_and_user_messages() {
    let req = ChatRequest::new(
        Some("You are helpful.".into()),
        vec![Message::plain(Role::User, "hi")],
    );
    let body = OpenAiCompat::new()
        .build_request_body(&cfg(), &req, false)
        .unwrap();
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "You are helpful.");
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[1]["content"], "hi");
    assert_eq!(body["model"], "deepseek-v4-pro");
    assert_eq!(body["max_tokens"], 4096);
}

/// `req.max_tokens` 必须真正写入 body（而非被 `cfg.max_tokens` 覆盖）。
/// 这里把 req 设为 1024、cfg 仍为 4096，body 应取 req 的 1024。
#[test]
fn honors_request_max_tokens_overrides() {
    let mut req = ChatRequest::new(None, vec![Message::plain(Role::User, "hi")]);
    req.max_tokens = 1024;
    let body = OpenAiCompat::new()
        .build_request_body(&cfg(), &req, false)
        .unwrap();
    assert_eq!(body["max_tokens"], 1024, "协议层应采用 req.max_tokens");
}

fn request_with_tool() -> ChatRequest {
    let mut req = ChatRequest::new(None, vec![Message::plain(Role::User, "hi")]);
    req.tools.push(ToolSchema {
        name: "read".into(),
        description: "read a file".into(),
        input_schema: json!({"type": "object"}),
    });
    req
}

#[test]
fn zhipu_stream_request_enables_tool_stream() {
    let mut zhipu = cfg();
    zhipu.name = "zhipu".into();
    zhipu.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
    zhipu.default_model = "glm-5.3".into();

    let body = OpenAiCompat::new()
        .build_request_body(&zhipu, &request_with_tool(), true)
        .unwrap();
    assert_eq!(body["stream"], true);
    assert_eq!(body["tool_stream"], true);
}

/// glm-5.3-flash 按官方「推荐设置」注入采样与思考参数，流式/非流式都生效。
#[test]
fn zhipu_flash_request_injects_recommended_params() {
    let mut zhipu_flash = cfg();
    zhipu_flash.name = "zhipu".into();
    zhipu_flash.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
    zhipu_flash.default_model = "glm-5.3-flash".into();

    for stream in [false, true] {
        let body = OpenAiCompat::new()
            .build_request_body(&zhipu_flash, &request_with_tool(), stream)
            .unwrap();
        assert_eq!(body["temperature"], 1);
        assert_eq!(body["top_p"], 0.95);
        assert_eq!(body["reasoning_effort"], "max");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["clear_thinking"], false);
    }
}

/// 推荐参数只对智谱 + glm-5.3-flash 注入：其它模型（glm-5.3）与非智谱 provider 一律不加，
/// 保持既有模型行为不变、未知字段不透传。
#[test]
fn flash_recommended_params_only_for_flash_on_zhipu() {
    let mut zhipu53 = cfg();
    zhipu53.name = "zhipu".into();
    zhipu53.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
    zhipu53.default_model = "glm-5.3".into();
    let mut body = json!({});
    apply_zhipu_flash_recommended_params(&zhipu53, &mut body);
    assert!(body.get("temperature").is_none());

    // 非智谱 provider（deepseek 名 + deepseek 域名）即便模型名叫 glm-5.3-flash 也不注入。
    let mut non_zhipu_flash = cfg();
    non_zhipu_flash.default_model = "glm-5.3-flash".into();
    let mut body = json!({});
    apply_zhipu_flash_recommended_params(&non_zhipu_flash, &mut body);
    assert!(body.get("temperature").is_none());

    // 正对照：门开时确有注入（完整字段断言见 zhipu_flash_request_injects_recommended_params）。
    let mut zhipu_flash = cfg();
    zhipu_flash.name = "zhipu".into();
    zhipu_flash.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
    zhipu_flash.default_model = "glm-5.3-flash".into();
    let mut body = json!({});
    apply_zhipu_flash_recommended_params(&zhipu_flash, &mut body);
    assert_eq!(body["temperature"], 1);
}

#[test]
fn non_zhipu_stream_request_omits_tool_stream() {
    let body = OpenAiCompat::new()
        .build_request_body(&cfg(), &request_with_tool(), true)
        .unwrap();
    assert_eq!(body["stream"], true);
    assert!(body.get("tool_stream").is_none());
}

#[test]
fn converts_assistant_tool_use_and_tool_result_to_wire() {
    let assistant = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![
            ContentBlock::text("thinking"),
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "read".into(),
                input: json!({"path":"agent/x"}),
            },
        ],
        tool_call_id: None,
        ts: 0,
    };
    let user_tool_result = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "file content".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let req = ChatRequest::new(None, vec![assistant, user_tool_result]);
    let body = OpenAiCompat::new()
        .build_request_body(&cfg(), &req, false)
        .unwrap();
    let msgs = body["messages"].as_array().unwrap();
    // assistant: content + tool_calls
    assert_eq!(msgs[0]["role"], "assistant");
    assert_eq!(msgs[0]["content"], "thinking");
    assert_eq!(msgs[0]["tool_calls"][0]["id"], "call_1");
    assert_eq!(msgs[0]["tool_calls"][0]["function"]["name"], "read");
    // tool_result → role=tool 消息
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "call_1");
    assert_eq!(msgs[1]["content"], "file content");
}

#[test]
fn converts_user_media_to_openai_content_array() {
    let user = Message {
        id: 0,
        role: Role::User,
        content: vec![
            ContentBlock::media_url(rclaw_core::MediaKind::Image, "data:image/png;base64,abc"),
            ContentBlock::text("请分析图片"),
        ],
        tool_call_id: None,
        ts: 0,
    };
    let req = ChatRequest::new(None, vec![user]);
    let body = OpenAiCompat::new()
        .build_request_body(&cfg_multimodal(), &req, false)
        .unwrap();
    let content = &body["messages"].as_array().unwrap()[0]["content"];
    assert!(content.is_array(), "含媒体时 content 应为数组: {content}");
    assert_eq!(content[0]["type"], "image_url");
    assert_eq!(content[0]["image_url"]["url"], "data:image/png;base64,abc");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "请分析图片");
}

#[test]
fn omits_user_media_for_non_multimodal_model() {
    let user = Message {
        id: 0,
        role: Role::User,
        content: vec![
            ContentBlock::media_url(rclaw_core::MediaKind::Image, "data:image/png;base64,abc"),
            ContentBlock::text("请分析图片"),
        ],
        tool_call_id: None,
        ts: 0,
    };
    let req = ChatRequest::new(None, vec![user]);
    let body = OpenAiCompat::new()
        .build_request_body(&cfg(), &req, false)
        .unwrap();
    let content = &body["messages"].as_array().unwrap()[0]["content"];
    assert_eq!(content, "请分析图片");
}

// -------------------------------------------------------------------------
// P2：坏 tool_use 必须整条工具链拒绝（fail loud），不得局部过滤留下孤儿 tool_result
// -------------------------------------------------------------------------

/// 构造一条带【非法 tool_use（空 name）】+ 对应 tool_result 的历史。
/// 旧实现（filter_map）会静默丢掉这条 tool_use，但 tool_result 仍被序列化成 role=tool，
/// 形成无前序 tool_calls 的孤儿消息 → 下游 400。现在应直接报错。
fn bad_assistant_tool_use_history(empty_name: bool, input: Value) -> ChatRequest {
    let assistant = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: if empty_name {
                String::new()
            } else {
                "read".into()
            },
            input,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let tool_result = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "result".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    ChatRequest::new(None, vec![assistant, tool_result])
}

/// 空 name 的 tool_use 必须被拒绝（而非过滤后留下孤儿 tool_result）。
#[test]
fn rejects_assistant_tool_use_with_empty_name() {
    let req = bad_assistant_tool_use_history(true, json!({"path": "a"}));
    let r = OpenAiCompat::new().build_request_body(&cfg(), &req, false);
    assert!(
        r.is_err(),
        "空 name 的 tool_use 应被拒绝（fail loud），而非过滤后留下孤儿 tool_result"
    );
}

/// `input = null` 必须被拒绝：旧实现放行后会序列化成 `arguments:"null"` 致下游 400。
#[test]
fn rejects_assistant_tool_use_with_null_input() {
    let req = bad_assistant_tool_use_history(false, Value::Null);
    let r = OpenAiCompat::new().build_request_body(&cfg(), &req, false);
    assert!(
        r.is_err(),
        "input=null 的 tool_use 应被拒绝，而非序列化成 arguments:null"
    );
}

/// 非对象 input（字符串）必须被拒绝：会变成非法 arguments 文本。
#[test]
fn rejects_assistant_tool_use_with_string_input() {
    let req = bad_assistant_tool_use_history(false, json!("not an object"));
    let r = OpenAiCompat::new().build_request_body(&cfg(), &req, false);
    assert!(
        r.is_err(),
        "非对象 input（字符串）的 tool_use 应被拒绝，而非当成合法 arguments"
    );
}

#[test]
fn parses_full_response_with_tool_calls() {
    let body = br#"{
            "choices":[{
                "message":{
                    "content":"",
                    "tool_calls":[{"id":"call_1","type":"function",
                        "function":{"name":"read","arguments":"{\"path\":\"a\"}"}}]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
    let out = OpenAiCompat::new()
        .parse_full_response(body, &Dialect::deepseek(), 4)
        .unwrap();
    assert_eq!(out.stop, Some(FinishReason::ToolCalls));
    assert_eq!(out.tool_calls.len(), 1);
    assert_eq!(out.tool_calls[0].id, "call_1");
}

#[test]
fn rejects_abnormal_finish_in_full_response() {
    let body = br#"{"choices":[{"message":{"content":"x"},"finish_reason":"length"}]}"#;
    let r = OpenAiCompat::new().parse_full_response(body, &Dialect::deepseek(), 4);
    assert!(r.is_err());
}

/// 畸形 arguments 必须整轮拒绝，而非静默当成 {} 执行/持久化。
#[test]
fn rejects_malformed_tool_arguments() {
    let body = br#"{
            "choices":[{
                "message":{
                    "content":"",
                    "tool_calls":[{"id":"call_1","type":"function",
                        "function":{"name":"read","arguments":"{not json"}}]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
    let r = OpenAiCompat::new().parse_full_response(body, &Dialect::deepseek(), 4);
    assert!(r.is_err(), "畸形 arguments 应被拒绝");
}

/// 空串 arguments 视为无参工具的合法 {} 表示。
#[test]
fn empty_arguments_normalized_to_object() {
    let body = br#"{
            "choices":[{
                "message":{
                    "tool_calls":[{"id":"call_1","type":"function",
                        "function":{"name":"ping","arguments":""}}]
                },
                "finish_reason":"tool_calls"
            }]
        }"#;
    let out = OpenAiCompat::new()
        .parse_full_response(body, &Dialect::deepseek(), 4)
        .unwrap();
    assert_eq!(out.tool_calls[0].input, "{}");
}

#[test]
fn stream_parser_accumulates_tool_call_deltas() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    // 第一片：index=0 带 id/name + arguments 开头
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"pa"}}]}}]}"#).unwrap();
    // 第二片：arguments 追加（同一 index）
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}"#).unwrap();
    // 结束：finish=tool_calls，校验通过
    let d = p
        .feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    assert_eq!(d.finished, Some(FinishReason::ToolCalls));
    p.finish().unwrap();
    let out = p.into_outcome();
    assert_eq!(out.tool_calls.len(), 1);
    assert_eq!(out.tool_calls[0].id, "call_1");
    assert_eq!(out.tool_calls[0].name, "read");
    // 两片 arguments 拼接后应为合法 JSON：{"path":"a"}
    assert_eq!(out.tool_calls[0].input, r#"{"path":"a"}"#);
}

/// 回归：单轮内并行发起多个工具调用（如批量导入 7 个文档），只要不超过配置的
/// `max_tool_calls` 上限就必须全部解析成功。旧实现上限硬编码 4，第 5 个调用
/// （index 4）即触发 `tool_call index 4 越界` 并拒绝整轮——这正是用户批量导入
/// 文档时遇到的错误。
#[test]
fn stream_parser_accepts_many_parallel_tool_calls_within_limit() {
    // 模拟「一次性导入 7 个文档」：模型一轮内并行发起 7 次工具调用。
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 16);
    for i in 0..7u64 {
        let chunk = serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": i,
                "id": format!("call_{i}"),
                "type": "function",
                "function": {"name": "import_doc", "arguments": "{}"}
            }]}}]
        });
        p.feed(&chunk.to_string()).unwrap();
    }
    let d = p
        .feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    assert_eq!(d.finished, Some(FinishReason::ToolCalls));
    p.finish().unwrap();
    let out = p.into_outcome();
    assert_eq!(out.tool_calls.len(), 7, "7 个并行调用应全部解析成功");
    for (i, tc) in out.tool_calls.iter().enumerate() {
        assert_eq!(tc.id, format!("call_{i}"));
        assert_eq!(tc.name, "import_doc");
    }
}

#[test]
fn stream_parser_accepts_out_of_order_tool_call_indices() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    // 智谱可能先发送 index=1，再发送 index=0；首次出现顺序不影响最终顺序。
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","function":{"name":"second","arguments":"{}"}}]}}]}"#).unwrap();
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_0","function":{"name":"first","arguments":"{}"}}]}}]}"#).unwrap();
    p.feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    p.finish().unwrap();

    let out = p.into_outcome();
    assert_eq!(out.tool_calls.len(), 2);
    assert_eq!(out.tool_calls[0].id, "call_0");
    assert_eq!(out.tool_calls[0].name, "first");
    assert_eq!(out.tool_calls[1].id, "call_1");
    assert_eq!(out.tool_calls[1].name, "second");
}

#[test]
fn stream_parser_rejects_missing_index_when_stream_finishes() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    // index=1 可以先暂存；若直到 SSE 流结束仍没有 index=0，整轮才被拒绝。
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":1,"id":"call_1","function":{"name":"f","arguments":"{}"}}]}}]}"#).unwrap();
    p.feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    let error = p.finish().unwrap_err();
    assert!(error.to_string().contains("缺少 index=0"));
}

#[test]
fn stream_parser_rejects_invalid_arguments_when_stream_finishes() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_0","function":{"name":"read","arguments":"{bad json"}}]}}]}"#).unwrap();
    p.feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();

    let error = p.finish().unwrap_err();
    assert!(error.to_string().contains("arguments 非合法 JSON"));
}

#[test]
fn stream_parser_rejects_tool_calls_finish_without_any_call() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();

    let error = p.finish().unwrap_err();
    assert!(error.to_string().contains("无 tool_call"));
}

/// 零参工具（如 cron_list）在流式里 provider 不发 arguments 字段：不得因 args 空
/// 而判失败，应成功并把 input 规整为 `{}`。
#[test]
fn stream_parser_allows_missing_arguments() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    // 工具调用增量只带 id/name，没有 arguments 字段。
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"cron_list"}}]}}]}"#).unwrap();
    let d = p
        .feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    assert_eq!(d.finished, Some(FinishReason::ToolCalls));
    p.finish().unwrap();
    let out = p.into_outcome();
    assert_eq!(out.tool_calls.len(), 1);
    assert_eq!(out.tool_calls[0].name, "cron_list");
    assert_eq!(out.tool_calls[0].input, "{}", "空参数应规整为空对象");
}

/// 流式里 arguments 为空串（provider 显式回传 ""）也应成功并规整为 `{}`。
#[test]
fn stream_parser_allows_empty_arguments_string() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"cron_list","arguments":""}}]}}]}"#).unwrap();
    let d = p
        .feed(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
        .unwrap();
    assert_eq!(d.finished, Some(FinishReason::ToolCalls));
    p.finish().unwrap();
    assert_eq!(p.into_outcome().tool_calls[0].input, "{}");
}

/// 只有 tool_call 增量、流以 [DONE] 结束但从未收到 finish_reason：finish() 必须拒绝，
/// 否则 into_outcome 会把不完整槽位补成空 id/name。
#[test]
fn finish_rejects_slots_without_tool_calls_stop() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{}"}}]}}]}"#).unwrap();
    assert!(
        p.finish().is_err(),
        "有 tool_call 槽位但无 tool_calls 终止应被拒绝"
    );
}

/// 槽位缺关键字段（只有 id、没有 name）：finish() 必须拒绝。
#[test]
fn finish_rejects_incomplete_slot() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"arguments":"{}"}}]}}]}"#).unwrap();
    assert!(p.finish().is_err(), "不完整槽位应被拒绝");
}

#[test]
fn done_marker() {
    let p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    assert!(p.is_done_marker("[DONE]"));
    assert!(!p.is_done_marker("{\"x\":1}"));
}

/// P3：role=tool 消息缺失 tool_call_id 必须被本地拒绝，而非序列化成空串致 400/配对错误。
#[test]
fn rejects_tool_message_without_tool_call_id() {
    let bad = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("result")],
        tool_call_id: None, // 缺失
        ts: 0,
    };
    let req = ChatRequest::new(None, vec![bad]);
    let r = OpenAiCompat::new().build_request_body(&cfg(), &req, false);
    assert!(r.is_err(), "缺 tool_call_id 的 role=tool 消息应被本地拒绝");
}

/// P3：tool_call_id 为空串同样应拒绝。
#[test]
fn rejects_tool_message_with_empty_tool_call_id() {
    let bad = Message {
        id: 0,
        role: Role::Tool,
        content: vec![ContentBlock::text("result")],
        tool_call_id: Some(String::new()), // 空串
        ts: 0,
    };
    let req = ChatRequest::new(None, vec![bad]);
    let r = OpenAiCompat::new().build_request_body(&cfg(), &req, false);
    assert!(r.is_err(), "空 tool_call_id 的 role=tool 消息应被本地拒绝");
}

// -------------------------------------------------------------------------
// P2：reasoning_field = none 真正生效（请求不写、响应不读）
// -------------------------------------------------------------------------

/// 构造一份 reasoning_field = none 的 provider 配置。
fn cfg_none_reasoning() -> ProviderConfig {
    let mut c = cfg();
    c.dialect = Dialect {
        reasoning_field: ReasoningField::None,
        supports_tool_choice: true,
        max_tokens_key: MaxTokensKey::MaxTokens,
    };
    c
}

/// P2：none 方言下，请求体【不得】包含 reasoning_content / reasoning 字段，
/// 即便历史 assistant 消息里持久化了推理内容。
#[test]
fn none_reasoning_field_omits_reasoning_in_request() {
    let cfg = cfg_none_reasoning();
    let assistant = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![
            ContentBlock::text("answer"),
            ContentBlock::reasoning("secret thinking"),
        ],
        tool_call_id: None,
        ts: 0,
    };
    let req = ChatRequest::new(None, vec![assistant]);
    let body = OpenAiCompat::new()
        .build_request_body(&cfg, &req, false)
        .unwrap();
    let msg = &body["messages"].as_array().unwrap()[0];
    assert_eq!(msg["role"], "assistant");
    assert!(
        msg.get("reasoning_content").is_none() || msg["reasoning_content"].is_null(),
        "none 方言不得序列化 reasoning_content"
    );
    assert!(
        msg.get("reasoning").is_none() || msg["reasoning"].is_null(),
        "none 方言不得序列化 reasoning"
    );
}

/// P2：none 方言下，响应解析【不得】读取 reasoning_content，
/// 即便 provider 在响应里返回了该字段。
#[test]
fn none_reasoning_field_skips_response_reasoning() {
    let cfg = cfg_none_reasoning();
    let body = br#"{"choices":[{"message":{"content":"hi","reasoning_content":"should be ignored"},"finish_reason":"stop"}]}"#;
    let out = OpenAiCompat::new()
        .parse_full_response(body, &cfg.dialect, 4)
        .unwrap();
    assert!(
        out.reasoning.is_none(),
        "none 方言不得解析 reasoning_content"
    );
    assert_eq!(out.text, "hi");
    assert_eq!(out.stop, Some(FinishReason::Stop));
}

// -------------------------------------------------------------------------
// P2：缺失 finish_reason 必须拒绝（非流式 / 流式）
// -------------------------------------------------------------------------

/// P2：非流式响应缺失 finish_reason 必须被拒绝，而非默认 Stop 接受。
#[test]
fn rejects_missing_finish_reason_in_full_response() {
    let body = br#"{"choices":[{"message":{"content":"hi"}}]}"#;
    let r = OpenAiCompat::new().parse_full_response(body, &Dialect::deepseek(), 4);
    assert!(
        r.is_err(),
        "缺失 finish_reason 的响应应被拒绝，而非默认 Stop 接受"
    );
}

/// P2：纯文本流以 [DONE] 结束但从未收到 finish_reason 时，finish() 必须拒绝，
/// 而非当成正常文本答案持久化。
#[test]
fn finish_rejects_text_stream_without_explicit_stop() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    // 仅累积文本增量，从不发 finish_reason（模拟截断/不完整的 provider 流）。
    p.feed(r#"{"choices":[{"delta":{"content":"hello"}}]}"#)
        .unwrap();
    assert!(
        p.finish().is_err(),
        "无明确 finish_reason 的文本流应被拒绝，而非持久化为正常答案"
    );
}

/// 反向保护：文本流【有】明确 finish_reason=stop 时，finish() 必须通过（不得误伤）。
#[test]
fn finish_accepts_text_stream_with_explicit_stop() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{"content":"hello"}}]}"#)
        .unwrap();
    p.feed(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
        .unwrap();
    assert!(
        p.finish().is_ok(),
        "有明确 finish_reason=stop 的文本流应通过"
    );
    let out = p.into_outcome();
    assert_eq!(out.text, "hello");
    assert_eq!(out.stop, Some(FinishReason::Stop));
}

// ---------------------------------------------------------------------------
// A1：真实 usage 捕获（请求侧 stream_options + 响应侧 usage 解析）
// ---------------------------------------------------------------------------

#[test]
fn stream_request_requests_usage_in_stream_options() {
    let body = OpenAiCompat::new()
        .build_request_body(
            &cfg(),
            &ChatRequest::new(None, vec![Message::plain(Role::User, "hi")]),
            true,
        )
        .unwrap();
    assert_eq!(body["stream_options"]["include_usage"], true);
    // 非流式请求不应携带 stream_options。
    let body = OpenAiCompat::new()
        .build_request_body(
            &cfg(),
            &ChatRequest::new(None, vec![Message::plain(Role::User, "hi")]),
            false,
        )
        .unwrap();
    assert!(body.get("stream_options").is_none());
}

#[test]
fn full_response_parses_usage_object() {
    let body = br#"{
        "usage": {"prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150},
        "choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]
    }"#;
    let out = OpenAiCompat::new()
        .parse_full_response(body, &Dialect::deepseek(), 4)
        .unwrap();
    let usage = out.usage.expect("非流式响应应解析出 usage");
    assert_eq!(usage.prompt_tokens, 120);
    assert_eq!(usage.completion_tokens, 30);
    assert_eq!(usage.total_tokens, 150);
}

#[test]
fn full_response_without_usage_parses_to_none() {
    let body = br#"{"choices":[{"message":{"content":"ok"},"finish_reason":"stop"}]}"#;
    let out = OpenAiCompat::new()
        .parse_full_response(body, &Dialect::deepseek(), 4)
        .unwrap();
    assert!(out.usage.is_none());
}

#[test]
fn stream_parser_captures_usage_chunk_before_done() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    p.feed(r#"{"choices":[{"delta":{"content":"hi"}}]}"#)
        .unwrap();
    p.feed(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)
        .unwrap();
    // include_usage chunk：choices 为空数组 + usage 对象。
    p.feed(
        r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}}"#,
    )
    .unwrap();
    assert!(p.is_done_marker("[DONE]"));
    p.finish().unwrap();
    let out = p.into_outcome();
    let usage = out.usage.expect("流式 usage chunk 应被捕获");
    assert_eq!(usage.total_tokens, 12);
}

#[test]
fn usage_from_openai_rejects_empty_object() {
    assert!(TokenUsage::from_openai(&json!({})).is_none());
    assert!(TokenUsage::from_openai(&json!(null)).is_none());
    let usage = TokenUsage::from_openai(&json!({"prompt_tokens": 5})).unwrap();
    // total 缺失时用 prompt+completion 兜底。
    assert_eq!(usage.total(), 5);
}
