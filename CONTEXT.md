# RClaw

RClaw 是多通道 LLM Agent 运行时：外部通道（微信/NapCat/控制台）的消息进入统一的 turn 循环，经 ReAct 工具调用完成任务并交付回复。本文件是项目术语表——只定义概念，不记录实现。

## Language

### 会话与循环

**turn（轮次）**:
一条用户消息触发的完整 ReAct 循环，止于最终回复、取消或失败。
_Avoid_: 对话轮、request

**工具批次（tool batch）**:
单次模型推理返回的多个工具调用的一次并行执行。

**agent-run 内核（agent-run kernel）**:
主 turn 与子 Agent 共享的循环策略机器——上下文尾部、重试退避、工具批次、媒体认领闸门、完成闸门。两循环的差异由适配器贡献，不进内核。
_Avoid_: 引擎（泛指时）、loop（泛指时）

**主 turn 适配器**:
内核在顶层会话的形态：贡献插话处理、执行信号量、流式推理。

**子 Agent 适配器**:
内核在投研/协作任务的形态：贡献团队闸门、推理错峰、非流式推理。

### Skill 域

**skill 激活（skill activation）**:
模型读取 SKILL.md 后，该 skill 的 manifest 工具在本轮内变为可调用的过程。生命周期：未激活 → 已激活 →（skill 被编辑时）已失效 →（下一轮）重置。
_Avoid_: skill 加载（与包级 skill 载入混淆时）

### Bot Mode 域

**bot**:
一个隔离的成员智能体：toml 定义（persona / 固定 provider / 工具面收窄）+ 独立记忆（`agent/bots/<name>/`）+ 专属会话（`bot-<name>` 收件箱）。见 ADR-0006。
_Avoid_: profile（与 Hermes 术语混淆时）、子 Agent（那是 turn 内的临时执行体，无持久身份）

**收件箱会话（inbox chat）**:
bot 的规范会话 `(desktop, bot-<name>)`：用户直聊与队友交接都落在这里；交接消息作为 bot 的下一轮执行，绝不打断进行中的 turn。

**交接（handoff）**:
`message_agent` 工具发起的智能体间消息传递：带归属前缀的 Agent 来源消息投到目标收件箱，发送方以工具结果同步等待回复。
_Avoid_: DM（口语）

**交接链（handoff chain）**:
一条交接到达当前 bot 前依次经过的发送者序列（不可伪造，随 BusMessage 携带）；用于深度上限（3）与环路判定。
_Avoid_: 调用栈

**群聊房间（group room）**:
2-6 个 bot 的有界协调面板：一条用户消息触发至多 3 个串行轮 / 10 条成员发言 / 2 次补轮，全员 `(pass)` 提前收敛；房间日志、成员水位线与活动流持久化在 `agent/bots/groups/`。

**水位线（watermark）**:
群聊成员已读位置（seq）：成员轮只见增量，见过的消息不重复投递。
_Avoid_: offset

**DynamicToolSet**:
拥有已激活 manifest 工具集合及其完整生命周期的模块；两个容器与迁移规则只有一个所有者。
_Avoid_: 动态工具表（口语）

**SkillState（存储生命周期）**:
skill 档案的 Active / Stale / Archived 状态，由使用记录驱动。与「激活」（单轮内的可调用性）是两个正交概念。
_Avoid_: skill 状态（不区分两者时）

**curator（策展器）**:
runtime 空闲时归档陈旧 skill 的后台过程。归档条件除了闲置时长，还包括可靠性提前归档：agent-owned 且已 Stale 的 skill，若结果样本充足（≥4）且平滑成功率持续偏低（<34%），不必等到完整归档期限。

**结果记账（outcome accounting）**:
turn / 子代理任务收尾时，对本轮读取过入口的技能按整轮成败记一次成功/失败计数；驱动索引行的可靠性标注（「近N次：X成Y败」）与策展器的提前归档决策。取消轮中性，不记账。

**进化信号（evolution signal）**:
运行期过程事件（工具循环熔断、完成闸门拒绝、团队任务闸门拒绝、迭代上限、用户纠正语气）的确定性记录，落在 skills/.signals.jsonl 的有限窗口里；后台 review fork 优先核实与消费这些信号（信号≠结论，改技能前仍需 read 核实）。
_Avoid_: 进化结论、错误日志（它不是日志，是评审输入）

**热经验层（EXPERIENCE.md hot layer）**:
技能目录根下的 EXPERIENCE.md：读取 SKILL.md 时自动注入、不改技能正文即生效的经验补丁。新鲜经验先进热经验层，被证明稳定后才由 review fork 固化进 SKILL.md 并从热层移除。

**交叉验证（cross-verification）**:
candidate_patch 编码子代理完成后，用只读 mini 循环（read/list/grep）独立核实其结构化交接的可选过程；结论附在 handoff 之后，验证失败不阻塞交付。

**plan 工件（plan artifact）**:
PlanRun 工具的确定性多步计划：JSON 步骤表（id/task/depends_on）+ 依赖序执行（并行波次、失败传染为 skipped、不重试）+ agent/state.sqlite 计划 journal（流 plan/<plan_id>）里的逐步事件日志。步骤本体是带全部门禁的子代理；计划不可嵌套。见 ADR-0005。

**review（复盘）**:
turn 结束后的记忆整理与自进化审查过程。

### 媒体

**媒体 marker（媒体标记）**:
回复文本中声明「此处交付媒体文件」的占位标记；完成前必须经过验证认领。
_Avoid_: 媒体占位符

**媒体认领闸门（media claim gate）**:
完成判定前对未验证媒体标记的拒绝机制：有标记未验证，turn 不得宣称完成。

**中间输出（intermediate output）**:
工具轮中将模型 text 作为中间消息转发到通道的行为，按通道能力许可。

### 通道

**通道能力（channel capabilities）**:
某通道的三项事实：是否可收发媒体、是否具备工作状态、是否许可中间输出。能力由适配器声明，随接线传递。
_Avoid_: 通道开关（与环境变量语义混淆时）

### 运行时状态

**TurnState**:
单个 turn 循环中全部可变状态的归属模块；工具批次与循环本体在它上面相遇。
_Avoid_: 循环上下文（与 dynamic context 混淆时）

**静默（quiescence）**:
「没有任何 turn、review、curator 在运行」的可订阅状态；/clear 与策展器等待它而非各自探测。
_Avoid_: idle（口语）、空闲检测（指各处各自的探测行为时）
