# ZCode 长程能力移植：五层体系 → RClaw（大规模并行任务定位）

> 参照系：`zai-org/ZCode`（v3.15.1，Apache-2.0）的 long-horizon 机制。
> RClaw 的定位是**大规模并行任务**——大量子代理并行 + 长 ReAct turn，因此移植
> 优先级按「哪一层先让并行编排不塌」排序，而非照抄 ZCode 的实现顺序。

## 移植总览

| ZCode 机制 | RClaw 移植 | 状态 |
|---|---|---|
| Layer 1a microcompact（`core/compact/microcompact.ts`） | `turn/in_turn.rs::microcompact_view`（每次请求视图本地剪枝，不落盘） | ✅ 第一轮 |
| Layer 1b auto compact（每迭代阈值检查） | `turn/in_turn.rs::maybe_auto_compress`（anchored 估算 + 头区摘要 + 追加 compaction boundary） | ✅ 第一轮 |
| Layer 1c reactive compact（context-exceeded 原地重试） | `turn/in_turn.rs::reactive_compress` + `error_indicates_context_exceeded` | ✅ 第一轮 |
| Layer 1d manual `/compact` | `/compact` 斜杠命令 → `TurnManager::handle_compact`（忽略阈值/防抖，活动 turn 时拒绝；在独立任务上运行，不阻塞路由） | ✅ 第二轮 |
| Layer 1e rapid-refill breaker | `HistoryLoader` 地板估算：压缩后未回升 5% 不再触发轮内压缩（`in_turn_blocked_by_rapid_refill`） | ✅ 第二轮补齐 |
| Layer 2 子代理即上下文隔离 | RClaw 已有（共享 Run 内核 + candidate_patch 隔离 + 并发 Agent 调用） | 已具备 |
| Layer 2a 前台自动转后台（`autoBackgroundMs` + `taskAbort.detachParent`） | Agent 工具竞速 + `launch_background_task_impl` 联动：预算内完成回传完整结果，到点 detach（父取消不再波及）返回 backgrounded | ✅ 第二轮 |
| Layer 2b 子代理独立模型/推理级别（`.zcode/agents` frontmatter） | Agent `model` 入参 → `spec.model` → `ModelFactory` 按 `[[llm.providers]]` 名解析独立 provider | ✅ 第二轮 |
| Layer 3 动态工作流（journal 重放 + amend） | `PlanRun resume_plan_id`（journal 断点续跑 + 按 id 继承已完成步骤） | ✅ 第一轮简化版 |
| Layer 3 escalate 提问通道（`escalate` / `ResolveWorkflowQuestion`） | 计划步骤阻塞式提问 + 编排者应答，问答进计划 journal（`agent/state.sqlite` 的 `journal_event` 表，流 `plan/<plan_id>`；`step_escalated` / `escalation_resolved`）支持重放 | ✅ 第三轮最小内核 |
| Layer 3 Programmer/Prover 对抗环（`while (!verdict.pass)`） | 步骤声明 `verified_by`/`pass_condition`：验证步骤返回 JSON verdict，失败摘要注入目标重跑，轮次预算（`max_verdict_rounds`，默认 2）经 journal 跨 resume 持久 | ✅ 第四轮原语 |
| Layer 3 引擎最小执行器（rquickjs 确定性沙箱） | `WorkflowRun` 工具：JS 脚本经 `agent(name).ask(question)`/`result()` facade 驱动子代理；`Date.now`/`Math.random` 钉死并冻结 `Date`/`Math`、无模块加载器（动态 `import()` 以 `could not load module` 拒绝）；ask 问答带 FNV-1a inputHash 增量写 run journal（`agent/state.sqlite` 流 `workflow/<run_id>`）；resume 按 (actor, inputHash) 零调用重放，script hash 拒改 | ✅ 第六轮 |
| Layer 3 引擎 async 化（ask 返回 promise + Promise.all 并行扇出 + set-order） | rquickjs `futures` 特性：脚本包进 async IIFE（顶层 await/Promise.all），ask 经 Async 宿主返回 promise，信号量 `max_parallel_asks`（默认 4/硬顶 8）限流；journal 记发起序 seq + 完成序 answer，重放按 (actor, inputHash) FIFO 结果确定 | ✅ 第七轮 |
| Layer 3 facade 类型/facade 检查（诊断拒绝提交） | `check_script`：QuickJS AsyncFunction 语法探针（只解析不执行，与 async IIFE 包装语义一致）+ 禁用模式静态扫描（Date/eval/new Function/require/import/setTimeout/process，真实 L:C）；带诊断的脚本拒绝提交，诊断只回模型 | ✅ 第七轮（语法+facade lint；tsc 级类型推断因 swc/serde 生态冲突记为后续） |
| Layer 3 WorkflowRun Amend 热修 | `amend_run_id`：修订脚本按 actor 名 + inputHash 导入前驱 journal 已完成 ask（同问文本零调用继承；改写/新增照常执行），新 lineage journal 记 `amended_from`/`imported_asks`；与 resume_run_id（逐字节一致续跑）互斥 | ✅ 第八轮 |
| Layer 3 类型级检查可选增强（node/tsc 工具链） | `check_script(script, type_check)`：node/tsc 探测（进程级缓存）→ spawn `tsc --noEmit --strict` 对脚本+facade.d.ts → 解析 `path(line,col): error TSxxxx` 诊断（只取 script.ts）；node/tsc 缺失或超时静默降级为语法+lint（两级共用 FacadeDiagnostic）。工具入参 `type_check`（默认 true），阻塞调用放 spawn_blocking | ✅ 第九轮 |
| Layer 3 askJson（泛型 T→JSON Schema 答案校验） | facade `askJson(question, schema)`：答案 JSON 解析 + Rust 侧子集校验器（type/properties/required/items 递归，深度上限 8）；校验失败按错误抛回脚本 | ✅ 第九轮 |
| Layer 3 静态洞察（依赖图近似） | `analyze_workflow_script` 文本级分析（actor 清单/ask 数/并行标记），入 `run_started` 事件与执行报告 `insight` 字段；AST 级依赖图为后续 | ✅ 第九轮（文本级近似） |
| Layer 3 依赖图静态分析（DAG） | `analyze_dependency_graph`：语句级解析 → 节点（ask 调用点，actor/行号/并行标记）+ 边（前波→后波）+ Kahn 拓扑波次；串行链成链、Promise.all 成波（内部无边）；入 run_started 与报告 | ✅ 第十一轮（语句级文本解析，非完整 AST） |
| Layer 3 字面 set-order 调度重放（race 保真） | ReplayAnswer 携带全局完成序 order（journal answer 事件序）；JS prelude 链式顺序门（`__gate_wait`/`__gate_pump` 逐级微任务泵送）：重放命中按原完成序延迟 resolve，Promise.race 赢家与解析顺序与原完成序一致 | ✅ 第十轮 |
| Layer 4 后台通知合批 + 唤醒（`runtime-task/notification.ts`） | `background_wake.rs::BackgroundWaker`（防抖合批 + Heartbeat 唤醒消息） | ✅ 第一轮 |
| Layer 5 主 turn 检查点 | durable usage anchor（`agent/state.sqlite` 的 `usage_anchor` 表，按消息 id 锚定，跨重启恢复估算精度）+ ADR-0004 aborted chain（已有） | ✅ 第二轮 |
| Layer 5 跨会话交接（`ReadSessionContext`） | `session_handoff` 工具（query 相关选取 + 尾部原文；单条上限与相关条数按 24K/48K 预算推导；中英分词；归档回退；防注入头部） | ✅ 第二轮 |
| Layer 5 按回滚点 id 选点回滚（rewind + hash 校验） | `turn/rewind.rs`：write/edit/delete 前后 checkpoint（before/after hash，按回滚分组 id 归档到 `file_checkpoint` 表，与工具链消息同一事务写入）+ `rewind_files` 工具与 `/rewind` 命令；恢复前 hash 校验，外部改动整组拒绝（all-or-nothing） | ✅ 第五轮 |
| 并行波次上限提升（`max_concurrent_turns` / PlanRun 波次） | 缺省 6→8、PlanRun `DEFAULT_MAX_PARALLEL`/`MAX_PARALLEL_STEPS` 2/4→4/8、WorkflowRun asks 4/8→8/16（schema 同步）；压测三件套：8 路并发错峰保持（TimingProvider）、8 步单波峰值并发 = 8（PeakConcurrencyProvider）、16 ask Promise.all 扇出聚合 | ✅ 第十二轮 |
| 工具结果落盘 + 引用句柄（artifact 策略） | `turn/artifact.rs`：工具结果超预算（默认 32KB）时全文写 `agent/artifacts/<会话键>/<回滚点id>-<工具>.txt`（脱敏后），模型收 4KB 头尾预览 + 路径句柄（read 分页取回）；落盘失败静默回退；前台 / 子代理 / Agent 并发三路径同层接入 | ✅ 第十二轮 |
| 会话历史任意锚点 fork | `SessionStore::fork_session`：把活动视图前缀（≤ 锚点，按视图序）单事务复制进新会话，boundary 摘要降级为普通行，血缘入 kv；`/fork [消息 id]` 命令（无参列锚点）；桌面/安卓 UI 消息下方「⑂ 分叉」按钮（chat_fork 命令 + 切换新会话） | ✅ 第十二轮 |
| 工作流 AST 级分析 + `.ask<T>` 编译 | `agent_tools/workflow_ast.rs`（oxc 0.116，纯 Rust）：AST 依赖图（字符串/注释不误报、hoisted Promise.all 扇出、inline agent() 调用；失败降级文本解析）；`.ask<T>` 内联类型编译为 JSON schema 并重写为 `askJson(question, schema)`（script hash 仍按原文；类型别名/字面量/联合/数组，Rust 校验器扩展 const/enum/anyOf） | ✅ 第十二轮 |
| 会话长期目标 /goal（ZCode /goal reminder） | `/goal <目标>` 写 kv（按会话隔离，2000 字上限）；无参查看、clear/done 收尾；活动目标时【每次推理的尾部上下文】注入提醒块（原文 + 持续工作 + 按证据核对 + 不自行宣告完成 + 提醒用户 /goal clear），Rust 侧 `goal_reminder_block` 可单测 | ✅ 第十二轮 |
| PlanRun 任务暂停 / 继续（ZCode 任务暂停） | kv `plan-pause/<plan_id>` 协作式暂停：执行器在【波次边界】轮询（300ms）——当前波跑完、后续波不派发，父 /cancel 语义不变；`plan_paused` / `plan_unpaused` 事件入 journal（kv 标志为权威，`plan_resumed` 是 resume 代际锚点、不混用）；执行器收尾守卫在取消 / 早退 / panic 时补 `plan_aborted` 终态；桌面 `plan_pause` 命令（只对执行中的计划接受暂停）+ 步骤卡片展开时的 ⏸/▶ 按钮（plan_status 投影 paused / aborted 字段，执行者不在本进程的进行中计划投影成已中止） | ✅ 第十二轮 |
| 桌面/安卓 UI：流程步骤 + todo + 排队消息 + 分叉 | `plan_status` 命令投影 PlanRun journal（plan_started 补步骤任务文本）；消息流「📋 流程步骤」可折叠卡片 + 会话面板 todo 区；agent 运行期间输入入队（删除/编辑/「立即」插队投递，turn 收尾自动按序发出）；消息下方「分叉」入口（桌面内联 + 移动端长按菜单） | ✅ 第十二轮 |

## Layer 1：轮内上下文管理（本次移植的核心）

**移植前的问题**：压缩只发生在轮次开始（`HistoryLoader::load`）。一个长 ReAct turn
（`max_tool_iter` 默认 256 次迭代）内部完全没有压缩——大规模并行时父 turn 汇总 N 个
子代理结果，巨大的工具输出一路堆积，单 turn 就可能撑爆窗口，且中途没有任何自愈手段。

**三层机制**（全部复用 RClaw 既有压缩基础设施，见 `turn/in_turn.rs`）：

1. **microcompact**（`turn.rs` 重试循环内，每次请求）：请求视图 = `microcompact_view(&messages)`
   ——对【视图】做 load 期同一套 prune（旧媒体剥离 / 重复与陈旧 tool result 摘要化 /
   大 tool_use input 裁剪），保护窗口（`compression_prune_protect_count` 条 +
   `compression_tail_token_budget` token）内最近消息原样保留。**只改视图不改内存与磁盘**，
   与 load 期预剪枝语义一致；下一轮 load 仍取回完整内容。由此 `max_history_bytes` 量的也是
   剪枝后的请求体量：load 期在 microcompact 视图上量出该裁掉几条，再按同一条数裁原始视图
   （剪枝不改条数、不改 tool_result 载体位置），原始工具输出的字节数不会把真实请求远低于
   上限的会话裁成不完整后缀。
2. **auto compact**（ReAct 循环顶部，每迭代一次）：`anchored_estimate`（真实 usage 锚点 +
   增量启发式，与 load 期同一套防抖/冷却状态）≥ `compression_threshold_tokens` 时，
   对头区 `[..turn_start]` 做预剪枝 → 复查 → 摘要 → 追加 compaction boundary（锚点 = 头区尾窗首条的 id）。
   预剪枝后的复查用「剪枝前估算 − 锚点之后的剪枝收益」：锚点记录的请求本身已是剪枝过的
   视图，锚点之前的收益早在真实用量里，再扣一次会把估算压到真实值之下、让压缩被静默抑制
   （`prune_savings_start` / `estimate_after_prune`，load 期同一套）。**当前 turn 的尾区
   `[turn_start..]`（本轮 user 消息起）永远原样保留**：本轮任务原文既不进摘要素材、也不是
   摘要的 merge 目标；头区里上一轮的 user / assistant 由尾窗锚点留在尾窗——对齐 ZCode
   「强制保留最后一轮原文」的任务保真设计。「历史太短」的前置门槛按尾窗策略推导
   （`history_too_short_to_compress`，load 期 / 轮内 / `/compact` 同一口径）：尾窗带 token
   预算时几条巨型工具输出也可能需要压，不能按 `keep_recent + 2` 条数一刀切，否则 reactive
   与 `/compact` 都「没有可压缩的窗口」、会话只能 `/clear`。
3. **reactive compact**（LLM 错误分支）：`error_indicates_context_exceeded`（中英文文案
   特征，覆盖 `StreamAborted` 与 `BadStatus 400` 两种到达形态）命中时忽略阈值强制压缩
   一次，成功后 `continue 'retry` 原地重试。每迭代至多一次，防止「压缩-超限」循环。

**关键设计决策**：

- **头区/尾区边界**：`turn_start` = 本轮 user 消息下标（push 完本轮 user 消息后取
  `len() - 1`；`compress_head_region` 核对尾区首条确是真实 user 消息，约定被破坏时宁可不压，
  也不按错误边界把本轮任务原文卷进摘要）。头区 =
  加载期返回的历史视图（store 活动视图经 `fold_compaction_view` 归一；不压缩时还会按
  `max_history` / `max_history_bytes` 裁掉前缀），本轮已持久化的消息 id 天然落在 boundary
  的保留区间内；本轮的 `skill_context` / 媒体附件消息是【不落盘】临时上下文（id 为 0），与
  本轮其他消息一起留在尾区整段保留（模型还要参考），store 里本就没有它们，不需要单独跟踪。
- **取不回被裁掉的前缀就不压**：裁剪后缀视图上落 boundary 必须把被裁掉的前缀喂给摘要器
  （`load_trimmed_prefix`）。本轮 load 后有并发 boundary 把视图首条折掉时放弃本次压缩，并以
  turn 级标志（`InTurnContext::prefix_unavailable`）记住——一个 turn 内不会自愈，后续迭代的
  auto / reactive 压缩直接跳过（reactive 在宣告「执行 reactive compact」之前就跳过），不再
  每迭代重读整库；一次读库失败则只放弃本次，下一迭代可再试。
- **轮内的「头区过短 / 无窗口」不记低效**：这两个分支没有 LLM 调用，只说明头区没有可摘要
  窗口，不代表下一次 load 在完整视图上也没有；按低效记会在本轮没再持久化任何消息（如最终
  LLM 调用失败）时让下一次 load 期压缩被「近期连续低效」无故压住。
- **视图来源决定能否保护头部**：加载期随视图返回 `ViewProvenance`（完整 / 裁剪后缀 + 被裁掉
  的旧摘要正文）。裁剪后缀上头区的前几条不是会话开头，轮内压缩一律 `protect_first = 0`
  （只落 `tail_from`；否则 `head_until` 锚在会话中段，store 锚点校验会拒绝或把被裁掉的前缀
  复活成受保护头部），被裁掉的旧摘要作为 previous_summary 增量更新。被裁掉的普通消息仍是
  会话历史（新 boundary 落库后它们只剩这份摘要代表）：从库里取回、剪枝后并入摘要素材最
  前面，但不参与压缩计划（尾窗 / 锚点只在视图头区上算，前缀不会被尾窗锚点拉回视图）；
  transcript 预算由头区优先占用，其次是前缀开头按受保护头部对待的几条——上一 boundary 的
  受保护头部，没有上一 boundary（首次压缩就落在裁剪视图上）时是会话开头 `protect_first` 条
  （会话开头的任务原文——从未被任何摘要覆盖、本次 boundary 落库后也不再在视图里，load 期
  在完整视图上压缩时它们要么进 `head_until` 要么在 transcript 最前面总能活下来，轮内不能因为
  视图被裁过就先丢它们），其余前缀放不下时从最早的让起，让位说明只说明省略、不声称旧摘要
  覆盖了被让掉的消息；前缀整段被让掉时记 `warn`（`merge_prefix_into_summary_material`）。
  读库失败、或视图首条已不在库的活动视图里（本轮 load 后有并发 boundary）时放弃本次压缩，
  而不是落一条没见过前缀的 boundary。
- **user 锚点受预算约束**：尾窗锚点把头区里上一轮的 user 留在尾窗，但从它起的尾段估算已
  超过 soft ceiling（上一轮是「1 条 user + 上百次工具迭代」）时不回拉——否则整轮被钉在尾窗、
  可摘要的只剩一两条，轮内与 reactive 每次都是无效计划、自愈链路卡死；此时用户原文由摘要
  模板的 `## Active Task` 兜底（`compute_tail_start_after_head`）。兜底要成立，摘要器得先
  看得到原文：transcript 里单块正文只有 2000 字符，素材里最后一条真实 user 消息在那里被截、
  或整条落在被省略的中段时，prompt 另给一个 verbatim 块（上限 8000 字符，
  `latest_user_input_verbatim`）；更长的原文仍只保前 8000 字符，此时块标题明说是
  「前 8000 字符、原文更长」而不是 verbatim，摘要器不会把前缀当全文。
- **transcript 超预算省略中段**：摘要素材超过 48k 字符时不再从末尾截断——末尾是紧挨尾窗、
  模型最需要的最新迭代，而且 boundary 之后不会再有别的摘要覆盖它们。`render_transcript`
  预算对半分：开头（前缀 / 任务原文）与结尾各装整条，中间以「N message(s) omitted」标记
  隔开；最后一条单独就超出尾部额度时按字符保留它的开头。被摘要区自身超预算时记 `warn`。
- **不再 acquire `execution_limit`**：load 期压缩在持槽前，轮内压缩在持槽后——若再拿一枚
  槽位，`max_concurrent_turns` 个 turn 同时压缩会互相等待死锁。
- **boundary 落库失败不回滚内存**：内存已切换为压缩视图，本轮继续；库中保留原历史（下次
  load 再压），与「摘要失败不改写历史」的 fallback 语义一致。此后到下次 load 之前不记 usage
  锚点（`anchors_suspended`）：内存与库分叉期间记下的真实用量对应压缩后的请求，拿去锚定库中
  视图会把下一轮的加载期估算整体低估。摘要 LLM 鉴权 / 网络失败与 load 期同样进入失败冷却，
  同一 turn 内不再反复撞摘要调用。
- **防抖水位**：「连续低效」防抖比较的是历史水位（视图里最大的已持久化消息 id），加载期与
  轮内同一把尺子；按条数比会因裁剪后缀条数长期小于完整视图条数而把轮内压缩无故压住。
- 开关：`AgentOptions::in_turn_compression_enabled`（默认 true；关闭退回旧行为；
  `session_compression_enabled = false` 时整体不生效——microcompact 也不剪枝请求视图）。

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
- 装配：`AgentToolRuntime::background_wake()` 句柄，每个二进制（CLI `rclaw-cli/src/main.rs`、
  桌面 / Android 宿主 `rclaw-desktop/src/agent.rs`）建好 inbound 总线后
  `attach(inbound_tx.clone())`（与 bot_router_slot 回填同模式）；未接线的宿主后台完成只入
  邮箱、不唤醒。
- 开关：`background_wake_enabled`（默认 true）。尾部提示文案已同步更新（不再声称
  「团队消息不会唤醒主循环」）。

## Layer 3：PlanRun journal 断点续跑 + amend（`agent_tools/plan.rs`）

**移植前的问题**：PlanRun 是一次性工件——执行器崩溃 / 上限中断后无法恢复，失败不重试，
只能整计划重跑。

**机制**：`resume_plan_id` 输入参数（事件日志标识在每次 PlanRun 结果里都有）：

- `load_plan_journal` 读取计划 journal（`agent/state.sqlite` 的 `journal_event` 表，流
  `plan/<id>`，`seq` 即事件先后），取每步【最后一次】`step_finished`。
- **续跑**：journal 里 succeeded 的步骤按 id 继承结果摘要、不再重跑；failed/skipped 回到
  Pending 走既有执行循环。例外是 verdict 预算已耗尽的对抗环对子：预算持久，原样 resume
  仍判 Failed、不碰 provider（改任务文本也不重置）——要重跑得带更大的 `max_verdict_rounds`
  （超过 journal 里记录的轮次即续算）或更换 `verified_by`（新对子没有 verdict 记录，目标
  产出复用、只补跑新验证者）。工具 schema 与报告的处置提示都写明了这条例外。
- **amend**：步骤按 id 匹配——模型可以修改失败步骤的 task 文本后重提交，成功步骤的
  缓存不失效（对齐 ZCode AmendWorkflow「按 actor 名字导入已完成工作」）。
- journal 先追加本代锚点 `plan_started` / `plan_resumed`，再装载旧事件（锚点后写会让
  落在「装载之后、锚点之前」的应答两头落空），装载统计（inherited / journal_entries /
  pending_escalations / injected_answers）记在 verdict / 升级重放之后的 `plan_seeded`——
  `inherited` 只计重放后仍复用产出的步骤（Succeeded / Verifying），被答案或未耗尽的
  verdict 翻回 Pending 的不算；`pending_escalations` 只计当前计划里仍存在的步骤。执行
  报告与处置提示都会带 `resume_plan_id="<plan_id>"` 指引，模型从失败报告里直接学会如何
  恢复；amend 移除 / 改名的步骤留下的提问在报告里单独列出（无需应答、不会被消费）。
- verdict 轮次预算的持久化有双份：`verdict_passed` / `verdict_failed` 与验证者自己的
  `step_finished`（带 verdict / target / round / reasons）都是宽松写入，装载时任一条
  缺失都能从另一条补回同轮 verdict，耗尽预算的失败不会因一次写丢而多跑一轮。「同轮」
  只在本对抗周期内比（只有晚于目标最近一次 `step_finished` 的记录算本周期）：轮次按周期
  从 1 重计，新周期的第 1 轮不是旧周期第 1 轮的重复。
- 日志不存在 → 显式报错（错误重定向比静默从零重跑安全）；只有 `plan_started` /
  `step_escalated` 而没有任何 `step_finished` 的 journal 是合法的（首步就 escalate 的计划）。

**与 ZCode 动态工作流的差距（有意保留）**：ZCode 的 dWF 是完整程序化引擎（TS 脚本 +
类型检查 + vm 沙箱 + inputHash 重放 + set-order 重放）。RClaw 的 PlanRun 是「JSON 步骤表
+ 拓扑波次」的最小工件，本次只补了 journal 续跑与 amend。完整的程序化编排（while-until-
pass 循环、Promise.all 扇出、escalate 提问）是后续独立立项的量级，见「后续工作」。

## Layer 2 / Layer 5 现状

- **Layer 2（子代理即上下文隔离）**：RClaw 原本已具备核心能力——子代理独立对话上下文、
  前台并行（`is_parallel_agent_tool`）、后台执行 + 快照 resume、candidate_patch 隔离、
  推理错峰。与 ZCode 的两项差距「前台任务中途自动转后台」（Layer 2a）与「子代理可配
  独立模型」（Layer 2b）已在第二轮补齐，见下文。
- **Layer 5（跨会话连续性）**：在会话存储（`agent/state.sqlite`）恢复 + 子代理执行快照 resume +
  `session_search` 之上，已补齐主 turn 检查点（durable usage anchor，第二轮）、
  `session_handoff` 跨会话交接工具（第二轮）与按回滚点 id 的文件选点回滚
  `rewind_files` / `/rewind`（第五轮）。仍未覆盖：会话历史本身的任意锚点 fork
  （rewind 只回滚文件，不动历史），见后续工作。

## 第六轮移植：Layer 3 引擎最小执行器（rquickjs 确定性沙箱）

嵌入 **rquickjs 0.9**（QuickJS-ng，MSVC 侧 C 源码捆绑构建）实现引擎骨架：

- **确定性沙箱**（`eval_deterministic`）：prelude 固定 `Date.now`/`Math.random`
  并 `Object.freeze(Date/Math)`（篡改赋值被 QuickJS 抛 TypeError 或静默忽略，钉死值
  始终生效）；不注册模块加载器 → 动态 `import()` 以 `could not load module` 拒绝
  （promise reject），模块内容不可得；内存上限 64MB + 120s 中断处理器。异常经
  `ctx.catch()` 提取真实消息。
- **facade**：`agent(name) → {ask(question)}` 与 `result(text)`（prelude 包装宿主
  函数 `__ask_raw`/`__result`，错误在 JS 侧抛出）。
- **journal**（`agent/state.sqlite` 流 `workflow/<run_id>`，逐事件增量落库）：`run_started
  {script_hash}` → `ask {seq, actor, question, input_hash}` → `answer {seq,
  input_hash, answer}` → `run_completed`。inputHash = FNV-1a64(actor\0question)。
- **断点重放 = 重跑脚本**：`resume_run_id` 时从 journal 重建 (actor, inputHash) →
  答案表；重跑时命中即直接结算（provider 零调用），脚本 FNV hash 不一致则拒绝
  （v1 要求逐字节一致，amend 后续迭代）。
- **桥接**：QuickJS 的引擎 future 非 Send（裸指针）——整段放 `spawn_blocking`，
  内部以嵌入的 current_thread runtime 驱动，ask 经 rquickjs Async 宿主以 promise
  形式驱动 `run_task` 子代理（本轮串行扇出；第七轮起 `Promise.all` 并行扇出，见下）。

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
  该增强已在第九轮落地：`WorkflowRun` 的真实入参 `type_check`（默认 true）——宿主
  node/tsc 在位即启用类型级检查，缺失或超时静默降级（见「第九轮移植」）。

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
代价是本轮不做 TS 类型推断（完整 .d.ts 类型检查依赖独立 JS 工具链，第九轮以
`type_check` 可选增强补上）。

## 第五轮移植：Layer 5 选点回滚（按回滚点 id 精确 rewind）

**checkpoint 采集**：前台路径的 write/edit/delete 在执行前后捕获文件快照
（`turn/rewind.rs::capture_before/finish_capture`；路径语义与文件工具 `resolve_path` 一致，
`resolve_like_file_tool` 纯词法规范化；内容超 2MB 不捕获——无法恢复的写入不做半吊子记录，
no-op 写入不入账）。turn 循环为每次 assistant 消息预分配**纳秒级单调回滚分组 id**
（与 `Message.id` 自增主键无关，只存在于 `file_checkpoint` 表；`/rewind` 列表与
`rewind_files` 以它引用回滚点），checkpoint 与 assistant/tool_result 消息在同一个
SQLite 事务里落库（`append_tool_chain_with_checkpoints`）——不存在「消息已写、检查点
未写」的半状态。

**选点恢复**：`/rewind [回滚点 id]` 斜杠命令与 `rewind_files` 工具（参数 `rewind_id`；经 CURRENT_CHAT
task-local 定位会话）。语义 = **回滚回滚点 M 及其后全部 checkpointed 写入**（最新优先，
恢复到 M 之前的文件状态）；会话历史不动。安全：每个文件先与它【最新】checkpoint 的
after_hash 比对（含回滚自身落下的 synthetic 重锚标记——连环回滚不会被上一次回滚误判
为外部改动），任何不符即外部改动 → 整组拒绝（all-or-nothing）。`/rewind` 命令持【静默门】
恢复：只要【任何】会话有活动 turn 或后台 review 在跑就拒绝（原因是别的 turn 的工具
文件写入可能与恢复交错——文件不分会话），持门期间新 turn 也无法启动；`rewind_files`
工具本身就运行在活动 turn 之内，不做此检查——它与本 turn 的文件写入串行，靠 hash
比对守住一致性。

已知边界：只覆盖结构化文件工具；shell/skill 脚本副作用与子代理（Subagent 策略）写入
不在 checkpoint 覆盖范围。

## 第三轮移植：Layer 3 最小内核——escalate 提问通道

ZCode 的 `escalate` 让子代理被卡住时「阻塞式向主代理提问而不是编造答案」。RClaw 的
PlanRun 前台运行期间编排者（父 turn）被工具调用阻塞、无法实时应答，因此 v1 采用
**journal 协议**（问答进计划 journal `agent/state.sqlite` 流 `plan/<plan_id>`，天然支持重放）：

1. **`escalate`**（子代理面，靠 `CURRENT_PLAN` task-local——`run_task` 按
   `spec.plan_id/step_id` 注入作用域——定位计划）：追加 `step_escalated`
   `{question_id, step, question, context}` 事件并登记待应答问题。
   `plan_escalation_wait_secs`（默认 0）> 0 时经 in-memory oneshot 通道阻塞等待实时
   答案（为将来 PlanRun 后台化预留真·阻塞语义），默认提交即挂起——步骤被明确指示
   「不要编造答案，立即以结构化 handoff 收尾」。
2. **`ResolveWorkflowQuestion`**（主代理面，对子代理屏蔽——`is_subagent_blocked_tool`）：
   `{question_id, answer}` → 先在 journal 的同一个写事务里校验问题仍是 Open 并追加
   `escalation_resolved` 事件（journal 单点记录，重放唯一事实源；已应答 / 不存在 / 读写
   失败一律拒绝、答案一个字都不投递），再实时解锁等待中的步骤（若在窗口内）。反过来会
   出现「步骤拿着答案继续了，journal 却没有应答记录」，下一次 resume 把同一问题再挂起一遍。
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
2. **verdict 协议**：验证者任务被注入对抗简报（目标任务文本【全文】——默认通过标准就是
   它，不能按依赖摘要的 1500 字裁 + 目标最新产出简报 + 通过标准 + 目标收到的编排者答复
   + JSON verdict 格式，`{"pass": bool, "reasons": [...]}`，pass=true 也要列自己核对到的
   证据）。产出简报 `verification_brief` = handoff 摘要 + 它自报的
   evidence / artifacts / validation / open_risks（条数与单条长度有上限，整体 12000 字），
   让验证者知道该跑哪条验证命令、看哪个产物，而不是对着一段文字猜；简报随目标的
   `step_finished {verification_brief}` 落盘，resume 补跑验证者时注入的是同一份。验证步骤
   一律 shared_cas 隔离（candidate_patch 沙箱没有 shell，验证者的任务恰恰是跑测试 / 复现），
   不按任务文本长相判成「编码任务」；`classify_verdict` 扫描正文中所有平衡的 JSON 对象
   （只有像 JSON 对象开头的 `{`——后接 `"` 或 `}`——才参与配对，散文 / 代码里的花括号与孤立
   引号不干扰扫描；跳过字符串内花括号），只取最外层、长成 verdict 形状（布尔 `pass` 且带
   `reasons` 数组或仅此一键；顶层或嵌套 `verdict`）的对象为候选；落在未闭合的 JSON 对象之后
   的片段是被截断外层的子值，不是候选；剔除与注入验证者任务全文（目标任务文本、目标产出
   简报、依赖摘要、通过标准、升级答案）中 JSON 完全相同的候选——引用集用 `all_json_values`
   相位自愈地扫描（`ScanMode::PhaseRobust`：字符串状态里遇到紧跟着键的 `{"`——引号之后是键的
   首字符，非空白、非 `,` `}` `]` `:`——也当对象起点：合法 JSON 字符串里不可能有未转义的 `"`，
   而收尾引号之后又不会紧跟键的首字符，所以它证明字符串状态已和正文错位；以 `{` 结尾的合法
   字符串（`"impl Foo {"`，其后的 `"` 是收尾引号）不算，否则那个合法对象会整个丢出引用集、被
   中和改坏、还各白耗两次重扫。于是正文里每个字面存在的、首键以普通字符开头的合法对象都
   单独配对入集，与前面的残骸无关，被裁短的依赖摘要留下的 `{"summary": "…` 不会遮住后面的
   目标自述；首键以空白 / 上述标点开头的对象是已知盲区，退化为严格处理；验证者输出那一侧的
   候选扫描仍是严格模式，最外层 / 截断语义不变）；遇到未闭合对象仍从它之后重扫，最多 64 次——文本是步骤产出可控的，塞满
   未闭合对象会把重扫拖成平方级 CPU——超限时引用集建不起来，fail-closed 而不是悄悄缩小引用集放行：这不是
   验证者的输出问题，所以在【派发验证者之前】就按同一对文本预检（`VerifierInputs::
   reference_set_failure`，简报与任务文本分开查、诊断按归因措辞），建不起来的验证者根本
   不上场——目标标 Failed、写 `step_unverified {reason: "reference set unavailable", source,
   detail}` 与新的 `step_finished failed`（resume 重跑目标、旧简报作废；被这样判失败的继承
   目标不再计入报告头的「继承 N 步产出，未重跑」——报告头按执行完时仍成立的集合算，
   `plan_seeded.inherited` 保留装载时的数字），验证者 skipped，不计它的「连续两次」预算；
   `classify_verdict` 里同一判定只是兜底。现实中的触发源已被堵住：`verification_brief`
   把摘要、每条列表项、整份简报截断点各自留下的未闭合 JSON 样式 `{` 中和成全角 `｛`
   （`neutralize_unclosed_json`，与引用集同一种扫描模式：正文照旧可读、不再是对象起点；
   中和只改入栈点、不改引用集来源——简报里字面存在的合法对象（首键以普通字符开头）照样被
   自愈扫描收进引用集，所以中和不会让这样的对象掉出引用集，也不会把以 `{` 结尾的合法字符串
   当残骸改坏；一段里残骸超过 64 个才退回裁切，且只裁这一段——摘要
   里一个残缺日志片段不会把后面的自报列表切没；resume 时记录里没有简报的兜底摘要同样中和），
   一条超长 JSON 列表项不再各留一个 `{"`；`all_json_values`
   的去重按哈希做，重扫不再因 `contains` 线性比对退化），整个对象相同或其内嵌 `verdict`
   相同都算引用，不是结论；两道
   截断闸门故意不对称：验证者输出里的未闭合对象判 `Malformed`（它的输出应当完整），任务文本
   里的未闭合对象只是重扫起点；候选的 `pass` 必须全部一致，一致则合并去重
   `reasons`（空串 / `...` 等占位符不算）；pass=true 必须至少剩一条实质 reason。没有候选
   / 只剩引用（除非引用一致为 fail 且出自目标产出摘要，此时保留其 reasons）/ 候选矛盾 /
   解析失败 / `pass` 非布尔 / pass 无证据 / 疑似截断（未闭合的 JSON 对象吞掉了候选、或尾部
   带 max_output_chars 截断标记）一律 `Malformed`——验证者输出不规范等同「未能证明」，
   但责任在验证者：不计 verdict 轮次、目标不重跑，诊断注入验证者的重跑任务
   （`## Verdict feedback`），与基础设施失败共用「连续两次」预算（`verifier_malformed
   {attempt}`），再失败双双 Failed、目标摘要标「未验证」（标签按两次的实际组合措辞：
   连续两次执行失败 / 连续两次未给出有效 verdict / 一次执行失败、一次输出不规范）。
   验证者自身也记 `step_finished {verdict, target, round, exhausted}`；验证者 is_error
   / cancelled 记 `verifier_errored {attempt}`、不计轮次、重试一次；验证者不可用时目标记
   `step_unverified {step, verifier, reason}`——reason 是耗尽标签或 `verifier unavailable`
   （验证者因依赖未成功被跳过 / 自身失败），目标不写 step_finished，产出留在
   awaiting_verdict 记录里，resume 只补跑验证者；验证者在等编排者应答时目标标 Skipped 而非
   Failed。第三种 reason 是上面说的 `reference set unavailable`（派发前预检），它与前两种
   相反：目标【写】新的 `step_finished failed`、产出作废、resume 重跑目标；事件多带
   `source`（`brief` = 目标简报建不起引用集，重跑目标会变；`task_text` = 只有验证任务文本
   建不起来——残骸来自目标任务文本 / 依赖摘要 / 升级答案，重跑目标改变不了，报告与诊断明说
   「需先修改任务文本或依赖再 resume」而不许诺重跑能修好）与 `detail`（诊断全文，journal
   自解释）。步骤因未应答的升级提问被挂起记 `step_skipped {reason: "awaiting_escalation",
   question_id}`；步骤实时等到答案记 `escalation_delivered`，报告里把「有应答但步骤没收到」
   的提问单独列为 answered_late（无需再次应答，直接 resume）。
3. **失败回路**：fail → `verdict_failed` 事件（含轮次与 reasons）→ 失败摘要注入目标
   任务文本（`## Verification feedback`）→ 目标与验证者双双回 Pending 重跑；轮次达
   `max_verdict_rounds`（默认 2，硬顶 4）→ 双双 Failed，依赖方按既有策略传染 skipped，
   报告的对抗环行带「最后原因」（验证者在第一轮就耗尽预算、轮次为 0 的目标也列出，标
   「未进入 verdict 轮次」；验证者被跳过 / 在等应答导致目标 Failed / Skipped 的对也列出）。
   pass → `verdict_passed` 事件，双双 Succeeded。
4. **预算持久**：journal 的 verdict 事件经 `seed_verdict_replay` 在 resume 时重建轮次
   计数——预算跨重启续算；未耗尽的失败恢复为「继续对抗」，已耗尽标 Failed 不自动加码
   （带更大的 `max_verdict_rounds` 或更换 `verified_by` 才续跑，见 journal 断点续跑一节）。
   轮次按【对抗周期】计：一次 pass 收口一个周期，目标之后因编排者答案重跑开启新周期、
   预算从零起算——否则「fail、pass、答案触发重跑、再 fail」会在新产出只被否决一次时就耗尽
   预算。重跑发生在上一次执行里时旧 pass 已 `superseded`，`seed_verdict_replay` 不入账它的
   轮次；答案在【本次】执行的装载期才触发重跑时（旧 pass 尚未被取代），执行器在升级重放后
   把该目标的轮次清零。同理，验证者自己被答案（或缺了 step_finished 的 verdict 记录）拉回
   Pending、而目标按仍有效的 pass 是 Succeeded 时，目标回 Verifying 让验证者带新前提重验，
   轮次同样清零（否则波次只挑目标在 Verifying 的验证者，这一对会卡成「调度不变量被破坏」）。
   周期内的 fail 之后被答案触发重跑仍续算（没有 pass 收口）。轮次自增用饱和加法：手工改过
   的 journal 把轮次写到 u32 顶也只会停在顶上按耗尽处理，不会回绕成 0 重置预算。

journal 新事件：`verdict_passed {verifier, target, round}` /
`verdict_failed {verifier, target, round, reasons[, exhausted]}` /
`verifier_errored {verifier, target, attempt, error}` /
`verifier_malformed {verifier, target, attempt, error}` /
`step_unverified {step, verifier, reason[, source, detail]}`；验证者自己的 `step_finished` 带
`{verdict, target, round, exhausted}`。resume 只重放 `verdict_*`（轮次预算）与
`step_finished`（终态）；`verifier_errored` / `verifier_malformed` 是诊断记录，不重放
——验证者的「连续两次」预算在每次执行内重新计数。

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

`TurnManager::handle_compact`：手动立即压缩当前会话——与自动压缩共用摘要/fallback/boundary
管线，但忽略阈值与防抖（用户显式意图优先）；有活动 turn 时拒绝（避免与会话原子追加交错）——
同 chat 上的 cron 轮次既不加载也不持久化会话历史，不算（`ActiveTurn::touches_history`，
spawn 时按消息来源登记，而不是从键的形状推断：旧式无 job id 的 cron 消息键与普通 turn 同形）。
并发口径：在独立任务上运行、不关静默门（摘要可达一分钟，关门会让其它会话也开不了 turn），
但像 turn 一样占一个门槽位并以 `cron_job_id = "/compact"` 键登记进 active map——`/clear`
等生命周期操作在压缩期间拿不到门、`/cancel` 能中止它（摘要已回来、boundary 未落库的窗口里
到达也仍兑现「历史不动」）、同会话第二个 `/compact` 被拒、摘要调用共享 `execution_limit`；
落库前再查一次本会话有无其它活动 turn。手动压缩没有内存视图：落库失败什么都没发生，
usage 锚点既不作废也不挂起（`persist_compaction(.., memory_switched = false)`），只有落库
成功才作废旧锚点。桌面端「恢复转录」（`chat_history`）与 `chat_export` / `/export` 同源读
transcript 的末尾（`load_transcript_tail`），不是 provider 可见的压缩视图——压缩点处不会无提示
地缺一段。

### Layer 5a：主 turn 检查点（durable usage anchor）

`HistoryLoader::record_usage_anchor` 在每次成功 LLM 调用后把 `{total_tokens,
anchor_message_id}`（请求里最后一条已持久化消息的 `Message.id`）写入 `agent/state.sqlite`
的 `usage_anchor` 表（/clear 随会话一并清除；任何压缩都会使锚点失效）。进程重启后
内存锚点清零、估算退化为纯启发式——`anchored_estimate` 现在在内存 miss 时从表里恢复
（每 key 只读库一次，含负缓存），按 id 在当前请求里定位锚点消息，不再依赖内容哈希。
这配合既有的 ADR-0004 aborted-chain 占位（崩溃后模型知道干到哪），构成主 turn 的断点
续接语义。

### Layer 5b：`session_handoff` 跨会话交接工具

`session_handoff(channel, chat_id, query, max_chars)`（`src/session_handoff.rs`）：把另一段
会话压缩成有界交接素材——query 相关的更早消息 + 最近 8 条原文；防注入头部与
`session_search` 同纪律（历史素材 ≠ 指令）。

- **预算语义**：`max_chars` 默认 24K、区间 [4K, 48K]（对齐 ZCode ReadSessionContext）。
  单条渲染上限 = 剩余预算 / (8 条尾部 + 4 个预留槽位)，夹在 [200, 4000]；尾部原文先占
  预算，query 相关消息按「命中 token 数 desc、新近度 desc」逐条填充到剩余预算放不下为止
  （硬顶 40 条只是安全上限）。预算越大带回越多——24K ≈ 2K/条、48K ≈ 4K/条。
- **分词**：移植 ZCode `tokenizeQuery`——ASCII 词串 `[a-z0-9_./-]+` 与汉字串各成 token，
  汉字串再补二字 bigram，按命中的不同 token 数计分（「部署nginx遇到502」不再是一个词）。
- **源会话定位**：`channel` 缺省取当前 turn 的 channel（`CURRENT_CHAT` task-local，作用域
  外回落 `cli`）；活跃与归档会话都在 `state.sqlite` 的 `session` 表里按 (channel, chat_id)
  查找（归档只是 `archived_at` 标记），读取完整 transcript（含已被压缩折叠的消息）；
  消息数超过 50,000 拒绝加载。跨会话读取能力与 `session_search` 一致，源 ≠ 当前
  会话时记 info 日志。

## 配置项汇总（`AgentOptions`，默认全开）

| 字段 | 默认 | 作用 |
|---|---|---|
| `in_turn_compression_enabled` | true | Layer 1 轮内压缩总开关 |
| `background_wake_enabled` | true | Layer 4 合批唤醒开关 |
| `background_wake_debounce` | 3s | Layer 4 合批窗口 |
| `subagent_auto_background` | 600s | Layer 2a 前台自动转后台预算（0=关闭） |
| `plan_escalation_wait_secs` | 0 | Layer 3 `escalate` 阻塞等待编排者实时答案的秒数（0=提交即挂起，不等待） |
| `tool_result_artifact_enabled` | true | 工具结果落盘 + 引用句柄开关（第十二轮；关闭退回原样进上下文） |
| `tool_result_artifact_bytes` | 32KB | 工具结果落盘触发预算 |

（CLI 未新增 toml 配置项，`assemble_core` 内部按默认值构造；这些选项只被 `AgentToolRuntime` 读取，接入配置时须写进传给它的 `runtime_opts`，而不是 AgentLoop 的 `opts`。）

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
- `session_handoff.rs::tests`：query 相关选取（命中条距末尾 ≥ 9、只经相关分支入选）+
  尾部保留 + 防注入头部 + 空/缺参报错 + 中文无空格 query 分词命中 + 预算随 `max_chars`
  伸缩（4K/48K）+ 归档回退 + 源会话 50,000 条消息上限守卫 + 默认 channel 取当前 turn。

## 第四轮测试

- `plan.rs::parse_verdict_extracts_pass_and_reasons`：正文内嵌 JSON 提取、fail 无
  reasons 兜底、提取/解析/缺字段一律判 fail。
- `plan.rs::parse_verdict_scans_balanced_objects_and_fails_closed` /
  `parse_verdict_ignores_nested_rows_and_echoed_target_json` /
  `classify_verdict_fails_closed_on_truncation_and_nested_echo`：handoff 与 verdict 并排、
  代码花括号、嵌套 `verdict`、`pass` 非布尔、候选矛盾判 fail、一致合并 reasons；嵌套的
  检查行 / 测试结果行不参与裁决，与注入任务相同的 JSON 视为引用（整个对象或内嵌 `verdict`
  相同都算；只剩引用 → `Malformed`，引用一致为 fail 则保留其 reasons）；外层对象未闭合
  （疑似截断）时其内的 pass 不顶替 verdict、尾部带 max_output_chars 截断标记一律 `Malformed`；
  完整 verdict 之后的截断 handoff 不影响采纳。
- `plan.rs::parse_steps_rejects_invalid_verification_relations`：自引用 / 未知验证者 /
  链式验证 / 验证者 depends_on 被验证者（死锁）全部拒绝。
- `plan.rs::verdict_replay_seeds_resume_state`：未耗尽失败恢复对抗、耗尽标 Failed、
  通过维持继承。
- `agent_tools/tests.rs::verified_plan_reruns_target_until_verdict_passes`：端到端
  先败后过（按请求分流的脚本 provider：验证者第 1 次 fail verdict、第 2 次 pass）——报告
  「第 2 轮通过」，失败原因只注入目标的重跑任务，journal 含 verdict_failed ×1 +
  verdict_passed ×1 + awaiting_verdict 标记。
- `verified_plan_exhausts_rounds_and_marks_failed`：验证者恒给出 fail verdict → 轮次
  耗尽 → 目标 Failed，报告带「最后原因」。
- `malformed_verdict_reruns_only_the_verifier_with_feedback` /
  `malformed_verdict_twice_fails_pair_without_counting_rounds`：验证者输出里没有可接受的
  verdict → `verifier_malformed {attempt}`、不计轮次、目标不重跑，诊断注入验证者的重跑任务
  （`## Verdict feedback`）；连续两次 → 双双 Failed、目标摘要标「未验证」，无 verdict_* 事件。
- `echoed_target_self_claim_is_not_accepted_as_verdict_end_to_end`：目标产出自带
  `{"pass": true, ...}` 自述，验证者第 1 次原样复述 → 全链路（产出摘要 → 注入验证任务 →
  引用剔除）拒收为 `verifier_malformed`（「完全相同」），目标不重跑；第 2 次给出自己的证据
  → 第 1 轮通过。
- `plan.rs::parse_verdict_extracts_pass_and_reasons`（补）：pass=true 无 reasons / 空串 /
  `...` 占位 → `Malformed`（「证据」）；占位符混在真实证据里只剔除占位符。
- `plan.rs::parse_verdict_scans_balanced_objects_and_fails_closed`（补）：散文花括号 +
  同行孤立引号后的 verdict 仍提取；目标摘要里同样写法的 JSON 被验证者复述时识别为引用。
- `plan.rs::classify_verdict_fails_closed_on_truncation_and_nested_echo`（补）：引用一致为
  fail 的捷径只认目标产出摘要，同一 JSON 只出现在依赖摘要里 → `Malformed`。
- `plan.rs::verifier_problem_label_reflects_the_attempt_mix`：预算耗尽标签按两次尝试的
  组合措辞（纯执行失败 / 纯格式 / 混合）。
- `plan.rs::classify_verdict_sees_echo_sources_hidden_behind_a_truncated_dependency_summary`：
  任务文本里被裁短的依赖摘要留下未闭合对象，其后的目标自述仍进引用集（`all_json_values`
  重同步扫描），复述自述 → `Malformed`（「完全相同」）；验证者自己的不同结论照常采纳。
- `plan.rs::verification_brief_lists_self_reported_claims_within_bounds`：简报 = 摘要 +
  自报证据 / 产物 / 验证 / 风险，空列表不出现、空白条目过滤、条数 / 单条上限；四个列表装满
  才触及 12000 的总量上限，截断点切开的那个对象被中和、简报干净、引用集建得起来。
- `plan.rs::verification_brief_keeps_lists_when_summary_has_unclosed_json`：摘要里的残缺 JSON
  只中和那个 `{`，后文与自报列表保留；残骸超过 64 个才退回裁切且只裁摘要。
- `plan.rs::reference_set_survives_quote_phase_inversion_after_neutralisation`：残骸之后引号
  相位反转的文本，中和前后 verdict 都在引用集里（自愈扫描）；严格扫描对照吞掉它；`{}` /
  `{ x` 不入栈、`\{"` 挡不住字面对象；验证者输出侧仍严格。
- `plan.rs::string_ending_in_brace_is_not_a_phase_inversion`：以 `{` 结尾的合法字符串
  （`"impl Foo {"`、`"a{"`、多行排版、以 `{` 结尾的键）不是相位错位：对象照常入集、零重扫、
  中和恒等；验证者原样复述这样的自述仍判「完全相同」；33 条这样的 hunk 不触发预检误报；
  相位错位时空键仍被自愈。
- `plan.rs::verdict_replay_starts_a_fresh_budget_after_a_superseded_pass`：最后一条是被
  取代的 pass → 轮次不带入新周期；被取代的 fail → 续算。
- `plan.rs::stalled_pairs_are_resolved_instead_of_hanging`：验证者被跳过 / 自身失败 →
  Verifying 目标 Failed 并写入「验证者不可用」反馈（措辞区分两种原因）、`step_unverified`；
  验证者在等应答 → 目标 Skipped 且无失败反馈；目标失败 → Pending 验证者 Skipped。
- `plan.rs::load_plan_journal_recovers_verdicts_from_verifier_step_finished`：`verdict_*`
  事件写丢时从验证者的 `step_finished {verdict, target, round}` 补回同轮 verdict。
- `agent_tools/tests.rs::verifier_infra_errors_retry_once_then_fail_pair_and_resume_reruns_only_verifier`：
  验证者 is_error 两次 → `verifier_errored ×2`、不计轮次、双双 Failed；resume 只重跑验证者，
  目标产出复用。
- `verifier_error_then_malformed_reports_the_mixed_label`：一次执行失败 + 一次输出不规范 →
  混合标签、`verifier_errored` + `verifier_malformed` 各一、`step_unverified` reason 同标签。
- `verifier_steps_run_with_shell_access_and_see_the_target_brief`：编码类目标走
  candidate_patch，验证者不套 Shared-file Isolation；验证者 prompt 含目标自报的 validation /
  artifacts / evidence 小节，与 `step_finished {verification_brief}` 落盘的是同一份。
- `plan.rs::parse_steps_rejects_shared_verifier_and_implicit_cycles` /
  `parse_steps_validates_verified_by_and_pass_condition_fields`：一个验证者验证两个目标、
  经 depends_on 绕回的隐式环（含「验证者依赖『依赖目标的步骤』」）、`verified_by` 非法 id
  （错误带 `steps[idx]` 前缀）、没有 `verified_by` 的 `pass_condition`、超长 `pass_condition`
  全部拒绝。
- `plan.rs::verdict_replay_matches_verifier_and_ignores_superseded_verdicts`：verdict 按
  (验证者, 目标) 匹配——amend 换了验证者旧 verdict 不复用；被 `superseded` 的 verdict 不
  决定起点（目标有产出则 Verifying 只补跑验证者）。
- `plan.rs::load_plan_journal_marks_superseded_verdicts_and_skips_malformed_events`：目标
  在 verdict 之后又有 step_finished → `superseded`；round 溢出 u32 / 字段残缺的事件跳过。
- `plan.rs::load_plan_journal_verdict_backup_dedupes_only_within_the_current_cycle`：新周期
  第 1 轮丢了 `verdict_failed` 时，验证者 step_finished 的备份不被旧周期的同号记录吃掉，
  重放按新周期第 1 轮续对抗。
- `plan.rs::bound_verdict_reasons_caps_item_and_total_length`：reasons 单条 / 总量上限。
- `plan.rs::open_escalations_distinguish_late_answers_from_delivered_and_seeded_ones`：
  本代前应答 → 不列；本代内应答且步骤实时收到（`escalation_delivered`）→ 不列；装载注入过
  （`injected`）→ 不列；其余 → `answered_late`。
- `plan.rs::escalation_seeding_parks_multi_question_steps_regardless_of_order` /
  `load_plan_journal_orders_escalations_against_step_outcomes`：同一步骤既有已解答又有未决
  提问 → 无论顺序都挂起；应答 / 提问相对步骤最后一次 succeeded 的先后决定已消费 / 降级 /
  强制重跑。
- `agent_tools/tests.rs::concurrent_resume_of_same_plan_is_rejected`：同一 plan_id 并发
  resume 被拒（`ActiveRunGuard`）。
- `escalating_step_is_parked_then_reruns_with_answer`：首轮提问未得应答 → skipped
  （awaiting_escalation）、依赖方不消费挂起的 handoff；应答后 resume 带答案重跑。
- `answer_forced_rerun_after_a_pass_starts_a_fresh_verdict_budget`：journal 里 fail(1) →
  pass(2) 收口后编排者应答了目标的提问 → 本次 resume 强制重跑目标，首次被否决按新周期
  第 1 轮（不耗尽）、再跑通过为第 2 轮；验证者 prompt 含目标收到的编排者答复与目标任务
  全文。
- `exhausted_verified_pair_stays_failed_on_resume_until_the_budget_is_raised`：预算耗尽的
  对子原样 resume 不碰 provider、报告写明加码方式；带更大的 `max_verdict_rounds` resume 才
  续算重跑。
- `plan.rs::escalating_verifier_parks_target_then_verifies_with_answer_on_resume`：验证者
  自己 escalate 的端到端链路——目标按【待验证】skipped、报告列「等待验证者的编排者应答」、
  目标产出留在 awaiting_verdict 记录；答案未到就 resume → 零次模型调用、目标继承进
  Verifying 后再次按待验证 skipped（`plan_seeded` 记 1 步继承、1 条待应答）；应答后 resume
  只补跑验证者（带答案与继承的简报），verdict 通过记第 1 轮。
- `unbuildable_reference_set_fails_the_target_without_dispatching_the_verifier`：目标任务
  文本塞满未闭合 JSON → 验证者未派发（provider 只被调 1 次）、目标 Failed 带
  `step_unverified {reason: "reference set unavailable", source: "task_text", detail}` 与新的
  failed 终态、验证者 skipped 且没有任何 verifier_* 记录；归因到任务文本的措辞（「需先修改
  任务文本或依赖再 resume」，不出现「resume 将重跑本步骤」）；resume 重跑的是目标，诊断
  不冒充 verdict 反馈。
- `reference_set_precheck_failure_does_not_block_the_rest_of_the_wave`：同一波里预检拦下
  验证者、其余步骤照常派发（base / impl / other 各一次调用），目标的依赖方下一波按
  `dependency 'impl' did not succeed` 跳过；事件顺序 拦截 → other 完成 → dep 跳过。
- `precheck_failure_on_an_inherited_target_discards_its_brief_and_inherited_count`：继承的
  Verifying 目标在 resume 时被预检判失败（零调用）：新 failed 终态不带简报、报告头不再
  写「继承 N 步产出」而 `plan_seeded.inherited` 仍是 1；再 resume 目标回 Pending 重跑。
- `verifier_answered_after_a_pass_reverifies_instead_of_stalling`：手工 journal 里验证者在
  pass 收口后又得到应答 → 目标回 Verifying 重验（新周期第 1 轮），而不是 (Succeeded, Pending)
  卡成 stalled。
- `escalate_inside_plan_context_journals_and_parks`：工具侧把 `context` 一并落 journal，超长
  佐证按文本上限裁（文本截断后缀，不套输出截断标记）。
- `classify_verdict_sees_echo_sources_hidden_behind_a_truncated_dependency_summary`：重扫
  上限——塞满未闭合对象的任务文本 fail-closed 为 `Malformed`，上限内照常重同步。

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
- `sandbox_pins_nondeterminism`：Date.now/Math.random 钉死（`[0,0.5]`）经 `__result`
  回传验证（async IIFE + Promise 全链路）；动态 `import()` 的拒绝由
  `check_tests::dynamic_import_is_rejected_statically` 在提交期覆盖。
- `agent_tools/tests.rs::workflow_serial_await_and_zero_cost_replay`：端到端
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
- `rewind_ids_are_unique_and_monotonic`：回滚点 id 严格递增。
- `unrecorded_edit_between_checkpoints_breaks_the_chain_and_refuses_restore`：两次 checkpoint 之间的未记录改动断开校验链 → 拒绝；回滚标记重锚后不误判。

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

## 第十二轮移植：并行上限 / artifact 落盘 / 会话 fork / AST 分析 / UI 三件套

五项一次落地（对应「后续工作」1-4 全部完成 + 桌面/安卓 UI）：

1. **并行波次上限提升**：`max_concurrent_turns` 缺省 6→8（`AgentOptions` / CLI 配置同源）；
   PlanRun `MAX_PARALLEL_STEPS` 4→8（缺省 2→4），WorkflowRun `max_parallel_asks` 硬顶
   8→16（缺省 4→8）。压测三件套（agent_tools/tests.rs）：
   `stagger_holds_at_eight_way_concurrent_fanout`（8 路并发错峰窗口仍成立）、
   `plan_wave_of_eight_runs_concurrently_at_raised_cap`（8 许可信号量整波同时在飞，
   峰值并发断言 = 8）、`workflow_sixteen_ask_fanout_at_raised_cap`（16 ask 扇出聚合正确）。
2. **工具结果落盘 + 引用句柄**（`turn/artifact.rs`）：三个结果组装点（parallel_safe
   快路径 / Agent 并发路径 / 串行路径）在脱敏后统一过 `spill_if_over_budget`——超预算
   （`tool_result_artifact_bytes`，默认 32KB）时全文（脱敏后）原子写
   `agent/artifacts/<会话键>/<回滚点id>-<工具名>.txt`（会话键经白名单清洗；子代理按
   `subagent-<channel>` 归档），模型收到「路径 + 4KB 头尾预览 + read 分页指引」句柄；
   落盘失败静默回退原内容（工具结果语义不变）。与 microcompact 互补：超预算结果从进入
   上下文的第一刻起就是小句柄。已知边界：shell 等工具内层的头尾截断（64KB 流式预算）
   先于本层发生，落盘存的是工具返回的全部而非命令原始输出。
3. **会话历史任意锚点 fork**（ZCode `forkStableConversationAtMessage` 同源语义）：
   - store：`SessionStore::fork_session(channel, chat, anchor, new_chat)` 单事务把
     【活动视图】前缀（≤ 锚点，按视图序——boundary 行 id 最大，不能按 id 大小截断）
     原样复制进新会话；boundary 摘要行降级为普通行（旧 meta 锚点引用源会话 id，带过去
     即坏）；血缘（父会话 + 锚点）写同一事务的 kv（`session-fork/<channel>/<id>`）。
     锚点不在活动视图（被压缩折叠）→ 明确报错（ZCode Unavailable 策略同判）。
   - 运行时：`/fork [消息 id]` 斜杠命令（无参列最近 15 条真实用户消息锚点；新
     chat_id `<orig>-fork<hex>` 撞名加计数）。fork 不持静默门：只读源 + 新建，
     与运行中 turn 互不干扰。
   - UI（桌面 + 安卓 APK 同一 Tauri 前端）：`chat_history` 带消息行 id；消息下方
     「⑂ 分叉」按钮（桌面内联）/ 长按菜单「从这里分叉到新会话」（移动端）→
     `chat_fork` 命令 → 切到新会话。直播条目无行 id 时按 role+content 从历史回查。
4. **工作流 AST 级分析**（`agent_tools/workflow_ast.rs`，oxc 0.116——swc 生态冲突的
   纯 Rust 替代；rust-version 1.91 兼容线内最新）：
   - 解析形态：脚本包进 `(async function __wf__(){ … })`（顶层 await/return 合法，
     与执行引擎同一语义），module+TS 模式解析；行号减一、偏移去头。
   - **AST 依赖图**：ask 调用点按语法识别（字符串/注释里的 `.ask(` 不再误报）；
     receiver 经句柄绑定（`const h = agent("x")`）或 inline `agent("x").ask()` 解析；
     `Promise.all` 数组元素（直接 ask **或持有 ask promise 的标识符**——变量提升
     扇出）合并成并行波；波间「前波出口 → 后波入口」全连边、Kahn 分层不变。
     oxc 解析失败降级回第十一轮文本解析（分析永不阻塞提交）。
   - **`.ask<T>` 泛型到 schema 的编译**：`.ask<` 预扫命中才走 oxc；内联类型
     （string/number/boolean/null/字面量/联合/数组 T[]与 Array<T>/对象字面量含可选
     属性/顶层 type 别名无泛型）编译为 JSON Schema，调用点重写为
     `h.askJson(question, <schema>)` 后交给 QuickJS（执行引擎零改动）；不支持的类型
     给出行列诊断提交期拒绝。script hash 仍按【作者原文】（resume 逐字节一致语义
     不变，重写确定性复现）；askJson 的 Rust 校验器扩展 const/enum/anyOf。
     tsc 可选类型检查与 lint 跑重写后的脚本（exec script）。
6. **/goal 会话长期目标**：`/goal <目标文本>` 设定（kv `session-goal/<channel>/<chat_id>`，
   按会话隔离）、`/goal` 查看、`/goal clear|done` 收尾。设定后本会话【每次推理的尾部
   上下文】注入 `[Session goal]` 提醒块（`TurnManager::goal_reminder_block`）：目标原文
   + 持续工作 + 收尾前按证据逐项核对 + 不把计划当完成 + 提醒用户收尾——对齐 ZCode
   goal reminder 的关键语义；RClaw 无运行时完成校验器，完成判定 = 模型按证据自评 +
   用户 `/goal clear`（提醒文案明说）。
7. **PlanRun 暂停 / 继续**：kv `plan-pause/<plan_id>` 标志 + 执行器波次边界协作轮询
   （300ms）。暂停语义 = 当前在飞步骤跑完、后续波次不派发（不是硬中断——与
   `/cancel` 的「终止」正交）；`plan_paused` / `plan_unpaused` journal 事件记录暂停态
   的翻转（**不是** `plan_resumed`——那是 resume 新一代执行的代际锚点，二者不可混用），
   kv 标志才是执行器与 `plan_status` 投影共同的权威；桌面 `plan_pause(plan_id, paused)`
   命令（只对正在执行的计划接受暂停）+ 流程步骤卡片展开时的
   ⏸/▶ 按钮（乐观翻转 + 失败回滚）。测试 `paused_plan_waits_at_wave_boundary_and_
   resumes`：延迟 provider 拉开时序，证明暂停（而非错峰）挡住了后续波次，恢复后
   全部收尾、事件齐备。
5. **桌面/安卓 UI**（`rclaw-desktop` 命令 + React 前端，两宿主同代码）：
   - **流程步骤 + todo**：`plan_status` 命令只读投影 PlanRun journal
     （plan_started 事件补记步骤任务文本 ≤80 字符，桌面解析兼容旧 id 数组形态）；
     消息流输入区上方「📋 流程步骤」可折叠卡片（✓/✗/⤼/⏳ + 步骤摘要，未收尾默认
     展开）；会话面板（侧栏）顶部紧凑 todo 区（最近未收尾计划）。
   - **排队消息**：运行期间发送不再被丢弃——进入输入框上方的排队队列（⏳ chip），
     可删除 / 编辑（回填草稿）/ 「立即」（绕过 busy 直接投递，运行时按既有插队语义
     并入当前 turn）；turn 收尾自动按序发出；桌面 Enter 入队、移动端「⏳ 排队」按钮。
   - **分叉**：见第 3 项。

**与既有机制的交互**（融洽性，见 review 记录）：artifact 落盘在脱敏之后（磁盘副本
无敏感明文）、不触碰附件与媒体卡；fork 不动源会话（append-only），与压缩 boundary /
rewind checkpoint 语义正交；排队消息走既有 inbound 插队通道（同会话串行不变）；
plan_status 是 journal 的只读投影，不与执行器写路径竞争。

## 第十二轮实现与 ZCode 源码的对照核对（review 记录）

逐项对照 `zcode-ref`（v3.15.1）源码核对本轮移植的语义，结论：无正确性错误；三处
为已声明的设计取舍。

| 功能 | ZCode 源码位置 | 核对结论 |
|---|---|---|
| artifact 落盘 | `core/tool/executor/result-serialization.ts`（>maxModelBytes 触发、写失败回退截断、2KB 预览 + Read 读回指引） | RClaw 同构：> 阈值触发、失败静默回退原内容、4KB 头尾预览 + read 分页指引；落盘在脱敏【后】（ZCode 的 redact 在 handler 内，RClaw 磁盘副本同样无敏感明文，更保守）；预览用头尾截断而非纯头部是自家 shell 截断哲学的延续（诊断在尾部） |
| fork 复制源 | `methods/session-fork.ts:938`「复制源必须是 UI transcript 语义，activeSessionMessages 会按 compact boundary 截掉旧 worklog」 | **已声明差异**：ZCode 保留 compact 折叠前的完整可见转录；RClaw 复制【活动视图前缀】（boundary 摘要降级为普通行）——子会话更轻，完整原文仍在源会话 transcript（append-only 不丢）。锚点校验（不在视图 → 拒绝）与 ZCode `evaluateRewindTarget` 的 Unavailable 策略同判；ZCode 有命令级幂等（同 command 返回同一 child），RClaw 每次显式分叉生成新分支（多次点击 = 多分支，符合按钮语义） |
| typed ask | `dynamic-workflow/src/schema/{synthesize,emit}.ts`（any 拒绝、unknown 不校验、字面量→const/enum） | 语义一致；ZCode 有 checker 级解析（别名/泛型实例化/递归 $defs/`additionalProperties:false`），RClaw 为语法层子集 + 深度上限（差异已在「后续工作」声明为类型面扩展项）。untyped `ask()` 不校验（ZCode `askResultType` 对纯 string 返回 undefined 同判） |
| 依赖图 | `dynamic-workflow/src/analysis/{sites,taint,graph}.ts` | RClaw 无 checker/taint（语法形状 + 绑定解析），边语义（相邻波全连 + join 内无边 + Kahn 分层）与 ZCode 站点/上下文边的保守近似一致；ZCode 的同 actor FIFO 保序边被串行链边蕴含 |
| /goal | ZCode 每 turn 的 goal reminder（objective + budget + completion audit + 不自行宣告完成） | RClaw 尾注注入对齐全部关键要素；ZCode 有运行时 completion verifier，RClaw 靠模型按证据自评 + 用户 `/goal clear`（文档已声明） |
| 暂停/继续 | ZCode TaskStop/Resume（取消 + 快照续跑） | 正交组合：RClaw `resume_plan_id`（崩溃恢复，等价 ZCode resume）+ 波次边界暂停（软等待，/cancel 硬终止语义不变）——三层语义清晰不混淆 |
| 排队消息 / todo 面板 | ZCode 输入排队 + send now；TodoWrite 面板 | RClaw 本地队列 + 「立即」走既有插队通道；todo = PlanRun journal 投影（RClaw 无模型侧 TodoWrite 工具，流程步骤即 todo 数据源） |

修复的核对产物：`plan_status` 投影里 `plan_resumed` 曾被 `plan_started | plan_resumed`
合并分支吞掉（clippy unreachable_patterns 揪出）——暂停后恢复的投影态翻不回 false，
已拆分为独立分支。

## 第十三轮：后续项清账（除 7 CLI 会话切换 / 9 全仓 fmt 外全部完成）

1. **artifact 清理策略 ✅**：会话键规则单一事实源下沉到 `store::session::
   artifact_chat_key`（runtime 落盘与 store 清理共用）；`delete_session` 级联删除该会话
   的 `artifacts/<会话键>/`（best-effort，失败只告警），`/clear`（clear_all_sessions）
   清空 artifacts 根。测试：`delete_session_cascades_artifact_dir_and_clear_wipes_root`。
2. **fork 文件联动 ✅**：schema v2（`file_checkpoint.anchor_message_id`，迁移
   v1→v2 保数据旧行 NULL）；工具链同事务写入的 checkpoint 锚定 tool_result 消息行；
   `rewind::restore_to_message`（回滚集 = 锚点之后的写入，锚点自身 checkpoint 作
   校验链起点、其写入保留——漏掉链起点会让锚点与其后写入之间的未记录改动漏检，
   测试曾以此失败形态验证）；入口 `/fork <id> files`（与 full 可组合；与 /rewind 同
   纪律持静默门）、桌面长按菜单「分叉并回滚文件」走命令文本。旧行（迁移前）归属
   未知不参与，报告注明。
3. **`.ask<T>` 类型面扩展 ✅**：tuple（prefixItems + min/maxItems，校验器同步支持
   按位与混合变长尾）、交叉类型（对象合并 properties/required 并集）、泛型别名
   （`type Box<T> = …` 单/多参实例化，语法层代入栈，嵌套 `Box<Box<string>>` 支持）；
   tsc 类型检查切换到【作者原文】（facade.d.ts 已声明 ask<T>，原文直接暴露类型错误；
   语法探针与 lint 仍查重写后的 exec script）。JSDoc 约束不做（oxc 需保留注释做
   span 关联，成本高于收益——ZCode jsdoc.ts 对位能力暂缺，在此声明）。
4. **step_started 事件 ✅**：执行器派发处写入；投影把「有 started 无终态」标
   running（重跑重记按序生效）；卡片与侧栏 todo 加运行中图标。
5. **/goal 完成审计 ✅**：`goal_audit_enabled`（默认开）；有活动目标的会话每次成功
   收尾后异步跑一次无工具小推理（max_tokens 200），按「可核对证据」标准判定
   `{goal_met, evidence_summary}`；达成 → 清 goal + 系统回执；未达成 / 解析失败 /
   调用失败 → 目标保持（`parse_goal_audit` fail-closed，绝不误清）。
6. **fork 完整转录开关 ✅**：`fork_session(full_transcript)`——true 复制 id ≤ 锚点的
   全部转录行（含被压缩折叠的原文与旧 boundary，ZCode transcript 语义对位；锚点
   在转录里即可）；false 保持活动视图前缀。入口 `/fork <id> full`、桌面长按菜单
   「分叉（含完整历史）」、chat_fork 命令 full 参数。
7. ~~CLI fork 会话切换~~ / 8. ~~insights ts 解析~~（核对为 false alarm：ts 本就按
   字符串直传显示）/ 9. ~~全仓 fmt~~——按用户指示跳过。

## 后续工作（第十三轮后）

1. **typed ask 的 JSDoc 约束**（@minimum/@maximum 等）：需 oxc 注释保留 + span 关联。
2. **/goal 审计的证据上下文扩展**：当前审计只看本轮最终回复；可带入目标设定以来的
   会话尾部或产物清单（成本换召回）。
3. **按消息锚点的 /rewind by message**：v2 的 anchor 列让 /rewind 也能按消息选点
   （当前只有 fork files 用到）。
