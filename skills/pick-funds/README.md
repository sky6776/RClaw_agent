# pick-funds — 基金研究(RClaw 技能包)

基金选品研究:多条件筛选、基金与经理画像、持仓风格与重合度分析、持有基金跟踪,
公募基金 / ETF / LOF。

自 ZCode 官方插件 `pick-funds@zcode-plugins-official`(上游 v0.6.1,published 0.1.1)
适配移植到 RClaw:SKILL.md 为实质性改写(双数据源、RClaw 工具命名、产出技能跨包复用)。

## 包结构

| 路径 | 职责 |
|---|---|
| `pick-funds.md` | 包根共享规范:双轨数据源、基金数据纪律(份额类/持仓滞后/年化口径/经理归属/费率三档)、溯源标签。读取包内任一技能时自动注入 |
| `skills/fund-screen` | 多条件筛选;附 `references/fund-return-traps.md`(「年化」口径陷阱与规模合并口径) |
| `skills/fund-profile` | 单基金深度画像 |
| `skills/manager-profile` | 基金经理跨产品画像(按任期拼合,增聘≠离任) |
| `skills/holdings-style` | 持仓法 + 净值回归(RBSA)双轨风格分析、跨基金重合度 |
| `skills/fund-watch` | 持有基金窗口监控(经理变更/规模异动/漂移/业绩偏离) |
| `scripts/fund_fallback.py` | akshare 兜底:rank(全市场筛选)/profile/nav/holdings |

产出类技能(report-render / xlsx-author)由 `watch-positions` 包统一承载,跨包复用。

## 数据源双轨

- **MCP 轨**:`wind-fund`(筛选引擎/费率栈/历任经理/区间收益)、`hexin-fund`(档案/净值/
  持仓/持有人结构)、`hexin-index`/`hexin-stock`(基准与重仓股)、`wind-docs`(经理变更公告)、
  `finance-search`(垂搜)。配置见 `rclaw.toml.example` `[mcp]` 段(双头鉴权)。;
  令牌随 ZCode 登录态轮换,`python scripts/zcode_tokens.py --write <rclaw.toml路径> --probe`
  一键解密提取并回写配置。
- **兜底轨**:未配置 MCP 时自动走 `scripts/fund_fallback.py`(天天基金/东财公开口径)。
  历任经理名册、变更公告、托管费/销售服务费、持有人结构在兜底轨 `源不可用`,各技能
  内置对应降级路径(如 manager-profile 降级为现任经理 + 网络检索佐证)。

## 平台支持

Windows 桌面版与 Termux(rclaw-cli)均可用;`pip install akshare` 即接通兜底轨。

## 部署

整个 `pick-funds/` 目录拷入运行时技能目录(桌面 `%USERPROFILE%\.rclaw\agent\skills\`;
CLI/Termux `<工作目录>/agent/skills/`)。

## License 现状

上游插件未声明 license;`references/fund-return-traps.md` 为上游产物。仅限学习研究使用,不分发。
