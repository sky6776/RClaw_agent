# 定时任务

[← 返回 README](../README.md)

定时任务让 agent 在指定时刻自动执行一段任务提示词，并把结果投递到指定通道。实现位于 `crates/rclaw-runtime/src/cron.rs`、`crates/rclaw-tools/src/{cron_tools,cron_controller}.rs`、`crates/rclaw-store/src/cron_store.rs`。书写范例见技能 `skills/cron-task-patterns/`。

## 调度类型

不使用传统 5 / 6 段 cron 表达式，而是结构化的 `schedule_type`：

| `schedule_type` | 含义 | 限制 |
|---|---|---|
| `after` | 相对延时（秒）后执行一次 | 最长 365 天 |
| `at` | 在指定 Unix 秒时间戳执行一次 | — |
| `clock` | 在最近的下一个本地钟点（`at_clock`，如 `08:30`）或指定的完整本地日期时间（`2026-06-28 22:00`，须在未来）执行一次 | — |
| `lunar_clock` | 在指定农历日期（`year` + `month` + `day`，可 `leap_month`）+ `at_clock` 执行一次 | — |
| `every` | 从创建时刻起按固定秒间隔重复（`interval_s`） | 最长 365 天 |
| `daily` | 每天在本地时区指定钟点重复（`at_clock`） | — |
| `weekly` / `monthly` / `yearly` | 每周指定星期（`weekday` 1–7）/ 每月指定日（`day_of_month`）/ 每年指定阳历月日（`month` + `day`），在 `at_clock` 重复 | — |
| `lunar_monthly` / `lunar_yearly` | 每个农历月指定农历日（`day_of_month`）/ 每年指定农历月日（`month` + `day`，可 `leap_month`），在 `at_clock` 重复 | — |
| `yearly_range` / `lunar_yearly_range` | 每年阳历 / 农历日期区间内每天在 `at_clock` 重复：起点 `month` + `day`，终点 `duration_days` 或 `end_month` + `end_day`（农历可 `end_leap_month`） | `duration_days` ≤ 400 |

- 一次性任务（`after` / `at` / `clock` / `lunar_clock`）默认执行后删除；显式保留则执行后禁用。重复任务按固定秒间隔或日历规则推进。
- 所有钟点按设备本地时区解析；`every` 不能用 `at_clock` 对齐到每天固定钟点——那是 `daily` 的事，工具会给出纠正提示。农历类型只在用户明确说「农历」时使用。
- 任务 ID 为 8 位十六进制，每个任务有简短 `name`。

## 工具

| 工具 | 说明 |
|---|---|
| `cron_add` | 新建任务：必填 `name`、`schedule_type`、`message`（任务提示词）；按类型附 `delay_s` / `at_epoch` / `at_clock` / `interval_s` / `weekday` / `day_of_month` / `year` / `month` / `day` / `leap_month` / `duration_days` / `end_month` / `end_day` / `end_leap_month`；`channel` / `chat_id` 指定结果投递目标；`delete_after_run` 控制一次性任务执行后是否保留。落盘后回显 next_run 的本地时间与距今时长 |
| `cron_list` | 列出全部任务 |
| `cron_remove` | 按 8 位任务 ID（`job_id`）删除 |

- `message` 必须**自包含**：到期的 cron turn 不读取创建它时的对话历史。
- `channel` / `chat_id` **务必传入发起会话的通道与会话 ID**（系统提示已要求模型这样做）：缺省会回落到 `system:cron`，而该目的地没有任何可见输出——CLI 只打一条日志，桌面端直接丢弃，cron turn 又不持久化历史，结果等于丢失。

## 执行语义

- 到期消息进入正常的 ReAct 主循环：可调用完整工具面并生成最终报告，但**不加载、也不持久化会话历史**；权限固定为 `Sandboxed`。正文格式为 `[CRON_TASK]\nJob name…\nJob message…`，并附带「Cron Task Handling」提示（不再调用 cron 工具、不反问是否创建提醒）。
- 结果投递到 `channel` / `chat_id` 指定的会话（如 QQ 私聊、桌面某会话）。
- 调度器默认每 60 秒扫描（`[cron] check_interval_sec`）。到期后先成功入队再推进状态；队列满时等待而不是丢弃；tick 延迟超过 5 秒或单步处理超过 2 秒会打 warn 日志。
- 错过多个周期（进程停机）只补执行一次：`every` 从实际执行时刻重新计时，日历任务跳到下一合法时刻，不逐次追赶。
- 不同任务可并行（按 `job_id` 隔离），同一任务不并发——上一轮仍在跑时再次到期，新触发排队，等当前 turn 结束后重路由；受全局 `max_concurrent_turns`（默认 8）约束。没有任务总数上限。

## 持久化

任务表保存在 `agent/cron.json`（启动即创建），原子整表写入；写盘失败登记 dirty 并逐 tick 重试；文件损坏时隔离为 `.corrupt-*` 后重建。加载期自修复：语义非法的任务就地禁用并回写，缺失的 `next_run` 自动补齐，重复 / 缺失的 id 重新生成。

## 图形界面

「定时任务」页可查看、刷新、删除任务；新增通过对话让 agent 调用 `cron_add`。没有「立即运行」或启停按钮。

## 心跳

`[heartbeat] interval_sec`（默认 1800，实际最少 60）周期检查 `agent/HEARTBEAT.md`：当文件存在非空、非标题、未勾选完成的行时，注入一条固定的自检提示触发一轮 turn。适合放「每半小时检查一下 X」之类的常驻待办；勾选完成（`- [x]`）的行不再触发。心跳 turn 走 `system:heartbeat` 会话：与 cron 不同，它正常加载历史、不注入 cron 提示；但同样没有可见的输出通道——需要通知用户时，应在待办中写明让 agent 用工具把结果送到具体会话。
