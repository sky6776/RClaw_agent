# 多智能体协作

[← 返回 README](../README.md)

RClaw 提供四个层次的多智能体能力：临时**子代理**（`Agent` 工具 + 团队任务表）、确定性**多步计划**（`PlanRun`）、程序化**动态工作流**（QuickJS 沙箱）、持久身份的 **bot** 与群聊房间。`Agent` / `Task` 子代理与工作流 ask 登记在执行表中，图形界面可实时查看。实现位于 `crates/rclaw-runtime/src/agent_tools.rs` 与 `agent_tools/` 子模块、`bots.rs`；bot 定义存储在 `crates/rclaw-store/src/bots.rs`，群聊编排在 `crates/rclaw-desktop/src/groups.rs`。术语见 [`CONTEXT.md`](../CONTEXT.md)，工作流工具面的设计记录见 [`workflow-tool-surface-port.md`](workflow-tool-surface-port.md)。

## `Agent`：子代理委派

- 参数：`description`、`prompt`、`split_reason` 必填；可选 `subagent_type`、`model`（填 `[[llm.providers]].name`）、`run_in_background`、`name`、`team_name`、`max_tool_iter`（默认 / 硬顶 256）、`max_output_chars`（默认 48000，范围 1000–120000）。`Task` 是同义工具。
- **拆分理由门控**：只允许因「新反馈」「独立证据」「并行外部执行」三类理由拆分，否则拒绝——防止无意义的委派。
- 子代理继承父工具集后做减法：禁止递归 `Agent` / `Task` 与 `AgentStatus` / `AgentWait` / `AgentCancel` / `AgentResume`、团队创建删除、`PlanRun`、工作流、bot 交接、cron；复杂编码任务进一步限制为读取、搜索与返回候选补丁（`candidate_patch`）。权限继承发起会话的快照（见 [安全设计](security.md#权限单向链)）。
- 运行特性：非流式推理，模型请求默认错峰 2 秒；团队成员若仍有未完成的自有任务，完成闸门拒绝收尾。
- 并发：全进程最多 32 个在途子代理。前台子代理 600 秒后自动转后台（固定值，无配置项）。
- 返回：前台返回结构化 handoff（目标、摘要、证据、产物、风险、验证与全文）；后台先返回 `agentId`，完成后投递到 team-lead 邮箱并合批唤醒发起会话（见 [ReAct 内核](react-core.md#后台唤醒与心跳)）。
- **目标式 / 契约式**是写 prompt 的两种模板而非运行时模式：目标式写「目标、完成条件、返回证据」；契约式额外规定阶段闸门、消息摘要等，用于需要精确时序的编排。

## 团队与任务表

| 工具 | 说明 |
|---|---|
| `TeamCreate` / `TeamDelete` | 创建共享任务表、成员表与邮箱；进程内只允许一个活动团队；仍有活跃成员时拒绝删除 |
| `TaskCreate` / `TaskUpdate` / `TaskList` / `TaskGet` | 任务字段：id、subject、description、activeForm、status、owner、blocks、blocked_by、metadata；状态 `queued` / `running` / `waiting` 之间可推进（含 `waiting → running`），再进入 `cancelling` 或终态 `succeeded` / `failed` / `cancelled`（终态不可逆）；`TaskUpdate status = "deleted"` 删除任务；`TaskList.wait_timeout_ms`（默认 0，上限 300000）可等待邮箱并消费消息 |
| `SendMessage` | `to`（成员名或 `*` 广播）、`message`、`wake`；点对点协调会给 team-lead 留观察副本，子成员不能强制唤醒主循环 |
| `AgentStatus` | 查单条或最近执行列表；结果附 `unresolved_disclosure`（子代理未解决的副作用工具错误） |
| `AgentWait` | 等待子代理完成，`timeout_ms` 默认 30000、上限 300000 |
| `AgentCancel` | 发出真实取消 |
| `AgentResume` | 仅恢复进程重启后处于 waiting 且无执行者的记录，沿用原权限与来源会话 |

## 执行登记与实时进度

每个 `Agent` / `Task` 子代理（前台竞速与后台）与工作流 ask 在执行表中有一份快照：状态（queued / running / waiting / cancelling / succeeded / failed / cancelled）、阶段进度与步数、起止时间、恢复标记、累计 prompt / completion / total token 与模型调用数（provider 未报 usage 时 token 只是下限）、handoff / error。`PlanRun` 的步骤子代理与交叉验证 reviewer **不进执行表**，其进度来自计划 journal 与对话页的计划卡片。启动时只装载最近 256 条终态执行，库中最多保留 1000 条终态流，超出在启动时裁掉。

- 每次状态变化形成一条执行日志；子代理的完整活动记录（思考块、正文、工具与 Skill 调用及其入参出参）另存于 `subagent/<agent_id>` transcript，与前台会话同一套数据模型。
- 快照变化、工具开始、transcript 新增都实时广播到图形界面：侧栏 **Agent** / **任务概览** 徽标与列表卡片实时显示阶段、排队 / 运行时长、token；点进详情看完整活动记录、状态时间线、handoff，可取消。
- **执行收件箱**：可向运行中的子代理发消息（最多 32 条、每条 8000 字），子代理在推理前后的安全点读取，消息同时持久化到其 transcript。
- 「清除历史」只删除终态执行及其 transcript，不动用户会话与活跃任务。

界面细节见 [图形界面](gui.md#agent)。

## `PlanRun`：依赖序多步计划

- 每计划 1–12 步；步骤字段 `id`、`task`、`role`、`depends_on`、`verified_by`、`pass_condition`；计划级 `name`、`max_parallel`（默认 4，硬顶 8）、`max_verdict_rounds`、`resume_plan_id`。
- 按依赖序分**波次**并行执行；依赖失败 / 跳过会传染为 `skipped`；步骤不重试。步骤本体是带全部门禁的子代理（工具迭代预算固定 64，任务文本 ≤ 4000 字）；计划不可嵌套（子代理不能再 `PlanRun`），同一计划禁并发。
- **对抗验证环**：`verified_by` 指定验证者，形成 Programmer / Prover 一层环——验证者输出 verdict，失败原因注回目标重跑，默认 2、硬顶 4 个失败轮次。
- **暂停 / 继续**：波次边界读取暂停标志，当前波完成后暂停，恢复后继续（图形界面计划卡片有按钮）。
- **journal 断点续跑**：逐步事件写入 `agent/state.sqlite` 的 journal 流 `plan/<plan_id>`；用 `resume_plan_id` 续跑时按步骤 id 继承已成功摘要，只重跑失败 / 跳过的步骤。
- **提问**：步骤内子代理用 `escalate` 向编排者阻塞式提问（不编造答案），默认立即挂起为 `awaiting_escalation`；主代理以 `ResolveWorkflowQuestion` 写入答案，再用 `resume_plan_id` 将答案注回重跑；未知或已答的问题 id 拒绝。问答进 journal 支持重放。

## 动态工作流

用 JavaScript 脚本在确定性沙箱里驱动子代理，适合「结果层层喂给下一步、带停止条件的循环、按结构化结果分支、单个上下文装不下的大范围审计 / 迁移 / 调研」。

- **引擎**：嵌入式 QuickJS-ng。`Date` 固定纪元、随机数固定 0.5、`performance` 固定；禁 `eval` / `Function` / `import` / `require` / 定时器；内存 64 MiB、连续 JS 片 120 秒、总墙钟默认 1 小时（硬顶 24 小时）。
- **`agent(name).ask()` / `.ask<T>()`**：脚本经 oxc 把内联 TypeScript 类型编译为 JSON Schema 并改写成结构化提问（`ask<string>` 仍走普通文本），答案自动校验；schema 深度上限 8，可选 tsc 检查。
- **journal 零调用重放**：问答按 actor、输入 hash、调用序号记入 journal；重放时命中的 ask 不调用模型并恢复原完成顺序。`AmendWorkflow` 可热修脚本——导入前驱已完成的 ask，或原地调整并发 / 模型。
- **限制**：单次执行最多 10000 个 ask、每 ask 最多 64 次工具迭代；ask 并发默认 8（硬顶 16）；同时运行的工作流默认桌面 4、安卓 2（`[agent] max_active_workflow_runs`）。run 启动时固化发起会话的权限快照；`world.run`（执行命令）仅 `Unrestricted` 会话可用，`[agent] workflow_command_confirm = true` 时逐命令弹窗确认。
- **启用**：`/workflow <请求>` 命令，或 `workflow_activation = "always"`（默认）时用户在对话中明确要求亦可、模型也可按意图主动启动并提前一句话告知；`"command"` 则只能由命令启用。两种模式都要先完整读取 `dynamic-workflows` 技能。`workflow_background = true`（默认）时 `CreateWorkflow` 立即返回后台句柄。

| 工具 | 用途 |
|---|---|
| `CreateWorkflow` | 提交脚本创建 run |
| `AmendWorkflow` | 热修运行中 / 停止的 run |
| `ResumeWorkflowRun` / `CancelWorkflowRun` | 恢复 stopped run / 取消 |
| `ListWorkflowRuns` / `GetWorkflowRun` | 列举 / 查看 run 与投影 |
| `SaveWorkflow` / `ListSavedWorkflows` | 保存 / 列出项目或全局存档（`saved` 来源豁免读取门） |
| `FillWorkflowHole` | 补分支、嵌套或尾部 hole（人工填洞） |
| `EvalWorkflowSnippet` | 在同一沙箱试跑片段，禁 agent / report / artifact / hole / phase |
| `ResolveWorkflowQuestion` | 回答 actor 的提问 |
| `WorkflowRun`（旧） | 默认不注册，仅 `workflow_legacy_tool = true` 时启用 |

运行状态投影到 `state.sqlite` 的 8 张 `workflow_*` 表（见 [数据目录与持久化](storage.md)），图形界面以 WorkflowCard 展示。

## bot 与群聊

**bot** 是有持久身份的成员智能体，区别于 turn 内的临时子代理。

- 定义文件 `agent/bots/<name>.toml`：`title`、`description`、`persona`、`provider`（固定 provider，缺省跟随主 provider）、`denied_tools`（工具面收窄）。独立记忆在 `agent/bots/<name>/`，收件箱会话为 `desktop/bot-<name>`：用户直聊与队友交接都落在这里，交接消息作为 bot 的下一轮执行，绝不打断进行中的 turn。
- **`message_agent` 交接**：发送方以工具结果同步等待目标回复（正文 ≤ 16000 字，等待超时 600 秒）；交接链由运行时随消息携带、工具参数不能伪造；深度上限 3，拒绝自交、链上环路，以及目标正在等待另一交接的情况。
- **群聊房间**：2–6 个 bot 的有界协调面板。一条用户消息触发至多 3 个串行主轮 / 10 条成员发言 / 2 次补轮；成员以 `(pass)` 弃权，整轮无实质回复即收敛；单成员轮超时 600 秒。成员只见增量（水位线，单次最多 24 行 / 12000 字）。房间日志、水位线与活动流持久化在 `agent/bots/groups/`。
- 图形界面「智能体」页可新建 / 编辑 / 删除 bot、选 provider、禁用工具、打开专属会话；建群、查看转录与活动流、@点名、发送任务、取消编排、删除群。

## 交叉验证

可选，`[agent] verify_candidate_patches`（默认 false）。`candidate_patch` 类编码子代理完成后，启动独立只读 reviewer（只能 `read` / `list` / `grep`，最多 4 轮、每轮 120 秒）核对路径、代码与验证声明，输出 VERIFIED / ISSUES 附在 handoff 之后；失败、超时或未收敛不阻塞原交付。
