# RClaw 工作流工具面对齐 ZCode：实施文档

> 参照系：`zai-org/ZCode` v3.16.2（main @ `3c5b227`，Apache-2.0），源码本地副本 `D:/sw/zcode-src`。
> 目标仓库：RClaw。代码路径相对仓库根；数据路径相对 `Layout.base`（§3.0）。
> 结论口径：只对齐"能力与契约"，不照抄实现；RClaw 已有的更强约束（确定性沙箱、QuickJS、type_check）保留。
>
> 修订记录：
> - v2（2026-10-22）：按 codex review 30 条修订。
> - v3（2026-10-22）：按第二轮 codex review 30 条修订——amend 排队/替换配额、终态 supersede、
>   SQLite 一致性备份、effect 身份与重放、缓存关闭时序、world 权限（ToolAccess）、hole 原子切换、
>   script/exec 双代次、问题/hole 持久化、outbox 确认点、幂等占位、单实例租约、legacy 可空、
>   投影折叠、回滚数据损失范围、开关契约、权限比较、技能门 hash、facade 附录、分阶段契约。
> - v7（2026-10-22）：结合 ZCode/RClaw 代码的贴合性修订——后台 token 与 `drop_guard` 解耦、
>   `world.run` 改为 ToolAccess（无逐命令审批）、`project_root = Layout.base`（无 per-session cwd）、
>   `/workflow` 的 display/model content 分离、P0 不暴露 retune、全局路径走 `workflow_home()`、
>   P1 表数降到 5、补 glob/git/Job Object/qid parser 任务；逐项对照见 §14。

---

## 0. 一句话结论

RClaw 已有 ZCode 动态工作流的**执行内核**（`crates/rclaw-runtime/src/agent_tools/workflow.rs` + `workflow_ast.rs`，共 5209 行），
但只有 1 个工作流执行工具 `WorkflowRun`（另有 `ResolveWorkflowQuestion` 提问应答）、且是**同步阻塞**调用；
ZCode 的 9 个工具中仍有 8 个未移植，
`/workflow` 入口、技能契约、后台运行与状态投影、存档复用、补洞能力**均未移植**。
本文按 **P0-P4 五个阶段**给出逐项差异与补齐方案。

---

## 1. 现状基线

### 1.1 RClaw 已有

| 事实 | 位置 |
|---|---|
| `WorkflowRun` 工具：`script` / `resume_run_id` / `amend_run_id` / `max_parallel_asks` / `max_run_secs` / `type_check` | `crates/rclaw-runtime/src/agent_tools/workflow.rs:1834-1862` |
| 确定性 QuickJS 沙箱：钉死 Date/Math/performance、禁 eval/Function/import、64MB 内存、120s 单片刻、默认 1h 墙钟 | `workflow.rs:25-45, 1088-1180` |
| facade：`agent(name).ask()` / `ask<T>()` / `askJson()` / `result()` | `workflow.rs:59-84, 1181` |
| journal 事件：`run_started` / `run_resumed` / `ask` / `ask_replayed` / `answer` / `run_completed` / `run_failed`，流 `workflow/<run_id>` | `workflow.rs:2006-2150` |
| resume：按 `(actor, inputHash, 第几次)` 零调用重放，script hash 校验 | `workflow.rs:1000-1070` |
| amend：按 actor + inputHash 导入前驱已完成 ask | `workflow.rs:1980-2030` |
| 工具注册（主代理专属，子代理屏蔽） | `crates/rclaw-cli/src/bootstrap.rs:343`；`crates/rclaw-runtime/src/turn/tool_execution.rs:813` |
| 斜杠命令表：`/cancel /context /skills /tools /clear /log /compact /rewind /fork /goal /export` | `crates/rclaw-runtime/src/turn/commands.rs:7-21` |
| 后台唤醒器：防抖合批 + Heartbeat 注入 | `crates/rclaw-runtime/src/background_wake.rs` |
| journal 存储：`state.sqlite.journal_event`，schema 以 `user_version` 版本化（当前 v2） | `crates/rclaw-store/src/db.rs:19,237` |
| 数据根 `Layout.base = cfg.agent.base_dir`（默认 `agent/`）；文件/命令工具以它为沙箱根 | `rclaw.toml.example:11`；`crates/rclaw-cli/src/bootstrap.rs:38` |
| 会话键只有 `channel/chat_id/cron_job_id`，无 per-session cwd | `crates/rclaw-runtime/src/turn.rs:141` |
| `CURRENT_CHAT` 作用域覆盖 turn 内的工具执行，可拿 session key | `crates/rclaw-runtime/src/turn.rs:2497` |
| `WorkflowRun` 的取消令牌来自 `turn_cancel()` 子令牌 + `drop_guard` | `crates/rclaw-runtime/src/agent_tools/workflow.rs:2049-2052` |
| `shell` 无逐命令审批；仅“工作区外路径”有确认钩子 | `crates/rclaw-tools/src/shell.rs:268-282`；`crates/rclaw-tools/src/access.rs:48` |
| 桌面 PlanRun 卡片：`plan_status` 轮询 + `📋 流程步骤` 可折叠卡 | `crates/rclaw-desktop/src/commands.rs:1143`；`ui/src/components/Chat.tsx:347`；`ui/src/lib/api.ts:418` |

### 1.2 RClaw 没有

- `/workflow` 斜杠入口与帮助条目；
- ZCode 9 个工具中的 8 个（`ResolveWorkflowQuestion` 已有最小实现）；
- `skills/dynamic-workflows/SKILL.md` 作者契约；
- 后台 run、状态机、跨会话 run 列表、运行卡片；
- 存档工作流与草稿/快照目录；
- `files` / `git` / `world.run` / `log` / `args` / `phase` / `artifact` / `hole` facade；
- 工具级"仅当用户明确要求"触发约束；
- 逐命令审批（只有 `shell` 的路径确认钩子，没有命令确认）。

---

## 2. 差异总表（逐项）

分级：**P0** 入口与语义、**P1** 后台与内省、**P2** 存档与参数、**P3** 片段与世界读、**P4** 留白与最小展示面。
"处理"列给出本文对应章节。

### 2.1 入口与触发

| # | 维度 | ZCode | RClaw 现状 | 差距 | 处理 |
|---|---|---|---|---|---|
| 1 | `/workflow` 命令 | 内置 prompt 命令，展开为"用技能设计并调用 CreateWorkflow" | 无；原文进 LLM | **高** | P0，§4.1 |
| 2 | 按需激活工具面 | 命中 `/workflow` 的那一轮首个请求即带工作流工具 | 所有工具常驻 | 低（可接受差异） | 不实现，§4.1 |
| 3 | 作者技能 | `dynamic-workflows` 技能，脚本提交前必须先加载 | 无 | **高** | P0-P4 分阶段扩写，§4.3 |
| 4 | 技能门 | 未加载技能时拒绝提交脚本 | 无技能工具；技能靠 `read SKILL.md` 发现 | **高** | P0，会话级 hash 标志，§4.3 |
| 5 | 触发约束 | 仅 `/workflow` 或用户明确点名时可调用 | 工具描述无约束，模型可自发长跑 | **高** | P0，§4.2 |
| 6 | 保留名 | `/workflow` 为保留字 | 未占位 | 中 | P0，§4.1 |
| 7 | 帮助与候选 | 共享 help 表 | `slash.rs` 帮助文本需补 | 低 | P0，§4.1 |

### 2.2 工具面

| # | 工具 | ZCode 语义 | RClaw 现状 | 处理 |
|---|---|---|---|---|
| 8 | `CreateWorkflow` | 校验脚本 → 确认 → 后台启动；三来源 | `WorkflowRun` 同步跑完才返回 | P0 改名，P1 后台化，P2 加 `saved` |
| 9 | `AmendWorkflow` | 修订任意 run，新 run 继承已完成工作，可只调设置 | `WorkflowRun.amend_run_id` | P0 拆工具，P1 后台化/排队 |
| 10 | `ResumeWorkflowRun` | 仅 `stopped`（非 superseded）可续 | `WorkflowRun.resume_run_id` | P0 拆工具，P1 后台化 |
| 11 | `ListWorkflowRuns` | 按项目枚举，含跨会话 | 无 | P1 |
| 12 | `GetWorkflowRun` | 单 run 快照：进度、log 尾、结果、失败、holes | 无 | P1（P4 补 phases/artifacts） |
| 13 | `FillWorkflowHole` | 给运行中 hole 补函数体 | 无 | P4 |
| 14 | `ResolveWorkflowQuestion` | 回答运行中 actor 的阻塞提问 | **已有** | P1 扩展后台 qid/持久化 |
| 15 | `SaveWorkflow` | 保存脚本+元数据，永不主动保存 | 无 | P2 |
| 16 | `EvalWorkflowSnippet` | 同步跑片段，无 agent/report | 无 | P3 |
| 17 | `ListSavedWorkflows` | 列出已保存工作流（ZCode 技能引用） | 无 | P2（辅助） |
| 18 | `CancelWorkflowRun` | TaskStop/UI 取消 | 无独立取消面 | P1（辅助） |
| 19 | `WorkflowRun` 旧名 | — | 已在用 | P0 deprecated 别名；P1 起由 `workflow_legacy_tool` 开关控制（默认 false） |

### 2.3 运行模型

| # | 维度 | ZCode | RClaw 现状 | 处理 |
|---|---|---|---|---|
| 20 | 执行形态 | 后台 run，立即返回 `backgrounded` | 同步阻塞，最长 1h 占住主 turn | P1 |
| 21 | 完成通知 | 通知管线回主 agent | 无 | P1，§3.6 |
| 22 | 状态机 | running/completed/errored/stopped(原因) | 只有 journal 事件 | P1，§3.3 |
| 23 | 中断恢复 | 进程退出后标 interrupted | 无 | P1 |
| 24 | 取消 | TaskStop / UI | 仅父 turn `/cancel` 联动 | P1，§5.11 |
| 25 | 归属 | 记录发起会话 | 无 | P1 |
| 26 | 项目作用域 | cwd 为 project key | 无列表 | P1，按 `Layout.base`（§3.0） |
| 27 | token 用量 | 每 run 汇总 | 无 | P1（有则填，无则缺省） |
| 28 | 确认窗 | 生产宿主确认后启动 | 无，靠工具权限 | P1 记录差异：沿用权限系统 |

### 2.4 脚本能力

| # | 维度 | ZCode | RClaw 现状 | 处理 |
|---|---|---|---|---|
| 29 | 语言 | TypeScript/Node | JS + `.ask<T>`，QuickJS | 保留（差异可接受） |
| 30 | 子代理 | `agent(name, persona?)`，可指定 model/system | `agent(name)`，无 persona/model | P2 加 model，P5 评估 persona |
| 31 | 结果 | `report(item, artifactId?)` + 顶层 return | `result(text)` | P4 加 report，result 兼容适配 |
| 32 | `log` | 有 | 无 | P3 |
| 33 | `args` | 有，保存时声明并校验 | 无 | P2 |
| 34 | `files` | glob/read/grep，可重放 | 无（工具层有 read/list/grep） | P3 |
| 35 | `git` | status/diff 只读 | 无 | P3 |
| 36 | `world.run` | 执行命令，需确认 | 无（有 `shell`） | P3，仅 `Unrestricted`（无逐命令审批） |
| 37 | `artifact.*` | 文件/markdown/图表/表格/看板 | 无 | P4 file/markdown；图表 P5 |
| 38 | `phase` | 运行图分组 | 无 | P4 |
| 39 | `hole` | 运行中留白 | 无 | P4 仅 tail hole |
| 40 | 类型检查 | 全量 tsc | 语法探针 + 禁用扫描 + 可选 tsc | **保留 RClaw 子集，接受差异**；`type_check` 只覆盖 facade.d.ts 可表达范围 |
| 41 | 确定性 | 运行时约束 | 更严格（冻结 Date/Math） | 保留 |
| 42 | 草稿 | `.zcode/workflow-drafts/` | 无 | P1 不可变快照 |
| 43 | 存档文件 | `.zcode/workflows/<name>.dwf.ts` + 元数据块 | 无 | P2 |
| 44 | amend 缓存边界 | 首个 live 写后停止导入 | 无条件导入 | P1，§5.2 |
| 45 | 脚本文件 | 结果回传 draft 路径 | 全部内联 | P1 快照，P2 复用 |

### 2.5 展示与运维

| # | 维度 | ZCode | RClaw 现状 | 处理 |
|---|---|---|---|---|
| 46 | 运行卡片 | run pane + 时间线 + 产物 | 无 | P1 最小卡，P4 产物区 |
| 47 | 列表入口 | workflows hub | 无 | P1 `workflow_status` |
| 48 | 跨会话可见 | 项目级 journal | journal 按 run 存 | P1 |
| 49 | 日志尾 | Get 返回 log 尾 | 无 `log` facade | P3 |
| 50 | 失败诊断 | failure + recoveryActions | 文本错误 | P1 结构化 error/recovery；多动作 P5 |

---

## 3. 目标架构

### 3.1 分层

```
用户 / 模型
   │
   ├─ /workflow（prompt 展开，不拦截）
   │
   ├─ 工具面（最终 11 个）
   │    CreateWorkflow / AmendWorkflow / ResumeWorkflowRun
   │    ListWorkflowRuns / GetWorkflowRun / CancelWorkflowRun
   │    SaveWorkflow / ListSavedWorkflows
   │    EvalWorkflowSnippet
   │    FillWorkflowHole
   │    ResolveWorkflowQuestion（已有）
   │
   ├─ WorkflowRunService（新）
   │    提交/修订/续跑/取消/列表/详情/状态迁移/配额/幂等/租约
   │
   ├─ WorkflowDriver（新）
   │    后台任务、取消令牌表、心跳、effect、outbox
   │
   └─ 引擎（已有）
        QuickJS 沙箱 / journal 重放 / amend 导入 / escalate
```

### 3.2 工具面与阶段累计

| 阶段 | 新增 | 累计 | 说明 |
|---|---|---|---|
| 现状 | Resolve + `WorkflowRun` | 2 | — |
| P0 | `CreateWorkflow`（改名）、`AmendWorkflow`、`ResumeWorkflowRun`（同步薄包装）；`WorkflowRun` 别名 | 5 | 含别名 |
| P1 | `ListWorkflowRuns`、`GetWorkflowRun`、`CancelWorkflowRun`；别名改为开关（默认关闭） | 7 | 7 个正式工具 |
| P2 | `SaveWorkflow`、`ListSavedWorkflows` | 9 | — |
| P3 | `EvalWorkflowSnippet` | 10 | — |
| P4 | `FillWorkflowHole` | 11 | — |

> 技能文件同步分阶段扩写，**不得提前宣告未落地工具**。

### 3.0 路径与项目模型（RClaw 特有，先定）

ZCode 的 `.zcode/` 项目根在 RClaw 没有对应物：RClaw 的 `Layout.base` = `cfg.agent.base_dir`
（默认 `agent/`，见 `rclaw.toml.example:11`、`bootstrap.rs:38`），`read/list/grep/shell`
都以它为沙箱根；`TurnKey` 只有 `channel/chat_id`（`turn.rs:141`），**没有 per-session cwd**。
因此本文统一采用下表，不再使用 `.rclaw/` 或 `%USERPROFILE%` 硬编码：

| 概念 | 定义 |
|---|---|
| `project_root` | `canonicalize(Layout.base)`；默认 `<进程工作目录>/agent` |
| 运行表 | `<project_root>/state.sqlite`（新增表，§3.4） |
| 草稿/快照 | `<project_root>/workflow-drafts/<run_id>/g<N>.dwf.js` |
| 项目存档 | `<project_root>/workflows/<name>.dwf.js` |
| 全局存档 | `workflow_home()/workflows/<name>.dwf.js`；`workflow_home()` 放在 `rclaw-store`，解析顺序与 `desktop/home.rs` 一致（`RCLAW_HOME` > `%USERPROFILE%\.rclaw` > XDG；Android 用 app-private），避免 runtime 依赖 desktop |
| `session_key` | `channel + "/" + chat_id`，只表示归属，不参与项目隔离 |
| `files`/`path` 根 | 与 `read` 相同：`Layout.base`；越界需 `Unrestricted` + 路径确认 |

同一进程内所有会话共享一个 `project_root`；`ListWorkflowRuns` 按它过滤，
`owned_by_this_session` 用 `session_key` 判定。若未来要 ZCode 式的“git 仓库工作区”，
需新增 `agent.project_dir` 配置并同时改文件/命令工具的沙箱根，不在本期。

### 3.3 状态机

```
                     submit
        (none) ──────────────▶ queued? ──no──▶ running
                                  │yes              │
                                  ▼                 │
                          (wait predecessor)        │
                                  │                 │
                                  └────▶ running ◀──┘
                                           │
              ┌────────────────────────────┼──────────────────────────┐
              │                            │                          │
           complete                      error                 cancel / amend
              │                            │                          │
              ▼                            ▼                          ▼
         completed                      errored                 stopping(reason)
                                                                     │
                                                       engine confirmed exit
                                                                     ▼
                                                               stopped(reason)
```

- `queued`：amend 后继已登记、等待前驱退出；**不执行任何脚本/副作用**。
- 终态：`completed` / `errored` / `stopped`；终态 run 不再进入 `stopping`。
- `stopping`：已发取消/被替换，引擎未确认退出；不可续跑、不可再取消、不可再 amend。
- 10s 宽限未确认 → 保持 `stopping`；进程重启扫描 → `stopped(interrupted)`（仍需副作用检查才可续）。
- 可续跑：`stopped` 且 **`superseded_by IS NULL`** 且 `stop_reason != superseded` 且无 `started/unknown` effect。
- 重启扫描把 `stopping` 转 `stopped(interrupted)` 时必须**保留原 `superseded_by` 与停止来源**；
  `superseded_by` 非空是 Resume 的无条件拒绝项，不能靠 `stop_reason` 判断。
- `errored` 不可续，用 Amend；`superseded` 不可续，指向后继。
- **迁移表**：

| 当前 | 事件 | 下一状态 | 备注 |
|---|---|---|---|
| — | create | running | 直接启动 |
| — | amend 后继 | queued → running | 等前驱退出 |
| queued | 前驱 stopped | running | driver 启动 |
| queued | 前驱 stop 超时 | queued | 保持等待，UI 提示 |
| queued | cancel | stopped(user) | 未执行，直接终结 |
| running | complete | completed | 幂等 |
| running | error | errored | 保留 journal |
| running | cancel | stopping(user/model) | 令牌触发 |
| running | amend | stopping(superseded) → 后继 queued | 见 §5.2 |
| running | provider abort | errored（kind=provider） | 恢复动作 = Amend |
| stopping | engine exit | stopped(reason) | reason 取来源；保留 superseded_by |
| stopping | cancel/amend/resume | `invalid_state` | — |
| completed/errored | amend | 原状态不变 + `superseded_by`；后继 queued→running | 不再 stopping |
| stopped | amend | 原状态不变 + `superseded_by`；后继 queued→running | — |
| stopped | resume | running（同 run，exec_generation+1） | 校验 hash/effect |
| stopped(superseded) | resume | `superseded` | 指向后继 |

### 3.4 存储模型

**新表（`state.sqlite`，schema v3）**：

```sql
CREATE TABLE IF NOT EXISTS workflow_run (
  run_id              TEXT PRIMARY KEY,
  cwd                 TEXT NOT NULL DEFAULT '',   -- project_root = canonicalize(Layout.base)；legacy 未知时 ''
  session_key         TEXT NOT NULL DEFAULT '',   -- channel + '/' + chat_id，只表归属
  label               TEXT NOT NULL,
  source_path         TEXT,
  snapshot_path       TEXT NOT NULL DEFAULT '',
  snapshot_hash       TEXT NOT NULL DEFAULT '',
  script_generation   INTEGER NOT NULL DEFAULT 1,
  exec_generation     INTEGER NOT NULL DEFAULT 0,
  legacy              INTEGER NOT NULL DEFAULT 0,
  resumable           INTEGER NOT NULL DEFAULT 1,
  status              TEXT NOT NULL,              -- queued/running/stopping/completed/errored/stopped
  queued_reason       TEXT,
  stop_reason         TEXT,
  stop_origin         TEXT,              -- user | model | provider | interrupted | superseded
  requires_fresh_auth INTEGER NOT NULL DEFAULT 0,  -- 用户取消后置 1，跨重启保留
  capacity_owner      TEXT,              -- 当前持有许可的 run_id
  capacity_claim_for  TEXT,              -- queued 后继的 claim（不占许可）
  resumed_from        TEXT,
  superseded_by       TEXT,
  started_by          TEXT NOT NULL DEFAULT 'main',
  execution_config    TEXT NOT NULL DEFAULT '{}',
  owner_instance      TEXT,
  lease_until         INTEGER,
  last_event_id       INTEGER NOT NULL DEFAULT 0,   -- 投影已消费的最大 journal id
  elapsed_runtime_ms  INTEGER NOT NULL DEFAULT 0,   -- 跨 exec_generation 累计执行时长（不含等待）
  cache_closed        INTEGER NOT NULL DEFAULT 0,
  created_at          INTEGER NOT NULL,
  updated_at          INTEGER NOT NULL,
  heartbeat_at        INTEGER NOT NULL DEFAULT 0,
  spent_tokens        INTEGER NOT NULL DEFAULT 0,
  result_json         TEXT,
  result_handle       TEXT,
  error               TEXT
);
CREATE INDEX IF NOT EXISTS workflow_run_cwd_updated ON workflow_run(cwd, updated_at DESC);

CREATE TABLE IF NOT EXISTS workflow_script_version (
  run_id        TEXT NOT NULL,
  script_generation INTEGER NOT NULL,
  path          TEXT NOT NULL,
  hash          TEXT NOT NULL,
  parent_hash   TEXT,
  reason        TEXT NOT NULL,   -- create | amend | hole_fill
  replay_prefix INTEGER NOT NULL DEFAULT 0,  -- 1=保留此前 effect 重放；amend 必须为 0
  created_at    INTEGER NOT NULL,
  PRIMARY KEY (run_id, script_generation)
);

CREATE TABLE IF NOT EXISTS workflow_effect (
  run_id          TEXT NOT NULL,
  effect_key      TEXT NOT NULL,   -- fnv1a64(call_index:op:args_hash)，不含代次
  first_script_generation INTEGER NOT NULL,
  last_exec_generation    INTEGER NOT NULL,
  call_index      INTEGER NOT NULL,
  op              TEXT NOT NULL,
  is_write        INTEGER NOT NULL DEFAULT 0,
  args_hash       TEXT NOT NULL,
  args_preview    TEXT,            -- 脱敏后的参数预览（≤2KB）
  status          TEXT NOT NULL,   -- started | completed | failed | unknown
  result          TEXT,
  decision        TEXT,            -- skip | rerun | NULL（UI 对 failed/unknown 的逐项裁决）
  decision_exec_generation INTEGER,
  substitute_result TEXT,          -- skip 时给脚本的替代返回值（JSON）
  decided_by      TEXT,
  created_at      INTEGER NOT NULL,
  updated_at      INTEGER NOT NULL,
  PRIMARY KEY (run_id, effect_key)
);

CREATE TABLE IF NOT EXISTS workflow_process (
  run_id          TEXT NOT NULL,
  exec_generation INTEGER NOT NULL,
  process_id      TEXT NOT NULL,   -- Windows Job Object id / Unix pgid
  root_pid        INTEGER NOT NULL,
  state           TEXT NOT NULL,   -- running | exited | unknown
  created_at      INTEGER NOT NULL,
  exited_at       INTEGER,
  PRIMARY KEY (run_id, exec_generation, process_id)
);

CREATE TABLE IF NOT EXISTS workflow_hole (
  run_id      TEXT NOT NULL,
  hole_id     TEXT NOT NULL,
  script_generation INTEGER NOT NULL,
  name        TEXT NOT NULL,
  prompt      TEXT,
  state       TEXT NOT NULL,     -- waiting | filling | filled | aborted
  last_rejected_hash TEXT,       -- 上次被拒正文 hash，用于 fill_unchanged
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (run_id, hole_id)
);

CREATE TABLE IF NOT EXISTS workflow_question (
  question_id TEXT PRIMARY KEY,
  run_id      TEXT NOT NULL,
  exec_generation INTEGER NOT NULL,
  actor       TEXT NOT NULL,
  question    TEXT NOT NULL,
  state       TEXT NOT NULL,     -- pending | answered | aborted
  answer      TEXT,
  created_at  INTEGER NOT NULL,
  answered_at INTEGER
);

CREATE TABLE IF NOT EXISTS workflow_notification (
  id          TEXT PRIMARY KEY,
  run_id      TEXT NOT NULL,
  session_key TEXT NOT NULL,     -- channel + '/' + chat_id
  channel     TEXT NOT NULL,
  chat_id     TEXT NOT NULL,
  payload     TEXT NOT NULL,
  state       TEXT NOT NULL,     -- pending | scheduled | consumed | dead
  attempts    INTEGER NOT NULL DEFAULT 0,
  last_error  TEXT,
  next_at     INTEGER NOT NULL,
  scheduled_at INTEGER,
  consumed_at INTEGER,
  created_at  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS workflow_request (
  scope       TEXT NOT NULL,     -- project_root + '/' + tool
  request_id  TEXT NOT NULL,
  digest      TEXT NOT NULL,
  state       TEXT NOT NULL,     -- in_progress | done | failed
  run_id      TEXT,              -- 关联 run；崩溃恢复用
  response    TEXT,
  created_at  INTEGER NOT NULL,
  updated_at  INTEGER NOT NULL,
  PRIMARY KEY (scope, request_id)
);
```

**表与阶段（总 8 张）**：P1 的 v3 迁移**一次建全 8 张**（后三张空置，避免多次
`user_version` 迁移）；P1 只实现/使用前五张；`workflow_process` 在 P3 启用，
`workflow_hole` / `workflow_question` 在 P4 启用。

- 迁移：`crates/rclaw-store/src/db.rs` 的 `migrate()` 追加 `if version < 3 { ... }`，`SCHEMA_VERSION = 3`。
- **一致性备份（可重入）**：迁移前停库（无其它连接），执行 `PRAGMA wal_checkpoint(TRUNCATE);`，
  再 `VACUUM INTO '<dir>/state.sqlite.bak-v2.<uuid>.tmp';`，以只读连接 `PRAGMA integrity_check;`
  并确认 `user_version=2`，最后原子 rename 为 `state.sqlite.bak-v2`。
  - 若 `state.sqlite.bak-v2` 已存在且验证通过，仅当备份元数据（源库 size + mtime + user_version）
    与当前库一致时复用；当前库在备份后已有新数据 → 重新生成新备份（新 UUID，原子替换），
    避免用旧备份回滚丢失更多数据。临时文件残留按 UUID 清理；任一步失败拒绝启动。
  - 不直接复制 `state.sqlite`（WAL 下可能漏数据）。
- **崩溃一致性**：终态事件、`workflow_run` 更新、`workflow_notification` 插入，以及
  effect/hole/question 的状态变更，必须在**同一事务**提交。
- **权威性**：`journal_event` 是唯一事实源；`workflow_run`/`workflow_effect`/`workflow_notification`
  等表是投影/索引。表与 journal 冲突时以 journal 为准并重建表，反向覆盖禁止。
- **投影重建**：`kv` 保存 `workflow-projection-cursor`（最后消费的 journal id）。
  启动时**严格按 journal id 升序**逐事件折叠：`INSERT ... ON CONFLICT(run_id) DO UPDATE`，
  每行 `last_event_id` 必须等于刚应用的事件 id；游标推进与折叠同一事务。
  不允许用"取最大事件 id 跳过"的方式处理乱序；**同一 stream 的 seq 缺口**视为损坏，停止重建并报
  `projection_corrupt`。journal 全局 id 只用于排序，不要求连续（其他流事件会穿插）。
  事件载荷见 §3.4.1。
- **旧数据回填**：启动后台任务按 `workflow-backfill-cursor` 增量扫描 stream，完成一个 stream 推进一次游标；
  缺 cwd/session/脚本 → `legacy=1, resumable=0, cwd='', snapshot_*=''`，List 默认只显示当前 `project_root` 的行，
  `include_legacy=true` 才显示全局 legacy；禁止伪造归属。
  **回填与实时投影共用 `last_event_id` 比较**：回填只补充 `last_event_id` 更小的行/字段，
  不得覆盖实时投影已更新的状态；回填与游标推进同一事务。重复扫描、并行回填均幂等。
- **execution_config**（JSON，恢复所需）：
  `args, max_parallel_asks, subagent_model, allowed_models, max_run_secs, type_check, facade_version,
  tool_access, source_kind, saved_name, saved_scope, permission_snapshot, background_mode`。
  `allowed_models` 初始 = 会话模型 + `subagent_model` + 脚本中 `model("...")` 字面量
  （提交时解析，上限 16）；retune 只增不减（在跑 ask 需要旧模型）。
  resume 继承；`max_run_secs` 是**跨代次总预算**：已用时长持久化在 `elapsed_runtime_ms`
  （只累计 driver 实际执行时间，不含 queued、hole/问题等待、停机），resume 的可用预算 =
  `max_run_secs - elapsed_runtime_ms`，多次续跑共享同一上限。执行区间记账：driver 启动/退出时结算；
  崩溃最多补计一个心跳周期（15s）；queued、hole/问题等待、停机不计入。
- **不可变快照**：所有来源启动前复制到
  `<project_root>/workflow-drafts/<run_id>/g<script_generation>.dwf.js`
  （`project_root = Layout.base`，§3.0）；`snapshot_path` 只指向它；`source_path` 仅展示。
  执行/续跑/补洞只用快照 + hash。没有“无工作区”分支：`Layout.base` 始终存在，
  Android 也由 `Layout` 派生。
- **文件与 DB 的提交顺序**（SQLite 事务不能原子提交文件）：
  1. 写临时文件 `<path>.tmp-<uuid>`，`fsync`；
  2. 原子 rename 到最终 `g<N>.dwf.js`，对目录 `fsync`（Windows 上 FlushFileBuffers）；
  3. 再提交 DB 事务，写入 `workflow_script_version` 与 `workflow_run.snapshot_*`。
  - 启动校验：每个 `running/queued` run 的 `snapshot_path` 必须存在且 hash 匹配；不匹配 → `errored` + `snapshot_missing`，禁止 resume/amend。
  - 孤儿清理：启动扫描删除无 DB 引用、且 mtime > 24h 的 `*.tmp-*` 与 `g*.dwf.js`。
  - 崩溃点语义：2 之后 3 之前崩溃 → 文件在、DB 未引用，启动清理；3 之后 → 两者一致。

#### 3.4.1 journal 事件载荷

| 事件 | 必填载荷 |
|---|---|
| `run_started` | run_id, script_generation, exec_generation, script_hash, snapshot_path, execution_config, label, cwd, session_key, started_by |
| `run_queued` | run_id, queued_reason, superseded_by, label, cwd, session_key, started_by, script_generation, script_hash, snapshot_path, execution_config |
| `run_stopping` | run_id, exec_generation, reason |
| `run_resumed` | run_id, script_generation, exec_generation, script_hash, snapshot_path |
| `run_retuned` | run_id, max_parallel_asks, subagent_model, execution_config |
| `run_cache_closed` | run_id, event_id, effect_key |
| `run_completed` | run_id, exec_generation, asks_total, asks_replayed, result_json |
| `run_failed` | run_id, exec_generation, error{code,message,kind} |
| `run_stopped` | run_id, exec_generation, reason, stop_origin, requires_fresh_auth, detail |
| `run_superseded` | run_id, by |
| `run_progress` | run_id, exec_generation, asks_done, asks_total, active_agents, elapsed_runtime_ms |
| `ask` / `ask_replayed` / `answer` | 现有字段 + exec_generation |
| `effect` | effect_key, call_index, op, args_hash, status, result_hash, script_generation |
| `hole_opened` | run_id, hole_id, script_generation, name, prompt |
| `hole_filled` | run_id, hole_id, script_generation, fill_hash, snapshot_path |
| `question_opened` | question_id, run_id, exec_generation, actor, question |
| `question_answered` | question_id, answer, answered_by |
| `effect_decided` | run_id, effect_key, decision, decision_exec_generation, substitute_result, decided_by |
| `process_confirmed` | run_id, exec_generation, process_ids, method, checked_at |

### 3.5 双代次与 hole

- `script_generation`：脚本内容代次；create=1，amend/hole_fill 各 +1。
- `exec_generation`：执行代次；每次 driver 启动 +1（初始=1，resume/hole 续跑各 +1）。
- `workflow_run.snapshot_*` 指向当前 `script_generation`；`exec_generation` 指向当前执行。
- qid = `dwfq-<run_id>-<exec_generation>-<seq>`；exec_generation 变化后旧 qid 失效。
- **effect 身份算法**（不含代次）：
  - `world.run` 给了 `idempotency_key`：`effect_key = fnv1a64("idem:" + idempotency_key)`，
    在 run 内唯一；复用前比对 `op` 与 `args_hash`，不同 → `effect_key_conflict`，**不得**换新身份重跑。
  - ask 内写：`fnv1a64("ask:" + actor + ":" + input_hash + ":" + ask_seq + ":" + call_site + ":" +
    invocation_index + ":" + op + ":" + args_hash)`。
  - 其它：`fnv1a64(call_site + ":" + invocation_index + ":" + op + ":" + args_hash)`。
  - `call_site` 是编译期静态调用点 id；`invocation_index` 是该调用点在本代脚本中第几次执行
    （循环里每次 +1，确定性重放可复现）；`ask_seq` 是同一 `(actor,input_hash)` 的第几次 ask。
  - 复用命中前必须核对 `op`/`args_hash`；不匹配一律 `effect_key_conflict` 并阻断，不得静默跳过。
  - `resume`（同 script_generation）与 `hole_fill`（`replay_prefix=1`）：已 `completed` 且 key 相同的 effect 跳过；
    `failed` 需逐项裁决后才可重跑；`started/unknown` 阻止启动。
  - `amend`（`replay_prefix=0`）：不导入任何 effect，全部重跑（新脚本视为新工作）；
    前驱已完成的 **ask** 仍按 actor+hash 导入（零调用），但**不会**因为 ask 导入而跳过 effect。
  - 这样 fill 只追加 tail 代码，前缀 effect key 不变，不会重复执行历史 world.run。
  - `is_write` 分类：`artifact.file/markdown`=1；`files.*`/`git.*`=0；`world.run` **一律 1**
    （没有可靠的只读判定，按最保守处理）；ask 内的 write/edit/delete/shell 写=1。
- **hole 切换协议**：
  1. **预检（在停旧 exec 之前）**：把正文在 hole 位置编译、跑 type_check、校验模型在
     `execution_config.allowed_models` 内、校验 `expected_script_generation`；失败直接返回，
     `workflow_hole.last_rejected_hash = hash(body)`，旧 exec 不受影响。
  2. CAS `workflow_hole.state: waiting→filling`（条件：run `running`、generation 匹配）；失败 `hole_not_waiting`/`generation_mismatch`。
  3. 取消旧 exec 令牌，停止派发新 ask；等待在途 ask 结算；旧 driver 写 `exec_aborted(exec_generation)` 并退出。
     **配额不释放**（仍归该 run）。
  4. 旧 driver 确认退出后：按 §3.4 文件协议写新快照，再事务提交
     `workflow_script_version(replay_prefix=1)` + `snapshot_*` + `script_generation+1` + `exec_generation+1`
     + `workflow_hole(state=filled)` + `hole_filled`。
  5. 启动新 driver；失败 → `errored`，不自动重试。
  6. 竞争：`filling` 期间 `CancelWorkflowRun` 优先——取消旧 exec、`workflow_hole(state=aborted)`、
     run → `stopping→stopped(user)`，Fill 返回 `fill_aborted`；`AmendWorkflow` 在 `filling` 返回 `invalid_state`。
  7. 崩溃恢复：
     - **宿主进程重启**：run 转 `stopped(interrupted)`，hole 一律 `aborted`（`hole_aborted`）；
       用户 Resume 后脚本重新执行到该 hole，产生新的 `hole_opened`/`hole_id`，再 Fill。
     - **仅 driver 任务崩溃、宿主仍在**：`filling` 且 DB 未出现新 `script_generation` → 回退 `waiting`（可重试）；
       已提交新版本但 driver 未启动 → `errored`，不自动重试。
- 只支持 **tail hole**：`hole<T>(name, prompt?)` 必须是所在顶层块最后一个表达式；AST 校验拒绝其他位置。
  分支级 hole 列入 P5。

### 3.6 后台执行、配额、租约与通知

```
CreateWorkflow.execute
  ├─ 校验（源唯一、大小、技能门 hash、项目/权限）
  ├─ 解析已有 request（命中直接返回；新请求才继续）
  ├─ 复制不可变快照 + hash
  ├─ 原子预留配额（WorkflowCapacity）
  ├─ 事务：INSERT run + script_version + request 行
  ├─ spawn WorkflowDriver（captures InheritedTurnContext）
  └─ 立即返回 {status:"backgrounded", run_id}

WorkflowDriver
  ├─ 持有配额许可直到终态
  ├─ 15s 心跳：run_progress + heartbeat_at + lease_until
  ├─ 副作用：effect(started) → 执行 → effect(completed/failed)
  ├─ 终态：事务（事件 + run 行 + outbox）
  └─ outbox dispatcher → workflow_notification(scheduled) → BusMessage → BackgroundWaker.notify
```

**后台模式的取消令牌（必须改代码）**：现有 `WorkflowRun` 在 `execute` 内取
`rclaw_tools::turn_state::turn_cancel()` 的子令牌并挂 `drop_guard`（`workflow.rs:2049-2052`）。
后台 driver 必须改为 **run 自有的 `CancellationToken`**：
- 不继承 `turn_cancel()`，不挂 `drop_guard`；
- driver 持有 token 直到终态；`CancelWorkflowRun`/`/cancel <run_id>` 只取消它；
- 同步模式（`workflow_background=false`）保留现有 drop-guard 语义；
- 父 turn `/cancel` 默认不波及后台 run（D7）。
不做这一步，`CreateWorkflow` 一旦提前返回，`drop_guard` 就会取消引擎，后台 run 根本跑不起来。

**配额与替换**：
- 新增 `WorkflowCapacity`，配置 `[agent] max_active_workflow_runs`（桌面默认 4、Android 默认 2）。
- 不复用 `MAX_ACTIVE_EXECUTIONS`（子代理上限，生命周期不同）。
- **amend 替换预留**：前驱 `running` 时，**许可由前驱持有**；后继只登记 `claim`（不占新许可）。
  前驱退出后在同一事务把许可所有权转给后继（若后继仍 queued/running），否则释放。
  后继 queued 取消只撤销 `claim`，**不释放前驱许可**；前驱终态才正常 `acquire`。
- 满额时，运行中 run 的 amend 仍可进行（claim 交接）；新 create 返回 `workflow_capacity_exceeded`。
- 许可只有一个所有者：前驱退出前是前驱，交接后是后继；终态/失败/取消统一由当前所有者释放。
- **归属持久化与重建**：`capacity_owner`（持许可的 run_id）/`capacity_claim_for`（queued 后继）
  写在 run 行；启动时在同一事务内重建：按 `capacity_owner IS NOT NULL` 计数
  （涵盖 running/stopping/已取得许可的 queued）；`capacity_claim_for` 只表示等待交接，不计入。
  无法唯一推导（如两行互指）→ 拒绝新请求 `capacity_unrecoverable`，只读模式等待人工处理。

**单实例与租约**：
- 工作流 driver 需要独占：启动时对 `agent/workflow.lock` 取排他文件锁。
- 拿不到锁 → 工作流服务进入**只读模式**：List/Get 可用，写工具返回 `instance_conflict`。
- 拿到锁即证明旧实例已退出：启动扫描可立即接管全部 `owner_instance != 当前` 的
  `running/stopping/queued`，不必等租约过期；租约（`lease_until = heartbeat + 60s`）只用于
  同实例的异常检测与持续扫描。
- 运行中每 60s 跑一次租约扫描：本实例 owner 但心跳停滞 > 120s 的 run **先转 `stopping(interrupted)`**
  并撤销执行权限（取消令牌、停止派发新 ask），**不能直接标 `stopped`**；确认引擎、子代理与命令进程树
  全部退出后才写 `stopped(interrupted)` 并释放配额。未确认退出期间不可 resume。
- 多实例共享同一 agent 目录不在支持范围（D9）。

**通知（at-least-once + 去重，单表实现）**：
- 事务内写 `workflow_notification(state=pending)`，`session_key = channel + "/" + chat_id`。
- dispatcher：事务把 `pending → scheduled` 并写 `scheduled_at`，**提交后**再发
  `BusMessage` + `BackgroundWaker.notify`。
- **确认点**：消息落到会话历史（`message` 表）与 `workflow_notification.state=consumed`、
  `consumed_at` 置位必须在**同一事务**；置位后不再重发。
- `scheduled` 但超时未 `consumed`（默认 60s）→ 回到 `pending` 重发；接收端按 `id` 去重。
  提交 `scheduled` 后崩溃、或消息已注入但 `consumed` 未提交，都走这条路径。
- 重试 24h 仍失败 → `dead` + `last_error`；`GetWorkflowRun.notifications` 可见，
  `workflow_resend_notification` 可重投。
- 验收语义是"至少一次 + 接收端去重"，不承诺 exactly-once。

**兼容开关（P1 起）**：`[agent] workflow_background`（默认 `true`）。`false` 为同步执行，但**仍写 v3 表（P1 五张）**；
返回契约、取消、通知差异见 §13.8。已有 run 保持启动时写入 `execution_config.background_mode` 的模式。

### 3.7 权限与项目隔离

- **权限比较**：`ToolAccess` 定义序 `Sandboxed(0) < Unrestricted(1)`；统一用 `current >= snapshot`。
- **项目标识**：`project_root = canonicalize(Layout.base)`（§3.0）；去掉 Windows `\\?\` 前缀 →
  Windows 上 ASCII 小写化。`workflow_run.cwd` 列存的就是这个值（列名沿用 `cwd`，语义是 project_root）。
  查询与写入都走同一函数；不做字符串前后缀匹配。
- **所有 run 级工具**（Get/Amend/Resume/Cancel/Fill/Resolve）：
  1. `run.cwd == canonicalize(Layout.base)`，否则 `forbidden_project`；
  2. `current_access >= execution_config.tool_access`，否则 `permission_downgraded`；
  3. 写操作（Amend/Resume/Cancel/Fill/Resolve）还要求当前会话是特权会话（cli/desktop）或
     `session_key` 与 run 一致；否则 `forbidden_session`。
- **legacy 例外**：`legacy=1` 且 `cwd=''` 的行，`GetWorkflowRun` 在 `Unrestricted` +
  `include_legacy=true` 时允许读取；任何写工具一律 `forbidden_project`（§13.5）。
- **UI 管理接口同权**：`workflow_confirm_effects`、`workflow_confirm_process_gone`、
  `workflow_resend_notification` 走同一服务端鉴权（项目 + `ToolAccess` 快照 + `exec_generation`），
  仅 `Unrestricted` 桌面/特权 UI；模型工具面不存在这些入口。
- **运行中权限变化**：driver 在每次派发 ask、每次实际写操作前重新检查当前会话权限与 `ToolAccess`；
  撤权立即停止派发、过期未决问题/hole，run 转 `stopping(permission_revoked)`，确认退出后 `stopped`。
- `ListWorkflowRuns` 只按 `project_root`（`Layout.base`）过滤；legacy 行 `cwd=''` 仅在
  `include_legacy=true` 且特权会话可见。
- **`world.run` 不新增逐命令审批**：复用 `shell` 的 `ToolAccess` 与路径确认语义——
  `Sandboxed` 一律拒绝；`Unrestricted` 直接执行（与现有 `shell` 工具一致）。
  RClaw 没有 per-command approval hook，`access.rs` 的确认钩子只覆盖“工作区外路径”，不覆盖命令本身。
  若未来要逐命令审批，需要新增 `set_command_confirm_hook` + 桌面 UI，列入 P5（§12）。
- **`world.run` 生命周期**：driver 以进程组/Job Object 持有命令进程树。
  Windows 使用 Job Object 且 `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，宿主进程退出由 OS 回收整棵树；
  Unix 使用独立进程组，启动恢复时先 `kill(-pgid, 0)` 探测。取消时先 TERM、宽限后 KILL，
  只有确认整棵树退出才写 `completed/failed`。超时、断连、取消、宿主崩溃一律记 `unknown`，
  阻止自动 resume（§5.3），由 §5.14 逐项裁决。
  **不能确认进程树已退出的平台**：run 保持 `stopping(interrupted)`，禁止 resume 与后继启动，
  直到人工确认（`workflow_confirm_process_gone` 提供的 OS 探测证据）。文件锁只证明宿主退出，
  不能替代进程树确认。
- 子代理继续被 `is_subagent_blocked_tool` 屏蔽工作流工具（`ResolveWorkflowQuestion` 主代理专属）。

---

## 4. 入口、技能与触发约束（P0）

### 4.1 `/workflow` 命令

**行为**：`/workflow <请求>` 不当作运行时命令拦截，而是在 `TurnManager::route` 开头展开为 prompt，再走正常 turn。

展开正文：

```
Use the `dynamic-workflows` skill to design and launch a dynamic workflow for this request:

$ARGUMENTS

Decide the subagent topology before writing any code: how many subagents, which of them
share a context, what result each one returns. Then write the script and call the
`CreateWorkflow` tool. Do not substitute the `Agent` tool and do not do the work inline.
```

**改动点**：

1. `crates/rclaw-runtime/src/turn.rs::route`（`turn.rs:978`）：
   - `dispatch` 之前识别 `^/workflow(?:\s+([\s\S]*))?$`（大小写不敏感）；
   - **不能直接替换 `msg.content`**：`BusMessage` 只有 `content`，替换后历史/UI 里只剩长 prompt，
     用户看不到自己输入的 `/workflow ...`。给 `BusMessage` 增加两个可选字段：
     `display_content`（历史/UI/回放）与 `model_content`（LLM 请求与工具可见文本），默认都等于 `content`。
     命中时保留原文到 `display_content`，展开文本写 `model_content`；
   - 空参数回送用法提示，不启动 turn。
2. `crates/rclaw-core/src/bus.rs`：新增两个字段并保持所有现有构造函数默认填充 `content`；
   历史落库/回放读 `display_content`，构造 LLM 请求读 `model_content`。
3. `crates/rclaw-runtime/src/slash.rs`：`/help` 加 `/workflow <请求>  设计并运行一个动态工作流`；补文档注释。
4. `/workflow` 不进 `RuntimeCommand`（否则会被当"已处理"跳过 LLM）；自定义命令解析处拒绝同名。
5. 全通道生效：CLI/桌面/微信/QQ 都走 `route`。
6. 不做 on-demand 工具面：工具 schema 常驻（可接受差异）。

**验收**：历史/UI 显示原始 `/workflow <请求>`；模型收到的 `model_content` 以
`Use the dynamic-workflows skill` 开头且 `$ARGUMENTS` 已替换；不产生命令回复；
`/help` 可见；同名自定义命令不能覆盖。

### 4.2 工具描述的触发约束

`CreateWorkflow` 常驻：

```
When to use:
- The user explicitly asks for a workflow — "use a workflow", "with a workflow",
  "使用 workflow", "用工作流", or any phrasing that names workflow/工作流 as the means:
  this tool is mandatory. Do not substitute the Agent tool and do not do the work inline.
- Without such an explicit request, do not start a workflow: delegate with the Agent tool
  or do the work yourself, even for multi-step or multi-subagent tasks.
```

`SaveWorkflow` 常驻：`NEVER call this tool unsolicited: saving writes a file into the user's repository.`
`AmendWorkflow`：不要先停、不要重写、用 Amend。
`ResumeWorkflowRun`：仅 stopped 可续；用户取消的 run 不得自行恢复。
`FillWorkflowHole`：只在 hole 通知/Get 指示时调用。

### 4.3 技能与技能门

**技能文件**：`<Layout.skills_dir>/dynamic-workflows/SKILL.md`（默认 `agent/skills/...`）按阶段扩写：
P0 只写 Create/Amend/Resume/Resolve + 现有 facade；P1 加 List/Get/Cancel + 状态机 + 通知；
P2 加 args/Save/ListSaved；P3 加 log/files/git/world；P4 加 phase/report/artifact(file,markdown)/hole。

**技能门**：
- 机制：`read` 工具**完整**读取 `<Layout.skills_dir>/dynamic-workflows/SKILL.md`（未截断，见 §13.10）时，在会话 kv 写入：
  `{content_sha256, contract_version, read_at}`，键 `workflow-skill/<channel>/<chat_id>`。
- 校验：提交脚本前要求 `content_sha256 == 当前文件 hash` 且 `contract_version >= 工具要求的版本`；
  技能文件更新后旧标志自动失效。上下文压缩不影响（kv 持久）。
- 未满足：`workflow_skill_required: read <Layout.skills_dir>/dynamic-workflows/SKILL.md first`。
- 豁免：`CreateWorkflow` 的 `saved` 来源。
- P0 若会话 kv 成本过高，可退化为"描述内嵌 + 不硬门"，但必须在 §11 决策记录标为已接受差异。

---

## 5. 工具契约

字段统一 `snake_case`；所有工具主代理专属。

### 5.0 公共约定

**错误结构**（`ToolResult.content` JSON，`is_error=true`）：

```json
{"ok":false,"code":"invalid_state","message":"...","retryable":false,
 "recovery":"call GetWorkflowRun and wait","kind":"state"}
```

`kind`：`validation|state|permission|capacity|not_found|provider|internal`。
规范错误码与兼容别名见 §13.7；各工具只列特有码。

**幂等**：
- `CreateWorkflow`/`AmendWorkflow`/`ResumeWorkflowRun` 接受 `request_id`（≤128）；P1 起**所有成功响应都回传 `request_id`**（未传则生成）。
- 事务内 `INSERT workflow_request(state=in_progress, digest, run_id)` 与 run/queued 行**同一事务**；
  提交时把 `state=done` 与响应写回同一行（提交点 = 该事务提交）。
- 唯一键冲突时读现有行：
  - `done` 且 digest 相同 → 返回原响应；
  - `in_progress` 且 digest 相同且 `run_id` 已存在且 run 存在 → 返回该 run 的当前状态（不重复创建）；
  - `in_progress` 且 `run_id` 为空 → 上一事务未提交，标 `failed` 后允许重试；
  - digest 不同 → `request_conflict`。
- `failed` 不缓存响应，同 id 可重试（重置为 `in_progress`）。
- **顺序**：权限/项目检查 → 解析已有 request（命中直接返回原响应）→ 仅新请求执行配额预留与
  run 状态变更。已成功请求在容量满额或 `already_superseded` 状态下重试，必须返回原响应，
  不能先报 `capacity_exceeded`/`already_superseded`。

**路由矩阵**：

| 工具 | 调用者 | 用户授权前提 | 允许状态 | 技能门 | 通知 |
|---|---|---|---|---|---|
| CreateWorkflow | 主代理 | `/workflow` 或明确点名 | — | 是（saved 豁免） | 终态 |
| AmendWorkflow | 主代理 | 运行中修订需用户要求；`stopped(user)` 的恢复/修订需**新的**用户授权 | running/stopped/completed/errored | 改脚本时是 | 后继终态 |
| ResumeWorkflowRun | 主代理 | 用户要求；`stopped(user)` 不得自行恢复 | stopped（非 superseded） | 否 | 终态 |
| ListWorkflowRuns | 主代理/UI | 只读 | 任意 | 否 | 无 |
| GetWorkflowRun | 主代理/UI | 只读 | 任意 | 否 | 无 |
| CancelWorkflowRun | 主代理/UI | 用户要求或失控 | running/queued | 否 | 停止通知 |
| SaveWorkflow | 主代理 | 用户同意/直接要求 | 任意 | 是 | 无 |
| ListSavedWorkflows | 主代理/UI | 只读 | 任意 | 否 | 无 |
| EvalWorkflowSnippet | 主代理 | 已要求工作流 | 任意 | 是 | 无 |
| FillWorkflowHole | 主代理 | 仅 hole 通知/Get | running + hole waiting | 是 | 无 |
| ResolveWorkflowQuestion | 主代理 | 仅 qid 通知/Get | running | 否 | 无 |

> `stopping` 对所有写工具一律 `invalid_state`；Amend/Cancel 不接受 `stopping`。
> Cancel 对 `queued` 直接置 `stopped(user)` 并撤销 claim；Resume 对 `queued` 拒绝；
> `stopped(user)` 的 Resume/Amend 都需要用户新的明确请求，模型不得仅凭"继续"自行发起。

### 5.1 `CreateWorkflow`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | 否 | 标签，默认 `workflow` |
| `script` | string | 三选一 | ≤100k 字符 |
| `path` | string | 三选一 | 启动前一次性读入并复制快照（§13.9） |
| `saved` | object | 三选一 | `{name,args?,scope?}`；P2；`scope=global` 仅 `Unrestricted` |
| `args` | object | 否 | 仅 `path` 使用；`saved` 用 `saved.args`；同时出现 → `args_conflict` |
| `max_parallel_asks` | integer | 否 | 1-16，默认 8；别名 `max_concurrency`（Create/Amend 同规则） |
| `subagent_model` | string | 否 | P2 |
| `max_run_secs` | integer | 否 | 1-86400，默认 3600 |
| `type_check` | boolean | 否 | 默认 true |
| `request_id` | string | 否 | 幂等键（P1 起） |

**输出**（P1+）：

```json
{"status":"backgrounded","run_id":"wf-...","label":"PR review","script_generation":1,
 "exec_generation":1,"snapshot_path":"workflow-drafts/wf-.../g1.dwf.js",
 "snapshot_hash":"0123456789abcdef","request_id":"req-...","note":"Use GetWorkflowRun."}
```

P0 同步契约见 §6.0。

**失败码**：`workflow_skill_required` / `source_conflict` / `script_too_large` / `script_diagnostics` /
`path_not_found` / `path_unreadable` / `saved_not_found` / `forbidden_global` / `args_invalid` /
`capacity_exceeded` / `request_conflict` / `request_in_progress` / `alias_conflict`。

### 5.2 `AmendWorkflow`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `run_id` | string | 是 | 前驱 |
| `script` | string | 至多一个 | 修订脚本 |
| `path` | string | 至多一个 | 一次性读入并复制快照（§13.9） |
| `name` | string | 否 | 新标签 |
| `max_parallel_asks` | integer/null | 否 | `null` 清除运行级设置，全局硬顶 16 不变 |
| `subagent_model` | string/null | 否 | `null` 回会话模型 |
| `type_check` | boolean | 否 | 默认 true |
| `request_id` | string | 否 | 幂等键（P1 起） |
| `auth_token` | string | 条件 | `requires_fresh_auth=1` 时必填 |

**至少一项实际变更**：否则 `nothing_to_change`。`script`+`path` 同给 → `source_conflict`。

**模式 A：原地调参（retune，P1 起）**
- **P0 不支持**：现有引擎在 `execute` 开头读一次 `max_parallel_asks` 并建 `Semaphore`
  （`workflow.rs:1929` 附近），没有共享原子；P0 的同步薄包装做不到运行中调参。
  P0 的 Amend 只走模式 B（对已完成/失败 run 重建），`retuned` 返回从 P1 起才出现。
- 条件：前驱 `running`，且只改 `max_parallel_asks`/`subagent_model`，且值确实变化。
- 事务：更新 `execution_config` + 写 `run_retuned`；输出 `{status:"retuned", run_id}`。
- 生效边界：降低并发只管**尚未开始**的 ask；在跑 ask 不取消。模型切换对下一个 ask 生效。
- `execution_config.allowed_models` 同步加入新模型（保留旧模型，供在跑 ask 完成）；
  Fill 的模型校验以该集合为准。
- 重启/resume 从 `execution_config` 恢复新值。

**模式 B：后继（supersede）**
- 条件：其余情况（改脚本/改名/前驱终态）。
- 顺序：
  1. 预检（编译、技能门、项目/权限）。
  2. 容量：前驱 running → `replace` 交接许可；终态 → `acquire`。
  3. 事务：写后继 `status=queued`、`queued_reason=predecessor_stopping|none`；
     前驱 `running` → `stopping(superseded)`，终态 → 仅写 `superseded_by`；写版本行/request 行。
  4. 若前驱 running：触发取消令牌；**等前驱 `stopped`**（最长 10s，超时保持 queued）。
  5. 前驱退出后，事务：后继 `running`，`exec_generation+1`，启动 driver。
- **queued 期间不执行任何脚本/副作用**。
- 前驱只允许一个后继：`superseded_by` 非空 → `already_superseded`（后继创建失败的重试除外）。
- **整链检查**：后继启动前遍历 `superseded_by` 链；任一祖先仍 `running/stopping/queued`，
  后继保持 `queued`，不启动、不接管许可。
- **失败处理**：
  - 3 之前失败：无变化。
  - 3 之后、5 之前失败：前驱保持 `stopping`（或终态 + superseded_by），后继 `errored`；
    **不承诺恢复前驱原状态**；恢复动作 = Amend 后继（errored）或从快照 Create。
  - 5 启动失败：后继 `errored`，前驱 `stopped(superseded)`；同上恢复。

**缓存边界（只限制 Amend 的可选复用，不影响 Resume 重放）**：
- Resume 必须按**本 run 自己的 journal** 重放已答 ask 与 effect；缓存关闭不能阻止这种重放，
  否则会重新调用已经执行过写操作的 ask。
- Amend 从**前驱**导入已完成 ask 是可选的；任一 live 工作区写**开始前**原子写
  `run_cache_closed(cache_closed_at_event_id)` 并置 `workflow_run.cache_closed=1`。
  此后 Amend 不再导入任何前驱 ask；此前已返回的答案不回滚。
- `run_id` 通过 `AgentTaskSpec`/`InheritedTurnContext` **显式传递**到所有写工具与子代理，不依赖 task-local；
  写工具在取得写锁/开始执行前调用 `WorkflowWriteTracker::close(run_id)`。
- `effect` 记录与 `cache_closed` 同事务落库。
- Amend 不导入 effect。若前驱存在 **任何 write effect**（direct 或 ask 内，`is_write=1`；
  status ∈ completed/failed/unknown/started），**v1 一律拒绝启动后继** `amend_blocked_direct_effect`；
  不提供"逐项确认后重跑"的放行路径（没有可靠的 effect→后继调用映射）。
  映射规则与放行在 P5 实现后再开放。写命令应提供幂等键，但不能把"交给 ask"当作防护。

**输出**：
`{"status":"backgrounded","run_id":"wf-new","superseded_run_id":"wf-old","script_generation":2,"request_id":"..."}`
或 `{"status":"retuned","run_id":"wf-old","request_id":"..."}`（P1 起）。
实际导入/重放计数在执行中才能确定，不出现在即时响应；终态由 `GetWorkflowRun` 与
`run_completed` 事件提供。

**授权**：`stopped(user)` 前驱的 Amend 需要用户新的明确请求；模型不能把"用户之前取消过"解释为"继续"。
`completed`/`errored` 的 Amend 若只是修复，可沿用用户的原始工作流请求。

**失败码**：`not_found` / `forbidden_project` / `forbidden_session` / `permission_downgraded` /
`invalid_state` / `nothing_to_change` / `source_conflict` / `script_unchanged` / `already_superseded` /
`amend_failed` / `amend_blocked_direct_effect` / `snapshot_missing` / `capacity_exceeded` / `request_conflict`。

### 5.3 `ResumeWorkflowRun`

**输入**：`run_id`（必填）、`request_id?`、`auth_token`（`requires_fresh_auth=1` 时必填）。P0 无快照，**还必须提供原 `script`**，服务端用它与 journal
`run_started.script_hash` 比对（不一致 `script_mismatch`），再重放；P1 起改由不可变快照校验，
`script` 不再需要。`allow_unknown_effects` **不是工具参数**（见 §5.14）。

**输出**：`{status:"backgrounded", run_id, exec_generation:N, note}`。
重放计数在恢复执行后才确定，由 `GetWorkflowRun` 的 `ask_stats` 与终态事件提供，不在即时响应里。

**规则**：
- 仅 `stopped` 且 **`superseded_by IS NULL`**；`errored` → `not_resumable`；`superseded` → 指向后继；
  `running/stopping/queued` → `invalid_state`。`superseded_by` 非空时无条件拒绝，不看 `stop_reason`。
- 当前 `script_generation` 的 hash 必须与 `snapshot_hash` 一致 → 否则 `script_mismatch`；
  快照文件缺失 → `snapshot_missing`。
- **副作用闸门（所有重新启动执行的路径共用）**：Resume、FillWorkflowHole、Amend 后继启动前，
  都检查本执行谱系（同一 run + 前驱链）是否存在 `started/unknown` effect；有则拒绝 `resume_blocked_effect`。
  唯一放行路径是 UI/特权会话通过 `workflow_confirm_effects` 对**具体 effect_key 列表**逐项裁决
  （`skip`/`rerun`），裁决持久化在 `workflow_effect.decision/decided_by`；模型工具没有该参数。
- `exec_generation+1`；同 run id；`ActiveRunGuard` 防并发；继承 `execution_config`；
  可用预算 = `max_run_secs - elapsed_runtime_ms`（§3.4），多次续跑共享上限。
- **配额**：状态切换前原子 acquire 全局工作流许可；满额 `capacity_exceeded`；与 Create 共用容量。
- 权限降级 → `permission_downgraded`。
- 停止来源持久化：`stop_origin` + `requires_fresh_auth` 跨重启保留；`user` 取消的 run，
  Resume 与 Amend 都需要用户新的明确请求，模型不得仅凭"继续"发起。
  **凭据**：`requires_fresh_auth=1` 时调用必须带 `auth_token`（UI/用户消息签发，绑定
  `run_id + request_id`，一次性消费）；模型工具不能生成或伪造该 token。`stop_origin` 由
  消息来源/UI 事件派生，`CancelWorkflowRun` 的 `reason` 不能由模型自行声明为 `user`。

### 5.4 `ListWorkflowRuns`

**输入**：`limit`（1-50，默认 20）、`statuses?`、`include_legacy?`（默认 **false**，仅特权可置 true）。

**输出**：

```json
{"runs":[{"run_id":"wf-...","label":"PR review","status":"running","stop_reason":null,
 "resumed_from":null,"superseded_by":null,"owned_by_this_session":true,
 "possibly_interrupted":true,"legacy":false,"resumable":true,
 "created_at":1760000000000,"updated_at":1760000100000,"spent_tokens":12345}],
 "truncated":false}
```

- 查询键是 `project_root = canonicalize(Layout.base)`（§3.0）；同一进程的所有会话共享它，
  归属用 `owned_by_this_session` 区分。
- **逐行权限过滤**：每行仍要求 `current_access >= execution_config.tool_access`；不满足的行不返回，
  `truncated=true` 并给 `filtered` 计数。降权会话不能读到高权限 run 的标签/状态/用量。
- `possibly_interrupted` 只表示**当前** `running` 且 `heartbeat_at` 超过 45s；
  历史扫描产生的 `stopped(interrupted)` 用 `stop_reason` 表达。
- `legacy=true` → `resumable=false`，`cwd=''` 的行只在特权 + `include_legacy` 时出现。
- 回填在后台按游标执行，List 只读，不以"表空"为条件。

### 5.5 `GetWorkflowRun`

**输入**：`run_id`（必填）、`reports_cursor?`（reports 分页，默认首页）、
`include_legacy?`（默认 false；true 仅 `Unrestricted` 生效）、`include_effects?`（默认 false）、
`effects_cursor?`（默认首页）。

**输出**（P1 基础 + P4 扩展）：

```json
{"run_id":"wf-...","label":"PR review","status":"running","stop_reason":null,
 "created_at":0,"updated_at":0,"spent_tokens":0,
 "snapshot_path":"...","snapshot_hash":"...","script_generation":2,"exec_generation":3,
 "progress":{"asks_done":3,"asks_total":10,"active_agents":2},
 "ask_stats":{"asks_total":10,"asks_done":3,"asks_replayed":0,"exec_generation":3},
 "log_tail":[],"pending_questions":[],"holes":[],
 "phases":[],"reports":[],"artifacts":[],
 "result":null,"result_handle":null,"error":null,"recovery":null,
 "notifications":[{"id":"...","state":"pending|scheduled|consumed|dead","attempts":2,"last_error":"..."}]}
```

- 瞬时快照，不等待；**不提供 AgentWait 映射**（等待用通知）。
- **响应预算**：单次 Get 正文 ≤48k 字符；`result` 超过 48k 时写 artifact，只返回 `result_handle`
  （句柄可按 4KB 分页读取，复用 `turn/artifact.rs`）。
- **统一截断顺序**（保证总响应 ≤48k）：先截 `reports`/`artifacts` 元数据并给 cursor，再截 `log_tail`，
  最后把 `result` 转 `result_handle`；被截断的集合必须返回续页游标。
- **其余字段上限**：`label` ≤200、`pending_questions` ≤50（问题正文 ≤2k/条）、`holes` ≤50
  （prompt ≤2k/条）、`phases` ≤200（名称 ≤100）、`error` ≤4k；
  `artifacts`/`log_tail` 分别有 `artifacts_cursor`/`log_cursor` 输入输出。
- `reports` 分页：默认 50 条/页，`reports_cursor` 续页；`artifacts` 只列元数据（≤100 条）。
- P4 字段：`phases[{name,status,started_at,finished_at}]`、`reports[{seq,item_preview}]`、
  `artifacts[{id,kind,title,version,primary,uri}]`；数据来自 journal 投影 + artifact 存储。
- `log_tail` P1 为空数组（P3 才有 `log`）。

### 5.6 `FillWorkflowHole`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `run_id` | string | 是 | — |
| `hole_id` | string | 是 | `hole#...` |
| `expected_script_generation` | integer | 是 | 防并发填充 |
| `script` | string | 二选一 | 仅函数体语句 |
| `path` | string | 二选一 | 填充文件 |

**输出**：`{status:"filled", run_id, hole_id, script_generation:N, exec_generation:N, snapshot_path, phases_added:[]}`。

**规则**：
- 仅 `running` 且 `workflow_hole.state=waiting` 且代次匹配；否则 `hole_not_waiting` / `generation_mismatch`。
- **预检必须在停旧 exec 之前**：在 hole 位置编译正文、type_check、校验模型 ∈ `execution_config.allowed_models`；
  任一失败保持 `waiting`，只记 `last_rejected_hash`，旧 exec 继续跑。
- 只支持 tail hole（§3.5）。
- 执行谱系存在 `started/unknown` effect → `resume_blocked_effect`（与 Resume 同一闸门，§5.3/§5.14）。
- 原子顺序：CAS `waiting→filling` → 停旧 exec、等在途 ask → 文件快照 + 事务提交新版本/代次/`filled` → 启动新 driver。
  最终提交事务必须再次 CAS：run 仍 `running`、hole 仍 `filling`、`script_generation` 未变、取消标记未置位；
  启动 driver 前再校验一次取消令牌。失败则回退/中止，不启动。
- 竞争：`filling` 期间 Cancel 优先（Fill 返回 `fill_aborted`，hole 置 `aborted`）；Amend 返回 `invalid_state`。
- 崩溃按 §3.5 三分支处理：宿主重启 → hole `aborted`（Resume 后重新打开）；仅 driver 崩溃且未提交
  → 回退 `waiting`；已提交但 driver 未启动 → `errored`。扫描顺序见 §13.1。
- `path` 字节与 `last_rejected_hash` 相同 → `fill_unchanged`。
- 正文只能使用 `execution_config.allowed_models` 里的模型；Amend retune 增加模型时同步更新该集合。

### 5.7 `ResolveWorkflowQuestion`

**输入**：`question_id`（`dwfq-<run_id>-<exec_generation>-<seq>`）、`answer`（≤4000）。

现有 PlanRun 的 qid 是 `<plan_id>:<step_id>:q-<ts>-<n>`（`escalate.rs:34-36`）。
P1 要给 parser 增加 `dwfq-` 分支，两套格式按前缀分派；不能直接复用现有冒号解析。

**输出**：`{status:"answered", question_id, run_id}`。

**规则**：
- 仅所属 run `running` 且问题 `pending`；`unknown_question` / `already_resolved` / `run_not_in_flight`。
- **答案裁决是 CAS**：`UPDATE workflow_question SET state='answered', answer=? WHERE question_id=? AND state='pending'`；
  只有一个并发回答成功，其余返回 `already_resolved` + 首次答案摘要，不覆盖。
- 答案先落库（同事务写 `question_answered`），再唤醒等待的 actor；若提交后、唤醒前崩溃，
  启动扫描按 `question_answered` 重新投递，不重复计费。
- exec_generation 变化后旧 qid 失效；resume 后重新提问会生成新 qid。
- **跨代次问题身份**：`question_key = fnv1a64(ask_identity + ":" + ask_seq + ":" + question_occurrence)`。
  `ask_identity` = `actor + input_hash + 该 ask 在 run 内的 occurrence`；
  `question_occurrence` = 同一 ask 内第几次 escalate。相同 key 已回答的答案在 Resume 时复用
  （不再问、不重复计费），新 qid 只服务本次执行的通知与应答；旧代等待者随旧代终结。
  并发顺序变化或跳过已完成 ask 时，key 仍指向同一 ask，不会误用另一任务的答案。
- `GetWorkflowRun.pending_questions` 为补答入口；进入等待与 outbox 插入同一事务。

### 5.8 `SaveWorkflow`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `name` | string | 是 | `[A-Za-z0-9._-]+`，≤64 |
| `description` | string | 是 | — |
| `when_to_use` | string | 是 | — |
| `scope` | enum | 是 | `project`/`global`，无默认 |
| `script` | string | 二选一 | 仅正文 |
| `script_path` | string | 二选一 | 快照/草稿路径 |
| `overwrite` | boolean | 否 | 覆盖同名文件必须显式 true |
| `expected_hash` | string | 否 | 覆盖前置 hash；不匹配 → `save_conflict` |
| `args` | object | 否 | 参数声明映射 |

**args 声明（Save 时校验，`args_decl_invalid`）**：
- 参数名 `[A-Za-z_][A-Za-z0-9_]*`，不重复；
- `type` ∈ `string|number|boolean|json`；
- `required` 与 `default` 不得同时出现；
- `default` 类型必须与 `type` 一致。

**args 取值（Create 时校验，`args_invalid`）**：
- 未知参数名 → `args_invalid`；
- `required=true` 且无 default 缺失 → `args_invalid`；
- 有 default 缺失 → 填默认；
- 类型不符 → `args_invalid`；
- `saved` 的取值入口是 `saved.args`；顶层 `args` 只用于 `path`；同时给 → `args_conflict`。

**输出**：`{status:"saved", path, scope, overwritten:bool}`。
路径：project `<project_root>/workflows/<name>.dwf.js`；
global `<workflow_home>/workflows/<name>.dwf.js`（`workflow_home()` 由 `rclaw-store` 按
`desktop/home.rs` 同序解析，覆盖 Windows `%USERPROFILE%\.rclaw`、`RCLAW_HOME`、XDG 与 Android app-private）。
元数据块 `/* rclaw-workflow ... */`（YAML）。解析用 `serde_yaml`（新依赖）。
保存前跑同一语法/type_check；失败不写；无变更不写。
- **原子替换**：校验通过后写同目录临时文件，`fsync` 后 rename 覆盖；同一 `name` 用文件锁串行化。
  覆盖已存在文件必须 `overwrite=true`；若同时给 `expected_hash`，还必须在同名文件锁内与当前文件比对，
  不匹配 → `save_conflict`。缺 `overwrite` 一律 `save_conflict`；失败保留旧文件。校验使用一次性读入的完整内容（§13.9）。

### 5.9 `ListSavedWorkflows`

**输入**：`scope?`。
**输出**：`{"workflows":[{"name","description","when_to_use","scope","path","args","shadowed"}]}`。
project 覆盖 global 时 `shadowed:true`。

### 5.10 `EvalWorkflowSnippet`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---|---|
| `code` | string | 二选一 | — |
| `path` | string | 二选一 | — |
| `timeout_ms` | integer | 否 | 1000-600000，默认 60000 |

**输出**：`{result, logs:[], diagnostics:[]}`。

**规则**：
- 与真实 run 同一编译器/沙箱；无 agent/report/artifact/hole/phase。
- 不落 journal、不建 run、不写表。
- 返回 ≤256KB。
- **`world.run` 仅 `Unrestricted` 会话可执行**；`Sandboxed` 返回 `forbidden_world`。
  没有逐命令审批（与 `shell` 一致，§3.7）；cwd 必须在 `project_root` 下。
  若未来需要逐命令审批，走 P5 的 `set_command_confirm_hook`（§12）。
- 片段有独立执行令牌；`world.run` 超时取 `min(300s, 剩余 timeout_ms)`。片段超时/取消时
  终止并确认回收整个命令进程树；未确认退出 → `world_run_unknown`，不自动重试。

### 5.11 `CancelWorkflowRun`

**输入**：`run_id`、`reason`（`user`/`model`；工具默认 `model`，UI 默认 `user`）。

**输出**：
- `queued`：`{status:"stopped", run_id, stop_reason}`（立即终态）；
- `running`：`{status:"stopping", run_id, stop_reason}`；确认退出后再发 `stopped` 通知；
- 10s 未确认：`is_error=true, code:"stop_pending"`，状态仍为 `stopping`。

**规则**：
- `running` 或 `queued` 可取消；`stopping` → `invalid_state`；终态 → `not_running`。
  `queued` 直接置 `stopped(reason)` 并撤销 claim，不触发引擎；`reason` 按调用来源取
  `user`（UI/用户消息）或 `model`（模型工具），不无条件写 `user`。
- 触发取消令牌；**引擎线程与所有子代理确认退出后才写 `stopped`**。
- 10s 宽限未确认 → 保持 `stopping`，返回 `stop_pending`；不可 resume。
- 进程重启把 `stopping` 转 `stopped(interrupted)`；仍受 effect 检查。
- 桌面卡片、`/cancel <run_id>`、工具调用复用同一实现。

### 5.12 结构化失败

`error = {code,message,retryable,recovery,kind}`；`kind` 见 §5.0。
ZCode 的 `recoveryActions` 多动作数组不在本期（§12）；`recovery` 只给一句下一步。

### 5.14 未知副作用确认（UI 专用，非模型工具）

```
workflow_confirm_effects(run_id, exec_generation,
                         target_request_id?: string,
                         decisions: [{effect_key, action: "skip"|"rerun",
                                      substitute_result?: unknown}])
```

- 仅桌面/特权 UI 可调用；模型工具面**不存在**该入口。
- 必须覆盖该 run 全部 `failed/started/unknown` effect；缺项 → `effects_unconfirmed`。
- `skip`：按已完成处理，不重跑；必须提供 `substitute_result`（JSON）作为返回值，
  否则脚本读取返回值时失败（`effect_result_required`）。`rerun`：允许再执行一次。
- 裁决写 `workflow_effect.decision/decision_exec_generation/decided_by`，绑定
  `(run_id, effect_key, exec_generation)`；effect 重新进入 `started` 时**原子消费**该 `rerun` 授权
  并清除旧裁决，下一次事故必须重新授权。
- **Amend 专用**：v1 对存在**任何 write effect**（direct 或 ask 内）的 run **不开放 Amend**（§5.2 直接拒绝），
  因此没有"前驱 effect → 后继调用"的映射要求。P5 实现映射与参数一致性校验后再开放确认入口。
- **查询入口**：`GetWorkflowRun` 在 `include_effects=true` 时返回
  `effects[{effect_key, op, is_write, status, args_preview, result_preview, decision}]`，
  供 UI 逐项裁决；该字段受 48k 预算与分页约束。

### 5.15 进程树确认（UI 专用，P3 起，非模型工具）

```
workflow_confirm_process_gone(run_id, exec_generation,
                              process_ids: string[],
                              evidence: {method: string, checked_at: int})
```

- 仅 `Unrestricted` 桌面/特权 UI；服务端校验项目、权限快照与 `exec_generation`。
- 必须覆盖该代 `workflow_process` 中全部 `running/unknown` 的 `process_id`；缺项 → `process_unconfirmed`。
- 成功：对应行置 `state=exited`、写 `process_confirmed` 事件；之后才允许 `stopped`、
  Resume 或后继启动。伪造/过期代次 → `invalid_state`。
- 这是"文件锁只能证明宿主退出"的补偿入口；模型工具面不存在该入口。

`workflow_resend_notification(notification_id)`：仅 `dead` 通知可重投；重置为 `pending`、`attempts=0`，
保留原 `id` 供接收端去重；仅 `Unrestricted` UI 可调用。
- 用途：让 §5.3 的 `resume_blocked_effect` 有一条真实可调用的放行路径。

### 5.13 facade 契约附录

**公开类型**（facade.d.ts 的等价定义）：

```ts
interface GrepMatch { path: string; line: number; text: string }
interface GitStatus { branch?: string; clean: boolean; staged: string[]; modified: string[] }
interface ArtifactRef { id: string; version: number }
interface ArtifactOptions { title?: string; description?: string; primary?: boolean }
interface ArtifactFileOptions extends ArtifactOptions { contentType?: string }
```

P4 只实现 `artifact.file` / `artifact.markdown`；`report` 的 `artifactId`、`artifact.chart/table/metrics/board`
与“预设声明”机制属于 P5，本期 facade 中不存在。

| 成员 | 签名 | 语义/限制 | 重放 |
|---|---|---|---|
| `args` | `Readonly<Record<string, unknown>>` | 保存/路径参数；无则 `{}`；不可变 | 来自 execution_config |
| `agent` | `agent(name?: string, persona?: {model?: string}): Agent` | 新上下文；名字是 amend 缓存键；本期只支持 `model`，字符串 persona 报 `persona_unsupported` | — |
| `ask` | `ask<T = string>(instructions: string): Promise<T>` | 每次启动一个有界子代理；`T` 生成 schema | 按 `(actor,inputHash,第几次)` 重放 |
| `askJson` | `askJson<T>(instructions: string, schema: object): Promise<T>` | 显式 JSON Schema 子集校验 | 同上 |
| `log` | `log(message: string): void` | 追加人读日志；内存环 500 条；入 journal | 不重放（仅展示） |
| `files.glob` | `glob(pattern: string): Promise<string[]>` | `project_root` 相对；≤2000 条；排序 | 入 journal，命中即重放 |
| `files.read` | `read(path: string): Promise<string>` | UTF-8；≤2MB；越界/非文件报错 | 同上 |
| `files.grep` | `grep(pattern: string, glob?: string): Promise<GrepMatch[]>` | ≤2000 条/256KB | 同上 |
| `git.status` | `status(): Promise<GitStatus>` | 只读 | 入 journal |
| `git.diff` | `diff(path?: string): Promise<string>` | 只读，≤1MB | 入 journal |
| `world.run` | `run(command: string, opts?: {cwd?: string; idempotency_key?: string}): Promise<{exit_code,stdout,stderr}>` | 仅 `Unrestricted`；**无逐命令审批**；cwd 限 `project_root`；超时 300s；输出截断 1MB；进程树随取消终止 | `completed` 跳过；`failed` 需裁决；`started/unknown` 阻止启动 |
| `phase` | `phase(name: string): void` | 顶层独立语句；非空字面量；同名合并 | 投影 |
| `report` | `report(item: unknown): void` | P4 无 artifactId；JSON 可序列化；≤1MiB/条、65536 条/run、总 64MiB/run | 入 journal + results |
| `result` | `result(text: string): void` | 最终结果；与 report 共存时以 result 为最终值 | 入 journal |
| `artifact.file` | `file(id, path, opts?: ArtifactFileOptions): Promise<ArtifactRef>` | `project_root` 内文件；≤20MiB；id ≤64 字符 | 发布即持久化 |
| `artifact.markdown` | `markdown(id, content, opts?: ArtifactOptions): Promise<ArtifactRef>` | ≤256KB；同 id 新版本 | 同上 |
| `hole` | `hole<T>(name, prompt?): Promise<T>` | 仅 tail hole；P4 | 等待/填充事件 |

**phase 生命周期**：遇到下一个 `phase`、脚本结束、run 终态或取消时关闭当前 phase；
同名 phase 的多个区间合并（总耗时相加，状态取优先级 `failed > aborted > completed > running > pending`）；
未闭合的 phase 在 run 终态时按终态标 `completed`/`failed`/`aborted`。

`world.run` 的 `idempotency_key` 按 `(run_id, key)` 唯一定位；复用前核对 `op`/`args_hash`，
不同 → `effect_key_conflict`。key 不能证明外部命令无副作用，`unknown` 状态仍必须经 §5.14 裁决。

模型优先级：`agent(name,{model})` > `subagent_model` > 会话模型；三者都必须落在
`execution_config.allowed_models` 内，否则 `model_not_allowed`。

**report/artifact 重放去重**：`report` 以 `(call_index, seq)` 为稳定键（不含代次），
同一次 Resume/Fill 的前缀重放不重复追加；Amend 视为新脚本，清空去重表并允许重新报告。
`artifact` 以 `(id, version)` 幂等，同代重放不新增版本。
验收：续跑/补洞后 report 条数与 artifact 版本不重复增加；Amend 后可有意新增。

顶层 `return` 与 `result`：`result` 优先；无 `result` 时用 IIFE 返回值（字符串原样，其余 JSON 字符串化；JSON 化不了的退回 `String()`）。`report` 只进 results，不改变最终值。
`artifact.chart/table/metrics/board` 不在 P4（P5）。

---

## 6. 分阶段实施

### 6.0 分阶段契约

| 能力 | P0（同步） | P1+（后台） |
|---|---|---|
| `CreateWorkflow` 返回 | `{status:"completed"|"errored", run_id, result, asks_total, asks_replayed}` | `{status:"backgrounded", run_id, ...}` |
| `AmendWorkflow` 返回 | `{status:"completed"|"errored", run_id, superseded_run_id}` | `{status:"backgrounded"|"retuned", ...}` |
| `ResumeWorkflowRun` 返回 | `{status:"completed"|"errored", run_id, replayed_asks}` | `{status:"backgrounded", ...}` |
| 恢复资格 | journal 存在 + 调用方提供原 `script` 且 hash 一致；用户明确要求 | `stopped` 且 `superseded_by IS NULL` |
| 幂等 | 不支持 `request_id` | §5.0 |
| 快照/配置 | 无（沿用内联脚本） | 不可变快照 + execution_config |
| 状态机 | 只用 completed/errored | 全状态（queued/running/stopping/stopped） |
| 持久化 | 无新表；journal 照旧 | 八表（P1 五 + P3 一 + P4 二）+ 快照 + 配置 |
| 取消 | 父 turn 取消 | CancelWorkflowRun |

P0 不实现 §5.0 幂等、§5.3 的 stopped 前置、快照字段与 `exec_generation`；这些从 P1 生效。
P0 也不支持 retune（运行中调参）：`AmendWorkflow` 只对已完成/失败 run 重建，`retuned` 从 P1 起。
P0 的 Resume/Amend 也要求"用户取消后需新授权"，但只做**进程内** journal 标记与校验，
跨重启不保留；持久化 `stop_origin/requires_fresh_auth/auth_token` 是 P1 任务。

P0 的测试只断言同步契约；P1 的测试断言后台契约。**不要求 P0 输出 P1 字段**。

### 6.1 P0：入口、改名、拆工具（同步，3-4 天）

| 任务 | 文件 | 验收 |
|---|---|---|
| `CreateWorkflow` 改名 + 触发约束 | `workflow.rs:1834-1862` | 新名可调用；`WorkflowRun` 注册为 deprecated 别名，接受旧 `resume_run_id`/`amend_run_id` |
| `AmendWorkflow`/`ResumeWorkflowRun` 同步薄包装 | 新 `amend_workflow.rs`/`resume_workflow_run.rs` | 与旧参数行为一致；输出 §6.0 P0 契约 |
| `/workflow` 展开 + `display_content`/`model_content` | `turn.rs:978`、`rclaw-core/src/bus.rs`、`turn/history.rs`、`turn/run.rs`、`slash.rs` | §4.1；历史显示原文 |
| 技能 v1（只写 P0 工具） | `skills/dynamic-workflows/SKILL.md` | `skills_list` 可见；read 后可提交 |
| 技能门 hash 版 | `turn/tool_execution.rs` + `workflow.rs` | 未读/旧 hash 拒绝；读后放行 |
| 状态表更新 | `docs/zcode-port-longhorizon.md` | — |

**退出标准**：`/workflow` 可用；历史显示原文；改名与拆工具同阶段，无能力断档；
旧 `WorkflowRun` 调用仍可用；P0 Amend 不暴露 retune（只支持已完成/失败 run 重建）；
现有 5209 行引擎测试全绿；P0 契约测试通过。

### 6.2 P1：后台化、持久化、内省、取消（10-14 天）

| 任务 | 文件 |
|---|---|
| v3 一次建 8 张表（P1 只用 5 张）+ 迁移 + 一致性备份 + 投影折叠 + 增量回填 | `db.rs`、新 `workflow_store.rs` |
| 不可变快照 + 双代次 + execution_config | `workflow.rs`、新 `workflow_snapshot.rs` |
| `WorkflowRunService`：状态/配额（含 replace）/幂等/租约 | 新 `workflow_service.rs` |
| `WorkflowDriver`：后台、令牌表、心跳、effect、outbox | 新 `workflow_driver.rs` |
| **后台取消令牌解耦**：run 自有 token，移除 `turn_cancel()` 继承与 `drop_guard` | `workflow.rs`、`workflow_driver.rs` |
| **retune 原子**：`EngineShared` 持 `AtomicUsize` + 模型 RwLock（模式 A） | `workflow.rs` |
| **WorkflowPaths**：`Layout.base` + `rclaw-store::workflow_home()`，覆盖 Windows/XDG/Android | 新 `workflow_paths.rs`、`rclaw-store` |
| **qid parser**：`dwfq-<run_id>-<exec_generation>-<seq>` 分支 | `escalate.rs` |
| **`MessageSource::Workflow`**（D2） | `rclaw-core/src/bus.rs`、`background_wake.rs` |
| Create/Amend/Resume 后台化 + queued | 对应工具文件 |
| `workflow_background` + `workflow_legacy_tool` 配置与模式矩阵（§13.8） | `options.rs`、`bootstrap.rs` |
| List/Get/Cancel | 新工具文件 |
| amend 写边界（显式上下文 + 写前原子关闭） | `turn/tool_execution.rs`、`workflow_driver.rs` |
| 单实例锁 + 租约回收 + 启动扫描 | `bootstrap.rs`、`workflow_service.rs` |
| outbox dispatcher + notification 状态机去重 + consumed_at 确认 | `background_wake.rs`、`workflow_driver.rs` |
| `workflow_confirm_effects` UI 命令（未知 effect 逐项裁决） | `desktop/src/commands.rs`、`WorkflowCard.tsx` |
| 快照启动校验 + 孤儿清理 | `workflow_snapshot.rs`、`bootstrap.rs` |
| 桌面 `workflow_status`/`workflow_cancel` + 最小卡片 | `commands.rs`、`WorkflowCard.tsx`、`api.ts` |
| 别名改为 `workflow_legacy_tool` 开关（默认 false） | `bootstrap.rs`、`workflow.rs` |

**退出标准**：Create 立即返回且后台 run 在父 turn 取消后存活；通知至少一次且去重；
List/Get/Cancel 可用；Resume/Amend 按 §5.2/§5.3；`project_root=Layout.base`（含 Android）可写；
v2→v3 迁移 + 降级演练（§9）通过；单实例锁行为符合 §3.6。

### 6.3 P2：存档与参数（4-6 天）

SaveWorkflow/ListSavedWorkflows、元数据块、saved 来源、args 校验、args facade、subagent_model。
**退出**：saved 可重跑；§5.8 参数规则逐条测试；模型选择生效；global scope 仅 `Unrestricted`。

### 6.4 P3：片段与世界读（8-12 天）

EvalWorkflowSnippet、log/args、files/git、world.run（仅 `Unrestricted`；无逐命令审批）、
world 读 journal 重放、log_tail。

| 任务 | 文件 | 说明 |
|---|---|---|
| `files.glob` | 新 helper（`globset`/`walkdir`） | 现有 `files.rs` 只有 read/list/grep，glob 是新实现 |
| `git.status/diff` | 新 helper（`git status --porcelain` 或 `git2`） | 现有无 git 结构 |
| Windows Job Object | `workflow_driver.rs` | `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`；Unix 进程组已有 |
| `workflow_process` 表 + 进程树确认 | `db.rs`、`workflow_store.rs`、`workflow_driver.rs` | P3 启用（v3 已建） |
| `workflow_confirm_process_gone` UI | `desktop/src/commands.rs`、`WorkflowCard.tsx` | 逐项确认残留命令树 |

**退出**：片段可读真实文件；resume 零副作用重放 world 读；`Sandboxed` 的 world 被拒；
Job Object 在宿主退出后确实回收命令树。

### 6.5 P4：留白与最小展示面（10-14 天）

tail hole + FillWorkflowHole、启用 `workflow_hole`/`workflow_question`（v3 已建）、phase、report/result、
artifact.file/markdown、卡片产物区、Get 的 phases/reports/artifacts 投影。
**不在 P4**：artifact 图表/看板、agent persona（P5）。
**退出**：tail hole 闭环；两并发填充只成功一个；切代崩溃可回退 waiting；file/markdown 可打开。

---

## 7. 文件级改动清单

| 文件 | 阶段 | 改动 |
|---|---|---|
| `crates/rclaw-runtime/src/agent_tools/workflow.rs` | P0-P4 | 改名、后台化、prelude、world/hole |
| `crates/rclaw-runtime/src/agent_tools/workflow_ast.rs` | P4 | tail hole 校验、phase 分析 |
| `crates/rclaw-runtime/src/agent_tools/workflow_service.rs` | P1 | 新：状态/配额/幂等/租约 |
| `crates/rclaw-runtime/src/agent_tools/workflow_driver.rs` | P1 | 新：后台/effect/outbox |
| `crates/rclaw-runtime/src/agent_tools/workflow_paths.rs` | P1 | 新：`Layout.base` + `rclaw-store::workflow_home()` |
| `crates/rclaw-store/src/workflow_home.rs` | P1 | 新：`RCLAW_HOME`/Windows/XDG/Android 解析 |
| `crates/rclaw-runtime/src/agent_tools/workflow_store.rs` | P1-P4 | 新：P1 实现五表；v3 建全 8，P3/P4 启用后三 |
| `crates/rclaw-runtime/src/agent_tools/escalate.rs` | P1 | `dwfq-` qid 分支 |
| `crates/rclaw-tools/src/files.rs` | P3 | `glob` helper（现有只有 read/list/grep） |
| `crates/rclaw-runtime/src/agent_tools/workflow_git.rs` | P3 | 新：git status/diff |
| `crates/rclaw-runtime/src/agent_tools/workflow_snapshot.rs` | P1 | 新：快照/双代次 |
| `crates/rclaw-runtime/src/agent_tools/amend_workflow.rs` | P0/P1 | 新 |
| `crates/rclaw-runtime/src/agent_tools/resume_workflow_run.rs` | P0/P1 | 新 |
| `crates/rclaw-runtime/src/agent_tools/list_workflow_runs.rs` | P1 | 新 |
| `crates/rclaw-runtime/src/agent_tools/get_workflow_run.rs` | P1/P4 | 新 |
| `crates/rclaw-runtime/src/agent_tools/cancel_workflow_run.rs` | P1 | 新 |
| `crates/rclaw-runtime/src/agent_tools/save_workflow.rs` | P2 | 新 |
| `crates/rclaw-runtime/src/agent_tools/list_saved_workflows.rs` | P2 | 新 |
| `crates/rclaw-runtime/src/agent_tools/eval_workflow_snippet.rs` | P3 | 新 |
| `crates/rclaw-runtime/src/agent_tools/fill_workflow_hole.rs` | P4 | 新 |
| `crates/rclaw-runtime/src/agent_tools.rs` | P0-P4 | 导出/注册 |
| `crates/rclaw-cli/src/bootstrap.rs` | P0-P4 | 注册、单实例锁、启动扫描 |
| `crates/rclaw-runtime/src/turn.rs` | P0 | `/workflow` 展开 |
| `crates/rclaw-runtime/src/turn/history.rs` | P0 | 历史落库/回放读 `display_content` |
| `crates/rclaw-runtime/src/turn/run.rs` | P0 | LLM 请求读 `model_content` |
| `crates/rclaw-runtime/src/slash.rs` | P0 | 帮助条目 |
| `crates/rclaw-runtime/src/turn/commands.rs` | P1 | `/cancel <run_id>`（可选） |
| `crates/rclaw-runtime/src/turn/tool_execution.rs` | P0/P1 | 技能门、写边界 |
| `crates/rclaw-runtime/src/background_wake.rs` | P1 | workflow 通知 |
| `crates/rclaw-core/src/bus.rs` | P0/P1 | `display_content`/`model_content`（P0）+ `MessageSource::Workflow`（P1） |
| `crates/rclaw-store/src/db.rs` | P1 | schema v3 + 备份 |
| `crates/rclaw-store/src/journal.rs` | P1 | 新事件常量 |
| `crates/rclaw-desktop/src/commands.rs` | P1/P4 | `workflow_status`/`workflow_cancel`/`workflow_detail` |
| `crates/rclaw-desktop/ui/src/components/WorkflowCard.tsx` | P1/P4 | 新 |
| `crates/rclaw-desktop/ui/src/components/Chat.tsx` | P1 | 挂卡片 |
| `crates/rclaw-desktop/ui/src/lib/api.ts` | P1 | 调用封装 |
| `skills/dynamic-workflows/SKILL.md` | P0-P4 | 新，分阶段 |
| `.gitignore` | P1 | `<base_dir>/workflow-drafts/`（默认 `agent/workflow-drafts/`） |
| `docs/workflow-rollback-drill.md` | P1 | 降级演练记录 |

---

## 8. 测试计划

### 8.1 单元

- 状态迁移表（§3.3）逐行；`queued`/`stopping`/终态 supersede 的边界；非法迁移。
- 投影折叠：重复扫描/乱序到达/游标中断；`ON CONFLICT` 更新。
- 回填：缺字段 → legacy、`cwd=''`、不伪造归属；`include_legacy` 过滤。
- 快照：三种来源统一复制；运行中改源不影响 hash；resume 用快照。
- 双代次：create/amend/hole 的 script/exec generation；qid 失效。
- effect：生成规则、completed 跳过、failed 重跑、started/unknown 阻止 resume。
- 缓存关闭：写前关闭、并发 ask、部分写失败、子代理写。
- 配额：acquire/replace/释放；满额 amend；并发替换。
- 幂等：同 request_id 同/不同 digest、in_progress、failed 重试。
- Save/args：默认/必填/类型/未知键、required+default 冲突。
- 技能门：hash 变化失效、版本升级、项目切换、压缩后仍有效。
- 片段：无 agent、超时、world 权限（`Sandboxed` 拒绝 / `Unrestricted` 执行）。
- effect 跨代：fill（replay_prefix=1）跳过前缀 completed；amend（replay_prefix=0）全部重跑。
- 快照提交：临时文件→rename→DB 指针三个崩溃点的启动行为；孤儿清理。
- 配额：claim/replace/queued 取消只撤 claim/前驱退出交接/满额 amend。
- 墙钟：多次 resume 的 `elapsed_runtime_ms` 累计与总上限。
- 幂等：in_progress + run_id 存在/不存在两条恢复路径。
- `/workflow`：历史/UI 存原文，LLM 读展开文本（`display_content`/`model_content`）。
- 后台 token：工具返回后引擎继续跑；父 turn `/cancel` 不取消后台 run；`CancelWorkflowRun` 才取消。
- 项目根：`project_root = Layout.base`；两个会话共享同一 key，归属按 session_key。
- world：`Sandboxed` 拒绝；`Unrestricted` 执行（无逐命令审批）。
- qid：`dwfq-` 与现有 `<plan>:<step>:q-...` 互不误解析。
- 全局路径：`workflow_home()` 在 `RCLAW_HOME`、Windows、XDG、Android 四种环境下都指向可写目录。

### 8.2 集成

- Create 立即返回；notification 状态机（pending→scheduled→consumed）至少一次 + 去重；崩溃后重发。
- 事务中断：事件/投影/outbox 原子。
- Cancel：确认退出才 stopped；宽限超时 stopping 且 resume 拒绝。
- Resume：零调用重放；未知 effect 拒绝；UI 逐项确认路径。
- Amend：queued 等前驱退出；前驱崩溃/超时；满额替换；重复 amend 唯一后继。
- 竞争：amend/cancel/complete/resume 并发矩阵。
- 权限：跨项目/降权/跨会话逐工具断言。
- 双会话同 `project_root`：A 的 run 在 B 的 List 可见，`owned_by_this_session` 区分；
  写工具按 §3.7 的会话规则拒绝/允许。
- Android：`base_dir` 为 app-private；`workflow_home`/`workflow-drafts` 可写；无 `%USERPROFILE%` 依赖。
- 迁移：v2→v3、备份可打开、降级恢复；WAL/SHM 处理。
- Fill：两并发填充只一个成功；切代崩溃回退 waiting；在途 ask 结算；filling 期间 Cancel 优先、Amend 拒绝。
- 满额 amend：claim 交接不超并发；queued 取消不释放前驱许可；前驱退出后交接/释放二选一。
- 回填：游标中断、重复扫描、回填与实时投影并发（last_event_id 不倒退）。
- 权限：运行中降权/撤权后 Resume/Amend/Fill 被拒。
- 通知：`scheduled` 后崩溃、`consumed` 前崩溃、重复注入三种恢复路径只产生一条会话消息。
- 降级演练：断言工作流之外的数据损失范围与文档一致。

### 8.3 回归

- 每阶段退出前必跑：`cargo test -p rclaw-runtime --lib workflow`、`cargo test -p rclaw-store`、
  `cargo test -p rclaw-tools`，以及该阶段新增的集成测试；P1 另跑迁移/降级故障注入脚本。
- 性能指标在桌面 i7/16GB、空载环境下取 P50：Create 校验+落盘+预留 ≤1s；列表 ≤200ms；详情 ≤300ms。
- 现有 5209 行引擎测试全绿；`workflow_legacy_tool=false` 时无 `WorkflowRun` 残留。
- `/help` 快照、工具 schema 快照。

### 8.4 端到端

- CLI/桌面/微信各一次 `/workflow` 全流程；桌面卡片与 `workflow_detail` 一致；
- Sandboxed 会话 world 被拒；
- 杀进程重启：running/stopping→stopped(interrupted)，未知 effect 不可 resume；
- 降级演练：迁移→建 run→停进程→恢复备份→旧版启动。

---

## 9. 风险与回滚

| 风险 | 影响 | 缓解 |
|---|---|---|
| 后台化 | 依赖同步结果的调用方失效 | `workflow_background=false` 只改调度；返回契约按 §6.0 同步模式，持久化仍走 v3；已有后台 run 继续跑；文档标 breaking |
| DB v3 | 旧库打不开 | 停库 + `wal_checkpoint(TRUNCATE)` + `VACUUM INTO` 备份 + `integrity_check`；失败拒绝启动 |
| 通知 | 漏/重 | outbox（notification 单表）先持久化后发送；至少一次 + 去重；24h 重试后 dead 可见 |
| world.run | 越权/写入 | `Sandboxed` 拒绝；`Unrestricted` 与 `shell` 同权；无逐命令审批（D10） |
| hole | 重复副作用 | effect 表 + 双代次；started/unknown 阻止 resume；只做 tail hole |
| amend 缓存 | 陈旧答案 | 写前原子关闭全部缓存；显式传 run_id；effect 同步 |
| token | 成本 | 并发硬顶 16；墙钟硬顶 24h；列表 24k / 详情 48k |
| 技能门 | 误伤 | hash+版本，拒绝给下一步；saved 豁免 |
| 配额 | 泄漏 | driver 持有 + replace 交接 + 租约回收 |
| 多实例 | 双 driver | 文件锁 + 只读降级；不支持多实例（D9） |
| 项目根/路径 | 误用 `.rclaw`/仓库根，Android 或自定义 `base_dir` 失败 | 统一 `Layout.base` + `workflow_home()`（§3.0）；不硬编码 |
| 后台 token | `CreateWorkflow` 提前返回触发 `drop_guard` 取消引擎 | run 自有 token，移除 `turn_cancel` 继承（§3.6） |

**回滚方案（必须演练）**：
1. **调度回滚**：`workflow_background=false` → 同步契约；v3 表继续写；已有后台 run 继续。
2. **工具回滚**：P1 二进制保留 `workflow_legacy_tool` 开关（默认 false）；开启即注册 `WorkflowRun`
   别名并接受旧参数，同时继续读写 v3 表。因此工具面回滚不需要降级数据库；P0 的别名与 P1 开关是
   同一适配层。工具面回滚与全库降级分开演练。
3. **数据库回滚**：旧二进制拒绝 `user_version>支持版本`。步骤：停进程 → 备份当前 v3 库 →
   恢复 `state.sqlite.bak-v2` → 删除 `-wal`/`-shm` → 装回旧版。
   **明确数据损失**：state.sqlite 是单库，恢复 v2 备份会丢失迁移后**所有**新数据
   （会话、消息、checkpoint、usage、journal、kv，不只是 workflow）；迁移前必须告知用户并让其确认。
4. **降级演练**：P1 退出前实测并记录 `docs/workflow-rollback-drill.md`，含"工作流之外的数据影响"断言。

---

## 10. 验收清单

- [ ] `/workflow <任务>` 全通道展开并执行；历史/UI 显示原始命令，模型读展开文本；
- [ ] 11 个正式工具按阶段注册；`WorkflowRun` 默认由开关关闭；
- [ ] 工具描述含触发约束；技能门按 hash/版本生效；
- [ ] Create 的校验+落盘+预留段 ≤1s（不含模型调用），完成有通知；
- [ ] 后台 run 在 Create 返回后继续；父 turn `/cancel` 不杀它；`CancelWorkflowRun` 才停；
- [ ] `project_root = Layout.base`；同进程会话共享；Android 下 `workflow_home`/草稿可写；
- [ ] 路由矩阵逐行有测试（11 工具 × 允许/拒绝状态）；
- [ ] List 按项目 + legacy 过滤；Get 覆盖 queued/running/stopping/completed/errored/stopped；
- [ ] Amend：queued 等前驱退出、满额 replace、唯一后继、失败恢复路径；
- [ ] Resume：仅 stopped、未知 effect 拒绝、UI 确认路径；
- [ ] Cancel：确认退出才 stopped、超时 stopping；
- [ ] Save/args 规则、Eval world 仅 `Unrestricted`、Fill 双代次/并发；
- [ ] `world.run`：`Sandboxed` 拒绝、`Unrestricted` 执行；无逐命令审批（与 `shell` 一致）；
- [ ] P0 不暴露 retune；P1 起运行中调参（模式 A）生效；
- [ ] Resolve 重复回答/代次失效；
- [ ] 桌面卡片/详情一致；
- [ ] v2→v3 迁移、备份可打开、增量回填、降级演练（含非 workflow 数据损失说明）；
- [ ] 现有引擎测试全绿。

---

## 11. 决策记录

状态：`proposed` 待确认；`accepted-by-plan` 本方案默认。各决策在其「最晚阶段」开工前落定：
D5/D6/D15 最迟 P0；D1/D2/D7/D8/D9/D11/D12/D13/D14 最迟 P1；D3 最迟 P2；D10 最迟 P3；D4 最迟 P4。

| # | 决策 | 推荐 | 备选 | 状态 | 最晚阶段 | 阻塞任务 |
|---|---|---|---|---|---|---|
| D1 | `WorkflowRun` 旧名 | P0 别名；P1 保留 `workflow_legacy_tool` 开关（默认 false），不随数据库降级 | 直接移除 | proposed | P1 | 工具回滚 |
| D2 | 通知来源 | 新增 `MessageSource::Workflow` | 复用 Heartbeat 前缀 | proposed | P1 | notification dispatcher |
| D3 | 存档目录 | `Layout.base/workflows/`（默认 `agent/workflows/`）+ `workflow_home()/workflows/` | 新增 `agent.project_dir` 配置 | accepted-by-plan | P2 | SaveWorkflow |
| D4 | hole 范围 | 只做 tail hole | 分支级（+50%） | accepted-by-plan | P4 | FillWorkflowHole |
| D5 | 技能门 | hash+版本硬门 | 只内嵌描述 | proposed | P0 | CreateWorkflow |
| D6 | 并发参数名 | 主名 `max_parallel_asks`，`max_concurrency` 别名 | 改名对齐 | proposed | P0 | Create/Amend schema |
| D7 | `/cancel` 与 run | 解耦；停 run 用 CancelWorkflowRun | 连带停本会话所有 run | proposed | P1 | Cancel 语义 |
| D8 | 配额 | 新增 `max_active_workflow_runs` | 复用子代理上限 | proposed | P1 | capacity |
| D9 | 多实例 | 单实例锁；多实例只读 | 跨实例租约+容量表 | accepted-by-plan | P1 | driver 启动 |
| D10 | world 执行 | `ToolAccess`（`Sandboxed` 拒绝，`Unrestricted` 执行；无逐命令审批） | 新增 command confirm hook（P5） | accepted-by-plan | P3 | world.run/Eval |
| D11 | 回滚范围 | 接受恢复 v2 备份丢失迁移后全部数据 | 写 v3→v2 导出工具 | proposed | P1 | 迁移 |
| D12 | 未知 effect 放行 | 仅 UI 逐项裁决（§5.14），模型无入口 | 模型可 `allow_unknown_effects` | accepted-by-plan | P1 | Resume/Fill |
| D13 | 工具面回滚 | `workflow_legacy_tool` 开关，不降级库 | 装回 P0 版本 | accepted-by-plan | P1 | 回滚演练 |
| D14 | 项目根 | `project_root = Layout.base`（默认 `agent/`），同进程会话共享 | 新增 `agent.project_dir` 并改文件/命令沙箱根 | accepted-by-plan | P1 | 存储/List/路径 |
| D15 | `/workflow` 历史 | `BusMessage` 加 `display_content`/`model_content` | 构造 LLM 请求时展开，历史不改 | accepted-by-plan | P0 | 历史/UI |

---

## 12. 与 ZCode 的已知不可对齐项（本方案默认取舍）

以下为本方案默认选择；用户可在 §11 推翻对应决策。未列入 §11 的条目视为 accepted-by-plan。

1. **确认窗**：RClaw 沿用工具权限系统，不新增独立确认窗。
2. **TypeScript 全量类型系统**：只做 facade.d.ts 可表达的语法/类型检查，不覆盖 checker 级别名/泛型实例化/递归 `$defs`。
3. **agent persona**：P2 只加 model；`system` persona 列入 P5。
4. **artifact 图表/看板**：P4 只做 file/markdown；chart/table/metrics/board 列入 P5。
5. **跨项目 hub**：以 `project_root = Layout.base` 为项目键，不做全局 hub 页面。
6. **recoveryActions 多动作**：以单条 `recovery` 替代。
7. **分支级 hole**：只做 tail hole。
8. **on-demand 工具面**：工具 schema 常驻。
9. **多实例**：不支持共享同一 agent 目录的并发 driver。
10. **Amend 的 write effect**：不导入前驱的 `world.run`/`artifact`/ask 内写 effect；v1 对存在任何
    write effect 的 run 一律拒绝 Amend（`amend_blocked_direct_effect`），P5 再开放映射与逐项确认。
11. **`world.run` 逐命令审批**：RClaw 不提供；仅按 `ToolAccess`（`Sandboxed` 拒绝、`Unrestricted` 执行）。
    需要逐命令审批时另做 `set_command_confirm_hook` + UI（P5）。
12. **项目根**：使用 `Layout.base`（默认 `agent/`），不是 git 仓库根；ZCode 的 `.zcode/` 项目模型不适用。

---

## 13. 恢复、并发与错误矩阵（规范性附录）

### 13.1 启动恢复顺序

1. 取 `agent/workflow.lock`；失败 → 服务只读（List/Get），写工具 `instance_conflict`。
2. 快照校验：`running/queued` 的 `snapshot_path` 必须存在且 hash 匹配；否则 `errored(snapshot_missing)`，
   不进入驱动。
3. `workflow_hole.state=filling`：
   - DB 未出现新 `script_generation` 且 run 未被判定中断 → 回退 `waiting`；
   - 已出现新版本但无 driver → `errored`，不自动重试；
   - 若同一次扫描把 run 判为 `stopped(interrupted)` → hole 一律 `aborted`，Resume 后重新打开（§3.5）。
4. 取得锁后，旧实例已退出——但**进程树未必退出**：
   - **P3 起**先查 `workflow_process`；存在 `running/unknown` 且 OS 探测无法证明退出 → run 保持
     `stopping(interrupted)`，不写 `stopped`、不释放配额，等待 `workflow_confirm_process_gone`（§5.15）；
     P1/P2 尚无 `world.run`，不需要该表，只检查引擎/子代理退出；
   - 确认无残留进程树后，才迁移：`running` → `stopped(interrupted)`（effect 检查后决定 `resumable`）；
     `stopping` → `stopped(interrupted)`（保留 `superseded_by`/`stop_origin`）；
     `queued` → 按 §13.2。
5. 本实例租约停滞（>120s）且 driver 仍存活 → 先 `stopping(interrupted)` 撤销执行，确认退出后 `stopped`；
   driver 已不存在也必须核验 `workflow_process` 中全部命令进程树已退出：
   已确认 → `stopped(interrupted)`；未确认 → 保持 `stopping`、**继续持有许可**，等待
   `workflow_confirm_process_gone`（§5.15）。
6. 清理等待对象（§13.3）、孤儿快照与 dead 通知。

### 13.2 queued 后继恢复矩阵

所有启动（正常或恢复）都先经过 §5.2 启动闸门：整条祖先链已退出、effect 已裁决、权限/预算/许可齐备，
才允许 `queued → running`。

| 前驱/后继状态 | 动作 |
|---|---|
| 前驱 `stopped/completed/errored`，`superseded_by` 已指向后继 | 启动后继（`running`） |
| 前驱 `running/stopping` | 保持 queued；前驱确认退出后再启动 |
| 前驱 owner 已死/租约接管 | 前驱转 `stopped(interrupted)`，再启动后继 |
| 后继 `superseded_by` 非空或已取消 | `stopped(user)`，撤销 claim |
| 后继快照缺失/hash 不符 | `errored`，撤销 claim |
| request 行 `in_progress` 且 `run_id` 存在 | 返回该 run 当前状态，不重复创建 |
| request 行 `in_progress` 且无 `run_id` | 标 `failed`，允许重试 |

### 13.3 等待对象清理

run 进入 `completed/errored/stopped` 或 `superseded` 时，在同一事务：
- `workflow_question(state=pending)` → `aborted`，写 `question_aborted`；旧 qid 不可再答；
- `workflow_hole(state=waiting/filling)` → `aborted`，写 `hole_aborted`；Fill 返回 `hole_not_waiting`；
- 未完成的 `world.run` → effect `unknown`（进程树未确认则保持 `stopping`，见 §13.1），不写 `completed`；
- `workflow_notification(state=pending)` 只保留终态一条，进度通知标 `dead`。
**exec_generation 切换（resume/hole_fill）时**同样把旧代 `pending` 问题置 `aborted`；
`ResolveWorkflowQuestion` 必须校验 qid 的 `exec_generation` 等于当前值，否则 `run_not_in_flight`。
旧通知携带的 qid/hole_id 在清理后一律 `invalid_state`。

### 13.4 effect 状态与裁决语义

| 状态 | 含义 | 重启前要求 |
|---|---|---|
| `completed` | 进程已退出且退出码已知；不保证"无写入" | 同代/前缀可跳过 |
| `failed` | 进程已退出，可能已部分写入 | 逐项裁决（skip/rerun） |
| `unknown` | 超时/断连/取消/宿主崩溃/租约接管，无法证明无写入 | 逐项裁决 |
| `started` | 已开始、未记录终态 | 逐项裁决 |

**统一准入谓词**：`effect_blocked(exec)` = 存在 `failed/started/unknown` 且没有满足条件的裁决
（`skip` + `substitute_result`，或未消费的 `rerun`）。`resumable` 投影、Resume、Fill、Amend 后继、
`queued → running` 全部复用同一谓词，不得各自实现。

- `skip` 必须提供 `substitute_result`（JSON）作为返回值；未提供 → `effect_result_required`，
  不允许 skip（不做静态分析豁免）。
- `rerun` 绑定 `(run_id, effect_key, exec_generation)`，在 effect 重新进入 `started` 时原子消费并清除；
  新一轮只能再次由 UI 授权。
- `failed/unknown/started` 都阻断 Resume、Fill、Amend 后继启动；只有 §5.14 的逐项裁决能放行。
- ask 内的写工具也记 effect：
  `effect_key = fnv1a64("ask:" + actor + ":" + input_hash + ":" + ask_seq + ":" + call_site + ":" +
  invocation_index + ":" + op + ":" + args_hash)`。`ask_seq` 是该 `(actor,input_hash)` 在本 run 内的第几次 ask，
  `call_site/invocation_index` 区分同一 ask 内的多次写。
  复用时必须核对 `op`/`args_hash`；不匹配 → `effect_key_conflict`，阻断重放。
  若 ask 的写已完成但 answer 未落库，该 effect 为 `unknown`，阻断重放，不允许靠重跑 ask 覆盖。

### 13.5 权限矩阵

| ToolAccess | 读工具（List/Get） | 写工具（Amend/Resume/Cancel/Fill/Resolve） | `world.run` / Eval world |
|---|---|---|---|
| `Sandboxed` | 允许（本 `project_root`，逐行过滤） | `session_key` 匹配即可（cli/desktop 或同会话）；否则拒绝 | **拒绝** |
| `Unrestricted` | 允许 | 允许（仍需项目/会话校验） | 允许（`ToolAccess`；无逐命令审批） |

`SaveWorkflow`：project scope 允许同会话 `Sandboxed`；global scope 仅 `Unrestricted`。
`ListSavedWorkflows`：project scope 允许同会话；global scope 仅 `Unrestricted`（避免跨项目泄露）。
`CreateWorkflow` 的 `saved.scope=global` 解析/复制**同权**：仅 `Unrestricted`；否则 `forbidden_global`。
`SaveWorkflow` 覆盖已有文件仍需用户同意 + `overwrite=true`（§5.8）。

权限检查**先于**审批；审批不能把 `Sandboxed` 提升为 `Unrestricted`。
`legacy` 行（`cwd=''`）：仅 `Unrestricted` + `include_legacy=true` 可读；写工具一律拒绝。

### 13.6 并发参数归一化

| 输入 | 归一化 |
|---|---|
| Create 均省略 | `max_parallel_asks=8` |
| Amend 均省略 | 保持前驱 `execution_config.max_parallel_asks` |
| 只给 `max_parallel_asks` | 1-16，越界截断/报错（报错优先） |
| 只给 `max_concurrency` | 等价于 `max_parallel_asks` |
| 两者都给且相等 | 接受 |
| 两者都给且不等 | `alias_conflict` |
| Amend 给 `null` | 清除运行级设置，回到默认 8；全局硬顶 16 不变 |
| 持久化 | 只存归一化后的 `max_parallel_asks`，不存别名 |

Create/Amend 共用该表；`execution_config.max_parallel_asks` 只存归一化值。

### 13.7 错误码归一化

| 场景 | 规范码 | 兼容别名（接受但不新用） |
|---|---|---|
| 参数取值非法 | `args_invalid` | `workflow_args_invalid` |
| 参数声明非法 | `args_decl_invalid` | — |
| 来源冲突 | `source_conflict` | `workflow_source_conflict` |
| 脚本诊断 | `script_diagnostics` | `workflow_script_diagnostics` |
| 容量满 | `capacity_exceeded` | `workflow_capacity_exceeded` |
| 技能未读/过期 | `workflow_skill_required` | — |
| 项目/会话/权限 | `forbidden_project` / `forbidden_session` / `permission_downgraded` | — |
| world 权限 | `forbidden_world` | — |
| 全局存档权限 | `forbidden_global` | — |
| 状态非法 | `invalid_state` | — |
| 未找到 | `not_found`（对象类型在 `message`） | — |

各工具只列特有码；公共码见上表，不再重复。

### 13.8 `workflow_background` 开关矩阵

| 开关 | Create/Amend/Resume 返回 | 持久化 | 取消 | 通知 |
|---|---|---|---|---|
| `true`（默认） | `backgrounded` | v3 表（P1 五张）+ 快照 | CancelWorkflowRun / UI | notification outbox |
| `false` | `completed/errored`（同步） | 同上（仍写 v3） | 父 turn `/cancel` | 终态不唤醒（返回即结果） |
| `false` + Amend 仅调参（P1 起） | `retuned`（立即） | 同上 | 父 turn | 无 |
| `false` + 取消 | `stopping` → `stopped`（调用方等待） | 同上 | 父 turn/工具 | 无 |
| `false` + Amend 后继 | 同一调用内等待前驱退出后再跑，不持久化 queued；前驱 10s 未退 → `stop_pending` | 同上 | 父 turn | 无 |
| `false` + hole | 提交前拒绝 `background_required` | 同上 | 父 turn | 无 |

同步模式下主 turn 被占住：`ResolveWorkflowQuestion` 仍可由另一个特权会话按 qid 回答（journal 持久）；
hole/补洞需要后台模式，同步模式返回 `background_required`。

- 开关只改调度方式，不改持久化与权限；已有 run 保持启动时的 `background_mode`（存入 `execution_config`）。
- 切换不中断已有 run；新 run 用新值。

### 13.9 path 来源的一次性快照

`path` 在预检开始时**一次性读入内存**（≤100k 字符 / 2MB），之后的 hash、语法/type_check、快照落盘
全部使用同一份字节；禁止再次读取源文件作为执行版本。源文件在检查后被改不影响本次运行；
`source_path` 仅展示。读入失败/超限 → `path_unreadable`/`script_too_large`。

**统一文件边界**：`path`、`files.*`、`artifact.*` 全部复用 `read` 工具的工作区沙箱，
根为 `Layout.base`（`project_root`，§3.0）：`canonicalize` 后必须位于其下；symlink/junction
指向区外一律拒绝；绝对路径同样受限；`world.run` 的 cwd 必须在 `project_root` 下。
**受管理根例外**：global 存档 `<workflow_home>/workflows/`、草稿/快照目录
`<project_root>/workflow-drafts/` 由服务端按管理路径访问，不受“用户 path 根”限制；
用户提供的 `path` 仍只允许 `project_root`。受管理根只接受服务端生成的路径，不接受模型拼接。

### 13.10 技能门的完整读取

只有 `read` 工具**完整**返回 `<Layout.skills_dir>/dynamic-workflows/SKILL.md`（`bytes_read == 文件大小`，未被截断）时，
才登记 `{content_sha256, contract_version, session_key, read_at}`。分页/范围读取不登记；
内容 hash 与当前文件不一致时旧登记失效。上下文压缩不影响登记。

### 13.11 本附录未覆盖的边界

以下明确不在本期，写入 §12 或 P5 backlog，不作为验收项：
- 分支级 hole 与 hole 内再次开 hole；
- chart/table/metrics/board 预设；
- 跨实例（多进程）并发 driver；
- `world.run` 的逐命令审批（当前只按 `ToolAccess`，无审批层）。

---

## 14. 本次代码贴合性修订对照（v7）

| 评估项 | 结论 | 落点 |
|---|---|---|
| 1. 后台 token 与 `drop_guard` 冲突 | 已改：run 自有 token，移除 `turn_cancel()` 继承与 `drop_guard` | §3.6、§6.2、§9 |
| 2. `world.run` 无审批链 | 已改为 `ToolAccess`（`Sandboxed` 拒绝 / `Unrestricted` 执行）；逐命令审批列 P5 | §3.7、§5.10、§5.13、§13.5、§12.11 |
| 3. 项目/工作区模型不匹配 | `project_root = Layout.base`；无 per-session cwd；同进程会话共享 | §3.0、§3.4、§3.7、§5.4 |
| 4. `/workflow` 历史污染 | `display_content` / `model_content` 分离 | §4.1、§6.1、§7、§8.1 |
| 5. P0 retune 不可行 | P0 只重建；retune 移到 P1 | §5.2、§6.0、§6.1、§13.8 |
| 6. 全局路径不可移植 | `workflow_home()` 走 `rclaw-store`，覆盖 Android | §3.0、§5.8、§13.9、D3 |
| 中 1. 九表过重 | P1 实现五表；v3 一次建全 8（后三张空置），P3/P4 启用 | §3.4、§6.2-§6.5 |
| 中 2. glob/git/Job Object 新实现 | 独立 P3 任务 | §6.4、§7 |
| 中 3. qid parser | `dwfq-` 分支，P1 | §5.7、§6.2 |
| 中 4. `MessageSource` | D2，P1 | §6.2、§11 |
| 中 5. journal/表权威 | journal 为事实源，表为投影 | §3.4 |

代码依据：`workflow.rs:2049-2052`（token/`drop_guard`）、`rclaw.toml.example:11` + `bootstrap.rs:38`
（`Layout.base`）、`turn.rs:141`（无 cwd 的 `TurnKey`）、`turn.rs:2497`（`CURRENT_CHAT` 覆盖工具执行）、
`shell.rs:268-282` + `access.rs:48`（只有路径确认）、`escalate.rs:34-36`（PlanRun qid）、
`journal.rs:189,208`（kv）、`files.rs:76,644`（Read/Grep 公开）。
