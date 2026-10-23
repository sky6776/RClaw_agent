//! download_media：把远程图片/视频/音频下载进工作区媒体沙箱（`media/download/`）。
//!
//! 与 [`crate::cdp_browser`]（browser_media 提取直链）、聊天面板（渲染 `media/…`
//! 路径媒体卡片）、`open_media`/`share_media`（APK 系统查看器/分享面板）衔接成
//! 完整链路：直链 → 本地文件 → 直接看/播 → 存本地/转发其它应用。
//!
//! 安全边界：
//! - 仅 http(s)，拒绝其它 scheme；
//! - SSRF 防护：主机名先做 DNS 解析，任一解析结果落在环回/私网/链路本地/
//!   未指定地址即拒绝（防止借 agent 之手探测内网）；
//! - 流式落盘 + 大小硬上限（[`MAX_DOWNLOAD_BYTES`]），超限即断并删除半成品；
//! - 文件名清洗（去目录成分、限白名单字符、限长），落盘路径恒在
//!   `base/media/download/` 之内，无法越界。

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::{Tool, ToolResult};

/// 单文件下载硬上限（200 MB：小红书/闲鱼视频普遍 ≤100 MB，留余量）。
const MAX_DOWNLOAD_BYTES: u64 = 200 * 1024 * 1024;
/// 整体下载时限（含连接、重定向与内容传输）。
const TOTAL_TIMEOUT: Duration = Duration::from_secs(180);
/// 连接超时。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// 临时文件序号（与时间戳共同保证唯一，防并发互踩）。
static DOWNLOAD_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 下载现场清理守卫：占位目标与半成品临时文件在成功提交前任一退出路径
/// （`?` 错误返回、turn 取消导致 future 被 drop——显式清理分支不会执行）
/// 都会删除两份文件；提交成功后 `armed = true` 解除。
struct DownloadGuard {
    temp: PathBuf,
    target: PathBuf,
    armed: bool,
}

impl Drop for DownloadGuard {
    fn drop(&mut self) {
        if self.armed {
            return;
        }
        let _ = std::fs::remove_file(&self.temp);
        let _ = std::fs::remove_file(&self.target);
    }
}

pub struct DownloadMedia {
    /// 工作区（agent）根目录：落盘固定在 `base/media/download/`。
    base: PathBuf,
}

impl DownloadMedia {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }
}

/// IP 是否属于 SSRF 危险段（环回/私网/链路本地/未指定/唯一本地/运营商 NAT）。
/// IPv4-mapped IPv6（::ffff:127.0.0.1 等）先还原成 IPv4 再判。
fn is_banned_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64 // CGNAT 100.64/10
        }
        std::net::IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_banned_ip(std::net::IpAddr::V4(v4));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6.is_unique_local()
        }
    }
}

/// 校验 URL：仅 http(s)、主机非空、DNS 解析结果全部不在禁用段。
/// 返回 (规范化 URL 串, 主机名, 全部已验证地址)——地址表供 HTTP 客户端
/// `resolve_to_addrs` 钉死连接目标（防 DNS rebinding），多地址保留连接
/// 回退（如 IPv6 不通走 IPv4），不再只取首选。
async fn validate_url(raw: &str) -> Result<(String, String, Vec<std::net::SocketAddr>), String> {
    let parsed: reqwest::Url =
        reqwest::Url::parse(raw.trim()).map_err(|e| format!("URL 无效：{e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!("仅支持 http/https，收到 {}", parsed.scheme()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL 缺少主机名".to_string())?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "URL 缺少端口".to_string())?;
    let authority = format!("{host}:{port}");
    // DNS 全解析全检查：任一地址落在禁用段即整体拒绝（防部分解析绕过）。
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host(&authority)
        .await
        .map_err(|e| format!("主机解析失败（{host}）：{e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("主机解析不到任何地址（{host}）"));
    }
    for addr in &addrs {
        if is_banned_ip(addr.ip()) {
            return Err(format!(
                "主机 {host} 解析到内网/保留地址（{}）——禁止下载，防止内网探测",
                addr.ip()
            ));
        }
    }
    // 钉住地址端口清零：resolve 只负责 IP；端口留给各次请求按 scheme 取默认
    // （http→80 / https→443），否则 http→https 同主机重定向会把 TLS 打到 80。
    let pinned = addrs
        .iter()
        .map(|a| std::net::SocketAddr::new(a.ip(), 0))
        .collect();
    Ok((parsed.to_string(), host, pinned))
}

/// 从 Content-Type 推断扩展名（无或通用类型返回空串，保持原文件名）。
fn ext_from_content_type(ct: &str) -> &'static str {
    let mime = ct
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match mime.as_str() {
        "image/jpeg" | "image/jpg" => ".jpg",
        "image/png" => ".png",
        "image/webp" => ".webp",
        "image/gif" => ".gif",
        "video/mp4" => ".mp4",
        "video/webm" => ".webm",
        "video/quicktime" => ".mov",
        "video/x-matroska" => ".mkv",
        "audio/mpeg" => ".mp3",
        "audio/mp4" | "audio/m4a" => ".m4a",
        "audio/aac" => ".aac",
        "audio/wav" | "audio/x-wav" => ".wav",
        "audio/ogg" => ".ogg",
        "audio/flac" => ".flac",
        _ => "",
    }
}

/// 文件名清洗：路径成分、控制字符与 Windows 非法字符统一替换为 `_`；仅保留
/// ASCII 字母/数字、CJK 与 `_ - .`（对齐聊天媒体解析器 parseMediaItems 的
/// 字符集：ASCII + CJK + 全角；`ï` 这类拉丁扩展字母会解析失败）；折叠残余
/// `..`；限长 80。输出恒为单段纯文件名。
fn sanitize_file_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        let banned =
            matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || ch.is_control();
        let allowed = ch.is_ascii_alphanumeric()
            || matches!(ch, '_' | '-' | '.')
            || matches!(ch, '\u{4E00}'..='\u{9FFF}');
        if banned || ch == '\0' || !allowed {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let trimmed = out.trim().trim_end_matches('.').trim();
    let mut name: String = trimmed.chars().take(80).collect();
    // 折叠残余的 ".."（Windows 文件名本就禁止；防呆而非防越界——落盘目录由构造保证）。
    while name.contains("..") {
        name = name.replace("..", "_");
    }
    name
}

/// 聊天媒体解析器（MediaPreviews.tsx）出卡片白名单——与
/// `\.(?:png|jpe?g|webp|gif|mp4|webm|m4v|mov)` 严格一致，引用这些路径才出
/// 媒体卡片；不在此列的后缀不出卡片（open_media 仍可播放/查看）。
const CHAT_CARD_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "mp4", "webm", "m4v", "mov",
];
/// 系统查看器/播放器（NativeBridge.openMedia 按扩展名定 MIME）可接受的
/// 媒体扩展名 = 卡片白名单 + 音频等。保留这些后缀不剥离。
const PLAYER_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "webp", "gif", "bmp", "avif", "mp4", "webm", "m4v", "mov", "mkv", "mp3",
    "m4a", "aac", "wav", "ogg", "flac",
];

/// 目标文件名原子预留：用户指定优先（清洗），否则取 URL 末段；后缀不是已知
/// 媒体扩展名时剥掉按 Content-Type 重补。用 `create_new` 原子占位——并发 turn
/// 撞名时后者自动落到 -2/-3 序号，最终 rename 不会覆盖他人的成品。
/// 成功返回占位路径（空文件，下载完成后由调用方 rename 覆盖）；失败返回错误。
fn reserve_target(
    dir: &Path,
    user_name: Option<&str>,
    url: &reqwest::Url,
    content_type: &str,
) -> Result<PathBuf, String> {
    let raw = user_name
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            url.path()
                .rsplit('/')
                .find(|seg| !seg.is_empty() && *seg != "." && *seg != "..")
                .map(|seg| seg.to_string())
        })
        .unwrap_or_else(|| "media-download".to_string());
    let mut name = sanitize_file_name(&raw);
    if name.is_empty() {
        name = "media-download".to_string();
    }
    let ext_known = name
        .rsplit_once('.')
        .map(|(_, e)| PLAYER_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false);
    if !ext_known {
        // 剥掉非媒体后缀（如 .php .ashx），按 Content-Type 重补。
        if let Some((stem, _)) = name.rsplit_once('.') {
            if !stem.is_empty() {
                name = stem.to_string();
            }
        }
        let ext = ext_from_content_type(content_type);
        if !ext.is_empty() {
            name.push_str(ext);
        }
    }
    let mut candidate = dir.join(&name);
    let stem = name
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or(name.clone());
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_string())
        .map(|e| format!(".{e}"))
        .unwrap_or_default();
    // create_new 原子预留；撞名递增序号（上限 64，防异常环境死循环）。
    for n in 2..=64 {
        match std::fs::File::create_new(&candidate) {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("预留目标文件失败: {error}")),
        }
        candidate = dir.join(format!("{stem}-{n}{ext}"));
    }
    Err("同名文件过多（≥64），请换一个 filename".to_string())
}

#[async_trait::async_trait]
impl Tool for DownloadMedia {
    fn name(&self) -> &str {
        "download_media"
    }

    fn description(&self) -> &str {
        "把远程图片/视频/音频下载到工作区媒体沙箱 media/download/（返回工作区相对路径）。配合 browser_media 的媒体直链使用：下载后回复里引用 media/… 路径（聊天面板渲染媒体卡片），APK 上可用 open_media 打开/播放、publish_media 发往 QQ。仅 http/https 公网地址；单文件上限 200 MB、总时限 180 秒。"
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "媒体直链（http/https，来自 browser_media 或用户提供的公开地址）" },
                "filename": { "type": "string", "description": "可选：目标文件名（不含路径）；缺省从 URL 推导" }
            },
            "required": ["url"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(url_raw) = input.get("url").and_then(Value::as_str) else {
            return ToolResult::err("缺少参数 url");
        };
        let user_name = input
            .get("filename")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // 单一整体 deadline：DNS 校验、响应头、内容传输共享同一预算，
        // 避免「头阶段耗光后再给正文一整轮」的隐性翻倍。
        let deadline = tokio::time::Instant::now() + TOTAL_TIMEOUT;
        let (url, host, pinned_addrs) =
            match tokio::time::timeout_at(deadline, validate_url(url_raw)).await {
                Ok(Ok(ok)) => ok,
                Ok(Err(error)) => return ToolResult::err(error),
                Err(_) => return ToolResult::err("整体超时：主机解析耗时超过总时限"),
            };

        // 沙箱包含性校验先于任何文件系统写入（fail-closed）：media 或 media/
        // download 若是链出工作区的符号链接/junction，在这里就拒绝，而不是
        // create_dir_all 跟着链接写出去。校验方式：沿 media/download 向上找
        // 【最近一个已存在的祖先】，其物理位置必须仍在工作区内。
        let base_canonical = match self.base.canonicalize() {
            Ok(p) => p,
            Err(e) => {
                return ToolResult::err(format!("工作区根不可达（{}）: {e}", self.base.display()))
            }
        };
        {
            let mut probe = self.base.join("media").join("download");
            loop {
                if probe.is_symlink() {
                    return ToolResult::err(format!(
                        "{} 是符号链接——拒绝下载（防越区写入）",
                        probe.display()
                    ));
                }
                match probe.canonicalize() {
                    Ok(physical) => {
                        if !physical.starts_with(&base_canonical) {
                            return ToolResult::err(format!(
                                "{} 物理指向工作区之外——拒绝下载（防越区写入）",
                                probe.display()
                            ));
                        }
                        break;
                    }
                    // 尚不存在：向上找最近的存在祖先继续校验。
                    Err(_) => {
                        let Some(parent) = probe.parent() else {
                            return ToolResult::err("media/download 无有效祖先路径");
                        };
                        probe = parent.to_path_buf();
                    }
                }
            }
        }

        let dir = self.base.join("media").join("download");
        if let Err(error) = tokio::fs::create_dir_all(&dir).await {
            return ToolResult::err(format!("创建 media/download 目录失败: {error}"));
        }
        // create_dir_all 之后复核一次物理位置（与前序校验之间的窗口兜底）。
        let dir_canonical = match dir.canonicalize() {
            Ok(p) => p,
            Err(e) => return ToolResult::err(format!("media/download 不可达: {e}")),
        };
        if !dir_canonical.starts_with(&base_canonical) {
            return ToolResult::err("media/download 物理指向工作区之外——拒绝下载（防越区写入）");
        }

        // resolve 钉死已验证地址（DNS rebinding 防线）；重定向只跟同主机
        // （跨主机 = 换源，让 agent 拿新地址重新走全套校验），上限 5 跳。
        let redirect_host = host.clone();
        let client = match reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Mobile Safari/537.36")
            .connect_timeout(CONNECT_TIMEOUT)
            // 禁用环境代理：代理会用原始主机名自行解析，绕过已验证地址的
            // SSRF 防线（HTTP_PROXY/HTTPS_PROXY/ALL_PROXY 全部不生效）。
            .no_proxy()
            .resolve_to_addrs(host.as_str(), &pinned_addrs)
            .redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 5 {
                    return attempt.error("重定向超过 5 跳");
                }
                match attempt.url().host_str() {
                    Some(h) if h == redirect_host => attempt.follow(),
                    _ => attempt.stop(),
                }
            }))
            .build()
        {
            Ok(client) => client,
            Err(error) => return ToolResult::err(format!("HTTP 客户端构建失败: {error}")),
        };
        // 手动整体限时的流式下载：先收响应头，边收边限量，避免大文件整体超时误伤慢链路。
        let request = async {
            let response = client.get(&url).send().await?;
            response.error_for_status()
        };
        let response = match tokio::time::timeout_at(deadline, request).await {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => return ToolResult::err(format!("下载请求失败: {error}")),
            Err(_) => {
                return ToolResult::err(format!(
                    "下载超时（>{:?}）——文件过大或链路过慢，建议换更小的源",
                    TOTAL_TIMEOUT
                ))
            }
        };

        // 同主机重定向策略下出现的 3xx = 跨主机跳转被 stop——显式拒绝，
        // 不把 3xx 的空壳响应当内容存盘。
        if response.status().is_redirection() {
            // stop 策略下 response.url() 停在原 URL——目标从 Location 解析，
            // 连同主机名一并返回，agent 拿新地址重试时会重新走全套公网校验。
            let dest = response
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|loc| {
                    reqwest::Url::options()
                        .base_url(Some(response.url()))
                        .parse(loc)
                        .ok()
                })
                .map(|u| u.to_string())
                .unwrap_or_else(|| "<无法解析 Location>".to_string());
            return ToolResult::err(format!(
                "下载被重定向到外部主机（{} → {dest}）——已停止；如需该资源，把 {dest} 作为 url 交给 download_media 重试（会重新走全套公网校验）",
                response.url().host_str().unwrap_or("未知主机")
            ));
        }

        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        if let Some(len) = response.content_length() {
            if len > MAX_DOWNLOAD_BYTES {
                return ToolResult::err(format!(
                    "文件 {} MB 超过上限 {} MB，未下载",
                    len / 1024 / 1024,
                    MAX_DOWNLOAD_BYTES / 1024 / 1024
                ));
            }
        }
        // HTML/JSON 响应几乎必然是登录页/反爬页/接口错误——按媒体文件名落盘
        // 只会产出打不开的假媒体，下载前直接拒绝（application/octet-stream
        // 保留放行：不少 CDN 对视频用通用二进制类型）。
        let ct_lower = content_type.to_ascii_lowercase();
        if ct_lower.starts_with("text/html")
            || ct_lower.starts_with("application/xhtml")
            || ct_lower.starts_with("application/json")
            || ct_lower.starts_with("text/plain")
        {
            return ToolResult::err(format!(
                "响应不是媒体文件（Content-Type: {content_type}）——多为登录页/反爬页/接口错误，未保存。可能需要先登录，或换真正的媒体直链"
            ));
        }

        // 原子预留最终名（create_new 空占位）：并发撞名自动落到 -2/-3，后续
        // rename 只覆盖「自己刚预留的空占位」，不会吞掉别的 turn 的成品。
        let target = match reserve_target(
            &dir,
            user_name,
            &reqwest::Url::parse(&url).unwrap(),
            &content_type,
        ) {
            Ok(t) => t,
            Err(error) => return ToolResult::err(error),
        };
        // 临时文件独占创建（create_new）：可预测的临时路径若被预置符号链接，
        // File::create 会跟随并截断链接目标（越区写入）——create_new 拒绝已
        // 存在路径，创建成功即「本调用所有」，且绝不跟随已有链接。
        // 清理守卫在【创建之前】就位（持有路径）：turn 取消时 open 的 future
        // 被 drop 并不回滚已完成的创建——守卫 drop 会补删占位与半成品。
        use futures::StreamExt;
        use tokio::io::AsyncWriteExt;
        let mut guard = DownloadGuard {
            temp: PathBuf::new(),
            target: target.clone(),
            armed: false,
        };
        let (temp, mut file) = loop {
            let temp = dir.join(format!(
                ".download-{}-{}.part",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or_default(),
                DOWNLOAD_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            guard.temp = temp.clone();
            // 同步创建：文件创建是微秒级元数据操作（tokio 文档允许短阻塞），
            // 没有 await 就没有取消点——turn 取消不可能留下孤儿空 .part。
            let std_file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
            {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return ToolResult::err(format!("创建临时文件失败: {error}（清理由守卫完成）"));
                }
            };
            break (temp, tokio::fs::File::from_std(std_file));
        };

        let mut stream = response.bytes_stream();
        let mut written: u64 = 0;
        loop {
            let chunk = match tokio::time::timeout_at(deadline, stream.next()).await {
                Err(_) => {
                    return ToolResult::err(format!(
                        "下载超时（>{:?}），半成品已清理——建议换更小的源",
                        TOTAL_TIMEOUT
                    ));
                }
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(error))) => {
                    return ToolResult::err(format!("下载中断: {error}（半成品已清理）"));
                }
                Ok(None) => break,
            };
            written += chunk.len() as u64;
            if written > MAX_DOWNLOAD_BYTES {
                return ToolResult::err(format!(
                    "文件超过上限 {} MB（响应头未声明长度），已中断并清理",
                    MAX_DOWNLOAD_BYTES / 1024 / 1024
                ));
            }
            if let Err(error) = file.write_all(&chunk).await {
                return ToolResult::err(format!("写入失败: {error}"));
            }
        }
        if let Err(error) = file.flush().await {
            return ToolResult::err(format!("落盘失败: {error}"));
        }
        drop(file);
        if written == 0 {
            return ToolResult::err("响应体为空，未保存");
        }
        if let Err(error) = tokio::fs::rename(&temp, &target).await {
            return ToolResult::err(format!("提交文件失败: {error}"));
        }
        guard.armed = true;

        // 落盘路径防越界复核（文件名已清洗，此处为纵深防御；canonicalize
        // 失败同样 fail-closed——删文件并报错，不做「跳过检查」的放行）。
        match (target.canonicalize(), self.base.canonicalize()) {
            (Ok(actual), Ok(base_abs)) if actual.starts_with(&base_abs) => {}
            _ => {
                let _ = tokio::fs::remove_file(&target).await;
                return ToolResult::err("落盘路径越界（不应发生），已清理");
            }
        }

        let file_name = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let is_card = file_name
            .rsplit_once('.')
            .map(|(_, e)| CHAT_CARD_EXTS.iter().any(|c| *c == e.to_ascii_lowercase()))
            .unwrap_or(false);
        let card_note = if is_card {
            "回复中引用该 media/… 路径即可在聊天面板出媒体卡片。"
        } else {
            "该格式聊天面板不出卡片，但 APK 可用 open_media 播放/查看。"
        };
        let rel = format!(
            "media/download/{}",
            target
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        ToolResult::ok(format!(
            "已下载：{rel}（{}，{:.1} MB）。
{card_note}",
            if content_type.is_empty() {
                "未知类型"
            } else {
                content_type.as_str()
            },
            written as f64 / 1024.0 / 1024.0
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn banned_ips_cover_loopback_and_private() {
        assert!(is_banned_ip("127.0.0.1".parse().unwrap()));
        assert!(is_banned_ip("10.1.2.3".parse().unwrap()));
        assert!(is_banned_ip("192.168.1.1".parse().unwrap()));
        assert!(is_banned_ip("172.16.0.9".parse().unwrap()));
        assert!(is_banned_ip("169.254.1.1".parse().unwrap()));
        assert!(is_banned_ip("100.64.0.1".parse().unwrap()));
        assert!(is_banned_ip("::1".parse().unwrap()));
        assert!(is_banned_ip("fe80::1".parse().unwrap()));
        assert!(!is_banned_ip("8.8.8.8".parse().unwrap()));
        assert!(!is_banned_ip("114.114.114.114".parse().unwrap()));
    }

    #[test]
    fn url_validation_rejects_non_http_and_private_hosts() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            assert!(validate_url("ftp://example.com/a.mp4").await.is_err());
            assert!(validate_url("file:///etc/passwd").await.is_err());
            assert!(validate_url("http://127.0.0.1:9222/json").await.is_err());
            assert!(validate_url("http://192.168.0.101/x.mp4").await.is_err());
            // 公网域名解析正常时通过；离线环境解析失败也算拒绝，不 panic。
            let _ = validate_url("https://www.xiaohongshu.com/a.mp4").await;
        });
    }

    #[test]
    fn reserve_target_sanitizes_and_extends() {
        let dir = std::env::temp_dir().join(format!("dlm-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let url = reqwest::Url::parse("https://cdn.example.com/v/a/b/clip.mp4").unwrap();
        let t = reserve_target(&dir, None, &url, "video/mp4").unwrap();
        assert_eq!(t.file_name().unwrap(), "clip.mp4");
        // 用户名带目录成分被清洗（../ 折叠，且 create_new 原子占位）。
        let t2 = reserve_target(&dir, Some("../evil/../../x.mp4"), &url, "").unwrap();
        assert!(!t2.to_string_lossy().contains(".."));
        // 无扩展名（.php 之类非媒体后缀同理）按 Content-Type 补。
        let url2 = reqwest::Url::parse("https://cdn.example.com/v/9821.php").unwrap();
        let t3 = reserve_target(&dir, None, &url2, "video/mp4").unwrap();
        assert!(t3.to_string_lossy().ends_with(".mp4"));
        // 清理占位文件。
        for f in [t, t2, t3] {
            let _ = std::fs::remove_file(f);
        }
        let _ = std::fs::remove_dir(&dir);
    }
}
