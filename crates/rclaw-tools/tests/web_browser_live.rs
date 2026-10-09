//! 可选的真实无头浏览器冒烟测试（默认忽略，人工显式运行）。
//! 走桌面后端完整链路：探测浏览器 → spawn `--headless --dump-dom` → HTML 解析。

use rclaw_tools::web_browser::{build_search_url, HeadlessBrowserLoader, PageLoader};

#[test]
#[ignore = "需要本机无头浏览器与外网，产生真实搜索流量"]
fn headless_browser_search_smoke() {
    let loader = HeadlessBrowserLoader;
    let url = build_search_url("bing", "八卦图 起源").unwrap();
    let page = loader.load(&url).expect("必应检索加载失败");
    assert!(!page.title.is_empty(), "应有页面标题");
    assert!(
        !page.results.is_empty(),
        "应解析出搜索结果（title={}）",
        page.title
    );
    let first = &page.results[0];
    assert!(first.url.starts_with("http"), "首条结果应有链接: {first:?}");
    assert!(!first.title.is_empty());
}

#[test]
#[ignore = "需要本机无头浏览器与外网，产生真实页面流量"]
fn headless_browser_open_article_smoke() {
    let loader = HeadlessBrowserLoader;
    let page = loader
        .load("https://baike.baidu.com/item/%E5%85%AB%E5%8D%A6/166475")
        .expect("百科页面加载失败");
    // 反爬拦截时 title 是「百度安全验证」——以真实词条标题判定拿到正文页。
    assert!(
        page.title.contains("百度百科"),
        "应拿到真实词条页（title={}）",
        page.title
    );
    assert!(!page.text.is_empty(), "应有正文");
    assert!(page.text.contains("八卦"), "正文应包含主题词");
}
