# 架构与 Workspace 布局

[← 返回 README](../README.md)

RClaw 是一个 11 crate 的 Cargo workspace。依赖方向自下而上、无环：`rclaw-core` 是根；可执行目标有三个——`rclaw-cli` 产出主二进制 `rclaw`（CLI 唯一入口）、`rclaw-desktop` 产出桌面 exe、`rclaw-weixin-login` 是独立的扫码登录工具（刻意不依赖任何 rclaw-* crate）；`rclaw-desktop` 以 feature 在桌面 GUI / Termux 桩 / APK 宿主三态之间切换。`default-members` 为 9 个桌面侧 crate，`rclaw-gbrain` / `rclaw-android` 只经 `apk-host` 链路进入构建。项目术语表见 [`CONTEXT.md`](../CONTEXT.md)。

## crate 职责

```
crates/
├── rclaw-core        消息总线（BusMessage、tool_access）与领域类型（Message / ContentBlock / Role …）
├── rclaw-store       SQLite 会话 / journal / kv、文件系统 Layout、记忆文件、技能索引、bot 定义、cron.json 任务表、redact
├── rclaw-llm         Provider 两层抽象（协议层 OpenAiCompat + 方言层）、SSE 流式解析、热切换
├── rclaw-tools       内置工具（文件 / shell / 记忆 / 检索 / 下载）、技能运行时（Manifest 入口、台账、信号、用量）、
│                     web_search 搜索池、CDP 浏览器、MCP 客户端、cron 工具
├── rclaw-gbrain      知识库子系统（lib-only，仅 APK 进程内集成）
├── rclaw-channels    外部通道适配：console / napcat / weixin / wxbot
├── rclaw-runtime     AgentLoop、turn 循环与 ReAct 内核、上下文压缩、回滚、review、多智能体
│                     （Agent / 团队 / PlanRun / 动态工作流 / bot）、动态工具集、斜杠命令、cron 调度、心跳
├── rclaw-cli         主二进制 rclaw；以 lib 形式暴露 config / bootstrap / logging 供桌面与安卓复用
├── rclaw-desktop     三态 crate：桌面 Tauri GUI（bin rclaw-desktop）/ Termux 提示桩 / APK 宿主（feature apk-host，
│                     staticlib + cdylib）；含共用 React 前端 ui/
├── rclaw-android     Android 桥接：GUI 装配、CDP WebView 浏览器、Chaquopy bridge executor、数据目录归一化
└── rclaw-weixin-login 微信 iLink Bot 扫码登录工具（独立 bin）
```

## 分层与数据流

```
通道适配器 ──入站 BusMessage──▶ AgentLoop（路由，按 (channel, chat_id, cron_job_id) 分派）
                                   │
                                   ▼
                          turn 循环（agent-run 内核）
                 系统提示 ◀── SOUL / 记忆 / 技能索引 / 目标 / 团队状态
                 模型推理 ◀── rclaw-llm Provider
                 工具批次 ──▶ ToolRegistry（内置 + 技能动态工具 + MCP）
                 完成闸门 / 媒体闸门
                                   │
                                   ▼
             出站 BusMessage ──▶ 通道适配器（按能力交付：文本 / 媒体 / 中间输出）
                                   │
                          SessionStore（state.sqlite）← 历史、checkpoint、journal
                          后台 review / curator（静默时）
```

- **agent-run 内核**：主 turn 与子代理共享循环策略（上下文尾部、重试退避、工具批次、媒体认领闸门、完成闸门）；适配器只贡献完成闸门后的附加检查（子代理的团队闸门），插话处理、执行信号量、流式 / 非流式与推理错峰等属于两处循环各自的结构差异。
- **TurnState**：单个 turn 全部可变状态的归属模块，工具批次与循环本体在它上面相遇。
- **DynamicToolSet**：已激活 Manifest 工具集合及其生命周期的唯一所有者。
- **静默（quiescence）**：「没有任何 turn、review、curator 在运行」的可订阅状态；`/clear` 与策展器等待它而非各自探测。
- **通道能力**：每个适配器声明是否可收发媒体、是否有工作状态、是否许可中间输出、是否接收逐片思考流（仅桌面为真）；能力随接线传递，运行时据此决定输出行为。

## 三态 `rclaw-desktop`

| 目标 | feature | 形态 |
|---|---|---|
| 非 Android target | 无 | Windows 桌面 Tauri GUI |
| Android target | 无 | Termux 提示桩（CLI 构建不包含 GUI） |
| Android target | `apk-host`（构建时另开 `tauri/custom-protocol`） | APK 宿主，产出 `librclaw_desktop.so` |

前端 `crates/rclaw-desktop/ui/` 为 React + Vite + Tailwind，构建产物 `ui/dist` 在 Tauri `generate_context!` 编译期内嵌。

## 开发

```bash
# 前置：rclaw-desktop 在 default-members 内，Tauri 的 generate_context! 编译期要求 ui/dist 存在（该目录不进 git）
cd crates/rclaw-desktop/ui && npm install && npm run build && cd ../../..

cargo check                      # 无参 = 默认 9 个桌面侧 crate（仓库门禁约定，不加 --workspace）
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cd crates/rclaw-desktop/ui && npm run typecheck   # tsc --noEmit
```

- 只想编 CLI 时用 `cargo build --release -p rclaw-cli`，不需要前端产物。
- `--workspace` 会把仅供 APK 的 `rclaw-gbrain` / `rclaw-android` 拉进宿主机编译，日常门禁不要加；Android 相关 crate 经 `apk-host` feature 链路进入 APK 构建，见 [平台与构建](platforms.md)。
- 测试依赖 `tempfile`；若本机 `%TEMP%` 对新建目录有权限限制，可把 `TMP` / `TEMP` 临时指到 `target/` 下。
- 协作规范见 [`AGENTS.md`](../AGENTS.md)。

## 设计文档

| 文档 | 内容 |
|---|---|
| [`CONTEXT.md`](../CONTEXT.md) | 项目术语表（turn、工具批次、skill 激活、bot、交接链、群聊房间、plan 工件、媒体闸门、静默…） |
| [`docs/zcode-port-longhorizon.md`](zcode-port-longhorizon.md) | ZCode 长程能力移植的逐轮对照记录（三层压缩、artifact、fork、后台唤醒、工作流沙箱、`/goal`） |
| [`docs/workflow-tool-surface-port.md`](workflow-tool-surface-port.md) | 动态工作流工具面（CreateWorkflow / Amend / Save / FillHole …）的设计与移植记录 |
| [`docs/review-deferred-issues.md`](review-deferred-issues.md) | 审查中记录的延后处理事项 |
| [`crates/rclaw-desktop/README.md`](../crates/rclaw-desktop/README.md) | 桌面 crate 的模块说明与运行语义 |
