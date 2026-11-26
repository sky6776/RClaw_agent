//! 可选的真实 API 冒烟测试。
//!
//! 默认忽略，只有人工显式提供环境变量时才运行；密钥不会写入源码或测试输出。

use rclaw_tools::{
    DeepSeekBackend, DeepSeekSearchConfig, DoubaoBackend, DoubaoSearchConfig, DoubaoVariant,
    ReaderBackend, SearchBackend, SearchRequest, TavilyClient, TavilySearchConfig, ZhipuBackend,
    ZhipuSearchConfig,
};

fn required_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("缺少真实 API 测试环境变量 {name}"))
}

fn request() -> SearchRequest {
    SearchRequest {
        query: "北京今日天气".into(),
        max_results: 3,
        need_content: false,
    }
}

#[tokio::test]
#[ignore = "需要真实 API Key，会产生外部搜索调用"]
async fn configured_search_providers_smoke() {
    let custom = DoubaoBackend::new(DoubaoSearchConfig {
        variant: DoubaoVariant::Custom,
        api_key: required_env("RCLAW_TEST_DOUBAO_CUSTOM_KEY"),
        endpoint: String::new(),
        timeout_secs: 20,
        max_results: 10,
        max_snippet_tokens: 500,
    })
    .unwrap();
    let custom_response = custom.search(&request()).await.unwrap();
    assert!(custom_response.is_usable());
    eprintln!("doubao-custom: ok ({} hits)", custom_response.hits.len());

    let global = DoubaoBackend::new(DoubaoSearchConfig {
        variant: DoubaoVariant::Global,
        api_key: required_env("RCLAW_TEST_DOUBAO_GLOBAL_KEY"),
        endpoint: String::new(),
        timeout_secs: 20,
        max_results: 10,
        max_snippet_tokens: 500,
    })
    .unwrap();
    let global_response = global.search(&request()).await.unwrap();
    assert!(global_response.is_usable());
    eprintln!("doubao-global: ok ({} hits)", global_response.hits.len());

    let tavily = TavilyClient::new(TavilySearchConfig {
        api_keys: required_env("RCLAW_TEST_TAVILY_KEYS")
            .split(',')
            .map(str::to_string)
            .collect(),
        base_url: String::new(),
        timeout_secs: 20,
        max_results: 5,
        search_depth: "basic".into(),
        include_answer: false,
    })
    .unwrap();
    let tavily_response = tavily.search(&request()).await.unwrap();
    assert!(tavily_response.is_usable());
    eprintln!("tavily: ok ({} hits)", tavily_response.hits.len());

    // 智谱额度可能已耗尽：仍发起一次真实请求以验证握手和错误分类，但不把额度错误
    // 作为其它三个后端冒烟失败的原因。
    let zhipu = ZhipuBackend::new(ZhipuSearchConfig {
        api_key: required_env("RCLAW_TEST_ZHIPU_KEY"),
        search_url: "https://open.bigmodel.cn/api/mcp/web_search_prime/mcp".into(),
        reader_url: "https://open.bigmodel.cn/api/mcp/web_reader/mcp".into(),
        startup_timeout_secs: 20,
        call_timeout_secs: 30,
    })
    .unwrap();
    match zhipu.search(&request()).await {
        Ok(response) => {
            assert!(response.is_usable());
            eprintln!("zhipu: ok ({} hits)", response.hits.len());
        }
        Err(error)
            if matches!(
                error.kind,
                rclaw_tools::BackendErrorKind::QuotaExhausted
                    | rclaw_tools::BackendErrorKind::RateLimited
            ) =>
        {
            eprintln!("zhipu: expected-possible failure ({:?})", error.kind);
        }
        Err(error) => panic!("zhipu smoke failed unexpectedly: {:?}", error.kind),
    }
}

#[tokio::test]
#[ignore = "需要真实 DeepSeek Key，会产生搜索/抓取用量计费"]
async fn deepseek_search_and_fetch_smoke() {
    let backend = DeepSeekBackend::new(DeepSeekSearchConfig {
        api_key: required_env("RCLAW_TEST_DEEPSEEK_KEY"),
        base_url: String::new(),
        timeout_secs: 30,
        max_results: 5,
        // 一次搜索直接带正文，顺带验证 need_content 路径。
        need_content: true,
    })
    .unwrap();
    let request = SearchRequest {
        query: "DeepSeek V4.1 Flash API".into(),
        max_results: 3,
        need_content: true,
    };
    let response = backend.search(&request).await.unwrap();
    assert!(response.is_usable());
    assert!(!response.hits.is_empty());
    eprintln!("deepseek search: ok ({} hits)", response.hits.len());

    // 取真实结果 URL 走 fetch；若命中内容已带正文，仍验证一次独立抓取。
    let url = response
        .hits
        .iter()
        .find_map(|hit| hit.url.clone())
        .expect("search hits should carry a url");
    let read = backend.read(&url).await.unwrap();
    assert!(!read.content.trim().is_empty());
    eprintln!(
        "deepseek fetch: ok ({} bytes)",
        read.content.chars().count()
    );
}
