//! `web_browser` 工具：用真实浏览器引擎检索/读取网页。
//!
//! 与 `web_search`（LLM 搜索 API）互补：本工具走真实浏览器（JS 渲染、
//! 真实 UA），适合 API 未配置、页面动态渲染、或已知链接要读正文的场景。
//!
//! 取页内核可插拔（[`PageLoader`]）：
//! - 桌面 / CLI / Termux：[`HeadlessBrowserLoader`]——spawn 系统
//!   Chrome / Edge / Chromium 的 `--headless --dump-dom`（Windows 10+
//!   自带 Edge，无需额外安装）；
//! - Android APK 宿主：rclaw-android 注入系统 WebView 离屏渲染后端
//!   （`assemble_core_with` 注册的默认后端在 APK 装配钩子里被替换）。
//!
//! 参数校验、URL 构建、结果格式化与 HTML 解析均为纯逻辑，宿主无关。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Tool, ToolResult};

/// 单次取页的外层兜底超时（各后端内部已有 30s 级超时，这里防卡死）。
const OUTER_TIMEOUT: Duration = Duration::from_secs(45);
/// 后端加载超时（进程 spawn / WebView 加载）。
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// 单次检索返回的结果条数上限。
const MAX_RESULTS: usize = 8;
/// open 动作返回正文的字符上限（保上下文预算）。
const PAGE_TEXT_MAX_CHARS: usize = 6_000;
/// 检索词字符上限（超过直接拒绝，避免拼出超长 URL）。
const QUERY_MAX_CHARS: usize = 400;
/// headless 浏览器 DOM 输出的读取上限（防异常页撑爆内存）。
const DUMP_MAX_BYTES: usize = 12 * 1024 * 1024;

/// 页面提取结果（与 Android 侧 HeadlessBrowser 的提取 JS 字段一一对应）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PagePayload {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub results: Vec<SearchEntry>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SearchEntry {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub snippet: String,
}

/// 取页内核：加载 url 并返回页面提取结果。
/// 实现必须是阻塞式的（调用方放入 `spawn_blocking`）。
pub trait PageLoader: Send + Sync {
    fn load(&self, url: &str) -> Result<PagePayload, String>;
}

pub struct WebBrowserTool {
    loader: Arc<dyn PageLoader>,
}

impl WebBrowserTool {
    pub fn new(loader: Arc<dyn PageLoader>) -> Self {
        WebBrowserTool { loader }
    }
}

#[async_trait]
impl Tool for WebBrowserTool {
    fn name(&self) -> &str {
        "web_browser"
    }

    /// 后端自持浏览器实例/进程且按调用销毁，保守不并发。
    fn parallel_safe(&self) -> bool {
        false
    }

    fn description(&self) -> &str {
        "用真实浏览器引擎（无头浏览器）检索或打开网页。action=search 传入检索词\
         返回结果列表（标题/链接/摘要；默认必应，搜不到自动换百度重试一次，\
         engine 可显式指定 bing/baidu，已过滤广告条目）；action=open 传入 URL\
         返回页面标题与正文文本。适合 web_search 未配置、页面需要 JS 渲染、或要读取某个\
         具体链接内容的场景。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["search", "open"],
                    "description": "search=搜索引擎检索；open=打开指定 URL 读正文"
                },
                "query": {
                    "type": "string",
                    "description": "search 动作的检索词"
                },
                "url": {
                    "type": "string",
                    "description": "open 动作的目标链接（http/https）"
                },
                "engine": {
                    "type": "string",
                    "enum": ["bing", "baidu"],
                    "description": "search 动作的搜索引擎；缺省 bing，且空结果时自动换另一家重试一次（显式指定时不重试）"
                }
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let action = input.get("action").and_then(Value::as_str).unwrap_or("");
        let explicit_engine = input.get("engine").and_then(Value::as_str);
        let engine = explicit_engine.unwrap_or("bing");
        match action {
            "search" => {
                let Some(query) = input
                    .get("query")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|q| !q.is_empty())
                else {
                    return ToolResult::err("web_browser search 需要非空 query".to_string());
                };
                if query.chars().count() > QUERY_MAX_CHARS {
                    return ToolResult::err(format!(
                        "query 过长（{} 字符 > {QUERY_MAX_CHARS}）",
                        query.chars().count()
                    ));
                }
                let Some(url) = build_search_url(engine, query) else {
                    return ToolResult::err(format!(
                        "未知搜索引擎 {engine}（支持 bing / baidu）"
                    ));
                };
                let mut payload = match self.fetch(url).await {
                    Ok(payload) => payload,
                    Err(error) => return ToolResult::err(error),
                };
                let mut used_engine = engine;
                // 缺省（未显式指定引擎）时必应空结果 → 自动换百度重试一次。
                // 显式指定不重试：调用方明确要那家引擎的结果。
                if explicit_engine.is_none()
                    && engine == "bing"
                    && payload.results.is_empty()
                {
                    if let Ok(baidu_payload) =
                        self.fetch(build_search_url("baidu", query).unwrap_or_default())
                            .await
                    {
                        if !baidu_payload.results.is_empty() {
                            payload = baidu_payload;
                            used_engine = "baidu";
                        }
                    }
                }
                format_search_page(&payload, used_engine, query)
            }
            "open" => {
                let Some(url) = input
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|u| !u.is_empty())
                else {
                    return ToolResult::err("web_browser open 需要非空 url".to_string());
                };
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return ToolResult::err("url 必须以 http:// 或 https:// 开头".to_string());
                }
                match self.fetch(url.to_string()).await {
                    Ok(payload) => format_article_page(&payload),
                    Err(error) => ToolResult::err(error),
                }
            }
            _ => ToolResult::err(format!(
                "未知 action {action:?}（支持 search / open）"
            )),
        }
    }
}

impl WebBrowserTool {
    /// 走后端取一页（阻塞放 spawn_blocking，外层超时兜底）。
    /// 桌面后端（dump-dom）拿不到最终 URL：回填请求 URL 供输出展示；
    /// Android WebView 后端会带真实 final URL，此处不动。
    async fn fetch(&self, url: String) -> Result<PagePayload, String> {
        let requested = url.clone();
        let loader = self.loader.clone();
        let job = tokio::task::spawn_blocking(move || loader.load(&url));
        let mut payload = match tokio::time::timeout(OUTER_TIMEOUT, job).await {
            Ok(Ok(Ok(payload))) => payload,
            Ok(Ok(Err(error))) => return Err(error),
            Ok(Err(error)) => return Err(format!("浏览器任务异常: {error}")),
            Err(_) => {
                return Err(format!(
                    "无头浏览器超时（>{OUTER_TIMEOUT:?}）；页面可能加载过慢，可稍后重试"
                ))
            }
        };
        if payload.url.is_empty() {
            payload.url = requested;
        }
        Ok(payload)
    }
}

/// 拼搜索引擎 URL。未知引擎返回 None。
pub fn build_search_url(engine: &str, query: &str) -> Option<String> {
    let (base, key) = match engine {
        "bing" | "" => ("https://www.bing.com/search", "q"),
        "baidu" => ("https://www.baidu.com/s", "wd"),
        _ => return None,
    };
    reqwest::Url::parse_with_params(base, &[(key, query)])
        .ok()
        .map(|url| url.to_string())
}

/// search 动作输出：结果列表优先，解析不到结果时退回页面正文摘要。
/// 头部展示原始检索词（SERP 页标题是引擎拼的，不适合当检索词展示）。
fn format_search_page(page: &PagePayload, engine: &str, query: &str) -> ToolResult {
    let engine_label = match engine {
        "baidu" => "百度",
        _ => "必应",
    };
    let mut out = String::new();
    if !page.results.is_empty() {
        out.push_str(&format!(
            "{}检索「{}」{} 条结果：\n",
            engine_label,
            single_line(query),
            page.results.len().min(MAX_RESULTS)
        ));
        for (index, entry) in page.results.iter().take(MAX_RESULTS).enumerate() {
            out.push_str(&format!("\n[{}] {}\n", index + 1, single_line(&entry.title)));
            if !entry.url.is_empty() {
                out.push_str(&format!("URL: {}\n", single_line(&entry.url)));
            }
            if !entry.snippet.is_empty() {
                out.push_str(&format!("{}\n", single_line(&entry.snippet)));
            }
        }
        out.push_str("\n（需要正文时用 action=open 打开对应 URL）");
        ToolResult::ok(out)
    } else {
        // 引擎改版 / 反爬页可能解析不出结果：退回正文摘要，把页面实际
        // 呈现的内容交回模型判断（如验证码页则如实可见）。
        let text = truncate_chars(&page.text, PAGE_TEXT_MAX_CHARS);
        if text.is_empty() {
            return ToolResult::err(format!(
                "检索页没有解析出结果，正文也为空（页面可能被拦截或加载失败）：{}",
                page.url
            ));
        }
        ToolResult::ok(format!(
            "{}检索页未解析出结果列表，以下是页面正文摘要（{}）：\n{}",
            engine_label, page.url, text
        ))
    }
}

/// open 动作输出：标题 + 最终 URL + 截断正文。
fn format_article_page(page: &PagePayload) -> ToolResult {
    if page.text.trim().is_empty() {
        return ToolResult::err(format!(
            "页面没有可读正文（可能是空页/纯媒体页/被拦截）：{}",
            page.url
        ));
    }
    let text = truncate_chars(&page.text, PAGE_TEXT_MAX_CHARS);
    let mut out = String::new();
    if !page.title.is_empty() {
        out.push_str(&format!("标题：{}\n", single_line(&page.title)));
    }
    if !page.url.is_empty() {
        out.push_str(&format!("URL：{}\n", page.url));
    }
    out.push_str(&format!("\n{text}"));
    ToolResult::ok(out)
}

/// 单行化：进度/结果行不允许内嵌换行破坏格式。
fn single_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// 按字符截断（CJK 安全），超长补省略说明。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let mut truncated: String = text.chars().take(max_chars).collect();
    truncated.push_str(&format!("\n\n（正文共 {count} 字符，已截断至 {max_chars}）"));
    truncated
}

// ---------------------------------------------------------------------------
// 桌面后端：spawn 系统 Chrome/Edge/Chromium 的 --headless --dump-dom。
// ---------------------------------------------------------------------------

/// headless 浏览器进程后端（桌面 / CLI / Termux 共用）。
pub struct HeadlessBrowserLoader;

impl PageLoader for HeadlessBrowserLoader {
    fn load(&self, url: &str) -> Result<PagePayload, String> {
        let browser = detect_browser().ok_or_else(|| {
            "未找到可用的无头浏览器（按序探测 Chrome/Chromium/Edge；\
             可用环境变量 RCLAW_HEADLESS_BROWSER 指定可执行文件路径）"
                .to_string()
        })?;
        let html = dump_dom(&browser, url)?;
        Ok(html::parse(&html))
    }
}

/// 探测本机 Chromium 系浏览器：环境变量覆盖 > 常见安装路径 / PATH。
fn detect_browser() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RCLAW_HEADLESS_BROWSER") {
        if !path.trim().is_empty() {
            return Some(PathBuf::from(path));
        }
    }
    #[cfg(windows)]
    {
        let mut candidates: Vec<PathBuf> = [
            r"C:\Program Files\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
            r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
            r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            candidates.push(PathBuf::from(&local).join(r"Google\Chrome\Application\chrome.exe"));
            candidates.push(PathBuf::from(&local).join(r"Microsoft\Edge\Application\msedge.exe"));
        }
        candidates.into_iter().find(|p| p.is_file())
    }
    #[cfg(not(windows))]
    {
        ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "chrome"]
            .iter()
            .find_map(|name| which(name))
    }
}

/// PATH 查找可执行文件（Unix）。
#[cfg(not(windows))]
fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// `browser --headless --dump-dom <url>`：虚拟时间预算让页面 JS 跑完再落 DOM。
/// 独立 user-data-dir：绕开浏览器单例锁（用户开着 Edge/Chrome 时新进程只会
/// 把 URL 转发给已运行实例然后退出，DOM 拿不到）；每次调用独立目录，
/// 并发调用互不抢锁，用完即删。
fn dump_dom(browser: &PathBuf, url: &str) -> Result<String, String> {
    use std::io::Read;
    let profile = std::env::temp_dir().join(format!(
        "rclaw-headless-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
    ));
    let mut child = std::process::Command::new(browser)
        .arg("--headless=new")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg("--virtual-time-budget=6000")
        .arg("--dump-dom")
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|error| format!("启动无头浏览器失败（{}）: {error}", browser.display()))?;
    let stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut data = Vec::new();
        let Some(mut stdout) = stdout else {
            return data;
        };
        let mut chunk = [0u8; 65536];
        loop {
            match stdout.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    data.extend_from_slice(&chunk[..n]);
                    if data.len() > DUMP_MAX_BYTES {
                        break;
                    }
                }
            }
        }
        data
    });
    let deadline = Instant::now() + LOAD_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => break,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&profile);
                    return Err(format!(
                        "无头浏览器加载超时（{}s）：{url}",
                        LOAD_TIMEOUT.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = std::fs::remove_dir_all(&profile);
                return Err(format!("等待无头浏览器退出失败: {error}"));
            }
        }
    }
    let data = reader.join().map_err(|_| "读取 DOM 输出失败".to_string())?;
    // 浏览器退出后再清 profile（运行中删除会留下残余/报错）。
    let _ = std::fs::remove_dir_all(&profile);
    Ok(String::from_utf8_lossy(&data).into_owned())
}

/// 无 DOM 引擎的轻量 HTML 提取（regex + 标签剥离）。
mod html {
    use super::PagePayload;
    use regex::Regex;

    /// 从 dump-dom 的 HTML 提取 title / 搜索结果 / 正文文本。
    pub fn parse(html: &str) -> PagePayload {
        let mut page = PagePayload {
            title: tag_inner(html, &Regex::new(r"(?s)<title[^>]*>(.*?)</title>").unwrap())
                .map(|t| text_from_html(&t))
                .unwrap_or_default(),
            url: String::new(),
            text: text_from_html(html),
            results: Vec::new(),
        };
        if html.contains("b_algo") {
            page.results = parse_bing(html);
        } else if html.contains("content_left") {
            page.results = parse_baidu(html);
        }
        page
    }

    /// 主文区域选择：`<article>` 优先、`<main>` 次之（取首个开标签到最后一个
    /// 闭标签的跨度）；区域太小（<20% 页面，多是页内小部件）时退回整页。
    fn main_region(html: &str) -> &str {
        for name in ["article", "main"] {
            let open = format!("<{name}");
            let close = format!("</{name}");
            if let (Some(start), Some(end)) = (html.find(&open), html.rfind(&close)) {
                let region = &html[start..(end + close.len()).min(html.len())];
                if region.len() * 5 >= html.len() {
                    return region;
                }
            }
        }
        html
    }

    /// 首个捕获组的原文（不剥标签）。
    fn tag_inner(html: &str, re: &Regex) -> Option<String> {
        re.captures(html)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str().to_string())
    }

    /// 必应桌面 SERP：`<li class="b_algo">` 块内 `<h2><a href>` + `b_caption` 摘要。
    fn parse_bing(html: &str) -> Vec<super::SearchEntry> {
        let link = Regex::new(r#"(?s)<h2[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
        let snippet =
            Regex::new(r#"(?s)<div class="b_caption">\s*<p[^>]*>(.*?)</p>"#).unwrap();
        // b_ad/b_adBadge 类名或「赞助商」角标 = 商业推广条目。
        collect_entries(html, "<li class=\"b_algo\"", &link, Some(&snippet), &[
            "class=\"b_ad", "赞助商",
        ])
    }

    /// 百度桌面 SERP：`<div class="result...">` 块内 `<h3><a href>` + `c-abstract` 摘要。
    /// 带商业推广标记（ec-/ec_/tuiguang/熊掌推广角标）的条目直接丢弃。
    fn parse_baidu(html: &str) -> Vec<super::SearchEntry> {
        let link = Regex::new(r#"(?s)<h3[^>]*>\s*<a[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap();
        let snippet = Regex::new(r#"(?s)class="c-abstract[^"]*"[^>]*>(.*?)</"#).unwrap();
        let mut entries = collect_entries(
            html,
            "<div class=\"result",
            &link,
            Some(&snippet),
            &["ec_tuiguang", "tuiguang", "class=\"ec-", "c-icon-bear"],
        );
        entries.retain(|entry| !is_ad_entry(entry));
        entries
    }

    /// 商业推广条目判定：百度推广跳转域（注意 `//e.` 前缀——`baike.baidu.com`
    /// 之类正常域包含子串 `e.baidu.com`，不能裸匹配）/ 推广专用链接形态。
    fn is_ad_entry(entry: &super::SearchEntry) -> bool {
        let url = entry.url.as_str();
        url.contains("//e.baidu.com")
            || url.contains("promotion.baidu.com")
            || url.contains("baidu.com/baidu.php")
    }

    /// 按 `marker` 切块，块内用 link/snippet 正则提取，含广告标记的块整块丢弃，
    /// 按 URL 去重，至多 10 条。
    fn collect_entries(
        html: &str,
        marker: &str,
        link: &Regex,
        snippet: Option<&Regex>,
        ad_markers: &[&str],
    ) -> Vec<super::SearchEntry> {
        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for chunk in html.split(marker).skip(1) {
            if entries.len() >= 10 {
                break;
            }
            // 广告块（引擎商业推广容器）整块跳过。
            if ad_markers.iter().any(|marker| chunk.contains(marker)) {
                continue;
            }
            let Some(caps) = link.captures(chunk) else {
                continue;
            };
            let url = caps.get(1).map(|m| m.as_str()).unwrap_or("").to_string();
            if url.is_empty() || url.starts_with("javascript:") || !seen.insert(url.clone()) {
                continue;
            }
            let title = caps
                .get(2)
                .map(|m| text_from_html(m.as_str()))
                .unwrap_or_default();
            let snippet_text = snippet
                .and_then(|re| tag_inner(chunk, re))
                .map(|s| text_from_html(&s))
                .unwrap_or_default();
            entries.push(super::SearchEntry {
                title,
                url,
                snippet: snippet_text,
            });
        }
        entries
    }

    /// HTML → 可读文本（Readability 式启发式的 regex 近似）：
    /// 1) 优先取 `<article>` / `<main>` 主文区域（存在且足够大时）；
    /// 2) 剔除 head/script/style/注释与 nav/header/footer/aside/form 样板容器；
    /// 3) 剥标签、块级转换行、实体解码、空白归一。
    pub fn text_from_html(html: &str) -> String {
        let region = main_region(html);
        let script =
            Regex::new(r"(?is)<(script|style|head|noscript)\b[^>]*>.*?</(script|style|head|noscript)\s*>").unwrap();
        // 样板容器成对剔除（同名容器几乎不嵌套，交叉误配对概率极低）。
        let boilerplate = Regex::new(r"(?is)<(nav|footer|aside|form)\b[^>]*>.*?</(nav|footer|aside|form)\s*>").unwrap();
        let comment = Regex::new(r"(?s)<!--.*?-->").unwrap();
        let tag = Regex::new(r"(?s)<[^>]+>").unwrap();
        let cleaned = script.replace_all(region, " ");
        let cleaned = boilerplate.replace_all(&cleaned, " ");
        let cleaned = comment.replace_all(&cleaned, " ");
        // 块级标签转换行，避免标题与正文粘连。
        let block = Regex::new(r"(?i)</?(p|div|br|li|h[1-6]|tr|section|article)\b[^>]*>").unwrap();
        let with_breaks = block.replace_all(&cleaned, "\n");
        let no_tags = tag.replace_all(&with_breaks, " ");
        let decoded = decode_entities(&no_tags);
        let mut out = String::with_capacity(decoded.len());
        let mut pending_space = false;
        for line in decoded.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                pending_space = true;
                continue;
            }
            if !out.is_empty() && pending_space {
                out.push('\n');
            }
            // 行内多余空白折叠为单个空格。
            let mut collapsed = String::with_capacity(trimmed.len());
            let mut last_space = false;
            for word in trimmed.split_whitespace() {
                if last_space {
                    collapsed.push(' ');
                }
                collapsed.push_str(word);
                last_space = true;
            }
            out.push_str(&collapsed);
            pending_space = false;
        }
        out.trim().to_string()
    }

    /// 常用 HTML 实体 + 数字实体解码。
    fn decode_entities(text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(start) = rest.find('&') {
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            let Some(end) = rest.find(';').filter(|&e| e <= start + 10) else {
                out.push('&');
                rest = &rest[1..];
                continue;
            };
            let entity = &rest[1..end];
            let decoded = match entity {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                _ => {
                    if let Some(hex) = entity.strip_prefix("#x").or_else(|| entity.strip_prefix("#X"))
                    {
                        u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
                    } else {
                        entity
                            .strip_prefix('#')
                            .and_then(|dec| dec.parse::<u32>().ok())
                            .and_then(char::from_u32)
                    }
                }
            };
            match decoded {
                Some(ch) => {
                    out.push(ch);
                    rest = &rest[end + 1..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            }
        }
        out.push_str(rest);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> WebBrowserTool {
        // 校验/格式化路径不触后端；load 不会被这些用例走到。
        struct NoopLoader;
        impl PageLoader for NoopLoader {
            fn load(&self, _url: &str) -> Result<PagePayload, String> {
                Err("测试后端不可用".to_string())
            }
        }
        WebBrowserTool::new(Arc::new(NoopLoader))
    }

    /// 可编程假后端：按序吐出预设结果，并记录每次请求的 URL。
    struct FakeLoader {
        replies: std::sync::Mutex<std::collections::VecDeque<Result<PagePayload, String>>>,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl FakeLoader {
        fn new(replies: Vec<Result<PagePayload, String>>) -> Arc<Self> {
            Arc::new(FakeLoader {
                replies: std::sync::Mutex::new(replies.into()),
                calls: std::sync::Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl PageLoader for FakeLoader {
        fn load(&self, url: &str) -> Result<PagePayload, String> {
            self.calls.lock().unwrap().push(url.to_string());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(PagePayload::default()))
        }
    }

    fn page_with_results(engine_title: &str, urls: &[&str]) -> PagePayload {
        PagePayload {
            title: engine_title.to_string(),
            url: format!("https://{engine_title}/serp"),
            text: format!("{engine_title} 正文兜底"),
            results: urls
                .iter()
                .map(|u| SearchEntry {
                    title: format!("标题-{u}"),
                    url: u.to_string(),
                    snippet: format!("摘要-{u}"),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn search_falls_back_to_baidu_when_bing_empty() {
        let loader = FakeLoader::new(vec![
            Ok(PagePayload {
                title: "必应空页".into(),
                url: "https://www.bing.com/search?q=x".into(),
                text: "必应拦截页正文".into(),
                results: vec![],
            }),
            Ok(page_with_results("baidu", &["https://baike.baidu.com/item/x"])),
        ]);
        let tool = WebBrowserTool::new(loader.clone());
        let result = tool
            .execute(json!({"action": "search", "query": "测试词"}))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("百度检索"), "{}", result.content);
        assert!(result.content.contains("https://baike.baidu.com/item/x"));
        let calls = loader.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].contains("bing.com"));
        assert!(calls[1].contains("baidu.com"));
    }

    #[tokio::test]
    async fn explicit_engine_does_not_fallback() {
        let loader = FakeLoader::new(vec![Ok(PagePayload {
            title: "必应空页".into(),
            url: "https://www.bing.com/search?q=x".into(),
            text: "必应拦截页正文".into(),
            results: vec![],
        })]);
        let tool = WebBrowserTool::new(loader.clone());
        let result = tool
            .execute(json!({"action": "search", "query": "x", "engine": "bing"}))
            .await;
        assert!(!result.is_error);
        // 显式 bing：空结果不重试，走正文兜底文案。
        assert!(result.content.contains("未解析出结果"), "{}", result.content);
        assert_eq!(loader.calls().len(), 1);
    }

    #[tokio::test]
    async fn baidu_results_are_used_directly_when_requested() {
        let loader = FakeLoader::new(vec![Ok(page_with_results("baidu-serp", &[
            "https://zhuanlan.zhihu.com/p/1",
        ]))]);
        let tool = WebBrowserTool::new(loader.clone());
        let result = tool
            .execute(json!({"action": "search", "query": "x", "engine": "baidu"}))
            .await;
        assert!(!result.is_error);
        assert!(result.content.contains("百度检索"));
        assert_eq!(loader.calls().len(), 1);
        assert!(loader.calls()[0].contains("baidu.com"));
    }

    #[tokio::test]
    async fn search_requires_non_empty_query() {
        let result = tool()
            .execute(json!({"action": "search", "query": "  "}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("非空 query"));
    }

    #[tokio::test]
    async fn search_rejects_unknown_engine() {
        let result = tool()
            .execute(json!({"action": "search", "query": "x", "engine": "google"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("未知搜索引擎"));
    }

    #[tokio::test]
    async fn open_requires_http_url() {
        let result = tool()
            .execute(json!({"action": "open", "url": "ftp://example.com"}))
            .await;
        assert!(result.is_error);
        assert!(result.content.contains("http"));
    }

    #[tokio::test]
    async fn open_requires_url() {
        let result = tool().execute(json!({"action": "open"})).await;
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn unknown_action_rejected() {
        let result = tool().execute(json!({"action": "click"})).await;
        assert!(result.is_error);
        assert!(result.content.contains("未知 action"));
    }

    #[test]
    fn search_url_encodes_cjk_and_picks_engine_params() {
        let bing = build_search_url("bing", "八卦图 起源").unwrap();
        assert!(bing.starts_with("https://www.bing.com/search?"));
        assert!(bing.contains("q=%E5%85%AB%E5%8D%A6%E5%9B%BE"));

        let baidu = build_search_url("baidu", "八卦图").unwrap();
        assert!(baidu.starts_with("https://www.baidu.com/s?"));
        assert!(baidu.contains("wd=%E5%85%AB%E5%8D%A6%E5%9B%BE"));

        // 空引擎默认 bing；未知引擎 None。
        assert!(build_search_url("", "x").unwrap().contains("bing.com"));
        assert!(build_search_url("doge", "x").is_none());
    }

    #[test]
    fn search_url_encodes_reserved_characters() {
        // 检索词带 & = ? # 等保留字必须转义，否则会破坏查询串结构。
        let url = build_search_url("bing", "a&b=c?d#e f").unwrap();
        let query = url.strip_prefix("https://www.bing.com/search?").unwrap();
        // url crate 的查询串编码用 + 表示空格（与 %20 等价）。
        assert_eq!(query, "q=a%26b%3Dc%3Fd%23e+f");
    }

    #[test]
    fn search_page_formats_entries_and_caps_count() {
        let page = PagePayload {
            title: "八卦图 - 搜索".into(),
            url: "https://www.bing.com/search?q=x".into(),
            text: "忽略".into(),
            results: (0..12)
                .map(|i| SearchEntry {
                    title: format!("结果{i}"),
                    url: format!("https://example.com/{i}"),
                    snippet: format!("摘要{i}\n第二行"),
                })
                .collect(),
        };
        let result = format_search_page(&page, "bing", "检索词");
        assert!(!result.is_error);
        assert!(result.content.contains("8 条结果"));
        assert!(result.content.contains("[1] 结果0"));
        assert!(result.content.contains("https://example.com/0"));
        // 摘要内换行被单行化
        assert!(result.content.contains("摘要0 第二行"));
        assert!(!result.content.contains("[9]"));
    }

    #[test]
    fn search_page_falls_back_to_text_without_entries() {
        let page = PagePayload {
            title: "t".into(),
            url: "https://u".into(),
            text: "页面正文".into(),
            results: vec![],
        };
        let result = format_search_page(&page, "baidu", "检索词");
        assert!(!result.is_error);
        assert!(result.content.contains("未解析出结果"));
        assert!(result.content.contains("页面正文"));
    }

    #[test]
    fn search_page_errors_when_nothing_usable() {
        let page = PagePayload::default();
        let result = format_search_page(&page, "bing", "检索词");
        assert!(result.is_error);
    }

    #[test]
    fn article_page_truncates_and_labels() {
        let page = PagePayload {
            title: "标题".into(),
            url: "https://u".into(),
            text: "字".repeat(7_000),
            results: vec![],
        };
        let result = format_article_page(&page);
        assert!(!result.is_error);
        assert!(result.content.contains("标题：标题"));
        assert!(result.content.contains("已截断至 6000"));
    }

    #[test]
    fn article_page_errors_on_empty_text() {
        let page = PagePayload {
            title: "t".into(),
            url: "https://u".into(),
            text: "  ".into(),
            results: vec![],
        };
        let result = format_article_page(&page);
        assert!(result.is_error);
    }

    #[test]
    fn page_payload_deserializes_android_bridge_shape() {
        let payload: PagePayload = serde_json::from_str(
            r#"{"title":"t","url":"u","text":"x","results":[{"title":"a","url":"https://b","snippet":"s"}]}"#,
        )
        .unwrap();
        assert_eq!(payload.results.len(), 1);
        assert_eq!(payload.results[0].url, "https://b");
        // 缺字段容忍（Java 侧提取失败时可能只回部分字段）
        let partial: PagePayload = serde_json::from_str(r#"{"title":"t"}"#).unwrap();
        assert!(partial.results.is_empty());
    }

    // ------------------------------------------------------------------
    // HTML 解析：用真实 dump-dom 片段做 fixture（必应桌面 SERP 结构）。
    // ------------------------------------------------------------------

    const BING_FIXTURE: &str = "<html><head><title>八卦图 起源 - 搜索</title></head><body><ul id=\"b_results\"><li class=\"b_algo\"><h2 class=\"\"><a target=\"_blank\" href=\"https://baike.baidu.com/item/%E5%85%AB%E5%8D%A6/166475\" h=\"ID=SERP,5129.2\"><strong>八卦</strong>（中国古人认识世界时对事物的归类）_百度百科</a></h2><div class=\"b_caption\"><p class=\"b_lineclamp2\">基本含义：八卦以阴、阳二爻为基元。</p></div></li><li class=\"b_algo\"><h2 class=\"\"><a href=\"https://zhuanlan.zhihu.com/p/123\">八卦图的由来</a></h2><div class=\"b_caption\"><p>知乎专栏解读。</p></div></li><li class=\"b_algo\"><h2>无链接块</h2></li></ul><script>var x=1;</script></body></html>";

    #[test]
    fn parse_extracts_bing_results() {
        let page = html::parse(BING_FIXTURE);
        assert_eq!(page.title, "八卦图 起源 - 搜索");
        assert_eq!(page.results.len(), 2);
        assert_eq!(
            page.results[0].url,
            "https://baike.baidu.com/item/%E5%85%AB%E5%8D%A6/166475"
        );
        // <strong> 内联标签剥除（替换处留空格，中文旁可能多一空格，无碍阅读）
        assert!(page.results[0].title.replace(' ', "").contains("八卦（中国古人认识世界时对事物的归类）_百度百科"));
        assert!(page.results[0].snippet.contains("基本含义"));
        assert_eq!(page.results[1].url, "https://zhuanlan.zhihu.com/p/123");
    }

    #[test]
    fn parse_text_strips_script_and_decodes_entities() {
        let page = html::parse(BING_FIXTURE);
        assert!(!page.text.contains("var x=1"));
        assert!(page.text.contains("基本含义：八卦以阴、阳二爻为基元。"));
        let decoded = html::text_from_html("<p>a &amp; b &lt;c&gt; &#20843; &#x5366;</p>");
        assert_eq!(decoded, "a & b <c> 八 卦");
    }

    #[test]
    fn parse_baidu_results() {
        let fixture = "<html><head><title>八卦图_百度搜索</title></head><body><div id=\"content_left\"><div class=\"result c-container new-pmd\"><h3 class=\"t\"><a href=\"https://baike.baidu.com/item/八卦图\">八卦图_百度百科</a></h3><div class=\"c-abstract\">八卦源自《周易》。</div></div><div class=\"result c-container\"><h3 class=\"t\"><a href=\"https://zhidao.baidu.com/q/1\">八卦图的起源</a></h3></div></div></body></html>";
        let page = html::parse(fixture);
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].url, "https://baike.baidu.com/item/八卦图");
        assert!(page.results[0].snippet.contains("周易"));
        assert_eq!(page.results[1].title, "八卦图的起源");
    }

    #[test]
    fn parse_non_ser_page_keeps_text_only() {
        let fixture = "<html><head><title>文章页</title></head><body><p>第一段</p><p>第二段</p></body></html>";
        let page = html::parse(fixture);
        assert!(page.results.is_empty());
        assert_eq!(page.text, "第一段\n第二段");
    }

    #[test]
    fn article_text_drops_boilerplate_and_nav_noise() {
        // Readability 式近似：<article> 区域优先，nav/footer/aside 样板剔除。
        let fixture = "<html><head><title>新闻</title></head><body>\
<nav>首页 新闻 体育 财经 登录 注册</nav>\
<article><h1>标题党不重要</h1><p>正文第一段，讲清楚来龙去脉。</p><p>正文第二段，给出结论。</p></article>\
<footer>关于我们 联系方式 版权声明 友情链接</footer>\
<aside>相关推荐：另一篇文章</aside></body></html>";
        let page = html::parse(fixture);
        assert!(page.text.contains("正文第一段"));
        assert!(page.text.contains("正文第二段"));
        assert!(!page.text.contains("登录 注册"), "{}", page.text);
        assert!(!page.text.contains("版权声明"), "{}", page.text);
        assert!(!page.text.contains("相关推荐"), "{}", page.text);
    }

    #[test]
    fn parse_bing_drops_ad_entries() {
        // 必应商业推广：b_ad 类名或「赞助商」角标条目丢弃。
        let fixture = r#"<html><head><title>t - 搜索</title></head><body><ul id="b_results"><li class="b_algo"><h2><a href="https://ad.example.com/buy">赞助内容标题</a></h2><div class="b_adBadge">赞助商</div></li><li class="b_algo"><h2><a href="https://organic.example.com/1">自然结果一</a></h2><div class="b_caption"><p>真实摘要</p></div></li><li class="b_algo"><h2><a href="https://ad2.example.com/x">推广二</a></h2><span class="b_ad">Ad</span></li></ul></body></html>"#;
        let page = html::parse(fixture);
        let urls: Vec<&str> = page.results.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls, vec!["https://organic.example.com/1"], "{urls:?}");
    }

    #[test]
    fn parse_baidu_drops_ad_entries() {
        let fixture = "<html><head><title>测试_百度搜索</title></head><body><div id=\"content_left\">\
<div class=\"result c-container\"><div class=\"ec_tuiguang\"><h3 class=\"t\"><a href=\"https://e.baidu.com/x\">推广条目标题</a></h3></div></div>\
<div class=\"result c-container\"><h3 class=\"t\"><a href=\"https://www.baidu.com/link?url=AD1\">广告二</a></h3><div class=\"c-abstract\">买它</div></div>\
<div class=\"result c-container\"><h3 class=\"t\"><a href=\"https://www.baidu.com/link?url=ORGANIC1\">自然结果一</a></h3><div class=\"c-abstract\">真实摘要</div></div>\
</div></body></html>";
        let page = html::parse(fixture);
        let urls: Vec<&str> = page.results.iter().map(|e| e.url.as_str()).collect();
        assert!(urls.contains(&"https://www.baidu.com/link?url=ORGANIC1"), "{urls:?}");
        assert!(!page.results.iter().any(|e| e.title.contains("推广条目")), "{urls:?}");
    }

    #[test]
    fn env_override_wins_in_detection() {
        // 只验证 env 覆盖路径优先；不依赖机器上真实浏览器。
        //（值指向不存在的文件也照样返回——覆盖即信任。）
        std::env::set_var("RCLAW_HEADLESS_BROWSER", "Z:/nowhere/fake-browser.exe");
        let detected = detect_browser();
        std::env::remove_var("RCLAW_HEADLESS_BROWSER");
        assert_eq!(
            detected.unwrap(),
            PathBuf::from("Z:/nowhere/fake-browser.exe")
        );
    }
}
