//! rclaw-weixin-login：微信扫码登录工具（移植自 CClaw `tools/weixin_login`）。
//!
//! 通过 iLink Bot API 取二维码 → 终端渲染 → 轮询扫码状态 → 拿到 `bot_token`。
//! 与 RClaw 微信通道（`rclaw-channels::weixin`）共用同一 iLink Bot 协议，
//! 因此取到的 token 可直接填入 `rclaw.toml` 的 `[weixin]` 段。
//!
//! 与 C 版的差异：
//! - 用纯 Rust [`qrcode`] crate 在终端渲染二维码，无需外部 `qrencode` 命令（Windows 友好）；
//! - 成功后输出 `[weixin]` TOML 片段（RClaw 配置全来自文件，不再写 `WEIXIN_BOT_TOKEN=` 环境变量）。
//!
//! 运行：`rclaw-weixin-login [--api-base URL] [--timeout SEC] [--toml-file PATH] [--no-qr]`

use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use qrcode::{render::unicode::Dense1x2, QrCode};
use rand::Rng;
use serde::Deserialize;

const DEFAULT_API_BASE: &str = "https://ilinkai.weixin.qq.com";
const LOGIN_TIMEOUT_SEC: u64 = 300;
const REQUEST_TIMEOUT_SEC: u64 = 35;
const LOGIN_MAX_REFRESHES: usize = 3;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

// ---------------------------------------------------------------------------
// iLink Bot 登录接口的响应结构。多余字段一律忽略，避免上游加减字段导致解析失败。
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct QrcodeResp {
    /// 二维码标识，用于后续轮询 `get_qrcode_status?qrcode=`。
    qrcode: String,
    /// 二维码承载的数据（手机微信扫描的内容），用于渲染二维码图像。
    qrcode_img_content: String,
}

#[derive(Deserialize)]
struct StatusResp {
    status: String,
    #[serde(default)]
    bot_token: Option<String>,
    #[serde(default)]
    ilink_bot_id: Option<String>,
    #[serde(default)]
    ilink_user_id: Option<String>,
    #[serde(default)]
    baseurl: Option<String>,
}

struct Args {
    api_base: String,
    timeout_sec: u64,
    toml_file: Option<String>,
    render_qr: bool,
}

fn print_usage() {
    println!(
        "Usage: rclaw-weixin-login [--api-base URL] [--timeout SEC] [--toml-file PATH] [--no-qr]"
    );
    println!();
    println!("Options:");
    println!("  --api-base URL   iLink API base URL. Default: {DEFAULT_API_BASE}.");
    println!("  --timeout SEC    Total wait time for each QR code. Default: {LOGIN_TIMEOUT_SEC}.");
    println!("  --toml-file PATH Write a [weixin] TOML snippet (token + ids) to PATH.");
    println!("  --no-qr          Do not render QR in terminal; print raw content only.");
    println!("  -h, --help       Show this help.");
}

fn parse_args(argv: Vec<String>) -> Result<Args> {
    let mut args = Args {
        api_base: DEFAULT_API_BASE.to_string(),
        timeout_sec: LOGIN_TIMEOUT_SEC,
        toml_file: None,
        render_qr: true,
    };
    let mut it = argv.into_iter().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            "--no-qr" => args.render_qr = false,
            "--api-base" => {
                let v = it.next().ok_or_else(|| anyhow!("{a} requires a URL"))?;
                args.api_base = v;
            }
            "--timeout" => {
                let v = it.next().ok_or_else(|| anyhow!("{a} requires seconds"))?;
                args.timeout_sec = v
                    .parse()
                    .with_context(|| format!("--timeout must be a positive integer, got {v}"))?;
                if args.timeout_sec == 0 {
                    return Err(anyhow!("--timeout must be positive"));
                }
            }
            "--toml-file" => {
                let v = it.next().ok_or_else(|| anyhow!("{a} requires a path"))?;
                args.toml_file = Some(v);
            }
            other => return Err(anyhow!("unknown option: {other}")),
        }
    }
    Ok(args)
}

/// 复刻 C 版 `generate_uin`：随机 u32 → 十进制字符串 → 标准 base64 编码其 ASCII 字节，
/// 作为 `X-WECHAT-UIN` 请求头的值。
fn generate_uin() -> String {
    let n: u32 = rand::thread_rng().gen();
    let number = n.to_string();
    base64::engine::general_purpose::STANDARD.encode(number.as_bytes())
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SEC))
        .build()
        .context("failed to build http client")
}

async fn fetch_qrcode(client: &reqwest::Client, api_base: &str) -> Result<QrcodeResp> {
    let uin = generate_uin();
    let url = format!("{api_base}/ilink/bot/get_bot_qrcode");
    let resp = client
        .get(&url)
        .header("X-WECHAT-UIN", uin)
        .query(&[("bot_type", "3")])
        .send()
        .await
        .context("request get_bot_qrcode")?
        .error_for_status()
        .context("get_bot_qrcode returned non-2xx")?
        .json::<QrcodeResp>()
        .await
        .context("parse qrcode response")?;
    if resp.qrcode.is_empty() || resp.qrcode_img_content.is_empty() {
        return Err(anyhow!(
            "qrcode response missing qrcode / qrcode_img_content"
        ));
    }
    Ok(resp)
}

async fn poll_status(client: &reqwest::Client, api_base: &str, qrcode: &str) -> Result<StatusResp> {
    let uin = generate_uin();
    let url = format!("{api_base}/ilink/bot/get_qrcode_status");
    let resp = client
        .get(&url)
        .header("iLink-App-ClientVersion", "1")
        .header("X-WECHAT-UIN", uin)
        // reqwest 的 query() 会自动对 qrcode 做 URL 编码，等价于 C 版的 url_escape。
        .query(&[("qrcode", qrcode)])
        .send()
        .await
        .context("request get_qrcode_status")?
        .error_for_status()
        .context("get_qrcode_status returned non-2xx")?
        .json::<StatusResp>()
        .await
        .context("parse status response")?;
    if resp.status.is_empty() {
        return Err(anyhow!("status response missing status"));
    }
    Ok(resp)
}

/// 把二维码内容渲染为终端可扫的紧凑 Unicode（半块字符）二维码。失败返回 `None`。
fn render_qr(content: &str) -> Option<String> {
    let code = QrCode::new(content.as_bytes()).ok()?;
    Some(code.render::<Dense1x2>().build())
}

fn print_qr(qr: &QrcodeResp, render: bool) {
    println!();
    println!("Weixin scan login");
    println!("=================");
    println!();
    if render {
        if let Some(image) = render_qr(&qr.qrcode_img_content) {
            println!("{image}");
            println!();
            return;
        }
        println!("QR rendering failed; printing raw content instead.");
        println!();
    }
    println!("Copy this content into a QR generator, then scan it with WeChat:");
    println!();
    println!("{}", qr.qrcode_img_content);
    println!();
}

/// TOML 基本字符串转义：仅处理必须转义的字符。
fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 构造可直接粘贴进 `rclaw.toml` 的 `[weixin]` 片段。
/// `token` 是 RClaw 实际读取的配置键；其余字段仅以注释形式留档（RClaw 当前 `[weixin]`
/// 不读这些键，且配置启用了 `deny_unknown_fields`，故不能作为活动键写入）。
fn build_toml_snippet(s: &StatusResp) -> String {
    let mut buf = String::new();
    buf.push_str("[weixin]\n");
    buf.push_str("enabled = true\n");
    let token = s.bot_token.as_deref().unwrap_or("");
    buf.push_str(&format!("token = \"{}\"\n", toml_escape(token)));
    if let Some(id) = s.ilink_bot_id.as_deref().filter(|v| !v.is_empty()) {
        buf.push_str(&format!("# ilink_bot_id = \"{}\"\n", toml_escape(id)));
    }
    if let Some(id) = s.ilink_user_id.as_deref().filter(|v| !v.is_empty()) {
        buf.push_str(&format!("# ilink_user_id = \"{}\"\n", toml_escape(id)));
    }
    if let Some(b) = s.baseurl.as_deref().filter(|v| !v.is_empty()) {
        buf.push_str(&format!("# baseurl = \"{}\"\n", toml_escape(b)));
    }
    buf
}

/// 把含 bot_token 的片段落盘：Unix 上以 0600 创建（`fs::write` 沿用 umask 默认 0644，
/// 多用户主机上其他本地用户可读），且不跟随已存在的符号链接覆盖目标。
fn write_secret_file(path: &str, content: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "refusing to write through a symlink",
        ));
    }
    // 目标已存在且不是本工具此前写出的 `[weixin]` 片段（典型误用：`--toml-file rclaw.toml`
    // 指向完整配置）时拒绝覆盖：truncate 会把 [[llm.providers]] 等全部配置整体抹掉。
    // 判定按结构而非前缀：首次用本工具生成的文件以 `[weixin]` 开头，用户随后在下面追加
    // [[llm.providers]] / [agent] 等节再重跑一次就会被前缀判定放行；只要出现任何非
    // `[weixin]` 的表头行就视为完整配置。读失败（非 UTF-8、无权限）一律拒绝，不能失败放行。
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(existing) = existing {
        let body = existing.trim_start_matches('\u{feff}').trim_start();
        let foreign_table = body
            .lines()
            .map(str::trim_start)
            .filter(|line| line.starts_with('['))
            .any(|line| !line.starts_with("[weixin]"));
        if !body.is_empty() && (!body.starts_with("[weixin]") || foreign_table) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "target exists and is not a [weixin] snippet; refusing to overwrite it \
                 (paste the printed snippet into rclaw.toml instead)",
            ));
        }
    }
    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    file.flush()
}

async fn run(args: &Args) -> Result<()> {
    let client = http_client()?;

    for _ in 0..LOGIN_MAX_REFRESHES {
        let qr = fetch_qrcode(&client, &args.api_base).await?;
        print_qr(&qr, args.render_qr);
        println!(
            "Waiting for scan confirmation, timeout: {} seconds...",
            args.timeout_sec
        );

        let started = Instant::now();
        let timeout = Duration::from_secs(args.timeout_sec);
        let mut last_status = String::new();

        while started.elapsed() < timeout {
            let status = poll_status(&client, &args.api_base, &qr.qrcode).await?;

            if status.status != last_status {
                println!("Status: {}", status.status);
                last_status = status.status.clone();
            }

            if status.status == "confirmed" {
                // 登录确认后必须有非空 token，否则视为失败。
                if status
                    .bot_token
                    .as_deref()
                    .filter(|t| !t.is_empty())
                    .is_none()
                {
                    return Err(anyhow!("login confirmed, but bot_token is empty"));
                }

                println!();
                println!("Weixin login success.");
                let snippet = build_toml_snippet(&status);
                println!("--- rclaw.toml snippet ---");
                print!("{snippet}");
                println!("--------------------------");
                if let Some(path) = &args.toml_file {
                    write_secret_file(path, &snippet)
                        .with_context(|| format!("write toml file {path}"))?;
                    println!();
                    println!("Saved TOML snippet to: {path}");
                }
                return Ok(());
            }

            if status.status == "expired" {
                println!("QR code expired, refreshing...");
                break;
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }

        println!("QR code wait timed out.");
    }

    Err(anyhow!(
        "weixin login failed after {LOGIN_MAX_REFRESHES} QR refresh attempts"
    ))
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args(std::env::args().collect()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::from(2);
        }
    };
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
