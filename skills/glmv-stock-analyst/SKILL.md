---
name: glmv-stock-analyst
description: >
  股票分析与涨跌预测分析。
  在用户表达分析、判断或预测意图时触发，如“分析一下腾讯”、“0700最近走势如何”、“XX能不能买”、“预测一下后续走势”、“生成一份分析报告”等；
  支持港股、A股、美股，整合多源数据（包括新闻、基本面、技术面、资金流及宏观信息）进行多维综合分析，输出图文结合、包含可视化图表的结构化分析报告。
  对于简单查询类需求（如“腾讯当前价格是多少”、“茅台代码是什么”）不触发本skill, 直接通过web_search 能力搜索并总结。
  ⚠️ 需要在 skill 配置中接入多模态 LLM（默认 GLM-5V-Turbo），由配置的 LLM 读取图片并生成报告。
---

# stock-analyst v3.3

> **路径约定：**
> - 读取本 Skill 后，只调用 Manifest 动态激活的 `skill__glmv_stock_analyst__...` 专用工具；不要自行选择平台 runner、Python、脚本路径或参数
> - RClaw 的工作目录已是 agent workspace；不要添加 `agent/` 前缀，也不要切换 cwd
> - **数据输出**默认到 agent 当前工作目录（即 workspace）下的 `stock_data_output/`，不放在 skill 目录内
> - 数据采集工具通过固定的 workspace cwd 定位输出目录；不要切换到 skill 目录
> - `skill__glmv_stock_analyst__fetch_data` 会在 stdout 靠前位置输出 `OUTPUT_DIR=<绝对路径>`。后续文件工具和结构化专用工具必须逐字复用等号右侧的绝对路径；不要改成 `agent/stock_data_output/...`，也不要再次拼接 workspace

## 目录结构

```
{SKILL_DIR}/
├── SKILL.md                      # 本文件（流程指令）
├── config.toml                   # skill 集中配置：LLM、请求参数、可选数据源 token
├── scripts/
│   ├── setup.sh                 # ⭐ 环境初始化与可重复健康检查
│   ├── setup.ps1                # Windows 原生环境初始化与健康检查
│   ├── run.sh                   # Linux/macOS/Termux 脚本启动器
│   ├── run.ps1                  # Windows 原生脚本启动器
│   ├── venv/                     # Python 虚拟环境（平台 setup 脚本自动创建）
│   ├── fetch_all.py              # 数据采集 + 图表生成（纯数据，不写报告）
│   ├── glm5v_report.py           # 调用 GLM-5V-Turbo 看图并生成 report.md + meta.json + JSON 结果
│   ├── md2html.py                # Markdown → HTML；外部通道内嵌图、发布 HTML+MD 两个媒体标记
│   └── export_report.py          # Markdown → PDF/DOCX（仅按需导出，不再作为最终投递）
└── references/
    ├── analysis_guidelines.md    # ⭐⭐ 研究、预测、估值、治理与证据规则
    ├── report_template.md        # ⭐⭐ 含预测/DCF/来源登记的完整报告模板
    ├── hk_stock_knowledge.md     # 港股/A股专业知识
    └── sensitive_companies.md    # 敏感标的合规规则

stock_data_output/                # ← 输出目录（workspace 下，每次运行自动创建）
├── 0700_20260331_2030/           # ← 例：腾讯的一次分析任务
│   ├── data.json                 # 原始数据
│   ├── summary.json              # 数据摘要
│   ├── kline_em.png              # 日K线图
│   ├── kline_intraday.png        # 分时图
│   ├── glm5v_result.json         # GLM-5V-Turbo 输出结果（webchat_summary完整报告 + 流式聚合响应）
│   ├── meta.json                 # 结构化 KPI 数据，供聊天摘要和 HTML 顶部展示
│   ├── report.md                 # ⭐⭐ 模型写的完整Markdown详细报告（外部通道随消息发送）
│   └── report.html               # 🌐 render-report 生成；外部通道内嵌图片自包含，CLI 用 file:// 本地浏览
└── 00981_20260331_2032/          # ← 另一次任务（完全独立文件夹）
```

## 你的角色

你负责为中国投资者编排股票分析流程。用户主要关注中国公司（港股/A股/美股均有），也会看美股常见公司。最终报告由 skill 配置的多模态 LLM 生成，RClaw 负责确认标的、调用 Manifest 动态工具、传入必要上下文并返回工具结果。

**关键能力：配置的 GLM-5V-Turbo 可以直接看到图片。** 数据采集工具生成的 K 线图等必须交给 `skill__glmv_stock_analyst__generate_report` 发送给配置 LLM 分析。

---

## 工作原理（架构图）

```
┌─────────────┐   数据+图片(独立任务文件夹)   ┌──────────────────┐
│ fetch_data   │ ─────────────────────────→  │ generate_report │
│ 动态工具      │   stock_data_output/        │ 动态工具/LLM 看图  │
│             │   {code}_{timestamp}/        │ + RClaw上下文     │
│             │   ├─ data.json              │ + GLM-5V-Turbo    │
│             │   ├─ summary.json           │                  │
│             │   ├─ kline_em.png           │                  │
│             │   └─ kline_intraday.png     │                  │
└─────────────┘                            └────────┬─────────┘
                                                   │
                                    ┌──────────────┼──────────────┐
                                    ↓              ↓              ↓
                             ════════════   ════════════   ════════════
                             webchat回复     report.md     report.html
                             (工具返回)      (LLM生成)      (render_report)
                                             真实图片       CLI打开本地HTML
                                             详细分析        外部通道发HTML+MD
                                                            （HTML内嵌图片）
```

**核心原则：**
- **`skill__glmv_stock_analyst__fetch_data` 只负责采集数据和画图** — 不生成报告、不写 HTML
- **`skill__glmv_stock_analyst__generate_report` 调用配置 LLM 输出三段内容** — 结构化 META + 聊天窗口完整报告 + report.md 图文报告
- **每次任务独立文件夹** — 输出到 agent workspace 下的 `stock_data_output/`，方便追溯和导出PDF
- **MD是核心产物** — CLI/本地由 `skill__glmv_stock_analyst__render_report` 转 HTML 并打开浏览器；当前及未来已使能的 RClaw 外部 channel 由同一工具生成自包含 HTML（图片内嵌 base64）并同时发送 HTML + MD 两个文件，不再发送 PDF
- **所有数据和分析必须基于搜索和动态工具获取的真实信息** — 禁止编造数据、价格、事件

## 多模态 LLM 边界

**不需要多模态 LLM 的环节：**
- 确认股票代码、web_search 搜索新闻、整理补充上下文
- `skill__glmv_stock_analyst__fetch_data` 采集行情/财务/资金流/新闻/研报数据
- 图表生成：东方财富 K 线/分时图直链下载，周K/月K/估值/资金流/财务趋势等 `matplotlib` 本地绘图
- `skill__glmv_stock_analyst__fetch_ir` 下载 PDF 并提取研报/业绩演示页面图
- `skill__glmv_stock_analyst__render_report` 把 `report.md` 转成 HTML（外部通道自动内嵌图片 + 发送 HTML/MD）
- `skill__glmv_stock_analyst__export_report` 按需导出 PDF/DOCX（非默认投递；仅用户明确要求时使用）

**必须使用多模态 LLM 的环节：**
- Step 5 `skill__glmv_stock_analyst__generate_report`：把 `summary.json` 和图片发给 `{SKILL_DIR}/config.toml` 配置的 GLM-5V-Turbo，由它看图生成结构化 META、聊天窗口完整报告（`webchat_summary`）和 `report.md`
- 发送给 GLM-5V-Turbo 的图片包括：日K、分时、周K/月K、估值、资金流、财务趋势、研报图表、业绩演示图表等 `summary.json.images` 中存在的图片

如果没有可用图片，RClaw 应如实提示图表缺失；不要让 RClaw 主模型替代配置 LLM 凭空写技术面图形结论。

---

## 安装与依赖

### 环境初始化与健康检查

调用 `skill__glmv_stock_analyst__setup`，输入空对象 `{}`。运行时会根据当前平台固定选择受 Manifest 约束的初始化脚本。

`skill__glmv_stock_analyst__setup` 选择的平台初始化实现会自动：
1. 用 venv Python 导入核心模块进行健康检查；健康时立即退出，不运行 `pkg` 或 `pip`
2. venv 存在但依赖不完整时打印真实 import 错误并原地补装，不重新创建 venv
3. Windows 原生使用 `venv/Scripts/python.exe`；Linux/macOS 使用 `venv/bin/python`
4. 初始化实现路径包含 `/data/data/com.termux` 时直接判定为 Termux（并保留环境变量检测兜底）；Termux 下用 `pkg`/TUR 安装 NumPy、Pandas、Matplotlib 等原生包，并让 venv 复用系统 site-packages，避免 pip 在 Android 上源码编译
5. 其它 Python 依赖从 `requirements.txt` 安装；初始化工具会测速选源，Termux 下将可选的 PyMuPDF 跳过，缺失时仅跳过外部研报/IR PDF 图表提取，不影响按需用 `skill__glmv_stock_analyst__export_report`（fpdf2 + Pillow）导出 `report.pdf`；Termux 下同时跳过 akshare（其依赖 mini_racer/V8 引擎在 aarch64 无法编译，曾导致 pip 整体失败回滚、bleach/markdown 等装不上），`fetch_data` 对缺失的 akshare 会优雅跳过并回落东财原始接口
6. venv 目录损坏或 Python 链接失效时停止并明确报错，不覆盖已有目录

Agent 重启后可以再次调用 setup 专用工具做健康检查，但只能根据工具结果判断环境状态；不要因为新会话或错误的相对路径而另建 venv。普通平台依赖由 `requirements.txt` 管理；Termux 原生依赖由初始化工具通过 `pkg`/TUR 管理，不要自行尝试普通 `pip install numpy pandas`。

### GLM-5V-Turbo 配置

按智谱官方 GLM-5V-Turbo 文档流式调用接口：`https://open.bigmodel.cn/api/paas/v4/chat/completions`，鉴权使用 `Authorization: Bearer <api_key>`，请求固定携带 `stream=true`，本地图片按 `image_url.url` 传入 base64 字符串。

所有模型交互配置（包括 API Key）只从 `{SKILL_DIR}/config.toml` 读取，不读取环境变量。
运行前在该配置文件中填写 `api_key`，并在同一处维护其它选项：

```toml
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = ""
model = "GLM-5V-Turbo"
auth = "bearer"
max_tokens_key = "max_tokens"
connect_timeout_sec = 8
timeout_sec = 300
max_retries = 3
max_tokens = 128000
thinking = "enabled"
stream = true
image_limit = 8
max_json_chars = 60000
tushare_token = ""
```

配置项说明：

| 配置项 | 是否必填 | 用途 |
|------|---------|------|
| `base_url` | 必填 | GLM/OpenAI-compatible chat API 基础地址 |
| `api_key` | 必填 | GLM-5V-Turbo API Key，仅从本配置文件读取 |
| `model` | 必填 | 多模态模型名称 |
| `auth` | 必填 | 鉴权方式，目前脚本支持 `bearer` |
| `max_tokens_key` | 必填 | 请求体中 token 上限字段名 |
| `connect_timeout_sec` | 必填 | HTTP 连接超时秒数 |
| `timeout_sec` | 必填 | 流式读取空闲超时秒数；每收到一个分片即重新计时 |
| `max_retries` | 必填 | 可重试错误的最大重试次数 |
| `max_tokens` | 必填 | LLM 输出 token 上限 |
| `thinking` | 必填 | 固定为 `enabled`；脚本拒绝 `disabled` 或空值 |
| `stream` | 必填 | 固定为 `true`；脚本拒绝关闭流式响应 |
| `image_limit` | 必填 | 发送给多模态 LLM 的图片数量上限 |
| `max_json_chars` | 必填 | 传给 LLM 的 summary JSON 最大字符数 |
| `tushare_token` | 可选 | Tushare Token，仅从本配置文件读取 |

### 可选：Tushare Token

如需更稳定的 A 股/港股数据，在 `{SKILL_DIR}/config.toml` 中填写
`tushare_token`。脚本不读取环境变量，也不会把 token 写入日志或产物。

---

## 完整分析流程（每次必做）

RClaw 仍负责确认标的、调用数据采集工具、必要时补充 web_search/用户上下文；但**看图出报告必须由 `skill__glmv_stock_analyst__generate_report` 调用 skill 配置的 GLM-5V-Turbo 完成**。RClaw 主模型不要自行看图写 `report.md`，也不要在工具结果外补写投资结论。

### Step 0：确认标的代码（每次必做）

**不论你是否认得这只股票，都必须先搜索一次。**

搜索内容：`{用户说的名字} 股票代码 上市 港股 OR A股 OR 美股`，**时间范围选 past_year**
本搜索借用已有web_search能力，
**如果搜索失败，继续尝试其他有web_search能力的工具，如果全部失败提醒用户需要先配置一种web_search工具，或直接给出股票代码**

根据搜索结果：
- 有明确代码 → 记录代码，检查 `references/sensitive_companies.md`，进 Step 1
- 说"未上市" → 再搜 `{名字} IPO 上市 2025 OR 2026` 确认
- 名字模糊 → 列选项让用户确认
- 无结果 → 告知未找到

代码格式：港股 `0700.HK` | A股 `600519.SS` | 美股 `AAPL`

---

### Step 1：采集数据

调用 `skill__glmv_stock_analyst__fetch_data`：

```json
{"stock":"0700.HK","adr":"TCEHY"}
```

其它示例只需替换 `stock` 为 `00981.HK`、`600519.SS` 或 `AAPL`。

> **环境说明：** 首次使用或专用工具报告模块导入错误时，调用上面的 setup 专用工具。健康的现有 venv 会被直接复用。

数据采集工具运行完成后会输出到 agent workspace 下的 `stock_data_output/{code}_{时间戳}/`：

| 文件 | 内容 | 用途 |
|------|------|------|
| `data.json` | 全部结构化原始数据 | 模型读取分析 |
| `summary.json` | 数据摘要 | 快速概览 |
| `kline_em.png` | 日K线图（东方财富） | ⭐ 核心技术分析依据 |
| `kline_intraday.png` | 分时图 | 当日走势 |
| *(可能还有)*周K/月K/估值/资金流图 | 其他图表 | 补充分析 |

**重要：记录 stdout 的 `OUTPUT_DIR=<绝对路径>`，把等号右侧绝对路径原样作为 `{output_dir}`，后续步骤都逐字复用。不得添加或删除 `agent/`，不得再拼接 workspace。**

---

### Step 2：读取数据与准备图片（核心步骤）

#### 2a. 读取 summary.json 和 data.json

```bash
# output_dir 是 stdout 的 OUTPUT_DIR= 后面的绝对路径
read("{output_dir}/summary.json")
read("{output_dir}/data.json")
```

重点关注：
- `images` 字段 → 所有可用图片路径
- 各维度数据 → 基本面/技术面/资金流向

#### 2b. 确认关键图片存在

RClaw 不需要自己看图写报告，但要确认 `summary.json` 的 `images` 字段中至少包含关键图片。Step 5 的 `skill__glmv_stock_analyst__generate_report` 会把这些图片发送给配置的 GLM-5V-Turbo 看图分析。

必需优先发送：
- 日K线图（最重要）：`kline` / `kline_em.png`
- 分时图（当日走势）：`kline_intraday`
- 有则发送：周K、月K、估值、资金流、财务趋势、研报/IR 图表

---

### Step 3：补齐关键证据与精准新闻（必做）

**⚠️ 先找法定披露和公司原始材料，再找新闻与外部观点；新闻必须精准相关。**

根据数据采集工具已有数据补缺，用 web_search 搜索以下内容：

1. `"{股票名称}" "{股票代码}" 年报 中报 业绩公告 投资者关系 {当前年份}` — 优先取得交易所/公司法定披露、业绩公告和指引
2. `"{股票名称}" "{股票代码}" 最新 {当前年份}年{月}` — 近期事件和动态
3. `"{股票名称}" 分析师 一致预期 盈利预测 评级 目标价` — 外部预测与专业观点
4. `"{股票名称}" 资金流向 南向资金 卖空` — 适用标的的交易结构信息

为填充独立研究章节，在法定披露和已有数据不足时再按缺口搜索：

- `"{股票名称}" 公司治理 董事会 控股股东 关联交易 股权激励` — 管理层与治理
- `"{股票名称}" 资本开支 并购 回购 分红 市场份额` — 资本配置与竞争壁垒证据

按 `references/analysis_guidelines.md` 的证据优先级筛选。预测研究至少尝试取得最新法定财务、公司指引和可识别日期/机构的外部一致预期；获取不到时明确缺失，不用历史 CAGR 代替。

`web_search` 结果仅用于发现来源；采用前必须打开原始页面核对发布主体、日期、期间、单位和原文。搜索摘要、AI 摘要或转载片段不能单独作为证据。页面无法打开或关键数字无法在原文确认时，不得写入报告；所谓“一致预期”只有在来源明确给出机构/样本、统计日期、预测期间和指标时才能使用。

**新闻筛选原则：**
- ✅ 直接提到该股票名称或代码的新闻
- ✅ 该公司发布的公告/财报/业绩指引
- ✅ 直接影响该标的的行业政策/监管变化
- ✅ 该公司相关的重要人物言论
- ❌ 全市场快讯（财联社滚动未匹配到该标的的）
- ❌ 其他不相关的个股新闻
- ❌ 泛宏观新闻（除非直接影响该标的）

将每项采用证据整理为 `[S#] 来源级别/类型｜发布主体｜文档/数据名称｜发布日期｜数据期间/行情时点｜获取时间｜URL/文件/字段定位｜可核实事实`，连同筛选后的法定披露、公司指引、外部预测、新闻和交易结构要点传给 Step 5 动态工具的 `extra_context`。来源级别按 `analysis_guidelines.md` 的 1-6 级填写；定位尽量写到网页 URL、PDF 页码/表名或 JSON 字段路径，不得编造链接。如果没有拿到强相关新闻，明确写“暂无强相关新闻”，但仍传入已取得的财报和其他关键证据。

---

### Step 4：业绩演示材料（可选但推荐）

```
web_search "{公司名} 业绩演示 投资者演示 PPT {当前年份}"
```
找到 PDF 后可调用 `skill__glmv_stock_analyst__fetch_ir` 提取图表。找不到就跳过，不要卡住。

若报告采用该 PDF 或提取图表，必须把发布主体、文档名称、发布日期、原始 URL、使用页码/图表名和本地图片 filename 加入 Step 3 的来源登记；原始 PDF、页码或图表数字无法核实时，不得引用精确数值。

> **`skill__glmv_stock_analyst__fetch_ir` 安全说明：** 远程输入仅接受 HTTPS PDF URL（拒绝 file://、http:// 等非安全 scheme），下载大小限制 50 MB，最大处理 50 页。

---

### Step 5：⭐⭐ 生成 report.md + 输出聊天窗口完整报告

这是最关键的一步：**不要由 RClaw 主模型自己看图写报告**，必须调用 `skill__glmv_stock_analyst__generate_report`，让 skill 配置的 GLM-5V-Turbo 读取图片并按「三段式分隔符协议」生成产物（`<<<META_BEGIN/END>>>` + `<<<WEBSUMMARY_BEGIN/END>>>` + `<<<REPORT_BEGIN/END>>>`，由工具底层解析，不再要求模型输出整段 JSON，避免长文本进 JSON 导致解析失败）：
- `meta`：严格结构化 META（投资观点 / 行动建议 / 风险等级 / 观点置信度 / 数据截至 / 币种 / 现价 / 目标价及期限 / 估值方法 / 上行空间% / PE_ttm / PB / 股息率% / 支撑位 / 压力位 / 止损位）。工具会校验协议结构、枚举、类型、字段依赖和上行空间公式，再把它格式化成 KPI 行拼到 `webchat_summary` 开头，并写入 `meta.json` 供渲染工具在 HTML 顶部生成 KPI 条
- `webchat_summary`：聊天窗口完整分析报告（开头已自动拼接 KPI 行），不是摘要
- `report.md`：图文并茂的完整 Markdown 报告

把 Step 3 形成并由 Step 4 补充的来源登记和采用事实原样作为 `extra_context` 传入，包括法定披露、公司指引、外部预测、新闻、交易结构和采用的业绩演示材料。只有在工具数据之外没有取得任何补充证据时才传空字符串；不得只传新闻而丢失更高优先级来源。

```json
{"output_dir":"{output_dir}","user_request":"{用户原始需求}","extra_context":"{Step 3 的来源登记与补充事实，可为空}"}
```

如果补充上下文较长，必须用文件工具将它直接写到绝对路径 `{output_dir}/extra_context.txt`，然后把同一个绝对路径原样传给专用工具：

```text
write(path="{output_dir}/extra_context.txt", content="{Step 3 的来源登记与补充事实}")
```

```json
{"output_dir":"{output_dir}","user_request":"{用户原始需求}","extra_context_file":"{output_dir}/extra_context.txt"}
```

`skill__glmv_stock_analyst__generate_report` 会：
1. 读取 `{output_dir}/summary.json`、`{output_dir}/data.json` 和 `references/` 下的分析规则、报告模板、市场知识、敏感标的规则
2. 读取 `summary.json` 中列出的 K 线、分时、资金流等图片
3. 按 `{SKILL_DIR}/config.toml` 流式调用 GLM-5V-Turbo，要求其按三段式分隔符协议输出
4. 写入 `{output_dir}/report.md` 与 `{output_dir}/meta.json`
5. 写入 `{output_dir}/glm5v_result.json`（含 `webchat_summary`、`meta`、`report_md`、`protocol_valid` 等键）
6. 在 stdout 的 `__GLM5V_RESULT_BEGIN__` 和 `__GLM5V_RESULT_END__` 之间输出最终 JSON（含 `webchat_summary`、`meta`、`report_md`、`protocol_valid` 等键）

**流式响应、思考模式与重试是强制约束：** GLM-5V-Turbo 请求固定携带 `stream=true` 和 `thinking={"type":"enabled"}`。连接超时、流读取空闲超时和网络重试次数分别读取 `connect_timeout_sec`、`timeout_sec`、`max_retries`；这只是单次传输故障的有限重试，不是报告结构审计的返工上限。只要持续收到 SSE 分片，总分析时间可以超过读取空闲超时。工具仅聚合 `delta.content`，不得打印、保存或混入 `delta.reasoning_content`。客户端连接/读取失败、HTTP 408/504 或流在 `finish_reason` 前异常结束时，必须丢弃本次全部未完成内容，再按原请求重试；所有尝试均不得关闭思考模式。`skill__glmv_stock_analyst__generate_report` 的 Manifest 已固定 `timeout_ms=1500000`（25 分钟），不得退回 shell 手写脚本命令或缩短进程超时，也不能在工具完成重试前终止进程。RClaw 不得把 `config.toml` 的 `thinking` 改为 `disabled`，不得删除该字段，也不得通过修改请求体、临时配置或其它降级方式关闭思考模式。自动重试仍失败时，应如实报告错误；如需再次执行，只能用完全相同的参数重调该专用工具，并继续保持 `stream=true`、`thinking="enabled"`。

工具会先确定性规范化可无损修复的标记/META，再对剩余结构问题调用同一模型返工。结构返工不设固定次数上限；只要审计问题数量持续下降就继续，连续两轮问题数不下降或问题状态进入循环时才停止并列出全部问题。RClaw 只读取最终 `protocol_valid=true` 的返回 JSON 中 `webchat_summary` 作为聊天回复正文，完整输出到聊天窗口，不要自行压缩、总结或改写投资结论。仅允许按 Step 6/7 在末尾追加投递说明；已使能的外部 channel 由 `skill__glmv_stock_analyst__render_report` 返回 HTML 与 MD 媒体标记，RClaw 自动校验并保留，模型不得手抄或追加媒体标记。PDF 不自动生成或投递。工具因连续无进展或调用失败退出时，如实报告其全部错误。

---

### Step 6：⭐⭐ 按当前会话通道投递最终产物（HTML + MD）

RClaw 会向 Manifest 专用工具的子进程环境注入当前会话的真实 `source_channel`，不要在参数中手填或猜测：

- `source_channel` 为任一已使能的外部 channel（当前包括 `weixin`、`napcat`/`napcatqq`，未来 channel 使用同一约定）：调用 `skill__glmv_stock_analyst__render_report` 生成自包含 HTML（图片内嵌 base64），**并同时发送 HTML + MD 两个文件**，不发送 PDF。
- `cli` 且非 daemon（`RCLAW_DAEMON != 1`）：调用同一 `skill__glmv_stock_analyst__render_report` 生成 HTML（本地 `file://` 引用图）并打开浏览器；`report.md` 已由 Step 5 生成，无需额外投递。
- 其它情况（通道未使能、daemon 下的 cli/system）：只说明产物已生成，不打开浏览器、不发媒体标记。

#### 6a. 外部通道：生成并发送 HTML + MD（不发送 PDF）

对任一已使能的外部 `source_channel`，调用 `skill__glmv_stock_analyst__render_report`：

```json
{"output":"{output_dir}/report.html","input":"{output_dir}/report.md","meta":"{output_dir}/meta.json"}
```

`skill__glmv_stock_analyst__render_report` 在外部通道下会自动：
- 把本地图片内嵌为 base64 data URI，生成自包含 HTML（收件端手机/电脑离线即可看图，不依赖 agent 本地文件）
- 应用专业 CSS 样式（表格渐变头、响应式、打印优化）
- 从同目录 `summary.json` 读取股票名称与代码，并按 `股票名称_股票代码_YYYYMMDD_HHMMSS.ext` 命名两个附件；时间取 `report.md` 的生成时间，精确到秒，例如 `中国石油_601857_20260723_201855.html`
- 依次打印两行 `MEDIA_MARKER=...`：第一行 HTML，第二行源 Markdown

RClaw 会读取工具 stdout，按先 HTML 后 MD 的顺序校验每行 `MEDIA_MARKER=...`，并自动把有效
标记保留到最终出站消息。不得手抄或根据预期输出路径手写媒体标记。通道会解析
标记并把对应文件发给用户，标记本身不显示。

- `weixin`、`napcat` / `napcatqq` 的 marker 差异由工具内部处理。这里不提供可复制的
  marker 模板，避免模型把示例路径误当成工具已经成功发布的真实回执。
- 未来 channel：RClaw 注入真实 `source_channel` 且该 channel 以
  `RCLAW_<CHANNEL>_ENABLED=1` 声明媒体能力后，工具会输出与该 channel 匹配的标记，
  无需修改 skill 流程；所有 channel 仍共用 `RCLAW_MEDIA_DIR`。
- 如果没有打印 `MEDIA_MARKER=`（通道未使能或 `RCLAW_MEDIA_DIR` 缺失）：只在回复中说明 `report.html` 与 `report.md` 已生成（含路径），不追加媒体标记。

外部通道**禁止**自动调用 `skill__glmv_stock_analyst__export_report` 导出 PDF，也**禁止**追加 PDF 媒体标记。PDF 仅作为"按需导出"能力保留（见 Step 7）。

#### 6b. CLI/本地：生成 HTML 并打开浏览器

仅当 `source_channel=cli` 且非 daemon 时调用同一个 `skill__glmv_stock_analyst__render_report`：

```json
{"output":"{output_dir}/report.html","input":"{output_dir}/report.md","meta":"{output_dir}/meta.json"}
```

CLI 通道下 `skill__glmv_stock_analyst__render_report` 不会内嵌图片、不打印媒体标记，图片用 `file://` 绝对路径供本地浏览器直接显示。

生成后按平台打开：Windows 用 `start "" "{output_dir}/report.html"`；macOS 用 `open "{output_dir}/report.html"`；Linux 桌面用 `xdg-open "{output_dir}/report.html"`；Termux 仅在已安装 `termux-api` 且 `termux-open` 可用时执行 `termux-open "{output_dir}/report.html"`，否则只返回文件路径。

`skill__glmv_stock_analyst__render_report` 会自动：
- 应用专业 CSS 样式（表格高亮渐变头、响应式、打印优化）
- 将 Markdown 表格/代码块/引用块/图片转为 HTML
- 图片相对路径自动转为 `file://` 绝对路径（浏览器可直接显示）

---

### Step 7：📝 后续重发 / 按需导出 PDF

- 外部通道：HTML 与 MD 已按 Step 6a 随最终回复发送，不要重复发送，也不要主动询问是否导出 PDF。
- CLI/本地：HTML 已在浏览器打开，`report.md` 即源文件；用户想要可编辑/归档版本时直接给出 `{output_dir}/report.md` 路径。
- 其它/未使能通道：说明 `report.html` 与 `report.md` 已生成并给出路径。

**用户明确要求 PDF/DOCX 时（按需导出，非默认投递）调用 `skill__glmv_stock_analyst__export_report`：**

PDF：

```json
{"input":"{output_dir}/report.md","format":"pdf","output":"{output_dir}/report.pdf"}
```

DOCX：

```json
{"input":"{output_dir}/report.md","format":"docx","output":"{output_dir}/report.docx"}
```

PDF 由 `skill__glmv_stock_analyst__export_report`（fpdf2）生成；注意 fpdf2 会截断超长表格单元格、剥离内联格式，信息完整度不如 HTML，因此只在用户明确要 PDF 时使用。若 stdout 有有效 `MEDIA_MARKER=...`，RClaw 会自动保留标记并发送对应的 PDF/DOCX。CLI 前台如需打开导出文件，沿用 Step 6b 的平台命令并替换文件名；Termux 无 `termux-open` 时只返回文件路径。

---

## 数据源一览

### 数据采集工具内置源

| 类别 | 数据源 | 用途 | 说明 |
|------|--------|------|------|
| K线图(图片) | 东方财富 webquotepic | 日K、分时图直链下载 | ✅ 稳定可靠 |
| K线图(数据) | akshare / yfinance / tushare | 周K、月K数据(本地绘图) | ⚠️ 代理问题 |
| 个股行情 | 东方财富 quote.eastmoney.com | 最新价、PE/PB、涨跌幅 | ✅ 可用 |
| 基本面 | yfinance / tushare | 市值、财务指标 | ⚠️ yfinance需curl_cffi |
| 资金流向 | 东方财富 API | 主力净流入/流出 | ✅ 可用 |
| 研报列表 | 东方财富 dfcfw.com | 券商评级、目标价 | ✅ 可用 |
| 新闻 | 财联社 cls.cn | 实时快讯 | ❌ 过滤不精准，优先用 web_search 补充 |
| 宏观 | (外部API) | 利率、PMI等 | ⚠️ 未稳定配置 |

### RClaw 补充源（Step 3 手动搜索）

| 工具 | 搜索内容 | 优势 |
|------|---------|------|
| web_search | "{股票名} 最新新闻" | 精准匹配标的 |
| web_search | "{股票名} 分析师 评级" | 专业观点 |
| web_search | "{股票名} 资金流向 南向" | 资金面动态 |

---

## 用户追问处理

| 追问类型 | 处理方式 |
|---------|---------|
| "那 XX 呢？" | 新股票走完整流程（新任务文件夹） |
| "XX 最新消息" | 只搜新闻 + 精炼回复 |
| "如果明天低开？" | 场景分析（基于已有数据） |
| "比较 XX 和 YY" | 各自分析后对比 |
| "导出 PDF" | Step 7 导出流程 |
| "再看看 XX" | 同一只股票重新调用数据采集与报告工具（新时间戳文件夹） |

#### 简单行情查询必须用当天实时/盘中数据（不触发本 skill 时）

用户说"XX 怎样了 / XX 行情 / 帮我看下 XX"等只想要当前行情的简单查询，按 frontmatter 不触发完整分析流程，直接走 web_search。但 **web_search 默认返回的多是上一交易日收盘快照，用户会因"给的还是昨天的数据"而不满**。务必：

1. 搜索 query 必须带上**今天日期 + "今日/实时/盘中"**，例如 `长江电力 600900 今日行情 实时 {今日年月日}`，不要只搜 `{股票名} 股价 最新行情`（这类老 query 命中的是收盘页缓存）。
2. 返回前核对页面时间戳是否为当天盘中/当日；若搜索命中的仍是"昨收/上一交易日"，补搜 `{股票名} {今日日期} 盘中` 或抓实时行情源（东方财富 quote、新浪/同花顺盘中页）。
3. 明确标注数据时点（如"截至 {今日} {HH:MM} 盘中"），不把前日收盘当成最新。

---

## 重要规则（踩坑总结）

1. **先搜代码再调用工具** — 永远不用记忆中的代码
2. **`skill__glmv_stock_analyst__fetch_data` 只采集数据，不写报告** — report.md 由 `skill__glmv_stock_analyst__generate_report` 调用 GLM-5V-Turbo 产出
3. **必须让 GLM-5V-Turbo 看图** — RClaw 主模型不直接看图时，必须通过 `skill__glmv_stock_analyst__generate_report` 发送图片给多模态模型
4. **每次任务独立文件夹** — 路径从工具 stdout 取，不同次运行互不干扰
5. **不编造、不补数** — 未在工具结果、图片或已核实原始来源中出现的公司事实、历史数字、文件、URL 和外部预测一律不得生成；本报告预测和估值参数只能由已列事实可复核地计算或设为有依据的条件情景，无法支持时明确缺失
6. **webchat 总结基于实际搜索和数据** — 禁止编造股价、事件、财务数据
7. **新闻要精准** — 不相关的全市场快讯不要塞进报告
8. **图片策略分层** — report.md用真实图，webchat不放文本折线图
9. **K线图放报告最前面** — 不是塞在章节末尾，是读者第一眼看到的
10. **语种跟随用户** — 术语保留英文
11. **敏感标的合规** — 见 `references/sensitive_companies.md`
12. **外部 channel 发送 HTML + MD，不发送 PDF** — 运行时 `source_channel` 为任一已使能外部 channel 时，调用 `skill__glmv_stock_analyst__render_report` 生成自包含 HTML（图片内嵌）并返回 HTML 与 MD 两个媒体标记；RClaw 自动校验并保留，模型不得手抄或追加标记。禁止自动导出 PDF（PDF 仅在用户明确要求时按 Step 7 按需导出）
13. **report.md 是核心产物** — HTML 从它派生；MD 本身也作为外部通道的投递物之一发送
14. **执行顺序** — report.md → `skill__glmv_stock_analyst__render_report` 生成 report.html（外部 channel 内嵌图 + 打印 HTML/MD 媒体标记；CLI 用 file:// 并打开浏览器）→ RClaw 自动保留有效 `MEDIA_MARKER=` → 回复用户。外部 channel 会话不打开浏览器
15. **法定披露优先且逐项可追溯** — 关键数字使用来源 ID，并在完整报告登记来源级别/类型、文档、日期、期间和 URL/文件/字段定位
16. **预测和估值必须勾稽** — 未来 2-3 年预测、利润桥、三表、DCF 股权价值桥和稀释股数遵守 `analysis_guidelines.md`；数据不足时不强行给预测或目标价
17. **Termux 环境坑（2026-09-08 实测）** — `setup` 的事务性 `pip install -r requirements.txt` 会被 akshare 的 `mini_racer` 依赖（V8 引擎，Termux aarch64 无法源码编译）整体失败回滚，连带 `bleach`/`markdown` 等纯 Python 依赖装不上，导致 `render_report` 报 `ModuleNotFoundError: No module named 'bleach'`。现 setup 已在 Termux 下自动排除 akshare；若历史 venv 仍缺模块，用 **系统级** `pip3 install bleach markdown fpdf2 -i <速度最快镜像>` 单独补装（Termux 下 venv 复用系统 site-packages，装完 venv python 即可导入）。注意排障用的 shell 命令里不得出现本技能 venv/脚本路径，否则被 SKILL_ENTRYPOINT_REQUIRED 门禁拦截，改用不带路径的 `pip3`/`python3`
18. **generate_report 耗时与超时（2026-09-09 实测）** — 思考模式下 GLM-5V-Turbo 会先输出数千行 reasoning 再写正文，健康的一次完整生成实测约 7-25 分钟（ streaming 持续有数据，不是挂起）；generate-report 的 Manifest 超时已放宽到 55 分钟，脚本自带墙钟预算（`attempt_timeout_sec=1500` 限单次尝试、`total_timeout_sec=3200` 限含重试与返工的整次调用），只有真正死流才会被中断并自动重试，HTTP 429/408/504 会退避重试；总预算先于 Manifest 超时耗尽时干净报错。若工具报错且任务目录只有采集数据与图片、没有 report.md/meta.json，先用 `list` 检查任务目录确认无产物，再用完全相同参数重调；等待期间不要提前 /cancel，也不要并行再调一次（key 并发配额只有 1，会互相顶掉）
