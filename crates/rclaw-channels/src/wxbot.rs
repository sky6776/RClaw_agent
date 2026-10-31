//! 个人微信通道（VA + WCX 容器内微信，通道名 `wxbot`）。
//!
//! 设计与契约见 `docs/wxbot-channel.md`（实施文档）。要点：
//!
//! - **双路收消息，同源合并**：wcx 侧 `WebhookPush` feature 的常驻事件收集器为每个
//!   入站消息一次性赋好 `seq / dedup_key / 归一化 type / 群 sender / at_users`；
//!   - 主路（webhook）：收集器把事件 POST 到本通道的 axum 监听（127.0.0.1，Bearer
//!     secret 校验，入队即回 200——处理慢不算失败，wcx 的 10s 超时不会误重试）；
//!   - 兜底（fallback）：常驻长轮询 `GET /api/messages/wait?cursor=<seq>`（cursor 式
//!     读同一个收集器缓冲，没有逐条挂 listener 的间隙丢失）。
//!   两路汇入同一条 ingest 队列，**单一任务顺序处理**：去重 → 账号核对 → 白名单 →
//!   媒体落盘 → `inbound.send().await` → 提交账本（投递成功才提交，失败可重投）。
//! - **发送**：REST `/api/messages/*`。文本按 UTF-8 安全分段；媒体先
//!   `/api/media/upload` 到 WCX 侧拿到手机路径再发；语音链路为
//!   mp3 → `utils/audio/mp3-to-silk`（WCX 本地路径）→ `utils/audio/duration` → 发送。
//! - **安全**：`api_base` / `webhook_listen` 强制回环，`api_token` / `webhook_secret`
//!   强制非空，reqwest 禁重定向——全部在 [`Wxbot::new`] 校验，任一不满足即不启动。
//! - **账号绑定**：启动时经 `/api/self/info` 绑定微信账号；运行中收到其他账号的事件
//!   一律丢弃（换号需重启通道，防跨账号复用历史 / 误发）。
//!
//! 白名单 = **回复**门槛（记录与回复解耦）：私聊 `allow_from` 空 = 放行全部（与
//! weixin 通道一致）；群聊 `allow_groups` 空 = 所有群不回复（即使被 @）。群消息
//! 一律静默落库——白名单外文本进本地索引、媒体仅标注不下载；白名单内按既有
//! 流程（媒体照常下载），未触发回复的归档（`archived=1`）。回复才按
//! `group_reply_policy`（默认 `"mention"`，仅精确 @ 本账号）触发。

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rand::Rng;
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use rclaw_core::{BusMessage, CHANNEL_WXBOT};

use crate::{Channel, OutboundReceiver};

/// 入站媒体单文件上限（整段驻留内存后落盘；与 weixin CDN 下载同一量级防御）。
const MEDIA_MAX_BYTES: u64 = 256 * 1024 * 1024;
/// 去重账本容量（dedup_key 条数）。满则驱逐最旧的一半。
const SEEN_CAP: usize = 8192;
/// ingest 队列容量（webhook + wait 双路汇入）。满时 webhook 返回 503 由 wcx 重试。
const QUEUE_CAP: usize = 256;
/// 媒体拉取专用超时：raw 端点服务端自身会等待文件就绪（最长 ~120s），客户端预算要
/// 盖过它，避免超时重试触发重复下载。
const MEDIA_TIMEOUT_SEC: u64 = 150;
/// 索引里的单条正文上限（持久化层截断标记，256KB 与 napcat 同款）。
const TEXT_INDEX_MAX_BYTES: usize = 256 * 1024;

/// 入站消息持久化库的 schema（v2）。
///
/// 一张表承载三类用途：引用回填索引（按 account+chat_id+svr_id 点查）、群消息静默
/// 归档（`archived=1`，含白名单外群——记录与回复解耦，替代 napcat 的散落 txt）、
/// 以及私聊/群聊全量消息的本地落盘（尽力而为，落库失败仅告警不重投）。
/// `dedup_key` 唯一约束让 webhook/wait 双路重复投递天然幂等（INSERT OR IGNORE）。
/// `account` 隔离不同微信账号：换号后同一群/同一 svr_id 的历史不得跨账号回填。
const DB_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS inbound_messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    dedup_key   TEXT    NOT NULL UNIQUE,
    account     TEXT    NOT NULL DEFAULT '',
    chat_id     TEXT    NOT NULL,
    is_group    INTEGER NOT NULL DEFAULT 0,
    sender      TEXT    NOT NULL DEFAULT '',
    svr_id      INTEGER NOT NULL DEFAULT 0,
    kind        TEXT    NOT NULL DEFAULT '',
    text        TEXT    NOT NULL DEFAULT '',
    media_json  TEXT    NOT NULL DEFAULT '[]',
    archived    INTEGER NOT NULL DEFAULT 0,
    create_time INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_inbound_acct_chat_svr ON inbound_messages(account, chat_id, svr_id);
DROP INDEX IF EXISTS idx_inbound_chat_svr;
"#;

/// 微信通道配置（TOML 字段名 snake_case，见 `rclaw.toml.example` 的 `[wxbot]` 段）。
#[derive(Debug, Clone)]
pub struct WxbotConfig {
    pub enabled: bool,
    /// wcx ApiServer 基址（同机 loopback；host 必须是 127.0.0.1/[::1]/localhost）。
    pub api_base: String,
    /// wcx「API + MCP 服务器」的认证令牌；启用时必须非空。
    pub api_token: String,
    /// webhook 监听地址（默认 127.0.0.1:38081；host 必须回环）。
    pub webhook_listen: String,
    /// webhook 路径（默认 /wxbot/webhook；须与 wcx WebhookPush 配的 URL 一致）。
    pub webhook_path: String,
    /// webhook secret（与 wcx WebhookPush 一致）；启用时必须非空。
    pub webhook_secret: String,
    /// 私聊白名单：空 = 放行全部；非空 = 仅名单内。逗号分隔 wxid。
    pub allow_from: String,
    /// 群回复白名单：空 = 所有群不回复（消息仍全量落库）。逗号分隔群 id（xxx@chatroom）。
    pub allow_groups: String,
    /// 群触发策略："mention"（默认，仅精确 @ 本账号）| "all"。
    pub group_reply_policy: String,
    /// 兜底长轮询开关（默认 true）。
    pub poll_fallback_enabled: bool,
    pub poll_timeout_sec: u64,
    pub retry_sec: u64,
    pub backoff_sec: u64,
    pub max_failures: u32,
    pub send_chunk_bytes: usize,
    /// 出站拟人化：相邻发送的基础间隔（秒）。
    pub send_min_gap_sec: u64,
    /// 出站拟人化：随机抖动上限（秒），实际间隔 = min_gap + rand(0..=jitter)。
    pub send_jitter_sec: u64,
    /// 出站拟人化：60s 滑动窗口内最大发送条数；0 = 关闭窗口限制。
    pub send_window_max: usize,
    /// 出站熔断：连续 `send_failure_fuse` 次发送失败后锁停通道；0 = 关闭熔断。
    pub send_failure_fuse: u32,
    /// 入站媒体落盘根目录（通常 `<agent_base>/media`）；为空时禁用下载，入站仅标注。
    pub media_dir: PathBuf,
}

impl Default for WxbotConfig {
    fn default() -> Self {
        WxbotConfig {
            enabled: false,
            api_base: "http://127.0.0.1:3001".into(),
            api_token: String::new(),
            webhook_listen: "127.0.0.1:38081".into(),
            webhook_path: "/wxbot/webhook".into(),
            webhook_secret: String::new(),
            allow_from: String::new(),
            allow_groups: String::new(),
            group_reply_policy: "mention".into(),
            poll_fallback_enabled: true,
            poll_timeout_sec: 60,
            retry_sec: 2,
            backoff_sec: 30,
            max_failures: 3,
            send_chunk_bytes: 2000,
            send_min_gap_sec: 2,
            send_jitter_sec: 3,
            send_window_max: 10,
            send_failure_fuse: 5,
            media_dir: PathBuf::new(),
        }
    }
}

/// wcx 收集器事件（webhook body ≡ `/api/messages/wait` 的事件体，契约见文档 §4.1）。
///
/// 字段大量缺省：wcx 侧构造时已尽力填充，缺失字段安全退化（去重键退化、类型走 other）。
#[derive(Debug, Clone, Deserialize)]
pub struct CollectorEvent {
    /// "message"（消息）| "test"（wcx 设置里的测试推送）。
    #[serde(default)]
    pub event: String,
    /// 收集器内单调递增序号（wait 的 cursor 锚点）。
    #[serde(default)]
    pub seq: u64,
    /// 两路稳定一致的去重键（wcx 生成；缺失时用 [`effective_key`] 退化）。
    #[serde(default)]
    pub dedup_key: String,
    /// 当前登录微信账号（wxid；可能为空串）。
    #[serde(default)]
    pub account: String,
    /// 服务端消息 id（raw 媒体拉取的锚点）；0 = 无服务端 id（无法拉媒体，标注降级）。
    #[serde(rename = "msg_svr_id", default)]
    pub svr_id: u64,
    /// 归一化类型（"text"/"image"/…/"other"），serde 撞关键字，rename 自 `type`。
    #[serde(rename = "type", default)]
    pub kind: String,
    /// 原始消息 type code（未知类型时兜底展示）。
    #[serde(rename = "type_code", default)]
    pub type_code: i64,
    /// appmsg 内层 `<type>`（仅 type=appmsg 类事件有值）。
    #[serde(rename = "appmsg_type", default)]
    pub appmsg_type: Option<i64>,
    /// 会话：单聊 = 对方 wxid；群聊 = 群 id（xxx@chatroom）。
    #[serde(default)]
    pub talker: String,
    /// 群聊发送者 wxid（严格前缀解析；失败 = "unknown"）；单聊 = talker。
    #[serde(default)]
    pub sender: String,
    #[serde(rename = "is_group", default)]
    pub is_group: bool,
    /// 实际正文（群聊已剥 "sender:\n" 前缀）。
    #[serde(default)]
    pub content: String,
    /// 原始 content（媒体类是 xml）。
    #[serde(rename = "raw_content", default)]
    pub raw_content: String,
    /// appmsg/link/miniprogram 标题。
    #[serde(default)]
    pub title: Option<String>,
    /// appmsg 摘要。
    #[serde(default)]
    pub des: Option<String>,
    /// 链接地址。
    #[serde(default)]
    pub url: Option<String>,
    /// @列表（msgSource/lvbuffer 解析；缺失 = 空）。
    #[serde(rename = "at_users", default)]
    pub at_users: Vec<String>,
    /// Unix 秒（微信 message 表 createTime 原始值）。
    #[serde(rename = "create_time", default)]
    pub create_time: i64,
    /// 引用消息的被引用消息 svrid（appmsg 57 refermsg；群引用回填的索引锚点）。
    #[serde(rename = "quote_svrid", default)]
    pub quote_svrid: u64,
}

/// 群/私聊引用回填的文本条目（查询载体；media 为该消息已落盘的媒体标注）。
#[derive(Debug, Clone)]
struct CachedText {
    text: String,
    truncated: bool,
    media: Vec<CachedMediaNote>,
}

/// 已落盘媒体标注（持久化为 `media_json`，引用回填时还原）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedMediaNote {
    kind_label: String,
    name: Option<String>,
    path: PathBuf,
}

/// 通道状态（账号绑定 + 去重账本），单一 ingest 任务独占访问，Mutex 只防 webhook
/// handler 之外的 bind_account 首访竞争。
#[derive(Default)]
struct WxbotState {
    /// 启动绑定的微信账号（wxid）。空 = 尚未绑定。
    account: String,
    /// 账号是否已显式绑定（区分「未绑定」与「绑定成功但 wxid 为空」）。
    account_bound: bool,
    /// 换号锁停闩：检测到账号不匹配后置位，收发全部拒绝直到进程重启
    /// （防止切换账号后，旧会话的待发回复经新账号发出造成跨账号泄露）。
    account_locked: bool,
    /// 去重账本：dedup_key 集合 + 插入顺序（驱逐需要 FIFO）。
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl WxbotState {
    fn contains(&self, key: &str) -> bool {
        self.seen.contains(key)
    }

    /// 投递成功后提交。超容量驱逐最旧的一半（保近期、去远期）。
    fn commit(&mut self, key: String) {
        if key.is_empty() || !self.seen.insert(key.clone()) {
            return;
        }
        self.order.push_back(key);
        if self.order.len() > SEEN_CAP {
            let drop = self.order.len() / 2;
            for _ in 0..drop {
                if let Some(old) = self.order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }

    /// 绑定账号（仅首次生效；后续调用 no-op）。
    fn bind(&mut self, account: &str) {
        if self.account_bound || account.is_empty() {
            return;
        }
        self.account = account.to_string();
        self.account_bound = true;
        tracing::info!("wxbot：已绑定微信账号 {account}");
    }

    /// 账号核对：事件 account 与绑定一致（或事件未带 account）→ true；
    /// 不一致 → 闩上锁停开关并丢弃。锁停后所有后续事件（含不带 account 的）一律拒绝。
    fn account_ok(&mut self, event_account: &str) -> bool {
        if self.account_locked {
            return false;
        }
        if event_account.is_empty() || !self.account_bound {
            return true;
        }
        if event_account == self.account {
            return true;
        }
        self.account_locked = true;
        tracing::warn!(
            "wxbot：检测到微信账号切换（绑定 {} → 事件 {}），通道锁停收发直至重启",
            self.account,
            event_account
        );
        false
    }
}

/// 个人微信通道。
pub struct Wxbot {
    cfg: Arc<WxbotConfig>,
    /// REST 客户端（禁重定向；超时盖过长轮询）。
    http: reqwest::Client,
    /// 媒体拉取客户端（raw 端点服务端会等待文件就绪，预算放宽到 150s）。
    media_http: reqwest::Client,
    state: Arc<Mutex<WxbotState>>,
    /// 入站消息持久化库（`media_dir/wxbot/message_index.db`；media_dir 为空或打不开
    /// 时为 None——回填与落盘降级，通道照常工作）。`Connection` 非 Sync，Mutex 串行化
    /// （rclaw-store `Db` 同款范式）；热路径是单行 UPSERT/点查（毫秒级），同步调用、
    /// 不跨 await 持锁。
    db: Option<Mutex<Connection>>,
}

impl Wxbot {
    pub fn new(mut cfg: WxbotConfig) -> Option<Self> {
        if !cfg.enabled {
            tracing::info!("wxbot 通道未启用（enabled=false）");
            return None;
        }
        // 安全前置：凭据非空 + 双向地址强制回环（本通道只在同机 loopback 拓扑下工作；
        // 回环绑定不替代鉴权，同机其他进程可伪造事件冒充白名单身份）。
        if cfg.api_token.trim().is_empty() {
            tracing::warn!("wxbot 通道启用但 api_token 为空，已拒绝启动");
            return None;
        }
        if cfg.webhook_secret.trim().is_empty() {
            tracing::warn!("wxbot 通道启用但 webhook_secret 为空，已拒绝启动");
            return None;
        }
        if !is_loopback_url(&cfg.api_base) {
            tracing::warn!(
                "wxbot：api_base 非 loopback（{}），已拒绝启动",
                cfg.api_base
            );
            return None;
        }
        if !is_loopback_listen(&cfg.webhook_listen) {
            tracing::warn!(
                "wxbot：webhook_listen 非 loopback（{}），已拒绝启动",
                cfg.webhook_listen
            );
            return None;
        }
        if !cfg.webhook_path.starts_with('/') {
            cfg.webhook_path = format!("/{}", cfg.webhook_path.trim_start_matches('/'));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            // 禁用环境代理：HTTP_PROXY/ALL_PROXY 存在时，回环请求也会被带 Bearer/正文
            // 转发给外部代理——强制回环拓扑必须显式 no_proxy。
            .no_proxy()
            .timeout(Duration::from_secs(cfg.poll_timeout_sec.saturating_add(15)))
            .build()
            .ok()?;
        let media_http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(MEDIA_TIMEOUT_SEC))
            .build()
            .ok()?;
        let db = open_message_db(&cfg.media_dir);
        Some(Wxbot {
            cfg: Arc::new(cfg),
            http,
            media_http,
            state: Arc::new(Mutex::new(WxbotState::default())),
            db,
        })
    }

    /// 发送闸门：锁停拒绝；账号未知（尚未绑定）同样拒绝——换号尚未产生入站事件时
    /// 无法察觉，宁可拒发不可跨账号误发。所有实际发送/重试路径的公共咽喉点。
    fn send_gate(&self) -> Result<(), ()> {
        let st = self.state.lock().map_err(|_| ())?;
        if !st.account_bound {
            tracing::warn!("wxbot：账号未绑定（未知身份），拒绝发送");
            return Err(());
        }
        if st.account_locked {
            tracing::warn!("wxbot：通道已锁停（换号或连续发送失败熔断），拒绝发送");
            return Err(());
        }
        Ok(())
    }

    /// 记录一次终态发送失败；达到阈值即锁停通道。调用方保证同一次发送只调用一次。
    fn record_send_failure(&self, pacer: &mut SendPacer) {
        if pacer.note_failure() {
            self.lock_stop("连续发送失败达到熔断阈值");
        }
    }

    /// 锁停通道：幂等；已锁停时不重复刷日志。用于换号与发送失败熔断。
    fn lock_stop(&self, reason: &str) {
        if let Ok(mut st) = self.state.lock() {
            if !st.account_locked {
                st.account_locked = true;
                tracing::error!("wxbot：{reason}，通道锁停收发直至进程重启");
            }
        }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.cfg.api_token)
    }

    fn api_url(&self, path: &str) -> String {
        format!("{}/api/{path}", self.cfg.api_base.trim_end_matches('/'))
    }

    /// 逗号分隔白名单 → 去空白条目。
    fn split_list(s: &str) -> Vec<&str> {
        s.split(',')
            .map(|x| x.trim())
            .filter(|x| !x.is_empty())
            .collect()
    }

    /// 回复门槛（文档 §6.2.1）：决定一条消息是否进入回复管线（记录门槛另见
    /// [`Self::ingest_one`] 步骤 3——群消息一律落库，与白名单无关）。
    ///
    /// 群消息过了这道门之后还要再过 [`Self::group_reaches_llm`] 的触发判定——未触发的
    /// 白名单群消息走「静默接收」：媒体照常下载、正文进引用索引、内容归档落盘，
    /// 只是投递给 LLM 与否按策略（napcat 群消息同款两层模型）。
    fn allowed(&self, ev: &CollectorEvent) -> bool {
        if ev.is_group {
            if self.cfg.allow_groups.is_empty() || ev.talker.is_empty() {
                return false;
            }
            Self::split_list(&self.cfg.allow_groups)
                .iter()
                .any(|s| *s == ev.talker)
        } else {
            if self.cfg.allow_from.is_empty() {
                return true;
            }
            Self::split_list(&self.cfg.allow_from)
                .iter()
                .any(|s| *s == ev.talker)
        }
    }

    /// 群触发判定：这条（已过白名单门槛的）群消息是否投递给 LLM。
    ///
    /// `group_reply_policy`：`none`（全部静默接收）/ `mention`（默认，仅精确 @ 本账号）/
    /// `all`（全部投递）。
    fn group_reaches_llm(&self, ev: &CollectorEvent) -> bool {
        match self
            .cfg
            .group_reply_policy
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "none" => false,
            "all" => true,
            _ => self.mentioned(ev),
        }
    }

    /// 群 mention 命中：事件 @列表精确包含绑定账号（无 at 兜底——@他人/邮箱不算）。
    fn mentioned(&self, ev: &CollectorEvent) -> bool {
        let account = self
            .state
            .lock()
            .map(|s| s.account.clone())
            .unwrap_or_default();
        !account.is_empty() && ev.at_users.iter().any(|u| u == &account)
    }

    /// 微信系统风控提示（账号受限等 weixin110 通知）：只静默归档，绝不投递 LLM。
    ///
    /// 这类通知随每次被拦的发送重复到达；回复它会形成「回复→被拦→再收通知」的
    /// 自激循环并加重账号风控（2026-10-30 实测连发 157 条的教训）。
    ///
    /// 判定只看 wcx 归一化后的 `type == "system"`，不能看正文是否以 `[系统]` 开头：
    /// 该前缀是 wxbot 在 [`Self::build_content`] 里为投递展示补的，`CollectorEvent.content`
    /// 仍是原始正文（§5 的映射表）。URL 可能落在 `content`、`raw_content` 或 `url`
    /// 任一字段，逐个大小写不敏感匹配。群系统通知同样要拦：`group_reply_policy=all`
    /// 时群里被拦的发送也会触发同类通知，不拦会复现同一个自激循环。
    fn is_risk_notice(ev: &CollectorEvent) -> bool {
        /// 只认 http(s) URL 的 **host** 完全等于 weixin110.qq.com；裸字符串、
        /// `notweixin110.qq.com`、`evil.weixin110.qq.com`、`weixin110.qq.com.evil.com`
        /// 都不算，避免用户可控昵称/问候语误吞正常系统通知。
        fn mentions_weixin110(s: &str) -> bool {
            const HOST: &str = "weixin110.qq.com";
            let lower = s.to_ascii_lowercase();
            for (idx, _) in lower.match_indices("http") {
                let tail = &lower[idx..];
                if !(tail.starts_with("http://") || tail.starts_with("https://")) {
                    continue;
                }
                // 外层 URL 的 query/path 里再嵌一个 URL（如 ?next=https://...）不算：
                // 只认文本层真正的链接起点，避免 example.com 的嵌套目标误吞系统通知。
                if lower[..idx]
                    .chars()
                    .next_back()
                    .is_some_and(|c| matches!(c, '=' | '?' | '&' | '/'))
                {
                    continue;
                }
                // URL 在正文/XML 里通常以空白、引号、尖括号或中文标点结束。
                let end = tail
                    .find(|c: char| {
                        c.is_whitespace()
                            || matches!(c, '"' | '\'' | '<' | '>' | '）' | '，' | '。' | '、')
                    })
                    .unwrap_or(tail.len());
                let candidate = tail[..end]
                    .trim_end_matches(|c: char| matches!(c, ')' | ']' | '}' | ',' | ';' | '.'));
                if let Ok(url) = reqwest::Url::parse(candidate) {
                    if url.host_str().is_some_and(|h| h.eq_ignore_ascii_case(HOST)) {
                        return true;
                    }
                }
            }
            false
        }

        ev.kind == "system"
            && (mentions_weixin110(&ev.content)
                || mentions_weixin110(&ev.raw_content)
                || ev.url.as_deref().is_some_and(mentions_weixin110))
    }

    /// 启动时绑定微信账号（best-effort；失败由首个事件的 account 兜底绑定）。
    ///
    /// 只信 `self/info` 的真实 `wxId`，不回退 `customWxId`（两字段独立读取，前者可为空
    /// 而后者有值；绑定到非真实账号会让后续正常事件被误判为换号而永久锁停）。
    async fn bind_account(&self) {
        match self.get_json("self/info", &[]).await {
            Ok((status, val)) if (200..300).contains(&status) => {
                let wxid = val
                    .get("wxId")
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty());
                if let Some(wxid) = wxid {
                    if let Ok(mut st) = self.state.lock() {
                        st.bind(wxid);
                    }
                } else {
                    tracing::info!("wxbot：self/info 的 wxId 为空，将由首个事件绑定真实账号");
                }
            }
            _ => tracing::info!("wxbot：启动绑定账号失败（self/info 不可达），将由首个事件绑定"),
        }
    }

    /// GET（带 Bearer），返回 `(status, body)`；非 JSON / 网络错误 → Err。
    async fn get_json(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, Value), ()> {
        let (status, body) = self.get_body(path, query).await?;
        match serde_json::from_str::<Value>(&body) {
            Ok(val) => Ok((status, val)),
            Err(e) => {
                tracing::warn!("wxbot 响应 JSON 解析失败 {}: {e}", self.api_url(path));
                Err(())
            }
        }
    }

    /// GET（带 Bearer），返回 `(status, 原始 body)`；网络错误 → Err。body 不保证是 JSON。
    async fn get_body(&self, path: &str, query: &[(&str, String)]) -> Result<(u16, String), ()> {
        let url = self.api_url(path);
        let mut req = self.http.get(&url).header("Authorization", self.bearer());
        for (k, v) in query {
            req = req.query(&[(k, v)]);
        }
        let resp = req.send().await.map_err(|e| {
            tracing::warn!("wxbot 请求失败 {url}: {e}");
        })?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| {
            tracing::warn!("wxbot 响应读取失败 {url}: {e}");
        })?;
        Ok((status, body))
    }

    /// POST JSON（带 Bearer），返回 `(status, body)`。
    async fn post_json(&self, path: &str, body: &Value) -> Result<(u16, Value), ()> {
        let url = self.api_url(path);
        match self
            .http
            .post(&url)
            .header("Authorization", self.bearer())
            .json(body)
            .send()
            .await
        {
            Ok(resp) => {
                let status = resp.status().as_u16();
                match resp.json::<Value>().await {
                    Ok(val) => Ok((status, val)),
                    Err(e) => {
                        tracing::warn!("wxbot 响应 JSON 解析失败 {url}: {e}");
                        Err(())
                    }
                }
            }
            Err(e) => {
                tracing::warn!("wxbot 请求失败 {url}: {e}");
                Err(())
            }
        }
    }

    /// 业务 POST（2xx 即成功），分片级有限重试（成功过的不重发）。
    ///
    /// **每次实际发送/重试前都过发送闸门**：换号锁停可能发生在一条回复的分片之间、
    /// 媒体上传/转码之后——闸门是所有发送路径的公共咽喉点，保证锁停立即生效。
    ///
    /// `pace=false` 仅用于语音转码等非消息请求：不占用发送间隔/窗口，但失败仍计入熔断。
    async fn post_json_ok(
        &self,
        path: &str,
        body: &Value,
        label: &str,
        pacer: &mut SendPacer,
        pace: bool,
    ) -> Result<(), ()> {
        use tokio::time::sleep;
        for attempt in 1..=self.cfg.max_failures {
            self.send_gate()?;
            if pace {
                // 重试也是一次真实出站动作：每次尝试都先过节流器（含间隔与窗口）。
                pacer.wait().await;
                // P0：节流可能睡数秒到一分钟，期间并发事件可能已换号锁停；
                // 必须重新过闸门，否则旧会话回复会从新绑定的账号发出去。
                self.send_gate()?;
            }
            match self.post_json(path, body).await {
                Ok((status, _)) if (200..300).contains(&status) => {
                    // 只有真正的消息发送成功才清零失败连击；转码成功不算消息送达。
                    if pace {
                        pacer.note_success();
                    }
                    return Ok(());
                }
                Ok((status, _)) => {
                    tracing::warn!(
                        "wxbot {label} 业务失败 HTTP={status} (第 {attempt}/{max} 次)",
                        max = self.cfg.max_failures,
                    );
                }
                Err(()) => {
                    tracing::warn!(
                        "wxbot {label} 请求失败 (第 {attempt}/{max} 次)",
                        max = self.cfg.max_failures,
                    );
                }
            }
            if attempt < self.cfg.max_failures {
                sleep(Duration::from_secs(self.cfg.retry_sec.max(1))).await;
            }
        }
        // 熔断只统计「整次发送最终失败」，不把同一次发送内部的 HTTP 重试逐次计数，
        // 否则一次网络抖动（max_failures=3）就会吃掉大半阈值。
        self.record_send_failure(pacer);
        Err(())
    }

    /// 兜底长轮询一次：`GET /api/messages/wait`。
    ///
    /// 成功时事件已入 ingest 队列并返回其 `seq`（游标推进语义：入队成功才推进）。
    async fn wait_once(
        &self,
        cursor: Option<u64>,
        queue: &mpsc::Sender<CollectorEvent>,
    ) -> Result<WaitOutcome, ()> {
        let mut query: Vec<(&str, String)> =
            vec![("timeout-ms", (self.cfg.poll_timeout_sec * 1000).to_string())];
        if let Some(c) = cursor {
            query.push(("cursor", c.to_string()));
        }
        let (status, body) = self.get_body("messages/wait", &query).await?;
        if !(200..300).contains(&status) {
            tracing::warn!("wxbot messages/wait HTTP {status}");
            return Err(());
        }
        // 200 + 空体/非 JSON = Ktor 保活层偶发空响应：视为本次无事件（游标不动），
        // 不按错误退避——否则会以 30s 为周期持续误报「连续 3 次失败」。
        let Some(val) = parse_wait_body(&body) else {
            return Ok(WaitOutcome::Idle(None));
        };
        if val.get("dropped").and_then(|v| v.as_u64()).unwrap_or(0) > 0 {
            tracing::warn!(
                "wxbot：wcx 收集器缓冲溢出，已丢弃 {} 条最旧事件（缺口不可补）",
                val.get("dropped").and_then(|v| v.as_u64()).unwrap_or(0)
            );
        }
        let Some(ev_val) = val.get("event") else {
            // 缺少 event 但可能带 head_seq（contract 里的空闲响应）：必须消费它，
            // 否则无 cursor 的轮询永远拿不到起点，之后也收不到任何兜底事件。
            return Ok(WaitOutcome::Idle(wait_idle_head(&val)));
        };
        if ev_val.is_null() {
            return Ok(WaitOutcome::Idle(wait_idle_head(&val)));
        }
        let ev: CollectorEvent = serde_json::from_value(ev_val.clone()).map_err(|e| {
            tracing::warn!("wxbot messages/wait 事件解析失败: {e}");
        })?;
        let seq = ev.seq;
        // 入队前账号准入（同 webhook 路径）：锁停立即置位；被拒事件直接丢弃，
        // 游标照常推进（通道已锁停，该事件永远不会被投递）。
        match self.state.lock() {
            Ok(mut s) => {
                s.bind(&ev.account);
                if !s.account_ok(&ev.account) {
                    return Ok(WaitOutcome::Got(seq));
                }
            }
            // 毒锁：不做准入，交由 ingest 复核兜底。
            Err(_) => {}
        }
        // 游标推进语义：入队成功才推进（入队失败重试同一条，游标不动）。
        enqueue_with_retry(queue, ev).await?;
        Ok(WaitOutcome::Got(seq))
    }

    /// 长轮询兜底主循环（带失败退避）。
    async fn poll_loop(self: Arc<Self>, queue: mpsc::Sender<CollectorEvent>) {
        let mut cursor: Option<u64> = None;
        let mut failures: u32 = 0;
        loop {
            match self.wait_once(cursor, &queue).await {
                Ok(WaitOutcome::Got(seq)) => {
                    cursor = Some(seq);
                    failures = 0;
                }
                Ok(WaitOutcome::Idle(head)) => {
                    // 正常长轮询超时、200 空体/非 JSON、缺失 event 的即时响应都归为
                    // Idle；统一按 retry_sec 间隔再拉，避免异常即时 200 打满 loopback。
                    // 有 head_seq 时消费为起点；无 cursor 首次调用靠它避免永远空转。
                    match (cursor, head) {
                        // 无 cursor 的首次调用：以 head_seq 为起点，不回放历史。
                        (None, h) => cursor = h,
                        // 对端 seq 回退（wcx/微信进程重启，收集器计数归零）：
                        // 旧游标永远等不到新事件，重置到新的 head 重新起步。
                        (Some(c), Some(h)) if h < c => {
                            tracing::warn!("wxbot：对端收集器 seq 回退（{c} → {h}，疑似 wcx 重启），游标已重置");
                            cursor = Some(h);
                        }
                        _ => {}
                    }
                    tokio::time::sleep(idle_retry_delay(self.cfg.retry_sec)).await;
                    failures = 0;
                }
                Err(()) => {
                    failures += 1;
                    if failures >= self.cfg.max_failures {
                        tracing::warn!(
                            "wxbot 兜底轮询：连续 {failures} 次失败，退避 {}s",
                            self.cfg.backoff_sec
                        );
                        tokio::time::sleep(Duration::from_secs(self.cfg.backoff_sec.max(1))).await;
                        failures = 0;
                    } else {
                        tokio::time::sleep(Duration::from_secs(self.cfg.retry_sec.max(1))).await;
                    }
                }
            }
        }
    }

    /// webhook 监听（主路）。绑定失败只影响主路，兜底路径继续。
    async fn webhook_server(self: Arc<Self>, queue: mpsc::Sender<CollectorEvent>) {
        use axum::routing::post;
        let state = WebhookState {
            queue,
            secret: Arc::new(self.cfg.webhook_secret.clone()),
            account: self.state.clone(),
        };
        let app = axum::Router::new()
            .route(&self.cfg.webhook_path, post(webhook_handler))
            .with_state(state);
        let listener = match tokio::net::TcpListener::bind(&self.cfg.webhook_listen).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(
                    "wxbot：webhook 监听绑定失败 {}（{e}）；仅兜底长轮询可用",
                    self.cfg.webhook_listen
                );
                return;
            }
        };
        tracing::info!(
            "wxbot：webhook 监听 http://{}{}",
            self.cfg.webhook_listen,
            self.cfg.webhook_path
        );
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("wxbot：webhook 服务退出（{e}）；仅兜底长轮询可用");
        }
    }

    /// 单一 ingest 任务：队列消费 → 去重 → 账号 → 回复门槛（白名单外群消息静默
    /// 落库后即返回；私聊未命中丢弃）→ 媒体 → 投递 → 提交。
    ///
    /// 全通道串行（顺序优先于吞吐）：媒体拉取最长 150s 会阻塞后续消息，可接受。
    async fn ingest_loop(
        self: Arc<Self>,
        mut queue: mpsc::Receiver<CollectorEvent>,
        inbound: mpsc::Sender<BusMessage>,
    ) {
        while let Some(ev) = queue.recv().await {
            self.ingest_one(ev, &inbound).await;
        }
    }

    async fn ingest_one(&self, ev: CollectorEvent, inbound: &mpsc::Sender<BusMessage>) {
        let key = effective_key(&ev);
        // 1) 去重（webhook/wait 双路、wcx 重试重发、乱序到达全部幂等）。
        {
            let mut st = match self.state.lock() {
                Ok(st) => st,
                Err(_) => return, // 毒锁（持锁线程 panic）：跳过本条，不投递
            };
            if st.contains(&key) {
                drop(st);
                tracing::debug!("wxbot：重复事件已丢弃 seq={} key={key}", ev.seq);
                return;
            }
            // 2) 账号绑定（启动 self/info 不可达时由首个带账号的事件兜底）+ 账号核对。
            st.bind(&ev.account);
            if !st.account_ok(&ev.account) {
                return;
            }
        }
        // 3) 回复门槛。私聊未命中直接丢弃（对陌生人零响应）；群消息未命中走
        //    「静默落库」：文本进本地索引（media_notes 空、不下载媒体），不投递
        //    LLM——记录与回复解耦（文档 §6.2.1）。落库尽力而为：webhook 入队即回
        //    200、wait 推进 cursor，双路都不会重放同一条事件，失败无法靠重投补救，
        //    仅 warn；账本仍提交（重复事件在去重处丢弃）。复核与落库同一临界区，
        //    不跨 await 持锁（锁停/换号后不落库，与步骤 5 的不变量一致）。
        if !self.allowed(&ev) {
            if ev.is_group && !ev.talker.is_empty() {
                let text = record_text(&ev);
                let mut st = match self.state.lock() {
                    Ok(st) => st,
                    Err(_) => return, // 毒锁：不落库不提交
                };
                if !st.account_ok(&ev.account) {
                    return;
                }
                self.db_record(&ev, &key, &[], true, &text);
                st.commit(key);
                tracing::debug!(
                    "wxbot：白名单外群消息已静默落库 talker={} type={}",
                    ev.talker,
                    ev.kind
                );
            } else {
                tracing::debug!(
                    "wxbot：白名单未命中已丢弃 talker={} type={}",
                    ev.talker,
                    ev.kind
                );
            }
            return;
        }
        // 4) 组装正文（媒体落盘在 ingest 内顺序执行，保序），并收集已落盘媒体标注
        //    （群引用回填索引用）。
        let mut media_notes: Vec<CachedMediaNote> = Vec::new();
        let Some(content) = self.build_content(&ev, &mut media_notes).await else {
            return;
        };
        // 4.5+5) 复核与落库同一临界区：媒体等待（单条最长 150s）期间锁停可能已置位
        //    （准入是入队前做的），复核不过则不落库不归档直接返回——「锁停后收发全部
        //    拒绝」覆盖在途处理；静默接收在同一临界区提交账本（双路重复不重复下载）。
        //    sqlite 写在 state 锁内是同步毫秒级单行操作（模块头注释的既定范式）。
        //    系统风控提示（账号受限等）只归档不投递：它会随每次被拦的发送重复到达，
        //    回复即自激循环（2026-10-30 实测 157 连发加重账号风控）。
        let reaches = (!ev.is_group || self.group_reaches_llm(&ev)) && !Self::is_risk_notice(&ev);
        {
            let mut st = match self.state.lock() {
                Ok(st) => st,
                Err(_) => return, // 毒锁：不投递
            };
            if !st.account_ok(&ev.account) {
                return;
            }
            // 落库文本与白名单外路径同用 record_text：text 类两路等价，媒体/链接
            // 类带类型标注（content 常为空的图片不再落成空行、链接不丢标题/URL），
            // 引用回填（db_quote_lookup）拿到的是消息自身的展示文本。
            self.db_record(&ev, &key, &media_notes, !reaches, &record_text(&ev));
            if !reaches {
                st.commit(key);
                return;
            }
        }
        // 6) 投递：reserve 拿到容量（背压等待在锁外），随后单一临界区内复核 →
        //    同步 permit.send → 提交账本——复核与投递之间无解锁窗口，锁停置位
        //    不可能插进这两步之间。
        let chat_id = inbound_chat_id(&ev);
        let permit = match inbound.reserve().await {
            Ok(p) => p,
            Err(_) => return, // 通道关闭（进程退出）
        };
        {
            let mut st = match self.state.lock() {
                Ok(st) => st,
                Err(_) => return, // 毒锁：不投递（permit 随作用域释放，容量归还）
            };
            if !st.account_ok(&ev.account) {
                return;
            }
            // 容量已由 reserve 保证；接收端仅在进程退出时关闭，close 后 send 的
            // panic 等价于退出路径，无需失败分支。
            permit.send(BusMessage::new(CHANNEL_WXBOT, &chat_id, content));
            // 7) 投递成功才提交账本（失败可重投，对齐 weixin poll_once 游标纪律）。
            st.commit(key);
        }
    }

    /// 组装投递给 agent 的正文：文本直接用；媒体类型按 §5 落盘/标注；失败降级纯标注。
    /// 群消息同时把已落盘媒体写入 `media_notes`（引用回填索引）。
    async fn build_content(
        &self,
        ev: &CollectorEvent,
        media_notes: &mut Vec<CachedMediaNote>,
    ) -> Option<String> {
        let mut parts: Vec<String> = Vec::new();
        // 群聊带发送者（让 agent 知道谁在说话）；解析失败（unknown）不带。
        if ev.is_group && !ev.sender.is_empty() && ev.sender != "unknown" {
            parts.push(format!("[来自 {}]", ev.sender));
        }
        let title = ev.title.clone().unwrap_or_default();
        match ev.kind.as_str() {
            "text" => parts.push(ev.content.clone()),
            "image" => {
                let (s, note) = self
                    .media_annotation(ev, "image", "[图片]", None, &title)
                    .await;
                media_notes.extend(note);
                parts.push(s);
            }
            "voice" => {
                let (s, note) = self
                    .media_annotation(ev, "voice", "[语音]", None, &title)
                    .await;
                media_notes.extend(note);
                parts.push(s);
            }
            "sticker" => {
                let (s, note) = self
                    .media_annotation(ev, "sticker", "[表情]", None, &title)
                    .await;
                media_notes.extend(note);
                parts.push(s);
            }
            // 视频体量大，默认不拉取，仅标注。
            "video" => parts.push(plain_label("[视频]", &title, &ev.content)),
            "file" => {
                let label = if title.is_empty() {
                    "[文件]".to_string()
                } else {
                    format!("[文件: {title}]")
                };
                let (s, note) = self
                    .media_annotation(ev, "file", &label, Some(&ev.talker), &title)
                    .await;
                media_notes.extend(note);
                parts.push(s);
            }
            "link" => {
                let mut s = format!("[链接] {title}");
                if let Some(u) = &ev.url {
                    s.push(' ');
                    s.push_str(u);
                }
                if let Some(d) = &ev.des {
                    s.push(' ');
                    s.push_str(d);
                }
                parts.push(s);
            }
            "miniprogram" => parts.push(plain_label("[小程序]", &title, &ev.content)),
            "quote" => {
                parts.push(plain_label("[引用]", &title, &ev.content));
                // 引用回填（napcat 同款，私聊/群聊都生效）：被引用消息此前已落盘的话，
                // 从持久化库点查其正文/媒体标注注入正文，agent 不缺上下文。
                // 账号未知（事件未带 account）时禁用——无法做账号隔离，宁可不回填。
                if ev.quote_svrid != 0 && !ev.account.is_empty() {
                    let chat_id = inbound_chat_id(ev);
                    if let Some(cached) =
                        self.db_quote_lookup(&ev.account, &chat_id, ev.quote_svrid)
                    {
                        parts.push(format_cached_text_note(&cached));
                        for m in &cached.media {
                            parts.push(format_cached_media_note(m, "引用"));
                        }
                    }
                }
            }
            "card" => parts.push(plain_label("[名片]", &title, &ev.content)),
            "location" => parts.push(plain_label("[位置]", &title, &ev.content)),
            "red_packet" => parts.push(plain_label("[红包]", &title, &ev.content)),
            "transfer" => parts.push(plain_label("[转账]", &title, &ev.content)),
            "video_account" => parts.push(plain_label("[视频号]", &title, &ev.content)),
            "system" => parts.push(plain_label("[系统]", &title, &ev.content)),
            "recall" => parts.push(plain_label("[撤回]", &title, &ev.content)),
            _ => parts.push(format!(
                "[消息 type_code={}] {}",
                ev.type_code,
                truncate(&ev.raw_content, 200)
            )),
        }
        let text = parts.join("\n");
        let text = text.trim();
        if text.is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    }

    // -------------------------------------------------------------------------
    // 入站消息持久化（SQLite；私聊/群聊全量落盘）
    // -------------------------------------------------------------------------

    /// 记录一条入站消息（含静默接收/白名单外静默落库的群消息：`archived=1`）。
    ///
    /// `INSERT OR IGNORE` + dedup_key 唯一约束：webhook/wait 双路与 wcx 重试的重复
    /// 投递天然幂等。失败仅告警——持久化是尽力而为，绝不阻断收消息（webhook 入队
    /// 即回 200、wait 推进 cursor，双路都不会重放同一条事件，失败无法靠重投补救）。
    /// 私聊与群聊都落盘：引用回填（quote）在两种会话里都可能指向任何一条历史消息。
    fn db_record(
        &self,
        ev: &CollectorEvent,
        dedup_key: &str,
        media_notes: &[CachedMediaNote],
        archived: bool,
        text: &str,
    ) {
        let Some(db) = &self.db else { return };
        let media_json = serde_json::to_string(media_notes).unwrap_or_else(|_| "[]".into());
        let (text, _) = trim_text_for_index(text);
        let chat_id = inbound_chat_id(ev);
        let result = match db.lock() {
            Ok(conn) => conn
                .execute(
                    "INSERT OR IGNORE INTO inbound_messages
                     (dedup_key, account, chat_id, is_group, sender, svr_id, kind, text, media_json, archived, create_time)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                    rusqlite::params![
                        dedup_key,
                        ev.account,
                        chat_id,
                        ev.is_group,
                        ev.sender,
                        ev.svr_id as i64,
                        ev.kind,
                        text,
                        media_json,
                        archived,
                        ev.create_time
                    ],
                )
                .map(|_| ()),
            Err(_) => return, // 毒锁：跳过本条记录。
        };
        if let Err(e) = result {
            tracing::warn!("wxbot：消息落库失败（尽力而为，不阻断）: {e}");
        }
    }

    /// 引用回填点查：同账号、同会话内按被引用消息的 svrid 取最近一条记录
    /// （正文 + 已落盘媒体）。账号隔离：换号后不得回填上一账号留存的历史。
    fn db_quote_lookup(&self, account: &str, chat_id: &str, svr_id: u64) -> Option<CachedText> {
        let db = self.db.as_ref()?;
        let conn = db.lock().ok()?;
        let (sender, text, media_json): (String, String, String) = conn
            .query_row(
                "SELECT sender, text, media_json FROM inbound_messages
                 WHERE account = ?1 AND chat_id = ?2 AND svr_id = ?3
                 ORDER BY id DESC LIMIT 1",
                rusqlite::params![account, chat_id, svr_id as i64],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .ok()
            .flatten()?;
        let media: Vec<CachedMediaNote> = serde_json::from_str(&media_json).unwrap_or_default();
        let _ = sender; // napcat 的引用标注不带原发送者（正文已有各自的 [来自 x]），留库不回填。
        Some(CachedText {
            text,
            truncated: false,
            media,
        })
    }

    /// 把单个入站媒体事件转成（标注文本, 已落盘媒体缓存条目）。
    async fn media_annotation(
        &self,
        ev: &CollectorEvent,
        raw_type: &str,
        label: &str,
        talker: Option<&str>,
        title: &str,
    ) -> (String, Option<CachedMediaNote>) {
        if self.cfg.media_dir.as_os_str().is_empty() {
            return (format!("{label}（未配置媒体目录，仅通知）"), None);
        }
        if ev.svr_id == 0 {
            return (format!("{label}（无服务端 id，无法下载）"), None);
        }
        match self.fetch_media(ev, raw_type, talker, title).await {
            Some(path) => {
                let p = path.to_string_lossy().replace('\\', "/");
                let hint = if raw_type == "file" {
                    "（可用 read 读取文本内容）"
                } else {
                    ""
                };
                let note = CachedMediaNote {
                    kind_label: kind_label_for(raw_type).to_string(),
                    name: if title.is_empty() {
                        None
                    } else {
                        Some(title.to_string())
                    },
                    path: path.clone(),
                };
                (format!("{label} 已下载到 {p}{hint}"), Some(note))
            }
            None => (format!("{label}（下载失败，仅通知）"), None),
        }
    }

    /// `GET /api/media/raw` 拉字节流落盘。服务端会等待文件就绪（最长 ~120s），
    /// 客户端预算 150s 盖过它，失败不重试（避免触发重复下载）。
    async fn fetch_media(
        &self,
        ev: &CollectorEvent,
        raw_type: &str,
        talker: Option<&str>,
        title: &str,
    ) -> Option<PathBuf> {
        let mut query: Vec<(&str, String)> = vec![
            ("msg-svr-id", ev.svr_id.to_string()),
            ("type", raw_type.to_string()),
        ];
        if let Some(t) = talker {
            query.push(("talker", t.to_string()));
        }
        let url = self.api_url("media/raw");
        let resp = self
            .media_http
            .get(&url)
            .query(&query)
            .header("Authorization", self.bearer())
            .send()
            .await
            .ok()?;
        if !resp.status().is_success() {
            tracing::warn!("wxbot media/raw({raw_type}) HTTP {}", resp.status());
            return None;
        }
        if let Some(len) = resp.content_length().filter(|len| *len > MEDIA_MAX_BYTES) {
            tracing::warn!("wxbot 媒体 {len} 字节超过上限 {MEDIA_MAX_BYTES}，跳过");
            return None;
        }
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("wxbot 媒体下载失败 {raw_type}: {e}");
                return None;
            }
        };
        if bytes.len() as u64 > MEDIA_MAX_BYTES {
            tracing::warn!("wxbot 媒体超过上限 {MEDIA_MAX_BYTES} 字节，已丢弃");
            return None;
        }
        if bytes.is_empty() {
            return None;
        }
        let dir = self.cfg.media_dir.join("wxbot");
        if std::fs::create_dir_all(&dir).is_err() {
            tracing::warn!("wxbot：无法创建媒体目录 {}", dir.display());
            return None;
        }
        // 文件名用 dedup_key 哈希（跨 wcx 重启稳定）而非 seq：wcx 重启后 seq 归零，
        // 不同消息的裸 seq 会撞名互相覆盖，历史标注路径将指向新内容。
        // 图片/表情按字节魔数定扩展名（wcx 不统一转码，PNG/WebP 原样透传，
        // 固定 .jpg 会让 MIME 与内容不符）；识别不了沿用类型推断。
        let base = sanitize_filename(if title.is_empty() { raw_type } else { title });
        let ext = match raw_type {
            "image" | "sticker" => sniff_image_ext(&bytes)
                .map(str::to_string)
                .unwrap_or_else(|| ext_for_media(raw_type, Some(title))),
            _ => ext_for_media(raw_type, Some(title)),
        };
        let fname = format!(
            "{:016x}_{base}.{ext}",
            media_file_hash(&effective_key(ev), raw_type, title),
        );
        let path = dir.join(fname);
        match std::fs::write(&path, &bytes) {
            Ok(()) => Some(path),
            Err(e) => {
                tracing::warn!("wxbot：媒体落盘失败 {}: {e}", path.display());
                None
            }
        }
    }

    /// 发送一条文本回复（UTF-8 安全分段，分片级重试——成功过的分片不重发）。
    async fn send_text(&self, conv_id: &str, text: &str, pacer: &mut SendPacer) -> Result<(), ()> {
        for chunk in utf8_chunks(text, self.cfg.send_chunk_bytes) {
            self.post_json_ok(
                "messages/text",
                &json!({ "type": "text", "convId": conv_id, "content": chunk }),
                &format!("文本发送 → {conv_id}"),
                pacer,
                true,
            )
            .await?;
        }
        Ok(())
    }

    /// 发送一个媒体附件：upload 到 WCX 侧 → 按类型发送（语音走 mp3→silk 转码链）。
    async fn send_media(
        &self,
        conv_id: &str,
        intent: &MediaIntent,
        pacer: &mut SendPacer,
    ) -> Result<(), ()> {
        // 安全校验：路径必须落在 media_dir 沙箱内（防 agent 经 marker 外发任意文件）。
        let safe_path = match resolve_media_path(&self.cfg.media_dir, &intent.path) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "wxbot 媒体发送：路径不在 media_dir 沙箱内，拒绝: {}",
                    intent.path
                );
                return Err(());
            }
        };
        if std::fs::metadata(&safe_path).map(|m| m.len()).unwrap_or(0) == 0 {
            tracing::warn!("wxbot 媒体发送：文件为空或不可读 {}", intent.path);
            return Err(());
        }
        // 语音须 silk：仅接受 mp3 源（mp3-to-silk 的输入格式；wav 等直接失败）。
        // 在 upload 之前检查，避免不支持的源白白产生 WCX 侧临时文件。
        if intent.kind == MediaKind::Voice {
            let ext = safe_path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if ext != "mp3" {
                tracing::warn!(
                    "wxbot 语音发送：仅支持 mp3 源（收到 .{ext}），已拒绝: {}",
                    intent.path
                );
                return Err(());
            }
        }
        // 1) 上传（回环本地上传，单次尝试；同样过发送闸门）。
        self.send_gate()?;
        let uploaded = match self.upload_media(&safe_path).await {
            Ok(p) => p,
            Err(()) => {
                // 上传失败也属于一次终态发送失败（HTTP 非 2xx/网络/本地读取）。
                self.record_send_failure(pacer);
                return Err(());
            }
        };
        // 2) 按类型发送（有限重试；post_json_ok 内部每次尝试都过闸门）。
        let label = format!("{:?} 发送 → {conv_id}", intent.kind);
        let sent = match intent.kind {
            MediaKind::Image => {
                self.post_json_ok(
                    "messages/image",
                    &json!({ "convId": conv_id, "path": uploaded }),
                    &label,
                    pacer,
                    true,
                )
                .await
            }
            MediaKind::Video => {
                self.post_json_ok(
                    "messages/video",
                    &json!({ "convId": conv_id, "path": uploaded }),
                    &label,
                    pacer,
                    true,
                )
                .await
            }
            MediaKind::File => {
                let name = intent
                    .name
                    .clone()
                    .unwrap_or_else(|| path_file_name(&intent.path));
                self.post_json_ok(
                    "messages/file",
                    &json!({ "convId": conv_id, "path": uploaded, "title": name }),
                    &label,
                    pacer,
                    true,
                )
                .await
            }
            MediaKind::Voice => {
                // mp3 上传后在 WCX 侧转码为 silk（REST 只接受 WCX 本地路径），再取时长发送。
                let silk_path = format!("{uploaded}.silk");
                let convert = self
                    .post_json_ok(
                        "utils/audio/mp3-to-silk",
                        &json!({ "srcPath": uploaded, "destPath": silk_path }),
                        &label,
                        pacer,
                        false,
                    )
                    .await;
                if convert.is_err() {
                    // 转码失败：该附件 dead-letter。统一收尾（下方 Voice 分支不再执行），
                    // mp3 源与 .silk 产物都要尽力删——服务端可能已转码成功但响应丢失。
                    self.temp_delete(&uploaded).await;
                    self.temp_delete(&silk_path).await;
                    return Err(());
                }
                let duration_ms = self.audio_duration_ms(&silk_path).await;
                let sent = self
                    .post_json_ok(
                        "messages/voice",
                        &json!({ "convId": conv_id, "path": silk_path, "durationMs": duration_ms }),
                        &label,
                        pacer,
                        true,
                    )
                    .await;
                sent
            }
        };
        // 语音链路收尾：无论成功失败，尽力清理 WCX 侧临时件（mp3 源 + silk 产物）。
        // temp-delete 有严格白名单（仅 moduleCache 的 upload_* 文件），失败不影响结果。
        if intent.kind == MediaKind::Voice {
            self.temp_delete(&uploaded).await;
            self.temp_delete(&format!("{uploaded}.silk")).await;
        }
        sent
    }

    /// 尽力删除 WCX 侧上传临时件（结果忽略，仅记 debug）。
    async fn temp_delete(&self, path: &str) {
        match self
            .post_json("media/temp-delete", &json!({ "path": path }))
            .await
        {
            Ok((status, _)) if (200..300).contains(&status) => {}
            other => tracing::debug!("wxbot temp-delete({path}) 未成功: {other:?}"),
        }
    }

    /// multipart 上传文件到 WCX，返回 WCX 侧落盘路径（响应 `{"path": "..."}`）。
    async fn upload_media(&self, path: &Path) -> Result<String, ()> {
        let bytes = std::fs::read(path).map_err(|e| {
            tracing::warn!("wxbot 媒体发送：读取失败 {}: {e}", path.display());
        })?;
        let name = path_file_name(&path.to_string_lossy());
        let part = reqwest::multipart::Part::bytes(bytes).file_name(name);
        let form = reqwest::multipart::Form::new().part("file", part);
        let url = self.api_url("media/upload");
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.bearer())
            .multipart(form)
            .send()
            .await
            .map_err(|e| tracing::warn!("wxbot 媒体上传失败 {}: {e}", path.display()))?;
        let status = resp.status();
        if !status.is_success() {
            tracing::warn!("wxbot 媒体上传 HTTP {status}");
            return Err(());
        }
        let val: Value = resp.json().await.map_err(|e| {
            tracing::warn!("wxbot 媒体上传响应解析失败: {e}");
        })?;
        let uploaded = val
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        match uploaded {
            Some(p) => Ok(p),
            None => {
                tracing::warn!("wxbot 媒体上传响应缺 path: {val}");
                Err(())
            }
        }
    }

    /// 取音频时长（ms）；端点返回裸数字或对象，解析失败回退 1000。
    async fn audio_duration_ms(&self, path: &str) -> i64 {
        match self
            .post_json("utils/audio/duration", &json!({ "path": path }))
            .await
        {
            Ok((status, val)) if (200..300).contains(&status) => {
                if let Some(n) = val.as_i64().filter(|n| *n > 0) {
                    return n.clamp(1, 60_000);
                }
                for k in ["durationMs", "duration", "value"] {
                    if let Some(n) = val.get(k).and_then(|v| v.as_i64()).filter(|n| *n > 0) {
                        return n.clamp(1, 60_000);
                    }
                }
                1000
            }
            _ => 1000,
        }
    }

    /// 发送循环：消费 outbound 回复发回微信（全通道串行，与 weixin 一致）。
    async fn send_loop(self: Arc<Self>, mut outbound: OutboundReceiver) {
        // 单消费者局部限速器：文本分片/媒体/语音（含重试）统一从这里过。
        let mut pacer = SendPacer::new(&self.cfg);
        while let Some(msg) = outbound.recv().await {
            // chat_id 前缀 → conv id（user:/group: 前缀 + 账号段一并剥掉）。
            let (chat_account, conv_id) = match parse_chat_id(&msg.chat_id) {
                Some((account, c)) if !c.is_empty() => (account, c.to_string()),
                _ => {
                    tracing::warn!("wxbot：无法识别的 chat_id，已跳过发送: {}", msg.chat_id);
                    continue;
                }
            };
            // 出站归属校验：带账号段的会话必须与当前绑定一致——定时任务/延迟回复可能
            // 跨换号幸存，凭旧账号段的 chat_id 找上门时不得经新账号发出（send_gate
            // 已保证此处绑定非空；毒锁按拒发处理）。
            if let Some(a) = chat_account {
                let owned = self.state.lock().map(|st| st.account == a).unwrap_or(false);
                if !owned {
                    tracing::warn!(
                        "wxbot：会话归属账号 {a} 与当前绑定不一致，拒绝发送: {}",
                        msg.chat_id
                    );
                    continue;
                }
            }
            let (intents, text) = parse_media_markers(&msg.content);
            let mut failed = false;
            // 1) 先发文本（若非空）。
            if !text.trim().is_empty() && self.send_text(&conv_id, &text, &mut pacer).await.is_err()
            {
                failed = true;
            }
            // 2) 再逐个发媒体（caption 作为前置文本）。
            for intent in &intents {
                if let Some(cap) = intent.caption.as_deref() {
                    if !cap.trim().is_empty()
                        && self.send_text(&conv_id, cap, &mut pacer).await.is_err()
                    {
                        failed = true;
                    }
                }
                if self.send_media(&conv_id, intent, &mut pacer).await.is_err() {
                    failed = true;
                }
            }
            if failed {
                tracing::warn!(
                    "wxbot 回复（部分）最终失败 → {}: {}",
                    msg.chat_id,
                    truncate(&msg.content, 80)
                );
            }
        }
    }
}

#[derive(Debug)]
enum WaitOutcome {
    /// 事件已入 ingest 队列，游标推进到该 seq。
    Got(u64),
    /// 无新事件（超时 / 首次探测）。携带 head_seq（仅首次探测时有意义）。
    Idle(Option<u64>),
}

/// 解析 `/api/messages/wait` 的 200 body；空体/非 JSON 返回 None（按空闲处理）。
///
/// 独立成函数便于单测：这是「保活空体不触发退避」行为的唯一入口。
fn parse_wait_body(body: &str) -> Option<Value> {
    if body.trim().is_empty() {
        tracing::debug!("wxbot messages/wait 响应为空体（视为空闲）");
        return None;
    }
    match serde_json::from_str(body) {
        Ok(v) => Some(v),
        Err(e) => {
            // 空体/非 JSON 都按空闲处理，避免 200 HTML 错误页每 2s 刷 warn 日志。
            tracing::debug!("wxbot messages/wait 响应非 JSON（视为空闲）: {e}");
            None
        }
    }
}

/// 从空闲响应里取 `head_seq`；无字段/类型不对 → None。
fn wait_idle_head(val: &Value) -> Option<u64> {
    val.get("head_seq").and_then(|v| v.as_u64())
}

/// 每次空闲都按 retry_sec 间隔重试：正常长轮询已阻塞到超时，额外 2s 可忽略；
/// 异常即时 200（空体/缺 event/只有 head_seq）不会变成 loopback 热循环。
/// `retry_sec=0` 也要保底 1s（配置层另有钳制，这里防直接构造绕过）。
fn idle_retry_delay(retry_sec: u64) -> Duration {
    Duration::from_secs(retry_sec.max(1))
}

/// 出站拟人化限速器的滚动窗口长度：60s。
const PACER_WINDOW: Duration = Duration::from_secs(60);

/// wxbot 出站限速器：单消费者（send_loop）持有的局部状态，无锁。
///
/// 每次实际发送（文本分片/媒体/语音，含重试）前调用 [`Self::wait`]：
/// 1. 相邻发送最小间隔 + 随机抖动（首条不等待）；
/// 2. 60s 滑动窗口内最多 `send_window_max` 条，超限等到最早一条出窗；
/// 3. 连续发送失败达到 `send_failure_fuse` 后由调用方触发账号锁停（0=禁用）。
struct SendPacer {
    min_gap_sec: u64,
    jitter_sec: u64,
    window_max: usize,
    fuse: u32,
    last_send: Option<Instant>,
    /// 已发送时间戳（升序）；只保留 60s 窗口内的条目。
    window: VecDeque<Instant>,
    failures: u32,
}

/// 配置层异常大间隔的硬上限：避免 `Duration`/`Instant` 加法溢出与超长阻塞。
const PACER_MAX_GAP_SEC: u64 = 3600;

impl SendPacer {
    fn new(cfg: &WxbotConfig) -> Self {
        let min_gap_sec = cfg.send_min_gap_sec.min(PACER_MAX_GAP_SEC);
        let jitter_sec = cfg.send_jitter_sec.min(PACER_MAX_GAP_SEC);
        if min_gap_sec != cfg.send_min_gap_sec || jitter_sec != cfg.send_jitter_sec {
            tracing::warn!(
                "wxbot 发送间隔配置过大（min={} jitter={}），已钳到 {PACER_MAX_GAP_SEC}s",
                cfg.send_min_gap_sec,
                cfg.send_jitter_sec
            );
        }
        Self {
            min_gap_sec,
            jitter_sec,
            window_max: cfg.send_window_max,
            fuse: cfg.send_failure_fuse,
            last_send: None,
            window: VecDeque::new(),
            failures: 0,
        }
    }

    /// 纯函数：给定抖动采样值，计算距离下一次可发送还需等待多久。
    fn next_delay(&self, now: Instant, jitter_sample: u64) -> Duration {
        let gap_secs = self
            .min_gap_sec
            .saturating_add(jitter_sample.min(self.jitter_sec));
        let gap = Duration::from_secs(gap_secs);
        let mut delay = self
            .last_send
            .and_then(|last| last.checked_add(gap))
            .and_then(|deadline| deadline.checked_duration_since(now))
            .unwrap_or_default();

        if self.window_max > 0 {
            // Instant 进程运行不足 60s 时 checked_sub 返回 None：此时窗口内所有
            // 记录都处于最近 60s，不能把 cutoff 退化成 now 而丢掉窗口上限。
            let cutoff = now.checked_sub(PACER_WINDOW);
            let in_window = |t: &Instant| cutoff.map_or(true, |c| *t > c);
            let active = self.window.iter().filter(|t| in_window(t)).count();
            if active >= self.window_max {
                if let Some(oldest) = self.window.iter().find(|t| in_window(t)) {
                    if let Some(d) = oldest
                        .checked_add(PACER_WINDOW)
                        .and_then(|deadline| deadline.checked_duration_since(now))
                    {
                        delay = delay.max(d);
                    }
                }
            }
        }
        delay
    }

    /// 等到允许发送，并把本次发送时间记入窗口。首条不等待。
    async fn wait(&mut self) {
        let sample = if self.jitter_sec == 0 {
            0
        } else {
            rand::thread_rng().gen_range(0..=self.jitter_sec)
        };
        let delay = self.next_delay(Instant::now(), sample);
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        self.record(Instant::now());
    }

    fn record(&mut self, now: Instant) {
        // Instant 太新（进程不足 60s）时没有可裁剪的过期项，保留全部。
        if let Some(cutoff) = now.checked_sub(PACER_WINDOW) {
            self.window.retain(|t| *t > cutoff);
        }
        self.window.push_back(now);
        self.last_send = Some(now);
    }

    fn note_success(&mut self) {
        self.failures = 0;
    }

    /// 记录一次终态发送失败；达到阈值返回 true（调用方负责锁停）。
    fn note_failure(&mut self) -> bool {
        self.failures = self.failures.saturating_add(1);
        self.fuse > 0 && self.failures >= self.fuse
    }
}

/// axum webhook handler 的共享状态。
#[derive(Clone)]
struct WebhookState {
    queue: mpsc::Sender<CollectorEvent>,
    secret: Arc<String>,
    /// 与 `Wxbot.state` 同源的账号状态：入队前先做账号准入（换号锁停立即生效，
    /// 不等可能被媒体下载阻塞数十秒的串行 ingest）。
    account: Arc<Mutex<WxbotState>>,
}

async fn webhook_handler(
    axum::extract::State(st): axum::extract::State<WebhookState>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::http::{header, StatusCode};
    use axum::response::IntoResponse;
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .unwrap_or_default();
    if presented != st.secret.as_str() {
        return (StatusCode::UNAUTHORIZED, "unauthorized").into_response();
    }
    let ev: CollectorEvent = match serde_json::from_str(&body) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!("wxbot webhook：事件解析失败（400，对端不重试）: {e}");
            return (StatusCode::BAD_REQUEST, "bad event").into_response();
        }
    };
    match ev.event.as_str() {
        "test" => {
            tracing::info!("wxbot webhook 测试事件收到（wcx 测试推送联通）");
            (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response()
        }
        "message" => {
            // 入队前账号准入：串行 ingest 可能被媒体下载阻塞数十秒，换号锁停必须
            // 在这里（锁内）立即置位，让并发发送分支的 send_gate 马上看到。
            // ingest 内保留复核（防准入后、入队前的窗口外变化）。
            let admitted = match st.account.lock() {
                Ok(mut s) => {
                    s.bind(&ev.account);
                    s.account_ok(&ev.account)
                }
                Err(_) => true, // 毒锁：交由 ingest 复核兜底
            };
            if !admitted {
                // 已锁停：确认但不入队（不投递，也不让 wcx 反复重试同一条）。
                return (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response();
            }
            match st.queue.try_send(ev) {
                Ok(()) => (StatusCode::OK, axum::Json(json!({ "ok": true }))).into_response(),
                // 入队即确认、满则让对端重试：503 在 wcx 侧按可重试处理。
                Err(_) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    axum::Json(json!({ "ok": false, "reason": "queue full" })),
                )
                    .into_response(),
            }
        }
        other => {
            tracing::warn!("wxbot webhook：未知事件类型 `{other}`（400）");
            (StatusCode::BAD_REQUEST, "unknown event").into_response()
        }
    }
}

/// 事件入 ingest 队列：满则短暂等待重试（poll 路径不能丢事件；游标未推进，
/// 下一轮 wait 会重新拿到同一条）。队列关闭（进程退出）→ Err。
async fn enqueue_with_retry(
    queue: &mpsc::Sender<CollectorEvent>,
    ev: CollectorEvent,
) -> Result<(), ()> {
    loop {
        match queue.try_send(ev.clone()) {
            Ok(()) => return Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(mpsc::error::TrySendError::Closed(_)) => return Err(()),
        }
    }
}

/// 入站会话地址（chat_id）：`user:<account>:<talker>` / `group:<account>:<talker>`；
/// 账号未知时退化为 `user:<talker>` / `group:<talker>`。
///
/// 账号段让运行时会话/历史/定时任务按微信账号隔离：换号（同联系人/群）重启后不再
/// 复用上一账号的上下文与工具结果——锁停闩管运行中，这里管跨重启的持久化历史。
/// 对外与 napcat 的 `user:<talker>` 约定因此有差异，见实施文档 §6.2。
fn inbound_chat_id(ev: &CollectorEvent) -> String {
    let kind = if ev.is_group { "group" } else { "user" };
    if ev.account.is_empty() {
        format!("{kind}:{}", ev.talker)
    } else {
        format!("{kind}:{}:{}", ev.account, ev.talker)
    }
}

/// chat_id → `(账号段, conv_id)`：剥 `user:` / `group:` 前缀后，若还有账号段
/// （`<account>:<talker>`）带出供发送归属校验——账号段只用于会话隔离与归属校验，
/// conv id 始终是纯 talker。legacy 形态（无账号段）账号段为 None。
/// wxid / 群 id 不含 `:`，`split_once` 不会误切。
fn parse_chat_id(chat_id: &str) -> Option<(Option<&str>, &str)> {
    let rest = chat_id
        .strip_prefix("user:")
        .or_else(|| chat_id.strip_prefix("group:"))?;
    Some(match rest.split_once(':') {
        Some((account, talker)) => (Some(account), talker),
        None => (None, rest),
    })
}

/// 事件的有效去重键：wcx 生成的 `dedup_key`；缺失时本地退化合成
/// （内容哈希兜底——两路拿到的同一事件内容一致，键即一致）。
fn effective_key(ev: &CollectorEvent) -> String {
    if !ev.dedup_key.is_empty() {
        return ev.dedup_key.clone();
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ev.account.hash(&mut h);
    ev.talker.hash(&mut h);
    ev.create_time.hash(&mut h);
    ev.seq.hash(&mut h);
    ev.raw_content.hash(&mut h);
    format!("f:{}:{}", ev.account, h.finish())
}

/// `url` 的 host 是否为回环（127.0.0.1 / [::1] / localhost，大小写不敏感）。
fn is_loopback_host(host: &str) -> bool {
    let h = host
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    matches!(h.as_str(), "127.0.0.1" | "::1" | "localhost")
}

/// `api_base`（URL）的 host 必须回环。
fn is_loopback_url(url: &str) -> bool {
    match reqwest::Url::parse(url.trim()) {
        Ok(u) => u.host_str().is_some_and(is_loopback_host),
        Err(_) => false,
    }
}

/// `webhook_listen`（host:port）的 host 必须回环。IPv6 需写成 `[::1]:38081`。
fn is_loopback_listen(listen: &str) -> bool {
    let s = listen.trim();
    if s.starts_with('[') {
        // [::1]:38081 → 取 `]` 之前。
        return s
            .find(']')
            .and_then(|i| s.get(1..i))
            .is_some_and(is_loopback_host);
    }
    match s.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => {
            is_loopback_host(host)
        }
        _ => is_loopback_host(s),
    }
}

/// 打开（或初始化）入站消息持久化库：`media_dir/wxbot/message_index.db`。
///
/// media_dir 为空（持久化未启用）或打不开时返回 None——回填与落盘降级、通道照常
/// 工作。WAL + `INSERT OR IGNORE`（dedup_key 唯一）：双路重复投递天然幂等。
fn open_message_db(media_dir: &Path) -> Option<Mutex<Connection>> {
    if media_dir.as_os_str().is_empty() {
        return None;
    }
    let dir = media_dir.join("wxbot");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(
            "wxbot：无法创建持久化目录 {}: {e}（索引与归档降级）",
            dir.display()
        );
        return None;
    }
    let path = dir.join("message_index.db");
    let conn = match Connection::open(&path) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                "wxbot：无法打开消息持久化库 {}: {e}（索引与归档降级）",
                path.display()
            );
            return None;
        }
    };
    if let Err(e) = conn.execute_batch("PRAGMA journal_mode = WAL;") {
        tracing::warn!("wxbot：消息库 WAL 设置失败（继续用默认日志模式）: {e}");
    }
    // v1 → v2 迁移：v1 表没有 account 列（引用回填可能跨账号命中历史）。索引进库的
    // 数据会随消息流自然重建，直接弃旧表最简且安全。
    let has_account: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('inbound_messages') WHERE name = 'account'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(1); // 表不存在 → 查询报错 → 视为无需迁移，交由 DB_SCHEMA 建表。
    if has_account == 0 {
        tracing::info!("wxbot：消息库 v1 → v2 迁移（补 account 隔离列），旧索引弃置重建");
        if let Err(e) = conn.execute_batch("DROP TABLE IF EXISTS inbound_messages;") {
            tracing::warn!("wxbot：v1 表弃置失败（索引与归档降级）: {e}");
            return None;
        }
    }
    if let Err(e) = conn.execute_batch(DB_SCHEMA) {
        tracing::warn!("wxbot：消息库 schema 初始化失败（索引与归档降级）: {e}");
        return None;
    }
    Some(Mutex::new(conn))
}

/// 索引正文截断（napcat trim_text_for_index 同款：UTF-8 边界 + 截断标记）。
fn trim_text_for_index(text: &str) -> (String, bool) {
    let text = text.trim();
    if text.len() <= TEXT_INDEX_MAX_BYTES {
        return (text.to_string(), false);
    }
    let mut end = TEXT_INDEX_MAX_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

/// 媒体类型 → 索引标注用的中文名（与 build_content 的标注一致）。
fn kind_label_for(raw_type: &str) -> &'static str {
    match raw_type {
        "image" => "图片",
        "sticker" => "表情",
        "voice" => "语音",
        "file" => "文件",
        _ => "媒体",
    }
}

/// 引用回填的文本标注行（napcat format_cached_text_note 同款）。
fn format_cached_text_note(text: &CachedText) -> String {
    let suffix = if text.truncated {
        "（已截断）"
    } else {
        ""
    };
    format!("[引用消息] {}{}", text.text, suffix)
}

/// 引用回填的媒体标注行（napcat format_cached_media_note 同款）。
fn format_cached_media_note(media: &CachedMediaNote, prefix: &str) -> String {
    let label = format!("{prefix}{}", media.kind_label);
    let title = media
        .name
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|n| format!(": {n}"))
        .unwrap_or_default();
    let p = media.path.to_string_lossy().replace('\\', "/");
    format!("[{label}{title}] 已下载到 {p}")
}

/// 入站媒体落盘文件名的稳定哈希（dedup_key + 类型 + 标题）。
fn media_file_hash(key: &str, raw_type: &str, title: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    key.hash(&mut h);
    raw_type.hash(&mut h);
    title.hash(&mut h);
    h.finish()
}

/// 日志用截断（按字符，不切断多字节）。
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max).collect();
        format!("{cut}…")
    }
}

/// 标注型消息的一行文本：`[标签] 标题 正文`（各段缺省自动跳过）。
fn plain_label(label: &str, title: &str, content: &str) -> String {
    let mut s = label.to_string();
    let title = title.trim();
    if !title.is_empty() {
        s.push(' ');
        s.push_str(title);
    }
    let content = content.trim();
    if !content.is_empty() {
        s.push(' ');
        s.push_str(content);
    }
    s
}

/// 入站消息的落库文本（类型映射的廉价版；白名单外群消息的静默落库、白名单内
/// 归档/投递与私聊路径共用）：与 [`Wxbot::build_content`] 同一张映射表，但
/// **不下载媒体**（白名单外是陌生人内容，下载有成本与安全面）、不做引用回填、
/// 不带发送者前缀（`sender` 列已单独存）。直接用 `ev.content` 会丢信息：图片的
/// content 常为空、链接的标题/URL/摘要分列在 title/url/des——这里保证非文本
/// 消息至少落类型标注与标题。
fn record_text(ev: &CollectorEvent) -> String {
    let title = ev.title.as_deref().unwrap_or("");
    match ev.kind.as_str() {
        "text" => ev.content.clone(),
        "image" => plain_label("[图片]", title, &ev.content),
        "voice" => plain_label("[语音]", title, &ev.content),
        "sticker" => plain_label("[表情]", title, &ev.content),
        "video" => plain_label("[视频]", title, &ev.content),
        "file" => {
            if title.is_empty() {
                "[文件]".to_string()
            } else {
                format!("[文件: {title}]")
            }
        }
        "link" => {
            let mut s = format!("[链接] {title}");
            if let Some(u) = &ev.url {
                s.push(' ');
                s.push_str(u);
            }
            if let Some(d) = &ev.des {
                s.push(' ');
                s.push_str(d);
            }
            s
        }
        "miniprogram" => plain_label("[小程序]", title, &ev.content),
        "quote" => plain_label("[引用]", title, &ev.content),
        "card" => plain_label("[名片]", title, &ev.content),
        "location" => plain_label("[位置]", title, &ev.content),
        "red_packet" => plain_label("[红包]", title, &ev.content),
        "transfer" => plain_label("[转账]", title, &ev.content),
        "video_account" => plain_label("[视频号]", title, &ev.content),
        "system" => plain_label("[系统]", title, &ev.content),
        "recall" => plain_label("[撤回]", title, &ev.content),
        _ => format!(
            "[消息 type_code={}] {}",
            ev.type_code,
            truncate(&ev.raw_content, 200)
        ),
    }
}

#[async_trait]
impl Channel for Wxbot {
    fn name(&self) -> &str {
        CHANNEL_WXBOT
    }

    fn capabilities(&self) -> rclaw_core::ChannelCaps {
        rclaw_core::ChannelCaps {
            name: CHANNEL_WXBOT.to_string(),
            // 微信可收发媒体；工作状态/中间输出/推理流对微信用户无价值。
            media: true,
            working_status: false,
            intermediate_output: false,
            reasoning_stream: false,
        }
    }

    async fn run(self, inbound: crate::InboundSender, outbound: OutboundReceiver) {
        let me = Arc::new(self);
        // 启动即尝试绑定账号（best-effort；失败由首个事件兜底绑定）。
        me.bind_account().await;
        // 群消息全量落库的显式提示（隐私边界：记录不受 allow_groups 约束，回复才
        // 受）。尽力而为口径——落库失败仅告警不重投；db 缺失时明示不会落库。
        // 注意措辞：白名单外群消息媒体仅标注不下载；白名单内按既有流程下载。
        match &me.db {
            Some(_) => tracing::warn!(
                "wxbot：群消息全量落库已启用——所有群（含白名单外）的入站消息将尽力记录到本地索引 {}（白名单外媒体仅标注不下载；回复仍按 allow_groups/group_reply_policy）",
                me.cfg.media_dir.join("wxbot").join("message_index.db").display()
            ),
            None => tracing::warn!(
                "wxbot：未配置媒体目录或索引库不可用，入站消息不会落库"
            ),
        }
        let (queue_tx, queue_rx) = mpsc::channel::<CollectorEvent>(QUEUE_CAP);
        let webhook = me.clone();
        let poller = me.clone();
        let ingester = me.clone();
        let sender = me.clone();
        let poll_enabled = me.cfg.poll_fallback_enabled;
        let poll_queue = queue_tx.clone();
        let poll_task = async move {
            if poll_enabled {
                poller.poll_loop(poll_queue).await;
            } else {
                tracing::info!("wxbot：兜底长轮询已关闭（poll_fallback_enabled=false）");
                std::future::pending::<()>().await;
            }
        };
        // 四个并发任务：webhook 监听 / 兜底长轮询 / 单一 ingest / 出站发送。
        let _ = tokio::join!(
            webhook.webhook_server(queue_tx.clone()),
            poll_task,
            ingester.ingest_loop(queue_rx, inbound),
            sender.send_loop(outbound),
        );
    }
}

mod media;
use media::*;

#[cfg(test)]
mod tests;
