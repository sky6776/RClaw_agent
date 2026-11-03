# MCP 扩展

[← 返回 README](../README.md)

在 `[mcp]` 段配置任意 MCP（Model Context Protocol）server，其工具会以 `<server>__<tool>` 注册到与内置工具相同的工具表，模型同面调用。实现位于 `crates/rclaw-tools/src/mcp/`（config / manager / provider / client / jsonrpc / transport）。

## 配置

```toml
[mcp]
startup_timeout_secs = 20     # 默认 20
call_timeout_secs = 60        # 默认 60
auto_start = true             # 默认 true

# stdio 方式
[mcp.servers.my-stdio]
command = "npx"
args = ["-y", "@some/mcp-server"]
env = { KEY = "value" }
# cwd = "..."

# streamable-http 方式
[mcp.servers.my-http]
url = "https://example.com/mcp"
transport = "http"
[mcp.servers.my-http.headers]
Authorization = "Bearer ..."
```

单个 server 的全部字段：`command` `args` `env` `cwd` `url` `headers` `transport` `enabled`（缺省 true）`auto_start` `startup_timeout_secs` `call_timeout_secs`（后三项缺省继承 `[mcp]` 顶层）。

- `transport` 取 `stdio` 或 `http`（即 streamable-http）。省略时：只有 `command` 推断为 stdio，只有 `url` 推断为 HTTP，其余组合启动期报错（fail-fast）。

## 生命周期

- `enabled && auto_start` 的 server 在 Manager 加载时立即握手并拉取工具列表（不是纯懒连接）；也支持按需启动与失效后重建。
- 启动失败只告警并跳过，不缓存失败状态；死连接在下次调用时重建。
- 调用因连接中断可重试一次；普通超时与工具返回 `isError` 不重试；超时会尽力发送取消通知。

## 工具命名与 schema

- 工具名 `<server>__<tool>`；描述与 `inputSchema` 原样透传给模型。非法或超长名称会被净化并附哈希消歧，实际远端调用仍使用原名。
- MCP 工具与内置工具在同一个 `ToolRegistry`，受同一套批次 / 熔断 / 截断规则约束（见 [ReAct 内核](react-core.md)、[内置工具](tools.md)）；图形界面「工具」页可逐项启停。
- 入参中的 `path` / `file` 会在 RClaw 侧按 server `cwd` → 其父目录 → 进程 cwd → 可执行文件目录解析为绝对路径（UNC / 设备前缀不解析）；结果中的非文本 content 块与 `structuredContent` 以 `[content]` / `[structuredContent]` 段附加在文本之后。

## 内置示例

`rclaw.toml.example` 自带：

- **gbrain**：安卓 APK 进程内集成的知识库，配置方式见 [gbrain 知识库](gbrain.md)。
- **ZCode 金融 MCP**（9 个 HTTP server）：`hexin-stock` `hexin-index` `hexin-fund` `finance-search` `wind-stock` `wind-docs` `wind-index` `wind-economic` `wind-fund`——同花顺 / Wind 数据轨，供 `pick-funds` / `read-macro` / `watch-positions` 等投研技能包使用。每个 server 同时发送 `Authorization` 与 `X-Bigmodel-Authorization` 两个头（双头鉴权）。

`scripts/zcode_tokens.py` 从本机 ZCode 的加密凭据解出双令牌：可脱敏显示、探测（initialize + 真实调用），或 `--write` 只改写 TOML 中这两类 header。

## 安全提示

header 值以明文字符串写在 `rclaw.toml`（已 gitignore）；错误信息不回显 header 值，传输错误经脱敏。详见 [安全设计](security.md)。
