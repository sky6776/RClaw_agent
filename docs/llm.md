# 模型接入与配置

[← 返回 README](../README.md)

RClaw 通过 OpenAI 兼容协议接入 LLM。本文说明 Provider 抽象、`[llm]` 配置字段、流式与工具调用的解析规则、重试策略、会话页的模型 / 思考强度选择器，以及 bot / 子代理 / 后台评审各自使用不同 provider 的方式。实现位于 `crates/rclaw-llm`，配置解析位于 `crates/rclaw-cli/src/config.rs`。

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
| `max_tokens` | 无 | per-provider 输出上限；缺省沿用 `[llm] max_tokens` |
| `thinking` | 无 | `enabled` / `disabled` / `auto`；缺省=服务端默认。glm-5.3-flash 禁止 `disabled` |
| `reasoning_effort` | 无 | DeepSeek：`none/low/high/max`；glm-5.3-flash / glm-5.4：`low/high/max` |
| `media_wire` | 自动 | 媒体 part 方言：`openai`（image/video/file URL）或 `deepseek`（image + file_data）；缺省按 base_url host 判定 |
| `flavor` | 自动 | 能力族：`zhipu` / `deepseek` / `generic`；缺省只按官方域名判定，代理端点必须显式写 |

未被主 provider、`review_provider`、bot 或子代理引用的条目不会在启动期全部预检。`[llm]` 与 `[[llm.providers]]` 都拒绝未知字段，键名拼错直接启动失败；`base_url` 必须是绝对 http(s) URL，且不允许携带用户名/密码/query/fragment。

显式 `thinking` / `reasoning_effort` 优先；未显式配置时，只有智谱 `glm-5.3-flash` 仍按官方推荐注入 `temperature=1`、`top_p=0.95`、`reasoning_effort=max`、`thinking={enabled, clear_thinking=false}`。`clear_thinking` 与 `tool_stream` 只发给智谱端点。

**多模态白名单**：图片等媒体块只在模型为 `glm-5.3-flash`、`glm-5.4`、`glm-5v-turbo`、`deepseek-flash`（含旧别名 `deepseek-v4-flash*`）时随请求发送；`deepseek-v4-pro` 是纯文本模型。DeepSeek 的 File 只支持 `file_data`（本地 data URI，≤20 MiB），远程文件 URL 与视频会降级成文本提示。流式请求固定带 `stream_options.include_usage`；返回的真实 usage 写入 `usage_anchor` 表，用于校准上下文压缩的估算。

## 流式解析与工具调用校验

- SSE 按原始字节跨 chunk 拼行，只处理 `data:` 行；支持 `[DONE]`、usage chunk、文本 / 推理增量、乱序的 tool-call index。
- 一轮响应必须给出明确的 `finish_reason`（`stop` 或 `tool_calls`）。工具调用槽缺失、id / name 重复或中途变化、参数不是 JSON 对象、结束原因不一致、工具调用数超过 `max_tool_calls`，都会拒绝整轮响应——这类协议错误**不重试**，本轮以错误结束。空参数归一为 `{}`。
- 大小限制：非流式响应 64 MiB、SSE 单行 16 MiB、错误体读取 1 MiB（展示截至 500 字节）。

## 重试与超时

- 请求总超时 `request_timeout_secs`（默认 300 秒）。
- 可重试：连接失败、超时、流异常、HTTP 408 / 425 / 429 / 5xx。不重试：协议错误、鉴权 / 配置错误。
- 运行时默认最多重试 10 次（共 11 次），退避 1、2、4、8、16、30… 秒，单次封顶 30 秒；每次迭代的重试总预算为 `max(600 秒, 2 × request_timeout)`。取消可中断请求与退避。
- 上下文超限（非可重试错误）另有一次 reactive 压缩后原地重试，不消耗普通重试次数，见 [长程会话](context-management.md)。

## 模型与思考强度（会话页选择器）

模型切换与思考强度都收在会话输入框底部、发送按钮左侧的「模型」控件里（它左边是环形上下文使用率按钮）。旧 `/llmodel` 命令已移除，只保留一条引导文案。桌面端按钮显示完整模型名与完整思考强度名；安卓窄屏显示缩写（`GLM-5.3F` / `GLM-5.4` / `dp-flash` + 单字档位），完整名称仍在菜单、悬停提示与无障碍标签里。菜单打开后连续选模型/档位都不会收起，点击会话页面其它位置（或按返回键）才关闭。

- **可选模型**（固定三项）：
  - `glm-5.3-flash`：智谱，1M 上下文 / 128K 输出，原生多模态，强制思考（低/高/最高）；
  - `glm-5.4`：智谱旗舰，1M / 128K，多模态，思考低/高/最高（仅 Coding Plan 端点）；
  - `deepseek-flash`：DeepSeek-V4.1-Flash-1017，1M / 384K，支持图像，思考可「关闭」或低/高/最高。
- **按会话独立**：每个 chat 各自保存选择（`state.sqlite` 的 `session-model/<channel>/<chat_id>`），互不影响；不修改 `rclaw.toml`，也不改变主 provider。普通会话、`bot-*`、`grp-*` 都可用这个选择器，各自独立。新建会话默认跟随主 provider / 该 bot 的固定 provider。
- **整组参数**：选模型不是只改 `model`，该会话的独立 provider 会按 profile 同步 `base_url` / `max_tokens` / `thinking` / `reasoning_effort` / `media_wire` / `reasoning_field` / `max_tokens_key`；覆盖以 profile 为准（包括条目里显式写的 `media_wire`），`flavor` 保持条目自身的显式值/域名判定。上下文压缩/用量预算仍按启动时的主配置派生；若启动阈值/尾窗超过目标模型的可用输入窗口（`context_length - max_tokens`），切换会被明确拒绝并提示调低 `compression_threshold_tokens` / `compression_tail_token_budget`（三个白名单模型都是约 1M 窗口）。
- **安全边界**：跨 provider 选择只在目标条目端点与官方端点同源时复用该条目的 key；代理端点不会被强行改写到官方端点（会明确报错）。`[llm].model` 全局覆盖不影响会话级选择。
- **缺失 provider 自动补条目**：选择的模型所属能力族在 `rclaw.toml` 里没有对应 `[[llm.providers]]` 条目时（例如从未配置 DeepSeek），选择器会按该模型 profile 自动追加一个条目（端点/模型/上下文窗口/输出上限/思考参数，`api_key` 留空），然后提示「已在 rclaw.toml 中添加 provider「X」，请在配置里填写它的 api_key 后重新选择该模型」。本次切换不生效，用户填好 key 后再选即可。后续新增模型 profile 复用同一路径，无需额外代码。
- **生效时机**：选择后只影响该会话的**下一轮**；进行中的 turn（含工具续轮与重试）继续用轮次开始时的 provider 快照。选择持久化，重启后仍生效。
- **思考强度**：同一控件里按该会话当前模型给出档位；DeepSeek 才有「关闭」。切换只重建该会话的独立 provider，不碰全局配置。
- **bot / 群聊**：bot 可在花名册里固定 provider；不同 bot 可指向不同 provider 条目（条目各自带 model），因而默认模型可以不同。bot/群聊的每个会话仍可再用选择器覆盖，二者互不影响。子代理 / 评审使用各自的 provider 或主 provider。

## 多 provider 场景

| 场景 | 配置方式 | 说明 |
|---|---|---|
| 主对话 | `[llm].provider` | 全局默认 |
| bot（持久身份智能体） | `agent/bots/<name>.toml` 的 `provider = "<条目名>"` | 缺省跟随主 provider；名字无效时告警并回退；该 provider 也用于 bot 的历史压缩。见 [多智能体协作](multi-agent.md#bot-与群聊) |
| 子代理 | `Agent` 工具的 `model` 参数填 `[[llm.providers]].name` | 未知名在入参校验期拒绝；指定后该子代理全部推理走独立、缓存的 Provider；子代理固定非流式调用 |
| 后台评审 | `[llm].review_provider` | 可选廉价模型；带工具的 review 路径启用后旧历史每条压至 300 字符、保留最近 24 条原文，降低异构模型全量重放成本（legacy JSON 路径仍用主 provider 与全量历史）。见 [记忆与自进化](self-evolution.md) |
