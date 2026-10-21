//! MCP 服务器生命周期管理：加载配置、自动/按需启动、把工具注册进 [`ToolRegistry`]。
//!
//! 设计要点：
//! - 持有已解析配置 + `server_name → Arc<OnceCell<Arc<McpClient>>>` 连接池。
//! - `load()` 构造后自动启动 `enabled && auto_start` 的服务器。
//! - `client_for()` 按需启动：用 [`tokio::sync::OnceCell`] 把「同一 server 的并发启动」
//!   收敛为单次 spawn——首个调用执行初始化，其余调用等待并复用同一结果，杜绝并发重复 spawn
//!   同一进程；不同 server 的 cell 相互独立，仍可并行启动。
//! - `extend_registry()` 把 `enabled && auto_start` 服务器的工具注册进注册表
//!   （`auto_start=false` 的服务器不启动、不注册）。
//!
//! 注册用 RClaw 的 `ToolRegistry::register(Box<dyn Tool>)`。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{info, warn};

use super::client::McpClient;
use super::config::{McpResolved, McpResolvedServer};
use super::provider::{build_exposed_name, sanitize_segment, McpToolWrapper};
use super::{McpError, Result as McpResult};
use crate::ToolRegistry;

/// 限定名 `<server>__<tool>` 的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpToolTarget {
    pub server_name: String,
    pub tool_name: String,
}

impl McpToolTarget {
    /// 解析 `<server>__<tool>`；任一段为空或缺少分隔符返回 `None`。
    pub fn parse(qualified: &str) -> Option<Self> {
        let (server, tool) = qualified.split_once("__")?;
        let server = server.trim();
        let tool = tool.trim();
        if server.is_empty() || tool.is_empty() {
            return None;
        }
        Some(Self {
            server_name: server.to_string(),
            tool_name: tool.to_string(),
        })
    }
}

/// 判定某个限定工具名是否在允许集合内。
///
/// 规则：精确命中 `allowed_tools` 直接放行；否则解析出 server 名，
/// 命中 `allowed_servers` 放行。两者都不命中则拒绝。
///
/// 注：本函数会解析 `<server>__<tool>` 串，仅适用于**净化后**的暴露名（段内不含
/// `__`）。注册流程中对**原始** server/tool 名（server 可能含 `__`）的放行判断请用
/// [`mcp_tool_allowed`]，以免 [`McpToolTarget::parse`] 错位导致漏注册。
pub fn mcp_access_allows(
    allowed_servers: &[String],
    allowed_tools: &[String],
    qualified_tool_name: &str,
) -> bool {
    if allowed_tools.iter().any(|t| t == qualified_tool_name) {
        return true;
    }
    let Some(target) = McpToolTarget::parse(qualified_tool_name) else {
        return false;
    };
    allowed_servers
        .iter()
        .any(|server| server == &target.server_name)
}

/// 判定 `(server_name, tool_name)` 是否被允许注册。
///
/// - 精确工具放行：`allowed_tools` 存的是**净化后的暴露名**（即 [`build_exposed_name`]
///   的输出，亦即对 LLM 暴露的 `function.name`）。暴露名经净化+消歧哈希后对每个
///   `(server, tool)` 对唯一，**不会**像原始 `server__tool` 串那样因 server/tool 含 `__`
///   而产生 `a__b__tool` 之类的歧义（杜绝越权注册）。干净名（无 `__`、不超长）的暴露名
///   就等于 `server__tool`，可直接书写。
/// - server 级放行：原始 server 名命中 `allowed_servers`（直接字符串比较，无歧义）。
fn mcp_tool_allowed(
    allowed_servers: &[String],
    allowed_tools: &[String],
    server_name: &str,
    tool_name: &str,
) -> bool {
    if !allowed_tools.is_empty() {
        let exposed = build_exposed_name(server_name, tool_name);
        if allowed_tools.iter().any(|t| t == &exposed) {
            return true;
        }
    }
    allowed_servers.iter().any(|s| s == server_name)
}

/// 判定某服务器是否应进入「启动期注册」流程（即是否可对其 [`McpManager::client_for`]）。
///
/// 这是 [`McpManager::extend_registry_for_rules`] 在 `client_for`（启动进程）**之前**的
/// 纯函数前置过滤，避免：
/// - 启动 `auto_start = false` 的服务器（用户明确表示不在启动期拉起）；
/// - 启动既不在 `allowed_servers`、其工具前缀也不在 `allowed_tools` 的服务器
///   （此类服务器不可能贡献任何被允许的工具，启动既浪费又越权）。
///
/// 返回 `true` 仅表示「值得启动去列举工具」；具体每个工具是否注册仍由
/// [`mcp_tool_allowed`] 在拿到工具清单后逐条判定。
fn should_register_at_boot(
    cfg: &McpResolvedServer,
    server_name: &str,
    allowed_servers: &[String],
    allowed_tools: &[String],
) -> bool {
    if !cfg.enabled || !cfg.auto_start {
        return false;
    }
    if allowed_servers.iter().any(|s| s == server_name) {
        return true;
    }
    // allowed_tools 为「暴露名」（净化+消歧后唯一）。注意暴露名在总长超 64 时会被
    // [`build_exposed_name`] **截断 server 段**，因此其 server 段不一定等于完整的
    // `sanitize_segment(server_name)`，而是后者的一个前缀。若仍按「以完整 sanitized 名 + `__`
    // 开头」做前缀匹配，长 server 名的放行工具将匹配失败、server 被漏启动、工具永远不注册。
    //
    // 修正：取 allowed_tool 在首个 `__` 前的 server 段，判断它是否为 `sanitize(server_name)` 的
    // 前缀（截断只会让 server 段变短，故真属于本 server 的暴露名必然满足该关系）。该判定保守——
    // 可能多启动一个最终无工具可注册的服务器，但绝不会漏掉真正放行的工具（精确匹配仍由
    // [`mcp_tool_allowed`] 在拿到工具清单后逐条做）。
    let sanitized = sanitize_segment(server_name);
    allowed_tools.iter().any(|t| match t.split_once("__") {
        Some((seg, _)) => !seg.is_empty() && sanitized.starts_with(seg),
        None => false,
    })
}

/// per-server 可替换槽：承载「当前缓存的 client」，可为 None 或被丢弃重建（详见 [`McpManager`]）。
type ClientSlot = Arc<Mutex<Option<Arc<McpClient>>>>;

/// MCP 连接池 + 配置视图。
///
/// 连接池用 `server_name → ClientSlot`（per-server 可替换槽）：
/// - 外层 `Mutex<HashMap>` 仅保护「按名取/建槽」这一极短临界区；
/// - 每个 server 一个独立的内层 `Mutex<Option<Arc<McpClient>>>`：承载「当前缓存的 client」，
///   且**允许替换**——这是与 `OnceCell` 的关键区别。子进程可能随时退出，一旦
///   [`McpClient::is_alive`] 判定失效，[`Self::client_for`] 会丢弃旧 client 并重建。
/// - 同一 server 的并发调用共享同一内层槽：首个持锁者执行 `start`，其余在锁上等待并复用结果，
///   从而**只 spawn 一次**进程；不同 server 的槽相互独立，可并行启动。
pub struct McpManager {
    resolved: McpResolved,
    clients: Mutex<HashMap<String, ClientSlot>>,
}

impl McpManager {
    pub fn new(resolved: McpResolved) -> Self {
        Self {
            resolved,
            clients: Mutex::new(HashMap::new()),
        }
    }

    /// 构造并自动启动 `enabled && auto_start` 的服务器（启动失败仅告警，不阻断构造）。
    pub async fn load(resolved: McpResolved) -> McpResult<Self> {
        let manager = Self::new(resolved);
        manager.start_auto_start_servers().await;
        Ok(manager)
    }

    pub fn resolved_config(&self) -> &McpResolved {
        &self.resolved
    }

    pub fn enabled_server_names(&self) -> Vec<String> {
        self.resolved.enabled_server_names()
    }

    async fn start_auto_start_servers(&self) {
        for server_name in self.enabled_server_names() {
            let Some(server_cfg) = self.resolved.servers.get(&server_name) else {
                continue;
            };
            if !server_cfg.auto_start {
                continue;
            }
            if let Err(e) = self.client_for(&server_name).await {
                // 启动期错误可能内嵌 reqwest 错误（Display 含完整 URL，query 里可带 token），
                // 与 provider.rs 的执行期日志同样先脱敏。
                warn!(
                    server = %server_name,
                    error = %rclaw_store::redact_sensitive_text(&e.to_string()),
                    "自动启动 MCP 服务器失败"
                );
            }
        }
    }

    /// 取（必要时启动 / 重建）某个服务器的客户端。
    ///
    /// 用 per-server 槽合并并发启动，并支持「失效重建」：
    /// - 同一 server 的多个并发调用共享同一内层槽，首个持锁者执行 `start`，其余等待并复用结果，
    ///   从而**只 spawn 一次**进程；不同 server 的槽独立，可并行启动。
    /// - 每次返回前检查 [`McpClient::is_alive`]：若缓存的 client 已不健康（子进程退出、
    ///   stdout 关闭），丢弃并重建。修复了「OnceCell 永久缓存 dead client、后续调用必失败」的问题。
    /// - 启动失败不写入槽（`*guard = None` 保持空），故后续可重试。
    pub async fn client_for(&self, server_name: &str) -> McpResult<Arc<McpClient>> {
        // 1) 短临界区：按名取或建 per-server 槽（仅插入空槽，不在此 spawn）。
        let slot = {
            let mut clients = self.clients.lock().await;
            Arc::clone(
                clients
                    .entry(server_name.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(None))),
            )
        };

        // 2) 锁 per-server 槽：串行化同 server 的「启动/重建」，并发调用共享一次初始化。
        //    持锁跨越 start 是期望行为——同 server 的其它调用方应等待而非并发重复 spawn。
        let mut guard = slot.lock().await;

        // 2a) 命中缓存且仍存活：直接复用。
        if let Some(client) = guard.as_ref() {
            if client.is_alive() {
                return Ok(Arc::clone(client));
            }
            // 缓存已失效（server 已退出）：丢弃，下面重建。
            warn!(server = %server_name, "缓存的 MCP client 已失效，重建连接");
            *guard = None;
        }

        // 3) 无可用 client：校验配置后启动新的。
        let server_cfg = self
            .resolved
            .servers
            .get(server_name)
            .ok_or_else(|| McpError::NotFound(server_name.to_string()))?;
        if !server_cfg.enabled {
            return Err(McpError::Config(format!(
                "MCP 服务器 '{server_name}' 已禁用"
            )));
        }

        let client = match server_cfg.transport {
            crate::mcp::config::TransportKind::Stdio => {
                let command = server_cfg.command.as_deref().ok_or_else(|| {
                    McpError::Config(format!("MCP '{server_name}' 缺少 command（stdio）"))
                })?;
                info!(server = %server_name, transport = "stdio", command = %command, "启动 MCP 服务器");
                Arc::new(
                    McpClient::start_stdio(
                        server_name,
                        command,
                        &server_cfg.args,
                        &server_cfg.env,
                        server_cfg.cwd.as_deref(),
                        Duration::from_secs(server_cfg.startup_timeout_secs),
                        Duration::from_secs(server_cfg.call_timeout_secs),
                    )
                    .await?,
                )
            }
            crate::mcp::config::TransportKind::Http => {
                let url = server_cfg.url.as_deref().ok_or_else(|| {
                    McpError::Config(format!("MCP '{server_name}' 缺少 url（http）"))
                })?;
                // URL 的 query 里常带 token / key，日志（可经 /log get 外发）只记脱敏后的。
                info!(
                    server = %server_name,
                    transport = "http",
                    url = %rclaw_store::redact_sensitive_text(url),
                    "启动 MCP 服务器"
                );
                Arc::new(
                    McpClient::start_http(
                        server_name,
                        url,
                        &server_cfg.headers,
                        Duration::from_secs(server_cfg.startup_timeout_secs),
                        Duration::from_secs(server_cfg.call_timeout_secs),
                    )
                    .await?,
                )
            }
        };
        // 4) 写入槽供后续复用（失败路径已提前 return Err，不会执行到此处）。
        *guard = Some(Arc::clone(&client));
        Ok(client)
    }

    /// 把 `enabled && auto_start` 服务器的工具注册进 `registry`（限定名 `<server>__<tool>`）。
    /// `auto_start=false` 的服务器不会被启动、也不会注册。
    ///
    /// 接收 `self: &Arc<Self>`：注册出的 wrapper 会持有 manager 句柄，以便在 server 退出后
    /// 通过 [`Self::client_for`] 重新解析到（重建的）client，而非永久绑定一个可能已死的 client。
    pub async fn extend_registry(self: &Arc<Self>, registry: &mut ToolRegistry) -> McpResult<()> {
        let allowed = self.enabled_server_names();
        self.extend_registry_for_rules(registry, &allowed, &[])
            .await
    }

    /// 按「允许服务器 / 允许工具」规则注册。供需要细粒度权限的上层使用。
    ///
    /// 在 `client_for`（启动进程）之前先用 [`should_register_at_boot`] 按 `auto_start` 与
    /// allowed 规则前置过滤，确保 `auto_start=false` 或全无放行的服务器不会被启动。
    pub async fn extend_registry_for_rules(
        self: &Arc<Self>,
        registry: &mut ToolRegistry,
        allowed_servers: &[String],
        allowed_tools: &[String],
    ) -> McpResult<()> {
        for (server_name, server_cfg) in &self.resolved.servers {
            if !should_register_at_boot(server_cfg, server_name, allowed_servers, allowed_tools) {
                continue;
            }
            let client = match self.client_for(server_name).await {
                Ok(c) => c,
                Err(e) => {
                    warn!(
                        server = %server_name,
                        error = %rclaw_store::redact_sensitive_text(&e.to_string()),
                        "跳过不可用的 MCP 服务器"
                    );
                    continue;
                }
            };
            for tool in client.list_tools().await {
                if mcp_tool_allowed(allowed_servers, allowed_tools, server_name, &tool.name) {
                    // wrapper 持有 manager 句柄（而非 client），execute 时再按需 client_for，
                    // 从而在 server 退出后能自动重建连接。
                    registry.register(Box::new(McpToolWrapper::new(
                        server_name,
                        tool,
                        Arc::clone(self),
                    )));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_qualified_tool_name() {
        let t = McpToolTarget::parse("filesystem__read_file").unwrap();
        assert_eq!(t.server_name, "filesystem");
        assert_eq!(t.tool_name, "read_file");
    }

    #[test]
    fn parse_rejects_malformed() {
        assert!(McpToolTarget::parse("noseparator").is_none());
        assert!(McpToolTarget::parse("__tool").is_none());
        assert!(McpToolTarget::parse("server__").is_none());
        assert!(McpToolTarget::parse("server__  ").is_none());
        // 注意：工具名内部含 "__" 仍可解析（split_once 只切第一个）。
        let t = McpToolTarget::parse("a__b__c").unwrap();
        assert_eq!(t.server_name, "a");
        assert_eq!(t.tool_name, "b__c");
    }

    #[test]
    fn access_allows_by_explicit_tool_or_server() {
        let servers = vec!["filesystem".to_string()];
        let tools = vec!["git__status".to_string()];

        assert!(mcp_access_allows(&servers, &tools, "filesystem__read")); // 命中 server
        assert!(mcp_access_allows(&servers, &tools, "git__status")); // 命中 tool
        assert!(!mcp_access_allows(&servers, &tools, "git__log")); // 都不命中
        assert!(!mcp_access_allows(&servers, &tools, "malformed")); // 无法解析
    }

    #[test]
    fn mcp_tool_allowed_uses_exposed_name_unambiguously() {
        // P2：allowed_tools 按「暴露名」匹配（净化+消歧后唯一），杜绝 server__tool 串歧义。
        use crate::mcp::provider::build_exposed_name;

        // server 级放行：原始 server 名命中（直接比较，无歧义）。
        let allowed_servers = vec!["a__b".to_string()];
        assert!(mcp_tool_allowed(&allowed_servers, &[], "a__b", "tool"));

        // 精确工具放行：干净名的暴露名即 server__tool，可直接书写。
        assert!(mcp_tool_allowed(
            &[],
            &["github__create_issue".to_string()],
            "github",
            "create_issue"
        ));

        // 关键：含 "__" 的两对 (a__b, tool) 与 (a, b__tool) 暴露名不同，
        // 放行其中一对不会越权匹配另一对（旧实现二者都拼成 a__b__tool）。
        let exposed_ab_tool = build_exposed_name("a__b", "tool");
        let exposed_a_btool = build_exposed_name("a", "b__tool");
        assert_ne!(exposed_ab_tool, exposed_a_btool);
        let allowed = vec![exposed_ab_tool];
        assert!(mcp_tool_allowed(&[], &allowed, "a__b", "tool"));
        assert!(!mcp_tool_allowed(&[], &allowed, "a", "b__tool"));

        // 都不命中。
        assert!(!mcp_tool_allowed(&allowed_servers, &[], "other", "tool"));
    }

    #[test]
    fn should_register_at_boot_respects_auto_start_and_allowlist() {
        // P1：auto_start=false 的服务器即便 enabled 且在 allowlist，也不应被启动/注册。
        let mk = |enabled: bool, auto_start: bool| crate::mcp::McpResolvedServer {
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("c".into()),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: None,
            enabled,
            auto_start,
            startup_timeout_secs: 20,
            call_timeout_secs: 60,
        };
        let allowed_servers = vec!["fs".to_string()];
        let allowed_tools = vec!["git__status".to_string()];

        // auto_start=false：即使 enabled 且在 allowlist，也不注册。
        assert!(!should_register_at_boot(
            &mk(true, false),
            "fs",
            &allowed_servers,
            &allowed_tools
        ));
        // enabled+auto_start，命中 allowed_servers：注册。
        assert!(should_register_at_boot(
            &mk(true, true),
            "fs",
            &allowed_servers,
            &allowed_tools
        ));
        // 不在 allowed_servers，但其工具前缀命中 allowed_tools：值得启动去列举。
        assert!(should_register_at_boot(
            &mk(true, true),
            "git",
            &allowed_servers,
            &allowed_tools
        ));
        // 既不在 allowed_servers，也无前缀命中：不注册（不启动）。
        assert!(!should_register_at_boot(
            &mk(true, true),
            "other",
            &allowed_servers,
            &allowed_tools
        ));
        // enabled=false：不注册。
        assert!(!should_register_at_boot(
            &mk(false, true),
            "fs",
            &allowed_servers,
            &allowed_tools
        ));
    }

    #[test]
    fn should_register_at_boot_matches_truncated_server_prefix() {
        // P2：长 server 名会触发 build_exposed_name 截断 server 段，暴露名的 server 段
        // 不再等于完整 sanitized 名，旧「以完整 sanitized 名 + __ 开头」的前缀匹配会漏判、
        // 导致放行工具所在 server 被漏启动。新实现按「server 段是否为 sanitized 名前缀」判定。
        use crate::mcp::provider::build_exposed_name;

        let long_server = "a".repeat(60);
        let tool_name = "t".repeat(20);
        // 总长 60+2+20 > 64，暴露名必然触发截断，server 段被缩短。
        let exposed = build_exposed_name(&long_server, &tool_name);
        assert!(exposed.len() <= 64);
        // 旧前缀（完整 sanitized 名）已无法匹配被截断的暴露名：
        let full_prefix = format!("{}__", sanitize_segment(&long_server));
        assert!(
            !exposed.starts_with(&full_prefix),
            "暴露名 server 段应被截断: {exposed}"
        );

        let cfg = crate::mcp::McpResolvedServer {
            transport: crate::mcp::TransportKind::Stdio,
            command: Some("c".into()),
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: None,
            enabled: true,
            auto_start: true,
            startup_timeout_secs: 20,
            call_timeout_secs: 60,
        };
        // 即便如此，前置过滤也应判定「值得启动」，不漏掉该放行工具。
        assert!(should_register_at_boot(&cfg, &long_server, &[], &[exposed]));
    }
}

// ===========================================================================
// HTTP 端到端集成测试：config → McpManager::load → start_http → list_tools/call_tool。
// 用 wiremock 进程内 mock server，零真实网络。
// ===========================================================================
#[cfg(test)]
mod http_integration_tests {
    use super::*;
    use crate::mcp::config::{McpConfig, McpServerDef};
    use std::collections::HashMap;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const INIT_RESULT: &str =
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-11-25","capabilities":{}}}"#;

    #[tokio::test]
    async fn http_server_loads_lists_and_calls_tool() {
        let server = MockServer::start().await;
        // initialize（JSON，含 session id）
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"initialize""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "s")
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(INIT_RESULT),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // initialized 通知（204）
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(
                r#""method":"notifications/initialized""#,
            ))
            .respond_with(ResponseTemplate::new(204))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // tools/list：返回一个工具
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/list""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"webSearchPrime","description":"d","inputSchema":{"type":"object"}}]}}"#,
                    ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // tools/call：返回文本结果
        Mock::given(method("POST"))
            .and(path("/mcp"))
            .and(body_string_contains(r#""method":"tools/call""#))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("Content-Type", "application/json")
                    .set_body_string(
                        r#"{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"hit"}],"isError":false}}"#,
                    ),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;

        let cfg = McpConfig {
            servers: HashMap::from([(
                "zhipu".into(),
                McpServerDef {
                    url: Some(format!("{}/mcp", server.uri())),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let resolved = crate::mcp::config::McpResolved::build(cfg).unwrap();
        assert_eq!(
            resolved.servers.get("zhipu").unwrap().transport,
            crate::mcp::TransportKind::Http
        );

        let manager = Arc::new(McpManager::load(resolved).await.unwrap());
        let client = manager.client_for("zhipu").await.unwrap();

        let tools = client.list_tools().await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "webSearchPrime");

        let result = client
            .call_tool("webSearchPrime", serde_json::json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(result.content, serde_json::json!("hit"));
    }
}
