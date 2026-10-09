# 跨包组合 — Prompt 模板

一条 prompt 串联多个技能包的功能点，用于联调与端到端验收。
三个分册见同目录：`watch-positions.md`、
`pick-funds.md`、`read-macro.md`。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号沿用全集 P1–P26，与各包分册不重号。

整理日期：2026-09-05

---

### P25 宏观 → 配置 → 落地选基
覆盖：macro-dashboard + asset-allocation + fund-screen 联动（read-macro × pick-funds）

```text
先告诉我现在的宏观状态和货币-信用象限，据此说说当前环境更适合配哪类方向的基金，
然后按这个方向帮我筛 <3-5> 只近三年业绩靠前、规模适中的基金给我备选。
```

### P26 周末全面体检
覆盖：watchlist + event-monitor + index-valuation + macro-dashboard + asset-allocation
+ xlsx-author 导出 + theses 存档回检（三个包全参与）

```text
做一次全面体检：
① 跑一遍宏观仪表盘和货币-信用象限；② 出一版大类资产观点（带证伪路标并存档）；
③ 看看 <沪深300> 和 <创业板指> 的估值分位；④ 对照"<核心池>"跑一次事件监控；
⑤ 需要留档的都导出 Excel，观点和本次结论存好，下次体检先回检上次的。
```

---

# 功能点覆盖对照表（跨包）

| 功能点 | 覆盖的 prompt |
|---|---|
| macro-dashboard 五板块/货币-信用象限 | P25、P26 |
| asset-allocation 观点/存档/回检 | P25、P26 |
| fund-screen 按宏观方向筛选 | P25 |
| watchlist + event-monitor + index-valuation | P26 |
| xlsx-author 导出 + theses 存档 | P26 |
