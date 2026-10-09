# read-macro — 宏观策略(RClaw 技能包)

自上而下的宏观与策略工作产物:宏观五板块仪表盘(增长/通胀/货币与流动性/信用/外部)、
指数估值分位与盈利 vs 估值归因、带证伪路标的大类资产配置观点、政策与产业规划跟踪。

自 ZCode 官方插件 `read-macro@zcode-plugins-official`(上游 v0.6.1,published 0.1.1)
适配移植到 RClaw:SKILL.md 为实质性改写(双数据源、RClaw 工具命名、产出技能跨包复用)。

## 包结构

| 路径 | 职责 |
|---|---|
| `read-macro.md` | 包根共享规范:双轨数据源、口径/陈旧度/分位窗口纪律、证伪路标、溯源标签。读取包内任一技能时自动注入 |
| `skills/macro-dashboard` | 宏观五板块仪表盘 + 货币-信用象限(可复现判据) |
| `skills/index-valuation` | 指数估值分位(带窗口)与盈利/估值拆分;附 `references/hexin-index-traps.md` 字段陷阱 |
| `skills/asset-allocation` | 跨资产观点:宏观主线 → 五条传导链 → 每资产观点+证伪路标;股债性价比(ERP) |
| `skills/policy-tracker` | 政策与产业规划:规划目标 [披露] / 口号 / 我们解读 [推断] 三分离;通稿措辞 diff |
| `scripts/macro_fallback.py` | akshare 兜底:macro(CPI/PPI/PMI/GDP/M2/社融/LPR/外储)、rate(中美国债)、index-value(指数 PE 分位自算)、index-hist |

产出类技能(report-render / xlsx-author)由 `watch-positions` 包统一承载,跨包复用。

## 数据源双轨

- **MCP 轨**:`wind-economic`(EDB 两步取数)、`wind-index`(估值分位/编制信息)、
  `hexin-index`(估值水平)、`hexin-stock`(行业合计)、`finance-search`(垂搜)、
  `wind-docs`。配置见 `rclaw.toml.example` `[mcp]` 段;鉴权为双头
  (`Authorization` ZCode JWT + `X-Bigmodel-Authorization` bigmodel OAuth 令牌)。;
  令牌随 ZCode 登录态轮换,`python scripts/zcode_tokens.py --write <rclaw.toml路径> --probe`
  一键解密提取并回写配置。
- **兜底轨**:未配置 MCP 时自动走 `scripts/macro_fallback.py`(公开接口口径,来源标注
  实际数据方)。DR007/信用利差/省级 EDB 等兜底轨没有的序列如实 `源不可用`。

## 平台支持

Windows 桌面版与 Termux(rclaw-cli)均可用;`pkg install python numpy pandas` 后
`pip install akshare` 即接通兜底轨。无重渲染依赖(长文档跨包走 report-render,
Termux 上按其平台门禁降级 Markdown)。

## 部署

整个 `read-macro/` 目录拷入运行时技能目录(桌面 `%USERPROFILE%\.rclaw\agent\skills\`;
CLI/Termux `<工作目录>/agent/skills/`)。

## License 现状

上游插件未声明 license;`references/hexin-index-traps.md` 为上游产物。仅限学习研究使用,不分发。
