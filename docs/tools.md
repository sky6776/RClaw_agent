# 内置工具

[← 返回 README](../README.md)

agent 每轮可用的工具面由三部分组成：本文的内置工具、技能激活的动态工具（见 [技能系统](skills.md)）、MCP 工具（见 [MCP 扩展](mcp.md)）。多智能体与工作流工具见 [多智能体协作](multi-agent.md)。实现位于 `crates/rclaw-tools`。

会话内 `/tools` 列出当前工具面；图形界面「工具」页可逐项启停（禁用后立即禁止执行，并从下一 turn 的工具描述中移除）。

## 工具清单

| 分类 | 工具 |
|---|---|
| 文件 | `read` `write` `edit` `delete` `list` `grep` `rewind_files` |
| 执行 | `shell` `shell_poll` |
| 记忆与检索 | `memory` `session_search` `session_handoff` |
| 联网 | `web_search`（及各来源独立工具 `doubao_custom_search` / `tavily_search` / `zhipu_search` / `doubao_global_search`）`tavily_extract` |
| 浏览器 | `browser_navigate` `browser_snapshot` `browser_extract` `browser_click` `browser_type` `browser_scroll` `browser_screenshot` `browser_media` `browser_close` |
| 媒体 | `download_media`（安卓另有 `open_media`） |
| 技能管理 | `skills_list` `skill_manage` |
| 定时任务 | `cron_add` `cron_list` `cron_remove` |
| 多智能体 | `Agent` `Task` `AgentStatus` `AgentWait` `AgentCancel` `AgentResume` `TeamCreate` `TeamDelete` `TaskCreate` `TaskUpdate` `TaskList` `TaskGet` `SendMessage` `PlanRun` `escalate` `ResolveWorkflowQuestion` `message_agent` 及工作流工具族 |
| 知识库（仅安卓 APK） | 进程内 `gbrain__<name>` 工具族（18 个），见 [gbrain 知识库](gbrain.md) |

标记为 `parallel_safe` 的工具（`read` / `list` / `grep`、`web_search` 族、`session_search` 等只读工具）可在同一批次内并发；`shell`、`write` / `edit` / `delete`、`memory`、`browser_*` 等副作用或有状态工具串行。

## 文件工具

默认沙箱根为数据目录（`agent/`）。路径规则与越界处理见下文 [沙箱规则](#沙箱规则)。

| 工具 | 参数 | 说明 |
|---|---|---|
| `read` | `path` 或 `url`, `offset=0`, `limit=2000`, `include_metadata` | 分页读取；每次正文最多 256 KiB，扫描上限 64 MiB。文本型 PDF 本地提取并分页；扫描 / 图像 / 混合 PDF 或提取失败时作为附件交给多模态模型；PDF / 本地媒体上限 20 MiB。远程 URL 只按扩展名附加，不下载 |
| `write` | `path`, `content`, `expected_sha256` | 原子覆盖并自动建父目录；`content` 上限 128 MiB；`expected_sha256 = "missing"` 表示仅新建 |
| `edit` | `path`, `old_string`, `new_string`, `expected_sha256` | 精确替换首处；`old_string` 不得为空；文件上限 128 MiB |
| `delete` | `path` | 真实目录递归删除、符号链接只删链接；禁止删除工作区根、其祖先、文件系统根 / 挂载点 |
| `list` | `path?`, `prefix?` | 默认工作区根；递归深度 32、最多 4096 项、不跟随链接 |
| `grep` | `pattern`, `path`, `max_results` | Rust regex；默认 / 硬上限 1000 命中、输出 256 KiB、单行 2048 字节、单文件 64 MiB、深度 32；跳过二进制、链接与运行时数据库 |
| `rewind_files` | `rewind_id` | 回滚该回滚点及之后的文件 checkpoint，hash 校验失败整组拒绝。见 [长程会话](context-management.md#文件回滚rewind) |

`write` / `edit` / `delete` 对 ≤ 2 MiB 的 UTF-8 文本文件的成功写入会记录 checkpoint 供 `/rewind`；非文本 / 超限 / 符号链接 / 目录，以及沙箱外首次获批放行的那次写入或删除不记录。

## Shell

| 工具 | 参数 | 说明 |
|---|---|---|
| `shell` | `command`, `cwd`, `timeout_ms`, `background` | `cwd` 默认工作区根；命令最长 8192 字节；前台超时默认 30000 ms、最大 1500000 ms；stdout / stderr 各保留 64 KiB 头尾。Windows 用 `cmd /D /S /C`（强制 Python UTF-8，超时 `taskkill /T`）；Unix 用 `sh -lc` 与进程组终止；APK 用 `/system/bin/sh -c` |
| `shell_poll` | `job_id`, `max_bytes` | 轮询 / 收割后台任务；每侧默认 16384、范围 256–65536 字节；终态轮询后从进程表收割，日志仍可查询 |

后台任务（`background = true`）忽略超时，stdout / stderr 合并写入 `.shell-jobs/`；进程表上限 64（满时只驱逐已结束任务）；单日志 64 MiB，磁盘最多保留 128 个日志。注意 `Sandboxed` 权限只限制 `cwd` 必须在工作区内，**命令正文可以访问任意路径**，见 [安全设计](security.md#文件沙箱)。技能 Manifest 工具激活后，在 shell 里直接运行该技能的脚本会被拒绝并回 `SKILL_ENTRYPOINT_REQUIRED`，必须走 `skill__<skill>__<entrypoint>`。

## 记忆与检索

| 工具 | 参数 | 说明 |
|---|---|---|
| `memory` | `target = memory \| user`, `action = list \| add \| replace \| remove`, 或 `operations[]` 原子批处理 | 操作 `MEMORY.md` / `USER.md`；默认预算 2200 / 1375 字符，写入脱敏与注入扫描。见 [记忆与自进化](self-evolution.md#记忆) |
| `session_search` | `query`, `limit=3` | 跨会话（含归档）关键词检索，AND 语义 + 词频计分 |
| `session_handoff` | `chat_id`, `query`, `channel?`, `max_chars=24000` | 把另一段会话的脉络带进当前会话，4K–48K 字符预算 |

后两者的细节见 [长程会话](context-management.md#跨会话检索与交接)。

## 联网搜索

`[web_search]` 启用后注册 `web_search` 及各已配置来源的独立工具；配置 Tavily 时另有 `tavily_extract(urls)`（最多 5 个 URL 的正文抽取）。

```toml
[web_search]
enabled = true
order = ["doubao-custom", "tavily", "zhipu", "doubao-global"]
fallback = true
read_top = 3                  # 示例值；缺省 1，范围 0–5：最多补读前 N 条正文
max_results = 10
total_timeout_secs = 25       # 1–120
circuit_failure_threshold = 3 # 1–20
circuit_cooldown_secs = 60    # 1–3600

[web_search.tavily]           # api_keys（多 Key 轮换）、timeout_secs 20、max_results 5、search_depth basic、include_answer
[web_search.zhipu]            # api_key、startup_timeout_secs 10、call_timeout_secs 10、search_url、reader_url
[web_search.doubao_custom]    # api_key、timeout_secs 8、max_results 50
[web_search.doubao_global]    # api_key、timeout_secs 8、max_results 20、max_snippet_tokens 1000
```

- `web_search(search_query | url, read_top, max_results)`：URL 优先（直接读正文）；查询最长 1024 字节；`read_top` 默认 1、范围 0–5；`max_results` 默认 10、范围 1–50；总预算 25 秒；只有 Tavily 与智谱补读正文。输出总限 48000 字符，单摘要 / 片段 / 正文分别 8000 / 1500 / 6000。
- **路由与降级**：按 `order` 顺序尝试（过滤未配置项），`fallback = true` 时失败自动切换下一来源。瞬态错误路由层快速重试 1 次（间隔 150 ms）；连续 3 次瞬态 / 限流失败后冷却 60 秒。鉴权 / 额度错误永久阻塞该来源直到 `/search en <来源>`；豆包 Custom 限流或额度错误优先切到 Global。Tavily 在鉴权 / 额度 / 限流时轮换全部 Key，成功后固定健康 Key；智谱后端内部最多再重试 1 次（共 2 次尝试）、退避 200 ms、请求间隔至少 250 ms。
- 命令：`/search` 查看状态；`/search <来源>` 持久调整首选；`/search dis|en <来源>` 持久禁用 / 启用（启用会清除阻塞）；`/searchfallback [on|off]` 持久切换降级（无参查看状态）。

## 浏览器

拟人化 CDP 浏览器，**仅桌面 GUI 与安卓 APK 注册**（CLI / Termux 不注册）。桌面查找 `RCLAW_BROWSER_EXE`，否则按常规安装路径寻找 Chrome / Chromium / Edge；APK 使用长驻调试 WebView。

| 工具 | 说明 |
|---|---|
| `browser_navigate(url, referrer?)` | 打开 URL 返回正文（约 6000 字符）；Google / Bing / Baidu 结果页返回解析后的结果列表（最多 10 条，过滤广告） |
| `browser_snapshot` | 可交互元素快照（最多 250 项），返回 ref 供点击 / 输入 |
| `browser_extract(selector, attributes?, skip?, limit?)` | 按 CSS 选择器抽取；默认 100、上限 200、属性最多 8、输出约 40000 字节（序列化 JSON） |
| `browser_click(ref)` / `browser_type(ref, text)` | 点击 / 输入 |
| `browser_scroll(direction, amount)` | 默认向下 5 格，范围 1–30 |
| `browser_screenshot` | 截图落 `browser/screenshot_<毫秒>.png` |
| `browser_media` | 列出页面媒体（最多 40 条） |
| `browser_close` | 关闭 |

拟人化策略：清洗 Headless / WebView UA 与 Client Hints、焦点、`webdriver`、`window.chrome`、PDF plugins / mimeTypes、移除 `X-Requested-With`，鼠标轨迹 / 按键 / 滚动随机节奏；Google 验证码每 1.5 秒检测、最多等待 180 秒（可在应用内浏览器面板人工过码）。

## 媒体下载

`download_media(url, filename?)` 固定写入 `media/download/`：仅公网 HTTP(S)，DNS 全解析校验并钉住地址、拒绝环回 / 私网 / 保留段，忽略环境代理；单文件 200 MiB、总时限 180 秒、连接 15 秒、同主机重定向最多 5 跳；文件名最长 80 字符，同名序号尝试至 64。出站媒体须落在媒体沙箱内，见 [外部通道](channels.md#媒体与语音)。

## 通用截断

工具结果按字节预算做头尾截断：各保留约一半、UTF-8 边界安全、中段标注省略字节数与原始字节数。超过 `tool_result_artifact_bytes`（默认 32768）的结果整体落盘为 artifact，见 [长程会话](context-management.md#工具结果落盘)。技能脚本 stdout / stderr 各默认 64 KiB 截断（`[skills] max_output_bytes` 可调）并携带「输出截断」标志。

## 沙箱规则

- 所有文件工具拒绝盘符相对路径，拒绝工作区根下的 `state.sqlite` 及 `-wal` / `-shm` / `-journal` 伴生文件；Windows 还拒绝 DOS 设备名。路径解析到已存在符号链接的物理落点后再判定。
- `Sandboxed`（默认）：文件工具越界访问时，桌面 / 安卓宿主弹确认按「操作 + 物理落点」授权（任何通道的请求都弹给本机用户）；CLI 宿主无确认钩子，越界、拒绝或超时均拒绝。shell 只限制 `cwd`。
- `Unrestricted`：可越出工作区（NapCat `max_from` 命中的私聊、桌面「配置 → 工具权限模式 → 完全访问」），但数据库与危险删除保护仍然有效。
- 权限随消息携带且不可由正文伪造；子代理与工作流继承发起会话的权限快照，cron 与缺失上下文的路径回退 `Sandboxed`。详见 [安全设计](security.md)。
