//! NapCatQQ 通道（OneBot 11 over WebSocket，ws-client 模式）。
//!
//! 流程（ws-client：RClaw 主动连接 NapCat 的 WS 服务）：
//! - 收消息：解析 OneBot 11 `message` 事件 → 抽取纯文本 + 媒体引用 → 媒体下载落盘 → 投递 inbound；
//! - 发回复：消费 outbound，按 `chat_id` 前缀（`group:`/`user:`）调用
//!   `send_group_msg` / `send_private_msg`，用 `echo` 字段做请求-响应匹配。
//!
//! # 媒体收发
//!
//! - **入站**：HTTP/HTTPS 直链优先由 RClaw 以流式 HTTP 写盘；NapCat Stream API 用于
//!   file_id / 协议端路径 / HTTP 失败兜底。image 段优先使用自带 URL，必要时 `get_image`
//!   刷新 URL，HTTP 下载失败再用 Stream 兜底；record(语音) 优先 `get_record` 转成 mp3。
//!   落盘到 `media_dir/napcat/`，正文注入
//!   `[图片: 名] 已下载到 <path>` 等标注，agent 即可用 `read` 读取文本类文件。
//!   下载失败或未配置 `media_dir` 时降级为纯标注，绝不阻断收消息。
//! - **出站**：`send_loop` 解析回复正文里的 `[:napcat-media ...]` 标记行（agent 用 `write`
//!   落盘后写入），优先经 NapCat Stream API 上传到协议端临时文件，再用返回的 `file_path`
//!   发送媒体或文件；Stream 失败时回退 `base64://` / `file:///` 发送，并把标记从可见文本剥离。
//!   路径必须在 `media_dir` 沙箱内（防 agent 经标记外发任意文件，如 `/etc/passwd`）。
//!
//! NapCat 官方参考：
//! - 文件处理与 Stream API：https://napneko.github.io/develop/file
//! - API 兼容情况：https://napneko.github.io/develop/api
//! - 发送消息接口：
//!   - `send_group_msg`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/group/SendGroupMsg.ts
//!   - `send_private_msg`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/msg/SendPrivateMsg.ts
//! - 媒体/文件获取接口：
//!   - `get_record`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/file/GetRecord.ts
//!   - `get_image`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/file/GetImage.ts
//!   - `get_file`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/file/GetFile.ts
//!   - `get_group_file_url`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/file/GetGroupFileUrl.ts
//!   - `get_private_file_url`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/file/GetPrivateFileUrl.ts
//! - 文件上传接口：
//!   - `upload_group_file`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/go-cqhttp/UploadGroupFile.ts
//!   - `upload_private_file`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/go-cqhttp/UploadPrivateFile.ts
//! - 心跳接口：
//!   - `get_status`：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/system/GetStatus.ts
//! - 群文件上传通知（OneBot 11 `group_upload`）：https://github.com/botuniverse/onebot-11/blob/master/event/notice.md
//! - `download_file_stream` 实现：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/stream/DownloadFileStream.ts
//! - `upload_file_stream` 实现：https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/stream/UploadFileStream.ts
//!
//! 鉴权：`access_token` 同时以 `?access_token=` 查询参数与 `Authorization: Bearer` 头携带
//! （OneBot 11 标准允许二者之一，双发最大化兼容不同实现）。NapCat 要求 URL 以 `/` 结尾。
//!
//! chat_id 约定：私聊 `user:<qq>`，群聊 `group:<group_id>`。出站据此选择 API。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use base64::Engine;
use futures::stream::FuturesUnordered;
use futures::{SinkExt, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tracing::{debug, error, info, warn};

use rclaw_core::{BusMessage, ToolAccess, CHANNEL_NAPCAT};

use crate::{Channel, OutboundReceiver};

/// 单条出站文本的最大字节数（按 UTF-8 字符边界安全分段）的缺省值。
///
/// 运行期实际阈值由 [`NapCatConfig::send_chunk_bytes`] 决定（默认与本常量一致）。超过该阈值的
/// 回复改走合并转发卡片（见 [`NapCat::send_outbound`]）。
const SEND_CHUNK_BYTES: usize = 4500;
/// 合并转发卡片内单个 node 的目标最大字节数；按段落/句末边界切分，尽量不切断整句。
const FORWARD_NODE_BYTES: usize = 3000;
/// 合并转发需要先生成各 node 再发送卡片，通常比普通消息耗时更长。
const FORWARD_API_TIMEOUT: Duration = Duration::from_secs(120);
/// API 请求-响应匹配的最长等待（防止 echo 永不返回时卡住发送任务）。
const API_TIMEOUT: Duration = Duration::from_secs(30);
/// 文本分段遇到可恢复发送超时时的额外重试次数（不含首次）。
///
/// 同时覆盖 NapCat `sendMsg` 服务层 NTEvent 超时和客户端等待 API 回执超时。两者的实际
/// 投递结果都不确定；重试可挽回未投递的分段，但首次请求若延迟成功也可能造成重复。
/// 响应通道关闭不在旧连接上重试。
const TEXT_CHUNK_RESEND_ATTEMPTS: usize = 1;
/// 文本分段重试前的退避：降低连续调用对 NapCat 内核的压力；无法消除重复投递风险。
const TEXT_CHUNK_RESEND_BACKOFF: Duration = Duration::from_millis(500);
/// 文件投递（`upload_private_file` / `upload_group_file`）的单次等待。
///
/// 文件类消息经 NT 内核 `sendMsg` 投递远慢于纯文本（大附件尤甚），通用的 [`API_TIMEOUT`]
/// (30s) 可能把正常的慢投递误判为超时、进而被「投递状态不确定」分支直接丢弃。这里放宽到
/// 120s，与合并转发卡片（[`FORWARD_API_TIMEOUT`]）对齐。
const FILE_DELIVERY_TIMEOUT: Duration = Duration::from_secs(120);
/// 媒体下载（含 get_file 换链 + 实际下载）的单次超时。
const MEDIA_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// Stream API 普通请求响应超时，覆盖 upload_file_stream 合并大文件的耗时。
const STREAM_API_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Stream API 下载时，两帧之间允许的最大空闲时间。总传输可超过该值，只要持续有分块。
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// Stream API 上传/下载分块大小。NapCat 官方示例默认 64KB，沿用该值降低单帧压力。
const STREAM_CHUNK_BYTES: usize = 64 * 1024;
/// Stream API 上传后 NapCat 临时文件保留时间，留足后续 upload_group_file/private_file 使用。
const STREAM_FILE_RETENTION_MS: u64 = 30 * 60 * 1000;
/// 出站媒体在 Stream 失败后，以内联 base64 回退发送的最大原始字节数。
///
/// 这样 NapCat 与 RClaw 不共享本地文件系统且 Stream 不可用时，小/常规媒体仍有兼容回退。
/// 100MB 对齐 NapCat 文档里普通视频上限；更大的文件改走 upload_file_stream，避免把超大文件整体塞进 WS JSON。
const OUTBOUND_BASE64_MAX_BYTES: u64 = 100 * 1024 * 1024;
/// 单连接最多缓存多少 message_id 做去重；超出后淘汰一半。
const DEDUP_CAP: usize = 10_000;
/// 单连接最多保留多少条已落盘媒体索引，用于群聊 `at_only` 的静默接收后复用。
const MEDIA_CACHE_CAP: usize = 10_000;
/// 单进程最多缓存多少条群文本索引；磁盘 JSONL 仍作为冷查来源。
const TEXT_CACHE_CAP: usize = 10_000;
/// 同一会话等待前一条出站发送完成时，最多缓冲的后续消息数。
///
/// 上限设为 64 条，允许 API 短暂变慢时积压；达到上限时记录可观测错误，而不是让
/// `VecDeque` 在 API 无响应时无限增长。
const OUTBOUND_CHAT_QUEUE_CAP: usize = 64;
/// 群文本引用索引单条最大保存字节数，避免超长文本把 JSONL 分片撑得过大。
const TEXT_INDEX_MAX_BYTES: usize = 256 * 1024;
/// 重连指数退避上限（秒）。
const MAX_RECONNECT_DELAY: u64 = 60;

/// 单个 chat 的出站发送。主循环继续处理 API 响应，避免发送任务等待响应时阻塞 WS 读循环。
/// future 完成时返回 chat_id，以便启动该会话的下一条待发消息。
type OutboundSend = Pin<Box<dyn Future<Output = String> + Send>>;

/// NapCatQQ 通道配置。
#[derive(Debug, Clone)]
pub struct NapCatConfig {
    pub enabled: bool,
    /// NapCat WebSocket 地址，如 `ws://127.0.0.1:3001`。
    pub ws_url: String,
    /// 与 NapCat 配置一致的 access_token；为空表示不鉴权（仅本地可信网络建议留空）。
    pub access_token: String,
    /// 逗号分隔的允许用户 QQ；空=放行全部，支持 `*`。
    pub allow_from: String,
    /// 逗号分隔的允许群号；空=放行全部群，支持 `*`。
    pub allow_groups: String,
    /// 逗号分隔的拉黑用户 QQ；优先级高于 allow_from，支持 `*`。
    pub block_from: String,
    /// 逗号分隔的「max 权限」用户 QQ：这些 QQ 的私聊消息可让 read/write/edit/
    /// list/grep/shell 操作 agent 目录外任意路径；其它消息仍被限制在 agent 目录内。
    /// 空 = 无人拥有 max 权限；支持 `*`（显式放行全部，等价所有 QQ 都是 max）。
    pub max_from: String,
    /// 群消息响应模式：`none`（不响应）/ `at_only`（仅 @机器人 时）/ `all`（全部，默认）。
    pub group_response_mode: String,
    /// 心跳间隔（秒）：周期性发送 get_status 探活，发送失败即认定连接已死并重连。
    pub heartbeat_interval_sec: u64,
    /// 重连基础退避（秒），每次失败翻倍，上限 60。
    pub reconnect_delay_sec: u64,
    /// 单条出站文本的最大字节数。≤ 该值按一条文本消息发送；> 该值改走合并转发卡片。
    pub send_chunk_bytes: usize,
    /// 入站媒体落盘根目录（通常 `<agent_base>/media`）。
    /// 为空时禁用下载：入站仅注入标注、出站媒体发送仍可用（只要 agent 给出沙箱内可读路径）。
    pub media_dir: PathBuf,
}

impl Default for NapCatConfig {
    fn default() -> Self {
        NapCatConfig {
            enabled: false,
            ws_url: String::new(),
            access_token: String::new(),
            allow_from: String::new(),
            allow_groups: String::new(),
            block_from: String::new(),
            max_from: String::new(),
            group_response_mode: "all".into(),
            heartbeat_interval_sec: 30,
            reconnect_delay_sec: 5,
            send_chunk_bytes: SEND_CHUNK_BYTES,
            media_dir: PathBuf::new(),
        }
    }
}

/// 请求-响应匹配表：echo → oneshot 发送端。读循环收到响应时按 echo 取出完成等待。
type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;
/// Stream API 匹配表：echo → 分块发送端。流式响应会复用 echo 返回多帧，不能走 oneshot。
type StreamPendingMap = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;
/// message_id 去重集合（NapCat 在重连/重试时可能重复上报同一消息）。
type DedupSet = Arc<Mutex<HashSet<i64>>>;
type MediaInflightMap = Mutex<HashMap<String, Vec<oneshot::Sender<Option<CachedMedia>>>>>;

/// `call_api_with_timeout` 在任何路径（包括 future 因断线被取消）结束时清理自身 pending 项。
struct PendingRequest {
    pending: PendingMap,
    echo: String,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&self.echo);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedMedia {
    kind: MediaKind,
    name: Option<String>,
    path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedText {
    message_id: i64,
    group_id: String,
    user_id: String,
    text: String,
    #[serde(default)]
    truncated: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct MediaCache {
    by_key: HashMap<String, CachedMedia>,
    by_message_id: HashMap<String, Vec<CachedMedia>>,
}

enum MediaDownloadSlot {
    Leader,
    Follower(oneshot::Receiver<Option<CachedMedia>>),
    Untracked,
}

/// NapCatQQ 通道。
pub struct NapCat {
    cfg: Arc<NapCatConfig>,
    request_id: AtomicU64,
    /// 媒体下载用 HTTP 客户端（与 WS 通道独立）。
    client: reqwest::Client,
    media_cache: Mutex<MediaCache>,
    media_inflight: MediaInflightMap,
    text_cache: Mutex<HashMap<String, CachedText>>,
}

impl NapCat {
    pub fn new(mut cfg: NapCatConfig) -> Option<Self> {
        if !cfg.enabled || cfg.ws_url.trim().is_empty() {
            info!("NapCatQQ 通道未启用（enabled=false 或 ws_url 为空）");
            return None;
        }
        if cfg.send_chunk_bytes == 0 {
            cfg.send_chunk_bytes = SEND_CHUNK_BYTES;
        }
        let client = reqwest::Client::builder()
            .connect_timeout(MEDIA_DOWNLOAD_TIMEOUT)
            .read_timeout(MEDIA_DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|e| warn!("NapCat reqwest client 构建失败: {e}"))
            .ok()?;
        let media_cache = load_media_cache(&cfg.media_dir);
        Some(NapCat {
            cfg: Arc::new(cfg),
            request_id: AtomicU64::new(0),
            client,
            media_cache: Mutex::new(media_cache),
            media_inflight: Mutex::new(HashMap::new()),
            text_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 生成唯一的 echo（用于匹配请求与响应）。
    fn next_echo(&self) -> String {
        format!("rclaw-{}", self.request_id.fetch_add(1, Ordering::SeqCst))
    }

    /// 逗号分隔白名单判定，支持 `*`。空 list 视为放行全部。
    fn allow_listed(list: &str, id: &str) -> bool {
        if list.is_empty() {
            return true;
        }
        list.split(',')
            .map(|s| s.trim())
            .any(|s| s == id || s == "*")
    }

    /// 逗号分隔黑名单命中判定，支持 `*`。空 list 视为无人被拉黑。
    fn is_blocked(list: &str, id: &str) -> bool {
        if list.is_empty() {
            return false;
        }
        list.split(',')
            .map(|s| s.trim())
            .any(|s| s == id || s == "*")
    }

    fn user_allowed(&self, user_id: &str) -> bool {
        if Self::is_blocked(&self.cfg.block_from, user_id) {
            return false;
        }
        Self::allow_listed(&self.cfg.allow_from, user_id)
    }

    /// `max_from` 命中判定。与 `allow_from`「空=放行全部」相反——max 是高权限，
    /// **空 = 无人拥有 max 权限**；支持 `*`（显式放行全部 QQ 为 max）。
    fn is_max_user(list: &str, user_id: &str) -> bool {
        if list.trim().is_empty() {
            return false;
        }
        list.split(',')
            .map(|s| s.trim())
            .any(|s| s == user_id || s == "*")
    }

    /// 当前消息应使用的工具沙箱权限。
    ///
    /// 群聊会话由全体成员共享，且运行中的 turn 会接收后续成员的插队消息；因此群消息
    /// 必须始终受限，避免普通成员借用 max 用户开启的 turn。只有私聊可按发送者 QQ 提升。
    fn tool_access_for(&self, user_id: &str, is_group: bool) -> ToolAccess {
        if !is_group && Self::is_max_user(&self.cfg.max_from, user_id) {
            ToolAccess::Unrestricted
        } else {
            ToolAccess::Sandboxed
        }
    }

    fn group_allowed(&self, group_id: &str) -> bool {
        Self::allow_listed(&self.cfg.allow_groups, group_id)
    }

    /// 记录 message_id 去重；返回 true 表示「已见过」（应跳过）。
    fn mark_seen(dedup: &DedupSet, msg_id: i64) -> bool {
        // message_id 缺失（0）的事件不去重：否则多条缺失 ID 的消息会被当作同一条只处理首条，
        // 静默丢消息。OneBot 11 正常都带 message_id，这里只是防御。
        if msg_id == 0 {
            return false;
        }
        let Ok(mut set) = dedup.lock() else {
            // 毒锁仅在持锁线程 panic 时出现；本临界区无 panic 路径。取不到锁时宁可放行（不跳过）。
            return false;
        };
        if set.contains(&msg_id) {
            return true;
        }
        // 容量保护：达到上限淘汰一半（HashSet 无序，随机淘汰一半即可，去重只需概率有效）。
        if set.len() >= DEDUP_CAP {
            let to_remove = set.len() / 2;
            let keys: Vec<i64> = set.iter().take(to_remove).copied().collect();
            for k in keys {
                set.remove(&k);
            }
        }
        set.insert(msg_id);
        false
    }

    /// 调用一次 OneBot API 并等待响应，成功返回 `data`。
    ///
    /// 注册 echo → 经 `api_tx` 把请求投给主循环写出 → 等待读循环按 echo 回送的响应。
    /// 超时或业务失败时清理 pending 项避免泄漏。
    async fn call_api(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        action: &str,
        params: Value,
    ) -> Result<Value, String> {
        self.call_api_with_timeout(api_tx, pending, action, params, API_TIMEOUT)
            .await
    }

    async fn call_api_with_timeout(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        action: &str,
        params: Value,
        wait: Duration,
    ) -> Result<Value, String> {
        let echo = self.next_echo();
        let req = json!({ "action": action, "params": params, "echo": echo.clone() });
        let text = serde_json::to_string(&req).map_err(|e| e.to_string())?;

        let (tx, rx) = oneshot::channel();
        {
            let mut g = pending.lock().map_err(|e| e.to_string())?;
            g.insert(echo.clone(), tx);
        }
        let _pending_request = PendingRequest {
            pending: pending.clone(),
            echo: echo.clone(),
        };

        if let Err(e) = api_tx.send(text).await {
            return Err(format!("api 通道已关闭: {e}"));
        }

        match timeout(wait, rx).await {
            Ok(Ok(resp)) => {
                let ok = resp.get("status").and_then(|v| v.as_str()) == Some("ok")
                    && resp.get("retcode").and_then(|v| v.as_i64()) == Some(0);
                if ok {
                    Ok(resp.get("data").cloned().unwrap_or(Value::Null))
                } else {
                    let wording = resp
                        .get("wording")
                        .and_then(|v| v.as_str())
                        .or_else(|| resp.get("message").and_then(|v| v.as_str()))
                        .unwrap_or("unknown error");
                    Err(wording.to_string())
                }
            }
            // 等待端被丢弃（连接重置时 pending 项可能已被移除）。
            Ok(Err(_)) => Err("api 响应通道关闭".into()),
            Err(_) => Err("api 超时".into()),
        }
    }

    /// 处理一条 WS 文本帧：事件 → inbound；API 响应 → 唤醒对应 pending。
    async fn handle_ws_text(
        self: Arc<Self>,
        text: &str,
        inbound: &mpsc::Sender<BusMessage>,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        stream_pending: &StreamPendingMap,
        dedup: &DedupSet,
    ) {
        let val: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                // NapCat 正常只发 JSON；非 JSON（如服务端调试输出）忽略即可。
                warn!("NapCat 收到非 JSON 文本，忽略: {e}");
                return;
            }
        };
        let post_type = val.get("post_type").and_then(|v| v.as_str()).unwrap_or("");
        if post_type.is_empty() {
            // 无 post_type → API 响应。按 echo 路由到等待中的发送方。
            let echo = echo_str(val.get("echo"));
            if let Some(echo) = echo {
                if is_stream_packet(&val) {
                    let stream_tx = stream_pending
                        .lock()
                        .ok()
                        .and_then(|m| m.get(&echo).cloned());
                    if let Some(tx) = stream_tx {
                        if tx.send(val).await.is_err() {
                            if let Ok(mut m) = stream_pending.lock() {
                                m.remove(&echo);
                            }
                        }
                        return;
                    }
                }

                let waiter = pending.lock().ok().and_then(|mut m| m.remove(&echo));
                if let Some(tx) = waiter {
                    // 无论成功失败都把响应交给调用方判定（失败也由其记录日志）。
                    let _ = tx.send(val);
                }
                // 未找到等待方（如心跳 echo）→ 静默丢弃。
            }
            return;
        }
        if post_type == "notice" {
            self.handle_notice_event(&val, inbound, api_tx, pending, stream_pending)
                .await;
            return;
        }
        if post_type != "message" {
            // request / meta_event / message_sent 忽略，保持通道最小。
            // （message_sent 是自己发出的回声，忽略可避免自回复循环。）
            return;
        }
        self.handle_message_event(&val, inbound, api_tx, pending, stream_pending, dedup)
            .await;
    }

    async fn handle_notice_event(
        self: Arc<Self>,
        val: &Value,
        inbound: &mpsc::Sender<BusMessage>,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        stream_pending: &StreamPendingMap,
    ) {
        let notice_type = val
            .get("notice_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if notice_type != "group_upload" {
            return;
        }
        let group_id = id_str(val.get("group_id"));
        let user_id = id_str(val.get("user_id"));
        if group_id.is_empty() || user_id.is_empty() {
            return;
        }
        if !self.group_allowed(&group_id) || !self.user_allowed(&user_id) {
            return;
        }

        let file = val.get("file").unwrap_or(&Value::Null);
        let media = group_upload_media_ref(file);
        let Some(media) = media else {
            return;
        };

        let mut content = String::new();
        let title = media
            .name
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(|n| format!(": {n}"))
            .unwrap_or_default();
        if self.cfg.media_dir.as_os_str().is_empty() {
            content.push_str(&format!("[文件{title}]（未配置下载目录，仅通知）"));
        } else {
            match self
                .cached_or_download_media(
                    api_tx,
                    pending,
                    stream_pending,
                    &media,
                    MediaDownloadContext {
                        target: "group",
                        target_id: &group_id,
                        message_id: 0,
                    },
                )
                .await
            {
                Some(cached) => content.push_str(&format_cached_media_note(&cached, "")),
                None => content.push_str(&format!("[文件{title}]（下载失败，仅通知）")),
            }
        }

        if !group_message_reaches_llm(&self.cfg.group_response_mode, false, false) {
            self.save_suppressed_group_content(&group_id, &user_id, content.trim());
            return;
        }

        let access = self.tool_access_for(&user_id, true);
        let chat_id = format!("group:{group_id}");
        if let Err(e) = inbound
            .send(
                BusMessage::new(CHANNEL_NAPCAT, &chat_id, content.trim()).with_tool_access(access),
            )
            .await
        {
            warn!("NapCat 群文件通知入队失败（inbound 已关闭）→ {chat_id}: {e:?}");
        }
    }

    /// 处理一条 message 事件：放行/去重/@判定 → 抽取文本与媒体 → 下载媒体 → inbound。
    async fn handle_message_event(
        self: Arc<Self>,
        val: &Value,
        inbound: &mpsc::Sender<BusMessage>,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        stream_pending: &StreamPendingMap,
        dedup: &DedupSet,
    ) {
        let mtype = val
            .get("message_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let user_id = id_str(val.get("user_id"));
        if user_id.is_empty() {
            return;
        }
        let self_id = id_str(val.get("self_id"));
        let msg_id = val.get("message_id").and_then(|v| v.as_i64()).unwrap_or(0);
        let event_time = value_i64(val.get("time")).unwrap_or_else(current_unix_secs);

        if Self::mark_seen(dedup, msg_id) {
            return;
        }
        if !self.user_allowed(&user_id) {
            return;
        }

        let is_group = mtype == "group";
        let group_id = if is_group {
            let g = id_str(val.get("group_id"));
            if g.is_empty() {
                return; // 群消息却没有 group_id：数据异常，丢弃。
            }
            if !self.group_allowed(&g) {
                return;
            }
            g
        } else {
            String::new()
        };

        // 抽取文本与媒体引用。
        let ext = extract_message(val.get("message").unwrap_or(&Value::Null), &self_id);
        if is_group {
            self.remember_group_text_message(msg_id, &group_id, &user_id, &ext.text, event_time);
        }

        let send_to_llm = !is_group
            || group_message_reaches_llm(&self.cfg.group_response_mode, ext.at_me, ext.at_all);

        // 拼接正文：文本 + 每个媒体的下载标注。
        let mut content = ext.text;
        if is_group {
            let replied_texts = self.cached_text_for_replies(&group_id, &ext.reply_ids);
            for cached in &replied_texts {
                content.push_str(&format!("\n{}", format_cached_text_note(cached)));
            }
            let replied_media = self.cached_media_for_replies("group", &group_id, &ext.reply_ids);
            for cached in &replied_media {
                content.push_str(&format!("\n{}", format_cached_media_note(cached, "引用")));
                self.remember_cached_media_for_message("group", &group_id, msg_id, cached.clone());
            }
        }
        let (media_target, media_target_id) = if is_group {
            ("group", group_id.as_str())
        } else {
            ("private", user_id.as_str())
        };
        for m in &ext.media {
            let label = m.kind.label();
            let title = m
                .name
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|n| format!(": {n}"))
                .unwrap_or_default();
            if self.cfg.media_dir.as_os_str().is_empty() {
                content.push_str(&format!("\n[{label}{title}]（未配置下载目录，仅通知）"));
            } else {
                match self
                    .cached_or_download_media(
                        api_tx,
                        pending,
                        stream_pending,
                        m,
                        MediaDownloadContext {
                            target: media_target,
                            target_id: media_target_id,
                            message_id: msg_id,
                        },
                    )
                    .await
                {
                    Some(cached) => {
                        content.push_str(&format!("\n{}", format_cached_media_note(&cached, "")))
                    }
                    None => content.push_str(&format!("\n[{label}{title}]（下载失败，仅通知）")),
                }
            }
        }

        let text = content.trim();
        if text.is_empty() {
            return; // 无有效文本（纯非文本消息且无标注时）也不打扰 agent。
        }
        if !send_to_llm {
            self.save_suppressed_group_content(&group_id, &user_id, text);
            return;
        }

        let chat_id = if is_group {
            format!("group:{group_id}")
        } else {
            format!("user:{user_id}")
        };

        let access = self.tool_access_for(&user_id, is_group);
        match inbound
            .send(BusMessage::new(CHANNEL_NAPCAT, &chat_id, text).with_tool_access(access))
            .await
        {
            Ok(()) => {}
            Err(e) => {
                warn!("NapCat 消息入队失败（inbound 已关闭，疑似进程退出）→ {chat_id}: {e:?}")
            }
        }
    }

    fn save_suppressed_group_content(&self, group_id: &str, user_id: &str, content: &str) {
        if self.cfg.media_dir.as_os_str().is_empty() || content.trim().is_empty() {
            return;
        }
        let dir = self.cfg.media_dir.join("napcat");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            warn!("NapCat 无法创建群消息归档目录 {}: {e}", dir.display());
            return;
        }
        let group = sanitize_filename(group_id);
        let user = sanitize_filename(user_id);
        let fname = format!(
            "group_{group}_user_{user}_{:08x}.txt",
            rand::thread_rng().gen::<u32>()
        );
        let path = dir.join(fname);
        let body = format!("group_id: {group_id}\nuser_id: {user_id}\n\n{content}\n");
        if let Err(e) = std::fs::write(&path, body) {
            warn!("NapCat 群消息本地归档失败 {}: {e}", path.display());
        }
    }

    fn remember_group_text_message(
        &self,
        message_id: i64,
        group_id: &str,
        user_id: &str,
        text: &str,
        event_time: i64,
    ) {
        let text = text.trim();
        if message_id == 0 || text.is_empty() {
            return;
        }
        let (text, truncated) = trim_text_for_index(text);
        let cached = CachedText {
            message_id,
            group_id: group_id.to_string(),
            user_id: user_id.to_string(),
            text,
            truncated,
        };
        if let Ok(mut cache) = self.text_cache.lock() {
            cache.insert(text_cache_key(group_id, message_id), cached.clone());
            trim_text_cache(&mut cache);
        }
        if let Err(e) = append_cached_text_to_disk(&self.cfg.media_dir, &cached, event_time) {
            warn!("NapCat 群文本引用索引写入失败: {e}");
        };
    }

    fn cached_text_for_replies(&self, group_id: &str, reply_ids: &[i64]) -> Vec<CachedText> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in reply_ids {
            if !seen.insert(*id) {
                continue;
            }
            let key = text_cache_key(group_id, *id);
            if let Some(cached) = self
                .text_cache
                .lock()
                .ok()
                .and_then(|cache| cache.get(&key).cloned())
            {
                out.push(cached);
                continue;
            }
            if let Some(cached) = find_cached_text_on_disk(&self.cfg.media_dir, group_id, *id) {
                if let Ok(mut cache) = self.text_cache.lock() {
                    cache.insert(key, cached.clone());
                    trim_text_cache(&mut cache);
                }
                out.push(cached);
            }
        }
        out
    }

    async fn cached_or_download_media(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        stream_pending: &StreamPendingMap,
        m: &MediaRef,
        ctx: MediaDownloadContext<'_>,
    ) -> Option<CachedMedia> {
        let keys = media_cache_keys(m, ctx.target, ctx.target_id);
        loop {
            if let Some(cached) = self.cached_media_for_keys(&keys) {
                self.remember_cached_media_for_message(
                    ctx.target,
                    ctx.target_id,
                    ctx.message_id,
                    cached.clone(),
                );
                return Some(cached);
            }

            match self.join_or_start_media_download(&keys) {
                MediaDownloadSlot::Follower(rx) => match rx.await {
                    Ok(Some(cached)) => {
                        self.remember_cached_media_for_message(
                            ctx.target,
                            ctx.target_id,
                            ctx.message_id,
                            cached.clone(),
                        );
                        return Some(cached);
                    }
                    Ok(None) | Err(_) => continue,
                },
                MediaDownloadSlot::Leader => {
                    let result = self
                        .download_and_cache_media(api_tx, pending, stream_pending, m, ctx)
                        .await;
                    self.finish_media_download(&keys, result.clone());
                    return result;
                }
                MediaDownloadSlot::Untracked => {
                    return self
                        .download_and_cache_media(api_tx, pending, stream_pending, m, ctx)
                        .await;
                }
            }
        }
    }

    async fn download_and_cache_media(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        stream_pending: &StreamPendingMap,
        m: &MediaRef,
        ctx: MediaDownloadContext<'_>,
    ) -> Option<CachedMedia> {
        let path = self
            .download_media(
                api_tx,
                pending,
                stream_pending,
                m,
                ctx.target,
                ctx.target_id,
            )
            .await?;
        let cached = CachedMedia {
            kind: m.kind,
            name: m.name.clone(),
            path,
        };
        self.remember_cached_media(m, ctx.target, ctx.target_id, ctx.message_id, cached.clone());
        Some(cached)
    }

    fn cached_media_for_keys(&self, keys: &[String]) -> Option<CachedMedia> {
        let Ok(cache) = self.media_cache.lock() else {
            return None;
        };
        keys.iter()
            .filter_map(|k| cache.by_key.get(k))
            .find(|cached| cached.path.is_file())
            .cloned()
    }

    fn join_or_start_media_download(&self, keys: &[String]) -> MediaDownloadSlot {
        if keys.is_empty() {
            return MediaDownloadSlot::Untracked;
        }
        let Ok(mut inflight) = self.media_inflight.lock() else {
            return MediaDownloadSlot::Untracked;
        };
        for key in keys {
            if let Some(waiters) = inflight.get_mut(key) {
                let (tx, rx) = oneshot::channel();
                waiters.push(tx);
                return MediaDownloadSlot::Follower(rx);
            }
        }
        for key in keys {
            inflight.insert(key.clone(), Vec::new());
        }
        MediaDownloadSlot::Leader
    }

    fn finish_media_download(&self, keys: &[String], result: Option<CachedMedia>) {
        let Ok(mut inflight) = self.media_inflight.lock() else {
            return;
        };
        let mut waiters = Vec::new();
        for key in keys {
            if let Some(mut list) = inflight.remove(key) {
                waiters.append(&mut list);
            }
        }
        drop(inflight);
        for waiter in waiters {
            let _ = waiter.send(result.clone());
        }
    }

    fn cached_media_for_replies(
        &self,
        target: &str,
        target_id: &str,
        reply_ids: &[i64],
    ) -> Vec<CachedMedia> {
        let Ok(cache) = self.media_cache.lock() else {
            return Vec::new();
        };
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for id in reply_ids {
            let key = media_message_cache_key(target, target_id, *id);
            if let Some(items) = cache.by_message_id.get(&key) {
                for cached in items {
                    if cached.path.is_file() && seen.insert(cached.path.clone()) {
                        out.push(cached.clone());
                    }
                }
            }
        }
        out
    }

    fn remember_cached_media(
        &self,
        m: &MediaRef,
        target: &str,
        target_id: &str,
        message_id: i64,
        cached: CachedMedia,
    ) {
        let keys = media_cache_keys(m, target, target_id);
        let Ok(mut cache) = self.media_cache.lock() else {
            return;
        };
        for key in keys {
            cache.by_key.insert(key, cached.clone());
        }
        remember_cached_media_for_message(&mut cache, target, target_id, message_id, cached);
        trim_media_cache(&mut cache);
        let snapshot = cache.clone();
        drop(cache);
        self.save_media_cache(&snapshot);
    }

    fn remember_cached_media_for_message(
        &self,
        target: &str,
        target_id: &str,
        message_id: i64,
        cached: CachedMedia,
    ) {
        let Ok(mut cache) = self.media_cache.lock() else {
            return;
        };
        remember_cached_media_for_message(&mut cache, target, target_id, message_id, cached);
        trim_media_cache(&mut cache);
        let snapshot = cache.clone();
        drop(cache);
        self.save_media_cache(&snapshot);
    }

    fn save_media_cache(&self, cache: &MediaCache) {
        let Some(path) = media_cache_index_path(&self.cfg.media_dir) else {
            return;
        };
        if let Some(dir) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(dir) {
                warn!("NapCat 无法创建媒体索引目录 {}: {e}", dir.display());
                return;
            }
        }
        match serde_json::to_vec_pretty(cache) {
            Ok(bytes) => {
                if let Err(e) = std::fs::write(&path, bytes) {
                    warn!("NapCat 媒体索引写入失败 {}: {e}", path.display());
                }
            }
            Err(e) => warn!("NapCat 媒体索引序列化失败: {e}"),
        }
    }

    /// 下载一个入站媒体到 `media_dir/napcat/`，返回落盘路径。
    ///
    /// 优先用媒体段自带的 `url`；`file` 段通常无 `url`，经 `get_file` API 换取下载链接。
    /// 任何环节失败返回 `None`（调用方降级为纯标注，不阻断收消息）。
    async fn download_media(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        stream_pending: &StreamPendingMap,
        m: &MediaRef,
        target: &str,
        target_id: &str,
    ) -> Option<PathBuf> {
        // 1) 解析下载来源。语音优先 get_record 转 mp3；普通文件保留 get_file 返回的 file_name。
        let source = self
            .resolve_media_source(api_tx, pending, m, target, target_id)
            .await;

        // 2) 落盘到 media_dir/napcat/<rand>_<safename>。HTTP 直链写临时文件再改名，避免大文件进内存。
        let dir = self.cfg.media_dir.join("napcat");
        if std::fs::create_dir_all(&dir).is_err() {
            warn!("NapCat 无法创建媒体目录 {}", dir.display());
            return None;
        }
        let base_name = source
            .as_ref()
            .and_then(|s| s.name.clone())
            .or_else(|| m.name.clone())
            .unwrap_or_else(|| m.kind.default_name().to_string());
        let safe = sanitize_filename(&base_name);
        let fname = format!("{:08x}_{safe}", rand::thread_rng().gen::<u32>());
        let path = dir.join(fname);

        if self
            .write_media_to_path(api_tx, stream_pending, source.as_ref(), m, &path)
            .await
        {
            match normalize_downloaded_media_extension(m.kind, &path) {
                Ok(corrected) => Some(corrected),
                Err(e) => {
                    warn!("NapCat 图片扩展名校正失败（保留原文件）: {e}");
                    Some(path)
                }
            }
        } else {
            let _ = std::fs::remove_file(&path);
            None
        }
    }

    /// 把 OneBot 媒体段解析成可下载来源（HTTP URL、本地文件或 base64）。
    async fn resolve_media_source(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        m: &MediaRef,
        target: &str,
        target_id: &str,
    ) -> Option<ResolvedMediaSource> {
        let mut source = ResolvedMediaSource::from_ref(m);

        // 语音直链通常是 raw silk；优先让 NapCat 用 get_record 转成通用 mp3。
        if m.kind == MediaKind::Voice {
            if let Some(fid) = m.file_id.as_deref().filter(|s| !s.is_empty()) {
                if let Ok(data) = self
                    .call_api(
                        api_tx,
                        pending,
                        "get_record",
                        json!({ "file": fid, "out_format": "mp3" }),
                    )
                    .await
                {
                    source.merge_api_data(&data);
                    if source.name.is_none() {
                        source.name = Some("voice.mp3".into());
                    }
                    if source.has_source() {
                        return Some(source);
                    }
                }
            }
        }

        if source.has_source() {
            return Some(source);
        }

        let fid = m.file_id.as_deref().filter(|s| !s.is_empty())?;

        // 文件消息优先用文件专用直链接口；失败后仍走 get_file 兜底。
        if m.kind == MediaKind::File {
            let api_result = match target {
                "group" => {
                    let mut params = json!({ "group_id": target_id, "file_id": fid });
                    if let Some(busid) = m.busid {
                        params["busid"] = json!(busid);
                    }
                    self.call_api(api_tx, pending, "get_group_file_url", params)
                        .await
                }
                "private" => {
                    self.call_api(
                        api_tx,
                        pending,
                        "get_private_file_url",
                        json!({ "file_id": fid }),
                    )
                    .await
                }
                _ => Err("unknown target".into()),
            };
            if let Ok(data) = api_result {
                source.merge_api_data(&data);
                if source.has_source() {
                    return Some(source);
                }
            }
        }

        // 图片无 url 时先尝试 get_image；其它类型/兜底走 get_file。
        if m.kind == MediaKind::Image {
            if let Ok(data) = self
                .call_api(api_tx, pending, "get_image", json!({ "file": fid }))
                .await
            {
                source.merge_api_data(&data);
                if source.has_source() {
                    return Some(source);
                }
            }
        }

        let data = self
            .call_api(api_tx, pending, "get_file", json!({ "file": fid }))
            .await
            .ok()?;
        source.merge_api_data(&data);
        source.has_source().then_some(source)
    }

    async fn write_media_to_path(
        &self,
        api_tx: &mpsc::Sender<String>,
        stream_pending: &StreamPendingMap,
        source: Option<&ResolvedMediaSource>,
        m: &MediaRef,
        target: &Path,
    ) -> bool {
        if let Some(source) = source {
            for candidate in source.candidates() {
                if is_http_url(&candidate) {
                    match self.download_http_to_path(&candidate, target, m.kind).await {
                        Ok(()) => return true,
                        Err(e) => warn!("NapCat 媒体 HTTP 下载失败 ({}): {e}", m.kind.label()),
                    }
                    if should_try_stream_after_http_failure(m.kind) {
                        match self
                            .download_file_stream_to_path(
                                api_tx,
                                stream_pending,
                                &candidate,
                                target,
                            )
                            .await
                        {
                            Ok(()) => return true,
                            Err(e) => {
                                debug!("NapCat Stream URL 兜底下载失败 ({}): {e}", m.kind.label())
                            }
                        }
                    }
                    continue;
                }

                if let Some(raw) = candidate.strip_prefix("base64://") {
                    match base64::engine::general_purpose::STANDARD.decode(raw) {
                        Ok(bytes) => match write_bytes_to_path(target, &bytes).await {
                            Ok(()) => return true,
                            Err(e) => warn!("NapCat base64 媒体落盘失败 ({}): {e}", m.kind.label()),
                        },
                        Err(e) => warn!("NapCat base64 媒体解码失败 ({}): {e}", m.kind.label()),
                    }
                    continue;
                }

                if should_try_stream_before_local(m.kind, &candidate) {
                    match self
                        .download_file_stream_to_path(api_tx, stream_pending, &candidate, target)
                        .await
                    {
                        Ok(()) => return true,
                        Err(e) => debug!(
                            "NapCat Stream 按协议端路径下载失败 ({}): {e}",
                            m.kind.label()
                        ),
                    }
                }

                let path = if candidate.starts_with("file://") {
                    file_url_to_path(&candidate)
                } else {
                    PathBuf::from(&candidate)
                };
                match copy_local_file_to_path(&path, target).await {
                    Ok(()) => return true,
                    Err(e) => warn!(
                        "NapCat 媒体本地读取失败 ({}) {}: {e}",
                        m.kind.label(),
                        path.display()
                    ),
                }
            }
        }

        if should_try_stream_by_file_id(m.kind) {
            if let Some(fid) = m.file_id.as_deref().filter(|s| !s.is_empty()) {
                match self
                    .download_file_stream_to_path(api_tx, stream_pending, fid, target)
                    .await
                {
                    Ok(()) => return true,
                    Err(e) => warn!(
                        "NapCat Stream 按 file_id 下载失败 ({}): {e}",
                        m.kind.label()
                    ),
                }
            }
        }
        false
    }

    async fn download_http_to_path(
        &self,
        url: &str,
        target: &Path,
        kind: MediaKind,
    ) -> Result<(), String> {
        let tmp = temp_download_path(target);
        let cleanup = || {
            let _ = std::fs::remove_file(&tmp);
        };
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let mut file = tokio::fs::File::create(&tmp)
            .await
            .map_err(|e| e.to_string())?;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let bytes = match chunk {
                Ok(bytes) => bytes,
                Err(e) => {
                    cleanup();
                    return Err(e.to_string());
                }
            };
            if let Err(e) = file.write_all(&bytes).await {
                cleanup();
                return Err(e.to_string());
            }
        }
        if let Err(e) = file.flush().await {
            cleanup();
            return Err(e.to_string());
        }
        drop(file);
        if let Err(e) = tokio::fs::rename(&tmp, target).await {
            cleanup();
            return Err(e.to_string());
        }
        debug!(
            "NapCat HTTP 媒体下载完成 ({}) -> {}",
            kind.label(),
            target.display()
        );
        Ok(())
    }

    async fn download_file_stream_to_path(
        &self,
        api_tx: &mpsc::Sender<String>,
        stream_pending: &StreamPendingMap,
        file_ref: &str,
        target: &Path,
    ) -> Result<(), String> {
        let download = self
            .download_file_stream(api_tx, stream_pending, file_ref)
            .await?;
        if download.bytes.is_empty() {
            return Err("stream 返回空文件".into());
        }
        if let Some(name) = download.name.as_deref() {
            debug!("NapCat Stream 媒体下载完成: {name} -> {}", target.display());
        }
        write_bytes_to_path(target, &download.bytes).await
    }

    async fn download_file_stream(
        &self,
        api_tx: &mpsc::Sender<String>,
        stream_pending: &StreamPendingMap,
        file_ref: &str,
    ) -> Result<MediaBytes, String> {
        // NapCat 官方 Stream 下载返回 `file_info` / `file_chunk` / `file_complete`：
        // https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/stream/DownloadFileStream.ts
        let echo = self.next_echo();
        let req = json!({
            "action": "download_file_stream",
            "params": {
                "file": file_ref,
                "chunk_size": STREAM_CHUNK_BYTES,
            },
            "echo": echo.clone(),
        });
        let text = serde_json::to_string(&req).map_err(|e| e.to_string())?;

        let (tx, mut rx) = mpsc::channel::<Value>(64);
        {
            let mut g = stream_pending.lock().map_err(|e| e.to_string())?;
            g.insert(echo.clone(), tx);
        }

        if let Err(e) = api_tx.send(text).await {
            if let Ok(mut g) = stream_pending.lock() {
                g.remove(&echo);
            }
            return Err(format!("stream api 通道已关闭: {e}"));
        }

        let mut state = StreamDownloadState::default();
        loop {
            if let Some(result) = state.try_finish()? {
                if let Ok(mut g) = stream_pending.lock() {
                    g.remove(&echo);
                }
                return Ok(result);
            }

            let resp = match timeout(STREAM_IDLE_TIMEOUT, rx.recv()).await {
                Ok(Some(resp)) => resp,
                Ok(None) => {
                    if let Ok(mut g) = stream_pending.lock() {
                        g.remove(&echo);
                    }
                    return Err("stream 响应通道关闭".into());
                }
                Err(_) => {
                    if let Ok(mut g) = stream_pending.lock() {
                        g.remove(&echo);
                    }
                    return Err("stream 空闲超时".into());
                }
            };

            let ok = resp.get("status").and_then(|v| v.as_str()) == Some("ok")
                && resp.get("retcode").and_then(|v| v.as_i64()) == Some(0);
            if !ok {
                if let Ok(mut g) = stream_pending.lock() {
                    g.remove(&echo);
                }
                return Err(api_error_wording(&resp));
            }

            let data = resp.get("data").unwrap_or(&Value::Null);
            if let Err(e) = state.ingest(data) {
                if let Ok(mut g) = stream_pending.lock() {
                    g.remove(&echo);
                }
                return Err(e);
            }
        }
    }

    /// 发送一条 outbound 消息：解析媒体标记，先发文本（分段），再逐个发媒体。
    ///
    /// 单条消息/单个媒体失败仅记录日志，不影响后续投递。
    async fn send_outbound(
        self: Arc<Self>,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        msg: BusMessage,
    ) -> Result<(), String> {
        let (target, id) = parse_chat_id(&msg.chat_id);
        let action = match target {
            "group" => "send_group_msg",
            "private" => "send_private_msg",
            _ => return Ok(()),
        };

        // 出站入口日志：静默丢失类故障（如 2026-09-05 凌晨的 DOCX 回复丢失）排查时，
        // 以此为起点对照后续「文本投递完成 / 媒体开始」日志判断消息消失在哪一跳。
        info!(
            chat_id = %msg.chat_id,
            bytes = msg.content.len(),
            "NapCat 出站开始"
        );

        let (intents, text) = parse_media_markers(&msg.content);

        // 1) 先发文本（若非空）。超过阈值改走合并转发卡片（一条消息承载整篇）；
        //    转发失败再回退到逐段纯文本，避免长回复丢失；若转发实际已送达但确认超时，
        //    回退可能造成重复。
        if !text.trim().is_empty() {
            if text.len() > self.cfg.send_chunk_bytes {
                if let Err(e) = self
                    .send_forward_text(api_tx, pending, target, &id, &text)
                    .await
                {
                    warn!(
                        "NapCat 合并转发失败或确认超时，回退逐段纯文本（可能重复） ({}): {e}",
                        msg.chat_id
                    );
                    for (index, chunk) in utf8_chunks(&text, self.cfg.send_chunk_bytes)
                        .into_iter()
                        .enumerate()
                    {
                        let params = match target {
                            "group" => json!({ "group_id": id, "message": chunk }),
                            _ => json!({ "user_id": id, "message": chunk }),
                        };
                        self.send_text_chunk(api_tx, pending, action, params, index + 1)
                            .await?;
                    }
                }
            } else {
                let params = match target {
                    "group" => json!({ "group_id": id, "message": text }),
                    _ => json!({ "user_id": id, "message": text }),
                };
                self.send_text_chunk(api_tx, pending, action, params, 1)
                    .await?;
            }
            info!(
                chat_id = %msg.chat_id,
                bytes = text.len(),
                "NapCat 文本投递完成"
            );
        }

        // 2) 再逐个发媒体（可选 caption 作为前置文本）。
        for intent in intents {
            if let Some(cap) = intent.caption.as_deref() {
                if !cap.trim().is_empty() {
                    let params = match target {
                        "group" => json!({ "group_id": id, "message": cap }),
                        _ => json!({ "user_id": id, "message": cap }),
                    };
                    if let Err(e) = self
                        .send_text_chunk(api_tx, pending, action, params, 1)
                        .await
                    {
                        warn!("NapCat 媒体 caption 发送失败 → {}: {e}", msg.chat_id);
                    }
                }
            }
            let prepared = match prepare_outbound_media(&intent, &self.cfg.media_dir) {
                Some(p) => {
                    info!(
                        chat_id = %msg.chat_id,
                        kind = intent.kind.label(),
                        name = %p.name,
                        bytes = p.size,
                        "NapCat 媒体开始发送"
                    );
                    p
                }
                None => {
                    warn!(
                        "NapCat 媒体路径不在 media_dir 沙箱内或不可读，跳过: {} {}",
                        intent.kind.label(),
                        intent.path
                    );
                    let media_name = intent
                        .name
                        .as_deref()
                        .map(sanitize_filename)
                        .unwrap_or_else(|| intent.kind.label().to_string());
                    self.send_media_delivery_notice(
                        api_tx,
                        pending,
                        OutboundTarget {
                            target,
                            id: &id,
                            action,
                        },
                        &media_name,
                        "文件不可读或不在允许的媒体目录内，未发送",
                    )
                    .await;
                    continue;
                }
            };
            match self
                .send_media_via_stream(
                    api_tx,
                    pending,
                    OutboundTarget {
                        target,
                        id: &id,
                        action,
                    },
                    intent.kind,
                    &prepared,
                )
                .await
            {
                Ok(()) => continue,
                Err(e) if !should_fallback_after_media_error(&e) => {
                    warn!(
                        "NapCat Stream 发送结果不确定，不再回退以避免重复投递 ({}): {}",
                        intent.kind.label(),
                        e.message
                    );
                    self.send_media_delivery_notice(
                        api_tx,
                        pending,
                        OutboundTarget {
                            target,
                            id: &id,
                            action,
                        },
                        &prepared.name,
                        "发送确认超时，实际可能已送达；为避免重复未自动重发",
                    )
                    .await;
                    continue;
                }
                Err(e) => warn!(
                    "NapCat Stream 发送失败，回退普通媒体发送 ({}): {e}",
                    intent.kind.label(),
                    e = e.message
                ),
            }

            if intent.kind == MediaKind::File || prepared.size > OUTBOUND_BASE64_MAX_BYTES {
                match self
                    .upload_file(api_tx, pending, target, &id, &prepared)
                    .await
                {
                    Ok(()) => continue,
                    Err(e) if delivery_status_unknown(&e) => {
                        warn!("NapCat 文件上传结果不确定，不再回退 file 消息段以避免重复投递: {e}");
                        self.send_media_delivery_notice(
                            api_tx,
                            pending,
                            OutboundTarget {
                                target,
                                id: &id,
                                action,
                            },
                            &prepared.name,
                            "发送确认超时，实际可能已送达；为避免重复未继续回退",
                        )
                        .await;
                        continue;
                    }
                    Err(e) => warn!("NapCat 文件上传 API 失败，回退 file 消息段: {e}"),
                }
            }
            let seg =
                media_segment_value(intent.kind, &fallback_file_ref(&prepared), &prepared.name);
            let params = match target {
                "group" => json!({ "group_id": id, "message": [seg] }),
                _ => json!({ "user_id": id, "message": [seg] }),
            };
            if let Err(e) = self.call_api(api_tx, pending, action, params).await {
                warn!("NapCat 媒体发送失败 ({}): {e}", intent.kind.label());
                let reason = if delivery_status_unknown(&e) {
                    "发送确认超时，实际可能已送达；为避免重复未自动重发"
                } else {
                    "媒体接口明确返回失败，未能完成发送"
                };
                self.send_media_delivery_notice(
                    api_tx,
                    pending,
                    OutboundTarget {
                        target,
                        id: &id,
                        action,
                    },
                    &prepared.name,
                    reason,
                )
                .await;
            }
        }
        Ok(())
    }

    /// 发送单个文本分段。对 NapCat `sendMsg` 服务层 NTEvent 超时和客户端等待 API 回执超时
    /// 做有限重试（见 [`TEXT_CHUNK_RESEND_ATTEMPTS`]）。由于超时后的实际投递结果不确定，
    /// 重试能挽回未投递的分段，也可能造成重复；重试耗尽后跳过该段并告警，仍继续后续分段。
    /// 响应通道关闭及其它错误直接上抛，由调用方决定是否中止整条消息。
    async fn send_text_chunk(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        action: &str,
        params: Value,
        index: usize,
    ) -> Result<(), String> {
        for attempt in 0..=TEXT_CHUNK_RESEND_ATTEMPTS {
            match self.call_api(api_tx, pending, action, params.clone()).await {
                Ok(_) => return Ok(()),
                Err(e) if text_send_timeout_retryable(&e) => {
                    if attempt < TEXT_CHUNK_RESEND_ATTEMPTS {
                        warn!(
                            index,
                            attempt = attempt + 1,
                            "NapCat 文本分段发送超时，退避后重试（可能重复）: {e}"
                        );
                        tokio::time::sleep(TEXT_CHUNK_RESEND_BACKOFF).await;
                        continue;
                    }
                    warn!(
                        index,
                        attempts = attempt + 1,
                        "NapCat 文本分段重试耗尽仍超时，跳过该段（投递状态不确定，可能丢失或重复）: {e}"
                    );
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
        }
        unreachable!("文本分段重试循环至少执行一次")
    }

    /// 媒体投递失败或状态不明时发送可见提示。附件本身不盲目重发，避免重复；
    /// 提示文本沿用文本超时重试策略，让用户能够判断是否需要手动要求重发。
    async fn send_media_delivery_notice(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        outbound: OutboundTarget<'_>,
        media_name: &str,
        reason: &str,
    ) {
        let notice = format!(
            "⚠️ 附件“{}”{}。如果你没有收到附件，请回复“重发附件”。",
            media_name, reason
        );
        let params = match outbound.target {
            "group" => json!({ "group_id": outbound.id, "message": notice }),
            _ => json!({ "user_id": outbound.id, "message": notice }),
        };
        if let Err(e) = self
            .send_text_chunk(api_tx, pending, outbound.action, params, 1)
            .await
        {
            warn!("NapCat 媒体投递提示也发送失败: {e}");
        }
    }

    /// 以合并转发卡片发送长文本：按段落/句末边界切成多个 node，一次 sendMsg 投递整篇。
    /// 失败时上抛错误，由调用方回退到逐段纯文本。
    async fn send_forward_text(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        target: &str,
        id: &str,
        text: &str,
    ) -> Result<(), String> {
        let action = match target {
            "group" => "send_group_forward_msg",
            "private" => "send_private_forward_msg",
            _ => return Ok(()),
        };
        let messages: Vec<Value> = split_forward_nodes(text, FORWARD_NODE_BYTES)
            .into_iter()
            .map(|node_text| {
                json!({
                    "type": "node",
                    "data": {
                        "name": "RClaw",
                        "nickname": "RClaw",
                        "content": node_text,
                    },
                })
            })
            .collect();
        let params = match target {
            "group" => json!({ "group_id": id, "messages": messages }),
            _ => json!({ "user_id": id, "messages": messages }),
        };
        info!(
            kind = target,
            nodes = messages.len(),
            "NapCat 合并转发卡片发送中"
        );
        self.call_api_with_timeout(api_tx, pending, action, params, FORWARD_API_TIMEOUT)
            .await?;
        Ok(())
    }

    fn outbound_send(
        self: Arc<Self>,
        api_tx: mpsc::Sender<String>,
        pending: PendingMap,
        msg: BusMessage,
    ) -> OutboundSend {
        let chat_id = msg.chat_id.clone();
        Box::pin(async move {
            if let Err(e) = self.send_outbound(&api_tx, &pending, msg).await {
                warn!("NapCat 发送失败: {e}");
            }
            chat_id
        })
    }

    async fn send_media_via_stream(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        outbound: OutboundTarget<'_>,
        kind: MediaKind,
        prepared: &PreparedMedia,
    ) -> Result<(), MediaSendError> {
        let remote_path = self
            .upload_file_stream(api_tx, pending, prepared)
            .await
            .map_err(MediaSendError::prepare)?;
        if kind == MediaKind::File || prepared.size > OUTBOUND_BASE64_MAX_BYTES {
            return self
                .upload_file_ref(
                    api_tx,
                    pending,
                    outbound.target,
                    outbound.id,
                    &remote_path,
                    &prepared.name,
                )
                .await
                .map_err(MediaSendError::deliver);
        }

        let seg = media_segment_value(kind, &remote_path, &prepared.name);
        let params = match outbound.target {
            "group" => json!({ "group_id": outbound.id, "message": [seg] }),
            _ => json!({ "user_id": outbound.id, "message": [seg] }),
        };
        self.call_api(api_tx, pending, outbound.action, params)
            .await
            .map(|_| ())
            .map_err(MediaSendError::deliver)
    }

    async fn upload_file(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        target: &str,
        id: &str,
        prepared: &PreparedMedia,
    ) -> Result<(), String> {
        self.upload_file_ref(
            api_tx,
            pending,
            target,
            id,
            &fallback_file_ref(prepared),
            &prepared.name,
        )
        .await
    }

    async fn upload_file_ref(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        target: &str,
        id: &str,
        file_ref: &str,
        name: &str,
    ) -> Result<(), String> {
        let action = match target {
            "group" => "upload_group_file",
            "private" => "upload_private_file",
            _ => return Ok(()),
        };
        // 文件投递比文本慢，因此放宽单次等待到 FILE_DELIVERY_TIMEOUT。所有超时都可能已由
        // NapCat 实际送达；附件没有幂等键，不能自动重发，否则会制造重复附件。错误上抛后，
        // 调用方停止级联回退并发送一条可见文本提示，让用户按需明确要求重发。
        let params = match target {
            "group" => json!({ "group_id": id, "file": file_ref, "name": name }),
            "private" => json!({ "user_id": id, "file": file_ref, "name": name }),
            _ => unreachable!("文件投递 target 已在调用前校验"),
        };
        self.call_api_with_timeout(api_tx, pending, action, params, FILE_DELIVERY_TIMEOUT)
            .await
            .map(|_| ())
    }

    async fn upload_file_stream(
        &self,
        api_tx: &mpsc::Sender<String>,
        pending: &PendingMap,
        prepared: &PreparedMedia,
    ) -> Result<String, String> {
        // NapCat 官方 Stream 上传逐块调用 `upload_file_stream`，完成后返回协议端 `file_path`：
        // https://github.com/NapNeko/NapCatQQ/blob/main/packages/napcat-onebot/action/stream/UploadFileStream.ts
        if prepared.size == 0 {
            return Err("空文件不能通过 upload_file_stream 上传".into());
        }

        let stream_id = format!(
            "rclaw-upload-{:016x}{:016x}",
            rand::thread_rng().gen::<u64>(),
            rand::thread_rng().gen::<u64>()
        );
        let total_chunks = prepared.size.div_ceil(STREAM_CHUNK_BYTES as u64);
        let expected_sha256 = sha256_file_hex(&prepared.path)?;

        let mut file = File::open(&prepared.path)
            .map_err(|e| format!("无法打开待上传文件 {}: {e}", prepared.path.display()))?;
        let mut buf = vec![0_u8; STREAM_CHUNK_BYTES];
        for chunk_index in 0..total_chunks {
            let offset = chunk_index * STREAM_CHUNK_BYTES as u64;
            let want = ((prepared.size - offset).min(STREAM_CHUNK_BYTES as u64)) as usize;
            let mut read = 0;
            while read < want {
                let n = file
                    .read(&mut buf[read..want])
                    .map_err(|e| format!("读取待上传文件失败 {}: {e}", prepared.path.display()))?;
                if n == 0 {
                    return Err(format!(
                        "读取待上传文件提前结束: {chunk_index}/{total_chunks}"
                    ));
                }
                read += n;
            }
            let chunk_data = base64::engine::general_purpose::STANDARD.encode(&buf[..read]);
            let params = json!({
                "stream_id": stream_id,
                "chunk_data": chunk_data,
                "chunk_index": chunk_index,
                "total_chunks": total_chunks,
                "file_size": prepared.size,
                "expected_sha256": expected_sha256.clone(),
                "filename": prepared.name.clone(),
                "file_retention": STREAM_FILE_RETENTION_MS,
            });
            self.call_api_with_timeout(
                api_tx,
                pending,
                "upload_file_stream",
                params,
                STREAM_API_TIMEOUT,
            )
            .await?;
        }

        let data = self
            .call_api_with_timeout(
                api_tx,
                pending,
                "upload_file_stream",
                json!({ "stream_id": stream_id, "is_complete": true }),
                STREAM_API_TIMEOUT,
            )
            .await?;
        if data.get("status").and_then(|v| v.as_str()) != Some("file_complete") {
            return Err(format!("upload_file_stream 未完成: {data}"));
        }
        first_string(&data, &["file_path", "path", "file"])
            .ok_or_else(|| format!("upload_file_stream 未返回 file_path: {data}"))
    }
}

#[async_trait]
impl Channel for NapCat {
    fn name(&self) -> &str {
        CHANNEL_NAPCAT
    }

    fn capabilities(&self) -> rclaw_core::ChannelCaps {
        rclaw_core::ChannelCaps {
            name: CHANNEL_NAPCAT.to_string(),
            // NapCat 可收发媒体；QQ 会话常驻展示工作状态；中间输出（reasoning
            // 经安全检查后的「💭 思考中...」转发）仅 NapCat 许可。reasoning 不做
            // 逐分片流式（reasoning_stream = false）：QQ 通道保持按轮整段快照。
            media: true,
            working_status: true,
            intermediate_output: true,
            reasoning_stream: false,
        }
    }

    /// 连接主循环：连接 → 收发 select → 断开 → 退避重连。
    ///
    /// inbound 投递（含媒体下载）使用独立任务；outbound 按 chat_id 并发、同会话串行，并由
    /// 本循环持续轮询。这样单条发送等待 API 响应时，WS 读循环仍能处理响应而不会死锁。
    /// outbound 关闭（agent loop 结束）时通道退出。
    async fn run(self, inbound: mpsc::Sender<BusMessage>, mut outbound: OutboundReceiver) {
        let me = Arc::new(self);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let stream_pending: StreamPendingMap = Arc::new(Mutex::new(HashMap::new()));
        let dedup: DedupSet = Arc::new(Mutex::new(HashSet::new()));
        // 同一会话的后续消息已从 outbound 接收端取出，必须跨重连保留；否则断线会使这些
        // 尚未开始发送的消息永久丢失。
        let mut queued_outbound: HashMap<String, VecDeque<BusMessage>> = HashMap::new();
        let mut reconnect_delay = me.cfg.reconnect_delay_sec.max(1);

        loop {
            // 构造 WS 请求：补齐尾部 '/' + access_token（查询参数 + Bearer 头双发）。
            let request = match build_ws_request(&me.cfg.ws_url, &me.cfg.access_token) {
                Ok(r) => r,
                Err(e) => {
                    error!("NapCat ws_url 非法 ({}): {e}", me.cfg.ws_url);
                    return; // URL 非法属于配置错误，重连无意义。
                }
            };

            match connect_async(request).await {
                Ok((ws_stream, _)) => {
                    info!("NapCatQQ WebSocket 已连接 ({})", me.cfg.ws_url);
                    reconnect_delay = me.cfg.reconnect_delay_sec.max(1);
                    let (mut write, mut read) = ws_stream.split();
                    // 内部 API 转发通道：发送任务把请求 JSON 投到这里，由主循环统一写 WS。
                    let (api_tx, mut api_rx) = mpsc::channel::<String>(64);
                    let mut outbound_sends = FuturesUnordered::<OutboundSend>::new();
                    let mut active_outbound_chats = HashSet::new();

                    // 重连后先恢复每个会话的一条队首消息；其余消息在该会话当前发送完成后
                    // 按原顺序继续，且不同会话仍可并发。
                    let queued_chats: Vec<String> = queued_outbound.keys().cloned().collect();
                    for chat_id in queued_chats {
                        let next = queued_outbound
                            .get_mut(&chat_id)
                            .and_then(VecDeque::pop_front);
                        if queued_outbound
                            .get(&chat_id)
                            .is_some_and(VecDeque::is_empty)
                        {
                            queued_outbound.remove(&chat_id);
                        }
                        if let Some(next) = next {
                            active_outbound_chats.insert(chat_id);
                            outbound_sends.push(me.clone().outbound_send(
                                api_tx.clone(),
                                pending.clone(),
                                next,
                            ));
                        }
                    }

                    let mut heartbeat = tokio::time::interval(Duration::from_secs(
                        me.cfg.heartbeat_interval_sec.max(1),
                    ));
                    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                    loop {
                        tokio::select! {
                            chat_id = outbound_sends.next(), if !outbound_sends.is_empty() => {
                                let chat_id = chat_id.expect("非空 FuturesUnordered 必须返回完成的发送任务");
                                let next = queued_outbound
                                    .get_mut(&chat_id)
                                    .and_then(VecDeque::pop_front);
                                if queued_outbound
                                    .get(&chat_id)
                                    .is_some_and(VecDeque::is_empty)
                                {
                                    queued_outbound.remove(&chat_id);
                                }
                                if let Some(next) = next {
                                    outbound_sends.push(me.clone().outbound_send(
                                        api_tx.clone(),
                                        pending.clone(),
                                        next,
                                    ));
                                } else {
                                    active_outbound_chats.remove(&chat_id);
                                }
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(WsMessage::Text(text))) => {
                                        let me2 = me.clone();
                                        let inbound2 = inbound.clone();
                                        let api_tx2 = api_tx.clone();
                                        let pending2 = pending.clone();
                                        let stream_pending2 = stream_pending.clone();
                                        let dedup2 = dedup.clone();
                                        // 独立任务处理：避免媒体下载 / get_file / inbound 背压卡住读循环。
                                        tokio::spawn(async move {
                                            me2.handle_ws_text(
                                                &text,
                                                &inbound2,
                                                &api_tx2,
                                                &pending2,
                                                &stream_pending2,
                                                &dedup2,
                                            )
                                            .await;
                                        });
                                    }
                                    Some(Ok(WsMessage::Ping(data))) => {
                                        if let Err(e) = write.send(WsMessage::Pong(data)).await {
                                            error!("NapCat 回 Pong 失败: {e}");
                                            break;
                                        }
                                    }
                                    Some(Ok(WsMessage::Pong(_))) => {}
                                    Some(Ok(WsMessage::Close(frame))) => {
                                        warn!("NapCat WS 被对端关闭: {frame:?}");
                                        break;
                                    }
                                    Some(Ok(_)) => {} // Binary / Frame 等忽略。
                                    Some(Err(e)) => {
                                        error!("NapCat WS 读错误: {e}");
                                        break;
                                    }
                                    None => {
                                        warn!("NapCat WS 流结束");
                                        break;
                                    }
                                }
                            }
                            req_text = api_rx.recv() => {
                                match req_text {
                                    Some(t) => {
                                        if let Err(e) = write.send(WsMessage::Text(t)).await {
                                            error!("NapCat WS 写失败: {e}");
                                            break;
                                        }
                                    }
                                    None => break,
                                }
                            }
                            ob = outbound.recv() => {
                                match ob {
                                    Some(bus) => {
                                        let chat_id = bus.chat_id.clone();
                                        if active_outbound_chats.insert(chat_id.clone()) {
                                            outbound_sends.push(me.clone().outbound_send(
                                                api_tx.clone(),
                                                pending.clone(),
                                                bus,
                                            ));
                                        } else {
                                            let queue = queued_outbound.entry(chat_id.clone()).or_default();
                                            if queue.len() < OUTBOUND_CHAT_QUEUE_CAP {
                                                queue.push_back(bus);
                                            } else {
                                                tracing::error!(
                                                    chat_id = %chat_id,
                                                    bytes = bus.content.len(),
                                                    "NapCat 同一会话的出站队列已满，丢弃消息"
                                                );
                                            }
                                        }
                                    }
                                    None => {
                                        // outbound（dispatcher→本通道）关闭 = agent loop 已结束 → 通道退出。
                                        info!("NapCat outbound 已关闭，通道退出");
                                        return;
                                    }
                                }
                            }
                            _ = heartbeat.tick() => {
                                // 周期探活：发送失败即认定连接已死，跳出重连。
                                let hb = json!({ "action": "get_status", "echo": me.next_echo() });
                                if let Ok(t) = serde_json::to_string(&hb) {
                                    if let Err(e) = write.send(WsMessage::Text(t)).await {
                                        error!("NapCat 心跳发送失败: {e}");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    error!("NapCat WS 连接失败 ({}): {e}", me.cfg.ws_url);
                }
            }

            // 退避重连（指数，上限 60s）。
            info!("NapCat 将在 {reconnect_delay}s 后重连...");
            tokio::time::sleep(Duration::from_secs(reconnect_delay)).await;
            reconnect_delay = (reconnect_delay * 2).min(MAX_RECONNECT_DELAY);
        }
    }
}

// ---------------------------------------------------------------------------
mod media;
use media::*;

#[cfg(test)]
mod tests;
