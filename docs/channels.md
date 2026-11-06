# 外部通道

[← 返回 README](../README.md)

通道把外部消息系统接到统一的消息总线：入站消息成为一个 `(channel, chat_id)` 会话的 turn，出站回复按通道能力交付。实现位于 `crates/rclaw-channels`（console / napcat / weixin / wxbot）、`crates/rclaw-weixin-login`（微信扫码）与 `crates/rclaw-desktop`（桌面 / 安卓虚拟通道）。

## 通道一览

| 通道 | 配置段 | 形态 | 会话 ID |
|---|---|---|---|
| 控制台 `cli` | 无 | Rustyline 按行输入 | 固定 `cli:console` |
| 桌面 / 安卓 `desktop` | 无 | 图形界面虚拟通道（安卓同名，无独立路由） | 前端提供的 chat_id |
| QQ `napcat` | `[napcat]` | NapCat OneBot 11 over WebSocket 客户端 | 私聊 `user:<QQ>`，群聊 `group:<群号>` |
| 微信 iLink Bot `weixin` | `[weixin]` | iLink Bot HTTP 长轮询 | `from_user_id`（仅单聊，无群聊处理逻辑） |
| 个人微信 `wxbot` | `[wxbot]` | VA + WCX 容器内微信，Webhook 主路 + 游标长轮询兜底 | 私聊 `user:<账号wxid>:<对方wxid>`，群 `group:<账号wxid>:<群id@chatroom>`（账号未知时无账号段） |

## 能力矩阵

每个通道声明四项能力，运行时据此决定行为（见 [ReAct 内核](react-core.md#流式输出与工作状态)）：

| 通道 | 收发媒体 | 工作状态提示 | 中间输出 | 逐片思考流 |
|---|---|---|---|---|
| cli | 否 | 否 | 否 | 否 |
| desktop（含安卓） | 是 | 是 | 是 | 是 |
| napcat | 是 | 是 | 是（整轮中间快照） | 否 |
| weixin | 是 | 否 | 否 | 否 |
| wxbot | 是 | 否 | 否 | 否 |

## 控制台

- 支持方向键、行内编辑、上下历史；空行忽略；Ctrl-C、Ctrl-D / EOF 结束输入（`--daemon` 模式忽略 EOF）。
- `[llm].stream = true` 只启用模型侧流式协议；控制台把通过完成闸门的文本整段输出，不逐 token 显示，也不展示思考与中间输出。

## QQ（NapCat）

```toml
[napcat]
enabled = true
ws_url = "ws://127.0.0.1:3001"
access_token = "..."              # 与 NapCat 一致；留空 = 不鉴权
# allow_from = ""                 # 允许用户 QQ，逗号分隔；空 = 全部，支持 *
# allow_groups = ""               # 允许群号；空 = 全部群，支持 *
# block_from = ""                 # 拉黑用户，优先级高于 allow_from，支持 *
max_from = "123456"               # 私聊可获得全盘文件权限的 QQ；空 = 无人提权
group_response_mode = "at_only"   # none / at_only / all（默认 all）
```

| 键 | 默认 | 说明 |
|---|---|---|
| `heartbeat_interval_sec` | 30 | 心跳探活间隔 |
| `reconnect_delay_sec` | 5 | 重连基础退避，指数翻倍上限 60 秒 |
| `send_chunk_bytes` | 4500 | 长文本分段阈值 |

- `access_token` 同时放入 URL 查询参数与 Bearer 头。
- 权限：`block_from` 优先；`allow_from` 空 = 允许全部；`max_from` 空 = 无人提权、`*` = 所有人；**只有命中 `max_from` 的私聊**被提升为 `Unrestricted`（可操作数据目录外路径），群聊始终 `Sandboxed`。
- 群消息响应：`at_only` 接受 @机器人或 @全体；`none` 不响应；`all` 全部响应。注意：`at_only` / `none` 下**未投递给模型的群消息仍会静默归档**到 `agent/media/napcat/group_<群>_user_<QQ>_*.txt`，并维护群文本 / 媒体引用索引（各 10000 条）供引用回填——这是隐私边界，请知悉。
- 长文本超过阈值时优先以合并转发卡片发送。
- 可靠性：按 `message_id` 进程内跨重连去重（上限 10000 条）；同一聊天出站串行、不同聊天并行；断线时文本最多重投 1 次，附件不自动重发。
- 媒体：收到的图片 / 语音 / 视频 / 文件下载到 `agent/media/napcat/`；语音只下载转换、不做 ASR。向用户发文件：在工作区生成文件后由模型调用 `glm-master-skill` 的 Manifest 工具 `skill__glm_master_skill__publish_media`，它把文件暂存到 `agent/media/out/<sha256>/<name>` 并在 stdout 输出 `MEDIA_MARKER=[:napcat-media type=file path=... name=...]` 回执；运行时校验（通道匹配、沙箱内真实文件）后自动附到回复。模型**手写**的 `[:xxx-media ...]` 行会被整行剥离，剥离后为空还会被完成闸门拒绝。

## 微信（iLink Bot）

```toml
[weixin]
enabled = true
api_base = "https://ilinkai.weixin.qq.com"
token = "your-ilink-bot-token"
```

| 键 | 默认 |
|---|---|
| `allow_from` | `""`（全部） |
| `cdn_base` | `https://novac2c.cdn.weixin.qq.com/c2c` |
| `chunk_bytes` | 2000 |
| `poll_timeout_sec` / `retry_sec` / `backoff_sec` / `max_failures` | 35 / 2 / 30 / 3 |

- 获取 token：运行 `rclaw-weixin-login`，终端渲染二维码、每秒轮询扫码状态，每个二维码默认等待 300 秒（`--timeout`），过期后最多再刷新 2 次（共 3 个二维码）；确认后输出可直接粘贴的 `[weixin]` 片段。参数：`--api-base`、`--timeout`、`--toml-file`（直接写入）、`--no-qr`、`-h/--help`。
- 消息类型：收发文本、图片、文件、视频；入站语音采用服务端转写文本；出站语音发送尚未稳定覆盖。文本按 UTF-8 分段。
- 可靠性：靠服务端游标，整批成功入队后才提交；没有独立的 message-id 去重。
- 媒体落盘 `agent/media/weixin/`。该通道的消息恒为 `Sandboxed`，没有提权机制。

## 个人微信（wxbot）

通过手机上 VA 内运行的微信 + WCX 模块（开启「API + MCP 服务器」与「Webhook 消息推送」）接入个人微信。仅支持同机 loopback：`api_base` 与 `webhook_listen` 非回环地址时拒绝启动。

```toml
[wxbot]
enabled = true
api_base = "http://127.0.0.1:3001"
api_token = "..."                 # 留空则通道不启动
webhook_listen = "127.0.0.1:38081"
webhook_path = "/wxbot/webhook"
webhook_secret = "..."            # 留空则通道不启动（Webhook 以 Bearer secret 校验）
# allow_from = ""                 # 允许私聊 wxid；空 = 不响应所有私聊（fail-closed）
# allow_groups = ""               # 允许回复的群；空 = 所有群不回复（即使被 @）
group_reply_policy = "mention"    # mention / all / none
```

| 键 | 默认 | 说明 |
|---|---|---|
| `poll_fallback_enabled` | true | Webhook 之外的兜底长轮询 |
| `poll_timeout_sec` / `retry_sec` / `backoff_sec` / `max_failures` | 60 / 2 / 30 / 3 | 轮询参数 |
| `chunk_bytes` | 2000 | 长文本分段 |
| `send_min_gap_sec` / `send_jitter_sec` | 2 / 3 | 相邻发送间隔 = min + rand(0..=jitter) |
| `send_window_max` | 10 | 60 秒滑动窗口内最大发送条数，0 = 关闭 |
| `send_failure_fuse` | 5 | 连续发送失败熔断阈值，0 = 关闭 |

- 群消息**记录**不受白名单约束：所有群的入站消息尽力落库到 `agent/media/wxbot/message_index.db`，图片 / 语音 / 表情 / 文件同链路下载（视频仅标注不下载）；只有白名单群按 `group_reply_policy` 回复。私聊白名单外的消息直接丢弃、不落库。
- 以 `dedup_key` 合并 Webhook 与轮询双路，SQLite 唯一键保证幂等。微信风控系统通知（`weixin110.qq.com`）只归档不投递，防自激循环。换微信账号或触发发送熔断后通道**锁停**（收发全部拒绝），需重启 RClaw 重新绑定（防跨账号误发）。
- 该通道消息恒为 `Sandboxed`。loopback 绑定不等于鉴权：同机其他进程仍可向 Webhook 伪造事件冒充白名单身份，请确保本机环境可信。
- 媒体落盘 `agent/media/wxbot/`；发文件用法与 NapCat 相同（经 `publish_media` 工具产出 `[:wxbot-media ...]` 回执）。

## 桌面 / 安卓虚拟通道

图形界面通过 `desktop` 通道接入，具备全部四项能力；上传 / 拍摄的文件落 `agent/uploads/`。详见 [图形界面](gui.md)。

## 媒体与语音

- 入站媒体按通道落到 `agent/media/<channel>/`，单文件上限 256 MB。
- 出站媒体标记只接受规范化后仍位于 `agent/media/` 内的真实路径，拒绝 `..`、目录前缀伪造与符号链接逃逸；未经发布工具回执验证的标记会被完成闸门拒绝。
- 本地 whisper.cpp 语音识别仅在安卓 APK 中可用（录音 / 上传音频）；模型文件为应用数据目录下 `models/ggml-base-q5_1.bin`；JNI 胶水在 `third_party/whisper-jni`，上游 `third_party/whisper.cpp` 由构建脚本自动拉取、不入库。

## 会话隔离与并发

- 运行键为 `(channel, chat_id, cron_job_id)`：同一普通会话同时只有一个活动 turn，期间到达的消息进入容量 32 的插话队列（见 [ReAct 内核](react-core.md#插话队列与取消)）；不同会话并行；全局默认最多 8 个 turn（`[agent] max_concurrent_turns`）。
- 权限随消息携带：通道按真实身份设置 `tool_access`，正文无法伪造；只有 NapCat（`max_from` 私聊）与桌面（权限模式）会提权，微信类通道恒为 `Sandboxed`。见 [安全设计](security.md#权限单向链)。
