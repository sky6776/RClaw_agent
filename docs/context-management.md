# 长程会话与上下文管理

[← 返回 README](../README.md)

长任务会撑爆上下文窗口、写坏文件、跑偏目标。RClaw 用一组互相配合的机制处理这些问题：三层压缩、工具结果落盘、文件 checkpoint 回滚、会话分叉、目标提醒、完整导出与跨会话检索。实现位于 `crates/rclaw-runtime/src/turn/{compression,history,in_turn,artifact,rewind}.rs`、`turn.rs`（`/compact` `/rewind` `/fork` `/goal` `/export` 的处理函数）、`session_handoff.rs` 与 `crates/rclaw-store/src/session.rs`。这组能力的参照系是 ZCode，逐轮对照记录见 [`zcode-port-longhorizon.md`](zcode-port-longhorizon.md)。

## 三层压缩

### 配置（`[agent]`）

| 键 | 默认 | 说明 |
|---|---|---|
| `session_compression` | true | 关闭后只按内置硬窗口（256 条 / 512 KiB）裁剪，不做语义摘要 |
| `compression_protect_first` | 3 | 首次压缩时原样保护会话开头消息数（已有摘要后视为 0） |
| `compression_keep_recent` | 80 | 压缩后保留的最近消息条数基线（尾窗） |
| `compression_threshold_tokens` | 0（自动） | 有效窗口 = `context_length − max_tokens`；阈值取窗口 50%、最低 64000；小窗口改为 85% |
| `compression_tail_token_budget` | 0（自动） | 尾窗预算 = 阈值 20% |
| `compression_tail_min_floor` | 8 | 尾窗至少保留条数 |
| `compression_summary_max_tokens` | 12000 | 摘要输出上限（实际取 `max(2000, min(被压内容 20%, context_length × 5%, 本值))` 再 ×1.3，且不超过 provider 的 `max_tokens`） |

尾窗规则：最新真实 user 消息与最新 assistant 消息必须留在尾窗；边界落在 tool_result 上时后移，避免孤立工具结果。未声明 `context_length` 时按 128K 推导。token 估算会用 provider 返回的真实 usage（`usage_anchor` 表）做锚定校准，而非纯字符估算。

### 加载期压缩

每个新 turn 加载历史前读取完整 active view，估算「完整请求压力」（system prompt + 工具 schema + 历史）。达到阈值时先做便宜剪枝——剥离旧媒体、去重 / 摘要化保护窗外大于 200 字符的工具结果、裁剪大于 500 字节的工具参数——再复查阈值；仍超限才把中段交给 LLM 摘要，保留开头与近期尾窗，并追加一条非破坏性的 **compaction boundary**。完整 transcript 不删除；摘要同步写入 `memory/session_summaries/`。

### 轮内三层

- **microcompact**：每次模型请求前执行，只改变请求副本，不改内存与磁盘。
- **auto compact**：每次 ReAct 迭代检查阈值，只压本轮开始前的头区，本轮用户输入及后续消息原样保留；压缩后估算未回升 5% 时 rapid-refill breaker 阻止重复压缩。
- **reactive compact**：收到非可重试的上下文超限错误后忽略阈值强制压缩并原地重试；每次迭代最多一次；429 等可重试限流不触发。

其他保护：压缩节省不足 10% 视为低效，连续 2 次且历史水位未增长时暂停；失败冷却 600 秒。

### 手动命令

- `/compact`：忽略阈值与防抖立即压缩；同会话有活动任务时拒绝；可被 `/cancel` 中止；至少保留 8 条近期消息；落库失败或摘要失败时原历史不变。`/rewind <id>`、`/fork <id>`、`/clear` 同样在有 turn 或后台 review 运行时直接拒绝（不排队等待）。
- `/context`：显示窗口、历史估算、完整请求压力、使用率、剩余量与压缩状态。

## 工具结果落盘

`tool_result_artifact_enabled = true`、`tool_result_artifact_bytes = 32768`：严格超过阈值且已脱敏的工具结果写入 `agent/artifacts/<会话键>/<纳秒id>-<工具>.txt`，模型只收到 4096 字节的头尾预览、绝对路径与 `read offset/limit` 取中段的指引。仅超长行按约 2048 字节折行；读取 artifact 本身不会再次落盘；写入失败退回原结果。会话删除 / `/clear` 时级联清理。

## 文件回滚（`/rewind`）

- `write` / `edit` / `delete` 成功时记录 before 内容与 before / after SHA-256，按工具迭代分组，与工具链同一事务提交。仅覆盖 UTF-8 普通文本且 ≤ 2 MiB；不覆盖无变化写入、符号链接、目录、shell / 技能 / 子代理的副作用，以及沙箱外首次获批的写入。
- `/rewind` 不带参数列出最近 20 组回滚点（每组 = 一次工具迭代，附时间）；`/rewind <id>` 或工具 `rewind_files({rewind_id})` 精确回滚该组及其后所有 checkpoint 写入。
- 恢复前校验：当前文件 hash 等于最新 after_hash，且同文件 checkpoint 链首尾相接；任一失败**整组拒绝**（被外部改动过的文件不会被覆盖）。应用中途失败逆序撤回已应用文件。会话历史不回滚。

## `/fork` 会话分叉

- `/fork` 不带参数列出最近 15 条真实用户消息作为可选锚点；锚点必须仍在 active view（已被压缩折叠的消息不可选）。
- `/fork <消息 id>` 复制锚点及之前的可见消息、工具调用元数据与 artifact 副本到新会话；boundary 摘要降为普通消息；不复制后续消息、checkpoint、goal 与 usage anchor；源会话不变。
- 分叉自动尝试把源工作区中锚点之后的 checkpoint 写入回滚到该时刻；失败不撤销已创建的分支。
- 全局有活动 turn / review 时拒绝；bot 会话拒绝。图形界面入口：消息下方「⑂ 分叉」按钮（移动端长按菜单）。

## `/goal` 目标提醒

- `/goal <文本>` 设定会话级长期目标（上限 2000 字符），存于 `kv` 键 `session-goal/<channel>/<chat_id>`；设定成功后同一条消息展开为「立即开工」的模型指令继续走 turn 路由，不需要用户再补发一条消息（历史 / UI 保留 `/goal <文本>` 原文）。
- 此后每次推理以非持久化尾部元消息注入目标、证据核对与持续工作提醒（按证据核对、不自行宣告完成）。
- `/goal` 查看；`/goal clear`（或 `done` / `完成` / `清除`，ASCII 别名大小写不敏感）清除。`goal_audit_enabled = true` 时，成功用户回合后还会运行一次最多 200 token 的无工具完成审计，确认达成即自动清除并回执。

## `/export` 导出

`/export` 导出当前会话，`/export all` 导出全部会话（含归档），目录 `agent/exports/`，每会话一个带时间戳的 JSONL：每行是完整 `Message` 与 id，boundary 另含 `meta.compaction`。被压缩进摘要的消息、工具结果载体都保留——压缩只影响模型可见视图，不删除历史。

## 跨会话检索与交接

| 工具 | 参数 | 语义 |
|---|---|---|
| `session_search` | `query`, `limit=3`（1–10） | 空格分词、AND 匹配、不区分大小写；按总词频降序、时间降序；候选最多 4000 行、每会话最多 32 条；摘要围绕首命中前后各 160 字符。覆盖归档与已压缩掉的原消息，排除摘要 boundary 与工具结果 |
| `session_handoff` | `channel?`, `chat_id`, `query`, `max_chars=24000`（4000–48000） | 把另一段会话的脉络带进当前会话：保留目标会话最近 8 条，并从更早消息按「命中不同 token 数 → 新近度」最多再选 40 条；超过 2 字的中文连续串补二字 bigram；单条 200–4000 字符；源会话超过 50000 条拒绝。结果标注为历史素材而非指令 |

## 会话存储模型

- 所有会话位于 `agent/state.sqlite`，以 `(channel, chat_id)` 隔离。
- **active view**（模型可见）= 受保护头部 + 最新 boundary 摘要 + 尾窗 + 后续消息；**transcript** 保留全部消息与所有 boundary。
- 归档只设置 `archived_at`，不移动或删除消息，因此搜索、交接、导出仍可访问。
- 表结构与 `/clear` 语义见 [数据目录与持久化](storage.md)。
