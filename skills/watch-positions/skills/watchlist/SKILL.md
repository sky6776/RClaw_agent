---
name: watchlist
description: Create, update, inspect, and validate persistent watchlists stored as JSON files. Triggers on "自选股", "watchlist", "监控清单", "add to my list", "把XX加入监控", "建一个组合清单", "加入自选".
---

# Watchlist Management

自选清单持久化为 JSON 文件,存于 agent 工作目录下 `watchlists/<清单名>.json`,一单一文件。
包根规范(已自动附上)定义了数据源双轨;本技能只在写清单时需要一次标的解析。

## Workflow

### Step 1: 确定操作

创建 / 加标的 / 移除 / 查看 / 重命名。用户提到的清单不存在且操作是「加」,就创建并说明。

### Step 2: 把每个名称解析为证券代码

- 接受带后缀代码——沪深 `.SH`/`.SZ`、港股 `.HK`、美股 `.O`/`.N`——或公司简称/别名。
- 有 MCP 时:用 `hexin-stock__search_stocks`(query = 简称)解析。
- 无 MCP 时:用兜底脚本 `python "<skills_dir>/watch-positions/scripts/ak_fallback.py" quotes --codes <猜测代码>`,
  从返回的 `名称` 列确认简称与代码对得上;查不到再换拼音/别名重试一次。
- 名称歧义(多处上市、A/H 双重上市)时,先问用户要哪只再写入。
- **绝不凭记忆猜代码**——始终用一次实时查询确认代码与简称,并把解析结果回读给用户。

### Step 3: 写文件

Schema:

```json
{
  "name": "<清单名>",
  "updated": "<YYYY-MM-DD, 今天>",
  "positions": [
    {"windcode": "[证券代码]", "label": "[证券简称]", "weight": null, "cost": null, "notes": ""}
  ]
}
```

- 没动到的标的,原样保留其 `weight` / `cost` / `notes`。
- `weight` 是小数(0.05 = 5%);`cost` 是上市币种每股成本。仅在用户提供时写入,永不发明。
- 写完必须回读校验 JSON 合法。

### Step 4: 确认回执

把结果清单以短表格展示(代码、简称、weight 若有),并**在正文中写明 JSON 文件完整路径**
(这是本技能的交付物——文件本身,不是关于文件的描述)。清单超过约 30 只时提醒:复盘与
事件扫描会变慢,建议拆分。写完后回读文件、展示解析后的内容,让用户确认的代码就是磁盘上
的代码,而不是你以为写了的代码。
