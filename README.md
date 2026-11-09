# RClaw

**装在自己电脑和手机上的个人 AI Agent** —— 从安卓APP、微信、QQ、控制台、桌面APP接收任务，以 ReAct 循环驱动技能、子代理、工作流与定时任务，并靠长期记忆与技能自进化在使用中持续变强；单二进制交付，开箱即用。

```
外部消息（微信 / QQ / 控制台 / 桌面 / 安卓手机） → 统一 turn 循环 → ReAct 工具调用 → 交付回复
```

本页只做整体介绍；每个功能模块都有独立的详细文档，见各节末尾的链接或 [文档导航](#文档导航)。

## 功能总览

### 运行形态与接入

- **多平台一体**：同一套核心以四种形态交付——全平台 CLI 二进制 `rclaw`、Windows 桌面 GUI（Tauri）、Android 原生 APK（arm64-v8a，内置 Python 运行时 / 语音识别 / 知识库）、Termux 手机直跑 CLI。桌面与安卓共用同一个 React 前端，GitHub Release 自动产出三种安装包。→ [平台与构建](docs/platforms.md)
- **多通道并发**：控制台、桌面 / 安卓虚拟通道、QQ（NapCat OneBot 11）、微信 iLink Bot、个人微信（wxbot）可同时在线；每个通道 / 聊天独立会话、独立并发控制，各自声明「能否收发媒体 / 有无工作状态 / 是否许可中间输出 / 是否接收逐片思考流」四项能力。→ [外部通道](docs/channels.md)
- **模型接入**：OpenAI 兼容协议即可接入任意服务（示例默认智谱 GLM，也可 DeepSeek 等）；SSE 流式解析、思考字段适配、工具调用严格校验、指数退避重试；`/llmodel` 热切换模型并写回配置；bot、子代理、后台评审可各自固定不同 provider。→ [模型接入与配置](docs/llm.md)
- **图形界面**：对话 / 会话 / 智能体 / 工具 / 技能 / 工作区 / Agent / 任务概览 / 自进化 / 定时任务 / 配置 / 关于 十余个面板；消息分叉、排队插话、计划步骤卡片、工具调用明细与 diff、媒体画廊与照片编辑、`rclaw.toml` 可视化编辑与校验、应用内更新；安卓端另有前台服务保活、拍照 / 录音 / whisper 转写、内置浏览器。→ [图形界面](docs/gui.md)

### Agent 内核

- **ReAct 内核**：主 turn 与子代理共享同一套循环策略机器——工具批次按 `parallel_safe` 并行、重复调用软 / 硬熔断、失败重试退避、**完成闸门**（结果声明必须有副作用回执、禁止空回复与内部帧泄露）、**媒体认领闸门**（未验证的媒体标记不得宣称交付）、用户插话在协议安全点注入、取消时补写 aborted 结果闭合历史链。→ [ReAct 内核](docs/react-core.md)
- **长程会话与上下文管理**：三层压缩（加载期压缩 + 轮内 microcompact / auto / reactive）、超预算工具结果落盘为「预览 + 路径」引用句柄、文件写入 checkpoint 与 `/rewind` 回滚、任意用户消息锚点 `/fork` 分叉、`/goal` 会话级目标提醒与完成审计、`/export` 完整导出、跨会话检索与交接（`session_search` / `session_handoff`）。→ [长程会话与上下文管理](docs/context-management.md)
- **内置工具**：文件读写 / 编辑 / 删除 / 列举 / grep（默认沙箱在数据目录内，越界需确认或显式提权）、shell 与后台任务轮询、记忆、跨会话检索、多来源高可用联网搜索池（熔断 / 额度降级 / 多 Key 轮换）、拟人化 CDP 浏览器（仅桌面 / APK）、媒体下载、wxbot 微信消息读取（`wxbot_chat_list` / `wxbot_chat_history` / `wxbot_message_search`，本地消息库只读检索）。→ [内置工具](docs/tools.md)

### 能力扩展

- **技能系统**：`SKILL.md` 渐进披露、读取即激活；`skill.toml` 声明式 Manifest 编译成结构化动态工具 `skill__<skill>__<entrypoint>`（14 种参数类型、约束、固定子命令，参数以 argv 直达进程、不经 shell），从源头消除路径幻觉、命令注入与 Key 泄漏。→ [技能系统](docs/skills.md)
- **内置技能目录**：仓库自带 40 个顶层技能——智谱 GLM 官方族（OCR / 多模态 / 文生图等 16 个）、图像视频生成 7 个、投研金融 7 个（含 4 个多子技能包）、办公文档 4 个（docx / pdf / pptx / xlsx）、电商与社媒抓取 4 个、工具类 2 个。→ [内置技能目录](docs/skills-catalog.md)
- **记忆与自进化**：`MEMORY.md` / `USER.md` / 每日笔记三层记忆（预算制、写入脱敏与提示注入防护）、`SOUL.md` 人格库（四份内置人格——微信真人口吻 / 苏轼 / 东北老铁 / 李逵——配置页可切换、可从本机或手机存储批量导入；覆盖前确认并自动备份为 `SOUL.md.bak.<时间戳>`，`[agent] soul_enabled` 可整体停用）、turn 结束后的后台 review（token 预算 + 辅助模型）、技能结果记账、进化信号、`EXPERIENCE.md` 热经验层、curator 策展器的 Active → Stale → Archived 生命周期、sha256 台账逐变更回滚。→ [记忆与自进化](docs/self-evolution.md)
- **多智能体协作**：`Agent` 子代理委派（拆分理由门控、前台 / 后台、结构化交接）、团队共享任务表与成员邮箱、子代理实时进度面板（状态 / 阶段 / 时长 / token，详情含完整活动记录并可对话）、`PlanRun` 依赖序多步计划（并行波次、对抗验证环、暂停 / 续跑、`escalate` 提问）、确定性 QuickJS **动态工作流**（`.ask<T>()` 类型编译、journal 零调用重放、amend 热修）、持久身份 **bot** 与 2–6 bot 群聊房间、`message_agent` 不可伪造交接链。→ [多智能体协作](docs/multi-agent.md)
- **定时任务与心跳**：`after / at / clock / every` 与日 / 周 / 月 / 年 / 农历日历规则，一次性或重复；到期任务走完整 ReAct turn 并投递到指定通道；`HEARTBEAT.md` 周期自检。→ [定时任务](docs/cron.md)
- **gbrain 知识库**（Android APK 进程内集成）：SQLite + sqlite-vec + FTS5 的混合检索、文档解析与 GLM-OCR 管线、RAPTOR 递归摘要树、制品路由与审核队列，18 个工具。→ [gbrain 知识库](docs/gbrain.md)
- **MCP 扩展**：`[mcp]` 段配置任意 stdio / streamable-http MCP server，工具以 `<server>__<tool>` 与内置工具同面调用；内置 ZCode 金融 MCP（同花顺 / Wind 数据轨）示例与令牌提取脚本。→ [MCP 扩展](docs/mcp.md)

### 运维与安全

- **斜杠命令**：`/llmodel` `/search` `/skills` `/tools` `/context` `/compact` `/rewind` `/fork` `/goal` `/export` `/log` `/clear` `/cancel` 等，由路由层直接处理、不进入 LLM（`/workflow` 与 `/goal <文本>` 例外：展开为提示词交给模型，`/goal <文本>` 设定后立即开始执行）；影响全局的命令仅限本机或已授权会话。→ [斜杠命令](docs/slash-commands.md)
- **数据与持久化**：一切运行数据集中在 `agent/`（会话 / 回滚点 / journal 在 `state.sqlite`，记忆、技能、日志、导出、媒体各有目录，定时任务在 `cron.json`）；`/clear` 的精确语义与备份建议。→ [数据目录与持久化](docs/storage.md)
- **安全设计**：文件工具 fail-safe 沙箱与路径规则、通道身份 → 权限单向链、工具结果 / 摘要 / 记忆写入的密钥脱敏、记忆与技能写入的注入 / 隐形字符防护、密钥只落本地文件。→ [安全设计](docs/security.md)
- **架构**：11 个 crate 的 workspace，`rclaw-core` 为无环依赖的根，`rclaw-cli` 产出主二进制 `rclaw`（另有桌面 exe 与 `rclaw-weixin-login` 两个 bin），`rclaw-desktop` 三态 crate（桌面 GUI / Termux 桩 / APK 宿主）。→ [架构与 Workspace 布局](docs/architecture.md)

## 平台支持一览

| 平台 | 形态 | 说明 |
|---|---|---|
| Windows / Linux / macOS | CLI 二进制 `rclaw` | 全功能，`cargo build --release -p rclaw-cli` 即得 |
| Windows | 桌面 GUI（Tauri 2） | 托盘常驻 + 全部图形面板，数据在 `%USERPROFILE%\.rclaw` |
| Android | 原生 APK（arm64-v8a） | Tauri Android 壳，内置 Python 3.12 技能运行时、whisper 语音识别与 gbrain 知识库 |
| Android | Termux CLI | 手机上直接编译运行 `rclaw`，专用低内存构建档 `release-termux` |

预构建产物（安卓 APK、Windows 免安装桌面版、Termux aarch64 包）在 [GitHub Releases](https://github.com/sky6776/RClaw_agent/releases) 自动发布。详见 [平台与构建](docs/platforms.md)。

## 快速开始（CLI）

环境要求：**Rust ≥ 1.91** 与一个 C 编译器（SQLite / QuickJS 以内嵌 C 源随 cargo 编译）。TLS 全程 rustls，无 OpenSSL、无需安装任何系统 C 库。

```bash
# 1. 编译（仓库根目录；只编 CLI，不需要前端产物）
cargo build --release -p rclaw-cli

# 2. 创建配置：复制示例并填入你的 API Key
cp rclaw.toml.example rclaw.toml   # Windows: copy rclaw.toml.example rclaw.toml

# 3. 启动，进入控制台对话
./target/release/rclaw             # Windows: target\release\rclaw.exe
```

最小配置只需改一处——`[[llm.providers]]` 段的 `api_key`（示例默认智谱 GLM）：

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

配置从**当前工作目录**读取 `rclaw.toml`；缺 Key 启动期直接报错（fail-fast）。无参的 `cargo build` / `cargo test` 会连同桌面 crate 一起编译，需要先构建前端产物（见 [架构与 Workspace 布局](docs/architecture.md#开发)）。要让内置技能可用，把仓库 `skills/` 目录的内容放进数据目录的 `agent/skills/`（APK 首次启动会自动解包）。完整的安装步骤、配置文件分段说明、全部 API Key 填写位置见 [快速开始与配置](docs/getting-started.md)。

## 文档导航

| 文档 | 内容 |
|---|---|
| [快速开始与配置](docs/getting-started.md) | 环境、编译、`rclaw.toml` 分段总览、配置文件位置、API Key 清单、日志 |
| [平台与构建](docs/platforms.md) | CLI / Windows 桌面 / Android APK（签名、应用内更新、安装）/ Termux、CI 发布、构建脚本、profile 与 feature |
| [图形界面](docs/gui.md) | 桌面与安卓共用前端的全部面板、桌面宿主（托盘 / 向导 / 导出）、安卓宿主（保活 / Python / whisper / 浏览器） |
| [外部通道](docs/channels.md) | 控制台、NapCat QQ、微信 iLink、个人微信 wxbot、能力矩阵、媒体与语音、会话隔离与可靠性 |
| [模型接入与配置](docs/llm.md) | Provider 两层抽象、`[llm]` 字段、流式与工具调用解析、重试、`/llmodel`、多 provider |
| [ReAct 内核](docs/react-core.md) | turn 流程、系统提示、工具批次、熔断、完成 / 媒体闸门、插话队列、流式输出、后台唤醒、运行时选项 |
| [长程会话与上下文管理](docs/context-management.md) | 三层压缩、工具结果落盘、`/rewind`、`/fork`、`/goal`、`/export`、跨会话检索与交接、会话存储模型 |
| [内置工具](docs/tools.md) | 全部内置工具的参数、限制与沙箱规则，搜索池与浏览器 |
| [技能系统](docs/skills.md) | 技能发现、Manifest 完整 schema、动态工具生命周期、运行时、设计动机、`skill_manage` |
| [内置技能目录](docs/skills-catalog.md) | 40 个内置技能的分类清单、Key 文件与运行依赖 |
| [记忆与自进化](docs/self-evolution.md) | 三层记忆、SOUL 人格、后台 review、技能记账 / 信号 / 热经验 / 策展 / 台账回滚 |
| [多智能体协作](docs/multi-agent.md) | Agent 委派、团队任务表、实时进度面板、PlanRun、动态工作流、bot 与群聊、交叉验证 |
| [定时任务](docs/cron.md) | 调度类型、投递与执行语义、持久化、工具与界面、心跳 |
| [gbrain 知识库](docs/gbrain.md) | 子系统组成、18 个工具、文档与 OCR、embedding 与 RAPTOR、配置、平台可用性 |
| [MCP 扩展](docs/mcp.md) | `[mcp]` 配置、传输方式、生命周期、工具命名、ZCode 金融 MCP |
| [斜杠命令](docs/slash-commands.md) | 全部 `/` 命令与子命令 |
| [数据目录与持久化](docs/storage.md) | `agent/` 布局、SQLite 表、journal、`/clear` 语义、备份 |
| [安全设计](docs/security.md) | 沙箱、权限链、脱敏范围、写入防护、密钥存放 |
| [架构与 Workspace 布局](docs/architecture.md) | crate 职责与依赖方向、三态 crate、开发命令、设计文档索引 |

## Workspace 布局

```
crates/
├── rclaw-core        消息总线与领域类型（无环依赖规则的根）
├── rclaw-store       会话 / 记忆 / journal 持久化 + 人格库（SOUL）+ 敏感信息 redact
├── rclaw-llm         Provider 两层抽象（协议层 + 方言层）、SSE 流式解析
├── rclaw-tools       内置工具、技能运行时、MCP 客户端、搜索池、浏览器
├── rclaw-gbrain      知识库子系统（lib-only，仅 APK 进程内集成）
├── rclaw-channels    外部通道适配（console / napcat / weixin / wxbot）
├── rclaw-runtime     AgentLoop、turn 循环、ReAct 内核、多智能体、工作流、cron
├── rclaw-cli         主二进制 rclaw（CLI 入口）；以 lib 暴露 config / bootstrap / logging
├── rclaw-desktop     三态 crate：桌面 Tauri GUI（bin rclaw-desktop）/ Termux 提示桩 / APK 宿主（apk-host）
├── rclaw-android     Android 桥接（GUI 装配、浏览器、bridge executor）
└── rclaw-weixin-login 微信扫码登录（独立 bin）
```

开发：先构建前端（`cd crates/rclaw-desktop/ui && npm install && npm run build`），再 `cargo check` / `cargo test` / `cargo clippy --all-targets -- -D warnings`（无参 = 默认 9 个桌面侧 crate）；详见 [架构与 Workspace 布局](docs/architecture.md)。项目术语表见 [`CONTEXT.md`](CONTEXT.md)。

## 致谢

RClaw 站在开源社区的肩膀上——从起源项目到同类 agent 的机制借鉴：

- **[CClaw](https://github.com/sky6776/CClaw)**——RClaw 最初是 CClaw 的 Rust 实现，后续逐步扩展为独立的多通道自进化 agent 运行时。
- **[ZCode](https://github.com/zai-org/ZCode)**（zai-org / 智谱）——长程任务能力的参照系：轮内三层压缩（microcompact / auto / reactive）、工具结果落盘与引用句柄、会话任意锚点 fork、后台完成合批唤醒、确定性工作流沙箱与 `.ask<T>` 类型编译、`/goal` 目标提醒等机制移植自此（逐轮对照记录见 [`docs/zcode-port-longhorizon.md`](docs/zcode-port-longhorizon.md)）。
- **[Hermes Agent](https://github.com/NousResearch/hermes-agent)**（NousResearch）——同谱系的自进化 agent。RClaw 的自进化体系（后台评审、记忆 / 技能 review、curator 策展），以及跨会话检索、评审成本控制（token 预算 + digest 重放）、技能 sha256 台账回滚、记忆加载期复扫与漂移检测等机制，均借鉴自 Hermes。
- **[Blockcell](https://github.com/blockcell-labs/blockcell)**（blockcell-labs）——Rust 构建的 agent 框架，RClaw 的 NapCat（QQ 通道）实现借鉴自该项目。
- **[ai-berkshire](https://github.com/xbtlin/ai-berkshire)**（xbtlin）——`skills/ai-berkshire` 投研技能包从该 Claude Code 仓库修改适配而来，四大师价值投资研究法的原始出处。
- **[GLM-skills](https://github.com/zai-org/GLM-skills)**（zai-org / 智谱）——本仓库 16 个 GLM 官方技能（OCR / 视觉 / 图像生成族）移植自此，并针对 RClaw 的 workspace 路径、共享 TOML 配置与按需交付做了适配。
- **OpenClaw 生态与 Anthropic 的技能目录设计**——渐进披露与技能描述三段式（Use when / Do NOT use / 前置成本）的设计来源。
- **[whisper.cpp](https://github.com/ggml-org/whisper.cpp)**——构建期由脚本拉取到 `third_party/whisper.cpp`，配合仓库内的 `third_party/whisper-jni` 胶水层，为语音消息提供本地 ASR。

## License

[MIT](LICENSE)
