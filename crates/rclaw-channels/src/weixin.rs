//! 微信 / iLink Bot 通道。
//!
//! 流程：
//! - 长轮询 `POST {base}/ilink/bot/getupdates` 收消息 → 投递 inbound；
//! - `POST {base}/ilink/bot/sendmessage` 发回复（按字节 UTF-8 安全分段）；
//! - allowlist 过滤来源；失败退避。
//!
//! 鉴权头：`Authorization: Bearer <token>` + `AuthorizationType: ilink_bot_token`
//! + `X-WECHAT-UIN: <base64(随机数字串)>`（服务端要求）。
//!
//! # 媒体收发（图片 / 文件 / 视频 / 语音）
//!
//! iLink 协议的 `item_list[].type`：1=文本、2=图片、3=语音、4=文件、5=视频。
//! 媒体本体经独立 CDN（`novac2c.cdn.weixin.qq.com/c2c`）中转，全程 **AES-128-ECB + PKCS7**
//! 加密（调用方自行加解密）。
//!
//! - **入站**：`classify_items` 抽出文本/媒体引用；`fetch_media` 从 CDN 下载并解密落盘到
//!   `media_dir/weixin/`，正文里注入 `[文件: x] <path>` 等标注，agent 即可用 `read`
//!   读取文本类文件。语音用服务端转写文本，无需下载。下载失败降级为纯标注，绝不阻断收消息。
//! - **出站**：`send_loop` 解析回复正文里的 `[:weixin-media ...]` 标记行（agent 用 `write`
//!   落盘后写入），走 `getuploadurl` → 本地 AES 加密 → CDN `/upload` → `sendmessage` 媒体项
//!   四步发出去，并把标记从可见文本剥离。先发文本、再发媒体（协议 §6.7 推荐拆两条）。
//!
//! `context_token` 是会话路由锚点（协议 §5.10）：入站时按 `from_user_id` 缓存，出站回传。

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use md5::Digest;
use rand::Rng;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use rclaw_core::{BusMessage, CHANNEL_WEIXIN};

use crate::{Channel, OutboundReceiver};

/// `base_info.channel_version`（业务 POST body 的协议版本字段）。
const CHANNEL_VERSION: &str = "0.1.0";

/// 微信通道配置。
#[derive(Debug, Clone)]
pub struct WeixinConfig {
    pub enabled: bool,
    pub token: String,
    /// 逗号分隔的允许来源（from_user_id）；为空表示放行全部。
    pub allow_from: String,
    pub api_base: String,
    pub poll_timeout_sec: u64,
    pub retry_sec: u64,
    pub backoff_sec: u64,
    pub max_failures: u32,
    pub send_chunk_bytes: usize,
    /// 媒体 CDN 基座（`https://novac2c.cdn.weixin.qq.com/c2c`）。
    pub cdn_base: String,
    /// 入站媒体落盘根目录（通常 `<agent_base>/media`）。
    /// 为空时禁用下载：入站仅注入标注、出站媒体发送仍可用（只要 agent 给出可读路径）。
    pub media_dir: PathBuf,
}

impl Default for WeixinConfig {
    fn default() -> Self {
        WeixinConfig {
            enabled: false,
            token: String::new(),
            allow_from: String::new(),
            api_base: "https://ilinkai.weixin.qq.com".into(),
            poll_timeout_sec: 35,
            retry_sec: 2,
            backoff_sec: 30,
            max_failures: 3,
            send_chunk_bytes: 2000,
            cdn_base: "https://novac2c.cdn.weixin.qq.com/c2c".into(),
            media_dir: PathBuf::new(),
        }
    }
}

/// 微信通道。
pub struct Weixin {
    cfg: Arc<WeixinConfig>,
    client: reqwest::Client,
    /// `from_user_id → context_token` 缓存（入站写、出站读，跨 poll/send 任务共享）。
    ///
    /// 用 `std::sync::Mutex`：临界区仅做同步 HashMap 读写、无 await，持锁极短；避免
    /// `tokio::sync::Mutex` 在此处的额外开销。`Arc<Mutex<_>>` 满足 `Send + Sync`。
    context_tokens: Arc<Mutex<HashMap<String, String>>>,
}

impl Weixin {
    pub fn new(cfg: WeixinConfig) -> Option<Self> {
        if !cfg.enabled || cfg.token.is_empty() {
            tracing::info!("微信通道未启用（enabled=false 或 token 为空）");
            return None;
        }
        let client = reqwest::Client::builder()
            // P2：poll_timeout_sec 来自 TOML 的 u64，极大值会让 `cfg.poll_timeout_sec + 15`
            // 在 debug 下 panic、release 下回绕成很小的超时（反而让长轮询几乎立即超时）。
            // 用 saturating_add 防溢出：极大值会得到一个非常大的超时（不会 panic、不会回绕成
            // 有害小值），正常配置（~35s）完全不受影响。
            .timeout(Duration::from_secs(cfg.poll_timeout_sec.saturating_add(15)))
            .build()
            .ok()?;
        Some(Weixin {
            cfg: Arc::new(cfg),
            client,
            context_tokens: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// 生成鉴权头（每次请求新生成 X-WECHAT-UIN）。
    fn headers(&self) -> Vec<(String, String)> {
        let uin = generate_uin();
        vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), format!("Bearer {}", self.cfg.token)),
            ("AuthorizationType".into(), "ilink_bot_token".into()),
            ("X-WECHAT-UIN".into(), uin),
        ]
    }

    fn allowed(&self, from: &str) -> bool {
        if from.is_empty() {
            return false;
        }
        if self.cfg.allow_from.is_empty() {
            return true;
        }
        self.cfg
            .allow_from
            .split(',')
            .map(|s| s.trim())
            .any(|s| s == from)
    }

    /// 读取（并克隆）某会话缓存的 `context_token`；未缓存返回空串。
    fn context_token(&self, chat_id: &str) -> String {
        match self.context_tokens.lock() {
            Ok(map) => map.get(chat_id).cloned().unwrap_or_default(),
            // 毒锁仅在持锁线程 panic 时出现；本临界区无 panic 路径，此处退化即可。
            Err(_) => String::new(),
        }
    }

    /// 缓存入站消息的 `context_token`，供后续出站回传。
    fn remember_context_token(&self, chat_id: &str, token: &str) {
        if token.is_empty() {
            return;
        }
        if let Ok(mut map) = self.context_tokens.lock() {
            map.insert(chat_id.to_string(), token.to_string());
        }
    }

    /// 一次 POST（JSON），返回 `(status, body)`。
    ///
    /// 返回 `Result`：只有在「HTTP 成功收到响应、且响应体是合法 JSON 对象」时才 `Ok`，
    /// 否则 `Err(())`。关键修复：旧实现用 `resp.json().await.unwrap_or(Value::Null)`，
    /// 把【非 JSON 的 2xx 响应】（例如服务端返回 HTML 错误页 / 纯文本）降级为
    /// `Value::Null`，随后 `poll_once` 读 `ret`/`errcode` 默认成 0，会把这种坏响应当成
    /// 「成功但空轮询」——真实故障被静默吞掉。现在 JSON 解析失败或根非对象都直接报错，
    /// 由上层走失败退避路径，下次重新拉取。
    async fn post(&self, path: &str, body: &Value) -> Result<(u16, Value), ()> {
        let url = format!("{}/{path}", self.cfg.api_base.trim_end_matches('/'));
        let mut req = self.client.post(&url);
        for (k, v) in self.headers() {
            req = req.header(k, v);
        }
        match req.json(body).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                match resp.json::<Value>().await {
                    Ok(val) if val.is_object() => Ok((status, val)),
                    Ok(val) => {
                        tracing::warn!("微信响应非 JSON 对象 {url}: {val}");
                        Err(())
                    }
                    Err(e) => {
                        tracing::warn!("微信响应 JSON 解析失败 {url}: {e}");
                        Err(())
                    }
                }
            }
            Err(e) => {
                tracing::warn!("微信请求失败 {url}: {e}");
                Err(())
            }
        }
    }

    /// 业务 POST：`post` + HTTP/业务码校验，成功返回响应对象。
    async fn business_post(&self, path: &str, body: &Value) -> Result<Value, ()> {
        let (status, val) = self.post(path, body).await?;
        if !(200..300).contains(&status) {
            tracing::warn!("微信 {path} HTTP {status}");
            return Err(());
        }
        if !is_business_ok(&val) {
            let (ret, errcode) = business_codes(&val);
            tracing::warn!("微信 {path} 业务失败 ret={ret} errcode={errcode}");
            return Err(());
        }
        Ok(val)
    }

    /// 长轮询一次：成功时把确认过的游标写回 `sync_buf`。
    ///
    /// 游标推进的时机非常关键：服务端返回的 `get_updates_buf` 必须在【本批消息全部成功
    /// 入队 inbound】之后才提交。若先推进游标、后投递，一旦中途投递失败（inbound 已关闭），
    /// 这批消息已被服务端「确认」却没交给 agent，且无法重发（下一轮会从新游标拉取）。
    /// 因此这里先把新游标【暂存】，全部 send 成功后再回写到 `sync_buf`；任一失败则返回
    /// `Err` 且不提交游标，让上层重试时重新拉取并投递这批消息。
    async fn poll_once(
        &self,
        sync_buf: &mut String,
        inbound: &mpsc::Sender<BusMessage>,
    ) -> Result<(), ()> {
        let body = json!({ "get_updates_buf": sync_buf.as_str() });
        // post 现在保证返回合法 JSON 对象；任何网络错误或非 JSON/非对象响应都走 Err。
        let (status, val) = self.post("ilink/bot/getupdates", &body).await?;
        if !(200..300).contains(&status) {
            tracing::warn!("微信 getupdates HTTP {status}");
            return Err(());
        }
        // 业务码校验：iLink 接口即使 HTTP 200，也会用 ret/errcode != 0 表示失败。
        // 这里与 [`send`](Self::send) 复用同一份 [`business_codes`] 判定，保证两条路径一致。
        let (ret, errcode) = business_codes(&val);
        if ret != 0 || errcode != 0 {
            tracing::warn!("微信 getupdates ret={ret} errcode={errcode}");
            return Err(());
        }
        // 1) 暂存新游标，先不提交。
        let staged_buf: Option<String> = val
            .get("get_updates_buf")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        // 2) 逐条入站消息：每条 await 入队，任一失败 → 整体 Err，不提交游标。
        if let Some(msgs) = val.get("msgs").and_then(|v| v.as_array()) {
            for m in msgs {
                self.handle_message(m, inbound).await?;
            }
        }
        // 3) 本批消息全部投递成功，才把新游标提交（写回 sync_buf）。
        if let Some(b) = staged_buf {
            *sync_buf = b;
        }
        Ok(())
    }

    /// 处理一条入站消息：放行校验 + 缓存 context_token + 抽取正文（含媒体）→ inbound。
    ///
    /// 返回 `Result`：投递成功 `Ok(())`；`inbound` 接收端全部关闭（通常进程退出）时
    /// 返回 `Err(())`，让 [`poll_once`](Self::poll_once) 据此放弃提交游标、整体重试。
    ///
    /// 用 `send().await`（而非 `try_send`）施加背压：队列满时阻塞等待而不是丢弃；
    /// 只有接收端关闭才会真的返回 Err——此时进程已在退出路径上，重试也无害（无消费者）。
    async fn handle_message(
        &self,
        msg: &Value,
        inbound: &mpsc::Sender<BusMessage>,
    ) -> Result<(), ()> {
        // 仅处理用户消息（message_type=1=USER；2=BOT 是自己发的回声，忽略）。
        let mtype = msg
            .get("message_type")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if mtype != 1 {
            return Ok(());
        }
        let from = match msg.get("from_user_id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s,
            _ => return Ok(()),
        };
        if !self.allowed(from) {
            tracing::info!("微信：忽略非白名单来源 {from}");
            return Ok(());
        }
        // 缓存 context_token，供出站回传（协议 §5.10：会话路由锚点）。
        if let Some(tk) = msg.get("context_token").and_then(|v| v.as_str()) {
            self.remember_context_token(from, tk);
        }
        let content = match self.extract_content(msg).await {
            Some(c) if !c.is_empty() => c,
            _ => return Ok(()),
        };
        // 投递：失败仅在 inbound 已关闭时发生，返回 Err 让上游不提交游标、重试本批。
        match inbound
            .send(BusMessage::new(CHANNEL_WEIXIN, from, content))
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                tracing::warn!("微信消息入队失败（inbound 已关闭，疑似进程退出）→ {from}: {e:?}");
                Err(())
            }
        }
    }

    /// 抽取一条入站消息的可处理正文：文本 + 媒体标注（媒体自动下载落盘）。
    ///
    /// 媒体下载失败或未配置 `media_dir` 时降级为纯标注，绝不因 CDN 故障阻断收消息。
    async fn extract_content(&self, msg: &Value) -> Option<String> {
        let (text, media) = classify_items(msg);
        let mut parts: Vec<String> = Vec::new();
        if !text.is_empty() {
            parts.push(text);
        }
        for m in media {
            parts.push(self.describe_media(&m).await);
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    }

    /// 把单个入站媒体引用转成给 agent 的标注文本（必要时下载落盘）。
    async fn describe_media(&self, m: &InboundMedia) -> String {
        // 语音：直接用服务端转写，不下载 SILK 原始流。
        if matches!(m.kind, MediaKind::Voice) {
            return format!("[语音] {}", m.voice_text.as_deref().unwrap_or("(无转写)"));
        }
        // 标题：有文件名才带 `: name`，否则只用类型名（避免「[图片: 图片]」这种冗余）。
        let label = m.kind.label();
        let title = match m.file_name.as_deref() {
            Some(n) if !n.is_empty() => format!("{label}: {n}"),
            _ => label.to_string(),
        };
        // 无 CDN 引用或未配置落盘目录 → 仅标注，不下载。
        if m.encrypt_query_param.is_empty() || self.cfg.media_dir.as_os_str().is_empty() {
            return format!("[{title}]（未能下载，仅通知）");
        }
        match self.fetch_media(m).await {
            Some(path) => {
                let p = path.to_string_lossy().replace('\\', "/");
                // read 提示只对「文件」有意义（图片/视频不是文本）。
                let hint = match m.kind {
                    MediaKind::File => "（可用 read 读取文本内容）",
                    _ => "",
                };
                format!("[{title}] 已下载到 {p}{hint}")
            }
            None => format!("[{title}]（下载失败，仅通知）"),
        }
    }

    /// 从 CDN 下载 + AES-128-ECB 解密媒体，落盘到 `media_dir/weixin/`，返回路径。
    async fn fetch_media(&self, m: &InboundMedia) -> Option<PathBuf> {
        let ciphertext = self.cdn_download(&m.encrypt_query_param).await?;
        // 解析 AES key：优先图片直给的 hex `aeskey`，其次 `media.aes_key`（双编码，见协议 §8.4）。
        let key = m
            .direct_aeskey
            .as_deref()
            .and_then(decode_hex_key)
            .or_else(|| m.aes_key_field.as_deref().and_then(decode_media_aes_key))?;
        let plaintext = aes_ecb_decrypt(&ciphertext, &key)?;
        // 落盘：`media_dir/weixin/<rand>_<safename>`。
        let dir = self.cfg.media_dir.join("weixin");
        if std::fs::create_dir_all(&dir).is_err() {
            tracing::warn!("微信：无法创建媒体目录 {}", dir.display());
            return None;
        }
        let safe = sanitize_filename(&m.file_name.clone().unwrap_or_else(|| m.kind.label().into()));
        let fname = format!("{:08x}_{safe}", rand::thread_rng().gen::<u32>());
        let path = dir.join(fname);
        match std::fs::write(&path, &plaintext) {
            Ok(()) => Some(path),
            Err(e) => {
                tracing::warn!("微信：媒体落盘失败 {}: {e}", path.display());
                None
            }
        }
    }

    /// `GET {cdn}/download?encrypted_query_param=...`，返回密文字节。
    async fn cdn_download(&self, encrypt_query_param: &str) -> Option<Vec<u8>> {
        let url = format!("{}/download", self.cfg.cdn_base.trim_end_matches('/'));
        let resp = self
            .client
            .get(&url)
            .query(&[("encrypted_query_param", encrypt_query_param)])
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!("微信 CDN download HTTP {}", resp.status());
            return None;
        }
        let bytes = resp.bytes().await.ok()?;
        Some(bytes.to_vec())
    }

    /// 发送一条文本回复（自动分段），每分片内有限重试。
    ///
    /// **分片级重试**（修复 P2）：旧实现失败后由 send_loop 整体重试，若第 1 分片成功、
    /// 第 2 分片失败，重试会从第 1 分片重新发送，导致用户收到重复内容。
    /// 现在把重试下移到分片层：每个分片独立重试最多 `max_failures` 次，
    /// 成功过的分片绝不重发。某分片最终仍失败则整条消息标记失败。
    async fn send(&self, to_user_id: &str, text: &str) -> Result<(), ()> {
        use tokio::time::sleep;

        let ctx = self.context_token(to_user_id);
        for chunk in utf8_chunks(text, self.cfg.send_chunk_bytes) {
            let body = build_send_body(to_user_id, chunk, &ctx);
            let mut chunk_ok = false;

            for attempt in 1..=self.cfg.max_failures {
                match self.post("ilink/bot/sendmessage", &body).await {
                    Ok((status, val)) if (200..300).contains(&status) && is_business_ok(&val) => {
                        chunk_ok = true;
                        break;
                    }
                    Ok((status, val)) => {
                        let (ret, errcode) = business_codes(&val);
                        let reason = business_reason(&val).unwrap_or("-");
                        // errcode/ret 之外，iLink 往往在响应体里带人话原因（errmsg/msg），
                        // 业务失败时一并落盘，便于定性（如频率超限 / 会话窗口关闭）。
                        tracing::warn!(
                            "微信 sendmessage 业务失败 → {to_user_id}: HTTP={status} ret={ret} errcode={errcode} \
                             reason={reason} (第 {attempt}/{max} 次)",
                            max = self.cfg.max_failures,
                        );
                    }
                    Err(()) => {
                        tracing::warn!(
                            "微信 sendmessage 请求失败 → {to_user_id} \
                             (第 {attempt}/{max} 次)",
                            max = self.cfg.max_failures,
                        );
                    }
                }
                if attempt < self.cfg.max_failures {
                    sleep(Duration::from_secs(self.cfg.retry_sec)).await;
                }
            }

            if !chunk_ok {
                return Err(());
            }
        }
        Ok(())
    }

    /// 发送一个媒体附件：`getuploadurl` → AES 加密 → CDN `/upload` → `sendmessage` 媒体项。
    ///
    /// 协议 §8.2 的四步流程。上传（CDN）不重试（§11：CDN 4xx 不要盲目重试）；
    /// 最后的 `sendmessage` 有限重试（与文本发同一退避策略）。
    async fn send_media(&self, to_user_id: &str, intent: &MediaIntent) -> Result<(), ()> {
        // 安全校验：路径必须落在 media_dir 沙箱内（防 agent 经标记读取/外发任意文件，
        // 例如 /etc/passwd 或 agent/../secret）。media_dir 为空时一律拒绝（防空前缀放行）。
        let safe_path = match resolve_media_path(&self.cfg.media_dir, &intent.path) {
            Some(path) => path,
            None => {
                tracing::warn!(
                    "微信媒体发送：路径不在 media_dir 沙箱内，拒绝: {}",
                    intent.path
                );
                return Err(());
            }
        };
        // 0) 读文件。
        let plain = match std::fs::read(&safe_path) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("微信媒体发送：读取失败 {}: {e}", intent.path);
                return Err(());
            }
        };
        let media_type = intent.kind.upload_media_type();

        // 1) 本地加密 + 摘要。
        let key = rand::thread_rng().gen::<[u8; 16]>();
        let aeskey_hex = hex::encode(key); // getuploadurl 与 CDNMedia 都用 hex 形态
        let ciphertext = aes_ecb_encrypt(&plain, &key);
        let rawsize = plain.len();
        let rawfilemd5 = hex::encode(md5::Md5::digest(&plain));
        let filesize = ciphertext.len();
        let filekey = hex::encode(rand::thread_rng().gen::<[u8; 16]>());

        // 2) 申请上传地址。
        let req = json!({
            "filekey": filekey,
            "media_type": media_type,
            "to_user_id": to_user_id,
            "rawsize": rawsize,
            "rawfilemd5": rawfilemd5,
            "filesize": filesize,
            "no_need_thumb": true,
            "aeskey": aeskey_hex,
            "base_info": { "channel_version": CHANNEL_VERSION }
        });
        let resp = self.business_post("ilink/bot/getuploadurl", &req).await?;
        let upload_param = match resp.get("upload_param").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => {
                tracing::warn!("微信 getuploadurl 未返回 upload_param: {resp}");
                return Err(());
            }
        };

        // 3) 上传密文到 CDN，拿回最终的 encrypt_query_param。
        let encrypt_query_param = match self.cdn_upload(&upload_param, &filekey, ciphertext).await {
            Some(p) => p,
            None => return Err(()),
        };

        // 4) 构造媒体 item 并经 sendmessage 发出（带 context_token）。
        let file_name = path_file_name(&intent.path);
        let aes_key_field = base64::engine::general_purpose::STANDARD.encode(aeskey_hex.as_bytes());
        let item = build_media_item(
            intent.kind,
            &encrypt_query_param,
            &aes_key_field,
            filesize,
            &file_name,
        );
        let ctx = self.context_token(to_user_id);

        for attempt in 1..=self.cfg.max_failures {
            let body = build_send_body_with_item(to_user_id, &item, &ctx);
            match self.post("ilink/bot/sendmessage", &body).await {
                Ok((status, val)) if (200..300).contains(&status) && is_business_ok(&val) => {
                    return Ok(());
                }
                Ok((status, val)) => {
                    let (ret, errcode) = business_codes(&val);
                    let reason = business_reason(&val).unwrap_or("-");
                    tracing::warn!(
                        "微信媒体 sendmessage 业务失败 → {to_user_id}: HTTP={status} ret={ret} errcode={errcode} \
                         reason={reason} (第 {attempt}/{max} 次)",
                        max = self.cfg.max_failures,
                    );
                }
                Err(()) => {
                    tracing::warn!(
                        "微信媒体 sendmessage 请求失败 → {to_user_id} (第 {attempt}/{max} 次)",
                        max = self.cfg.max_failures
                    );
                }
            }
            if attempt < self.cfg.max_failures {
                tokio::time::sleep(Duration::from_secs(self.cfg.retry_sec)).await;
            }
        }
        Err(())
    }

    /// 上传密文到 CDN，返回响应头里的 `x-encrypted-param`（即最终 `encrypt_query_param`）。
    async fn cdn_upload(
        &self,
        upload_param: &str,
        filekey: &str,
        ciphertext: Vec<u8>,
    ) -> Option<String> {
        let url = format!("{}/upload", self.cfg.cdn_base.trim_end_matches('/'));
        let resp = self
            .client
            .post(&url)
            .query(&[
                ("encrypted_query_param", upload_param),
                ("filekey", filekey),
            ])
            .header("Content-Type", "application/octet-stream")
            .body(ciphertext)
            .send()
            .await
            .ok()?;
        let status = resp.status();
        if !status.is_success() {
            tracing::warn!("微信 CDN upload HTTP {status}");
            return None;
        }
        let param = resp
            .headers()
            .get("x-encrypted-param")?
            .to_str()
            .ok()?
            .to_string();
        if param.is_empty() {
            tracing::warn!("微信 CDN upload 成功但缺 x-encrypted-param");
            return None;
        }
        Some(param)
    }

    /// 长轮询主循环（带失败退避）。
    async fn poll_loop(self: Arc<Self>, inbound: mpsc::Sender<BusMessage>) {
        let mut sync_buf = String::new();
        let mut failures: u32 = 0;
        loop {
            if self.poll_once(&mut sync_buf, &inbound).await.is_ok() {
                failures = 0;
            } else {
                failures += 1;
                if failures >= self.cfg.max_failures {
                    tracing::warn!(
                        "微信：连续 {failures} 次失败，退避 {}s",
                        self.cfg.backoff_sec
                    );
                    tokio::time::sleep(Duration::from_secs(self.cfg.backoff_sec)).await;
                    failures = 0;
                } else {
                    tokio::time::sleep(Duration::from_secs(self.cfg.retry_sec)).await;
                }
            }
        }
    }

    /// 发送循环：消费 outbound 回复发回微信。
    ///
    /// 解析正文里的 `[:weixin-media ...]` 标记（媒体发送意图），先发剩余文本、再逐个发媒体，
    /// 标记从可见文本剥离。连续消息失败达阈值后触发退避（`backoff_sec`），给服务端恢复窗口；
    /// 最终失败记录 dead-letter 日志。
    async fn send_loop(self: Arc<Self>, mut outbound: OutboundReceiver) {
        let mut send_failures: u32 = 0;

        while let Some(msg) = outbound.recv().await {
            let (intents, text) = parse_media_markers(&msg.content);
            let mut failed = false;

            // 1) 先发文本（若非空）。
            if !text.trim().is_empty() && self.send(&msg.chat_id, &text).await.is_err() {
                failed = true;
            }
            // 2) 再逐个发媒体（可选 caption 作为前置文本）。
            for intent in &intents {
                if let Some(cap) = intent.caption.as_deref() {
                    if !cap.trim().is_empty() && self.send(&msg.chat_id, cap).await.is_err() {
                        failed = true;
                    }
                }
                if self.send_media(&msg.chat_id, intent).await.is_err() {
                    failed = true;
                }
            }

            if failed {
                send_failures += 1;
                tracing::warn!(
                    "微信回复（部分）最终失败 → {}: {}",
                    msg.chat_id,
                    msg.content.chars().take(80).collect::<String>(),
                );
                if send_failures >= self.cfg.max_failures {
                    tracing::warn!(
                        "微信发送：连续 {send_failures} 次失败，退避 {}s",
                        self.cfg.backoff_sec,
                    );
                    tokio::time::sleep(Duration::from_secs(self.cfg.backoff_sec)).await;
                    send_failures = 0;
                }
            } else {
                send_failures = 0;
            }
        }
    }
}

#[async_trait]
impl Channel for Weixin {
    fn name(&self) -> &str {
        CHANNEL_WEIXIN
    }

    fn capabilities(&self) -> rclaw_core::ChannelCaps {
        rclaw_core::ChannelCaps {
            name: CHANNEL_WEIXIN.to_string(),
            // 微信可收发媒体；工作状态推送对微信用户无价值（会话不常驻展示）。
            media: true,
            working_status: false,
            intermediate_output: false,
            reasoning_stream: false,
        }
    }

    async fn run(self, inbound: mpsc::Sender<BusMessage>, outbound: OutboundReceiver) {
        let me = Arc::new(self);
        let poller = me.clone();
        let sender = me.clone();
        // 两个并发循环：收消息 / 发回复。
        let _ = tokio::join!(poller.poll_loop(inbound), sender.send_loop(outbound),);
    }
}

mod media;
use media::*;

#[cfg(test)]
mod tests;
