//! `dynamic-workflows` 技能文件与技能门。
//!
//! 技能门：只有 `read` 工具**完整**读取 `<project_root>/skills/dynamic-workflows/SKILL.md`
//! 时，才在会话 kv 记 `{content_sha256, contract_version, read_at}`；`CreateWorkflow`
//! 提交前要求当前文件 hash 与该记录一致（文件更新后旧记录自动失效）。
//! `saved` 来源豁免（存档本身已含作者契约）。

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use sha2::{Digest, Sha256};

use rclaw_store::journal::JournalStore;

use super::workflow_paths::normalize_project_key;

/// 当前技能契约版本（工具面变更时 +1，旧登记自动失效）。
pub const CONTRACT_VERSION: u32 = 5;

/// 技能文件正文（首次启动安装；已存在不覆盖）。
pub const SKILL_MD: &str = r#"---
name: dynamic-workflows
description: Design and run deterministic JS workflows that orchestrate sub-agents, read files, run read-only git, execute commands, publish artifacts, and pause at holes.
---

<!-- contract-version: 5 -->

# Dynamic workflows

A workflow is a **deterministic JS script** executed in an embedded QuickJS sandbox.
Use it when the user explicitly asks for a workflow ("use a workflow", "with a workflow",
"使用 workflow", "用工作流"). Do not substitute the `Agent` tool and do not do the work inline.

## Lifecycle

1. Read this file completely (the skill gate rejects scripts until you do).
2. Decide the subagent topology: how many subagents, which share a context, what each returns.
3. Write the script; call `CreateWorkflow`.
4. `CreateWorkflow` returns `{status:"backgrounded", run_id}`. Inspect with
   `GetWorkflowRun`; completion arrives as a notification.
5. If the script needs revision, call `AmendWorkflow` (never stop then rewrite).
6. `ResumeWorkflowRun` only works for `stopped` runs whose `superseded_by` is null.
7. `CancelWorkflowRun` stops a running/queued run.

## Tools

| Tool | Purpose |
|---|---|
| `CreateWorkflow` | Validate a script and start a run. Inputs: `name`, one of `script` / `path` / `saved`, `args`, `max_parallel_asks` (alias `max_concurrency`), `subagent_model`, `max_run_secs`, `type_check`, `request_id`. |
| `AmendWorkflow` | Revise a run. `script`/`path` for a new script; `max_parallel_asks`/`subagent_model` alone retunes a running run. |
| `ResumeWorkflowRun` | Resume a stopped run (same run id, new exec_generation). |
| `ListWorkflowRuns` | List runs in this project. |
| `GetWorkflowRun` | Snapshot: progress, log tail, reports, phases, artifacts, holes, pending questions, result/error, effects. |
| `CancelWorkflowRun` | Cancel a running/queued run. |
| `SaveWorkflow` | Save a script + metadata under `workflows/<name>.dwf.js`. Never call unsolicited. |
| `ListSavedWorkflows` | List saved workflows (project shadows global). |
| `EvalWorkflowSnippet` | Run a small snippet through the same sandbox (no agents/report/hole/artifact). |
| `FillWorkflowHole` | Fill a waiting hole with a function body. |
| `ResolveWorkflowQuestion` | Answer a `dwfq-...` question from an actor. |

## Script facade

```ts
args: Readonly<Record<string, unknown>>;

agent(name: string, persona?: {model?: string; system?: string}): {
  ask<T = string>(question: string): Promise<T>;
  askJson<T>(question: string, schema: object): Promise<T>;
};

log(message: string): void;
phase(name: string): void;
report(item: unknown): void;
result(text: string): void;

files.glob(pattern: string): Promise<string[]>;
files.read(path: string): Promise<string>;
files.grep(pattern: string, glob?: string): Promise<{path:string;line:number;text:string}[]>;

git.status(): Promise<{branch?:string;clean:boolean;staged:string[];modified:string[]}>;
git.diff(path?: string): Promise<string>;

world.run(command: string, opts?: {cwd?: string; idempotency_key?: string}):
  Promise<{exit_code:number;stdout:string;stderr:string}>;

artifact.file(id: string, path: string, opts?: {title?:string;description?:string;primary?:boolean;contentType?:string}): Promise<{id:string;version:number}>;
artifact.markdown(id: string, content: string, opts?: ArtifactOptions): Promise<ArtifactRef>;
artifact.table(id: string, rows: unknown, opts?: ArtifactOptions): Promise<ArtifactRef>;
artifact.chart(id: string, spec: {type?: "bar"|"line"; title?: string; data: Array<{label?:string;value:number}>}, opts?: ArtifactOptions): Promise<ArtifactRef>;
artifact.metrics(id: string, metrics: unknown, opts?: ArtifactOptions): Promise<ArtifactRef>;
artifact.board(id: string, columns: Array<{title:string;items:unknown[]}>, opts?: ArtifactOptions): Promise<ArtifactRef>;

hole<T = string>(name: string, prompt?: string): Promise<T>;
```

## Determinism

- `Date.now()`, zero-arg `new Date()`, `Math.random()` are pinned; `Date`/`Math` are frozen.
- `eval`, `Function`, `import`, `require`, `setTimeout`, `setInterval`, `process` are blocked.
- `agent.ask` is journaled by `(actor, question hash, occurrence)` and replays at zero cost.
- `files.*`, `git.*`, `world.run`, `artifact.*` are journaled as effects. A completed effect
  replays; a `failed`/`started`/`unknown` effect blocks resume until the UI confirms it.

## Rules

- `world.run` requires an unrestricted session; cwd must stay under the project root.
  There is no per-command approval by default (same policy as `shell`).
- `hole` pauses the run. The fill body replaces the `hole(...)` call expression and the run
  resumes with prefix asks/effects replayed; writes before the hole must be idempotent.
- `report` is the primary result channel; `result` sets the final value.
- `AmendWorkflow` does not import effects; a run with write effects needs UI confirmation.
"#;

/// 技能文件路径：`<project_root>/skills/dynamic-workflows/SKILL.md`。
pub fn skill_path(project_root: &Path) -> PathBuf {
    project_root
        .join("skills")
        .join("dynamic-workflows")
        .join("SKILL.md")
}

/// 首次启动安装技能文件（已存在不覆盖）。
pub fn ensure_skill_file(project_root: &Path) -> Result<(), String> {
    let path = skill_path(project_root);
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("创建技能目录 {} 失败: {error}", parent.display()))?;
    }
    std::fs::write(&path, SKILL_MD)
        .map_err(|error| format!("写入技能文件 {} 失败: {error}", path.display()))
}

/// 文件 sha256（缺失返回 None）。
pub fn file_hash(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(format!("{:x}", hasher.finalize()))
}

/// 安装完整读取观察者：命中技能文件时登记会话级 hash。
///
/// 会话键取当前 turn 的 `(channel, chat_id)`；后台 / 作用域外读取不登记（fail-safe）。
pub fn install_observer(journal: JournalStore, project_root: PathBuf) {
    let skill = skill_path(&project_root);
    // 两边都按 canonicalize 后的形态比较：`read` 传来的路径可能是相对 agent 根、绝对、
    // 大小写 / 分隔符不同的写法，字符串形态对不上会让技能门永远登记不上。
    let canonical_key = |path: &Path| {
        normalize_project_key(&std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    };
    let observer = Arc::new(move |path: &Path, _content: &str| {
        if canonical_key(path) != canonical_key(&skill) {
            return;
        }
        let Some((channel, chat_id)) = crate::turn::rewind::current_chat() else {
            return;
        };
        // 以文件字节为准（read 的 content 可能带 metadata 前缀 / 分页提示）。
        let Some(hash) = file_hash(path) else {
            return;
        };
        let key = format!("workflow-skill/{channel}/{chat_id}");
        if let Err(error) = journal.kv_set(
            &key,
            &json!({
                "content_sha256": hash,
                "contract_version": CONTRACT_VERSION,
                "session_key": format!("{channel}/{chat_id}"),
                "read_at": rclaw_store::db::now_ms(),
            }),
        ) {
            tracing::warn!("登记工作流技能读取失败: {error}");
        }
    });
    rclaw_tools::files::set_full_read_observer(observer);
}
