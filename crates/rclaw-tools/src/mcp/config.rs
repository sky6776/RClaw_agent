//! MCP 服务器配置：数据结构 + 「解析/回退」逻辑。
//!
//! 适配 RClaw 的配置风格（snake_case、`deny_unknown_fields`）：
//! - 配置**仅**来自 `rclaw.toml` 的 `[mcp]` 段（标量默认值 + `[mcp.servers.<name>]` 表）。
//! - 单个服务器缺省的超时/auto_start 字段回退到 `[mcp]` 段的全局默认值。
//!
//! 本模块只做配置解析，不启动任何进程；进程启动在 [`crate::mcp::client`] 与
//! [`crate::mcp::manager`] 中完成。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 传输类型
// ---------------------------------------------------------------------------

/// MCP 服务器的传输类型。
///
/// - [`TransportKind::Stdio`]：spawn 子进程，stdin/stdout 收发 JSON-RPC（传统、零网络依赖）。
/// - [`TransportKind::Http`]：远程 streamable-http，per-request POST，响应可为 JSON 或 SSE。
///
/// 配置缺省 `transport` 时按字段自动推断：仅有 `command` → Stdio；仅有 `url` → Http；
/// 同时存在或都不存在则启动期 fail-fast（见 [`McpServerDef::resolve`]）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransportKind {
    Stdio,
    Http,
}

// ---------------------------------------------------------------------------
// 默认值函数（serde `default = ...` 与 `Default` 实现共用，避免两处不一致）
// ---------------------------------------------------------------------------

fn dft_startup_timeout() -> u64 {
    20
}
fn dft_call_timeout() -> u64 {
    60
}
fn dft_auto_start() -> bool {
    true
}
fn dft_enabled() -> bool {
    true
}

/// 单个服务器「未显式配置」时回退的全局默认值。
///
/// 同时也是 [`McpResolved`] 里保留的 defaults 快照（供后续按需启动复用）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpDefaults {
    #[serde(default = "dft_startup_timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default = "dft_call_timeout")]
    pub call_timeout_secs: u64,
    #[serde(default = "dft_auto_start")]
    pub auto_start: bool,
}

impl Default for McpDefaults {
    fn default() -> Self {
        Self {
            startup_timeout_secs: dft_startup_timeout(),
            call_timeout_secs: dft_call_timeout(),
            auto_start: dft_auto_start(),
        }
    }
}

/// 单个 MCP 服务器的「原始定义」（对应 `[mcp.servers.<name>]`）。
///
/// `auto_start` / 两个超时字段为 `Option`：缺省时回退到所属配置段的默认值。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpServerDef {
    /// 启动命令（stdio 传输必填；可执行文件名或绝对路径）。
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// 远程 streamable-http 端点 URL（http 传输必填）。
    #[serde(default)]
    pub url: Option<String>,
    /// 远程 http 请求附加头（如 `Authorization = "Bearer ..."`）。
    #[serde(default)]
    pub headers: Option<HashMap<String, String>>,
    /// 传输类型；缺省时按 `command`/`url` 推断（见 [`TransportKind`]）。
    #[serde(default)]
    pub transport: Option<TransportKind>,
    #[serde(default = "dft_enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub auto_start: Option<bool>,
    #[serde(default)]
    pub startup_timeout_secs: Option<u64>,
    #[serde(default)]
    pub call_timeout_secs: Option<u64>,
}

impl Default for McpServerDef {
    // 与 serde 默认保持一致：手工构造的服务器默认 enabled=true，避免「派生 Default 给
    // false、反序列化给 true」的不一致（定义但未显式 enabled 的服务器应被视为启用）。
    fn default() -> Self {
        Self {
            command: None,
            args: Vec::new(),
            env: HashMap::new(),
            cwd: None,
            url: None,
            headers: None,
            transport: None,
            enabled: dft_enabled(),
            auto_start: None,
            startup_timeout_secs: None,
            call_timeout_secs: None,
        }
    }
}

impl McpServerDef {
    /// 用全局默认值填补缺省字段，得到确定值的 [`McpResolvedServer`]。
    ///
    /// 校验传输类型并确定化：显式 `transport` 按其校验（stdio 需 `command`，http 需 `url`）；
    /// 缺省则按字段推断（仅 `command`→Stdio，仅 `url`→Http，两者都有/都无→错误）。
    fn resolve(self, defaults: &McpDefaults) -> Result<McpResolvedServer, String> {
        let transport = match self.transport {
            Some(t) => t,
            None => match (&self.command, &self.url) {
                (Some(_), None) => TransportKind::Stdio,
                (None, Some(_)) => TransportKind::Http,
                _ => {
                    return Err(
                        "无法推断传输类型：请显式指定 transport，或仅提供 command(stdio)/url(http) 之一"
                            .into(),
                    );
                }
            },
        };
        match transport {
            TransportKind::Stdio => {
                if self.command.is_none() {
                    return Err("stdio 传输需要 command".into());
                }
            }
            TransportKind::Http => {
                let Some(url) = self.url.as_deref() else {
                    return Err("http 传输需要 url".into());
                };
                let parsed = reqwest::Url::parse(url).map_err(|e| format!("http url 非法: {e}"))?;
                if !(parsed.scheme() == "http" || parsed.scheme() == "https")
                    || parsed.host_str().is_none()
                {
                    return Err(format!("http url 必须是 http(s) URL 且含 host: {url}"));
                }
            }
        }
        Ok(McpResolvedServer {
            transport,
            command: self.command,
            args: self.args,
            env: self.env,
            cwd: self.cwd,
            url: self.url,
            headers: self.headers,
            enabled: self.enabled,
            auto_start: self.auto_start.unwrap_or(defaults.auto_start),
            startup_timeout_secs: self
                .startup_timeout_secs
                .unwrap_or(defaults.startup_timeout_secs),
            call_timeout_secs: self.call_timeout_secs.unwrap_or(defaults.call_timeout_secs),
        })
    }
}

/// `rclaw.toml` 的 `[mcp]` 段。
///
/// 顶层三个标量即全局默认值；`servers` 为 `name → 定义` 映射。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default = "dft_startup_timeout")]
    pub startup_timeout_secs: u64,
    #[serde(default = "dft_call_timeout")]
    pub call_timeout_secs: u64,
    #[serde(default = "dft_auto_start")]
    pub auto_start: bool,
    #[serde(default)]
    pub servers: HashMap<String, McpServerDef>,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            startup_timeout_secs: dft_startup_timeout(),
            call_timeout_secs: dft_call_timeout(),
            auto_start: dft_auto_start(),
            servers: HashMap::new(),
        }
    }
}

impl McpConfig {
    fn defaults(&self) -> McpDefaults {
        McpDefaults {
            startup_timeout_secs: self.startup_timeout_secs,
            call_timeout_secs: self.call_timeout_secs,
            auto_start: self.auto_start,
        }
    }
}

/// 解析后的单服务器配置（所有字段已具确定值，可直接用于启动）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResolvedServer {
    pub transport: TransportKind,
    pub command: Option<String>,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    pub headers: Option<HashMap<String, String>>,
    pub enabled: bool,
    pub auto_start: bool,
    pub startup_timeout_secs: u64,
    pub call_timeout_secs: u64,
}

/// 解析后的 MCP 配置：全局默认值 + 全部已解析的服务器。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpResolved {
    #[serde(default)]
    pub defaults: McpDefaults,
    #[serde(default)]
    pub servers: HashMap<String, McpResolvedServer>,
}

impl McpResolved {
    /// 由 `[mcp]` 段构造：把每个服务器定义用全局默认值填补缺省字段。
    ///
    /// 返回 `Result`：任一服务器的传输类型校验/推断失败时聚合首个错误（启动期 fail-fast）。
    pub fn build(config: McpConfig) -> Result<Self, String> {
        let defaults = config.defaults();
        let mut servers: HashMap<String, McpResolvedServer> = HashMap::new();
        for (name, def) in config.servers {
            let resolved = def
                .resolve(&defaults)
                .map_err(|e| format!("MCP[{name}]: {e}"))?;
            servers.insert(name, resolved);
        }
        Ok(Self { defaults, servers })
    }

    /// 所有 `enabled = true` 的服务器名（顺序不确定，调用方按需排序）。
    pub fn enabled_server_names(&self) -> Vec<String> {
        self.servers
            .iter()
            .filter_map(|(name, cfg)| cfg.enabled.then_some(name.clone()))
            .collect()
    }
}

// ===========================================================================
// 单元测试：聚焦配置「解析 / 默认回退」这条无子进程依赖的确定性路径。
// ===========================================================================
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_falls_back_to_defaults() {
        let cfg = McpConfig {
            startup_timeout_secs: 5,
            call_timeout_secs: 9,
            auto_start: false,
            servers: HashMap::from([(
                "fs".to_string(),
                McpServerDef {
                    command: Some("npx".into()),
                    ..Default::default()
                },
            )]),
        };
        let resolved = McpResolved::build(cfg).unwrap();
        let fs = resolved.servers.get("fs").unwrap();
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert_eq!(fs.transport, TransportKind::Stdio);
        assert_eq!(fs.startup_timeout_secs, 5); // 来自 [mcp] 默认
        assert_eq!(fs.call_timeout_secs, 9);
        assert!(!fs.auto_start);
        assert!(fs.enabled); // 默认 true
    }

    #[test]
    fn per_server_override_beats_defaults() {
        // 单个服务器显式给出的超时/auto_start 覆盖全局默认。
        let cfg = McpConfig {
            servers: HashMap::from([(
                "git".to_string(),
                McpServerDef {
                    command: Some("uvx".into()),
                    auto_start: Some(false),
                    startup_timeout_secs: Some(7),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let resolved = McpResolved::build(cfg).unwrap();
        let git = resolved.servers.get("git").unwrap();
        assert_eq!(git.startup_timeout_secs, 7);
        assert!(!git.auto_start);
        assert_eq!(git.call_timeout_secs, 60); // 仍回退到全局默认
    }

    #[test]
    fn enabled_server_names_filters_disabled() {
        let cfg = McpConfig {
            servers: HashMap::from([
                (
                    "on".to_string(),
                    McpServerDef {
                        command: Some("c".into()),
                        enabled: true,
                        ..Default::default()
                    },
                ),
                (
                    "off".to_string(),
                    McpServerDef {
                        command: Some("c".into()),
                        enabled: false,
                        ..Default::default()
                    },
                ),
            ]),
            ..Default::default()
        };
        let resolved = McpResolved::build(cfg).unwrap();
        let mut names = resolved.enabled_server_names();
        names.sort();
        assert_eq!(names, vec!["on".to_string()]);
    }

    #[test]
    fn empty_config_resolves_to_empty() {
        let resolved = McpResolved::build(McpConfig::default()).unwrap();
        assert!(resolved.servers.is_empty());
    }

    #[test]
    fn mcp_config_default_is_sensible() {
        let d = McpConfig::default();
        assert_eq!(d.startup_timeout_secs, 20);
        assert_eq!(d.call_timeout_secs, 60);
        assert!(d.auto_start);
        assert!(d.servers.is_empty());
    }

    #[test]
    fn infer_stdio_from_command_only() {
        let cfg = McpConfig {
            servers: HashMap::from([(
                "fs".into(),
                McpServerDef {
                    command: Some("npx".into()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let r = McpResolved::build(cfg).unwrap();
        assert_eq!(r.servers.get("fs").unwrap().transport, TransportKind::Stdio);
    }

    #[test]
    fn infer_http_from_url_only() {
        let cfg = McpConfig {
            servers: HashMap::from([(
                "zhipu".into(),
                McpServerDef {
                    url: Some("https://open.bigmodel.cn/api/mcp/x/mcp".into()),
                    headers: Some(HashMap::from([("Authorization".into(), "Bearer k".into())])),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        let r = McpResolved::build(cfg).unwrap();
        let s = r.servers.get("zhipu").unwrap();
        assert_eq!(s.transport, TransportKind::Http);
        assert_eq!(
            s.url.as_deref(),
            Some("https://open.bigmodel.cn/api/mcp/x/mcp")
        );
        assert_eq!(
            s.headers.as_ref().unwrap().get("Authorization").unwrap(),
            "Bearer k"
        );
    }

    #[test]
    fn both_command_and_url_without_transport_is_error() {
        let cfg = McpConfig {
            servers: HashMap::from([(
                "x".into(),
                McpServerDef {
                    command: Some("npx".into()),
                    url: Some("https://e/mcp".into()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert!(McpResolved::build(cfg).is_err());
    }

    #[test]
    fn http_without_url_is_error() {
        let cfg = McpConfig {
            servers: HashMap::from([(
                "x".into(),
                McpServerDef {
                    transport: Some(TransportKind::Http),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert!(McpResolved::build(cfg).is_err());
    }

    #[test]
    fn http_bad_url_is_error() {
        // 非 http(s) scheme 或缺 host 的 url 应在配置期 fail-fast，而非推迟到首次请求。
        for bad in ["not-a-url", "ftp://example.com/mcp", "://nohost"] {
            let cfg = McpConfig {
                servers: HashMap::from([(
                    "x".into(),
                    McpServerDef {
                        url: Some(bad.into()),
                        ..Default::default()
                    },
                )]),
                ..Default::default()
            };
            assert!(McpResolved::build(cfg).is_err(), "应拒绝 url: {bad}");
        }
    }

    #[test]
    fn explicit_transport_overrides_inference() {
        let cfg = McpConfig {
            servers: HashMap::from([(
                "x".into(),
                McpServerDef {
                    transport: Some(TransportKind::Stdio),
                    command: Some("npx".into()),
                    ..Default::default()
                },
            )]),
            ..Default::default()
        };
        assert_eq!(
            McpResolved::build(cfg)
                .unwrap()
                .servers
                .get("x")
                .unwrap()
                .transport,
            TransportKind::Stdio
        );
    }
}
