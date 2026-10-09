# 投研团队：四个独立角色的错峰并行与交叉质询框架

对 $ARGUMENTS 进行团队化投资研究分析。该流程用于独立证据链和观点对抗能显著提高结论质量的任务；是否选择本 Skill，由 LLM 根据完整任务目标、研究深度和执行成本判断，不使用关键词匹配。在 RClaw 中使用 `TeamCreate` 创建团队，并在同一条 assistant 消息中发起四个命名 `Agent`/`Task` tool call。四个调用必须对应四个真正独立的子 agent，各自拥有独立上下文、工具循环、证据和初始结论；禁止由 team-lead 或单个子 agent 模拟四种视角。RClaw 会错开它们实际访问 LLM 的时间，由当前 agent 作为 team-lead 组织交叉质询并汇总结果。

## 执行流程

### 第零步：拆分多公司请求

先从 `$ARGUMENTS` 提取公司列表。只有一家时直接进入后续步骤；有两家或更多公司时，必须逐家公司串行完成以下全部步骤。

- 当前公司的四个独立 agent 可以并行研究，但不得提前创建或启动下一家公司的团队。
- 必须等当前公司完成四份研究、交叉质询、最终报告、抽检、成员关闭和 `TeamDelete success=true` 后，才处理下一家公司。
- 禁止在同一条 assistant 消息中为两家公司发起 8 个 `Agent`/`Task` 调用。
- 全部公司完成后，可以直接基于各公司的最终报告做横向比较，不再用一个 agent 模拟新的“四大师”意见。
- `reports/` 中已有报告只能作为待复核的历史证据，不能跳过当前公司的 `TeamCreate`、四个独立 Agent、交叉质询或数据抽检；复用前必须核对来源日期和统计口径。

### 第一步：展示团队框架

向用户展示以下团队结构后立即启动；用户已经明确要求使用本 Skill 时无需再次确认：

| 角色 | 职责 | 分析框架 |
|------|------|----------|
| **team-lead**（你自己） | 统筹协调、汇总研判、输出最终报告 | 四大师综合框架 |
| **business-analyst** | 商业模式 & 护城河分析 | 段永平视角 |
| **financial-analyst** | 财务报表 & 估值分析 | 巴菲特视角 |
| **industry-researcher** | 行业格局 & 竞争态势 | 芒格视角 |
| **risk-assessor** | 风险评估 & 管理层研判 | 李录视角 |

### 第一步半：AI研究偏见评估

在创建团队前，先向用户展示该公司的"AI可研究性"评估：

**信息丰富度评级**（决定研究策略）：
| 等级 | 特征 | 研究策略调整 |
|------|------|------------|
| A级（信息充裕） | 上市多年、券商覆盖广 | 团队重点放在**反面检验**和**非共识视角**，避免输出与市场一致的"正确的废话" |
| B级（信息适中） | 上市不久、覆盖有限 | 每个Agent的推算数据必须标注置信度，team-lead汇总时标注"数据充分度" |
| C级（信息稀缺） | 冷门/新上市/新兴市场 | 团队转为"第一性原理模式"：不追求报告完整性，聚焦商业本质的几个核心问题 |

**关键提醒**：资料多≠确定性高，资料少≠确定性低。AI能输出的置信度 ≠ 投资的真实确定性。确定性来自商业模式本身，不来自资料数量。

将评级结果告知每个Agent，影响其研究方式。

### 第一步补充：联网能力预检

创建团队前，由 team-lead 使用 `web_search` 或 `tavily_search` 执行一次真实搜索探测，例如搜索 `{公司名} 最新财报 investor relations`，并确认至少得到一条可访问、带来源链接的近期结果。训练知识、记忆中的网址或未实际打开的缓存内容不能算探测成功。

- 探测成功：记录本次可用的搜索工具，并把联网状态告知每个 Agent。
- 搜索工具缺失、报错或没有可验证来源：默认停止，不启动后台 Agent，并向用户说明无法完成基于最新公开信息的研究。
- 只有用户明确接受离线降级时才继续；此时必须在报告顶部标注“⚠️ 本报告未能联网，基于训练知识，置信度降级”，并在已知时写明知识截止日期、未知时明确写“知识截止日期未知”，且不得把训练知识表述为最新事实。

### 第二步：规划四个独立角色

使用 `TeamCreate` 创建 `{公司名}-research` 团队，当前 agent 扮演 team-lead，负责创建任务、启动 4 个命名 agent、通过 `TaskList`/`SendMessage` 跟踪进度并汇总结果。

### 第三步：定义4个独立研究任务

用 `TaskCreate` 准备以下 4 个任务，保存每次返回的真实 `taskId`；后续 `Agent`/`Task` tool call 必须使用当前工具 schema 中的 `description`、`prompt`、`subagent_type`、`name`、`team_name` 和 `split_reason`。

#### 任务1：商业模式分析
- subject: `分析{公司名}商业模式、护城河与用户价值`
- description 包含：
  1. 商业模式本质：核心生意定义、收入结构拆解
  2. 平台/产品飞轮效应如何运转
  3. 护城河分析：品牌/转换成本/网络效应/规模效应/技术壁垒，逐一验证
  4. 用户/客户价值：为各方创造了什么独特价值
  5. 业务矩阵与协同效应
  6. 段永平"好生意"标准评估：差异化、定价权、可持续竞争优势
  7. 要求搜索最新财报、行业报告等公开信息

#### 任务2：财务与估值分析
- subject: `分析{公司名}财务数据、盈利能力与估值`
- description 包含：
  1. 近3-5年营收、净利润、经营利润趋势
  2. 盈利能力指标：ROE、ROA、毛利率、经营利润率
  3. 现金流分析：经营性现金流、自由现金流、资本开支
  4. 资产负债表健康度：现金储备、负债率、流动性
  5. 估值分析：PE/PS/PB/EV等，与历史及同业对比
  6. 安全边际评估：内在价值 vs 当前股价
  7. **金融严谨性验证（必须使用动态专用工具，禁止心算）**：
     - 市值验算：调用 `skill__ai_berkshire__financial_verify_market_cap`，传入 `price`、`shares`、`reported` 和可选 `currency`
     - 估值验算：调用 `skill__ai_berkshire__financial_verify_valuation`，传入 `price` 及可用的每股指标
     - 关键数据交叉验证：先用文件工具将来源和值写入 `skills/ai-berkshire/tmp/{公司名}-cross-validate.json`，再调用 `skill__ai_berkshire__financial_cross_validate`，把 `tolerance` 设为 `1`
     - 三情景估值：调用 `skill__ai_berkshire__financial_three_scenario`；`growth` 和 `pe` 都是依次包含乐观、中性、悲观值的三元素数组，增速必须传小数（如 5%=`0.05`，不得写 `5`）
     - 将工具输出结果直接嵌入报告中作为验证记录
     - 所有路径字段以 RClaw 工作目录为基准并带 `skills/ai-berkshire/` 前缀；不得自行选择解释器或拼接脚本命令

#### 任务3：行业与竞争分析
- subject: `分析{行业}行业格局与{公司名}竞争态势`
- description 包含：
  1. 行业规模与增长：市场规模、增速、渗透率
  2. 竞争格局：主要对手市场份额、竞争策略对比
  3. 核心竞争者威胁评估：逐个分析主要竞争对手
  4. 各细分赛道格局
  5. 行业趋势：技术变革、政策影响、新进入者
  6. 产业链分析：上中下游价值分配
  7. 要求搜索最新行业数据和竞争动态

#### 任务4：风险与管理层评估
- subject: `评估{公司名}投资风险与管理层质量`
- description 包含：
  1. 管理层评估：CEO能力圈、诚信度、战略眼光、资本配置能力、历史决策质量
  2. 监管风险：当前及潜在监管影响
  3. 竞争风险：各竞争对手威胁程度评估
  4. 业务风险：新业务亏损、扩张不确定性
  5. 宏观风险：经济周期、行业周期影响
  6. 治理结构：股权结构、关联交易、股东回报政策
  7. 长期确定性：10年后公司会怎样？什么可能颠覆其商业模式？
  8. 要求搜索最新监管动态、管理层言论等

### 第四步：启动4个独立Agent（模型请求错峰）

在同一条 assistant 消息中**连续**发起 4 个 `Agent`/`Task` tool call（必须相邻，RClaw 才会把它们作为独立后台 teammate 调度）。运行时会为实际 LLM 请求自动错峰，并对 429、网络错误和服务端过载执行指数退避重试；不要因此删减角色、合并 prompt 或让一个 agent 模拟多人。每个调用的 `team_name` 都显式使用 `TeamCreate` 返回的真实团队名；报告通过 team-lead 收件箱送达，team-lead 用 `TaskList` 取回。

每个 task 的配置：
- `description`: 该角色任务的一句话短标题（必填）
- `prompt`: 下方完整 prompt 模板，包含真实 `taskId`、公司名、联网状态、信息丰富度和输出规则（必填）
- `split_reason`: 固定为 `independent_evidence`，表示四个角色各自形成独立证据（必填）
- `name`: 对应角色名（business-analyst / financial-analyst / industry-researcher / risk-assessor）
- `subagent_type`: 对应大师视角和职责
- `team_name`: `TeamCreate` 返回的真实团队名
- `max_tool_iter`: 固定为 `256`，为独立研究、阶段屏障、质询回应和必要的联网重试预留充足迭代空间

不要发送未出现在 `Agent`/`Task` schema 中的 `role`、`task`、`context` 来替代上述字段；完整任务与共享上下文都写入 `prompt`。

每个Agent的prompt模板：

**这是必须完整复制到每个 Agent 调用中的执行契约，不是供 team-lead 摘要的提纲。禁止缩写、改写或删除下方“交叉质询”中的 challenge、response、complete 及“先回执、后等待”规则；四个 prompt 只替换花括号变量和各自研究内容。**

```
你是{公司名}投研团队中的"{角色中文名}"，负责从{大师名}投资视角分析{公司名}。

请完成任务 #{任务编号}：{任务subject}

启动后立即用 `TaskUpdate` 将原样、不带 `#` 前缀的真实 `taskId` 更新为 `running`，并把 owner 设为你的 `name`。

具体要求：
{任务description的内容}

**研究方法**：
- 使用 WebSearch 搜索最新公开信息（财报、行业报告、新闻）
- **财务数据必须来自两个独立来源**，按 `skills/financial-data.md` 规范执行（美股：macrotrends+stockanalysis；港股：aastocks+macrotrends；A股：东方财富+巨潮资讯；台股：FinMind 台股动态工具+Goodinfo），两源误差>1%须标记
- 确保数据准确，关键数据标注来源
- 分析要深入，不流于表面
- **联网失败禁止伪装**：如果搜索工具被拦截、报错或无法返回可验证来源，立即停止把该角色任务作为“最新信息研究”，如实通知 team-lead 并等待其决定停止或请求用户接受离线降级；禁止自行降级，也禁止用训练知识冒充联网结果或“最新”数据

**交叉质询（必须完成）**：
- 先独立完成研究并形成初始结论，不得先读取其他角色意见后再迎合
- 先用 `SendMessage` 将带有“独立初始判断”标记的初始结论发送给 `team-lead`；在收到 team-lead 发出的 `challenge_round_1_start` 前，禁止联系其他角色、禁止开始质询，也不得结束任务
- 发送初始结论后保持 `running`，用 `TaskList(wait_timeout_ms=300000)` 阻塞等待 `challenge_round_1_start`（收到邮箱消息会提前返回）；该阶段屏障保证四份初稿都在接触其他观点前独立形成
- 第一轮严格按 business-analyst → financial-analyst → industry-researcher → risk-assessor → business-analyst 的唯一闭环质询：business-analyst 质询 financial-analyst 并回应 risk-assessor，financial-analyst 质询 industry-researcher 并回应 business-analyst，industry-researcher 质询 risk-assessor 并回应 financial-analyst，risk-assessor 质询 business-analyst 并回应 industry-researcher。`challenge_round_1_start` 只能列出本角色对应的这一组 challenge/respond 动作，不得添加闭环之外的质询关系；例如 financial-analyst 不得等待 risk-assessor 的质询。开始消息会包含指定对象的初始报告；收到后，基于证据向指定对象发送至少一条核心质疑，并使用 `challenge_round_1_challenge:{发送者}->{接收者}` 作为 `summary`
- 每个角色必须接收并直接回应本轮发给自己的质疑，必要时修正自己的假设、评分或结论；回应消息使用 `challenge_round_1_response:{回应者}->{质疑者}` 作为 `summary`，正文必须写明“收到的挑战 / 回应证据 / 是否修订 / 尚未解决的分歧”。运行时会把角色间消息同步给 `team-lead`，无需重复转发同一正文
- 第一轮中，一个角色只有在“已发出自己的质疑、已回应收到的质疑、已收到并评估对方对自己质疑的答复”三项都完成后，才能发送完成回执。后续轮次只执行 `challenge_round_N_start` 为本角色明确列出的 challenge/respond 动作；被分配 challenge 时必须等目标回应并完成评估，全部分配动作闭环后才能发送回执。未被列为参与者时继续等待，不发送该轮回执
- **本轮闭环后的工具顺序不可颠倒**：先调用 `SendMessage(to="team-lead", summary="challenge_round_N_complete:{角色名}", message={"revision_summary":"<本轮是否及如何修订>","response_evaluation":"<目标答复是否解决质疑>","open_disagreements":["<未决分歧>"]})`；确认发送成功后，才调用 `TaskList(wait_timeout_ms=300000)` 等待下一条控制消息。每个角色每轮只发送一条回执，`N` 和角色名必须替换为开始消息给出的实际值。收到 `challenge_round_{N+1}_start` 时围绕其中指定的未决分歧继续质询；只有收到 `challenge_complete` 才可结束质询
- 质询至少 1 轮但不设固定轮数或轮数上限；team-lead 根据分歧是否收敛动态决定。最终报告必须保留每一轮的挑战、回应、修订及未消除分歧

**输出要求**：
- 报告要详尽，使用Markdown表格呈现关键数据
- 每个分析维度要有明确结论和评分
- 报告末尾要有该维度的总体结论

**完成后**：
只有在独立研究完成且收到 team-lead 的 `challenge_complete` 后，才使用 `TaskUpdate` 将任务状态更新为 `succeeded`，并用 `SendMessage` 将完整分析报告发送给 `team-lead`。
```

### 第五步：接收报告、检查交叉质询并跟踪进度

- 通过 `TaskList(wait_timeout_ms=300000)` 阻塞等待并跟踪 4 个角色进度和 team-lead 收件箱消息；收到消息会提前返回，逐项检查是否有错误、缺口或需要补充搜索
- 初稿屏障和多轮质询阶段使用 `TaskList(wait_timeout_ms=300000)` 等待邮箱消息；禁止对这些角色调用 `AgentWait`。`AgentWait` 等待的是角色最终结束，而角色在收到 `challenge_complete` 前按设计不会结束，两者会形成最长 300 秒的无效等待
- 先收齐四条带“独立初始判断”标记的报告；未收齐前不得让任何角色开始质询
- 四份初始报告全部到齐后，将四个角色全部声明为第一轮参与者，并按以下固定映射分别发送 `challenge_round_1_start`：business-analyst=`challenge financial-analyst + respond risk-assessor`；financial-analyst=`challenge industry-researcher + respond business-analyst`；industry-researcher=`challenge risk-assessor + respond financial-analyst`；risk-assessor=`challenge business-analyst + respond industry-researcher`。不得从闭环顺序臆造第二个质询者或让角色等待映射之外的消息。每条开始消息必须附上该角色质询对象的初始报告、上述唯一一组 challenge/respond 动作，以及 `SendMessage(to="team-lead", summary="challenge_round_1_complete:{实际角色名}", message={"revision_summary":"<本轮是否及如何修订>","response_evaluation":"<目标答复是否解决质疑>","open_disagreements":["<未决分歧>"]})`；发送开始消息前替换 summary 中的实际角色名，调用模板不得使用省略号，message 中的尖括号提示由角色在回执时填写实际内容
- 运行时会把角色间的 `SendMessage` 同步到 team-lead 收件箱。第一轮收齐四个参与者各自唯一一条完成回执，并结合收件箱中的挑战/回应记录核对四个角色确实完成“发出质疑 + 回应收到的质疑 + 收到并评估自己质疑的答复”；回执内容必须包含修订摘要、答复评估和未决分歧，不得仅凭回执跳过证据核对
- team-lead 基于证据而非固定计数决定是否继续。若仍有关键结论冲突、数据口径冲突、挑战未被正面回应或新证据可能改变结论，先声明下一轮参与者集合，再分别发送 `challenge_round_{N+1}_start`；每条开始消息必须写明该角色需要执行的 challenge/respond 动作、对象和待解决问题，被质疑的目标必须同时列为需要 respond 的参与者
- 后续每轮只等待声明的参与者各自唯一一条 `challenge_round_N_complete:{角色名}` 回执，并逐项核对其已完成开始消息分配的全部动作；每条开始消息都必须附上已替换实际轮次号和角色名的完整回执模板。被分配 challenge 的参与者必须在 `response_evaluation` 中评估目标回应是否解决质疑。不得等待未参与角色的该轮回执
- 当关键争议已经收敛，或公开证据不足且各方已明确记录保留分歧时，team-lead 向四个角色发送 `challenge_complete`。轮数不设固定上限；不得为了凑轮数机械追问，也不得在 `challenge_complete` 前提前汇总
- 每次 `TaskList(wait_timeout_ms=300000)` 超时返回后，必须先按该轮开始消息的实际映射核对收件箱 challenge/response 和角色状态，禁止不核对就再次等待。若已分配动作实际闭环但回执缺失，向该角色发送一次定向提醒并再次附上该轮完整回执模板，明确告知不要等待未分配的挑战、直接发送回执；若仍未闭环，只提醒尚缺的实际 challenge/respond 动作。相同轮次和角色的提醒只发一次，提醒后再用 `TaskList` 等待，不得连续空等
- 向用户展示已完成的角色和每份报告的核心要点（3-5条）
- 如果某个角色在 `challenge_round_1_start` 前、任一进行中的质询轮未闭环，或 `challenge_complete` 前提前变为 inactive，即使执行状态显示 succeeded，也按“阶段未完成”处理，不得接受为最终结果
- 如某个角色在重试耗尽后失败、证据不足或阶段未完成，先用 `TaskUpdate` 将该角色的原 `taskId` 标为 `failed`，再用 `TaskCreate` 为该次补做创建新任务和新 `taskId`，最后只对该角色单独调用 `Agent`。不得让同一 owner 同时遗留旧的 `running` 任务，不得重启全部四个角色，也不得复用原任务：尚未提交独立初稿时使用 `split_reason=independent_evidence`；已有独立初稿时使用 `split_reason=new_feedback`，prompt 带上原初稿、当前阶段和尚未回应的挑战
- 补做角色发出质疑时，如果目标角色已经 inactive，则先用 `TaskCreate` 创建一个独立的“回应补充质询”任务并保存新 `taskId`，再单独以该目标的原 `name` 启动一次 `split_reason=new_feedback` 的回应任务；prompt 必须带上新 `taskId`、目标角色的原始报告和新质疑，要求它把新任务从 `running` 更新到 `succeeded`，只做回应、修订并发送给 `team-lead`，不得复用已经 succeeded 的原任务，也不得重新做整份研究。失败恢复调用必须串行，任何时刻只补做一个角色

### 第六步：检查子 agent 输出

命名 agent 作为后台 teammate 运行，报告通过 team-lead 收件箱送达。反复调用 `TaskList(wait_timeout_ms=300000)`（收到消息会提前返回），收齐 4 份独立初稿、每一轮“挑战—回应—修订—未决分歧”记录，并在宣布 `challenge_complete` 后收齐 4 份最终报告；逐份确认输出已纳入汇总，并标注各自的置信度和信息缺口。

### 第七步：汇总最终报告

综合4份分析报告，输出以下结构的最终报告：

---

#### 1. 一句话结论
> 用一段话（50-100字）概括是否值得投资及核心逻辑

#### 2. 四维评分总表
| 维度 | 框架 | 评分(1-5星) | 核心判断 |
|------|------|------------|----------|

综合评分：X / 5

#### 3. 核心数据速览
关键财务和经营指标表格（近2年对比）

#### 4. 各维度分析摘要
每个维度摘取3-5条最重要的发现

#### 5. 投资论点（Bull vs Bear）
- 🟢 看多逻辑（5-7条）
- 🔴 看空逻辑（5-7条）

#### 6. 巴菲特买入前Checklist
| # | 检查项 | 通过? | 说明 |
10个核心检查项，逐一评估

#### 7. 最终投资建议
- 定性判断表（生意质量/管理层/估值/时机）
- 分层操作建议表（激进型/稳健型/保守型 → 建议+价格区间）
- 关键催化剂（加仓信号/减仓信号各3-5条）

#### 8. 总结段落
100-200字的最终总结

---

### 第八步：保存报告

将完整最终报告写入 `reports/{公司名}/最终报告.md`，必要时同时保存四个视角底稿。

### 第九步：数据抽检（准出流程）

```text
skill__ai_berkshire__report_extract
{"report":"skills/ai-berkshire/reports/<报告文件>","output":"skills/ai-berkshire/tmp/{公司名}-audit.json"}

skill__ai_berkshire__report_preflight
{"results_file":"skills/ai-berkshire/tmp/{公司名}-audit.json","report":"skills/ai-berkshire/reports/<报告文件>"}

skill__ai_berkshire__report_verdict
{"results_file":"skills/ai-berkshire/tmp/{公司名}-audit.json","report":"skills/ai-berkshire/reports/<报告文件>"}
```

`skill__ai_berkshire__report_extract` 会直接写入与报告哈希、完整事实池和固定 15% 样本（不足 20 项时至少 3 项或全量）绑定的审计包；不得重建 JSON、换比例/seed 或修改样本字段。

每项填写两组 `fetched_raw_value` / `fetched_raw_unit` / `fetched_evidence` / `fetched_source` / `fetched_locator`，其中 `fetched_raw_value` / `fetched_raw_value2` 必须是 JSON 数值而不是带引号的字符串，`fetched_value` 与 `fetched_value2` 保持 `null` 交由脚本换算；两个直接 URL 必须来自不同发布方，摘录必须原样出现对应的原始值和完整单位。先一次性填完全部样本，再运行 `preflight` 并修复其一次汇总的全部问题；输入和错误完全相同时不得原样重复命令。预检通过后，`verdict` 会在首次判决时锁定两个来源槽位。**【准出】** 全部通过 → 报告可发布；**【打回】** 有不通过 → 修正报告后重新提取，或如实保持打回并交由人工复核；不得删除证据账本、改写旧观察或更换 URL。返工不设固定次数上限；只要输入有实质修正就继续。

### 第十步：收尾

确认最终报告、四个视角底稿和数据抽检记录都已写入 `reports/{公司名}/`。团队清理前调用 `SendMessage(to="*", message="shutdown_request")`；运行时仅向发送瞬间实际活跃的成员投递，若返回 `skipped=true` 且 `recipients=[]` 则直接进入清理，不得使用文案关键词判断。等待成员结束后再用 `TeamDelete` 清理团队，并确认返回的 `success` 为 `true`；如果仍提示 active member，继续等待后重试。未成功清理会让 `active_team` 残留，阻塞后续 `TeamCreate`。

### 第十一步：向用户交付

全部公司都已通过抽检且对应团队已 `TeamDelete success=true` 后，默认向用户交付完整的 `最终报告.md`，不得只发摘要后再询问“是否需要完整报告”。当前真实来源是已启用媒体能力的外部 channel 时，按照包级“外部通道报告交付”规则，先读取 `skills/glm-master-skill/SKILL.md`，再逐份调用 `skill__glm_master_skill__publish_media`；只传递结构化字段，不得自行选择解释器、脚本路径或命令行参数。最后一个发布器调用之后不得再执行检查、复制、改名、团队或其他工具。只有收到当前通道有效的 `MEDIA_MARKER` 才能说附件已发送；媒体 channel 发布失败或没有有效 marker 时，明确报告发布失败和报告实际路径，不得用“已经发布”“应该已发送”等措辞替代回执。当前来源是 `cli`、`system`、未启用或不支持媒体的 channel 时，不运行发布器，直接返回完整报告的实际路径；这是该通道的正常交付方式，不得误报为发布失败。

## 重要注意事项

1. **必须是4个真正独立的Agent**——在同一条 assistant 消息中发起 4 个 `Agent`/`Task` tool call；不得由一个 agent 模拟四种视角，实际 LLM 请求由运行时错峰
2. **Agent输出通过团队通道返回**——每个 agent 用 `TaskUpdate` 更新状态，并用 `SendMessage` 把完整报告发给 `team-lead`
3. **数据准确性**——要求Agent使用WebSearch搜索最新数据，关键数据交叉验证
4. **联网失败必须显式降级**——探测失败时默认停止；用户明确接受离线研究时，所有 Agent 和最终报告都必须标注来源缺口与置信度降级
5. **结论要明确**——不回避给出买入/观望/回避建议和具体价格区间
6. **所有分析必须有数据支撑**——附数据来源
7. **耐心等待**——4个Agent研究需要几分钟，实时向用户更新进度
8. **反偏见意识**——team-lead在汇总时必须评估：各Agent的分析是否受限于资料充裕度？是否与市场共识过度趋同？最终报告需包含"信息丰富度评级"和"AI研究局限性声明"
9. **信息稀缺时的诚实原则**——宁可在报告中留白标注"数据不足"，也不要用推测填满框架伪装确定性
10. **多公司必须串行**——当前公司完整收尾并成功删除团队后才能开始下一家公司，任何时刻最多只有一组四角色投研团队
11. **交叉质询轮数由 team-lead 决定**——四个角色都要独立形成初始判断，至少完成 1 轮挑战与回应；之后根据争议是否收敛动态追加轮次，直到 team-lead 发送 `challenge_complete`
