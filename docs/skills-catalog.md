# 内置技能目录

[← 返回 README](../README.md)

仓库 `skills/` 下共 **40 个顶层技能**（含 4 个多子技能包），按类别：智谱 GLM 官方族 16、图像视频生成 7、投研金融 7、办公文档 4、电商与社媒抓取 4、工具类 2。技能机制见 [技能系统](skills.md)。GLM 族、图像视频、投研金融与 `cron-task-patterns` 的详细说明在 `skills/doc/<名称>.md`；办公文档、抓取与 `extract-archive` 直接看各自的 `SKILL.md`。

**Key 文件标记**（这些文件以空占位入库、未 gitignore，填入 Key 后勿提交）：

| 标记 | 文件 | 说明 |
|---|---|---|
| KZ | `skills/glm-master-skill/config.toml` | `[zhipu].api_key`，GLM 技能族共用（`glmv-stock-analyst` / `glm-quota-query` 除外，见 KS） |
| KS | `skills/glmv-stock-analyst/config.toml` | `api_key` + `tushare_token` |
| KA | `skills/ark_key.toml` | 火山方舟 `[ark].api_key` |
| KH | `skills/happyhorse_key.toml` | 阿里百炼 `[happyhorse].api_key` |
| KM | `rclaw.toml` 的 ZCode 金融 MCP 条目 | 可选，见 [MCP 扩展](mcp.md) |

技能目录每轮只以标题 + 60 字符描述进系统提示（渐进披露），模型按需读取 `SKILL.md` 后才激活其工具。

## 智谱 GLM 官方技能族（16：15 个移植自 zai-org/GLM-skills，`glm-quota-query` 为 RClaw 自研）

| 技能 | 用途 | Key | 运行依赖 |
|---|---|---|---|
| `glm-master-skill` | GLM 技能总路由与运行规范：附件意图门禁、统一配置读取、能力分诊、媒体发布；只路由不调 API | — | Python 标准库 |
| `glmocr` | GLM-OCR 通用文字识别（图片 / PDF） | KZ | requests |
| `glmocr-table` | 表格识别，输出 Markdown | KZ | requests |
| `glmocr-formula` | 数学公式识别，输出 LaTeX | KZ | requests |
| `glmocr-handwriting` | 手写文字识别 | KZ | requests |
| `glmocr-sdk` | GLM-OCR SDK 批量解析（Markdown + 结构化 JSON） | KZ | requests，可选官方 SDK |
| `glm5v-multimodal` | GLM-5V 通用多模态分析：视觉问答、多图对比、图表 / 文档问答、结构化提取 | KZ | requests；视频需 ffmpeg |
| `glmv-caption` | 图片 / 视频 / 文档内容描述与摘要 | KZ | requests |
| `glmv-doc-based-writing` | 基于文档写作：读入 PDF / DOCX 产出 Markdown 文章 | KZ | requests |
| `glmv-pdf-to-ppt` | PDF 转结构化 PowerPoint | KZ | PyMuPDF / Pillow / requests、Poppler、Node + pptxgenjs |
| `glmv-pdf-to-web` | PDF 转单页学术 / 项目网站 | KZ | PyMuPDF / Pillow / requests、Poppler |
| `glmv-prompt-gen` | 从图片 / 视频反推专业提示词 | KZ | requests |
| `glmv-resume-screen` | 多简历批量筛选，输出对比表 | KZ | requests |
| `glmv-stock-analyst` | 股票多维综合分析（港股 / A 股 / 美股），产出图文报告 | KS | pandas / numpy / matplotlib / akshare / yfinance / tushare / PyMuPDF / fpdf2 等 |
| `glm-image-gen` | GLM-Image 文生图（科学插画、人像、海报） | KZ | Python 标准库 |
| `glm-quota-query` | 查询智谱 GLM Coding Plan 实时额度与用量 | KS | PowerShell 或 curl / sed / awk |

## 图像 / 视频生成（7）

| 技能 | 用途 | Key | 运行依赖 |
|---|---|---|---|
| `seedance-video` | 火山方舟 Seedance 视频生成与编辑（文生视频、首尾帧、参考生成、续写） | KA | Python 3.11 标准库 |
| `sd25-pe` | Seedance 官方提示词工程：粗文案编译为可提交 prompt | — | 无脚本 |
| `seedream-image` | Seedream 图片生成 / 编辑 / 多图融合 / 组图 | KA | Python 3.11 标准库 |
| `seedream-prompt-gen` | Seedream 图片提示词写作 | — | 无脚本 |
| `happyhorse-t2v` | 阿里百炼 HappyHorse 文生视频 | KH | Python 3.11 标准库 |
| `happyhorse-i2v` | HappyHorse 图生视频（首帧驱动） | KH | Python 3.11 标准库 |
| `happyhorse-r2v` | HappyHorse 参考图生视频（多张参考图控主体 / 风格） | KH | Python 3.11 标准库 |

## 投研 / 金融（3 个技能 + 4 个技能包）

| 技能 / 包 | 用途 | Key | 运行依赖 |
|---|---|---|---|
| `ai-berkshire`（包） | 四大师价值投资研究包，23 个子技能：`bottleneck-hunter` `daily-stock-analysis` `deep-company-series` `dyp-ask` `earnings-review` `earnings-team` `era-alpha` `financial-data` `income-investment` `industry-funnel` `industry-research` `investment-checklist` `investment-memo-craft` `investment-research` `investment-team` `management-deep-dive` `news-pulse` `portfolio-review` `private-company-research` `quality-screen` `thesis-drift` `thesis-tracker` `wechat-article`，配 A 股行情 / 财务 / 估值数据入口 | 台股数据可选 FinMind token（环境变量 `FINMIND_TOKEN` 或 `local/finmind_token.txt`） | 标准库，可选 Playwright |
| `evaluate-fund` | 公募基金 / ETF / FOF / QDII / REIT 系统评估与持仓分析，产出 Markdown / 自包含 HTML 报告 | 可选 KS（识图） | Python 标准库 |
| `screen-funds` | 全市场基金候选池：硬过滤 + 分类评分 + 稳健性检查 | — | Python 标准库 |
| `analyze-gold-market` | 人民币金价与黄金行情评估，情景化操作计划（不代客下单） | — | 联网工具 |
| `pick-funds`（包） | 基金研究包，5 个子技能：`fund-profile` `fund-screen` `fund-watch` `holdings-style` `manager-profile` | KM | akshare 兜底 |
| `read-macro`（包） | 宏观与策略包，4 个子技能：`macro-dashboard` `index-valuation` `asset-allocation` `policy-tracker` | KM | numpy / pandas / akshare |
| `watch-positions`（包） | 持仓监控包，6 个子技能：`intraday-watch` `close-recap` `event-monitor` `watchlist` `report-render` `xlsx-author` | KM | akshare，可选 reportlab / pypdfium2 / matplotlib / openpyxl |

## 办公文档（4）

| 技能 | 用途 | Key | 运行依赖 |
|---|---|---|---|
| `docx` | Word 文档创建 / 编辑 / 格式化 / 批注 / 转换，内置学术、合同、公文、简历等场景模板 | — | Node docx、pandoc、python-docx、defusedxml、LibreOffice / Poppler |
| `pdf` | PDF 生成 / 处理 / 转换，含海报、报告、简历等排版方案 | — | reportlab / pypdf / PyMuPDF / pikepdf / pdfplumber，可选 Node / Playwright / LaTeX / LibreOffice |
| `pptx` | 演示文稿创建 / 编辑 / 转换 | — | pptxgenjs / Playwright / sharp、markitdown / defusedxml、LibreOffice / Poppler |
| `xlsx` | 表格创建 / 分析 / 编辑 / 转换，含图表、VBA、财务模板 | — | openpyxl / XlsxWriter，按需 LibreOffice |

## 电商与社媒抓取（4）

| 技能 | 用途 | Key | 运行依赖 |
|---|---|---|---|
| `jd-scrape` | 京东商品 / 列表采集 | — | 内置 CDP 浏览器 |
| `taobao-scrape` | 淘宝 / 天猫采集 | — | 内置 CDP 浏览器 |
| `xhs-scrape` | 小红书笔记采集 | — | 内置 CDP 浏览器 |
| `xianyu-scrape` | 闲鱼商品采集 | — | 内置 CDP 浏览器 |

这四个技能依赖 `browser_*` 工具，因此仅在桌面 GUI 与安卓 APK 可用（见 [内置工具](tools.md#浏览器)）。

## 工具类（2）

| 技能 | 用途 | Key | 运行依赖 |
|---|---|---|---|
| `extract-archive` | 解压 zip / tar 全家桶与单文件 .gz / .bz2 / .xz，防 zip-slip、修 Windows 中文乱码 | — | Python 标准库 |
| `cron-task-patterns` | 定时任务的书写模式与范例 | — | 无 |

## 不入库的运行期技能

`.gitignore` 排除了 agent 在部署侧自建 / 修改的技能（如 `image-to-md-transcription`、`report-audit-preflight`、`finance-data-tooling-pitfalls`）与本机凭据目录——它们属于运行时数据，家在数据目录 `agent/skills/`，不在仓库 `skills/`。
