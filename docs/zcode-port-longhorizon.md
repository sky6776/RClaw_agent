# ZCode 长程能力移植：五层体系 → RClaw（大规模并行任务定位）

> 参照系：`zai-org/ZCode`（v3.15.1，Apache-2.0）的 long-horizon 机制。
> RClaw 的定位是**大规模并行任务**——大量子代理并行 + 长 ReAct turn，因此移植
> 优先级按「哪一层先让并行编排不塌」排序，而非照抄 ZCode 的实现顺序。

## 移植总览

| ZCode 机制 | RClaw 移植 | 状态 |
|---|---|---|
| Layer 1a microcompact（`core/compact/microcompact.ts`） | `turn/in_turn.rs::microcompact_view`（每次请求视图本地剪枝，不落盘） | ✅ 第一轮 |
| Layer 1b auto compact（每迭代阈值检查） | `turn/in_turn.rs::maybe_auto_compress`（anchored 估算 + 头区摘要 + session rewrite） | ✅ 第一轮 |
| Layer 1c reactive compact（context-exceeded 原地重试） | `turn/in_turn.rs::reactive_compress` + `error_indicates_context_exceeded` | ✅ 第一轮 |
| Layer 1d manual `/compact` | `/compact` 斜杠命令 → `TurnManager::handle_compact`（忽略阈值/防抖，活动 turn 时拒绝） | ✅ 第二轮 |
| Layer 1e rapid-refill breaker | `HistoryLoader` 地板估算：压缩后未回升 5% 不再触发轮内压缩（`in_turn_blocked_by_rapid_refill`） | ✅ 第二轮补齐 |
| Layer 2 子代理即上下文隔离 | RClaw 已有（共享 Run 内核 + candidate_patch 隔离 + 并发 Agent 调用） | 已具备 |
| Layer 2a 前台自动转后台（`autoBackgroundMs` + `taskAbort.detachParent`） | Agent 工具竞速 + `launch_background_task_impl` 联动：预算内完成回传完整结果，到点 detach（父取消不再波及）返回 backgrounded | ✅ 第二轮 |
| Layer 2b 子代理独立模型/推理级别（`.zcode/agents` frontmatter） | Agent `model` 入参 → `spec.model` → `ModelFactory` 按 `[[llm.providers]]` 名解析独立 provider | ✅ 第二轮 |
| Layer 3 动态工作流（journal 重放 + amend） | `PlanRun resume_plan_id`（journal 断点续跑 + 按 id 继承已完成步骤） | ✅ 第一轮简化版 |
| Layer 3 escalate 提问通道（`escalate` / `ResolveWorkflowQuestion`） | 计划步骤阻塞式提问 + 编排者应答，问答进 `agent/plans/<plan_id>.jsonl`（`step_escalated` / `escalation_resolved`）支持重放 | ✅ 第三轮最小内核 |
| Layer 3 Programmer/Prover 对抗环（`while (!verdict.pass)`） | 步骤声明 `verified_by`/`pass_condition`：验证步骤返回 JSON verdict，失败摘要注入目标重跑，轮次预算（`max_verdict_rounds`，默认 2）经 journal 跨 resume 持久 | ✅ 第四轮原语 |
| Layer 3 引擎最小执行器（rquickjs 确定性沙箱） | `WorkflowRun` 工具：JS 脚本经 `agent(name).ask(question)`/`result()` facade 驱动子代理；`Date.now`/`Math.random` 钉死并冻结 `Date`/`Math`、无模块加载器（import 永不 resolve）；ask 问答带 FNV-1a inputHash 增量写 `agent/workflows/<run_id>.jsonl`；resume 按 (actor, inputHash) 零调用重放，script hash 拒改 | ✅ 第六轮 |
| Layer 3 引擎 async 化（ask 返回 promise + Promise.all 并行扇出 + set-order） | rquickjs `futures` 特性：脚本包进 async IIFE（顶层 await/Promise.all），ask 经 Async 宿主返回 promise，信号量 `max_parallel_asks`（默认 4/硬顶 8）限流；journal 记发起序 seq + 完成序 answer，重放按 (actor, inputHash) FIFO 结果确定 | ✅ 第七轮 |
| Layer 3 facade 类型/facade 检查（诊断拒绝提交） | `check_script`：QuickJS AsyncFunction 语法探针（只解析不执行，与 async IIFE 包装语义一致）+ 禁用模式静态扫描（Date/eval/new Function/require/import/setTimeout/process，真实 L:C）；带诊断的脚本拒绝提交，诊断只回模型 | ✅ 第七轮（语法+facade lint；tsc 级类型推断因 swc/serde 生态冲突记为后续） |
| Layer 3 WorkflowRun Amend 热修 | `amend_run_id`：修订脚本按 actor 名 + inputHash 导入前驱 journal 已完成 ask（同问文本零调用继承；改写/新增照常执行），新 lineage journal 记 `amended_from`/`imported_asks`；与 resume_run_id（逐字节一致续跑）互斥 | ✅ 第八轮 |
| Layer 3 类型级检查可选增强（node/tsc 工具链） | `check_script(script, type_check)`：node/tsc 探测（进程级缓存）→ spawn `tsc --noEmit --strict` 对脚本+facade.d.ts → 解析 `path(line,col): error TSxxxx` 诊断（只取 script.ts）；node/tsc 缺失或超时静默降级为语法+lint（两级共用 FacadeDiagnostic）。工具入参 `type_check`（默认 true），阻塞调用放 spawn_blocking | ✅ 第九轮 |
| Layer 3 askJson（泛型 T→JSON Schema 答案校验） | facade `askJson(question, schema)`：答案 JSON 解析 + Rust 侧子集校验器（type/properties/required/items 递归，深度上限 8）；校验失败按错误抛回脚本 | ✅ 第九轮 |
| Layer 3 静态洞察（依赖图近似） | `analyze_workflow_script` 文本级分析（actor 清单/ask 数/并行标记），入 `run_started` 事件与执行报告 `insight` 字段；AST 级依赖图为后续 | ✅ 第九轮（文本级近似） |
| Layer 3 依赖图静态分析（DAG） | `analyze_dependency_graph`：语句级解析 → 节点（ask 调用点，actor/行号/并行标记）+ 边（前波→后波）+ Kahn 拓扑波次；串行链成链、Promise.all 成波（内部无边）；入 run_started 与报告 | ✅ 第十一轮（语句级文本解析，非完整 AST） |
| Layer 3 字面 set-order 调度重放（race 保真） | ReplayAnswer 携带全局完成序 order（journal answer 事件序）；JS prelude 链式顺序门（`__gate_wait`/`__gate_pump` 逐级微任务泵送）：重放命中按原完成序延迟 resolve，Promise.race 赢家与解析顺序与原完成序一致 | ✅ 第十轮 |
| Layer 4 后台通知合批 + 唤醒（`runtime-task/notification.ts`） | `background_wake.rs::BackgroundWaker`（防抖合批 + Heartbeat 唤醒消息） | ✅ 第一轮 |
| Layer 5 主 turn 检查点 | durable usage anchor（`agent/sessions/checkpoints/`，跨重启恢复锚定估算精度）+ ADR-0004 aborted chain（已有） | ✅ 第二轮 |
| Layer 5 跨会话交接（`ReadSessionContext`） | `session_handoff` 工具（query 相关选取 + 尾部原文 + 24K 预算防注入头部） | ✅ 第二轮 |
| Layer 5 按 messageId 选点回滚（rewind + hash 校验） | `turn/rewind.rs`：write/edit/delete 前后 checkpoint（before/after hash，按持久 messageId 分组）+ `rewind_files` 工具与 `/rewind` 命令；恢复前 hash 校验，外部改动整组拒绝（all-or-nothing） | ✅ 第五轮 |

## Layer 1：轮内上下文管理（本次移植的核心）

**移植前的问题**：压缩只发生在轮次开始（`HistoryLoader::load`）。一个长 ReAct turn
（`max_tool_iter` 默认 256 次迭代）内部完全没有压缩——大规模并行时父 turn 汇总 N 个
子代理结果，巨大的工具输出一路堆积，单 turn 就可能撑爆窗口，且中途没有任何自愈手段。

**三层机制**（全部复用 RClaw 既有压缩基础设施，见 `turn/in_turn.rs`）：

1. **microcompact**（`turn.rs` 重试循环内，每次请求）：请求视图 = `microcompact_view(&messages)`
   ——对【视图】做 load 期同一套 prune（旧媒体剥离 / 重复与陈旧 tool result 摘要化 /
   大 tool_use input 裁剪），保护窗口（`compression_prune_protect_count` 条 +
   `compression_tail_token_budget` token）内最近消息原样保留。**只改视图不改内存与磁盘**，
   与 load 期预剪枝语义一致；下一轮 load 仍取回完整内容。
2. **auto compact**（ReAct 循环顶部，每迭代一次）：`anchored_estimate`（真实 usage 锚点 +
   增量启发式，与 load 期同一套防抖/冷却状态）≥ `compression_threshold_tokens` 时，
   对头区 `[..turn_start]` 做预剪枝 → 摘要 → `session.rewrite`。**当前 turn 的尾区
   `[turn_start..]` 永远保留**，原始任务消息由尾窗锚点（最新真实 user/assistant 留在尾窗）
   保护不被摘要化——对齐 ZCode「强制保留最后一轮原文」的任务保真设计。
3. **reactive compact**（LLM 错误分支）：`error_indicates_context_exceeded`（中英文文案
   特征，覆盖 `StreamAborted` 与 `BadStatus 400` 两种到达形态）命中时忽略阈值强制压缩
   一次，成功后 `continue 'retry` 原地重试。每迭代至多一次，防止「压缩-超限」循环。

**关键设计决策**：

- **头区/尾区边界**：`turn_start` = 本轮 user 消息下标。头区与磁盘一致，可安全 rewrite；
  本轮的 `skill_context` / 媒体附件消息是【不落盘】临时上下文，以 `ephemeral` 下标跟踪：
  内存保留（模型还要参考）、磁盘重写时剔除（维持既有持久化行为）。
- **不再 acquire `execution_limit`**：load 期压缩在持槽前，轮内压缩在持槽后——若再拿一枚
  槽位，`max_concurrent_turns` 个 turn 同时压缩会互相等待死锁。
- **rewrite 失败不回滚内存**：内存已切换为压缩视图，本轮继续；磁盘保留原历史（下次 load
  全量重建），与「摘要失败不 rewrite」的 fallback 语义一致。
- 开关：`AgentOptions::in_turn_compression_enabled`（默认 true；关闭退回旧行为）。

## Layer 4：后台完成通知合批 + 唤醒（`background_wake.rs`）

**移植前的问题**：后台子代理完成只入团队邮箱 + 尾部 `[System hint]`（不唤醒）。父 turn
先于子任务收尾时（大规模并行的常态），没有任何机制驱动收集轮次。

**机制**：`BackgroundWaker::notify(channel, chat_id)` 在每个后台任务终态落盘后调用
（`run_background_execution` 收尾处）：

- **合批**：防抖窗口（`background_wake_debounce`，默认 3s）内同会话的 N 个完成去重合并，
  只投一条消息——不按完成数线性放大请求数（对齐 ZCode notification batch 的动机）。
- **唤醒消息**：Heartbeat 来源（正常加载/持久化历史，不触发 cron 提示），投给发起任务的
  会话（`_rclaw_source_chat` 运行时注入，与 `_rclaw_source_channel` 同范式）。若有活动
  turn，按既有语义延后为 turn 结束后的下一轮——不打断进行中的工作。
- **fire 时未读为 0**（父已在活动 turn 用 TaskList 消费）→ 降级为轻量核对提醒，不驱动
  专门的收集轮次。
- 装配：`AgentToolRuntime::background_wake()` 句柄，二进制建好 inbound 总线后
  `attach(inbound_tx.clone())`（main.rs，与 bot_router_slot 回填同模式）。
- 开关：`background_wake_enabled`（默认 true）。尾部提示文案已同步更新（不再声称
  「团队消息不会唤醒主循环」）。

## Layer 3：PlanRun journal 断点续跑 + amend（`agent_tools/plan.rs`）

**移植前的问题**：PlanRun 是一次性工件——执行器崩溃 / 上限中断后无法恢复，失败不重试，
只能整计划重跑。

**机制**：`resume_plan_id` 输入参数（事件日志路径在每次 PlanRun 结果里都有）：

- `load_plan_journal` 解析 `agent/plans/<id>.jsonl`，取每步【最后一次】`step_finished`。
- **续跑**：journal 里 succeeded 的步骤按 id 继承结果摘要、不再重跑；failed/skipped 回到
  Pending 走既有执行循环。
- **amend**：步骤按 id 匹配——模型可以修改失败步骤的 task 文本后重提交，成功步骤的
  缓存不失效（对齐 ZCode AmendWorkflow「按 actor 名字导入已完成工作」）。
- journal 追加 `plan_resumed` 事件（inherited 数量）；执行报告与处置提示都会带
  `resume_plan_id="<plan_id>"` 指引，模型从失败报告里直接学会如何恢复。
- 日志不存在 / 无 step_finished 记录 → 显式报错（错误重定向比静默从零重跑安全）。

**与 ZCode 动态工作流的差距（有意保留）**：ZCode 的 dWF 是完整程序化引擎（TS 脚本 +
类型检查 + vm 沙箱 + inputHash 重放 + set-order 重放）。RClaw 的 PlanRun 是「JSON 步骤表
+ 拓扑波次」的最小工件，本次只补了 journal 续跑与 amend。完整的程序化编排（while-until-
pass 循环、Promise.all 扇出、escalate 提问）是后续独立立项的量级，见「后续工作」。

## Layer 2 / Layer 5 现状

- **Layer 2（子代理即上下文隔离）**：RClaw 已具备核心能力——子代理独立对话上下文、
  前台并行（`is_parallel_agent_tool`）、后台执行 + 快照 resume、candidate_patch 隔离、
  推理错峰。本次未改动。与 ZCode 的差距是「前台任务中途自动转后台」与「子代理可配
  独立模型」，属于增量优化。
- **Layer 5（跨会话连续性）**：RClaw 有 session JSONL 恢复 + 子代理执行快照 resume +
  `session_search`，未做主 turn 检查点 / 跨会话交接工具 / 文件状态追踪。**未动**，
  见后续工作。

## 第六轮移植：Layer 3 引擎最小执行器（rquickjs 确定性沙箱）

嵌入 **rquickjs 0.9**（QuickJS-ng，MSVC 侧 C 源码捆绑构建）实现引擎骨架：

- **确定性沙箱**（`eval_deterministic`）：prelude 固定 `Date.now`/`Math.random`
  并 `Object.freeze(Date/Math)`（篡改赋值被 QuickJS 抛 TypeError 或静默忽略，钉死值
  始终生效）；不注册模块加载器 → 动态 `import()` 返回永不 resolve 的 promise，模块
  内容不可得；内存上限 64MB + 120s 中断处理器。异常经 `ctx.catch()` 提取真实消息。
- **facade**：`agent(name) → {ask(question)}` 与 `result(text)`（prelude 包装宿主
  函数 `__ask_raw`/`__result`，错误在 JS 侧抛出）。
- **journal**（`agent/workflows/<run_id>.jsonl`，增量落盘）：`run_started
  {script_hash}` → `ask {seq, actor, question, input_hash}` → `answer {seq,
  input_hash, answer}` → `run_completed`。inputHash = FNV-1a64(actor question)。
- **断点重放 = 重跑脚本**：`resume_run_id` 时从 journal 重建 (actor, inputHash) →
  答案表；重跑时命中即直接结算（provider 零调用），脚本 FNV hash 不一致则拒绝
  （v1 要求逐字节一致，amend 后续迭代）。
- **桥接**：QuickJS 是同步执行器——引擎放 `spawn_blocking`，ask 经
  `Handle::block_on` 驱动 `run_task` 子代理（v1 串行扇出；`Promise.all` 并行需
  async 运行时绑定，后续迭代）。

## 第八轮移植：WorkflowRun Amend 热修

`amend_run_id`（与 `resume_run_id` 互斥）：修订脚本提交时从**前驱 journal** 按
`(actor, input_hash)` 导入已完成 ask 的答案做缓存——

- 问题文本未变 → (actor, hash) 命中 → 零 provider 调用继承答案；
- 问题文本改写 / 新增 → hash 不命中 → 照常执行子代理；
- **脚本可以任意修改**（这正是 amend 与 resume 的分界：resume 要求逐字节一致）；
- amend 产生**新 lineage** 的 run_id（`<name>-amend-<ts>`），新 journal 独立，
  `run_started` 事件记 `amended_from` + `imported_asks`，前驱 journal 不改写；
- 结果报告带 `amended_from` / `imported_asks` / `asks_replayed`。

纯 Rust、零新依赖：复用第六轮的 `load_workflow_journal`（ask→answer 配对）与
EngineShared 重放表机制。至此 ZCode amend 的核心语义（按子代理名字导入前驱
已完成工作、只重跑没完成的部分）在 WorkflowRun 上闭环。

## 第九轮移植：类型级检查可选增强 + askJson + 静态洞察

**tsc 可选增强落地**（第八轮评估结论的实现）：`check_script(script, type_check)` 三段——
① QuickJS AsyncFunction 语法探针；② 禁用模式静态扫描（真实 L:C）；③ `type_check=true`
（默认）且宿主 node/tsc 在位时 spawn `tsc --noEmit --strict --pretty false` 对脚本 +
facade.d.ts（facade.d.ts 已升级为可编译声明：`agent()/askJson()/result()` 全签名），
解析 `path(line,col): error TSxxxx` 诊断（只取 script.ts，facade 自身报错不归咎脚本）。
node/tsc 缺失或 30s 超时 → 静默降级（两级共用 FacadeDiagnostic）；探测结果进程级缓存；
tsc 为秒级阻塞调用 → 放 `spawn_blocking`。

**askJson（泛型 T→JSON Schema 答案校验）**：facade `askJson(question, schema)`——
答案 JSON 解析 + Rust 侧递归校验器（`type` 五原语 + `properties`/`required`/`items`，
深度上限 8），校验失败按错误抛回脚本（不静默放行）。schema 从脚本内联传入（v1 不做
T→schema 编译，由模型直接给出 schema 字面量——语义等价、实现零依赖）。

**静态洞察（依赖图近似）**：`analyze_workflow_script` 文本级分析（actor 清单 /
ask 计数 / Promise.all 并行标记），入 `run_started` 事件与执行报告 `insight` 字段。

**set-order 完成序基础设施**：journal 的 answer 事件 seq 即完成序（发起序在 ask 事件），
`load_workflow_journal` 已按配对重建重放表；字面完成顺序调度重放对 Promise.race 类
脚本的保真度差异已记录（沙箱无世界交互 ⇒ 结果确定）。

## 第十一轮移植：依赖图静态分析（DAG）

`analyze_dependency_graph(script) → DependencyGraph { nodes, edges, waves }`——
把第九轮的文本计数升级为真实 DAG 结构（维持 rquickjs 决策后不引 swc，用语句级
文本解析实现）：

- **节点**：`<handle>.ask(...)` 调用点（id = `ask-<行号>`、actor、行号、并行标记）；
  `const <handle> = agent("<actor>")` 解析为句柄绑定；
- **边**：顺序语句之间「前波出口 → 后波入口」；Promise.all 行内节点标记并行
  （内部无边）；
- **波次**：Kahn 拓扑分层（入度 0 → 层 1，逐层推进）——同层节点可并行。

串联用例：串行链三层波；Promise.all 两节点同波无边；波后接串行 ask → 波末节点
→ 后续节点有边。产出经 `WorkflowInsight.dependency_graph` 序列化进 `run_started`
事件与执行报告，供调度参照与可观测性。

## 第十轮移植：字面 set-order 调度重放（race 保真）

重放命中的 ask 不再立即结算：envelope 携带该 ask 在原完成顺序中的全局位次
（`order` = journal answer 事件序，0 起），JS prelude 以**链式顺序门**延迟 resolve：

- `__gate_wait(N)`：N == 游标 → 立即放行、游标推进、微任务泵释放 N+1；
  N > 游标 → 挂起等待；N < 游标（迟到的旧序）→ 直接放行不阻塞。
- `__gate_pump` 每次只释放一级（order N+1），并由被释放 promise 的下游链天然触发
  下一级——保证 order N 的下游回调（`.then(push)`/`await` 续体）总是先于
  order N+1 的解析入队。

**端到端证据**（`workflow_race_replay_preserves_completion_order`）：SlowFastProvider
对「慢问题」延迟 150ms、「快问题」立即回答——原 run 的 Promise.race 赢家为快、解析序
`["fast","race:…"]`；resume 后两个 ask 零调用重放，**顺序门使解析序仍为 fast 先**
（无门时重放按脚本微任务序 slow 先解析、race 赢家反转——测试曾以此失败形态验证
门的有效性）。断言口径：重放解析序以 fast 开头（原 run 的慢 push 发生在 result
之后，字面数组相等的不可达性已在测试注释说明）。

调试记录：初版 order 按 (actor, hash) 独立计数——两个 ask 的 order 同为 0，门形同
虚设；改为 journal answer 事件的**全局序**后门生效。级联释放改为逐级微任务泵送
（同步级联会让 order 1 的解析反超 order 0 的下游）。

## 脚本语言选型决策：维持 rquickjs（已定）

候选对比（rhai / mlua / Python PyO3）与结论：嵌入成本（rhai 纯 Rust 最低）、并行扇出
表达（JS 原生 async/await + Promise.all；rhai 无 async 需批量 API 改写；mlua 靠
coroutine；Python GIL 不友好）、模型写编排脚本的可靠性（JS 最优）、现有投入
（rquickjs 引擎已闭环，317 测试全绿）。**决策：维持 rquickjs**；rhai 仅在将来
Termux 交叉编译 QuickJS 成为现实问题时作为可切换后端重开（facade 语义不变，各自
实现宿主桥）。

## facade .d.ts 类型级检查：替代工具链评估（第八轮结论）

**背景**：进程内 TS 类型检查经 swc 路线已验证不可行（swc_common 0.37–0.40 依赖
`serde::__private`，被 serde ≥1.0.220 移除；本工程 serde_json 被 tauri 链钉死
≥1.0.150 抬高 serde 下限，无法降级——两轮探针实测）。

**替代方案评估：独立 Node 工具链（tsc / esbuild + tsc）**：
- 做法：`WorkflowRun` 检查阶段把脚本 + `FACADE_DTS` 写入临时目录，spawn
  `npx tsc --noEmit --strict script.ts facade.d.ts`（或先用 esbuild 快速转译再
  tsc 检查），解析 tsc 的 JSON 诊断输出映射为 FacadeDiagnostic（L:C + 消息）。
- 收益：真正的类型级检查（`.ask<T>` 泛型、schema 从 T 合成的前提）；诊断格式
  成熟。
- 前提与风险：**宿主必须有 Node/npm**——RClaw 的部署面含 Windows/Termux（Android），
  Termux 侧 Node 可用但 npx 首次下载 tsc 需网络且慢；tsc 冷启动秒级延迟会加在每次
  提交上；版本漂移（tsc 输出格式）需要钉版本。
- 结论：**可行但引入外部运行时依赖**，与 RClaw「Termux 单进程自包含」的部署形态
  冲突。建议：作为**可选增强**实现——检测到宿主 `node` 可用时自动启用类型级检查，
  不可用时静默降级为第七轮的语法+lint 检查（两级检查共用 FacadeDiagnostic 格式）。
  该增强工作量约 1 轮迭代（spawn + JSON 解析 + 可用性探测 + 降级开关），待
  Termux 部署面验证 Node 可用性后启动。

## 第七轮移植：引擎 async 化 + facade 检查

**ask 返回 promise / Promise.all 并行扇出**（rquickjs `futures` 特性）：脚本包进
async IIFE，`ask` 经 rquickjs `Async` 宿主包装返回 promise——`Promise.all` 的 N 个
ask 由信号量 `max_parallel_asks`（默认 4，硬顶 8）限流并发驱动 `run_task`。引擎
future 非 Send（QuickJS 裸指针），整段放 `spawn_blocking` + 内嵌 current_thread
runtime 驱动（JoinHandle 结果 Send）。**set-order 语义**：journal 的 `ask` 事件记录
发起序（seq 单调）、`answer` 按完成序追加；重放按 (actor, inputHash) FIFO 结算——
沙箱无世界交互，完成顺序对脚本不可观测，按 inputHash 结算即结果确定（answer 事件
仍完整保留完成顺序供观测/审计）。

**facade 检查**（`check_script`，提交期拒绝、诊断只回模型）：
1. **语法探针**：QuickJS `AsyncFunction` 构造器只解析不执行（与 async IIFE 包装
   语义一致——普通 `Function` 探针会把 `await` 误报为语法错误）；
2. **facade 静态扫描**：禁用模式（`Date.now`/`new Date`/`Math.random =`/`eval(`/
   `new Function`/`require(`/`import(`/`setTimeout`/`process.`）带真实行/列诊断；
   `FACADE_DTS` 常量随诊断回传 facade 契约。
工程决策记录：曾引入 swc_core 做 TS 解析，但 swc_common 0.37–0.40 依赖的
`serde::__private` 已被 serde ≥1.0.220 移除，而本工程 serde_json 被 tauri 链钉死
≥1.0.150（抬高 serde 下限）无法降级——swc 不可用，改用 QuickJS 语法探针 + lint，
代价是不做 TS 类型推断（完整 .d.ts 类型检查依赖独立 JS 工具链，后续迭代）。

## 第五轮移植：Layer 5 选点回滚（按 messageId 精确 rewind）

**checkpoint 采集**：前台路径的 write/edit/delete 在执行前后捕获文件快照
（`turn/rewind.rs::capture_before/finish_capture`；路径语义与文件工具一致，经
`normalize_agent_tool_path` 解析；内容超 2MB 不捕获——无法恢复的写入不做半吊子记录，
no-op 写入不入账）。turn 循环为每次 assistant 消息预分配**纳秒级单调 messageId**
（`session.append` 保留非 0 ts，故该 id 原样落进会话 JSONL，就是可引用的消息身份），
工具链持久化后 checkpoint 以它分组落盘到 `agent/sessions/rewind/<key>.jsonl`。

**选点恢复**：`/rewind [message_id]` 斜杠命令与 `rewind_files` 工具（经 CURRENT_CHAT
task-local 定位会话）。语义 = **回滚消息 M 及其后全部 checkpointed 写入**（最新优先，
恢复到 M 之前的文件状态）；会话历史不动。安全：每个文件先与它【最新】checkpoint 的
after_hash 比对（含回滚自身落下的 synthetic 重锚标记——连环回滚不会被上一次回滚误判
为外部改动），任何不符即外部改动 → 整组拒绝（all-or-nothing）。恢复前有活动 turn 则
拒绝（防与会话原子追加交错）。

已知边界：只覆盖结构化文件工具；shell/skill 脚本副作用与子代理（Subagent 策略）写入
不在 checkpoint 覆盖范围。

## 第三轮移植：Layer 3 最小内核——escalate 提问通道

ZCode 的 `escalate` 让子代理被卡住时「阻塞式向主代理提问而不是编造答案」。RClaw 的
PlanRun 前台运行期间编排者（父 turn）被工具调用阻塞、无法实时应答，因此 v1 采用
**journal 协议**（问答进 `agent/plans/<plan_id>.jsonl`，天然支持重放）：

1. **`escalate`**（子代理面，靠 `CURRENT_PLAN` task-local——`run_task` 按
   `spec.plan_id/step_id` 注入作用域——定位计划）：追加 `step_escalated`
   `{question_id, step, question, context}` 事件并登记待应答问题。
   `plan_escalation_wait_secs`（默认 0）> 0 时经 in-memory oneshot 通道阻塞等待实时
   答案（为将来 PlanRun 后台化预留真·阻塞语义），默认提交即挂起——步骤被明确指示
   「不要编造答案，立即以结构化 handoff 收尾」。
2. **`ResolveWorkflowQuestion`**（主代理面，对子代理屏蔽——`is_subagent_blocked_tool`）：
   `{question_id, answer}` → 实时解锁等待中的步骤（若在窗口内），并追加
   `escalation_resolved` 事件（journal 单点记录，重放唯一事实源）。
3. **PlanRun 重放**：`load_plan_journal` 现在同时解析升级问答；`apply_escalation_seeding`
   把已解答的升级步骤强制回 Pending 并把答案注入任务文本
   （`## Orchestrator answers (resolved escalations)`，标注「编排者输入，视为最新事实
   依据」），未决步骤 Skipped 并在报告中列出全部未决提问的【问题原文】
   （`open_escalations_from_journal`）——父模型是应答者，看不到问题原文就无法作答。
4. question_id 形如 `<plan_id>:<step>:q-<ts>-<n>`，冒号分隔支持从 id 反查 journal。

通往 Programmer/Prover 对抗环的第一块基石：答案经 journal 重放后注入重跑步骤，
`while (!verdict.pass)` 的「规则循环」所需的全部原语（可恢复步骤 + 编排者输入通道）
已就位；剩余差是 TS 脚本引擎本体（独立立项）。

## 第四轮移植：Programmer/Prover 对抗环原语

「规则是代码」的最小落地：步骤声明 `verified_by`（验证者步骤 id）与 `pass_condition`
（注入验证者的通过标准），执行器内建 `while (!verdict.pass)` 循环——不再依赖模型
手动 resume：

1. **调度**：被验证步骤执行成功后进入 `Verifying`（新状态），依赖方暂不消费其产出；
   验证步骤只在目标 Verifying 时进波次（验证依赖是隐式的，显式 `depends_on` 指向
   被验证者会在 parse_steps 拒绝——死锁防护；自引用与链式验证同样拒绝）。
2. **verdict 协议**：验证者任务被注入对抗简报（目标产出摘要 + 通过标准 + JSON verdict
   格式，`{"pass": bool, "reasons": [...]}`，第一个 `{` 到最后一个 `}`）；`parse_verdict`
   提取失败/解析失败/缺 `pass` 一律判 fail——验证者输出不规范等同「未能证明」。
3. **失败回路**：fail → `verdict_failed` 事件（含轮次与 reasons）→ 失败摘要注入目标
   任务文本（`## Verification feedback`）→ 目标与验证者双双回 Pending 重跑；轮次达
   `max_verdict_rounds`（默认 2，硬顶 4）→ 双双 Failed，依赖方按既有策略传染 skipped。
   pass → `verdict_passed` 事件，双双 Succeeded。
4. **预算持久**：journal 的 verdict 事件经 `seed_verdict_replay` 在 resume 时重建轮次
   计数——预算跨重启续算；未耗尽的失败恢复为「继续对抗」，已耗尽标 Failed 不自动加码。

journal 新事件：`verdict_passed {verifier, target, round}` /
`verdict_failed {verifier, target, round, reasons[, exhausted]}`。

## 第二轮移植：Layer 2 剩余项 + Layer 5

### Layer 1 补遗：rapid-refill breaker

第二轮测试抓到第一轮的一个真缺陷：低阈值配置（threshold=1 的测试）下，load 压缩后估算
仍超阈值 → 轮内 auto compact 每迭代重新摘要，无限消耗摘要调用。现在 load 期与轮内压缩
成功后都登记「压缩后地板估算」（`record_compression_floor`），估算未从地板回升 5% 之前
轮内压缩直接跳过——对齐 ZCode `rapid-refill breaker`（压缩后立刻又满则不再压）。

### Layer 2a：前台子代理自动转后台（`autoBackgroundMs` + `taskAbort.detachParent` 同源）

**机制**（`agent_tools/tools.rs` ClaudeAgentTool + `agent_tools.rs::launch_background_task_impl`）：
前台 Agent 调用从 t=0 起就是登记过的后台执行快照（AgentStatus 立即可见），父工具调用与它
**竞速**：
- `subagent_auto_background`（默认 600s，0=关闭）预算内完成 → 竞速通道回传完整结果，
  渲染与旧行为一致；
- 预算到点 → **detach**：工具丢弃竞速接收端（此后子任务完成时 send 失败，
  `run_background_execution` 判定「未被前台消费」→ 触发 Layer 4 合批唤醒），立即返回
  `{status:"backgrounded", agentId}`，父 turn 不再被子代理长尾阻塞。

**detachParent 两段语义**：启动时可携带 `(parent_cancel, done)` 联动——`done` 触发前父
`/cancel` 会终止子任务（保持前台「cancel 连子代理一起停」的既有语义）；done 触发即 detach，
之后父取消不再波及，子任务只受 collaboration cancel（AgentCancel）管控。联动令牌经
`turn_state::scope_top_level` 的 task-local（`turn_cancel()`）从主循环传入。

### Layer 2b：子代理独立模型/推理级别

Agent 工具 `model` 入参（schema 里原本就有但被忽略）→ `spec.model` → `ModelFactory`
（bootstrap 用与 BotRouter 相同的 `cfg.build_provider_config_for` 构建）按
`[[llm.providers]]` 名字解析独立 provider；未知名字在**入参校验期拒绝**（运行期解析失败
回落主 provider 不静默换模型）。`run_task_inner` 全部推理调用走解析后的 provider。

### Layer 1 补完：`/compact`

`TurnManager::handle_compact`：手动立即压缩当前会话——与自动压缩共用摘要/fallback/rewrite
管线，但忽略阈值与防抖（用户显式意图优先）；有活动 turn 时拒绝（避免与会话原子追加交错）。

### Layer 5a：主 turn 检查点（durable usage anchor）

`HistoryLoader::record_usage_anchor` 在每次成功 LLM 调用后把 `{total_tokens, anchor_hash}`
落盘到 `agent/sessions/checkpoints/<key-hash>.json`（/clear 随会话一并清除）。进程重启后
内存锚点清零、估算退化为纯启发式——`anchored_estimate` 现在在内存 miss 时从检查点恢复
（每 key 只读盘一次，含负缓存）。这配合既有的 ADR-0004 aborted-chain 占位（崩溃后模型
知道干到哪），构成主 turn 的断点续接语义。

### Layer 5b：`session_handoff` 跨会话交接工具

`session_handoff(channel, chat_id, query, max_chars)`（`src/session_handoff.rs`）：把另一段
会话压缩成有界交接素材——query 相关的更早消息（命中词数 + 新近度排序，最多 10 条）+ 最近
8 条原文；默认 24K 字符预算、48K 硬上限（对齐 ZCode ReadSessionContext）；防注入头部与
`session_search` 同纪律（历史素材 ≠ 指令）。

## 配置项汇总（`AgentOptions`，默认全开）

| 字段 | 默认 | 作用 |
|---|---|---|
| `in_turn_compression_enabled` | true | Layer 1 轮内压缩总开关 |
| `background_wake_enabled` | true | Layer 4 合批唤醒开关 |
| `background_wake_debounce` | 3s | Layer 4 合批窗口 |
| `subagent_auto_background` | 600s | Layer 2a 前台自动转后台预算（0=关闭） |

（CLI 未新增 toml 配置项；嵌入方可经 `AgentOptions` 显式覆盖。）

## 测试

第一轮：
- `turn/in_turn.rs::tests::context_exceeded_detection_covers_common_phrasings`：
  中英文超限文案识别（正/反例）。
- `background_wake.rs::tests`：窗口内去重合批（3 次完成 1 条消息）、多会话各一条、
  禁用/未 attach 零投递。
- `agent_tools/plan.rs::tests`：journal 最新结果解析（重跑覆盖 + 损坏行跳过）、
  缺失/空日志显式报错。

第二轮：
- `agent_tools/tests.rs::foreground_racing_channel_delivers_result_and_snapshot`：
  竞速通道回传终态 + 后台快照并存。
- `agent_tools/tests.rs::parent_cancel_linkage_kills_child_before_detach`：
  detach 前父取消联动杀死子任务（前台 cancel 语义保持）。
- `agent_tools/tests.rs::model_factory_resolves_per_spec_providers`：
  factory 注入前拒绝 / 注入后按名解析。
- `agent_tools/tests.rs::agent_tool_racing_completes_within_budget_and_returns_full_result`
  与 `agent_tool_auto_backgrounds_when_budget_exhausted`：Agent 工具端到端
  （预算内完整结果 / 超时转后台 + 快照 running）。
- `turn/history.rs::anchor_checkpoint_tests::usage_anchor_checkpoint_survives_process_restart`：
  检查点跨重启恢复锚定估算，无检查点回落纯启发式。
- `session_handoff.rs::tests`：query 相关选取 + 尾部保留 + 防注入头部 + 空/缺参报错。

## 第四轮测试

- `plan.rs::parse_verdict_extracts_pass_and_reasons`：正文内嵌 JSON 提取、fail 无
  reasons 兜底、提取/解析/缺字段一律判 fail。
- `plan.rs::parse_steps_rejects_invalid_verification_relations`：自引用 / 未知验证者 /
  链式验证 / 验证者 depends_on 被验证者（死锁）全部拒绝。
- `plan.rs::verdict_replay_seeds_resume_state`：未耗尽失败恢复对抗、耗尽标 Failed、
  通过维持继承。
- `agent_tools/tests.rs::verified_plan_reruns_target_until_verdict_passes`：端到端
  先败后过（脚本化 provider 第 4 次调用返回 pass verdict）——报告「第 2 轮通过」，
  journal 含 verdict_failed ×1 + verdict_passed ×1 + awaiting_verdict 标记。
- `verified_plan_exhausts_rounds_and_marks_failed`：验证者恒不给出 verdict → 轮次
  耗尽 → 目标 Failed（NoopProvider）。

## 第十一轮测试

- `agent_tools/workflow.rs::dependency_graph_sequential_chain_and_parallel_wave`：
  串行链三层波 + Promise.all 同波无边（并行标记）。
- `dependency_graph_mixed_wave_then_chain`：波末节点 → 后续 ask 有边，波次两层。

## 第十轮测试

- `agent_tools/tests.rs::workflow_race_replay_preserves_completion_order`：
  SlowFastProvider（慢 150ms/快即时）→ 原 run race 赢家 fast；resume 零调用重放 →
  顺序门使解析序仍 fast 先（无门时 slow 先——曾以此失败验证门必要性）。

## 第九轮测试

- `agent_tools/tests.rs::workflow_ask_json_schema_validates_answers`：askJson 合法
  JSON 答案通过校验聚合字段；非 JSON 答案 → 校验失败 → run 失败（工具 err）。
- `workflow_type_check_downgrades_gracefully_without_tsc`：`type_check: true` 下
  banned pattern 诊断必然存在（lint 层与 tsc 可用性无关，降级路径确定性）。
- `agent_tools/workflow.rs::schema_validator_enforces_types_and_required`：schema
  子集校验（类型不符/缺必填/数组元素递归）。
- `insight_extracts_actors_and_parallel`：actor 提取/ask 计数/并行标记。
- 报告含 `insight` 字段（actors/ask_count/parallel）。

## 第八轮测试

- `agent_tools/tests.rs::workflow_amend_imports_completed_asks_and_reruns_changed_ones`：
  前驱 2 问 → amend 保留 1 问（零调用导入）+ 改写 1 问（真实重跑）→ provider 总
  调用 3；新 lineage run_id；`amended_from`/`imported_asks` 入报告。
- `workflow_amend_rejects_missing_predecessor`：前驱 journal 缺失显式报错。
- `workflow_rejects_resume_and_amend_together`：两参数互斥。

## 第七轮测试

- `agent_tools/tests.rs::workflow_serial_await_and_zero_cost_replay`：await 串行
  扇出（2 ask = 2 provider 调用）→ resume 全量重放零调用。
- `workflow_promise_all_parallel_fanout_and_replay`：Promise.all 并行扇出 + 重放
  零调用（`max_parallel_asks: 2`）。
- `workflow_resume_rejects_modified_script`：script hash 拒改（async 引擎回归）。
- `agent_tools/workflow.rs::sandbox_pins_nondeterminism`：async IIFE + Promise
  全链路下钉死值生效（`[0,0.5]`）。
- `check_tests::clean_async_script_passes_check`：干净 async 脚本零诊断。
- `syntax_error_is_rejected_with_diagnostic` / `banned_patterns_reported_with_line_and_col`
  （eval 报 L2）/ `dynamic_import_is_rejected_statically`。

## 第六轮测试

- `agent_tools/workflow.rs::fnv1a64_matches_known_vectors`：FNV-1a 64 标准向量。
- `sandbox_pins_nondeterminism_and_blocks_import`：Date.now/Math.random 钉死
  （`[0,0.5]`）、import 永不 resolve、冻结后篡改无效（抛错或静默两可）、语句结尾
  completion 不报错。
- `agent_tools/tests.rs::workflow_serial_fanout_and_zero_cost_replay`：端到端
  串行扇出（2 ask = 2 provider 调用，result 聚合 "ok|ok"）→ resume 全量重放
  （asks_replayed=2、provider 调用数不变）。
- `workflow_resume_rejects_modified_script`：脚本变更被 script hash 拒绝。

## 第五轮测试

- `turn/rewind.rs::capture_records_relative_path_against_agent_base`：相对路径按
  agent 根解析；非文件写入工具不捕获。
- `restore_round_trip_and_delete_recreation`：连环选点（回滚 200 → a=v2、b 不变；
  回滚 100 → a=v1、100 新建的 b 删除）——synthetic 重锚使连环回滚不误判。
- `restore_refuses_externally_modified_files_without_touching_anything`：外部改动
  一文件 → 整组拒绝且其余文件保持原样。
- `restore_errors_when_no_checkpoints_for_message`：无 checkpoint 显式报错。
- `message_ids_are_unique_and_monotonic`：messageId 严格递增。

## 第三轮测试

- `agent_tools/tests.rs::escalate_requires_plan_context`：计划上下文缺失报错。
- `escalate_inside_plan_context_journals_and_parks`：CURRENT_PLAN 作用域内提交 →
  `submitted` + journal `step_escalated`（含 question_id / 原文）。
- `resolve_delivers_to_waiting_escalate_and_journals`：`plan_escalation_wait_secs=10`
  时 escalate 阻塞等待 → ResolveWorkflowQuestion 实时解锁（`delivered_live=true`），
  答案经通道回到步骤 + journal 两种事件齐备。
- `resolve_without_waiter_records_journal_for_resume`：无等待者 → 只记 journal，
  指引 resume_plan_id。
- `journal_escalation_parsing_pairs_questions_with_latest_answers`：journal 解析
  （问题/答案配对，答案后到覆盖）。
- `escalation_seeding_reruns_answered_and_skips_unresolved`：已解答强制重跑 + 答案
  注入；未决 Skipped；未知步骤忽略。

## 后续工作（按并行任务价值的优先级）

1. **并行波次上限提升**：`max_concurrent_turns` 与 PlanRun `MAX_PARALLEL_STEPS=4` 是
   大规模并行的直接瓶颈；唤醒合批已消除通知放大，下一步是压测信号量与错峰参数。
2. **工具结果落盘 + 引用句柄**（ZCode artifact 策略）：超预算结果落盘、模型拿预览+路径，
   与 microcompact 互补。
3. **按 messageId 的主 turn 精确 rewind**：现在有检查点 + aborted-chain 续接，
   任意锚点 fork 恢复是下一步。
4. **完整动态工作流引擎**：程序化编排（脚本 + journal 重放 + escalate）——量大，
   建议独立立项；PlanRun resume 是它的第一块基石。
