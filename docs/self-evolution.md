# 记忆与自进化

[← 返回 README](../README.md)

RClaw 的「自进化」分两层：**记忆**（跨会话的稳定事实与用户偏好）与**技能治理**（把工作方式沉淀为技能，并按可靠性维护其生命周期）。驱动两者的是 turn 结束后的后台 review。实现位于 `crates/rclaw-runtime/src/turn/review.rs`、`crates/rclaw-store/src/memory.rs`、`crates/rclaw-tools/src/{memory_tool,skill_usage,skill_signals,skill_ledger,skill_manage}.rs`。这套体系借鉴自 Hermes Agent。

## 记忆

### 文件

| 文件 | 用途 | 默认预算 |
|---|---|---|
| `agent/memory/MEMORY.md` | 长期记忆（稳定事实、可复用上下文） | 2200 字符（`[agent] memory_char_limit`） |
| `agent/config/USER.md` | 用户画像 | 1375 字符（`[agent] user_char_limit`） |
| `agent/memory/daily/YYYY-MM-DD.md` | 每日笔记 | 单日 12000 字符 |
| `agent/memory/session_summaries/` | 会话压缩摘要副本 | — |

单条记忆 ≤ 1200 字符；自定义预算最低按 256 处理。每次推理前重新读取 `MEMORY.md` / `USER.md` 与最近 3 天 daily notes，以一条末尾的非持久化运行时上下文消息（`<rclaw-runtime-context>`）注入——不进 system prompt、不写入会话历史；MEMORY 段带 `[memory usage: 已用/上限 chars]` 用量头，让模型自知预算。bot 会话读写的是 bot 自己的记忆目录（`bots/<name>/memory/`）。

### `memory` 工具

- `target = memory | user`（默认 memory），`action = list | add | replace | remove`；或 `operations: [{action, content?, old_text?}]` 对同一 target 原子批处理——任一操作未命中、多义或非法则全部不写，预算只按最终结果检查。
- 写入先做提示注入 / 隐形字符等威胁扫描，再把 `sk-*`、Bearer、PAT、AWS key、密码等替换为 `[REDACTED]`；条目换行折叠为单行。
- 同一 turn 内记忆整合失败（越限 / 未命中 / 多义 / 漂移）累计超过 3 次后，再失败即返回「停止重试」终局信号（`done=true, success=false`）；工具并未禁用，后续调用仍执行。

### 加载期复扫与漂移检测

- 手工编辑绕过写入门的内容在加载时再次扫描：危险 bullet 仅在注入视图中替换为 `[BLOCKED: threat scanner: <原因>]`，不改磁盘。
- **漂移**：首个 bullet 之后出现非空、非标题、非 bullet 的行。结构化写入检测到漂移时保存 `.<文件名>.drift.bak` 并拒绝覆盖，防止把手写内容冲掉。

## SOUL 人格

运行时只读取 `agent/config/SOUL.md` 作为启动期稳定的系统提示组成部分，改动后需重启生效。各人格只改变表达方式，不改变数据、评级、建议、授权与安全判断。

- **人格库**：四份内置人格（微信真人口吻 / 苏轼（重文言） / 东北老铁 / 李逵）编译在二进制内（源文件在 `crates/rclaw-store/assets/souls/`），启动时物化到 `agent/config/souls/`；已存在且被改过的文件不会被覆盖。
- **切换**：桌面 / 安卓「配置 → 人格（SOUL）」列出人格库，点「使用」把该文件复制为 `config/SOUL.md`；当前人格被编辑过时先弹确认，确认后自动备份为 `config/SOUL.md.bak.<YYYYMMDD-HHmmss>` 再覆盖。也可在「文件」面板直接编辑 `config/SOUL.md` 或人格库文件。
- **导入**：同一卡片支持从本机 / 手机存储一次选择多个 `.md`/`.markdown`/`.txt` 导入（单文件 ≤256 KiB、单次最多 20 个；逐项校验失败只跳过该项，重名自动 `-2`/`-3` 去重）。
- **停用**：`[agent] soul_enabled = false` 时不注入人格段（文件仍保留），需重启生效。
- 全新安装 / `SOUL.md` 为空或仍是 `# Personality` 占位时，启动会用默认人格（微信真人口吻）播种。

## 后台 review

- **触发**：仅成功的用户 / Agent turn 后异步触发，不阻塞当前回复，失败不影响对话。两个开关按分发**默认开启**：桌面 / 安卓首运行向导恒写 `auto_memory = true`，`auto_evolve` 为向导中默认勾选的选项（取消勾选时连同 `skill_curator` 一起关闭），`rclaw.toml.example` 也都为 `true`；只有手写 `[agent]` 段且省略这两个键时才按 `false` 处理。开启后记忆 review 每 `memory_review_nudge_interval`（默认 10）个成功 turn 触发，技能 review 每累计 `skill_review_nudge_interval`（默认 10）个 ReAct 迭代触发，计数按会话独立。前台本轮已成功写过 memory / `skill_manage` 时，对应计数归零且当轮不复盘。review 只占槽位，可与其他 turn 并行；仅当生命周期操作（curator 归档、`/clear`、技能 pin / unpin / adopt / restore）正持有静默门时跳过。技能进化只在主 persona 会话触发，bot 会话只做自己的记忆 review。
- **两种路径**：`auto_review_tooled`（缺省跟随 `auto_evolve`）为 true 时，review fork 真实使用 `memory` 或 `skills_list` / `read` / `skill_manage` 工具（按开关裁剪：`auto_memory` 关则不注册 `memory`，`auto_evolve` 关则不注册技能工具；受 read-before-write 与受保护技能约束）；为 false 时走 legacy JSON 路径，只提取 JSON 写 `MEMORY.md` / daily，并把经验以 `[自进化]` 前缀写入 `MEMORY.md`。
- **成本闸**：`auto_review_max_iters = 32`；`auto_review_max_input_tokens = 600000`（实际取其与上下文窗口 75% 的较小值，0 = 不限）；单次带工具调用超时 180 秒，legacy 120 秒。
- **辅助模型**：`[llm] review_provider` 可指定廉价 provider；带工具路径启用后旧历史每条压至 300 字符、保留最近 24 条原文（digest 重放），降低异构模型全量重放成本；legacy 路径始终用主 provider 与全量历史。
- **回执**：review 结束后把成功动作摘要以 `💾 [self-improvement review] …` 回发原会话（微信类通道除外）。

## 技能自进化

### 结果记账

turn / 子代理任务收尾时，对本轮读取过入口的技能按整轮成败记一次成功 / 失败计数（取消轮中性，不记账）。样本 ≥ 3 时索引行显示「近 N 次：X 成 Y 败」（底层为累计计数）；可靠率 = (成功 + 1) / (总数 + 2)。

### 进化信号

运行期过程事件的确定性记录，落在 `agent/skills/.signals.jsonl`：`circuit_breaker`（工具循环熔断）、`completion_rejected`（完成闸门拒绝）、`team_gate_rejected`（团队任务闸门拒绝）、`iter_limit`（迭代上限）、`user_correction`（用户纠正语气）、`failed`。详情 ≤ 400 字符并脱敏；最多 300 条，溢出裁至最新 150；review 只取最近 20 条。信号 ≠ 结论——review fork 改技能前仍需 `read` 核实。

### 热经验层 `EXPERIENCE.md`

顶层目录型技能（`skills/<name>/SKILL.md`）根下的 `EXPERIENCE.md` 在读取 `SKILL.md` 时自动注入（最多 4000 字符；包内 / 平铺技能不注入），不改技能正文即生效。新鲜经验先进热经验层，被证明稳定后才由 review fork 固化进 `SKILL.md` 并从热层移除。

### 策展器（curator）

- 只在无任何 turn / review 运行时执行，默认每 168 小时检查一次（`skill_curator_interval_hours`）；`/skills curate` 可手动触发。
- 生命周期 Active → Stale → Archived：闲置达 `skill_stale_after_days`（默认 14，示例配置 30）标 Stale；达 `skill_archive_after_days`（默认 30，示例 90）归档到 `.archive/`，可用 `/skills restore <name>` 恢复。
- **可靠性提前归档**：agent-owned 且结果样本 ≥ 4、平滑成功率 < 34% 的技能，闲置达到 Stale 阈值即可提前归档。
- 只处理 agent-owned、未 pinned、无 `.protected` 标记的技能；仓库技能与用户技能不碰。要让用户技能纳入自动维护，用 `/skills adopt <name>` 转为 agent-owned。

### 台账与回滚

每次技能内容变更前，`agent/skills/.ledger/ledger.jsonl` 记录 SHA-256 与 before blob（保留最近 50 次）。`skill_manage rollback` 校验 hash 后恢复 / 删除文件；回滚本身留痕且不可再套娃回滚。

### 写入防护

技能文本拒绝隐形 / 双向控制字符，密钥自动脱敏；不会因教学文本含「ignore instructions」之类字样而误杀。后台 review 的 `delete` 必须提供已存在的 `absorbed_into`（实际归档而非删除）；前台 `delete` 才永久删除。其余 `skill_manage` 约束见 [技能系统](skills.md#查看与管理)。

## 图形界面「自进化」面板

展示四个开关（自动记忆 / 自进化 / 策展器 / 会话压缩）状态、`MEMORY.md` / `USER.md` 正文（各最多 24000 字符）、技能库路径；洞察含近 14 天活跃、工具 / 技能 Top 8、会话 / 消息数、记忆占用、最近 8 次技能变更。面板唯一操作是「刷新」；开关在 `rclaw.toml` 的 `[agent]` 段修改。

## 相关配置速查（`[agent]`）

下面是 `rclaw.toml.example` 的示例值（与首运行向导写出的值一致）；注释中给出键缺失时的兜底值：

```toml
auto_memory = true                 # 向导恒写 true；键缺失按 false
auto_evolve = true                 # 向导默认勾选；键缺失按 false
# auto_review_tooled = true        # 缺省跟随 auto_evolve
# memory_review_nudge_interval = 10
# skill_review_nudge_interval = 10
# auto_review_max_iters = 32
# auto_review_max_input_tokens = 600000
skill_curator = true               # 默认 true
skill_curator_interval_hours = 168
skill_stale_after_days = 30        # 默认 14
skill_archive_after_days = 90      # 默认 30
# memory_char_limit = 2200
# user_char_limit = 1375
```
