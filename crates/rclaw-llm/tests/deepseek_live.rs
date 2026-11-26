//! 可选的真实 DeepSeek 对话冒烟（默认忽略，需要 `RCLAW_TEST_DEEPSEEK_KEY`）。
//!
//! 验证最终构造的 provider（deepseek-flash + 关闭思考）在真实端点上完成
//! 非流式与流式对话，且密钥不会写入源码或输出。

use rclaw_core::{ChatRequest, Message, Role};
use rclaw_llm::{Provider, ProviderConfig, ReasoningConfig, Result, SwappableProvider};
use secrecy::SecretString;

fn provider() -> SwappableProvider {
    let key = std::env::var("RCLAW_TEST_DEEPSEEK_KEY")
        .expect("缺少真实 API 测试环境变量 RCLAW_TEST_DEEPSEEK_KEY");
    let mut cfg = ProviderConfig::deepseek(SecretString::from(key));
    cfg.default_model = "deepseek-flash".into();
    cfg.max_tokens = 64;
    cfg.reasoning = ReasoningConfig {
        thinking: Some(false),
        effort: None,
    };
    SwappableProvider::new(cfg).unwrap()
}

fn request(stream: bool) -> ChatRequest {
    let mut request = ChatRequest::new(None, vec![Message::plain(Role::User, "只回复：pong")]);
    request.max_tokens = 64;
    request.stream = stream;
    request
}

#[tokio::test]
#[ignore = "需要真实 DeepSeek Key，会产生对话用量计费"]
async fn deepseek_flash_chat_smoke() -> Result<()> {
    let provider = provider();
    let outcome = provider.chat(request(false)).await?;
    assert!(
        outcome.text.to_ascii_lowercase().contains("pong"),
        "unexpected reply: {:?}",
        outcome.text
    );
    eprintln!(
        "deepseek chat: ok ({} chars, stop={:?})",
        outcome.text.chars().count(),
        outcome.stop
    );
    Ok(())
}

#[tokio::test]
#[ignore = "需要真实 DeepSeek Key，会产生对话用量计费"]
async fn deepseek_flash_stream_smoke() -> Result<()> {
    let provider = provider();
    let outcome = provider
        .chat_stream(request(true), std::sync::Arc::new(|_| {}))
        .await?;
    assert!(
        outcome.text.to_ascii_lowercase().contains("pong"),
        "unexpected reply: {:?}",
        outcome.text
    );
    eprintln!(
        "deepseek stream: ok ({} chars, stop={:?})",
        outcome.text.chars().count(),
        outcome.stop
    );
    Ok(())
}
