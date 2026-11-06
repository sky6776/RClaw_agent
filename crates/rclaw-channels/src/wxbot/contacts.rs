//! wcx REST 联系人查询客户端 + 本地最近会话只读兜底（配置页「选择群/联系人」数据源）。
//!
//! 独立于 [`super::Wxbot`]：不建 DB、不做通道启动校验；构造期强制 loopback + 非空
//! token（与 [`super::Wxbot::new`] 同一信任边界——查询命令不要求通道在运行，安全
//! 前置必须在客户端构造时生效）；reqwest no_proxy + 禁重定向 + 10s 短超时（弹层
//! 交互场景，不沿用长轮询的 poll_timeout+15 预算）。
//!
//! 错误分类供 UI 精确提示：业务 404（无命中，WCX 返回错误 JSON 体）≠ 端点 404
//! （旧版本 WCX 无此路由，Ktor 返回空 body）——以「404 且响应体可解析出 error」区分。

use std::path::Path;
use std::time::Duration;

use rusqlite::OpenFlags;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::is_loopback_url;

/// 联系人查询固定超时（秒）：配置弹层的交互预算，独立于通道长轮询。
const CONTACTS_TIMEOUT_SEC: u64 = 10;

/// 最近会话聚合的单次上限（本地库查询的 LIMIT 硬顶）。
const RECENT_LIMIT_MAX: usize = 500;

/// 联系人条目（对齐 wcx `WeChatService.ContactInfo` 的 camelCase JSON；同一结构
/// 经 Tauri 透传给前端，字段名保持 camelCase 与 wcx 域语言一致）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContactInfoDto {
    pub wx_id: String,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub custom_wx_id: String,
    #[serde(default)]
    pub remark_name: String,
}

/// 最近活跃会话条目（本地 message_index.db 只读聚合，不含消息正文）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecentChat {
    /// 记录归属的微信账号。带账号过滤查询时 legacy 空账号行会被排除（与当前账号
    /// 无法确认同一性）；无过滤查询时空串原样保留，UI 标注「账号未知」。
    pub account: String,
    /// 纯会话 id（已剥 `user:/group:` 与账号段）：私聊 wxid / 群 `xxx@chatroom`。
    pub conv_id: String,
    pub is_group: bool,
    pub msg_count: u64,
    /// 最近消息时间（Unix 秒；0 = 未知）。
    pub last_ts: i64,
}

/// contacts 查询错误分类（[`ContactsError::code`] 供前端结构化提示）。
#[derive(Debug, thiserror::Error)]
pub enum ContactsError {
    /// api_base / token 不合法（非回环、URL 无法解析、token 为空）。
    #[error("连接配置不合法：{0}")]
    Config(String),
    /// 连接失败：WCX API 服务器未开启，或与当前设备网络不可达（手机端部署需端口转发）。
    #[error("无法连接 WCX API（{0}）：请确认 WCX 的「API + MCP 服务器」已开启，且本机与其网络可达（手机部署需端口转发，如 adb reverse tcp:3001 tcp:3001）")]
    WcxOffline(String),
    /// 请求超时。
    #[error("WCX API 请求超时")]
    Timeout,
    /// 401：token 与 WCX 认证令牌不一致。
    #[error("api_token 与 WCX 认证令牌不一致")]
    TokenMismatch,
    /// 404 且响应体不是错误 JSON：当前 WCX 版本没有该端点。
    #[error("当前 WCX 版本不支持联系人接口")]
    UnsupportedEndpoint,
    /// 404 且响应体为错误 JSON：业务无命中。
    #[error("{0}")]
    NotFound(String),
    /// 400：参数/状态问题（服务端 error 消息透传）。
    #[error("{0}")]
    BadRequest(String),
    /// 5xx：WCX 服务端故障。
    #[error("WCX 服务错误（HTTP {0}）")]
    ServerError(u16),
    /// 响应体不符合预期结构。
    #[error("WCX 响应解析失败")]
    Decode,
    /// 本地消息索引库不可用（缺表/旧 schema/损坏/长期被锁）。
    #[error("本地消息索引库不可用：{0}")]
    LocalDb(String),
}

impl ContactsError {
    /// 稳定错误码（前端 switch 文案用；不得在版本间改值）。
    pub fn code(&self) -> &'static str {
        match self {
            ContactsError::Config(_) => "config",
            ContactsError::WcxOffline(_) => "wcx_offline",
            ContactsError::Timeout => "timeout",
            ContactsError::TokenMismatch => "token_mismatch",
            ContactsError::UnsupportedEndpoint => "unsupported_endpoint",
            ContactsError::NotFound(_) => "not_found",
            ContactsError::BadRequest(_) => "bad_request",
            ContactsError::ServerError(_) => "server_error",
            ContactsError::Decode => "decode",
            ContactsError::LocalDb(_) => "local_db",
        }
    }
}

/// wcx REST 联系人查询小客户端（只读；不做任何配置/存储写入）。
pub struct WxbotApiClient {
    base: String,
    token: String,
    http: reqwest::Client,
}

impl std::fmt::Debug for WxbotApiClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // 手写而非 derive：token 不进任何调试输出（防未来的 {:?} 日志泄漏）。
        f.debug_struct("WxbotApiClient")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl WxbotApiClient {
    /// 构造期安全校验：URL 可解析且为 http/https、host 回环、token 非空。
    /// 任一不满足即不发请求。
    pub fn new(api_base: &str, api_token: &str) -> Result<Self, ContactsError> {
        let base = api_base.trim().trim_end_matches('/').to_string();
        let url = reqwest::Url::parse(&base)
            .map_err(|_| ContactsError::Config(format!("api_base 无法解析（{base}）")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ContactsError::Config(format!(
                "api_base 仅支持 http/https（{base}）"
            )));
        }
        if !is_loopback_url(&base) {
            return Err(ContactsError::Config(format!(
                "api_base 非回环地址（{base}）"
            )));
        }
        let token = api_token.trim();
        if token.is_empty() {
            return Err(ContactsError::Config(
                "api_token 为空，请先填写 wxbot.api_token 并保存".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .timeout(Duration::from_secs(CONTACTS_TIMEOUT_SEC))
            .build()
            .map_err(|_| ContactsError::Config("HTTP 客户端构建失败".into()))?;
        Ok(WxbotApiClient {
            base,
            token: token.to_string(),
            http,
        })
    }

    async fn get(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<reqwest::Response, ContactsError> {
        self.http
            .get(format!("{}/api/{path}", self.base))
            .bearer_auth(&self.token)
            .query(query)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ContactsError::Timeout
                } else {
                    ContactsError::WcxOffline(self.base.clone())
                }
            })
    }

    /// 非 2xx 状态 → 结构化错误。body 只取 `{"error": ...}` 的文案，其余丢弃（不落日志）。
    fn check_status(status: u16, body: String) -> Result<String, ContactsError> {
        if (200..300).contains(&status) {
            return Ok(body);
        }
        let server_message = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string));
        match status {
            400 | 403 => Err(ContactsError::BadRequest(server_message.unwrap_or_else(
                || {
                    if status == 403 {
                        "WCX 拒绝访问".into()
                    } else {
                        "WCX 拒绝了请求".into()
                    }
                },
            ))),
            // 重定向已被 client 禁用，出现 3xx 说明代理/网关异常插手，按端点不可用处理。
            s if (300..400).contains(&s) => Err(ContactsError::UnsupportedEndpoint),
            401 => Err(ContactsError::TokenMismatch),
            404 => match server_message {
                Some(msg) => Err(ContactsError::NotFound(msg)),
                None => Err(ContactsError::UnsupportedEndpoint),
            },
            // 限流与服务端故障都算服务端侧不可用；其余未预期 4xx 按 BadRequest 透传。
            s if s == 429 || (500..600).contains(&s) => Err(ContactsError::ServerError(s)),
            _ => Err(ContactsError::BadRequest(
                server_message.unwrap_or_else(|| format!("WCX 返回异常状态（HTTP {status}）")),
            )),
        }
    }

    /// 发 GET 并按状态分类，成功时返回响应体文本。
    async fn get_text(&self, path: &str, query: &[(&str, &str)]) -> Result<String, ContactsError> {
        let resp = self.get(path, query).await?;
        let status = resp.status().as_u16();
        // body 读取失败（连接中断/截断）：按状态码分派（401→TokenMismatch 等）；
        // 404 例外——空 body 会误判成「版本旧」，而读失败时无从分辨业务 404，按 Decode。
        let body = match resp.text().await {
            Ok(text) => text,
            Err(_) if status == 404 => return Err(ContactsError::Decode),
            Err(_) => {
                return match Self::check_status(status, String::new()) {
                    Ok(_) => Err(ContactsError::Decode),
                    Err(e) => Err(e),
                };
            }
        };
        Self::check_status(status, body)
    }

    /// `GET /api/contacts?type=<kind>`：好友/群列表。兼容裸数组（现行 WCX 契约）与
    /// `{contacts: [...]}` envelope（未来可能的演进），响应里的联系人数据原样返回。
    pub async fn list_contacts(&self, kind: &str) -> Result<Vec<ContactInfoDto>, ContactsError> {
        let body = self.get_text("contacts", &[("type", kind)]).await?;
        parse_contacts_body(&body)
    }

    /// `GET /api/contacts/search?display-name=&group-id=`：按昵称/备注精确反查 wxid。
    pub async fn lookup_contact(
        &self,
        display_name: &str,
        group_id: Option<&str>,
    ) -> Result<String, ContactsError> {
        let mut query = vec![("display-name", display_name)];
        if let Some(g) = group_id {
            query.push(("group-id", g));
        }
        let body = self.get_text("contacts/search", &query).await?;
        let value: Value = serde_json::from_str(&body).map_err(|_| ContactsError::Decode)?;
        value
            .get("wxId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or(ContactsError::Decode)
    }

    /// `GET /api/self/info`：WCX 当前登录账号。未登录 / 返回为空 → `None`
    /// （不把它包装成「绑定确认」——这里只是 WCX 侧登录态快照）。
    pub async fn self_wxid(&self) -> Result<Option<String>, ContactsError> {
        let body = self.get_text("self/info", &[]).await?;
        let value: Value = serde_json::from_str(&body).map_err(|_| ContactsError::Decode)?;
        Ok(value
            .get("wxId")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .filter(|s| !s.is_empty()))
    }
}

/// 解析 /api/contacts 响应体：裸数组或 envelope，条目 camelCase。
fn parse_contacts_body(body: &str) -> Result<Vec<ContactInfoDto>, ContactsError> {
    let value: Value = serde_json::from_str(body).map_err(|_| ContactsError::Decode)?;
    let list = match value {
        Value::Array(items) => Value::Array(items),
        Value::Object(ref map) => map.get("contacts").cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    };
    serde_json::from_value(list).map_err(|_| ContactsError::Decode)
}

/// 最近会话 chat_id → `(账号段, conv_id)`：严格按 `user:/group:` 前缀解析，前缀与
/// `is_group` 不一致、id 为空的行一律丢弃（不对任意字符串「取最后一段」）。
fn parse_recent_chat_id(chat_id: &str, is_group: bool) -> Option<(String, String)> {
    let prefix = if is_group { "group:" } else { "user:" };
    let rest = chat_id.strip_prefix(prefix)?;
    let (account, conv_id) = match rest.split_once(':') {
        Some((account, talker)) => (account, talker),
        None => ("", rest),
    };
    if conv_id.is_empty() {
        return None;
    }
    if is_group && !conv_id.ends_with("@chatroom") {
        return None;
    }
    Some((account.to_string(), conv_id.to_string()))
}

/// 只读聚合最近活跃会话（本机 `media_dir/wxbot/message_index.db` 兜底数据源）。
///
/// 只读打开：不建目录/表、不迁移、不改 journal_mode（不影响通道侧的读写连接）；
/// 文件不存在 → `Ok(空)`（本机宿主从未收到过 wxbot 消息，不是错误）；缺表/旧
/// schema/损坏 → [`ContactsError::LocalDb`]（UI 显示「不可用」而不是误报空）。
pub fn list_recent_chats(
    db_path: &Path,
    is_group: bool,
    account: Option<&str>,
    limit: usize,
) -> Result<Vec<RecentChat>, ContactsError> {
    if !db_path.exists() {
        return Ok(Vec::new());
    }
    // limit 钳制到 1..=RECENT_LIMIT_MAX：0 视为 1（调用方 cmd 层默认 100，UI 传 200）。
    let limit = limit.clamp(1, RECENT_LIMIT_MAX) as i64;
    let conn = rusqlite::Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| ContactsError::LocalDb(format!("打开失败：{e}")))?;
    conn.busy_timeout(Duration::from_secs(2))
        .map_err(|e| ContactsError::LocalDb(format!("busy timeout 设置失败：{e}")))?;

    // 前缀/后缀在 SQL 侧过滤：坏行不占 LIMIT 名额（Rust 侧 parse_recent_chat_id 仍
    // 复核一次，双保险）；GROUP BY 带 account——形式严谨（SELECT 非聚合列 = 分组键），
    // 且跨账号同 conv_id 天然是两行，与 UI 的 account+convId 复合键对应。
    // user：`user:%_` 排除空前缀行（"user:"），`NOT LIKE '%:'` 排除空 id 行
    // （"user::"/"user:acc:"）；group 的 `@chatroom` 后缀已排除这两类。
    let chat_id_like = if is_group {
        "group:%@chatroom"
    } else {
        "user:%_"
    };
    let mut sql = format!(
        "SELECT account, chat_id, COUNT(*) AS n, MAX(create_time) AS last_ts \
         FROM inbound_messages WHERE is_group = ?1 AND chat_id LIKE '{chat_id_like}'"
    );
    if !is_group {
        sql.push_str(" AND chat_id NOT LIKE '%:'");
    }
    // 占位序号必须连续：无 account 过滤时 LIMIT 用 ?2（?3 会让 SQLite 按 3 参校验）。
    let limit_idx = if account.is_some() {
        sql.push_str(" AND account = ?2");
        3
    } else {
        2
    };
    sql.push_str(&format!(
        " GROUP BY account, chat_id ORDER BY last_ts DESC LIMIT ?{limit_idx}"
    ));
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| ContactsError::LocalDb(format!("查询失败：{e}")))?;

    let is_group_flag = if is_group { 1 } else { 0 };
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<Option<RecentChat>> {
        let account: String = row.get(0)?;
        let chat_id: String = row.get(1)?;
        let n: i64 = row.get(2)?;
        let last_ts: i64 = row.get(3)?;
        let (_, conv_id) = match parse_recent_chat_id(&chat_id, is_group) {
            Some(parsed) => parsed,
            // 严格解析不过的行（前缀错乱/空 id）跳过，不做猜测性兜底。
            None => return Ok(None),
        };
        Ok(Some(RecentChat {
            account,
            conv_id,
            is_group,
            msg_count: n.max(0) as u64,
            last_ts,
        }))
    };
    let rows = if let Some(acc) = account {
        stmt.query_map(rusqlite::params![is_group_flag, acc, limit], map_row)
    } else {
        stmt.query_map(rusqlite::params![is_group_flag, limit], map_row)
    }
    .map_err(|e| ContactsError::LocalDb(format!("查询失败：{e}")))?;

    let mut out = Vec::new();
    for row in rows {
        if let Some(chat) = row.map_err(|e| ContactsError::LocalDb(format!("读取失败：{e}")))?
        {
            out.push(chat);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    // DB_SCHEMA 声明在父模块 wxbot（私有），本模块是其后代，可见。
    use super::super::DB_SCHEMA;

    // ---- WxbotApiClient::new 构造期校验 ----

    #[test]
    fn new_rejects_non_loopback_base() {
        let err = WxbotApiClient::new("http://192.168.1.5:3001", "tok").unwrap_err();
        assert_eq!(err.code(), "config");
    }

    #[test]
    fn new_rejects_unparseable_base() {
        let err = WxbotApiClient::new("::not-a-url::", "tok").unwrap_err();
        assert_eq!(err.code(), "config");
    }

    #[test]
    fn new_rejects_blank_token() {
        assert_eq!(
            WxbotApiClient::new("http://127.0.0.1:3001", "  ")
                .unwrap_err()
                .code(),
            "config"
        );
        assert_eq!(
            WxbotApiClient::new("http://localhost:3001", "")
                .unwrap_err()
                .code(),
            "config"
        );
    }

    #[test]
    fn new_accepts_loopback_and_trims_base() {
        for base in [
            "http://127.0.0.1:3001/",
            " http://[::1]:3001 ",
            "http://LOCALHOST:3001",
        ] {
            assert!(WxbotApiClient::new(base, "tok").is_ok(), "{base} 应可构造");
        }
    }

    // ---- /api/contacts 响应解析 ----

    #[test]
    fn parse_contacts_accepts_bare_array() {
        let body = r#"[{"wxId":"wxid_abc","nickname":"张三","customWxId":"zs2020","remarkName":"老张"},
                        {"wxId":"123@chatroom","nickname":"群"}]"#;
        let list = parse_contacts_body(body).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].wx_id, "wxid_abc");
        assert_eq!(list[0].custom_wx_id, "zs2020");
        assert_eq!(list[0].remark_name, "老张");
        assert_eq!(list[1].wx_id, "123@chatroom");
    }

    #[test]
    fn parse_contacts_accepts_envelope() {
        let body = r#"{"contacts":[{"wxId":"wxid_x","nickname":"n"}],"total":1}"#;
        let list = parse_contacts_body(body).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].wx_id, "wxid_x");
    }

    #[test]
    fn parse_contacts_defaults_optional_fields() {
        // groups 分支的条目只有 wxId/nickname。
        let list = parse_contacts_body(r#"[{"wxId":"g@chatroom","nickname":"群名"}]"#).unwrap();
        assert_eq!(list[0].custom_wx_id, "");
        assert_eq!(list[0].remark_name, "");
    }

    #[test]
    fn parse_contacts_rejects_garbage() {
        assert_eq!(
            parse_contacts_body("not json").unwrap_err().code(),
            "decode"
        );
        assert_eq!(
            parse_contacts_body(r#"{"other":1}"#).unwrap_err().code(),
            "decode"
        );
    }

    // ---- HTTP 状态分类 ----

    #[test]
    fn status_400_maps_bad_request_with_server_error_text() {
        let err =
            WxbotApiClient::check_status(400, r#"{"error":"WeChat database not ready"}"#.into())
                .unwrap_err();
        assert_eq!(err.code(), "bad_request");
        assert!(err.to_string().contains("database not ready"));
    }

    #[test]
    fn status_401_maps_token_mismatch() {
        let err = WxbotApiClient::check_status(401, String::new()).unwrap_err();
        assert_eq!(err.code(), "token_mismatch");
    }

    #[test]
    fn status_404_empty_body_maps_unsupported_endpoint() {
        // 旧版本 WCX：Ktor 对未注册路由返回 404 + 空 body。
        let err = WxbotApiClient::check_status(404, String::new()).unwrap_err();
        assert_eq!(err.code(), "unsupported_endpoint");
    }

    #[test]
    fn status_404_json_body_maps_not_found() {
        let err =
            WxbotApiClient::check_status(404, r#"{"error":"search matched 0 contact"}"#.into())
                .unwrap_err();
        assert_eq!(err.code(), "not_found");
    }

    #[test]
    fn status_3xx_maps_unsupported_and_429_5xx_maps_server_error() {
        // 重定向被 client 禁用，3xx = 代理/网关插手 → 端点不可用；429/5xx → 服务端侧。
        assert_eq!(
            WxbotApiClient::check_status(302, String::new())
                .unwrap_err()
                .code(),
            "unsupported_endpoint"
        );
        assert_eq!(
            WxbotApiClient::check_status(429, String::new())
                .unwrap_err()
                .code(),
            "server_error"
        );
        assert_eq!(
            WxbotApiClient::check_status(503, String::new())
                .unwrap_err()
                .code(),
            "server_error"
        );
    }

    #[test]
    fn status_403_maps_bad_request() {
        let err = WxbotApiClient::check_status(403, String::new()).unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }

    #[test]
    fn debug_never_leaks_token() {
        let client = WxbotApiClient::new("http://127.0.0.1:3001", "super-secret-token").unwrap();
        let rendered = format!("{client:?}");
        assert!(
            !rendered.contains("super-secret-token"),
            "token 不得进 Debug 输出"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn new_rejects_non_http_scheme() {
        assert_eq!(
            WxbotApiClient::new("file://127.0.0.1", "tok")
                .unwrap_err()
                .code(),
            "config"
        );
    }

    #[test]
    fn status_2xx_passes_body_through() {
        let body = WxbotApiClient::check_status(200, r#"{"wxId":"x"}"#.into()).unwrap();
        assert_eq!(body, r#"{"wxId":"x"}"#);
    }

    // ---- chat_id 解析 ----

    #[test]
    fn parse_recent_chat_id_handles_account_segment() {
        let (account, conv) = parse_recent_chat_id("group:acc1:123@chatroom", true).unwrap();
        assert_eq!((account.as_str(), conv.as_str()), ("acc1", "123@chatroom"));
        let (account, conv) = parse_recent_chat_id("user:acc2:wxid_x", false).unwrap();
        assert_eq!((account.as_str(), conv.as_str()), ("acc2", "wxid_x"));
    }

    #[test]
    fn parse_recent_chat_id_handles_legacy_no_account() {
        let (account, conv) = parse_recent_chat_id("group:999@chatroom", true).unwrap();
        assert_eq!(account, "");
        assert_eq!(conv, "999@chatroom");
    }

    #[test]
    fn parse_recent_chat_id_rejects_mismatched_or_malformed() {
        assert!(parse_recent_chat_id("user:wxid_x", true).is_none()); // 前缀与 is_group 不一致
        assert!(parse_recent_chat_id("group:123@chatroom", false).is_none());
        assert!(parse_recent_chat_id("group:acc:", true).is_none()); // 空 id
        assert!(parse_recent_chat_id("junk", true).is_none()); // 无前缀
        assert!(parse_recent_chat_id("group:acc:not_a_room", true).is_none()); // 群无 @chatroom 后缀
    }

    // ---- 本地只读聚合 ----

    fn seed_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(DB_SCHEMA).unwrap();
        for (account, chat_id, is_group, ts) in [
            ("acc1", "group:acc1:a@chatroom", 1, 100),
            ("acc1", "group:acc1:b@chatroom", 1, 300),
            ("acc1", "user:acc1:wxid_u1", 0, 200),
            ("acc2", "group:acc2:a@chatroom", 1, 400), // 同群 id，不同账号
            ("", "group::legacy@chatroom", 1, 50),     // legacy 空账号
        ] {
            conn.execute(
                "INSERT INTO inbound_messages(dedup_key, account, chat_id, is_group, create_time) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![format!("{chat_id}:{ts}"), account, chat_id, is_group, ts],
            )
            .unwrap();
        }
    }

    #[test]
    fn recent_missing_db_is_empty_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let list = list_recent_chats(&dir.path().join("nope.db"), true, None, 100).unwrap();
        assert!(list.is_empty());
    }

    #[test]
    fn recent_groups_sorted_by_last_ts_with_account_filter() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("message_index.db");
        seed_db(&db);
        let list = list_recent_chats(&db, true, Some("acc1"), 100).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].conv_id, "b@chatroom");
        assert_eq!(list[0].last_ts, 300);
        assert_eq!(list[1].conv_id, "a@chatroom");
        assert_eq!(list[0].account, "acc1");
    }

    #[test]
    fn recent_without_account_keeps_isolation_keys() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("message_index.db");
        seed_db(&db);
        let list = list_recent_chats(&db, true, None, 100).unwrap();
        // acc2 的 a@chatroom 与 acc1 的 a@chatroom 是不同行（account 字段保留隔离键）。
        let ids: Vec<(String, String)> = list
            .iter()
            .map(|c| (c.account.clone(), c.conv_id.clone()))
            .collect();
        assert!(ids.contains(&("acc2".into(), "a@chatroom".into())));
        assert!(ids.contains(&("".into(), "legacy@chatroom".into())));
        assert_eq!(list.len(), 4);
    }

    #[test]
    fn recent_private_chats_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("message_index.db");
        seed_db(&db);
        let list = list_recent_chats(&db, false, None, 100).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].conv_id, "wxid_u1");
        let limited = list_recent_chats(&db, true, None, 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].conv_id, "a@chatroom"); // last_ts=400 的 acc2 行
    }

    #[test]
    fn recent_read_only_open_does_not_create_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("absent.db");
        assert!(list_recent_chats(&db, true, None, 10).unwrap().is_empty());
        assert!(!db.exists(), "只读兜底不得创建库文件");
    }
}
