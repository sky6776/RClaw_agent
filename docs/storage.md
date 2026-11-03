# 数据目录与持久化

[← 返回 README](../README.md)

RClaw 的全部运行数据集中在 `[agent] base_dir`（默认 `agent/`，相对启动目录；桌面版为 `%USERPROFILE%\.rclaw\agent`，安卓为应用私有 `files/agent`）。改这个值等于切换一套独立的记忆、会话、技能与定时任务。实现位于 `crates/rclaw-store`。

## 目录布局

| 路径 | 内容 | 创建时机 |
|---|---|---|
| `config/SOUL.md` | 人格（系统提示组成部分） | 启动 |
| `config/USER.md` | 用户画像记忆 | 启动 |
| `memory/MEMORY.md` | 长期记忆 | 启动 |
| `memory/daily/` | 每日笔记 | 启动 |
| `memory/session_summaries/` | 会话压缩摘要副本 | 启动 |
| `skills/` | 技能目录（含 `.usage.json`、`.signals.jsonl`、`.ledger/`、`.archive/`、`.bundled_manifest.json`） | 启动 |
| `logs/` | 运行日志 `rclaw.log.YYYY-MM-DD` | 启动 |
| `state.sqlite` | 会话、回滚点、journal、工作流投影 | 启动 |
| `cron.json` | 定时任务表（启动即写入 `{"jobs":[]}`） | 启动 |
| `HEARTBEAT.md` | 心跳待办 | 启动 |
| `exports/` | `/export` 产出的 JSONL | 按需 |
| `media/<channel>/` | 各通道入站媒体；`media/out/` 出站；`media/download/` 下载 | 按需 |
| `uploads/` | 图形界面上传 / 拍摄的文件 | 按需 |
| `artifacts/<会话键>/` | 超长工具结果落盘 | 按需 |
| `bots/<name>.toml`、`bots/<name>/memory/`、`bots/groups/` | bot 定义、bot 独立记忆、群聊房间 | 按需 |
| `browser/` | 浏览器截图 | 按需 |
| `.shell-jobs/` | 后台 shell 任务日志（`<job_id>.log`，最多保留 128 个） | 按需 |
| `workflow-drafts/`、`workflow-artifacts/`、`workflows/`（`<name>.dwf.js` 存档）、`workflow.lock` | 动态工作流快照、产物与保存的工作流 | 按需 |

## SQLite（`state.sqlite`）

共 15 张表：

| 组 | 表 | 用途 |
|---|---|---|
| 会话 | `session` `message` `file_checkpoint` `usage_anchor` `tool_call_meta` | 会话与消息、文件回滚点、用量锚点、工具调用元数据 |
| 事件状态 | `journal_event` `kv` | 有序追加的事件流；键值（会话目标、分叉血缘、团队收件箱等） |
| 工作流投影 | `workflow_run` `workflow_script_version` `workflow_effect` `workflow_process` `workflow_hole` `workflow_question` `workflow_notification` `workflow_request` | 动态工作流 run 的状态投影 |

- **journal**：按 `kind + stream + seq` 有序追加，承载计划（`kind=plan`，`stream=<plan_id>`）、工作流（`workflow/<run_id>`）、团队状态（`team_state`）与子代理执行日志（`exec_log/<agent_id>`）；团队收件箱存于 `kv`。
- 会话以 `(channel, chat_id)` 隔离；active view 与完整 transcript 的关系见 [长程会话](context-management.md#会话存储模型)。
- 数据库启用 WAL、`synchronous=NORMAL`、外键、`secure_delete`、5 秒 busy timeout。schema 版本记在 `PRAGMA user_version`（当前 4）：库版本高于程序支持时拒绝启动；迁移前以 checkpoint + `VACUUM INTO` 备份为同目录的 `state.sqlite.bak-v<N>` 并校验。

## `/clear` 的语义

`/clear confirm`（或 `/clear yes`）删除：全部会话、消息、文件 checkpoint、用量锚点、工具元数据、会话目标 / 分叉血缘键、会话 artifact 目录，以及 `memory/` 下的全部内容（目录本身保留）。

保留：journal、非会话 kv、`cron.json`、`skills/`、`bots/`、日志、媒体、导出与配置。有 turn / review 在跑或其他维护操作（curator 归档、技能 pin / adopt 等）持有静默门时**直接拒绝**（提示 `/cancel` 后重试），不会排队等待。该命令仅对 CLI / 桌面会话或已提升为 `Unrestricted` 的会话开放，外部通道普通用户不可用。

## 备份与导出

- WAL 模式下**不要只复制 `state.sqlite` 主文件**：应在进程停止后连同 `-wal` / `-shm` 一起复制，或用 `sqlite3 state.sqlite "VACUUM INTO 'backup.sqlite'"` 产生一致快照。
- `/export [all]` 只导出会话记录为 JSONL，不等同于全量备份（不含记忆、技能、定时任务）。
- 整目录复制 `agent/` 是最简单的全量备份方式（注意其中的 `rclaw.toml` 不在此目录内——它在启动目录或数据根）。

## 文件工具对数据目录的保护

文件工具拒绝访问 `state.sqlite` 及其 `-wal` / `-shm` / `-journal` 伴生文件，拒绝删除工作区根与其祖先；即使在 `Unrestricted` 权限下这些保护仍然有效。见 [安全设计](security.md)。
