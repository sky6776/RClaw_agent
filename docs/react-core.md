# ReAct 内核

[← 返回 README](../README.md)

一条用户消息触发一个 **turn**：加载历史 → 模型推理 → 工具批次执行 → 再推理……直到最终回复、取消或失败。主 turn 与子代理共享同一套「agent-run 内核」（上下文尾部、重试退避、工具批次、媒体认领闸门、完成闸门）；适配器只贡献完成闸门后的附加检查（子代理的团队闸门），插话处理、执行信号量、流式 / 非流式与推理错峰等是两处循环各自的结构差异。实现位于 `crates/rclaw-runtime/src/turn.rs` 及 `turn/` 子模块。术语定义见 [`CONTEXT.md`](../CONTEXT.md)。

## turn 流程

1. `AgentLoop` 只做路由：同一 `(channel, chat_id)` 同时只有一个活动 turn；定时任务再按 `job_id` 隔离；全局并发默认 8（`[agent] max_concurrent_turns`）。
2. 加载会话 active view，必要时做加载期压缩（见 [长程会话](context-management.md)），持久化用户消息。
3. 组装上下文：系统提示只含启动期稳定内容（身份、规则、工具说明与 `agent/config/SOUL.md`）；每次推理前重新读取净化后的 `USER.md`、`MEMORY.md`、最近 3 天 daily notes、技能索引、时间 / 通道 / 环境、`/goal` 目标提醒、团队未读提示与子代理状态，以一条**末尾的非持久化运行时上下文消息**（`<rclaw-runtime-context>`）注入，不写入会话历史。每次推理末尾还附一帧 `<agent-status>`（目标 / todo / 迭代与拒绝预算 / 重复指纹 / 最近 8 条回执 / 闸门结果）。
4. 模型推理 → 若有工具调用则执行批次、结果入历史、继续推理；无工具候选时须依次通过**媒体认领闸门**与**完成闸门**才结束。
5. 单 turn 最多 256 次 ReAct 迭代，达到上限按「未通过完成验证」收尾；模型单次响应工具调用数超过 `[llm] max_tool_calls`（256）则整轮响应被解析端拒绝（协议错误，不重试）。

## 工具批次

- 批次不是无条件并行：只有多个调用**全部标记 `parallel_safe`**、且本批不涉及动态工具 / 技能变更等条件时才并发，否则串行。连续的 `Agent` / `Task` / `PlanRun` 调用可并发。
- **熔断**：同一规范化调用第 3 次出现触发软熔断——本批不执行并返回改道提示；再次重复（计数 > 3）硬停。带正 `wait_timeout_ms` 的 `TaskList` 不计入。任一副作用工具成功后清掉所有只读指纹并清零计数、只保留副作用指纹，交替读写无法逃逸熔断。
- 取消时为未完成的工具补写 aborted 结果，保证历史链 tool_use / tool_result 配对完整。
- `memory` 工具的记忆整合失败（越限 / 未命中 / 漂移）在本 turn 累计超过 3 次后，再失败即返回 `done=true` 的「停止重试」终局信号提示模型不再尝试；调用本身不被禁用，后续成功操作照常执行。

## 重试

- 可重试错误：连接失败、超时、流异常、HTTP 429 / 5xx 等；默认重试 10 次（共至多 11 次），退避 1、2、4、8、16、30… 秒，单次封顶 30 秒。
- 每次迭代的重试总预算 `max(600 秒, 2 × request_timeout)`；`[llm] request_timeout_secs` 默认 300。
- 上下文超限（非可重试）另允许一次 reactive 压缩后原地重试，不消耗普通重试次数。
- 取消可中断请求与退避。

## 完成闸门

模型声称完成时，运行时核对：

- **结果声明须有副作用回执**：若回复宣称做了「外部动作」，必须有对应工具的成功回执。外部动作分六类：`create`、`write_or_update`、`delete`、`source_control`、`deploy`、`install`，按目标 / 回复措辞与工具回执匹配。普通「发送」不在其中（闸门运行时尚不能证明通道已交付），媒体由独立闸门负责。
- 未解决的副作用错误必须被修复，或由运行时脱敏后如实披露。
- 正文不得为空，不得泄露 `<agent-status>` 等运行时内部帧。
- 历史成功回执每会话最多保留 16 条，供纯汇报类回复对账。模型在最终回复中引用回执号（`r<迭代>-<序>-<指纹>`）时，闸门按 id 精确对账而非关键词匹配。

拒绝后模型可继续修正 2 次；第 3 次拒绝时，安全且非空的候选以「⚠️ 完成验证未通过」连同原因交付，空正文、质量失败或内部帧泄露只交付错误。

## 媒体认领闸门

回复中的媒体标记（声明「此处交付媒体文件」）只认发布工具的成功回执：须匹配当前通道、类型为 `file / image / video / voice`、位于共享媒体沙箱且为非空真实文件；同路径去重，并在最终发送前复验。夸大附件数量或只剩无效标记会被拒绝——有标记未验证，turn 不得宣称完成。

## 插话、队列与取消

- 真实用户插话按到达顺序在**协议安全点**注入：迭代顶部注入；工具执行前到达则废弃旧 tool_calls；最终候选发送前到达则废弃候选重算。内部消息延后重路由。
- 插话通道容量 32，满时丢弃并告警。`/cancel` 取消该会话全部活动 turn。
- 图形界面的排队消息存于前端：空闲后逐条发送；「编辑」移出队列回填输入框，「删除」移除，「立即」绕过忙碌直接投递成为当前 turn 的插话。

## 流式输出与工作状态

- SSE 正文片段先缓冲，**完成闸门通过后才外发**；控制台最终一次性输出。只有声明逐片思考流的通道（桌面 / 安卓）实时收到思考增量。
- 声明**中间输出**能力的通道（desktop、napcat）接收整轮中的中间内容：工具轮优先 text、否则 reasoning，最终轮只发 reasoning；外部完成声明、媒体声明、控制行或内部帧会被抑制。
- 声明**工作状态**能力的通道首轮收到「🟢 Agent工作中...」，工具轮收到调用进度；均为中间输出。

## 后台唤醒与心跳

- 后台子代理完成后，默认在 3 秒防抖窗内按会话合并为一条 Heartbeat 来源消息唤醒发起会话：有未读邮箱时提示用 `TaskList` 收集，否则发轻量核对提醒。
- 心跳：默认每 1800 秒（`[heartbeat] interval_sec`，实际最少 60 秒）检查 `agent/HEARTBEAT.md`，仅当存在非空、非标题、未勾选完成的行时注入固定自检提示。见 [定时任务](cron.md#心跳)。

## 运行时选项（`[agent]`）

| 键 | 默认 | 说明 |
|---|---|---|
| `max_concurrent_turns` | 8 | 可同时执行的顶层 turn 数 |
| `auto_memory` | true（向导恒写；键缺失按 false） | 每 `memory_review_nudge_interval`（10）个成功 turn 触发后台记忆 review |
| `auto_evolve` | true（向导默认勾选；键缺失按 false） | 每 `skill_review_nudge_interval`（10）个 ReAct 迭代触发技能 review；关闭时 `skill_curator` 随之关闭 |
| `auto_review_tooled` | 跟随 `auto_evolve` | 带工具 review；`auto_review_max_iters` 32、`auto_review_max_input_tokens` 600000（0 = 不限） |
| `skill_curator` | true | 静默时运行策展器；`skill_curator_interval_hours` 168，`skill_stale_after_days` 14（示例 30），`skill_archive_after_days` 30（示例 90） |
| `session_compression` 等 | 见 [长程会话](context-management.md) | 压缩阈值与尾窗 |
| `goal_audit_enabled` | true | `/goal` 完成审计 |
| `tool_result_artifact_enabled` / `tool_result_artifact_bytes` | true / 32768 | 超长工具结果落盘 |
| `verify_candidate_patches` | false | 编码类子代理（candidate_patch）完成后跑一次只读交叉验证（最多 4 次小推理、120 秒），见 [多智能体协作](multi-agent.md#交叉验证) |
| `workflow_activation` 等 | `always` | 动态工作流选项，见 [多智能体协作](multi-agent.md#动态工作流) |
| `workflow_command_confirm` | false | 工作流 `world.run` 逐命令确认（桌面弹窗，60 秒超时按拒绝） |
| `memory_char_limit` / `user_char_limit` | 2200 / 1375 | 记忆预算，见 [记忆与自进化](self-evolution.md) |

`[agent]` 拒绝未知字段，键名拼错会直接启动失败。

记忆 / 自进化相关选项的完整语义见 [记忆与自进化](self-evolution.md)。
