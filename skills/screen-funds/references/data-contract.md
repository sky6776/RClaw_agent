# 基金筛选数据契约

## 目录

1. Universe manifest
2. Universe CSV
3. Screening config
4. 输出状态

## 1. Universe manifest

全市场运行必须提供 UTF-8 JSON：

```json
{
  "schema_version": 1,
  "scope": "all-market",
  "universe_name": "中国境内公募基金存续及状态快照",
  "snapshot_date": "2026-07-21",
  "expected_share_count": 4,
  "expected_strategy_count": 3,
  "max_enumeration_age_days": 7,
  "sources": [
    {
      "id": "S1",
      "publisher": "发布主体",
      "locator": "可打开链接或原始快照文件定位",
      "snapshot_file": "raw/S1.json",
      "content_sha256": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
      "roles": ["universe-enumeration", "identity", "metrics"],
      "as_of": "2026-07-21",
      "acquired_at": "2026-07-21"
    }
  ]
}
```

规则：

- `scope` 只能是 `all-market` 或 `subset`。
- `all-market` 必须提供正整数预期份额数和策略数，且与标准化 CSV 完全一致。
- `all-market` 必须提供非负整数 `max_enumeration_age_days`；`0` 表示必须同日，最新枚举来源超过该期限时拒绝运行。
- `subset` 只有登记了 `universe-enumeration` 来源时才可设置 `max_enumeration_age_days`；否则该参数没有可验证对象，会被拒绝。
- `snapshot_date` 是宇宙口径日且不得在未来；来源 `as_of` 不得晚于该日期或获取日期。
- `locator` 记录官方 URL、数据库查询或原始定位；不得只写平台名称。
- `snapshot_file` 是相对 manifest 目录或绝对路径的本地原始快照；动态筛选工具必须能读取。
- `content_sha256` 是该快照文件的 SHA-256；实时 URL 也必须先保存内容再计算，不能对 URL 字符串本身哈希。动态筛选工具会实际复算并拒绝不匹配文件。
- 动态工具的 `output` 字段不得指向任何已登记的 `snapshot_file`，避免筛选结果覆盖原始证据。
- `roles` 可使用 `universe-enumeration`、`identity`、`metrics`、`validation`。全市场至少有一个枚举来源，且每行必须引用一个枚举来源。
- 合并多个来源时给每个来源唯一 ID；CSV 每行用分号连接支持该行的来源 ID。

## 2. Universe CSV

每行表示一个具体份额。必需列：

| 列 | 类型/示例 | 规则 |
|---|---|---|
| `snapshot_date` | `2026-07-21` | 必须等于 manifest |
| `fund_code` | `000001` | 六位字符串，唯一 |
| `fund_name` | 基金完整份额名 | 不得为空 |
| `share_class` | `A`/`C`/`ETF场内` | 不得为空 |
| `strategy_id` | 稳定的底层组合 ID | 同组合份额共用 |
| `ranking_share` | `true`/`false` | 每个 strategy 恰好一个 true |
| `cohort` | `主动权益|CNY|中证800类` | 可比组 |
| `category` | `主动股票` | 同 strategy 一致 |
| `currency` | `CNY` | 同 strategy 一致 |
| `status` | `active`/`terminated` 等 | 由 config 过滤 |
| `inception_date` | `2020-01-01` | 不得晚于快照日 |
| `metrics_as_of` | `2026-07-20` | 所有动态筛选字段中最早的截至日；无指标可留空 |
| `source_ids` | `S1;S2` | 必须存在于 manifest，且含身份和指标来源 |

脚本派生：

- `history_years`：快照日距成立日的年数。
- `metric_age_days`：指标截至距快照日的天数。
- `share_count_in_strategy`、`strategy_ranking_code`。

其它指标列由评分配置引用。所有比例使用小数，不能写 `%`；金额需在列名标明币种和单位，例如 `aum_cny_mn`。空值可用空字符串、`NA`、`N/A`、`null`、`none` 或 `--`。某行填写 `metrics_as_of` 时，引用的 `metrics` 来源截至日必须覆盖它；留空可保留只有身份记录的终止/合并产品，但排名份额会进入 `insufficient_data`。

## 3. Screening config

```json
{
  "schema_version": 1,
  "top_n_per_cohort": 5,
  "minimum_cohort_size": 10,
  "hard_filters": [
    {
      "id": "active",
      "field": "status",
      "op": "eq",
      "value": "active",
      "missing": "exclude"
    },
    {
      "id": "fresh_metrics",
      "field": "metric_age_days",
      "op": "lte",
      "value": 45,
      "missing": "review"
    }
  ],
  "cohort_settings": {
    "主动权益|CNY|中证800类": {"top_n": 5, "minimum_cohort_size": 20},
    "沪深300ETF|CNY|沪深300全收益": {"top_n": 3, "minimum_cohort_size": 5}
  },
  "scorecards": {
    "主动权益|CNY|中证800类": [
      {"field": "excess_return_3y", "direction": "higher", "weight": 0.25, "valid_min": -1.0},
      {"field": "sharpe_3y", "direction": "higher", "weight": 0.25},
      {"field": "max_drawdown_3y", "direction": "higher", "weight": 0.20, "valid_min": -1.0, "valid_max": 0.0},
      {"field": "manager_tenure_years", "direction": "higher", "weight": 0.15},
      {"field": "expense_rate", "direction": "lower", "weight": 0.15, "valid_min": 0.0}
    ],
    "沪深300ETF|CNY|沪深300全收益": [
      {"field": "tracking_difference_abs_3y", "direction": "lower", "weight": 0.35},
      {"field": "tracking_error_3y", "direction": "lower", "weight": 0.25},
      {"field": "expense_rate", "direction": "lower", "weight": 0.20},
      {"field": "aum_cny_mn", "direction": "higher", "weight": 0.10},
      {"field": "avg_turnover_cny_mn_20d", "direction": "higher", "weight": 0.10}
    ]
  }
}
```

支持的硬过滤操作：`eq`、`neq`、`in`、`not_in`、`gte`、`lte`、`between`、`truthy`、`falsy`。缺失策略：

- `exclude`：缺失即硬过滤失败；只用于确实不可缺的字段。
- `review`：进入 `insufficient_data`，待补资料。

`eq` / `neq` / `in` / `not_in` 以配置值类型决定比较方式：自定义指标的 JSON 数字与 CSV 中的有限数值文本按数值比较；JSON 字符串按字符串精确比较。必需字符串列只允许字符串相等/集合过滤，基金代码等带前导零标识必须写成字符串；日期范围请改用派生的天数/年数字段，不做字符串大小比较。

硬过滤默认应用于所有 cohort；可增加 `"cohorts": ["完整 cohort 名"]` 只应用于指定组。引用不存在的评分卡 cohort 会被拒绝，避免规则因拼写错误静默失效。

评分方向：`higher`、`lower`、`lower_abs`。权重只需为正，脚本会归一化。用可选 `valid_min` / `valid_max` 表示数据语义边界；缺失、非有限或越界值进入 `insufficient_data`，不作为产品淘汰。

全局 `minimum_cohort_size` 至少为 2；正式筛选通常应更高。`cohort_settings` 可用 `top_n` 和 `minimum_cohort_size` 覆盖某组默认值。数值过滤的阈值和 `between` 上下界会在读取配置时预校验。

## 4. 输出状态

| 状态 | 含义 |
|---|---|
| `ranked` | 在足够大的可比组内完成评分 |
| `hard_filter_fail` | 至少一项硬过滤失败 |
| `insufficient_data` | 缺失或错误字段需复核 |
| `non_ranking_share` | 同策略其它份额，不重复占榜 |
| `no_scorecard` | cohort 没有配置评分卡 |
| `small_cohort` | 可比样本不足，不强行排名 |

`audit_rows` 保留所有输入行及原因；不得只转存 `shortlist` 后删除完整结果。

`universe_validation` 分别输出份额数和策略数的 `*_check_performed` 与 `*_matches`。未提供预期计数时，`*_matches` 为 `null`，表示未检查而不是检查通过。`source_snapshot_hashes_verified=true` 只会由完成本地快照复算的动态筛选工具写入。
