# AI Berkshire for RClaw — 投研包指令

## 项目定位

这是 AI Berkshire 在 RClaw 中使用的投研能力包，不是原 Claude Code 仓库的完整开发目录。

核心框架仍然是四大师价值投资研究法：巴菲特、芒格、段永平、李录。RClaw 扫描本包中的 `skills/*.md`；每轮首次读取其中任一具体 Skill 时，会同时按需加载本文件，并用 RClaw 自身工具执行搜索、读写、脚本和多 agent 任务。本文件不常驻全局系统提示。

## 子技能选择

- 根据用户的完整目标、研究深度、是否需要独立意见与交叉质询、执行成本和交付形式选择最合适的子技能；不使用关键词匹配，也不设置固定默认子技能。
- `investment-research` 使用统一上下文完成系统化研究，适合不需要四个独立角色交叉质询的任务。
- `daily-stock-analysis` 复用最近一次已准出的深度报告，只更新交易日行情、新公告、
  新事件和由此变化的估值/论点，适合定时执行的每日跟踪；无有效基线或出现新财报
  时才升级为 `investment-research`。
- `investment-team` 使用四个真正独立的 Agent 研究并交叉质询，适合独立证据链和观点对抗能显著提高结论质量的任务。
- 用户明确指定包内子技能时遵从其选择；没有指定时，由 LLM 选择足以完成目标的最小流程。
- `reports/` 中的旧报告只能作为待复核的历史证据或缓存，不能作为已执行当前 Skill 的证明，也不能替代所选 Skill 本轮要求的数据收集、独立 Agent、交叉质询、数据抽检或报告汇总。复用任何旧数据前必须核对其来源日期和统计口径。
- 多家公司使用 `investment-team` 时逐家公司串行完成团队全流程；全部单公司报告完成后再直接制作横向比较。

## 包结构

```text
ai-berkshire/
├── skills/       投研 skill 定义，RClaw 会扫描这里的 .md
├── tools/        由 Manifest 动态入口封装的投研辅助程序
├── data/         工具运行需要的 watchlist、fundamentals 等轻量数据
├── reports/      已有或运行时生成的投研报告
├── assets/       运行时按需创建的图片、图表和文章资产
├── tmp/          运行时按需创建的临时文件、缓存和中间数据
├── berkshire.md  本文件，按需加载的包级投研原则
└── LICENSE
```

本包不依赖原 AI Berkshire 仓库的 `scripts/`、`codex-skills/` 或 `codex-prompts/`。外部文件交付复用 RClaw 随附的 `glm-master-skill` 动态发布工具。现有 `reports/` 内容视为用户产物，不作为 Skill 指令；`reports/`、`assets/`、`tmp/` 等输出目录不存在时，可在 AI Berkshire 包目录内按需创建。

读取本包任一 `skills/*.md` 时，运行时会从包根目录 `skill.toml` 激活 AI Berkshire 专用工具。所有本包自带程序都必须通过这些结构化工具调用；不得自行选择 Python、拼接 `tools/` 目录内程序路径或重建命令行参数。路径字段以 RClaw 工作目录为基准，因此包内文件使用 `skills/ai-berkshire/...`。

| 用途 | 动态工具 | 主要字段 |
|---|---|---|
| A 股行情、财务、估值、搜索 | `skill__ai_berkshire__ashare_quote`、`skill__ai_berkshire__ashare_financials`、`skill__ai_berkshire__ashare_valuation`、`skill__ai_berkshire__ashare_search` | `code` 或 `keyword` |
| 市值与估值验算 | `skill__ai_berkshire__financial_verify_market_cap`、`skill__ai_berkshire__financial_verify_valuation` | `price`、股本/报告市值或每股指标 |
| 多源核验与 Benford 检测 | `skill__ai_berkshire__financial_cross_validate`、`skill__ai_berkshire__financial_benford` | 优先传 `values_file`；多源核验另传 `field` |
| 精确算式与三情景估值 | `skill__ai_berkshire__financial_calc`、`skill__ai_berkshire__financial_three_scenario` | `expression`；或 `price`、`eps`、`shares`、`growth`、`pe` |
| 台股行情、估值、财务、月营收 | `skill__ai_berkshire__twstock_quote`、`skill__ai_berkshire__twstock_valuation`、`skill__ai_berkshire__twstock_financials`、`skill__ai_berkshire__twstock_revenue`、`skill__ai_berkshire__twstock_dividend`、`skill__ai_berkshire__twstock_search` | `code`；搜索传 `keyword`（新台币口径，用法见 `skills/financial-data.md` 台股章节） |
| 十年折现估值（终值倍数/IRR/准出审计） | `skill__ai_berkshire__terminal_value_audit`、`skill__ai_berkshire__terminal_value_pe`、`skill__ai_berkshire__terminal_value_irr`、`skill__ai_berkshire__terminal_value_sweep`、`skill__ai_berkshire__terminal_value_check`、`skill__ai_berkshire__terminal_value_table` | audit 传 `currency`、`r`、`roic`、`g`；pe/irr 见 `skills/investment-research.md` 长期折现估值 |
| 报告全量检查与抽样 | `skill__ai_berkshire__report_lint`、`skill__ai_berkshire__report_extract` | `report`；抽样可传 `output` 或 `dry_run` |
| 来源快照与准出复核 | `skill__ai_berkshire__report_source_snapshot`、`skill__ai_berkshire__report_verify_pass` | `source`、`input`、`output`；或 `report` |
| 审计预检与判决 | `skill__ai_berkshire__report_preflight`、`skill__ai_berkshire__report_verdict` | `results_file`、`report`，判决可传 `output_json` |
| 雪球抓取与缓存筛选 | `skill__ai_berkshire__xueqiu_scrape`、`skill__ai_berkshire__xueqiu_filter_cache` | 抓取传 `user_id`；缓存筛选传 `from_cache`、`keywords`、`output` |

## RClaw 适配规则

原 skill 中的 Claude/Codex 术语按以下方式映射：

| 原 skill 说法 | RClaw 中的执行方式 |
|---|---|
| `$ARGUMENTS` | 当前用户请求 |
| 团队、任务、消息和进度更新描述 | 直接使用 `TeamCreate` / `TaskCreate` / `TaskUpdate` / `TaskList` / `TaskGet` / `SendMessage` / `Agent` / `Task` |
| `WebSearch` | 使用 `web_search` 的 `search_query`，或使用 `tavily_search` 的 `query` |
| `WebFetch` | 使用 `web_search` 的 `url`，或使用 `tavily_extract` 的 `urls` |
| `Bash` | 使用 `shell` |
| `Read` | 使用 `read` |
| `Write` | 使用 `write` |
| `Edit` | 使用 `edit` |
| 原规则中的 `~/ai-berkshire/tools/...` 或 `tools/...` 程序 | 调用包根 Manifest 激活的对应 `skill__ai_berkshire__...` 结构化工具，不得改写为 shell 命令 |

动态工具的路径字段以 RClaw 工作目录为基准；包内的 `reports/`、`assets/`、`data/` 和 `tmp/` 都要写成 `skills/ai-berkshire/...`。`read`、`write`、`edit` 等文件工具使用 `{skills_dir}/ai-berkshire/...` 绝对路径。只有用户明确指定其他位置时，才写到包目录之外。

绝对包路径包含 `/data/data/com.termux` 时按 Termux 分支处理依赖，并以 `PREFIX` / `TERMUX_VERSION` 作为兜底判断。不要在 Termux 上自动下载 Playwright 浏览器；确需且获准安装时，使用 `pkg install x11-repo && pkg install chromium` 和当前 Python 的 `-m pip install playwright`，不要运行 `playwright install chromium`。`skill__ai_berkshire__xueqiu_scrape` 会优先复用 PATH 中的 `chromium-browser` / `chromium`，纯 headless 时也可回退到 `headless_shell`；`chromedriver` 不是 Playwright 依赖。不可用或启动失败时再改用已配置的数据接口或联网搜索兜底。

## 报告目录结构

RClaw 中生成的投研输出建议按以下结构写入 AI Berkshire 包目录：

```text
skills/ai-berkshire/              # 动态工具路径字段中的包前缀
├── assets/
├── data/
├── tmp/
└── reports/
    ├── AI产业研究/
    ├── 腾讯/
    │   ├── 腾讯-research-YYYYMMDD.md
    │   ├── 腾讯-earnings-YYYYQX.md
    │   ├── 腾讯-management-YYYYMMDD.md
    │   └── 腾讯-thesis.md
    ├── 拼多多/
    ├── 行业名-funnel-YYYYMMDD.md
    ├── 行业名-industry-YYYYMMDD.md
    ├── portfolio-latest.md
    └── 多公司对比-checklist-YYYYMMDD.md
```

旧 skill 中只写 `reports/...`、`assets/...` 或 `data/...` 时，均解释为相对于 AI Berkshire 包目录：

```text
reports/
├── AI产业研究/
├── 腾讯/
│   ├── 腾讯-research-YYYYMMDD.md
│   ├── 腾讯-earnings-YYYYQX.md
│   ├── 腾讯-management-YYYYMMDD.md
│   └── 腾讯-thesis.md
├── 拼多多/
├── 行业名-funnel-YYYYMMDD.md
├── 行业名-industry-YYYYMMDD.md
├── portfolio-latest.md
└── 多公司对比-checklist-YYYYMMDD.md
```

公司相关的新报告统一放在 `reports/{公司名}/` 下；行业、主题、组合和多公司筛选报告可放在 `reports/` 根目录。读取历史产物时兼容旧版位于 `reports/` 根目录的公司报告，但不要在未征得用户同意时移动或删除旧文件。

## 报告命名规范

| Skill | 建议文件命名 |
|---|---|
| investment-team | `reports/{公司名}/最终报告.md`，必要时同时保存四个视角文件 |
| investment-research | `reports/{公司名}/{公司名}-research-YYYYMMDD.md` |
| daily-stock-analysis | `reports/{公司名}/{公司名}-daily-YYYYMMDD.md` |
| investment-checklist | `reports/{公司名}/{公司名}-checklist-YYYYMMDD.md` 或 `reports/多公司对比-checklist-YYYYMMDD.md` |
| industry-research | `reports/{行业名}-industry-YYYYMMDD.md` |
| industry-funnel | `reports/{行业名}-funnel-YYYYMMDD.md` |
| private-company-research | `reports/{公司名}/{公司名}-private-YYYYMMDD.md` |
| earnings-review | `reports/{公司名}/{公司名}-earnings-{期间}.md` |
| earnings-team | `reports/{公司名}/{公司名}-earnings-{期间}.md`，可附四个视角底稿 |
| thesis-tracker | `reports/{公司名}/{公司名}-thesis.md` |
| portfolio-review | `reports/portfolio-latest.md` |
| management-deep-dive | `reports/{公司名}/{公司名}-management-YYYYMMDD.md` |
| news-pulse | `reports/{公司名}/{公司名}-news-YYYYMMDD.md` |
| investment-memo-craft | 公司类新报告沿用 `reports/{公司名}/{公司名}-research-YYYYMMDD.md`；改写既有报告时沿用原路径 |
| deep-company-series | `reports/{公司名}/《看懂{公司名}》/0X-XX.md`；目录已存在时追加日期后缀，禁止覆盖 |
| wechat-article | 技术主题写入 `reports/AI产业研究/`，投资主题写入 `reports/{公司名}/`，通用主题写入 `reports/` |
| bottleneck-hunter | `reports/bottleneck-map/` 下按具体 Skill 约定命名 |

## 外部通道报告交付

报告保留在本包 `reports/` 中作为权威产物；外部通道附件只能发送 RClaw 的 `media/` 沙箱副本，不得直接把 `reports/` 原路径写入媒体标记。

报告生成完成（需数据抽检的已通过准出）且需要向用户交付完整文件时：

1. 该规则适用于所有外部 channel，不维护 channel 白名单。先读取 `skills/glm-master-skill/SKILL.md` 激活动态入口，再调用 `skill__glm_master_skill__publish_media`；只传结构化字段，不得自行选择解释器、脚本路径或命令行参数，也不得手填或猜测 channel：

   ```json
   {
     "path": "skills/ai-berkshire/reports/<报告文件>",
     "media_type": "file",
     "name": "<收件文件名>",
     "caption": "<报告说明>"
   }
   ```

2. 如果该 channel 已启用且媒体目录可用，动态工具会把文件复制到 `RCLAW_MEDIA_DIR/out`。RClaw 运行时会复核 stdout 中的 `MEDIA_MARKER=`、真实来源 channel 和沙箱内实际文件，并自动把有效标记保留到最终出站消息；无需手抄，也不得手写标记。多份报告仍须逐份调用动态工具。
3. 动态工具缺失、执行失败、只输出 `ARTIFACT=` 或没有有效媒体标记时，报告错误与本地文件路径，不得声称“附件已发送”；运行时也会拒绝这种没有媒体回执的成功声明。
4. `source_channel` 为 `cli`、`system`、对应外部通道未启用或该通道不支持媒体标记时，不生成媒体标记，只返回报告路径。

## 投研分析核心原则

- 客观、客观、客观：所有分析必须基于事实和数据，严禁主观臆断。
- 严格区分事实、观点和推测。事实要有来源，观点要说明推理链，推测必须标注不确定性。
- 不预设看多或看空。先摆数据，再推逻辑，最后给结论。
- 不要用“我认为”“显然”等词替代证据。优先写“数据显示”“根据公司披露”“根据某来源”。
- 每个核心判断都要给出反面证据或失效条件。
- 数据不足时明确说“不确定”或“数据不足”，不要用合理想象填满空白。
- 投资结论要明确，但必须同时写清价格、条件、风险和触发重新评估的信号。

## 数据与工具纪律

- 以当前 turn 上下文中的 `current_time` 作为“今天”和“最新”的时间基准；如果该字段缺失，再用 shell 获取系统日期。报告头部必须写明数据截止日期，不得用模型训练数据截止时间代替运行时日期。
- 关键财务数据至少使用两个独立来源交叉验证；来源差异超过 1% 要标注并解释。调用 `skill__ai_berkshire__financial_cross_validate` 时把 `tolerance` 显式设为 `1`，不要沿用工具的 2% 默认值。
- 数据抽检对每个来源只填写来源页面上实际出现的 `fetched_raw_value`、`fetched_raw_unit`、`fetched_evidence`（数值与单位必须相邻描述同一观测）、`fetched_source` 与 `fetched_locator`；第二来源填写对应的 `*2` 字段。`fetched_value` / `fetched_value2` 必须保持 `null`，由判决脚本统一换算，禁止模型直接填写归一化值。两个 source 必须是不同发布方的直接 `http/https` URL；来源名称、首页、工具计算说明、行业常识和口径解释都不是来源 URL。
- 新投研报告必须把所有可外部核验事实集中放进唯一一对 `<!-- audit:scope:start -->` / `<!-- audit:scope:end -->` 标记中，每行一个事实并显式写完整单位。先调用 `skill__ai_berkshire__report_lint` 全量检查事实池，通过后才调用 `skill__ai_berkshire__report_extract`；日期、年龄、任期、预测、情景参数、操作阈值、评分和工具计算值不得进入事实区。
- 必须用 `skill__ai_berkshire__report_extract` 直接生成审计包，再只填写来源证据字段。抽样固定为完整事实池的 15%（不足 20 项时至少抽 3 项或全量），seed 由事实池摘要确定；审计包同时绑定报告文本哈希、事实池、样本数量和样本摘要。不得手工重建结果数组、换比例/seed、删除失败项或修改 id、标签、报告值、单位、行号、原文及审计元数据。
- 联网工具读取的每个来源页面正文必须保存为 UTF-8 文本，并用 `skill__ai_berkshire__report_source_snapshot` 生成 URL 绑定快照；把工具打印的绝对快照路径和 SHA-256 填入 `fetched_snapshot` / `fetched_snapshot_sha256` 及第二来源对应字段，避免相对路径被按审计文件目录重复拼接。填完后调用 `skill__ai_berkshire__report_preflight`，它会验证快照哈希、精确 URL 绑定及摘录确实存在于所保存正文。来源快照工具本身不联网，只保护取证后的完整性，不能替代联网工具的真实读取；不得把自行编写的文字冒充页面正文。预检通过后才调用 `skill__ai_berkshire__report_verdict`，两者的 `report` 都必须传同一实际报告路径。
- `lint` 必须在抽样前一次性检查完整事实池；通过后若样本仍出现非事实、缺单位或错误量纲，停止通过改写报告刷新样本，保留失败输入并修复工具或交由人工复核。纯证据字段问题按预检的完整错误列表一次修复；数值真实不一致才修改事实账本和报告并从 `lint` 重走。不得在输入、命令和错误均未变化时原样重复调用。
- `skill__ai_berkshire__report_verdict` 会依据 `results_file` 中的稳定事实身份在 Skill 级目录建立证据观察账本；无关排版、事实行顺序、给结果文件改名，或复制/改名同一语义报告，都不会刷新账本。每个事实的两个来源槽位在首次判决时锁定，之后不得更换 URL、原始值、单位、摘录或定位；追加 query/fragment、切换 http/https、www 或等价的点路径/百分号编码也不能刷新身份。不得手工删除或覆盖账本、也不要删除锁文件；锁文件使用操作系统锁，进程崩溃时会自动释放，文件继续存在是正常现象。
- `skill__ai_berkshire__report_verdict` 准出时会在报告旁生成 `.audit-pass.json`；复用基线必须先调用
  `skill__ai_berkshire__report_verify_pass`，同时验证报告哈希、当前审计
  规则和证据账本哈希。没有有效旁路凭证时不得仅凭报告正文声称曾经准出。
- 抽检不通过时，只能修正报告后重新提取样本，或如实保持【打回】并交由人工复核；不得改写或更换已锁定来源、反复挑选来源直到碰到有利数值。严禁为了准出而把任一来源原始值、摘录或复核结果改成报告值，也严禁用“口径差异/假设参数”等说明包装复制值。估值情景、目标倍数、操作阈值等模型输入不属于外部事实，交给提取器排除，不得伪造两条来源。
- `skill__ai_berkshire__financial_cross_validate` 的 `values_file` 必须指向 `{"来源A": 数值, "来源B": 数值}` 扁平对象；先用文件工具写入包内 `skills/ai-berkshire/tmp/*.json`。`skill__ai_berkshire__report_extract` 使用 `output` 生成审计包，填写后依次由预检和判决工具读取。不要把 JSON 或超长 Python 程序内联到命令行。
- 市值必须独立验算：股价 × 总股本，并与报告市值对比。
- PE、ROE、FCF yield、三情景估值等计算优先使用已激活的金融计算专用工具，不要让 LLM 心算。
- 报告发布或作为决策依据前，优先使用已激活的报告审计专用工具做抽检。
- 雪球/社媒信息只能作为情绪或观点来源，不能替代公司公告、交易所披露、财报和权威数据源。
- 联网搜索失败、工具缺失、网页不可读时必须如实说明，不要编造数据或引用。

## 多 Agent 使用原则

AI Berkshire 的团队类 skill 应优先使用 RClaw 的 Claude Code 兼容团队工具：

- investment-team：商业模式、财务估值、行业竞争、风险管理层由四个真正独立的子 agent 分别研究并交叉质询；禁止由 team-lead 或单个子 agent 模拟四种视角。
- news-pulse：公司事件、监管政策、行业对手、市场情绪四个角色并行。
- earnings-team：财报资料抓取与四大师研究按阶段并行，编辑完成后再由读者顺序评审。

子 agent 输出必须自成一体：列出来源、关键事实、推理、结论和不确定性。Team lead 汇总时不能盲目平均四个结论，要处理冲突和证据强弱。

并行研究时，在同一条 assistant 消息中发起多个 `Agent` 或 legacy `Task` tool call；RClaw 会保持各子 agent 的独立上下文，并将其模型请求错峰发出。需要团队协作、互相挑战或实时进度时，先用 `TeamCreate` 创建团队，用 `TaskCreate`/`TaskUpdate` 维护任务板，用 `SendMessage` 在 agent 之间发送挑战、澄清和阶段性报告。Team lead 通过 `TaskList` 查看任务进展和收件箱消息；任务完成后，调用 `SendMessage(to="*", message="shutdown_request")`。运行时会在发送瞬间仅投递给实际 `is_active=true` 的成员；若返回 `skipped=true` 且 `recipients=[]`，表示当时没有活跃成员，可直接进入清理。不得根据任务文案或关键词推断是否跳过发送。等待成员结束后再调用 `TeamDelete`，并确认返回的 `success` 为 `true`。如果仍有 active member，等待后重试，不得把工具返回成功当作团队已经删除。

所有多 agent skill 统一遵守以下调用契约：

- 同一条 assistant 消息中连续发起的 `Agent`/`Task` tool call 不再受旧的“每批最多 4 个”执行器限制；在 `[llm].max_tool_calls` 的单次响应上限内（默认 256），需要多少独立角色、公司、持仓或地区，就创建多少真正独立的子 agent。所有子 agent 的实际 LLM 请求统一经过运行时错峰门，并对 429、网络错误和服务端过载执行指数退避重试；不得为了规避并发而让一个 agent 模拟多个角色。
- 每个调用使用运行时实际生效的字段：必填 `description`、`prompt`、`split_reason`，按需使用 `subagent_type`、`name`、`team_name`、`run_in_background`、`max_tool_iter`、`max_output_chars`。不要发送会被忽略的 `model`，也不要发送旧字段 `role`、`task`、`context`；这些内容全部写进 `prompt`。
- `max_tool_iter` 固定为 `256`。独立取证使用 `split_reason=independent_evidence`，可并行的外部资料抓取使用 `parallel_external_execution`，拿到新反馈后的复核或修订使用 `new_feedback`。
- 不在 skill 中自行加入固定 `sleep`；运行时统一负责模型请求错峰，并对 429、网络错误和服务端过载执行指数退避重试。`web_search` 还会在工具层把同一进程内的智谱搜索/网页读取请求统一错开至少 2 秒，并对瞬态 429/1302、网络错误和 5xx 最多尝试 4 次。单个角色失败时，先把该次原任务更新为 `failed`，再新建任务并只补做该角色；不得让同一 owner 遗留旧的 `running` 任务，也不得重启已经成功的整批。
- 协作 Agent 等待队友消息时使用 `TaskList(wait_timeout_ms=300000)`；不要用等待 Agent 最终退出的 `AgentWait` 代替邮箱等待。
- 角色之间通过 `SendMessage` 直发的消息会由运行时同步到 team-lead 收件箱，供 team-lead 核对协作闭环；角色无需把同一正文重复发送给 team-lead。需要阶段屏障的 skill 必须在每轮开始时声明参与者及各自动作，并在每条开始消息中附上使用实际轮次号和角色名的完整阶段完成回执模板；参与者完成分配动作后先成功发送结构化回执，再调用 `TaskList` 等待。Team lead 只等待该轮已声明参与者的回执，并结合已同步的 challenge/response 记录审计；不得等待未参与角色，也不得仅凭回执跳过证据核对。被分配 challenge 动作的参与者只有在收到并评估目标回应后才能发送回执。
- 对明确要求交叉质询的 `investment-team`、`earnings-team`，至少完成 1 轮，但**不预设固定轮数或轮数上限**。每轮结束后由 team-lead 根据关键分歧、证据冲突和未回应挑战是否仍存在，决定继续 `challenge_round_{N+1}_start` 或发送 `challenge_complete`；其他角色在收到 `challenge_complete` 前保持 `running`。若公开证据不足以消除分歧，team-lead 可在明确记录保留分歧及原因后结束质询，禁止为了凑轮数机械追问。其他多 Agent skill 仅在自身流程明确要求时执行交叉质询。

使用 `investment-team` 等“为每家公司分别调用 `TeamCreate`”的团队流程研究多个公司时，必须按公司串行：一家公司完成独立研究、该 skill 要求的交叉质询、汇总、报告写入、成员关闭和 `TeamDelete success=true` 后，才能为下一家公司创建团队。禁止同时创建多个公司团队，也禁止把两家公司的团队角色放在同一批工具调用中。全部单公司报告完成后，team-lead 可以在不新增角色 agent 的情况下基于已验证报告制作横向对比。`quality-screen`、`investment-checklist`、`portfolio-review` 等不为每家公司创建团队的批量独立取证流程仍按各自 skill 并行。

## 报告语言与风格

- 所有报告默认使用中文。
- 风格直接、清楚、有判断，不写泛泛而谈的平衡话术。
- 用表格呈现关键数据、情景估值、风险路径、行动阈值。
- 估计值必须标注“估计”，不要伪装成精确数据。
- 评分使用 1-5 星，不使用半星。
- 可使用巴菲特、芒格、段永平、李录式追问，但不要让名人语录替代证据。

## 边界

本包用于研究和学习，不构成投资建议。RClaw 可以生成明确的买入、观望、回避或持有建议，但必须呈现数据来源、假设、风险和适用价格区间。

不要主动执行原仓库 GitHub 推送、安装 Claude/Codex command、同步 Codex skills 等动作；这些不属于 RClaw 运行本投研包的职责。只有用户明确要求管理外部仓库时，才处理对应 Git 操作。
