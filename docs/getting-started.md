# 快速开始与配置

[← 返回 README](../README.md)

本文覆盖 CLI 的安装与启动、`rclaw.toml` 的分段总览、配置文件在各平台的位置、全部 API Key 的填写位置以及日志。桌面 / 安卓 / Termux 的构建见 [平台与构建](platforms.md)。

## 环境要求

- **Rust ≥ 1.91**（rustup 安装）与一个 C 编译器（MSVC Build Tools / gcc / clang）：SQLite 与 QuickJS 以内嵌 C 源随 cargo 编译。TLS 全程 rustls，无 OpenSSL、无需安装任何系统 C 库。
- 图形界面（桌面 / 安卓）另需 **Node.js 18+** 构建前端；安卓还需 JDK 17 / SDK / NDK / Gradle，见 [平台与构建](platforms.md)。
- 技能脚本多为 Python（部分需 Node），按 [内置技能目录](skills-catalog.md) 中各技能的依赖自备。

## 编译与启动（CLI）

```bash
# 1. 编译（仓库根目录；-p rclaw-cli 只编 CLI，不需要前端产物）
cargo build --release -p rclaw-cli

# 2. 创建配置：复制示例并填入你的 API Key
cp rclaw.toml.example rclaw.toml   # Windows: copy rclaw.toml.example rclaw.toml

# 3. 启动，进入控制台对话
./target/release/rclaw             # Windows: target\release\rclaw.exe
```

- `rclaw` 唯一识别的命令行参数是 `--daemon`：后台常驻模式，忽略控制台 EOF（适合 `nohup` / 服务方式部署，专供外部通道）。前台模式下 Ctrl-C、Ctrl-D / EOF 退出。
- 控制台按行输入（支持方向键、行内编辑、历史记录）。回复在完成闸门通过后整段输出；控制台不显示思考过程与中间输出，也不收发媒体。
- 启动顺序：读配置 → 建数据目录 → 日志 → 通道 → Provider / 工具 / 定时任务 / 斜杠命令 → 消息总线与 AgentLoop → 常驻。任一步失败即终止（fail-fast）。
- 无参的 `cargo build` 会连同桌面 crate 一起编译，需要先生成前端产物 `ui/dist`，见 [架构与 Workspace 布局](architecture.md#开发)。

## 最小配置

只需改 `[[llm.providers]]` 的 `api_key`（示例默认智谱 GLM）：

```toml
[llm]
provider = "zhipu"
stream = true

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/paas/v4"   # Coding Plan 套餐改用 /api/coding/paas/v4（示例文件默认值）
model = "glm-5.3-flash"
context_length = 1000000
api_key = "your-zhipu-api-key"     # ← 唯一必改项
auth = "bearer"
```

任何 OpenAI 兼容服务都可以接入（DeepSeek、Kimi、OpenRouter、Groq 等），字段含义见 [模型接入与配置](llm.md)。注意 `[llm]`、`[[llm.providers]]`、`[agent]` 都拒绝未知字段，键名拼错会直接启动失败。

## 配置文件位置

| 形态 | 读取位置 | 备注 |
|---|---|---|
| CLI / Termux | **当前工作目录**下的 `rclaw.toml` | 在哪个目录启动就从哪里找 |
| Windows 桌面版 | `RCLAW_HOME` 指定目录，否则 `%USERPROFILE%\.rclaw\rclaw.toml`（非 Windows 回退 `$XDG_CONFIG_HOME/rclaw` → `~/.config/rclaw`） | 首运行无配置时进入图形向导生成；旧版 `%APPDATA%\rclaw` 会在升级后首次启动自动迁入 |
| Android APK | 应用私有目录 | 首运行向导生成，或在向导中导入现成 TOML |

解析规则：顶层与各子段**拒绝未知字段**；非法枚举、非法 URL、空 Key、零上限等在启动期报错。双引号字符串里的 Windows 路径（如 `"D:\rclaw\agent"`）会先自动转义再解析，不必手写双反斜杠。启动时若发现已退役的顶层配置节（如 `web_browser`），会把它从你的 `rclaw.toml` 中剥离并原子写回。`rclaw.toml` 已在 `.gitignore` 中，不会被提交。

## 数据目录与技能目录

`[agent] base_dir`（默认 `agent`，相对启动目录）是全部运行数据的根：会话 / 回滚点 / journal 在 `state.sqlite`，记忆在 `memory/`，人格与用户画像在 `config/`，技能在 `skills/`，还有 `cron.json`、`logs/`、`exports/`、`media/` 等。改这个值等于切换一套独立的记忆、会话、技能与定时任务。完整布局见 [数据目录与持久化](storage.md)。

技能只从数据目录的 `agent/skills/` 发现。仓库根目录的 `skills/` 是**安装源**：

- CLI / 桌面版：把仓库 `skills/` 的内容复制（或链接）到 `agent/skills/`（桌面版即 `%USERPROFILE%\.rclaw\agent\skills\`）。
- Android APK：Gradle 把仓库 `skills/` 打入 APK，首次启动按版本自动解包并保留用户改动。

## `rclaw.toml` 分段总览

完整示例与逐项注释见 [`rclaw.toml.example`](../rclaw.toml.example)。

| 配置段 | 作用 | 详细文档 |
|---|---|---|
| `[agent]` | 数据根目录、会话压缩阈值、自动记忆 / 自进化 / 策展开关、并发上限、工具结果落盘、动态工作流选项、`/goal` 审计 | [ReAct 内核](react-core.md)、[长程会话](context-management.md)、[自进化](self-evolution.md) |
| `[llm]` / `[[llm.providers]]` | 主模型、provider 条目、流式、超时、工具调用上限、评审辅助模型 | [模型接入与配置](llm.md) |
| `[web_search]` 及 `[web_search.<来源>]` | 搜索池顺序、fallback、熔断阈值、各来源 Key | [内置工具](tools.md#联网搜索) |
| `[napcat]` / `[weixin]` / `[wxbot]` | QQ、微信 iLink、个人微信通道 | [外部通道](channels.md) |
| `[cron]` / `[heartbeat]` | 定时任务扫描间隔、心跳间隔 | [定时任务](cron.md) |
| `[log]` | 日志等级与输出目标 | 本文 [日志](#日志) |
| `[mcp]` / `[mcp.servers.<name>]` | MCP server 接入（含 gbrain、ZCode 金融 MCP 示例） | [MCP 扩展](mcp.md)、[gbrain 知识库](gbrain.md) |
| `[skills]` | 技能入口执行参数（如 `max_output_bytes`），整段可省略 | [技能系统](skills.md) |

## API Key 配置清单

主 LLM、通道与技能的 Key 都以本地文件填写。`rclaw.toml` 已 gitignore；技能 Key 文件（`skills/ark_key.toml`、`skills/happyhorse_key.toml`、`skills/*/config.toml`）以空占位**入库且未 gitignore**，填入真实 Key 后切勿 `git add -A` 提交。

### 主配置 `rclaw.toml`

| 配置段 | 字段 | 用途 | 何时需要 |
|---|---|---|---|
| `[[llm.providers]]` | `api_key` | LLM 主推理 | **必填**，缺 Key 启动期报错 |
| `[web_search.doubao_custom]` / `[web_search.tavily]` / `[web_search.zhipu]` / `[web_search.doubao_global]` | `api_key` / `api_keys` | 联网搜索池各来源（Tavily 支持多 Key 自动轮换） | 启用 `[web_search]` 后按启用的来源填 |
| `[weixin]` | `token` | 微信 iLink Bot 凭据（可用 `rclaw-weixin-login` 扫码获取） | 启用微信通道时必填 |
| `[napcat]` | `access_token` | QQ 通道鉴权（与 NapCat 配置一致；留空 = 不鉴权） | 可选 |
| `[wxbot]` | `api_token`、`webhook_secret` | 个人微信 WCX 的 API 令牌与 Webhook 密钥（任一留空则通道不启动） | 启用 wxbot 时必填 |
| `[mcp.servers.gbrain.env]` | `GBRAIN_EMBEDDING_API_KEY`、`GBRAIN_EXPANSION_API_KEY`、`GBRAIN_CHUNKER_API_KEY`、`GBRAIN_KB_RAPTOR_API_KEY`、`GBRAIN_OCR_API_KEY`、转写后端 Key | gbrain 各环节（embedding / 查询扩展 / 分块 / RAPTOR 摘要 / OCR / 语音转写） | 启用 gbrain 时按需 |
| `[mcp.servers.<金融 MCP>.headers]` | `Authorization` + `X-Bigmodel-Authorization` | ZCode 金融 MCP 双头鉴权（同花顺 / Wind 数据轨） | 可选；可用 `scripts/zcode_tokens.py --write` 从本机 ZCode 凭据提取回写 |

### 技能 Key 文件（`skills/` 下，入库的是空值占位）

| 文件 | 字段 | 用途 |
|---|---|---|
| `skills/glm-master-skill/config.toml` | `[zhipu].api_key` | 一份 Key 供 GLM 技能族（OCR / 视觉理解 / 图像生成）共用（`glmv-stock-analyst` 除外） |
| `skills/glmv-stock-analyst/config.toml` | `api_key` + `tushare_token` | 股票分析技能的多模态模型 Key 与 tushare 数据 token；`glm-quota-query` 与 `evaluate-fund`（识图）复用此 Key |
| `skills/ark_key.toml` | `[ark].api_key` | 火山方舟（Seedance / Seedream）生成技能 |
| `skills/happyhorse_key.toml` | `[happyhorse].api_key` | 阿里百炼（HappyHorse）视频生成技能 |
| `skills/ai-berkshire/local/finmind_token.txt` | — | 可选，台股数据 FinMind token（也可用环境变量 `FINMIND_TOKEN`；该目录已 gitignore） |

各技能需要哪个 Key 见 [内置技能目录](skills-catalog.md)。

## 日志

`[log]` 段（整段缺省 = 只写文件、info 级）：

| 键 | 默认 | 说明 |
|---|---|---|
| `console_level` / `file_level` | `info` | 各自独立，取 `error / warn / info / debug / trace` |
| `console` | `false` | 是否输出到 stdout |
| `file` | `true` | 是否写文件，按日轮转为 `<file_name>.YYYY-MM-DD` |
| `dir` | `""`（= `agent/logs`） | 日志目录，相对 CWD 或绝对路径 |
| `file_name` | `rclaw.log` | 文件名前缀 |

环境变量 `RUST_LOG` 若设置则同时覆盖两端等级（运维临时调参）。会话内 `/log` 可列出、读取、删除日志文件，见 [斜杠命令](slash-commands.md)。

## 下一步

- 启动后输入 `/help` 查看全部斜杠命令，`/tools` 与 `/skills` 查看当前工具面与技能。
- 接入 QQ / 微信见 [外部通道](channels.md)；用图形界面见 [平台与构建](platforms.md) 与 [图形界面](gui.md)。
