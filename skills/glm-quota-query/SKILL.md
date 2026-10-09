---
name: glm-quota-query
description: 查询智谱 GLM Coding Plan 编码套餐的实时额度与用量：5 小时 Token 窗口与每周 Token 窗口的已用百分比、重置倒计时，MCP 工具月度配额（联网搜索/网页读取/开源仓库），套餐档位，以及可选的 24 小时模型 Token 用量。当用户说"查额度""GLM 剩余额度""5 小时额度还剩多少""编码套餐用量""coding plan 额度""额度什么时候刷新"等时触发。API Key 复用 skills/glmv-stock-analyst/config.toml，不打印不外传。
---

# GLM Coding Plan 额度查询

查询智谱（BigModel）GLM Coding Plan 编码套餐的剩余额度与用量统计。

## 何时使用

- 用户问"查额度 / 剩余额度 / 5小时额度 / 周额度 / MCP 次数 / 额度什么时候重置"
- 用户担心 Coding Plan 被限流，想看当前窗口还剩多少
- 用户想看 24 小时内各模型 Token 消耗（需带 `--model-usage`）

## 如何使用

先 read 本 SKILL.md 激活 Manifest，然后调用动态工具：

```
skill__glm_quota_query__quota            # 查额度（5h/周/MCP/档位）
skill__glm_quota_query__quota --model-usage   # 追加 24 小时模型用量
```

不要在 shell 里手写脚本路径或 curl 命令；Key 只由脚本内部读取。

## 数据来源与解读规则

- 端点：`GET https://open.bigmodel.cn/api/monitor/usage/quota/limit`（回退 `https://bigmodel.cn/...`），鉴权 `Authorization: Bearer <key>`
- `data.level`：套餐档位（lite / pro / max）
- `data.limits[]` 每条含义：
  - `type=TOKENS_LIMIT` + `unit=3,number=5` → **5 小时 Token 窗口**
  - `type=TOKENS_LIMIT` + `unit=6,number=1` → **每周 Token 窗口**
  - `type=TIME_LIMIT` + `unit=5,number=1` → **MCP 月度配额**（usage=已用次数，currentValue/remaining=剩余，usageDetails 按工具拆分：search-prime / web-reader / zread）
- `percentage` 一律是**已用百分比**（0-100），剩余 = 100 - 已用
- `nextResetTime` 为毫秒时间戳，脚本换算为本地时间并给出剩余时长
- MCP 的 `number` 是次数上限（Pro=1000，Lite=100，Max=4000）

## 注意事项

- monitor 端点属于平台内部接口，未写入官方公开 API 文档，结构可能调整；脚本对未知 `unit`/`type` 做了兜底（原样输出）
- 该接口反映的是 **GLM Coding Plan 编码套餐**额度；按量付费（paas/v4 资源包/余额）不在此口径内
- 汇报时**绝不**输出 API Key、完整 Authorization 头或 config.toml 原文
- `--model-usage` 端点偶尔回空体，属正常（暂无数据），脚本会如实标注"暂无数据"
