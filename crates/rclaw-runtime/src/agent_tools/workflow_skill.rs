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

/// 当前技能契约版本（工具面或使用契约变更时 +1，旧登记自动失效；带更旧版本头的
/// 已安装文件由 [`ensure_skill_file`] 用新正文覆盖）。
///
/// v6：「何时使用」改为按 `[agent] workflow_activation` 路由（规则正文在 `CreateWorkflow`
/// 工具描述里），技能文件不再写死「仅限用户明确要求」。
pub const CONTRACT_VERSION: u32 = 6;

/// 技能文件正文（首次启动安装；已安装文件仅在版本头更旧时覆盖，见 [`ensure_skill_file`]）。
pub const SKILL_MD: &str = r#"---
name: dynamic-workflows
description: Design and run deterministic JS workflows that orchestrate sub-agents, read files, run read-only git, execute commands, publish artifacts, and pause at holes.
---

<!-- contract-version: 6 -->

# Dynamic workflows

A workflow is a **deterministic JS script** executed in an embedded QuickJS sandbox.
When a workflow may be started is set by `[agent] workflow_activation`, and the
`CreateWorkflow` tool description states the rule in force — follow that description, not
this paragraph. The `/workflow` command is a valid trigger in every mode; whether a prose
request ("use a workflow", "用工作流") or your own initiative also counts depends on the mode.
Once a workflow is the chosen means, do not substitute the `Agent` tool and do not do the work
inline.

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

/// 安装技能文件。
///
/// - 不存在：写入 [`SKILL_MD`]。
/// - 已存在且带 `<!-- contract-version: N -->` 头、N 小于 [`CONTRACT_VERSION`]：是本程序旧版
///   生成的文件，用新正文覆盖（否则旧文件会继续向模型陈述已失效的使用契约）；覆盖前把旧文件
///   留作同目录隐藏备份 `.SKILL.md.v<N>.bak`（与 `rclaw_store::fs` / skill_manage 的
///   `.<名>.bak` 内部产物约定一致；技能扫描只认 `SKILL.md`，备份不会被当成技能），用户在
///   旧版上的批注不丢。备份失败则放弃刷新（宁可旧契约多活一阵，不丢用户文件）。
/// - 已存在但无版本头（用户自行改写）或版本不低于当前：不动。
///
/// 写入走原子替换：半截文件若已带新版本头，后续启动不会再修复它。
pub fn ensure_skill_file(project_root: &Path) -> Result<(), String> {
    let path = skill_path(project_root);
    if path.exists() {
        let installed_version = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| contract_version_of(&text));
        match installed_version {
            Some(version) if version < CONTRACT_VERSION => {
                let backup = stale_backup_path(&path, version);
                std::fs::copy(&path, &backup).map_err(|error| {
                    format!("备份旧技能文件到 {} 失败: {error}", backup.display())
                })?;
                tracing::info!(
                    "技能文件 {} 契约版本 {version} 过旧，刷新为 v{CONTRACT_VERSION}（旧文件已备份为 {}）",
                    path.display(),
                    backup.display()
                );
            }
            _ => return Ok(()),
        }
    }
    rclaw_store::fs::atomic_write(&path, SKILL_MD.as_bytes())
        .map_err(|error| format!("写入技能文件 {} 失败: {error}", path.display()))
}

/// 旧版技能文件的备份路径：`<skill_dir>/.SKILL.md.v<version>.bak`。
fn stale_backup_path(skill_file: &Path, version: u32) -> PathBuf {
    skill_file.with_file_name(format!(".SKILL.md.v{version}.bak"))
}

/// 解析技能正文里的 `<!-- contract-version: N -->` 头（缺失 / 不合法 → None）。
fn contract_version_of(text: &str) -> Option<u32> {
    const MARKER: &str = "<!-- contract-version:";
    let start = text.find(MARKER)? + MARKER.len();
    let rest = &text[start..];
    let end = rest.find("-->")?;
    rest[..end].trim().parse().ok()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_skill_header_matches_contract_version() {
        assert_eq!(contract_version_of(SKILL_MD), Some(CONTRACT_VERSION));
        assert_eq!(contract_version_of("no header here"), None);
        assert_eq!(contract_version_of("<!-- contract-version: x -->"), None);
        assert_eq!(
            contract_version_of("<!-- contract-version:  3 -->"),
            Some(3)
        );
    }

    #[test]
    fn ensure_skill_file_installs_refreshes_stale_and_keeps_custom() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = skill_path(dir.path());

        // 不存在 → 安装。
        ensure_skill_file(dir.path()).expect("首次安装");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL_MD);

        // 版本头更旧（本程序旧版生成）→ 覆盖为当前正文，旧文件留隐藏 .bak。
        let stale = format!(
            "---\nname: dynamic-workflows\n---\n\n<!-- contract-version: {} -->\n\nold body\n",
            CONTRACT_VERSION - 1
        );
        std::fs::write(&path, &stale).unwrap();
        ensure_skill_file(dir.path()).expect("刷新旧版");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SKILL_MD);
        let backup = stale_backup_path(&path, CONTRACT_VERSION - 1);
        assert_eq!(
            backup.file_name().unwrap().to_string_lossy(),
            format!(".SKILL.md.v{}.bak", CONTRACT_VERSION - 1)
        );
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), stale);

        // 当前版本 → 不动（用户在当前版本上的修改得以保留）。
        let edited = format!("{SKILL_MD}\n## Local notes\n");
        std::fs::write(&path, &edited).unwrap();
        ensure_skill_file(dir.path()).expect("同版本保持");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);

        // 无版本头（用户自行改写）→ 不动。
        let custom = "# my own workflow notes\n";
        std::fs::write(&path, custom).unwrap();
        ensure_skill_file(dir.path()).expect("无版本头保持");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), custom);
    }
}
