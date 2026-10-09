# watch-positions — 持仓跟踪(RClaw 技能包)

面向 A/H/美股的自选与持仓跟踪:持久化自选清单、带异动归因的盘后复盘、持仓事件提醒、
盘中实时视图,以及 PDF/DOCX/XLSX 产出管线。

自 ZCode 官方插件 `watch-positions@zcode-plugins-official`(上游 v0.6.1,published 0.1.1)
适配移植到 RClaw 技能体系:SKILL.md 为实质性改写(双数据源、RClaw 工具命名、RClaw 调度);
`report-render/scripts`、`xlsx-author/scripts/recalc.py` 自上游 vendored。

## 包结构

| 路径 | 职责 |
|---|---|
| `watch-positions.md` | 包根共享规范:数据源双轨优先级、溯源标签、覆盖度区块、清单存储约定。读取包内任一技能时由运行时自动注入 |
| `skills/watchlist` | 自选清单管理(JSON,`agent` 工作目录 `watchlists/` 下) |
| `skills/intraday-watch` | 盘中快照:实时报价、相对基准、常态波动判定;盘中不归因 |
| `skills/close-recap` | 盘后复盘:涨跌、大盘/板块背景、异动归因、决策先行摘要 |
| `skills/event-monitor` | 事件扫描:公告、业绩预告、质押、解禁,按严重度分级 |
| `skills/report-render` | 分页 PDF / 可编辑 DOCX 交付物,CJK 渲染 + `[n]` 链接引注 + 强制渲染自检 |
| `skills/xlsx-author` | 专业 .xlsx:公式/配色规范、Class A/B 溯源、交付前重算检查 |
| `scripts/ak_fallback.py` | akshare(东财/巨潮)兜底数据脚本:quotes/hist/index/events/announcements/news |

## 数据源双轨

| | 轨道一:ZCode 金融 MCP | 轨道二:akshare 兜底 |
|---|---|---|
| 启用条件 | `rclaw.toml` 配置了 `[mcp.servers.*]` 且启动成功 | 默认可用(需 `pip install akshare`) |
| 行情 | 同花顺 iFinD(实时/日频) | 东方财富公开接口(延迟快照/日频) |
| 公告/新闻 | Wind 文档检索 + 财经垂搜 | 巨潮公告列表 + 东财个股新闻 |
| 质押/解禁 | iFinD/Wind 结构化数据 | 东财聚合口径 |
| 板块聚合/风险指标 | 有 | 无(技能内标注 `源不可用`) |

技能文本按「工具清单里有没有 `hexin-stock__*` 等 MCP 工具」自动选择轨道,无需手工切换。

## MCP 配置(轨道一)

在 `rclaw.toml` 加入(示例见 `rclaw.toml.example` `[mcp]` 段):

```toml
[mcp.servers.hexin-stock]
url = "https://zcode.z.ai/api/v1/mcp/server/finance_hexin_stock"

[mcp.servers.hexin-stock.headers]
Authorization = "Bearer <ZCode JWT>"
X-Bigmodel-Authorization = "Bearer <bigmodel OAuth access token>"
```

五个服务器:`finance_hexin_stock` / `finance_hexin_index` / `finance_search` /
`finance_wind_stock` / `finance_wind_docs`(url 仅末段不同)。配置后工具以
`hexin-stock__get_stock_performance` 这样的 `<server>__<tool>` 名称出现。

**鉴权说明**:网关校验两个头。`Authorization` 用 ZCode 客户端的 JWT,
`X-Bigmodel-Authorization` 用绑定 coding plan 的 bigmodel OAuth 令牌——
普通 bigmodel API key 在网关上会得到 `{"code":1006,"message":"no permission"}`,
只有 JWT 则得到 `{"code":3101,"message":"coding plan is required"}`。
两个令牌均来自 ZCode 客户端本机凭据(`~/.zcode/v2/credentials.json`,AES-GCM 加密),
**会随登录态轮换,过期后需重新提取**;且数据为有许可的商业数据,需要对应付费套餐
(上游标记 `requiresPaidPlan`)。

**令牌轮换**:两份令牌均无 exp 过期声明,有效期由服务器端会话管理——客户端重新
登录会刷新凭据文件。运行 `python scripts/zcode_tokens.py --write <rclaw.toml路径> --probe`
可解密出当前值、就地更新两组 headers(注释保真)并实测网关;更新后重启 rclaw 生效。

## 平台支持

| | Windows 桌面版 | Termux(rclaw-cli) |
|---|---|---|
| 监控 4 技能 + akshare 兜底 | ✅ | ✅(`pkg install python numpy pandas` 后 `pip install akshare`) |
| xlsx-author | ✅(装 LibreOffice 可重算) | ⚠️ 可生成;无 LibreOffice → `recalc_unavailable`,按技能内替代检查执行 |
| report-render(PDF/DOCX) | ✅(`pip install -r skills/watch-positions/skills/report-render/scripts/requirements.txt`) | ❌ 平台门禁自动降级为 Markdown 交付 |

## 部署

把整个 `watch-positions/` 目录拷入运行时的技能目录即可:

- 桌面版:`%USERPROFILE%\.rclaw\agent\skills\`
- CLI/Termux:`<工作目录>/agent/skills/`(或 `[agent] base_dir` 指向的 `skills/`)

## License 现状

上游插件与本项目 vendored 的 `report-render/scripts`、`recalc.py` 均未附带 license
声明(上游 UPSTREAM.md 记录「授权未决」)。本包其余内容为面向 RClaw 的实质性改写。
仅限学习研究使用,不分发。
