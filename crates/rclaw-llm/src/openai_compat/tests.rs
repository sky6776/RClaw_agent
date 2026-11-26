use super::*;
use rclaw_core::{ContentBlock, ToolSchema};

fn cfg() -> ProviderConfig {
    ProviderConfig::deepseek(secrecy::Secret::new(String::from("sk-test")))
}

fn cfg_multimodal() -> ProviderConfig {
    let mut cfg = cfg();
    cfg.default_model = "glm-5.3-flash".into();
    cfg.name = "zhipu".into();
    cfg.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
    cfg.flavor = ProviderFlavor::Zhipu;
    cfg.media_wire = MediaWire::OpenAiCompat;
    cfg
}

/// 智谱官方端点夹具：flavor/media_wire 必须与真实构造一致（不能只改 name/host）。
fn zhipu_cfg() -> ProviderConfig {
    let mut cfg = cfg();
    cfg.name = "zhipu".into();
    cfg.base_url = "https://open.bigmodel.cn/api/coding/paas/v4".into();
    cfg.flavor = ProviderFlavor::Zhipu;
    cfg.media_wire = MediaWire::OpenAiCompat;
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
    let mut zhipu = zhipu_cfg();
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
    let mut zhipu_flash = zhipu_cfg();
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
    let mut zhipu53 = zhipu_cfg();
    zhipu53.default_model = "glm-5.3".into();
    let mut body = json!({});
    apply_reasoning_params(&zhipu53, &mut body);
    assert!(body.get("temperature").is_none());

    // 非智谱 provider（deepseek 名 + deepseek 域名）即便模型名叫 glm-5.3-flash 也不注入。
    let mut non_zhipu_flash = cfg();
    non_zhipu_flash.default_model = "glm-5.3-flash".into();
    let mut body = json!({});
    apply_reasoning_params(&non_zhipu_flash, &mut body);
    assert!(body.get("temperature").is_none());

    // 正对照：门开时确有注入（完整字段断言见 zhipu_flash_request_injects_recommended_params）。
    let mut zhipu_flash = zhipu_cfg();
    zhipu_flash.default_model = "glm-5.3-flash".into();
    let mut body = json!({});
    apply_reasoning_params(&zhipu_flash, &mut body);
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
    let text = content.as_str().expect("纯文本模型应保持 content 字符串");
    assert!(text.contains("请分析图片"), "{text}");
    assert!(text.contains("attachment(s) omitted"), "{text}");
    assert!(text.contains("text-only"), "{text}");
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

/// DeepSeek 的 `insufficient_system_resource` / `aborted` 是上游繁忙，不是协议错误：
/// 必须归为可重试，否则一次服务端抖动会让整轮直接失败且不重试。
#[test]
fn upstream_busy_finish_is_retryable_in_full_response() {
    for fr in ["insufficient_system_resource", "aborted"] {
        let body =
            format!(r#"{{"choices":[{{"message":{{"content":"x"}},"finish_reason":"{fr}"}}]}}"#);
        let err = OpenAiCompat::new()
            .parse_full_response(body.as_bytes(), &Dialect::deepseek(), 4)
            .expect_err("上游繁忙结束原因不应被当成正常响应");
        assert!(matches!(err, LlmError::UpstreamBusy(_)), "got {err:?}");
        assert!(err.is_retryable(), "{err:?}");
    }
}

#[test]
fn upstream_busy_finish_is_retryable_in_stream() {
    for fr in ["insufficient_system_resource", "aborted"] {
        let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
        let err = p
            .feed(&format!(
                r#"{{"choices":[{{"delta":{{}},"finish_reason":"{fr}"}}]}}"#
            ))
            .expect_err("上游繁忙结束原因不应被当成正常流结束");
        assert!(matches!(err, LlmError::UpstreamBusy(_)), "got {err:?}");
        assert!(err.is_retryable(), "{err:?}");
    }
}

#[test]
fn null_error_field_does_not_abort_chunk() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    let delta = p
        .feed(r#"{"error":null,"choices":[{"delta":{"content":"hi"}}]}"#)
        .expect("error:null 不是错误事件");
    assert_eq!(delta.text.as_deref(), Some("hi"));
}

#[test]
fn sse_transient_code_wins_over_context_wording() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    let err = p
        .feed(
            r#"{"error":{"code":"rate_limit_exceeded","message":"request too large for this endpoint"}}"#,
        )
        .expect_err("限流错误不是心跳");
    assert!(err.is_retryable(), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("rate_limit_exceeded"), "{text}");
    assert!(!text.contains("context_length_exceeded"), "{text}");
}

#[test]
fn sse_transient_code_is_normalized_and_beats_conflicting_type() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    let err = p
        .feed(r#"{"error":{"code":" RATE_LIMIT_EXCEEDED ","type":"context_length_exceeded"}}"#)
        .expect_err("限流错误不是心跳");
    assert!(err.is_retryable(), "{err:?}");
    let text = err.to_string();
    assert!(text.contains("rate_limit_exceeded"), "{text}");
    assert!(!text.contains("context_length_exceeded"), "{text}");
}

#[test]
fn sse_content_policy_error_is_not_retryable() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    let err = p
        .feed(r#"{"error":{"message":"blocked by content policy"}}"#)
        .expect_err("内容策略错误不是心跳");
    assert!(!err.is_retryable(), "{err:?}");
}

#[test]
fn error_with_empty_choices_is_not_treated_as_heartbeat() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    let err = p
        .feed(r#"{"choices":[],"error":{"message":"invalid api key","code":"invalid_api_key"}}"#)
        .expect_err("error 必须先于空 choices 判断");
    assert!(matches!(err, LlmError::StreamAborted(_)), "{err:?}");
    assert!(!err.is_retryable(), "鉴权错误不应按瞬时失败重试: {err:?}");
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
        classify_upstream_busy: false,
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

// ---------------------------------------------------------------------------
// 媒体方言：DeepSeek 只认 image_url + file(file_data)，无 video
// ---------------------------------------------------------------------------

fn deepseek_flash_cfg() -> ProviderConfig {
    let mut cfg = cfg();
    cfg.default_model = "deepseek-flash".into();
    cfg
}

fn media_message(kind: rclaw_core::MediaKind, url: &str) -> Message {
    Message {
        id: 0,
        role: Role::User,
        content: vec![
            ContentBlock::text("see attachment"),
            ContentBlock::media_url(kind, url),
        ],
        tool_call_id: None,
        ts: 0,
    }
}

fn body_content(body: &serde_json::Value) -> &serde_json::Value {
    &body["messages"][0]["content"]
}

#[test]
fn deepseek_image_uses_image_url() {
    let req = ChatRequest::new(
        None,
        vec![media_message(
            rclaw_core::MediaKind::Image,
            "data:image/png;base64,abc",
        )],
    );
    let body = OpenAiCompat::new()
        .build_request_body(&deepseek_flash_cfg(), &req, false)
        .unwrap();
    let content = body_content(&body).as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert_eq!(content[1]["image_url"]["url"], "data:image/png;base64,abc");
}

#[test]
fn deepseek_data_uri_file_uses_file_data_without_prefix() {
    let req = ChatRequest::new(
        None,
        vec![media_message(
            rclaw_core::MediaKind::File,
            "data:application/pdf;base64,QUJD",
        )],
    );
    let body = OpenAiCompat::new()
        .build_request_body(&deepseek_flash_cfg(), &req, false)
        .unwrap();
    let content = body_content(&body).as_array().unwrap();
    assert_eq!(content[1]["type"], "file");
    assert_eq!(content[1]["file"]["file_data"], "QUJD");
    assert!(!content[1]["file"]["file_data"]
        .as_str()
        .unwrap()
        .contains("data:"));
}

#[test]
fn deepseek_remote_file_falls_back_to_text_without_query() {
    let req = ChatRequest::new(
        None,
        vec![media_message(
            rclaw_core::MediaKind::File,
            "https://example.com/doc.pdf?sig=secret",
        )],
    );
    let body = OpenAiCompat::new()
        .build_request_body(&deepseek_flash_cfg(), &req, false)
        .unwrap();
    let content = body_content(&body).as_array().unwrap();
    assert_eq!(content[1]["type"], "text");
    let text = content[1]["text"].as_str().unwrap();
    assert!(text.contains("https://example.com/doc.pdf"), "{text}");
    assert!(!text.contains("sig=secret"), "{text}");
}

#[test]
fn deepseek_video_falls_back_to_text() {
    let req = ChatRequest::new(
        None,
        vec![media_message(
            rclaw_core::MediaKind::Video,
            "https://example.com/v.mp4",
        )],
    );
    let body = OpenAiCompat::new()
        .build_request_body(&deepseek_flash_cfg(), &req, false)
        .unwrap();
    let content = body_content(&body).as_array().unwrap();
    assert_eq!(content[1]["type"], "text");
    assert!(content[1]["text"]
        .as_str()
        .unwrap()
        .contains("does not accept video input"));
}

#[test]
fn zhipu_file_and_video_keep_url_parts() {
    for (kind, expected) in [
        (rclaw_core::MediaKind::File, "file_url"),
        (rclaw_core::MediaKind::Video, "video_url"),
    ] {
        let req = ChatRequest::new(None, vec![media_message(kind, "https://example.com/a")]);
        let body = OpenAiCompat::new()
            .build_request_body(&cfg_multimodal(), &req, false)
            .unwrap();
        let content = body_content(&body).as_array().unwrap();
        assert_eq!(content[1]["type"], expected);
        assert_eq!(content[1][expected]["url"], "https://example.com/a");
    }
}

/// DeepSeek 带 tools 时，参与工具链的 assistant 消息必须有 reasoning_content；
/// 缺失时本地 fail loud（Config），而不是把必然 400 的请求发出去。
#[test]
fn deepseek_tools_require_reasoning_on_tool_chains() {
    let assistant_without_reasoning = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input: json!({"path":"a"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let tool_result = Message {
        id: 0,
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "ok".into(),
            is_error: false,
        }],
        tool_call_id: None,
        ts: 0,
    };
    let mut req = ChatRequest::new(None, vec![assistant_without_reasoning, tool_result.clone()]);
    req.tools.push(ToolSchema {
        name: "read".into(),
        description: "read".into(),
        input_schema: json!({"type":"object"}),
    });
    let error = OpenAiCompat::new()
        .build_request_body(&deepseek_flash_cfg(), &req, false)
        .expect_err("缺失 reasoning 的 DeepSeek 工具链必须本地拒绝");
    assert!(matches!(error, LlmError::Config(_)), "{error:?}");

    // 补上 reasoning 后放行。
    let mut req_ok = req.clone();
    req_ok.messages[0]
        .content
        .push(ContentBlock::reasoning("thinking"));
    OpenAiCompat::new()
        .build_request_body(&deepseek_flash_cfg(), &req_ok, false)
        .expect("带 reasoning 的工具链应放行");

    // 非 DeepSeek 方言没有这条限制。
    OpenAiCompat::new()
        .build_request_body(&cfg_multimodal(), &req, false)
        .expect("非 DeepSeek 方言不应被该规则拦截");
}

/// UpstreamBusy 分类仅对 DeepSeek 方言生效；通用方言同名结束原因仍是协议错误。
#[test]
fn upstream_busy_finish_reason_is_deepseek_only() {
    let body = br#"{"choices":[{"message":{"content":""},"finish_reason":"insufficient_system_resource"}]}"#;
    let error = OpenAiCompat::new()
        .parse_full_response(body, &Dialect::openai_compat(), 4)
        .expect_err("通用方言不应重分类");
    assert!(matches!(error, LlmError::BadResponse(_)), "{error:?}");
}

/// 关闭思考时，工具续轮没有 reasoning 是合法的——不能被 DeepSeek 的“必须回传”
/// 校验误杀（否则关闭思考后第一次工具调用就断链）。
#[test]
fn deepseek_tools_allowed_when_thinking_disabled() {
    let mut cfg = deepseek_flash_cfg();
    cfg.reasoning = crate::config::ReasoningConfig {
        thinking: Some(false),
        effort: None,
    };
    let assistant_without_reasoning = Message {
        id: 0,
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "read".into(),
            input: json!({"path":"a"}),
        }],
        tool_call_id: None,
        ts: 0,
    };
    let mut req = ChatRequest::new(None, vec![assistant_without_reasoning]);
    req.tools.push(ToolSchema {
        name: "read".into(),
        description: "read".into(),
        input_schema: json!({"type":"object"}),
    });
    OpenAiCompat::new()
        .build_request_body(&cfg, &req, false)
        .expect("关闭思考时工具续轮不应要求 reasoning");
}

/// 资源类终帧即使同时带不完整 tool_call 分片，也必须先归类为可重试的上游繁忙，
/// 不能被 delta 解析抢先判成协议错误。
#[test]
fn upstream_busy_finish_wins_over_bad_delta() {
    let mut p = OpenAiCompat::new().make_stream_parser(Dialect::deepseek(), 4);
    let err = p
        .feed(
            r#"{"choices":[{"delta":{"tool_calls":[{"function":{"name":"read"}}]},"finish_reason":"insufficient_system_resource"}]}"#,
        )
        .expect_err("资源类终帧应优先归类");
    assert!(matches!(err, LlmError::UpstreamBusy(_)), "{err:?}");
    assert!(err.is_retryable());
}

/// File 的 data URI 必须严格校验；非法 base64 不能被当成 file_data 发出去。
#[test]
fn deepseek_invalid_base64_file_is_omitted() {
    for bad in [
        "data:application/pdf;base64,!!!!",
        "data:application/pdf;base64,QUJ", // 长度不是 4 的倍数
        "data:no-mime;base64,QUJD",        // MIME 缺 /
        "data:application/pdf,QUJD",       // 缺 ;base64
    ] {
        assert!(
            matches!(parse_data_uri(bad), DataUriPayload::Invalid),
            "{bad}"
        );
        let req = ChatRequest::new(None, vec![media_message(rclaw_core::MediaKind::File, bad)]);
        let body = OpenAiCompat::new()
            .build_request_body(&deepseek_flash_cfg(), &req, false)
            .unwrap();
        let content = body_content(&body).as_array().unwrap();
        assert_eq!(content[1]["type"], "text", "{bad}");
        assert!(
            content[1]["text"]
                .as_str()
                .unwrap()
                .contains("invalid data URI"),
            "{bad}"
        );
    }
}

/// 纯文本模型收到附件时必须显式告诉模型“附件被省略”，不能让它在没有附件的情况下
/// 继续回答。
#[test]
fn text_only_model_reports_attachment_omitted() {
    let mut cfg = cfg(); // deepseek-v4-pro：纯文本
    cfg.default_model = "deepseek-v4-pro".into();
    let req = ChatRequest::new(
        None,
        vec![media_message(
            rclaw_core::MediaKind::Image,
            "https://example.com/a.png",
        )],
    );
    let body = OpenAiCompat::new()
        .build_request_body(&cfg, &req, false)
        .unwrap();
    let content = body["messages"][0]["content"].as_str().unwrap();
    assert!(content.contains("attachment(s) omitted"), "{content}");
    assert!(content.contains("text-only"), "{content}");
}

#[test]
fn redact_secrets_removes_keys_and_bearer_tokens() {
    let text = r#"bad key sk-abc123DEF456 and header "Authorization: Bearer tok_987xyz""#;
    let redacted = crate::error::redact_secrets(text);
    assert!(!redacted.contains("sk-abc123DEF456"), "{redacted}");
    assert!(!redacted.contains("tok_987xyz"), "{redacted}");
    assert!(redacted.contains("<redacted>"), "{redacted}");
}
