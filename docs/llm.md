# 模型接入与配置

[← 返回 README](../README.md)

RClaw 通过 OpenAI 兼容协议接入 LLM。本文说明 Provider 抽象、`[llm]` 配置字段、流式与工具调用的解析规则、重试策略、`/llmodel` 热切换，以及 bot / 子代理 / 后台评审各自使用不同 provider 的方式。实现位于 `crates/rclaw-llm`，配置解析位于 `crates/rclaw-cli/src/config.rs`。

## Provider 两层抽象

- **协议层**（`ChatProtocol` / `StreamParser`）：负责请求构造、完整响应与 SSE 解析。当前唯一实现是 `OpenAiCompat`（`/chat/completions`）。Anthropic Messages、智谱原生 JWT 等协议未实现。
- **方言层**（`ProviderConfig` + `Dialect`）：描述某个服务的身份与差异——鉴权方式、推理字段名、`max_tokens` 键名、是否支持 `tool_choice` 等。

Provider 名不设白名单：任何 OpenAI 兼容端点（DeepSeek、Kimi、OpenRouter、Groq……）都能以一个 `[[llm.providers]]` 条目接入。示例配置默认 `zhipu`；配置缺省时内置默认 `deepseek`；桌面 / 安卓首运行向导提供 `zhipu`、`deepseek`、`custom` 三个预设。

## `[llm]` 字段

| 键 | 默认 | 说明 |
|---|---|---|
| `provider` | `deepseek`（示例为 `zhipu`） | 选中的 provider 条目名 |
| `model` | 无 | 全局模型覆盖：对**所有**按名字构建的 provider 都优先于条目的 `model`（含 `review_provider`、bot 固定 provider、子代理 `model` 参数解析出的 provider），多 provider 场景慎用 |
| `stream` | `true` | 启用 SSE 流式协议 |
| `max_tokens` | `4096`（示例 `128000`） | 输出上限，必须 > 0 |
| `request_timeout_secs` | `300` | 单次请求总超时，必须 > 0；深度投研长上下文建议不少于 300 |
| `max_tool_calls` | `256` | 模型单次响应允许的工具调用上限，必须 > 0 |
| `review_provider` | 无 | 后台评审（记忆 / 技能 review）使用的辅助 provider 名；空或与主 provider 同名时复用主 provider |

## `[[llm.providers]]` 字段

| 键 | 默认 | 说明 |
|---|---|---|
| `name` | 必填 | 条目名，供 `provider`、bot、子代理引用 |
| `base_url` | 必填 | OpenAI 兼容端点根（不含 `/chat/completions`） |
| `model` | 无 | 默认模型（与 `[llm].model` 同时缺省时回退硬编码的 `deepseek-v4-pro`） |
| `api_key` | 无 | 解析期可缺，但实际构建 Provider 时必须非空 |
| `auth` | `bearer` | `bearer` → `Authorization: Bearer …`；`apikey:<Header-Name>` → 自定义头（仍走 OpenAI 协议） |
| `reasoning_field` | `reasoning_content` | 思考内容字段：`reasoning_content` / `reasoning` / `none`；同一字段用于响应解析、SSE 增量与历史回传，`none` 完全不读写推理内容 |
| `max_tokens_key` | `max_tokens` | 或 `max_completion_tokens` |
| `supports_tool_choice` | `true` | 是否发送 `tool_choice` |
| `context_length` | `0` | 上下文窗口；`0` 按 128000 推导压缩预算（见 [长程会话](context-management.md)） |

未被主 provider、`review_provider`、bot 或子代理引用的条目不会在启动期全部预检。`[llm]` 与 `[[llm.providers]]` 都拒绝未知字段，键名拼错直接启动失败；`base_url` 必须是绝对 http(s) URL。

主配置没有通用的 `temperature` / `top_p` / thinking 开关；仅对智谱 `glm-5.3-flash` 硬编码注入 `temperature=1`、`top_p=0.95`、`reasoning_effort=max`、`thinking={enabled, clear_thinking=false}`。

**多模态白名单**：图片等媒体块只在模型为 `glm-5.3-flash` 或 `glm-5v-turbo` 时随请求发送，其他模型会静默剥离媒体 URL（只剩文字描述）。流式请求固定带 `stream_options.include_usage`，智谱 + 工具调用时附 `tool_stream=true`；返回的真实 usage 写入 `usage_anchor` 表，用于校准上下文压缩的估算。

## 流式解析与工具调用校验

- SSE 按原始字节跨 chunk 拼行，只处理 `data:` 行；支持 `[DONE]`、usage chunk、文本 / 推理增量、乱序的 tool-call index。
- 一轮响应必须给出明确的 `finish_reason`（`stop` 或 `tool_calls`）。工具调用槽缺失、id / name 重复或中途变化、参数不是 JSON 对象、结束原因不一致、工具调用数超过 `max_tool_calls`，都会拒绝整轮响应——这类协议错误**不重试**，本轮以错误结束。空参数归一为 `{}`。
- 大小限制：非流式响应 64 MiB、SSE 单行 16 MiB、错误体读取 1 MiB（展示截至 500 字节）。

## 重试与超时

- 请求总超时 `request_timeout_secs`（默认 300 秒）。
- 可重试：连接失败、超时、流异常、HTTP 408 / 425 / 429 / 5xx。不重试：协议错误、鉴权 / 配置错误。
- 运行时默认最多重试 10 次（共 11 次），退避 1、2、4、8、16、30… 秒，单次封顶 30 秒；每次迭代的重试总预算为 `max(600 秒, 2 × request_timeout)`。取消可中断请求与退避。
- 上下文超限（非可重试错误）另有一次 reactive 压缩后原地重试，不消耗普通重试次数，见 [长程会话](context-management.md)。

## `/llmodel` 热切换

- `/llmodel` 不带参数显示当前模型；`/llmodel <模型名>` 切换。**只接受内置清单**中的名字：`glm-5.3`、`glm-5.3-flash`（旧名 `glm-5.2`、`glm-5v-turbo` 分别映射到二者），其他模型名一律回「未知模型」拒绝；切换后 `context_length` 写为 1,000,000，`base_url` 固定改为智谱 coding 端点。非智谱模型请直接改 `rclaw.toml` 重启。
- 只切换当前选中 provider 的 `model` / `base_url`，保留 Key、鉴权与方言；构建成功后原子替换，进行中的调用继续用旧 Provider，下一次调用生效。
- 同步把 `model` / `base_url` / `context_length` 写回 `rclaw.toml` 对应条目；写盘失败时运行时已切换、重启会回退。压缩预算不会热重算，需重启；若 `[llm].model` 存在，重启后仍以它为准。

## 多 provider 场景

| 场景 | 配置方式 | 说明 |
|---|---|---|
| 主对话 | `[llm].provider` | 全局默认 |
| bot（持久身份智能体） | `agent/bots/<name>.toml` 的 `provider = "<条目名>"` | 缺省跟随主 provider；名字无效时告警并回退；该 provider 也用于 bot 的历史压缩。见 [多智能体协作](multi-agent.md#bot-与群聊) |
| 子代理 | `Agent` 工具的 `model` 参数填 `[[llm.providers]].name` | 未知名在入参校验期拒绝；指定后该子代理全部推理走独立、缓存的 Provider；子代理固定非流式调用 |
| 后台评审 | `[llm].review_provider` | 可选廉价模型；带工具的 review 路径启用后旧历史每条压至 300 字符、保留最近 24 条原文，降低异构模型全量重放成本（legacy JSON 路径仍用主 provider 与全量历史）。见 [记忆与自进化](self-evolution.md) |
