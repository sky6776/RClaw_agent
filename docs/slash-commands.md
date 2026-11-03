# 斜杠命令

[← 返回 README](../README.md)

会话内以 `/` 开头的已知命令由路由层直接处理、不进入 LLM（`/workflow` 例外：展开为提示词后进入 LLM；未知的 `/xxx` 原样交给模型）。所有通道与图形界面输入框通用，但影响全局的命令——`/clear`、`/log`、`/rewind`、`/export all`、`/skills` 的 `pin|unpin|adopt|restore|curate`——只对 CLI / 桌面会话或已提升为 `Unrestricted` 的会话开放，外部通道普通用户收到「权限不足」；`/clear`、`/rewind <id>`、`/fork <id>`、`/skills` 变更子命令在有 turn 或后台 review 运行时直接拒绝。`/help` 列出全部。实现位于 `crates/rclaw-runtime/src/slash.rs`、`turn.rs` 与 `turn/commands.rs`。

| 命令 | 说明 | 详细文档 |
|---|---|---|
| `/help` | 显示帮助与已知模型 | — |
| `/llmodel [模型名]` | 查看 / 热切换 LLM 模型，同步写回 `rclaw.toml`；只接受内置清单 `glm-5.3`、`glm-5.3-flash`（旧名 `glm-5.2`、`glm-5v-turbo`），其他名字被拒绝 | [模型接入](llm.md#llmodel-热切换) |
| `/search [来源]` | 显示搜索池状态，或把指定来源移到首位 | [内置工具](tools.md#联网搜索) |
| `/search dis\|en <来源>` | 禁用 / 启用 `zhipu`、`tavily`、`doubao-custom`、`doubao-global`（启用会清除额度 / 鉴权阻塞） | 同上 |
| `/searchfallback [on\|off]` | 开关搜索 fallback（关闭后只用主来源，失败不回退）；无参查看状态，也接受 `true/false`、`开/关` | 同上 |
| `/skills [子命令]` | 列出全部可发现技能入口、所属包与功能（默认 `list`）；子命令 `help` / `status [name]` / `pin <name>` / `unpin <name>` / `adopt <name>` / `restore <name>` / `curate` | [技能系统](skills.md#查看与管理) |
| `/tools` | 列出当前工具面及说明 | [内置工具](tools.md) |
| `/context` | 当前会话上下文用量：窗口 / 历史估算 / 完整请求压力 / 使用率 / 剩余 / 压缩状态 | [长程会话](context-management.md#手动命令) |
| `/compact` | 立即压缩当前会话历史（忽略阈值与防抖；有任务运行时拒绝） | 同上 |
| `/rewind [回滚点 id]` | 不带参数列出最近 20 组可回滚的文件 checkpoint；带 id 回滚该点及之后的文件写入（hash 校验，被外部改动过则整组拒绝） | [长程会话](context-management.md#文件回滚rewind) |
| `/fork [消息 id]` | 不带参数列出最近 15 条可作锚点的用户消息；带 id 从该消息分叉出新会话并把锚点之后的文件改动回滚到该时刻 | [长程会话](context-management.md#fork-会话分叉) |
| `/goal [目标文本\|clear]` | 设定 / 查看 / 清除会话级长期目标（≤ 2000 字符；`clear` / `done` / `完成` / `清除` 等价；每轮注入提醒，成功收尾后小审计确认达成即自动清除） | [长程会话](context-management.md#goal-目标提醒) |
| `/export [all]` | 导出当前会话（或全部会话）完整记录为 JSONL 到 `agent/exports/`，含已压缩进摘要的消息 | [长程会话](context-management.md#export-导出) |
| `/workflow <请求>` | 设计并运行一个动态工作流（展开为 prompt，历史保留原文） | [多智能体协作](multi-agent.md#动态工作流) |
| `/log [list\|get <名>\|del <名>\|del all confirm]` | 列出 / 读取 / 删除 `agent/logs` 日志文件；`get` 在控制台超过 20 KB 只显示尾部，在媒体通道以附件发送 | [快速开始](getting-started.md#日志) |
| `/clear [confirm]` | 清空全部会话状态（`agent/state.sqlite` 中的会话数据）、会话 artifact 目录与 `agent/memory/` 全部文件；需 `/clear confirm`（或 `yes`） | [数据目录与持久化](storage.md#clear-的语义) |
| `/cancel` | 停止当前会话正在运行的任务（路由层即时生效） | [ReAct 内核](react-core.md#插话队列与取消) |

图形界面还提供部分命令的按钮形式：消息下方「⑂ 分叉」= `/fork <id>`，对话页输入卡的「■ 停止」= `/cancel`，侧栏「导出」= `/export`。计划卡片的暂停 / 继续与 Agent 详情页的取消子代理是 GUI 独有操作，没有对应斜杠命令。
