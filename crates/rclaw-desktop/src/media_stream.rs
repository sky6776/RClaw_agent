//! 会话媒体本地流式服务：仅回环地址的极简 HTTP 服务（支持 Range/206 分段），
//! 供 WebView 内 `<video>` 元素流式加载工作区视频/音频——data URL 整包注入
//! 对视频完全不可行（体积），WebView 自带播放器走 HTTP Range 是各类开源
//! WebView 应用（Nextcloud/Telegram Web 等）的标准做法。
//!
//! 安全面：只绑定 127.0.0.1（外部不可达）；每次应用启动随机 token，URL 必须
//! 携带；路径经与 media_base64 相同的工作区守卫（禁越界/符号链接逃逸）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// 流媒体允许的扩展名 → Content-Type（视频/音频容器，WebView 可直接解码的
/// 子集；不认识的容器走系统播放器，不进这里）。
const STREAM_TYPES: &[(&str, &str)] = &[
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("m4v", "video/mp4"),
    ("mov", "video/quicktime"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("wav", "audio/wav"),
    ("mp3", "audio/mpeg"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/ogg"),
    ("flac", "audio/flac"),
];

pub struct StreamServer {
    pub port: u16,
    pub token: String,
    agent_root: PathBuf,
}

static SERVER: OnceLock<StreamServer> = OnceLock::new();

/// 懒启动本地流服务（进程内单例）。agent_root 用于请求期的路径守卫。
pub fn server(agent_root: PathBuf) -> &'static StreamServer {
    SERVER.get_or_init(|| {
        let token: String = {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            (0..32)
                .map(|_| {
                    let alphabet = b"abcdef0123456789";
                    alphabet[rng.gen_range(0..alphabet.len())] as char
                })
                .collect()
        };
        // 端口 0 = 内核随机分配，避免与其它本地服务冲突。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("绑定本地流媒体端口失败");
        let port = listener.local_addr().expect("读取本地端口失败").port();
        let agent_root = agent_root.canonicalize().unwrap_or(agent_root);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(socket) = stream else { continue };
                // 每连接一个轻量线程：视频拖动进度会产生频繁的小段请求。
                std::thread::spawn(move || handle_conn(socket));
            }
        });
        StreamServer {
            port,
            token,
            agent_root,
        }
    })
}

/// 生成某工作区媒体路径的流式 URL（含 token）。路径守卫失败返回 None。
/// 命令入口：确保服务已按 agent 根目录启动，再取流式 URL。
pub fn stream_url_with_root(agent_root: PathBuf, path: &str) -> Option<String> {
    server(agent_root);
    stream_url(path)
}

pub fn stream_url(path: &str) -> Option<String> {
    let server = SERVER.get()?;
    // 与 media_base64 相同的守卫，提前校验（请求期还会再验一次）。
    resolve_media_path(&server.agent_root, path)?;
    if !is_streamable(path) {
        return None;
    }
    Some(format!(
        "http://127.0.0.1:{}/m?t={}&p={}",
        server.port,
        server.token,
        percent_encode_path(path)
    ))
}

/// 扩展名白名单（与 media_base64 的媒体扩展守卫同一防御纵深）：工作区里的
/// 非媒体文件（会话记录 / 配置等）不进 HTTP 面。
fn is_streamable(path: &str) -> bool {
    let ext = path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    STREAM_TYPES.iter().any(|(e, _)| *e == ext)
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::new();
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 工作区路径守卫：禁 ..、反斜杠、绝对路径；归一后必须仍位于 agent/ 之内。
fn resolve_media_path(agent_root: &Path, path: &str) -> Option<PathBuf> {
    if path.contains("..") || path.contains('\\') || path.starts_with('/') {
        return None;
    }
    let full = agent_root.join(path).canonicalize().ok()?;
    let root_canon = agent_root.canonicalize().ok()?;
    if !full.starts_with(&root_canon) {
        return None;
    }
    Some(full)
}

fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    STREAM_TYPES
        .iter()
        .find(|(e, _)| *e == ext.as_str())
        .map(|(_, t)| *t)
        .unwrap_or("application/octet-stream")
}

/// 解析请求首行与头（只需 Range 与路径 query），返回 (path, range)。
/// token 不符 / 畸形请求返回 None（调用方回 403）。
fn parse_request(socket: &mut std::net::TcpStream) -> Option<(String, Option<(u64, u64)>)> {
    let mut buf = [0u8; 4096];
    let n = std::io::Read::read(socket, &mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    let mut lines = text.lines();
    // 首行：GET /m?t=..&p=.. HTTP/1.1
    let request_line = lines.next()?.to_string();
    let target = request_line.split_whitespace().nth(1)?;
    let query = target.split('?').nth(1).unwrap_or_default();
    let mut params: HashMap<String, String> = HashMap::new();
    for pair in query.split('&') {
        let mut kv = pair.splitn(2, '=');
        let k = kv.next().unwrap_or_default().to_string();
        let v = kv.next().unwrap_or_default().to_string();
        params.insert(k, v);
    }
    let token = params.get("t")?;
    let path_encoded = params.get("p")?;
    let server = SERVER.get()?;
    if token != &server.token {
        return None;
    }
    let path = percent_decode(path_encoded)?;
    // Range: bytes=start-end（end 可省略 = 到文件尾）。
    let mut range = None;
    for line in lines {
        if let Some(value) = line
            .strip_prefix("Range: ")
            .or(line.strip_prefix("range: "))
        {
            if let Some(spec) = value.strip_prefix("bytes=") {
                let mut parts = spec.trim().split('-');
                let start = parts.next().and_then(|s| s.parse::<u64>().ok());
                let end = parts.next().and_then(|s| s.parse::<u64>().ok());
                if let Some(start) = start {
                    range = Some((start, end.unwrap_or(u64::MAX)));
                }
            }
        }
    }
    Some((path, range))
}

// %XX 手工解码（路径只含安全字符 + %XX 转义的 CJK/空格）。必须按字节收集
// 再整体 from_utf8：逐字节 `as char` 会把 UTF-8 多字节序列拆成 Latin-1 字符，
// 中文文件名全部变成乱码路径（404）。
fn percent_decode(encoded: &str) -> Option<String> {
    let bytes = encoded.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            let byte = u8::from_str_radix(hex, 16).ok()?;
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn handle_conn(mut socket: std::net::TcpStream) {
    let Some((path, range)) = parse_request(&mut socket) else {
        let _ = write_all(
            &mut socket,
            b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n",
        );
        return;
    };
    let server = match SERVER.get() {
        Some(s) => s,
        None => return,
    };
    // 请求期复验扩展名（URL 可能被手工构造）：非媒体文件一律 404。
    if !is_streamable(&path) {
        let _ = write_all(
            &mut socket,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        );
        return;
    }
    let Some(full) = resolve_media_path(&server.agent_root, &path) else {
        let _ = write_all(
            &mut socket,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        );
        return;
    };
    let Ok(meta) = std::fs::metadata(&full) else {
        let _ = write_all(
            &mut socket,
            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n",
        );
        return;
    };
    let total = meta.len();
    if total == 0 {
        // 空文件：Content-Length 必须为 0（否则承诺 1 字节发 0 字节，客户端按
        // 截断报错）。
        let _ = write_all(&mut socket, b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        return;
    }
    let ctype = content_type(&full);
    let (start, end, status, extra) = match range {
        Some((s, e)) => {
            let end = e.min(total.saturating_sub(1));
            if s >= total {
                // 越界起始：416
                let _ = write_all(
                    &mut socket,
                    format!(
                        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nContent-Length: 0\r\n\r\n"
                    )
                    .as_bytes(),
                );
                return;
            }
            (
                s,
                end,
                "HTTP/1.1 206 Partial Content",
                format!("Content-Range: bytes {s}-{end}/{total}\r\n"),
            )
        }
        None => (0, total.saturating_sub(1), "HTTP/1.1 200 OK", String::new()),
    };
    let header = format!(
        "{status}\r\nContent-Type: {ctype}\r\n{extra}Accept-Ranges: bytes\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        end - start + 1
    );
    if write_all(&mut socket, header.as_bytes()).is_err() {
        return;
    }
    // 分块读文件发送（64KB），Seek 到 start 起。
    let Ok(mut file) = std::fs::File::open(&full) else {
        return;
    };
    use std::io::{Read, Seek, SeekFrom};
    if file.seek(SeekFrom::Start(start)).is_err() {
        return;
    }
    let mut remaining = end - start + 1;
    let mut chunk = [0u8; 64 * 1024];
    while remaining > 0 {
        let want = remaining.min(chunk.len() as u64) as usize;
        let n = match file.read(&mut chunk[..want]) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if write_all(&mut socket, &chunk[..n]).is_err() {
            break;
        }
        remaining -= n as u64;
    }
    let _ = socket.shutdown(std::net::Shutdown::Write);
}

fn write_all(socket: &mut std::net::TcpStream, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let n = std::io::Write::write(socket, data)?;
        if n == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "write zero",
            ));
        }
        data = &data[n..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_streamable, percent_decode, percent_encode_path};

    /// 流式白名单：媒体容器放行，文本/配置类拒绝。
    #[test]
    fn streamable_extensions() {
        assert!(is_streamable("media/a.MP4"));
        assert!(is_streamable("media/视频 拍摄.webm"));
        assert!(is_streamable("media/x.m4a"));
        assert!(!is_streamable("sessions/main.jsonl"));
        assert!(!is_streamable("config.toml"));
        assert!(!is_streamable("media/noext"));
    }

    /// 编码→解码必须无损（重点：UTF-8 多字节中文、空格与安全字符直通）。
    #[test]
    fn percent_roundtrip_cjk() {
        for p in [
            "media/微信图片_2024.jpg",
            "media/out/abc123/视频 拍摄.mp4",
            "uploads/Naïve file (1).png",
            "media/plain_ascii-name.mp4",
        ] {
            let enc = percent_encode_path(p);
            assert_eq!(percent_decode(&enc).as_deref(), Some(p), "roundtrip {p}");
        }
    }

    /// 安全字符不转义；多字节字符按字节转义为 %XX。
    #[test]
    fn percent_encode_keeps_safe_chars() {
        assert_eq!(percent_encode_path("a/b~c-d.mp4"), "a/b~c-d.mp4");
        assert_eq!(percent_encode_path("微.mp4"), "%E5%BE%AE.mp4");
    }

    /// 非法转义序列整体拒绝（None → 403）：编码器只产出合法 %XX，出现
    /// 畸形串说明请求并非来自本服务生成的 URL。
    #[test]
    fn percent_decode_invalid_rejects() {
        assert_eq!(percent_decode("100%.mp4"), None);
        assert_eq!(percent_decode("%zz.mp4"), None);
    }
}
