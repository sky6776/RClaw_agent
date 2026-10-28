//! WorkflowRunService：工作流运行状态机、配额、幂等、租约与通知 outbox。
//!
//! 所有写工具都先经过这里做权限 / 项目 / 状态校验，再把具体执行交给
//! [`super::workflow_driver`]。`journal_event` 是唯一事实源，投影表只做索引。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use rclaw_core::ToolAccess;
use rclaw_store::fs::Layout;

use super::truncate_chars;
use super::workflow::{
    append_workflow_event, DEFAULT_MAX_PARALLEL_ASKS, DEFAULT_MAX_RUN_SECS, MAX_MAX_PARALLEL_ASKS,
    MAX_MAX_RUN_SECS, MAX_SCRIPT_CHARS,
};
use super::workflow_paths::{resolve_under, session_key, WorkflowPaths};
use super::workflow_snapshot;
use super::workflow_store::{
    self, RunRow, WorkflowStore, STATUS_COMPLETED, STATUS_ERRORED, STATUS_QUEUED, STATUS_RUNNING,
    STATUS_STOPPED, STATUS_STOPPING, STOP_INTERRUPTED, STOP_MODEL, STOP_SUPERSEDED, STOP_USER,
};
use super::*;

/// 工作流运行期开关（来自 `[agent]`）。
#[derive(Debug, Clone)]
pub struct WorkflowOptions {
    pub background: bool,
    pub legacy_tool: bool,
    pub max_active: usize,
    pub command_confirm: bool,
}

impl Default for WorkflowOptions {
    fn default() -> Self {
        WorkflowOptions {
            background: true,
            legacy_tool: false,
            max_active: 4,
            command_confirm: false,
        }
    }
}

/// 结构化工作流错误。
#[derive(Debug, Clone)]
pub(crate) struct WorkflowError {
    pub code: String,
    pub message: String,
    pub kind: String,
    pub retryable: bool,
    pub recovery: String,
}

impl WorkflowError {
    pub(crate) fn new(code: &str, kind: &str, message: impl Into<String>) -> Self {
        WorkflowError {
            code: code.to_string(),
            message: message.into(),
            kind: kind.to_string(),
            retryable: false,
            recovery: "Check the run state with GetWorkflowRun.".to_string(),
        }
    }

    pub(crate) fn invalid_state(message: impl Into<String>) -> Self {
        WorkflowError::new("invalid_state", "state", message)
    }

    pub(crate) fn not_found(run_id: &str) -> Self {
        WorkflowError::new("not_found", "not_found", format!("未找到 run '{run_id}'"))
    }

    pub(crate) fn to_json(&self) -> Value {
        json!({
            "ok": false,
            "code": self.code,
            "message": self.message,
            "retryable": self.retryable,
            "recovery": self.recovery,
            "kind": self.kind,
        })
    }
}

/// 一条运行中的 driver 句柄。
pub(crate) struct ActiveRun {
    pub(crate) cancel: CancellationToken,
    pub(crate) retune_slot: Arc<StdMutex<Option<super::workflow::RetuneHandle>>>,
}

pub(crate) struct WorkerInner {
    pub(crate) runtime: AgentToolRuntime,
    pub(crate) store: WorkflowStore,
    pub(crate) paths: WorkflowPaths,
    pub(crate) opts: WorkflowOptions,
    pub(crate) capacity: StdMutex<HashSet<String>>,
    pub(crate) active: StdMutex<HashMap<String, ActiveRun>>,
    pub(crate) instance_id: String,
    /// 单实例排他锁的文件句柄：只为随服务存活、进程退出时自动释放而持有。
    pub(crate) _lock: StdMutex<Option<std::fs::File>>,
    pub(crate) read_only: AtomicBool,
    pub(crate) outbox_started: AtomicBool,
    /// 启动期发现状态库不可信（`projection_corrupt` / `capacity_unrecoverable`）时的
    /// 降级原因 `(code, message)`：List/Get 照常，写工具一律返回它，等待人工处理。
    pub(crate) degraded: StdMutex<Option<(String, String)>>,
}

static GLOBAL_SERVICE: std::sync::OnceLock<WorkflowService> = std::sync::OnceLock::new();

/// 进程内工作流服务（工具与 UI 命令共用）。
pub(crate) fn global() -> Option<WorkflowService> {
    GLOBAL_SERVICE.get().cloned()
}

/// 工具错误结果（结构化 JSON）。
pub(crate) fn error_result(error: WorkflowError) -> rclaw_tools::ToolResult {
    rclaw_tools::ToolResult::err(error.to_json().to_string())
}

/// 快照读取失败 → §5.3 失败码：hash 不一致（文件被改）是 `script_mismatch`，缺失 / 不可读是
/// `snapshot_missing`。
pub(crate) fn snapshot_error(error: String) -> WorkflowError {
    let code = if error.starts_with(workflow_snapshot::HASH_MISMATCH) {
        "script_mismatch"
    } else {
        "snapshot_missing"
    };
    WorkflowError::new(code, "state", error)
}

/// Create/Amend/Resume 的成功响应 → 工具结果：同步模式（§13.8 `workflow_background=false`）
/// 跑完才返回，run 以 `errored` 收场对调用方就是这次工具调用失败，按 is_error 回给模型
/// （内容仍是结构化 `{status, run_id, error}`）；后台 `backgrounded` / 同步 `completed` 为 ok。
pub(crate) fn run_result(value: Value) -> rclaw_tools::ToolResult {
    if value.get("status").and_then(Value::as_str) == Some(STATUS_ERRORED) {
        rclaw_tools::ToolResult::err(value.to_string())
    } else {
        rclaw_tools::ToolResult::ok(value.to_string())
    }
}

/// 工作流服务句柄。
#[derive(Clone)]
pub struct WorkflowService {
    pub(crate) inner: Arc<WorkerInner>,
}

/// CreateWorkflow 的输入（工具层已完成字段解析）。
pub(crate) struct CreateInput {
    pub name: Option<String>,
    pub script: Option<String>,
    pub path: Option<String>,
    pub saved: Option<SavedRef>,
    pub args: Option<Value>,
    pub max_parallel_asks: Option<u32>,
    pub max_concurrency: Option<u32>,
    pub subagent_model: Option<String>,
    pub max_run_secs: Option<u64>,
    pub type_check: Option<bool>,
    pub request_id: Option<String>,
    pub channel: String,
    pub chat_id: String,
    pub tool_access: ToolAccess,
}

/// `saved` 来源。
pub(crate) struct SavedRef {
    pub name: String,
    pub args: Option<Value>,
    pub scope: Option<String>,
}

/// GetWorkflowRun 的输入选项（§5.5：分页游标都是 0 起的下标，缺省首页）。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GetOptions {
    pub include_legacy: bool,
    pub include_effects: bool,
    pub reports_cursor: usize,
    pub effects_cursor: usize,
    pub artifacts_cursor: usize,
    /// `log_tail` 向前翻页的结束下标；None = 最新一页。
    pub log_cursor: Option<usize>,
}

/// Get 正文预算（字符）：超过就按 §5.5 的顺序缩页，最后把 result 转句柄。
const GET_BUDGET_CHARS: usize = 48_000;
/// reports / effects 每页条数。
const GET_PAGE: usize = 50;
/// artifacts 只列元数据，单页上限。
const GET_ARTIFACTS_MAX: usize = 100;
/// log_tail 单页条数。
const LOG_PAGE: usize = 100;

/// Get 正文各集合的当前页宽；`shrink` 按 §5.5 的统一截断顺序收一步，收无可收返回 false。
struct PageSizes {
    reports: usize,
    effects: usize,
    artifacts: usize,
    log: usize,
    spill_result: bool,
}

impl PageSizes {
    /// 每页至少留 1 条：页宽缩到 0 会让续页游标原地踏步（同一 cursor 永远拿到空页），
    /// 分页就再也翻不过去了。
    fn shrink(&mut self) -> bool {
        if self.reports > 1 {
            self.reports = (self.reports / 2).max(1);
        } else if self.effects > 1 {
            self.effects = (self.effects / 2).max(1);
        } else if self.artifacts > 1 {
            self.artifacts = (self.artifacts / 2).max(1);
        } else if self.log > 1 {
            self.log = (self.log / 2).max(1);
        } else if !self.spill_result {
            self.spill_result = true;
        } else {
            return false;
        }
        true
    }
}

/// 从 `cursor` 起取一页；还有剩余时返回下一页游标。游标越界按空页处理（不报错）。
fn paginate(items: &[Value], cursor: usize, page: usize) -> (Vec<Value>, Option<usize>) {
    let start = cursor.min(items.len());
    let end = start.saturating_add(page).min(items.len());
    let next = (end < items.len()).then_some(end);
    (items[start..end].to_vec(), next)
}

/// 已解析的脚本来源。
struct ScriptSource {
    script: String,
    source_path: Option<String>,
    source_kind: &'static str,
    saved_name: Option<String>,
    saved_scope: Option<String>,
    args: Value,
}

impl WorkflowService {
    pub fn new(runtime: AgentToolRuntime, layout: &Layout, opts: WorkflowOptions) -> Self {
        super::workflow_world::set_command_confirm_enabled(opts.command_confirm);
        let paths = WorkflowPaths::new(layout);
        let store = WorkflowStore::new(runtime.journal_store());
        let instance_id = format!("wf-inst-{:016x}", rand::random::<u64>());
        // 单实例排他锁：拿不到 = 旧实例仍活着，服务只读。
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&paths.lock_file)
            .ok();
        let mut lock = None;
        let mut read_only = true;
        if let Some(file) = lock_file {
            if fs2::FileExt::try_lock_exclusive(&file).is_ok() {
                lock = Some(file);
                read_only = false;
            }
        }
        if read_only {
            tracing::warn!(
                "工作流单实例锁被占用（{}）；工作流服务进入只读模式",
                paths.lock_file.display()
            );
        }
        let service = WorkflowService {
            inner: Arc::new(WorkerInner {
                runtime,
                store,
                paths,
                opts,
                capacity: StdMutex::new(HashSet::new()),
                active: StdMutex::new(HashMap::new()),
                instance_id,
                _lock: StdMutex::new(lock),
                read_only: AtomicBool::new(read_only),
                outbox_started: AtomicBool::new(false),
                degraded: StdMutex::new(None),
            }),
        };
        let _ = GLOBAL_SERVICE.set(service.clone());
        if !read_only {
            if let Err(error) = service.bootstrap_state() {
                tracing::warn!("工作流启动恢复失败（服务继续，新请求可能被拒绝）: {error}");
            }
            service.ensure_outbox();
        }
        service
    }

    pub(crate) fn is_read_only(&self) -> bool {
        self.inner.read_only.load(Ordering::SeqCst)
    }

    fn set_degraded(&self, code: &str, message: impl Into<String>) {
        let message = message.into();
        tracing::error!("工作流服务降级为只读（{code}）: {message}");
        *self
            .inner
            .degraded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some((code.to_string(), message));
    }

    fn check_writable(&self) -> Result<(), WorkflowError> {
        if self.is_read_only() {
            return Err(WorkflowError::new(
                "instance_conflict",
                "state",
                "另一个 RClaw 实例正在管理该工作区；当前实例只读",
            ));
        }
        if let Some((code, message)) = self
            .inner
            .degraded
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
        {
            return Err(WorkflowError::new(&code, "state", message));
        }
        Ok(())
    }

    /// 读取 `path` 来源的脚本：项目根内、≤2MB、UTF-8、≤ MAX_SCRIPT_CHARS。Create / Amend /
    /// Fill / Eval / Save 的 path 入口共用这一处上限——否则模型给个大文件路径就能绕过
    /// 内联 script 的长度校验，把几十 MB 塞进编译器和快照目录。
    pub(crate) fn read_script_file(
        &self,
        path: &str,
    ) -> Result<(std::path::PathBuf, String), WorkflowError> {
        // 先按「可以不存在」解析出落点：根内但不存在 → `path_not_found`（§5.1 失败码，与
        // 越界 / 不可读区分）；存在的再走 must_exist 的符号链接校验。
        let lexical = resolve_under(&self.inner.paths.project_root, path, false)
            .map_err(|error| WorkflowError::new("path_unreadable", "validation", error))?;
        if !lexical.exists() {
            return Err(WorkflowError::new(
                "path_not_found",
                "not_found",
                format!("path 不存在: {path}"),
            ));
        }
        let resolved = resolve_under(&self.inner.paths.project_root, path, true)
            .map_err(|error| WorkflowError::new("path_unreadable", "validation", error))?;
        let size = std::fs::metadata(&resolved)
            .map_err(|error| {
                WorkflowError::new("path_unreadable", "validation", error.to_string())
            })?
            .len();
        if size > 2 * 1024 * 1024 {
            return Err(WorkflowError::new(
                "script_too_large",
                "validation",
                "path 文件超过 2MB",
            ));
        }
        let bytes = std::fs::read(&resolved).map_err(|error| {
            WorkflowError::new("path_unreadable", "validation", error.to_string())
        })?;
        if bytes.len() > 2 * 1024 * 1024 {
            return Err(WorkflowError::new(
                "script_too_large",
                "validation",
                "path 文件超过 2MB",
            ));
        }
        let script = String::from_utf8(bytes).map_err(|_| {
            WorkflowError::new("path_unreadable", "validation", "path 文件不是 UTF-8")
        })?;
        if script.chars().count() > MAX_SCRIPT_CHARS {
            return Err(WorkflowError::new(
                "script_too_large",
                "validation",
                format!("script 超过 {MAX_SCRIPT_CHARS} 字符"),
            ));
        }
        Ok((resolved, script))
    }

    fn bootstrap_state(&self) -> Result<(), String> {
        self.inner.paths.ensure_dirs()?;
        // §3.4：journal 是唯一事实源；seq 缺口 = 损坏，停止重建、服务降级（写工具拒绝），
        // 不能带着不可信的投影去启动 driver / 发配额。
        if let Err(error) = self.inner.store.project_from_cursor() {
            if error.contains("projection_corrupt") {
                self.set_degraded("projection_corrupt", error.clone());
            }
            return Err(error);
        }
        self.inner.store.mark_legacy_rows()?;
        self.inner.store.clear_stale_capacity_owners()?;
        // §3.6：许可归属无法唯一推导 → 拒绝新请求 `capacity_unrecoverable`，只读等人工处理。
        let inconsistent = self.inner.store.inconsistent_capacity_owners()?;
        if !inconsistent.is_empty() {
            let detail = inconsistent
                .iter()
                .map(|(run_id, owner)| format!("{run_id} → {owner}"))
                .collect::<Vec<_>>()
                .join(", ");
            self.set_degraded(
                "capacity_unrecoverable",
                format!("工作流许可归属无法唯一推导（{detail}）；请人工修正 workflow_run.capacity_owner 后重启"),
            );
            return Err(format!("capacity_unrecoverable: {detail}"));
        }
        let owners = self.inner.store.capacity_owners()?;
        {
            let mut capacity = self
                .inner
                .capacity
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            capacity.extend(owners);
        }
        // 快照校验 + 残留进程判定 + 中断收口。
        self.recover_unfinished()?;
        let referenced = self
            .inner
            .store
            .referenced_snapshots()?
            .into_iter()
            .map(|path| {
                if path.is_absolute() {
                    path
                } else {
                    self.inner.paths.project_root.join(path)
                }
            })
            .collect();
        workflow_snapshot::cleanup_orphans(&self.inner.paths, &referenced);
        Ok(())
    }

    /// §13.1 第 2 步：`running/queued`（含 stopping）的快照必须存在且 hash 匹配，否则
    /// `errored(snapshot_missing)`、不可 resume/amend、不进入驱动。
    fn snapshot_valid(&self, row: &RunRow) -> Result<(), String> {
        let path = resolve_snapshot(&self.inner.paths, row).map_err(|error| error.message)?;
        workflow_snapshot::read_snapshot(&path, &row.snapshot_hash).map(|_| ())
    }

    fn fail_snapshot_missing(&self, row: &RunRow, detail: &str) -> Result<(), String> {
        let message = format!("快照校验失败: {detail}");
        let event = json!({
            "event": "run_failed",
            "run_id": row.run_id,
            "exec_generation": row.exec_generation,
            "error": {"code": "snapshot_missing", "message": truncate_chars(&message, 4_000), "kind": "state"},
        });
        // queued 后继的 claim 一并撤销（§13.2「后继快照缺失/hash 不符 → errored，撤销 claim」）。
        if row.capacity_claim_for.is_some() {
            let _ = self.inner.store.set_capacity_claim(&row.run_id, None);
        }
        self.terminal_transaction(
            row,
            STATUS_ERRORED,
            &event,
            None,
            None,
            false,
            Some(false),
            Some(&message),
        )
    }

    fn recover_unfinished(&self) -> Result<(), String> {
        // 单行恢复失败只记日志、继续下一行：一条坏行不能让其余 run 全部卡在 running/queued。
        let mut scheduled: HashSet<String> = HashSet::new();
        for row in self.inner.store.list_unfinished()? {
            match self.recover_row(&row) {
                Ok(true) => {
                    scheduled.insert(row.run_id.clone());
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!("工作流 {} 重启恢复失败，跳过: {error}", row.run_id);
                }
            }
        }
        // 第二轮：第一轮把前驱收口后，queued 后继现在可以启动（第一轮已排期的不重复）。
        for row in self.inner.store.list_unfinished()? {
            if row.status != STATUS_QUEUED || scheduled.contains(&row.run_id) {
                continue;
            }
            let Some(predecessor) = row.resumed_from.clone() else {
                continue;
            };
            if let Ok(Some(predecessor_row)) = self.inner.store.get_run(&predecessor) {
                if predecessor_row.is_terminal() {
                    self.schedule_recovered_successor(&row, &predecessor);
                }
            }
        }
        Ok(())
    }

    /// 重启恢复：前驱已终态的 queued 后继——交接许可、计入本地配额、排期启动。
    /// 同步模式（§13.8 不持久化 queued）没有后台执行者，残留的 queued 后继只能是崩在 Amend
    /// 等待窗口里的产物：按同步 Amend 超时同样回滚（后继 stopped、前驱解除 superseded）。
    fn schedule_recovered_successor(&self, row: &RunRow, predecessor: &str) {
        if !self.inner.opts.background {
            self.rollback_sync_successor(predecessor, row, "", "");
            return;
        }
        let _ = self.inner.store.transfer_capacity(predecessor, &row.run_id);
        let mut capacity = self
            .inner
            .capacity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        capacity.insert(row.run_id.clone());
        drop(capacity);
        self.start_recovered_driver(&row.run_id, row.exec_generation.max(1), Some(predecessor));
    }

    /// 重启恢复期拉起 driver：`WorkflowService::new` 跑在装配中段、早于工具注册表回填，
    /// 直接启动的话第一个 ask 就撞上「registry 尚未初始化」把 run 判失败。先等注册表就绪
    /// （上限 60s，装配通常毫秒级）再走正常的 `start_driver`（重复启动仍由 register_active 挡）。
    fn start_recovered_driver(&self, run_id: &str, generation: u32, predecessor: Option<&str>) {
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.start_driver(run_id, generation, predecessor);
            return;
        };
        let service = self.clone();
        let run_id = run_id.to_string();
        let predecessor = predecessor.map(str::to_string);
        handle.spawn(async move {
            for _ in 0..1_200 {
                if service.inner.runtime.registry_ready() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            service.start_driver(&run_id, generation, predecessor.as_deref());
        });
    }

    /// 单行重启恢复；返回是否已为该行排期 driver。
    fn recover_row(&self, row: &RunRow) -> Result<bool, String> {
        if let Err(error) = self.snapshot_valid(row) {
            tracing::warn!("工作流 {} 快照校验失败，标为 errored: {error}", row.run_id);
            self.fail_snapshot_missing(row, &error)?;
            return Ok(false);
        }
        match row.status.as_str() {
            STATUS_QUEUED => {
                if row.superseded_by.is_some() {
                    self.finish_queued_aborted(row, "successor superseded")?;
                    return Ok(false);
                }
                // 前驱已终态 → 启动；前驱不存在 → 无法恢复，按 interrupted 收口。
                if let Some(predecessor) = &row.resumed_from {
                    match self.inner.store.get_run(predecessor)? {
                        Some(predecessor_row) if predecessor_row.is_terminal() => {
                            self.schedule_recovered_successor(row, predecessor);
                            return Ok(true);
                        }
                        Some(_) => {}
                        None => self.finish_queued_aborted(row, "predecessor missing")?,
                    }
                } else {
                    self.finish_queued_aborted(row, "queued without predecessor")?;
                }
            }
            STATUS_RUNNING | STATUS_STOPPING => {
                // 进程树未确认退出 → 保持 stopping，等待 UI confirm。
                let running_processes = self
                    .inner
                    .store
                    .running_processes(&row.run_id, row.exec_generation)?;
                if !running_processes.is_empty() && !process_tree_proven_gone(&running_processes) {
                    if row.status != STATUS_STOPPING {
                        let _ = self.mark_stopping_interrupted(row);
                    }
                    return Ok(false);
                }
                self.finish_interrupted(row)?;
            }
            _ => {}
        }
        Ok(false)
    }

    /// 重启时无法再调度的 queued 后继：这是宿主中断（§13.1 `interrupted`），不是用户取消——
    /// 标成 user + fresh-auth 会让它没有任何人签得出 token、永远不可 resume。
    fn finish_queued_aborted(&self, row: &RunRow, detail: &str) -> Result<(), String> {
        let origin = row
            .stop_origin
            .clone()
            .unwrap_or_else(|| STOP_INTERRUPTED.to_string());
        let requires_fresh_auth = row.requires_fresh_auth || origin == STOP_USER;
        let event = json!({
            "event": "run_stopped",
            "exec_generation": row.exec_generation,
            "reason": STOP_INTERRUPTED,
            "stop_origin": origin,
            "requires_fresh_auth": requires_fresh_auth as i64,
            "detail": detail,
        });
        self.terminal_transaction(
            row,
            STATUS_STOPPED,
            &event,
            Some(STOP_INTERRUPTED),
            Some(&origin),
            requires_fresh_auth,
            None,
            Some(detail),
        )
    }

    fn mark_stopping_interrupted(&self, row: &RunRow) -> Result<(), String> {
        let event = json!({
            "event": "run_stopping",
            "exec_generation": row.exec_generation,
            "reason": STOP_INTERRUPTED,
            "stop_origin": STOP_INTERRUPTED,
        });
        let journal = EventLog::workflow(self.inner.store.journal(), &row.run_id);
        journal.append(&event)?;
        self.inner.store.set_status(
            &row.run_id,
            STATUS_STOPPING,
            None,
            Some(STOP_INTERRUPTED),
            Some(STOP_INTERRUPTED),
            row.requires_fresh_auth,
            false,
        )
    }

    fn finish_interrupted(&self, row: &RunRow) -> Result<(), String> {
        let next_generation = row.exec_generation.saturating_add(1);
        let blocked = self
            .inner
            .store
            .blocking_effects(&row.run_id, next_generation)?;
        // 文档 §13.1：崩在 stopping 窗口的 run 保留原 stop_origin / fresh-auth 要求——用户
        // 取消过的 run 不能因为宿主重启就变成模型可自行 Resume 的 interrupted。要求按
        // stop_origin 兜底推导，不只信行上的标志位（投影重放后标志位可能落后于 origin）。
        let origin = row
            .stop_origin
            .clone()
            .unwrap_or_else(|| STOP_INTERRUPTED.to_string());
        let requires_fresh_auth = row.requires_fresh_auth || origin == STOP_USER;
        let event = json!({
            "event": "run_stopped",
            "exec_generation": row.exec_generation,
            "reason": STOP_INTERRUPTED,
            "stop_origin": origin,
            "requires_fresh_auth": requires_fresh_auth as i64,
            "detail": if blocked.is_empty() { "host restarted" } else { "host restarted with unresolved effects" },
        });
        self.terminal_transaction(
            row,
            STATUS_STOPPED,
            &event,
            Some(STOP_INTERRUPTED),
            Some(&origin),
            requires_fresh_auth,
            blocked.is_empty().then_some(true),
            None,
        )
    }

    // -----------------------------------------------------------------------
    // Create
    // -----------------------------------------------------------------------

    pub(crate) async fn create(&self, input: CreateInput) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let project_root = self.inner.paths.project_root.clone();
        let session = session_key(&input.channel, &input.chat_id);
        let source = self.resolve_source(&input, &session).await?;
        let max_parallel_asks = normalize_parallel(
            input.max_parallel_asks,
            input.max_concurrency,
            DEFAULT_MAX_PARALLEL_ASKS as usize,
        )?;
        let type_check = input.type_check.unwrap_or(true);
        let prepared = super::workflow::prepare_script(&source.script, type_check)
            .await
            .map_err(|error| WorkflowError::new("script_diagnostics", "validation", error))?;
        let max_run_secs = input
            .max_run_secs
            .unwrap_or(DEFAULT_MAX_RUN_SECS)
            .clamp(1, MAX_MAX_RUN_SECS);
        let name = super::team::sanitize_agent_name(
            input
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or("workflow"),
        );
        let allowed_models = self.allowed_models(&prepared.script, input.subagent_model.as_deref());
        let execution_config = json!({
            "args": source.args,
            "max_parallel_asks": max_parallel_asks,
            "subagent_model": input.subagent_model,
            "allowed_models": allowed_models,
            "max_run_secs": max_run_secs,
            "type_check": type_check,
            "facade_version": 5,
            "tool_access": access_name(input.tool_access),
            "source_kind": source.source_kind,
            "saved_name": source.saved_name,
            "saved_scope": source.saved_scope,
            "channel": input.channel,
            "chat_id": input.chat_id,
            "background_mode": self.inner.opts.background,
        });
        let request_id = normalize_request_id(input.request_id.clone())?;
        let scope = format!("{}/CreateWorkflow", project_root.to_string_lossy());
        let digest = digest_json(&json!({
            "script": prepared.script,
            "name": name,
            "max_parallel_asks": max_parallel_asks,
            "subagent_model": input.subagent_model,
            "max_run_secs": max_run_secs,
            "type_check": type_check,
            "source_kind": source.source_kind,
            "saved_name": source.saved_name,
            "args": execution_config.get("args"),
        }));
        if let Some(existing) = self
            .inner
            .store
            .get_request(&scope, &request_id)
            .map_err(internal_error)?
        {
            if existing.digest != digest {
                return Err(WorkflowError::new(
                    "request_conflict",
                    "validation",
                    "同一 request_id 对应不同请求内容",
                ));
            }
            match existing.state.as_str() {
                "done" => {
                    if let Some(response) = existing.response {
                        return Ok(serde_json::from_str(&response).unwrap_or(Value::Null));
                    }
                }
                "in_progress" => {
                    if let Some(run_id) = existing.run_id {
                        if let Some(row) =
                            self.inner.store.get_run(&run_id).map_err(internal_error)?
                        {
                            return Ok(self.create_response(&row, &request_id));
                        }
                    }
                    let _ = self.inner.store.mark_request_failed(&scope, &request_id);
                }
                _ => {
                    let _ = self.inner.store.reset_request(&scope, &request_id, &digest);
                }
            }
        }
        let run_id = super::team::next_execution_id(&format!("wf-{name}"));
        if self
            .inner
            .store
            .get_run(&run_id)
            .map_err(internal_error)?
            .is_some()
        {
            return Err(WorkflowError::new(
                "internal",
                "internal",
                "run_id 冲突，请重试",
            ));
        }
        let (snapshot_path, snapshot_hash) =
            workflow_snapshot::write_snapshot(&self.inner.paths, &run_id, 1, &prepared.script)
                .map_err(|error| WorkflowError::new("internal", "internal", error))?;
        let display_snapshot = workflow_snapshot::display_path(&self.inner.paths, &snapshot_path);
        let acquired = self.acquire_capacity(&run_id)?;
        if !acquired {
            return Err(WorkflowError::new(
                "capacity_exceeded",
                "capacity",
                format!(
                    "同时运行的工作流已达上限 {}；等一个 run 结束后重试",
                    self.inner.opts.max_active
                ),
            ));
        }
        let now = rclaw_store::db::now_ms();
        let row = RunRow {
            run_id: run_id.clone(),
            cwd: self.inner.paths.project_key.clone(),
            session_key: session.clone(),
            label: name.clone(),
            source_path: source.source_path.clone(),
            snapshot_path: display_snapshot.clone(),
            snapshot_hash: snapshot_hash.clone(),
            script_generation: 1,
            exec_generation: 1,
            legacy: false,
            resumable: true,
            status: STATUS_RUNNING.to_string(),
            queued_reason: None,
            stop_reason: None,
            stop_origin: None,
            requires_fresh_auth: false,
            capacity_owner: Some(run_id.clone()),
            capacity_claim_for: None,
            resumed_from: None,
            superseded_by: None,
            started_by: "main".to_string(),
            execution_config: serde_json::to_string(&execution_config)
                .unwrap_or_else(|_| "{}".into()),
            owner_instance: Some(self.inner.instance_id.clone()),
            lease_until: Some(now + 60_000),
            last_event_id: 0,
            elapsed_runtime_ms: 0,
            cache_closed: false,
            created_at: now,
            updated_at: now,
            heartbeat_at: now,
            spent_tokens: 0,
            result_json: None,
            result_handle: None,
            error: None,
        };
        let response = self.create_response(&row, &request_id);
        let response_text = response.to_string();
        let store = self.inner.store.clone();
        let insert = store.transaction(|tx| {
            workflow_store::insert_run_tx(tx, &row).map_err(|error| error.to_string())?;
            workflow_store::insert_script_version_tx(
                tx,
                &run_id,
                1,
                &display_snapshot,
                &snapshot_hash,
                None,
                "create",
                false,
            )
            .map_err(|error| error.to_string())?;
            // 上面 failed / 孤儿 in_progress 的旧请求行只是被 reset / 标失败，并没有删除：
            // 这里必须覆盖写，否则重试同一 request_id 会撞 UNIQUE 失败、还漏一份快照文件。
            tx.execute(
                "INSERT OR REPLACE INTO workflow_request (scope, request_id, digest, state, run_id, \
                 response, created_at, updated_at) VALUES (?1, ?2, ?3, 'in_progress', ?4, NULL, ?5, ?5)",
                rusqlite::params![scope, request_id, digest, run_id, now],
            )
            .map_err(|error| error.to_string())?;
            workflow_store::set_request_done_tx(tx, &scope, &request_id, &response_text)
                .map_err(|error| error.to_string())?;
            Ok(())
        });
        if let Err(error) = insert {
            self.release_capacity(&run_id);
            return Err(WorkflowError::new("internal", "internal", error));
        }
        if self.inner.opts.background {
            self.start_driver(&run_id, 1, None);
            Ok(response)
        } else {
            let outcome = super::workflow_driver::run_sync(self.clone(), &run_id, 1, None).await;
            // 同步模式：request 行缓存同步契约的最终响应（重试返回同一结果）。
            let text = outcome.to_string();
            let _ = self.inner.store.transaction(|tx| {
                tx.execute(
                    "UPDATE workflow_request SET response = ?3, updated_at = ?4                      WHERE scope = ?1 AND request_id = ?2",
                    rusqlite::params![
                        scope,
                        request_id,
                        text,
                        rclaw_store::db::now_ms()
                    ],
                )
                .map_err(|error| error.to_string())?;
                Ok(())
            });
            Ok(outcome)
        }
    }

    fn create_response(&self, row: &RunRow, request_id: &str) -> Value {
        json!({
            "status": if self.inner.opts.background { "backgrounded" } else { "completed" },
            "run_id": row.run_id,
            "label": row.label,
            "script_generation": row.script_generation,
            "exec_generation": row.exec_generation,
            "snapshot_path": row.snapshot_path,
            "snapshot_hash": row.snapshot_hash,
            "request_id": request_id,
            "note": "Use GetWorkflowRun to inspect progress; completion arrives as a notification.",
        })
    }

    async fn resolve_source(
        &self,
        input: &CreateInput,
        session: &str,
    ) -> Result<ScriptSource, WorkflowError> {
        let mut given = 0;
        if input
            .script
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            given += 1;
        }
        if input
            .path
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
        {
            given += 1;
        }
        if input.saved.is_some() {
            given += 1;
        }
        if given != 1 {
            return Err(WorkflowError::new(
                "source_conflict",
                "validation",
                "script / path / saved 必须且只能给一个",
            ));
        }
        if input.args.is_some() && input.saved.is_some() {
            return Err(WorkflowError::new(
                "args_conflict",
                "validation",
                "顶层 args 与 saved.args 不能同时给",
            ));
        }
        if input.args.is_some() && input.path.is_none() {
            return Err(WorkflowError::new(
                "args_conflict",
                "validation",
                "顶层 args 只用于 path 来源",
            ));
        }
        if let Some(script) = input.script.clone().filter(|text| !text.trim().is_empty()) {
            self.check_skill_gate(session)?;
            return Ok(ScriptSource {
                script,
                source_path: None,
                source_kind: "inline",
                saved_name: None,
                saved_scope: None,
                args: input.args.clone().unwrap_or_else(|| json!({})),
            });
        }
        if let Some(path) = input.path.clone().filter(|text| !text.trim().is_empty()) {
            self.check_skill_gate(session)?;
            let (resolved, script) = self.read_script_file(&path)?;
            return Ok(ScriptSource {
                script,
                source_path: Some(resolved.to_string_lossy().to_string()),
                source_kind: "path",
                saved_name: None,
                saved_scope: None,
                args: input.args.clone().unwrap_or_else(|| json!({})),
            });
        }
        let saved = input.saved.as_ref().expect("given == 1 且非 script/path");
        if saved.name.is_empty()
            || saved.name.len() > 64
            || !saved
                .name
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        {
            return Err(WorkflowError::new(
                "args_invalid",
                "validation",
                "saved.name 必须匹配 [A-Za-z0-9._-]+ 且 <=64",
            ));
        }
        let scope = saved.scope.as_deref().unwrap_or("project");
        if !matches!(scope, "project" | "global") {
            return Err(WorkflowError::new(
                "args_invalid",
                "validation",
                "saved.scope 只能是 project / global",
            ));
        }
        if scope == "global" && input.tool_access != ToolAccess::Unrestricted {
            return Err(WorkflowError::new(
                "forbidden_global",
                "permission",
                "global 存档仅 Unrestricted 会话可用",
            ));
        }
        let dir = if scope == "global" {
            self.inner.paths.global_saved_dir.clone()
        } else {
            self.inner.paths.saved_dir.clone()
        };
        let path = dir.join(format!("{}.dwf.js", saved.name));
        let text = std::fs::read_to_string(&path).map_err(|error| {
            WorkflowError::new(
                "saved_not_found",
                "not_found",
                format!("读取存档 {} 失败: {error}", path.display()),
            )
        })?;
        let (script, declared) = super::workflow_saved::parse_saved_script(&text)
            .map_err(|error| WorkflowError::new("saved_invalid", "validation", error))?;
        let args = match saved.args.clone().or_else(|| input.args.clone()) {
            Some(args) => super::workflow_saved::apply_args(&declared, &args)
                .map_err(|error| WorkflowError::new("args_invalid", "validation", error))?,
            None => super::workflow_saved::apply_args(&declared, &json!({}))
                .map_err(|error| WorkflowError::new("args_invalid", "validation", error))?,
        };
        Ok(ScriptSource {
            script,
            source_path: Some(path.to_string_lossy().to_string()),
            source_kind: "saved",
            saved_name: Some(saved.name.clone()),
            saved_scope: Some(scope.to_string()),
            args,
        })
    }

    /// 当前 turn 的技能门（Save / Eval 这类没有显式 channel/chat 入参的工具用）。
    pub(crate) fn check_skill_gate_for_current_session(&self) -> Result<(), WorkflowError> {
        let (channel, chat_id) = current_session();
        self.check_skill_gate(&session_key(&channel, &chat_id))
    }

    fn check_skill_gate(&self, session: &str) -> Result<(), WorkflowError> {
        let path = super::workflow_skill::skill_path(&self.inner.paths.project_root);
        let current_hash = super::workflow_skill::file_hash(&path);
        let key = format!("workflow-skill/{session}");
        let stored = self
            .inner
            .runtime
            .journal_store()
            .kv_get(&key)
            .ok()
            .flatten();
        // 文档 §4.3：hash 一致且登记时的契约版本不低于工具要求（工具面变更后旧登记失效）。
        let valid = current_hash.is_some()
            && stored
                .as_ref()
                .and_then(|value| value.get("content_sha256").and_then(Value::as_str))
                == current_hash.as_deref()
            && stored
                .as_ref()
                .and_then(|value| value.get("contract_version").and_then(Value::as_u64))
                .is_some_and(|version| version >= super::workflow_skill::CONTRACT_VERSION as u64);
        if valid {
            return Ok(());
        }
        Err(WorkflowError::new(
            "workflow_skill_required",
            "validation",
            format!(
                "先完整读取 {}（dynamic-workflows 技能），再提交工作流脚本",
                path.display()
            ),
        ))
    }

    // -----------------------------------------------------------------------
    // Amend
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn amend(
        &self,
        run_id: &str,
        script: Option<String>,
        path: Option<String>,
        name: Option<String>,
        max_parallel_asks: Option<Option<u32>>,
        subagent_model: Option<Option<String>>,
        type_check: Option<bool>,
        request_id: Option<String>,
        auth_token: Option<String>,
        channel: String,
        chat_id: String,
        tool_access: ToolAccess,
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let predecessor = self.load_run_checked(run_id, tool_access, &channel, &chat_id, true)?;
        // 幂等优先于状态检查：已成功请求的重试直接返回原响应。
        let request_id = normalize_request_id(request_id)?;
        let scope = format!(
            "{}/AmendWorkflow",
            self.inner.paths.project_root.to_string_lossy()
        );
        let digest = digest_json(&json!({
            "run_id": run_id,
            "script": &script,
            "path": &path,
            "name": &name,
            "max_parallel_asks": &max_parallel_asks,
            "subagent_model": &subagent_model,
        }));
        if let Some(existing) = self
            .inner
            .store
            .get_request(&scope, &request_id)
            .map_err(internal_error)?
        {
            if existing.digest != digest {
                return Err(WorkflowError::new(
                    "request_conflict",
                    "validation",
                    "同一 request_id 对应不同 amend 请求",
                ));
            }
            if existing.state == "done" {
                if let Some(response) = existing.response {
                    return Ok(serde_json::from_str(&response).unwrap_or(Value::Null));
                }
            }
            if existing.state == "in_progress" {
                if let Some(existing_run) = existing.run_id {
                    if let Some(row) = self
                        .inner
                        .store
                        .get_run(&existing_run)
                        .map_err(internal_error)?
                    {
                        return Ok(json!({
                            "status": "backgrounded",
                            "run_id": row.run_id,
                            "superseded_run_id": run_id,
                            "script_generation": row.script_generation,
                            "request_id": request_id,
                        }));
                    }
                }
                let _ = self.inner.store.mark_request_failed(&scope, &request_id);
            }
        }
        if predecessor.superseded_by.is_some() {
            return Err(WorkflowError::new(
                "already_superseded",
                "state",
                "该 run 已有后继",
            ));
        }
        if predecessor.status == STATUS_STOPPING || predecessor.status == STATUS_QUEUED {
            return Err(WorkflowError::invalid_state(
                "run 正在停止 / 排队中，不能 amend",
            ));
        }
        // 文档 §5.6：`filling` 期间 Amend 返回 invalid_state（Cancel 才有优先权），否则会把
        // 正在提交的 Fill 半路掐断成 fill_aborted。
        if predecessor.status == STATUS_RUNNING
            && self
                .inner
                .store
                .list_holes(run_id)
                .map_err(internal_error)?
                .iter()
                .any(|hole| hole.state == "filling")
        {
            return Err(WorkflowError::invalid_state(
                "run 正在补洞（hole filling），稍后再 amend",
            ));
        }
        if predecessor.stop_origin.as_deref() == Some(STOP_USER)
            && predecessor.requires_fresh_auth
            && !self.consume_auth_token(run_id, auth_token.as_deref())
        {
            return Err(WorkflowError::new(
                "fresh_auth_required",
                "permission",
                "用户取消过的 run 需要 UI 签发的一次性 auth_token",
            ));
        }
        if script.is_some() && path.is_some() {
            return Err(WorkflowError::new(
                "source_conflict",
                "validation",
                "script 与 path 互斥",
            ));
        }
        // 技能门与 Create 同口径：改脚本（内联或文件）必须先完整读过 SKILL.md，
        // 否则 amend 成了绕过技能门提交新脚本的后门。
        if script.is_some() || path.is_some() {
            self.check_skill_gate(&session_key(&channel, &chat_id))?;
        }
        let has_retune = max_parallel_asks.is_some() || subagent_model.is_some();
        let has_change = script.is_some() || path.is_some() || name.is_some() || has_retune;
        if !has_change {
            return Err(WorkflowError::new(
                "nothing_to_change",
                "validation",
                "没有提供任何变更",
            ));
        }
        // §13.6：越界报错而不是夹到边界（与 Create 同一校验）。
        if let Some(Some(limit)) = max_parallel_asks {
            normalize_parallel(Some(limit), None, DEFAULT_MAX_PARALLEL_ASKS as usize)?;
        }
        // 没给 script / path 就复用前驱快照（改名 / 终态只调设置都是 §5.2 模式 B 的后继）。
        let reuse_snapshot = script.is_none() && path.is_none();
        // 模式 A（原地调参）只对 running 前驱；终态前驱"只调设置"走下面的模式 B 用同一快照重建。
        if reuse_snapshot && name.is_none() && has_retune && predecessor.status == STATUS_RUNNING {
            let mut config = predecessor.execution_config();
            if let Some(limit) = max_parallel_asks {
                match limit {
                    Some(limit) => {
                        let limit = limit.clamp(1, MAX_MAX_PARALLEL_ASKS);
                        config["max_parallel_asks"] = json!(limit);
                    }
                    None => {
                        config["max_parallel_asks"] = json!(DEFAULT_MAX_PARALLEL_ASKS);
                    }
                }
            }
            if let Some(model) = &subagent_model {
                config["subagent_model"] = json!(model);
                if let Some(model) = model {
                    if let Some(models) = config
                        .get_mut("allowed_models")
                        .and_then(Value::as_array_mut)
                    {
                        if !models.iter().any(|value| value.as_str() == Some(model)) {
                            models.push(json!(model));
                        }
                    }
                }
            }
            let event = json!({
                "event": "run_retuned",
                "max_parallel_asks": max_parallel_asks.flatten(),
                "subagent_model": subagent_model.clone().flatten(),
                "execution_config": config,
            });
            let journal = EventLog::workflow(self.inner.store.journal(), run_id);
            journal
                .append(&event)
                .map_err(|error| WorkflowError::new("internal", "internal", error))?;
            self.inner
                .store
                .set_execution_config(run_id, &config)
                .map_err(internal_error)?;
            if let Some(handle) = self
                .inner
                .active
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(run_id)
                .and_then(|active| {
                    active
                        .retune_slot
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner())
                        .clone()
                })
            {
                handle.apply(
                    max_parallel_asks.flatten().map(|value| value as usize),
                    subagent_model.clone(),
                    subagent_model.as_ref().and_then(|model| model.as_deref()),
                );
            }
            return Ok(json!({
                "status": "retuned",
                "run_id": run_id,
                "request_id": request_id,
            }));
        }
        // 写 effect 的 run 默认拒绝 Amend（需要 UI 逐项确认后才开放）。
        if self
            .inner
            .store
            .has_write_effects(run_id)
            .map_err(internal_error)?
            && !self.confirmed_amend(run_id)
        {
            return Err(WorkflowError::new(
                "amend_blocked_direct_effect",
                "state",
                "前驱存在写副作用；需要 UI 调用 workflow_confirm_amend 确认后才能重建",
            ));
        }
        let new_script = if let Some(script) = script {
            if script.chars().count() > MAX_SCRIPT_CHARS {
                return Err(WorkflowError::new(
                    "script_too_large",
                    "validation",
                    format!("script 超过 {MAX_SCRIPT_CHARS} 字符"),
                ));
            }
            script
        } else if let Some(path) = path {
            self.read_script_file(&path)?.1
        } else {
            let snapshot = resolve_snapshot(&self.inner.paths, &predecessor)?;
            workflow_snapshot::read_snapshot(&snapshot, &predecessor.snapshot_hash)
                .map_err(snapshot_error)?
        };
        let prepared = super::workflow::prepare_script(&new_script, type_check.unwrap_or(true))
            .await
            .map_err(|error| WorkflowError::new("script_diagnostics", "validation", error))?;
        // 只有调用方显式给了新脚本才查"与前驱相同"：复用快照的改名 / 终态调设置本就同一脚本。
        if !reuse_snapshot
            && Some(prepared.script_hash)
                == u64::from_str_radix(&predecessor.snapshot_hash, 16).ok()
        {
            return Err(WorkflowError::new(
                "script_unchanged",
                "validation",
                "新脚本与前驱相同；只调参请不传 script",
            ));
        }
        let mut config = predecessor.execution_config();
        let limit = match max_parallel_asks {
            Some(Some(limit)) => limit.clamp(1, MAX_MAX_PARALLEL_ASKS),
            Some(None) => DEFAULT_MAX_PARALLEL_ASKS,
            None => config
                .get("max_parallel_asks")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_MAX_PARALLEL_ASKS as u64) as u32,
        };
        config["max_parallel_asks"] = json!(limit);
        if let Some(model) = &subagent_model {
            config["subagent_model"] = json!(model);
        }
        config["source_kind"] = json!("amend");
        config["import_replay_from"] = json!(run_id);
        config["import_replay"] = json!(!predecessor.cache_closed);
        let successor_id =
            super::team::next_execution_id(&format!("wf-{}-amend", predecessor.label));
        let generation = predecessor.script_generation.saturating_add(1);
        let (snapshot_path, snapshot_hash) = workflow_snapshot::write_snapshot(
            &self.inner.paths,
            &successor_id,
            generation,
            &prepared.script,
        )
        .map_err(|error| WorkflowError::new("internal", "internal", error))?;
        let display_snapshot = workflow_snapshot::display_path(&self.inner.paths, &snapshot_path);
        let predecessor_running = predecessor.status == STATUS_RUNNING;
        if predecessor_running {
            // 许可由前驱持有；后继只登记 claim，不占新许可。
            self.inner
                .store
                .set_capacity_claim(&successor_id, Some(&successor_id))
                .map_err(internal_error)?;
        } else if !self.acquire_capacity(&successor_id)? {
            return Err(WorkflowError::new(
                "capacity_exceeded",
                "capacity",
                "同时运行的工作流已达上限",
            ));
        }
        let now = rclaw_store::db::now_ms();
        let successor = RunRow {
            run_id: successor_id.clone(),
            cwd: predecessor.cwd.clone(),
            session_key: predecessor.session_key.clone(),
            label: name
                .map(|name| super::team::sanitize_agent_name(&name))
                .unwrap_or_else(|| predecessor.label.clone()),
            source_path: predecessor.source_path.clone(),
            snapshot_path: display_snapshot.clone(),
            snapshot_hash,
            script_generation: generation,
            exec_generation: 1,
            legacy: false,
            resumable: true,
            status: STATUS_QUEUED.to_string(),
            queued_reason: Some(if predecessor_running {
                "predecessor_stopping".to_string()
            } else {
                "none".to_string()
            }),
            stop_reason: None,
            stop_origin: None,
            requires_fresh_auth: false,
            capacity_owner: (!predecessor_running).then(|| successor_id.clone()),
            capacity_claim_for: predecessor_running.then(|| successor_id.clone()),
            resumed_from: Some(run_id.to_string()),
            superseded_by: None,
            started_by: "main".to_string(),
            execution_config: serde_json::to_string(&config).unwrap_or_else(|_| "{}".into()),
            owner_instance: Some(self.inner.instance_id.clone()),
            lease_until: None,
            last_event_id: 0,
            elapsed_runtime_ms: 0,
            cache_closed: false,
            created_at: now,
            updated_at: now,
            heartbeat_at: 0,
            spent_tokens: 0,
            result_json: None,
            result_handle: None,
            error: None,
        };
        let response = json!({
            "status": "backgrounded",
            "run_id": successor_id,
            "superseded_run_id": run_id,
            "script_generation": generation,
            "request_id": request_id,
        });
        let response_text = response.to_string();
        let journal = EventLog::workflow(self.inner.store.journal(), run_id);
        let successor_journal = EventLog::workflow(self.inner.store.journal(), &successor_id);
        let store = self.inner.store.clone();
        let predecessor_clone = predecessor.clone();
        let successor_clone = successor.clone();
        let transaction = store.transaction(|tx| {
            workflow_store::insert_run_tx(tx, &successor_clone).map_err(|e| e.to_string())?;
            workflow_store::insert_script_version_tx(
                tx,
                &successor_clone.run_id,
                generation,
                &successor_clone.snapshot_path,
                &successor_clone.snapshot_hash,
                Some(&predecessor_clone.snapshot_hash),
                "amend",
                false,
            )
            .map_err(|e| e.to_string())?;
            let queued = json!({
                "event": "run_queued",
                "run_id": successor_clone.run_id,
                "queued_reason": successor_clone.queued_reason,
                "superseded_by": serde_json::Value::Null,
                "label": successor_clone.label,
                "cwd": successor_clone.cwd,
                "session_key": successor_clone.session_key,
                "started_by": successor_clone.started_by,
                "script_generation": generation,
                "script_hash": successor_clone.snapshot_hash,
                "snapshot_path": successor_clone.snapshot_path,
                "execution_config": successor_clone.execution_config(),
            });
            successor_journal
                .append_tx(tx, &queued)
                .map_err(|e| e.to_string())?;
            let changed = tx
                .execute(
                    "UPDATE workflow_run SET superseded_by = ?2, updated_at = ?3, \
                     status = CASE WHEN status = 'running' THEN 'stopping' ELSE status END, \
                     stop_reason = CASE WHEN status = 'running' THEN 'superseded' ELSE stop_reason END, \
                     stop_origin = CASE WHEN status = 'running' THEN 'superseded' ELSE stop_origin END \
                     WHERE run_id = ?1 AND superseded_by IS NULL AND status = ?4 \
                     AND script_generation = ?5",
                    rusqlite::params![
                        run_id,
                        successor_clone.run_id,
                        now,
                        predecessor_clone.status,
                        predecessor_clone.script_generation as i64
                    ],
                )
                .map_err(|e| e.to_string())?;
            if changed == 0 {
                return Err("predecessor_changed".to_string());
            }
            journal
                .append_tx(
                    tx,
                    &json!({"event":"run_superseded","run_id":run_id,"by":successor_clone.run_id}),
                )
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO workflow_request (scope, request_id, digest, state, run_id, \
                 response, created_at, updated_at) VALUES (?1, ?2, ?3, 'done', ?4, ?5, ?6, ?6)",
                rusqlite::params![scope, request_id, digest, successor_clone.run_id, response_text, now],
            )
            .map_err(|e| e.to_string())?;
            Ok(())
        });
        if let Err(error) = transaction {
            self.release_capacity(&successor_id);
            // `WorkflowStore::transaction` 会把闭包错误包成「工作流状态库操作失败: 其它: …」，
            // 哨兵只能按包含判断。
            if error.contains("predecessor_changed") {
                let current = self.inner.store.get_run(run_id).ok().flatten();
                let code = if current
                    .as_ref()
                    .and_then(|row| row.superseded_by.as_ref())
                    .is_some()
                {
                    "already_superseded"
                } else {
                    "invalid_state"
                };
                return Err(WorkflowError::new(
                    code,
                    "state",
                    "前驱状态已变化，请重新读取",
                ));
            }
            return Err(WorkflowError::new("internal", "internal", error));
        }
        if predecessor_running {
            let had_driver = self.is_active(run_id);
            if let Some(token) = self.cancel_token(run_id) {
                token.cancel();
            }
            if !had_driver {
                // hole 等待 / driver 已退出：没有引擎可确认退出，直接把前驱收口。
                let event = json!({
                    "event": "run_stopped",
                    "exec_generation": predecessor.exec_generation,
                    "reason": STOP_SUPERSEDED,
                    "stop_origin": STOP_SUPERSEDED,
                    "requires_fresh_auth": 0,
                    "detail": "superseded without active driver",
                });
                let _ = self.terminal_transaction(
                    &predecessor,
                    STATUS_STOPPED,
                    &event,
                    Some(STOP_SUPERSEDED),
                    Some(STOP_SUPERSEDED),
                    false,
                    Some(true),
                    None,
                );
                self.transfer_capacity(run_id, &successor_id);
                if self.inner.opts.background {
                    self.start_driver(&successor_id, 1, Some(run_id));
                    return Ok(response);
                }
                let outcome =
                    super::workflow_driver::run_sync(self.clone(), &successor_id, 1, Some(run_id))
                        .await;
                self.update_request_response(&scope, &request_id, &outcome);
                return Ok(outcome);
            }
            if !self.inner.opts.background {
                // 同步模式（§13.8）：同一调用内等前驱退出（10s）再跑后继。等待期间工具 future
                // 被丢弃（父 turn /cancel）同样要回滚：否则 queued 后继被持久化、前驱终态时还
                // 把许可交接给它，而同步模式没有任何执行者会来启动它。
                let mut guard = SyncAmendGuard {
                    service: self.clone(),
                    predecessor: run_id.to_string(),
                    successor: successor.clone(),
                    scope: scope.to_string(),
                    request_id: request_id.to_string(),
                    armed: true,
                };
                for _ in 0..100 {
                    match self.inner.store.get_run(run_id) {
                        Ok(Some(row)) if row.is_terminal() => break,
                        _ => tokio::time::sleep(Duration::from_millis(100)).await,
                    }
                }
                let predecessor_terminal = self
                    .inner
                    .store
                    .get_run(run_id)
                    .ok()
                    .flatten()
                    .map(|row| row.is_terminal())
                    .unwrap_or(false);
                guard.armed = false;
                if !predecessor_terminal {
                    // §13.8 同步模式不持久化 queued：没有后台 driver 会来启动这个后继，留着
                    // 就是永远 queued 且前驱 lineage 死锁（already_superseded）。回滚：后继收口
                    // 为 stopped(interrupted)、撤销前驱的 superseded 标记、幂等记录置 failed，
                    // 前驱停下后同一 request_id 重试即可（模式 B 直接启动），也可 Resume 前驱。
                    self.rollback_sync_successor(run_id, &successor, &scope, &request_id);
                    return Err(WorkflowError {
                        code: "stop_pending".to_string(),
                        message: "前驱尚未确认退出；本次 Amend 已回滚".to_string(),
                        kind: "state".to_string(),
                        retryable: true,
                        recovery: "Wait for the predecessor to stop, then retry Amend.".to_string(),
                    });
                }
                self.transfer_capacity(run_id, &successor_id);
                let outcome =
                    super::workflow_driver::run_sync(self.clone(), &successor_id, 1, Some(run_id))
                        .await;
                self.update_request_response(&scope, &request_id, &outcome);
                return Ok(outcome);
            }
            let service = self.clone();
            let successor_id = successor_id.clone();
            let predecessor_id = run_id.to_string();
            tokio::spawn(async move {
                // 前驱退出后交接许可并启动后继；超时保持 queued，由 driver 终态回调补启。
                for _ in 0..100 {
                    if let Ok(Some(row)) = service.inner.store.get_run(&successor_id) {
                        if row.status != STATUS_QUEUED {
                            return;
                        }
                    }
                    if let Ok(Some(predecessor)) = service.inner.store.get_run(&predecessor_id) {
                        if predecessor.is_terminal() {
                            service.transfer_capacity(&predecessor_id, &successor_id);
                            service.start_driver(&successor_id, 1, Some(&predecessor_id));
                            return;
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            });
        } else if self.inner.opts.background {
            self.start_driver(&successor_id, 1, Some(run_id));
        } else {
            let outcome =
                super::workflow_driver::run_sync(self.clone(), &successor_id, 1, Some(run_id))
                    .await;
            self.update_request_response(&scope, &request_id, &outcome);
            return Ok(outcome);
        }
        Ok(response)
    }

    fn issue_auth_token(&self, run_id: &str) -> String {
        let token = format!(
            "wf-auth-{:016x}{:016x}",
            rand::random::<u64>(),
            rand::random::<u64>()
        );
        let _ = self.inner.runtime.journal_store().kv_set(
            &format!("workflow-auth/{run_id}"),
            &json!({
                "token": token,
                "run_id": run_id,
                "issued_at": rclaw_store::db::now_ms(),
            }),
        );
        token
    }

    /// UI 代签：只给 `requires_fresh_auth=1` 的 run 签（其它 run 不需要 token，返回 None），
    /// 且 run 必须属于本项目。只有桌面 `workflow_api` 调用，工具面不可达。
    pub(crate) fn issue_auth_token_for_ui(
        &self,
        run_id: &str,
    ) -> Result<Option<String>, WorkflowError> {
        let row =
            self.load_run_checked(run_id, ToolAccess::Unrestricted, "desktop", "console", true)?;
        Ok(row
            .requires_fresh_auth
            .then(|| self.issue_auth_token(run_id)))
    }

    /// 一次性消费 UI 签发的 auth_token；模型无法生成（不在工具响应里）。
    fn consume_auth_token(&self, run_id: &str, token: Option<&str>) -> bool {
        let Some(token) = token.filter(|token| !token.trim().is_empty()) else {
            return false;
        };
        let key = format!("workflow-auth/{run_id}");
        let stored = self
            .inner
            .runtime
            .journal_store()
            .kv_get(&key)
            .ok()
            .flatten();
        let Some(stored) = stored else {
            return false;
        };
        let matches = stored.get("token").and_then(Value::as_str) == Some(token)
            && stored.get("run_id").and_then(Value::as_str) == Some(run_id);
        if matches {
            let _ = self.inner.runtime.journal_store().kv_delete(&key);
        }
        matches
    }

    /// 接收端认领通知：返回 false 表示这条已经被消费过（重发到达的副本），调用方应整条丢弃。
    /// 落库失败按「可消费」处理——宁可模型多看一次，也不能把一条真实的终态通知吞掉。
    pub(crate) fn mark_notification_consumed(&self, notification_id: &str) -> bool {
        match self.inner.store.mark_notification_consumed(notification_id) {
            Ok(claimed) => claimed,
            Err(error) => {
                tracing::warn!("确认工作流通知消费失败: {error}");
                true
            }
        }
    }

    /// 认领后没能把通知消息落进会话：退回 pending 让 outbox 重发。
    pub(crate) fn release_notification(&self, notification_id: &str, error: &str) {
        if let Err(error) = self
            .inner
            .store
            .release_notification(notification_id, error)
        {
            tracing::warn!("退回工作流通知失败: {error}");
        }
    }

    fn update_request_response(&self, scope: &str, request_id: &str, response: &Value) {
        let text = response.to_string();
        let _ = self.inner.store.transaction(|tx| {
            tx.execute(
                "UPDATE workflow_request SET response = ?3, updated_at = ?4                  WHERE scope = ?1 AND request_id = ?2",
                rusqlite::params![scope, request_id, text, rclaw_store::db::now_ms()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        });
    }

    fn confirmed_amend(&self, run_id: &str) -> bool {
        self.inner
            .runtime
            .journal_store()
            .kv_get(&format!("workflow-amend-confirm/{run_id}"))
            .ok()
            .flatten()
            .and_then(|value| value.get("confirmed").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    // -----------------------------------------------------------------------
    // Resume
    // -----------------------------------------------------------------------

    pub(crate) async fn resume(
        &self,
        run_id: &str,
        request_id: Option<String>,
        auth_token: Option<String>,
        channel: String,
        chat_id: String,
        tool_access: ToolAccess,
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let row = self.load_run_checked(run_id, tool_access, &channel, &chat_id, true)?;
        // 幂等优先：已成功请求的重试直接返回原响应，不被当前状态变化挡住。
        let request_id = normalize_request_id(request_id)?;
        let scope = format!(
            "{}/ResumeWorkflowRun",
            self.inner.paths.project_root.to_string_lossy()
        );
        let digest = digest_json(&json!({"run_id": run_id}));
        if let Some(existing) = self
            .inner
            .store
            .get_request(&scope, &request_id)
            .map_err(internal_error)?
        {
            if existing.digest != digest {
                return Err(WorkflowError::new(
                    "request_conflict",
                    "validation",
                    "同一 request_id 对应不同 resume 请求",
                ));
            }
            if existing.state == "done" {
                if let Some(response) = existing.response {
                    return Ok(serde_json::from_str(&response).unwrap_or(Value::Null));
                }
            }
        }
        if row.superseded_by.is_some() || row.stop_reason.as_deref() == Some(STOP_SUPERSEDED) {
            return Err(WorkflowError::new(
                "superseded",
                "state",
                format!("该 run 已被 {:?} 取代", row.superseded_by),
            ));
        }
        if row.status == STATUS_ERRORED {
            return Err(WorkflowError::new(
                "not_resumable",
                "state",
                "errored 不可续跑；请用 AmendWorkflow 修订",
            ));
        }
        if row.status != STATUS_STOPPED {
            return Err(WorkflowError::invalid_state(
                "只有 stopped 的 run 才能 resume",
            ));
        }
        if row.requires_fresh_auth && !self.consume_auth_token(run_id, auth_token.as_deref()) {
            return Err(WorkflowError::new(
                "fresh_auth_required",
                "permission",
                "用户取消过的 run 需要 UI 签发的一次性 auth_token",
            ));
        }
        let next_generation = row.exec_generation.saturating_add(1);
        let blocked = self
            .inner
            .store
            .blocking_effects(run_id, next_generation)
            .map_err(internal_error)?;
        if !blocked.is_empty() {
            return Err(WorkflowError::new(
                "resume_blocked_effect",
                "state",
                format!(
                    "存在 {} 个未裁决副作用：{}",
                    blocked.len(),
                    blocked
                        .iter()
                        .map(|effect| effect.effect_key.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        let snapshot = resolve_snapshot(&self.inner.paths, &row)?;
        workflow_snapshot::read_snapshot(&snapshot, &row.snapshot_hash).map_err(snapshot_error)?;
        if !self.acquire_capacity(run_id)? {
            return Err(WorkflowError::new(
                "capacity_exceeded",
                "capacity",
                "同时运行的工作流已达上限",
            ));
        }
        let _ = self.inner.store.transaction(|tx| {
            tx.execute(
                "INSERT OR REPLACE INTO workflow_request (scope, request_id, digest, state, run_id,                  response, created_at, updated_at) VALUES (?1, ?2, ?3, 'done', ?4, ?5, ?6, ?6)",
                rusqlite::params![
                    scope,
                    request_id,
                    digest,
                    run_id,
                    json!({
                        "status": "backgrounded",
                        "run_id": run_id,
                        "exec_generation": next_generation,
                        "request_id": request_id,
                    })
                    .to_string(),
                    rclaw_store::db::now_ms()
                ],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        });
        if self.inner.opts.background {
            self.start_driver(run_id, next_generation, None);
            Ok(json!({
                "status": "backgrounded",
                "run_id": run_id,
                "exec_generation": next_generation,
                "request_id": request_id,
                "note": "Use GetWorkflowRun for progress.",
            }))
        } else {
            let outcome =
                super::workflow_driver::run_sync(self.clone(), run_id, next_generation, None).await;
            self.update_request_response(&scope, &request_id, &outcome);
            Ok(outcome)
        }
    }

    // -----------------------------------------------------------------------
    // Cancel
    // -----------------------------------------------------------------------

    pub(crate) async fn cancel(
        &self,
        run_id: &str,
        reason: &str,
        channel: String,
        chat_id: String,
        tool_access: ToolAccess,
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let mut row = self.load_run_checked(run_id, tool_access, &channel, &chat_id, true)?;
        let origin = if reason == "user" {
            STOP_USER
        } else {
            STOP_MODEL
        };
        if row.status == STATUS_QUEUED {
            // queued 直接终结，但要 CAS：读行之后后继 driver 可能已把它推成 running（前驱刚退
            // 出、交接启动），无条件写 stopped 会让引擎在"已停止"的行上跑完全部副作用、名额
            // 还少计一个。围栏不满足就按当前状态（running）走下面的取消路径。
            let event = json!({
                "event": "run_stopped",
                "exec_generation": row.exec_generation,
                "reason": origin,
                "stop_origin": origin,
                "requires_fresh_auth": (origin == STOP_USER) as i64,
                "detail": "queued run cancelled",
            });
            let stopped = self
                .terminal_transaction_if(
                    &row,
                    Some(STATUS_QUEUED),
                    STATUS_STOPPED,
                    &event,
                    Some(origin),
                    Some(origin),
                    origin == STOP_USER,
                    Some(true),
                    None,
                )
                .map_err(internal_error)?;
            if stopped {
                self.inner
                    .store
                    .set_capacity_claim(run_id, None)
                    .map_err(internal_error)?;
                self.release_capacity(run_id);
                let auth_token = (origin == STOP_USER).then(|| self.issue_auth_token(run_id));
                return Ok(json!({
                    "status":"stopped",
                    "run_id":run_id,
                    "stop_reason":origin,
                    "auth_token": auth_token,
                }));
            }
            row = self
                .inner
                .store
                .get_run(run_id)
                .map_err(internal_error)?
                .ok_or_else(|| WorkflowError::new("not_found", "state", "run 不存在"))?;
        }
        match row.status.as_str() {
            STATUS_QUEUED => Err(WorkflowError::invalid_state("run 状态已变化，请重试取消")),
            STATUS_RUNNING => {
                // CAS running→stopping 与 journal 事件同事务：读行之后 driver 可能已写了
                // completed/errored，无条件 UPDATE 会把终态覆盖成 stopping 再被下面的无 driver
                // 分支改写成 stopped。
                let transitioned = self
                    .inner
                    .store
                    .begin_stopping(run_id, row.exec_generation, origin, origin)
                    .map_err(internal_error)?;
                if !transitioned {
                    return Err(WorkflowError::new(
                        "not_running",
                        "state",
                        "run 已不在 running（刚结束或正在停止），无需取消",
                    ));
                }
                if let Some(token) = self.cancel_token(run_id) {
                    token.cancel();
                }
                let auth_token = (origin == STOP_USER).then(|| self.issue_auth_token(run_id));
                if !self.is_active(run_id) {
                    // hole 等待 / driver 已退出：没有执行者可确认，直接收口。
                    let event = json!({
                        "event": "run_stopped",
                        "exec_generation": row.exec_generation,
                        "reason": origin,
                        "stop_origin": origin,
                        "requires_fresh_auth": (origin == STOP_USER) as i64,
                        "detail": "no active driver",
                    });
                    self.terminal_transaction(
                        &row,
                        STATUS_STOPPED,
                        &event,
                        Some(origin),
                        Some(origin),
                        origin == STOP_USER,
                        Some(true),
                        None,
                    )
                    .map_err(internal_error)?;
                    self.start_claimed_successor(run_id);
                    return Ok(json!({
                        "status": "stopped",
                        "run_id": run_id,
                        "stop_reason": origin,
                        "auth_token": auth_token,
                    }));
                }
                for _ in 0..100 {
                    if let Ok(Some(current)) = self.inner.store.get_run(run_id) {
                        if current.status != STATUS_STOPPING {
                            return Ok(json!({
                                "status": "stopped",
                                "run_id": run_id,
                                "stop_reason": origin,
                                "auth_token": auth_token,
                            }));
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(WorkflowError {
                    code: "stop_pending".to_string(),
                    message: "取消已发出，引擎尚未确认退出".to_string(),
                    kind: "state".to_string(),
                    retryable: true,
                    recovery: "Wait for the stopped notification, then GetWorkflowRun.".to_string(),
                })
            }
            STATUS_STOPPING => Err(WorkflowError::invalid_state("run 正在停止，不能重复取消")),
            _ => Err(WorkflowError::new(
                "not_running",
                "state",
                "run 已结束，无需取消",
            )),
        }
    }

    // -----------------------------------------------------------------------
    // FillHole
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn fill_hole(
        &self,
        run_id: &str,
        hole_id: &str,
        expected_script_generation: u32,
        body: String,
        channel: String,
        chat_id: String,
        tool_access: ToolAccess,
        skill_gate: bool,
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        if !self.inner.opts.background {
            return Err(WorkflowError::new(
                "background_required",
                "state",
                "hole 只能在后台模式使用",
            ));
        }
        // 路由矩阵（文档 §5.0）：模型经工具面 Fill 提交的是新脚本正文，同样过技能门；桌面
        // 弹窗（`workflow_api`）是人工补全，desktop/console 会话没有技能登记，不设门。
        if skill_gate {
            self.check_skill_gate(&session_key(&channel, &chat_id))?;
        }
        let row = self.load_run_checked(run_id, tool_access, &channel, &chat_id, true)?;
        if row.status != STATUS_RUNNING {
            return Err(WorkflowError::invalid_state("run 不在 running"));
        }
        let blocked = self
            .inner
            .store
            .blocking_effects(run_id, row.exec_generation.saturating_add(1))
            .map_err(internal_error)?;
        if !blocked.is_empty() {
            return Err(WorkflowError::new(
                "resume_blocked_effect",
                "state",
                format!("存在 {} 个未裁决副作用，不能补洞", blocked.len()),
            ));
        }
        let hole = self
            .inner
            .store
            .get_hole(run_id, hole_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::new("hole_not_found", "not_found", "未知 hole"))?;
        if hole.state != "waiting" {
            return Err(WorkflowError::new(
                "hole_not_waiting",
                "state",
                format!("hole 状态为 {}", hole.state),
            ));
        }
        if hole.script_generation != expected_script_generation {
            return Err(WorkflowError::new(
                "generation_mismatch",
                "state",
                "expected_script_generation 与当前代次不符",
            ));
        }
        let body_hash = workflow_snapshot::hash_bytes(body.trim().as_bytes());
        if hole.last_rejected_hash.as_deref() == Some(body_hash.as_str()) {
            return Err(WorkflowError::new(
                "fill_unchanged",
                "validation",
                "与上次被拒正文相同",
            ));
        }
        let snapshot = resolve_snapshot(&self.inner.paths, &row)?;
        let script = workflow_snapshot::read_snapshot(&snapshot, &row.snapshot_hash)
            .map_err(snapshot_error)?;
        let call_sites = super::workflow_saved::find_hole_calls(&script);
        // hole_id = hole#<safe_name>-<script_generation>-<seq>：seq 是 prepare_script 注入的
        // 全局源码序号（0 起），按它定位并核对同名（与 host_hole 同一口径的安全名）；只有
        // 未注入序号的调用点才退回“同名第几个”（1 起）。不做跨名字的模糊回退——打错调用点
        // 比报错严重得多。
        let (site_name, hole_seq) = parse_hole_id(hole_id)
            .unwrap_or_else(|| (super::workflow::safe_hole_name(&hole.name), 0));
        let site = call_sites
            .get(hole_seq as usize)
            .filter(|site| super::workflow::safe_hole_name(&site.name) == site_name)
            .cloned()
            .or_else(|| {
                call_sites
                    .iter()
                    .filter(|site| super::workflow::safe_hole_name(&site.name) == site_name)
                    .nth((hole_seq as usize).saturating_sub(1))
                    .cloned()
            })
            .ok_or_else(|| {
                WorkflowError::new("hole_not_found", "state", "脚本中找不到 hole 调用点")
            })?;
        let replacement = format!("(async function () {{\n{}\n}})()", body.trim());
        let new_script = format!(
            "{}{}{}",
            &script[..site.start],
            replacement,
            &script[site.end..]
        );
        // 预检必须在停旧 exec 之前（文档 §5.6）：编译 / type_check 按 run 的配置，正文里的
        // 模型必须都在 execution_config.allowed_models 内；任一失败 hole 保持 waiting、只记
        // last_rejected_hash，旧 exec 继续跑。
        let config = row.execution_config();
        let type_check = config
            .get("type_check")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let allowed_models: Vec<String> = config
            .get("allowed_models")
            .and_then(Value::as_array)
            .map(|models| {
                models
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        let unbound: Vec<String> = super::workflow_saved::model_literals(body.trim())
            .into_iter()
            .filter(|model| !allowed_models.iter().any(|allowed| allowed == model))
            .collect();
        if !unbound.is_empty() {
            let _ = self
                .inner
                .store
                .set_hole_rejected(run_id, hole_id, &body_hash);
            return Err(WorkflowError::new(
                "model_not_allowed",
                "validation",
                format!(
                    "正文使用了不在 execution_config.allowed_models 内的模型: {}（先用 AmendWorkflow 调参加入）",
                    unbound.join(", ")
                ),
            ));
        }
        let prepared = super::workflow::prepare_script(&new_script, type_check)
            .await
            .map_err(|error| {
                let _ = self
                    .inner
                    .store
                    .set_hole_rejected(run_id, hole_id, &body_hash);
                WorkflowError::new("script_diagnostics", "validation", error)
            })?;
        if !self
            .inner
            .store
            .cas_hole_filling(run_id, hole_id, expected_script_generation)
            .map_err(internal_error)?
        {
            return Err(WorkflowError::new(
                "hole_not_waiting",
                "state",
                "hole 已不在等待状态",
            ));
        }
        // 从 CAS 到最终事务提交之间，任何未提交的退出（回退 / 出错 / 本 future 被父 turn 取消
        // drop 掉）都由守卫把 hole 放回 waiting（CAS，已被 Cancel 置 aborted 的不动），否则
        // hole 永远卡在 filling、run 留在 running 却没有 driver。
        let mut fill_guard = FillGuard {
            store: self.inner.store.clone(),
            run_id: run_id.to_string(),
            hole_id: hole_id.to_string(),
            armed: true,
        };
        if let Some(token) = self.cancel_token(run_id) {
            token.cancel();
        }
        // 等旧 driver 退出（ActiveRun 移除）：到达 hole 后其它分支 / 在途 ask 可能还在跑，
        // 取消令牌会让新 ask 被拒、在途子代理联动取消，一般数秒内静止；30s 仍未退出 =
        // 回退 waiting 让调用方稍后重试。
        for _ in 0..600 {
            if !self.is_active(run_id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if self.is_active(run_id) {
            return Err(WorkflowError::new(
                "fill_aborted",
                "state",
                "旧 exec 尚未确认退出，hole 回到 waiting",
            ));
        }
        let current = self
            .inner
            .store
            .get_run(run_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::not_found(run_id))?;
        if current.status != STATUS_RUNNING {
            fill_guard.armed = false;
            let _ = self.inner.store.set_hole_state(run_id, hole_id, "aborted");
            return Err(WorkflowError::new(
                "fill_aborted",
                "state",
                "run 在补洞期间已取消 / 停止",
            ));
        }
        let next_script_generation = expected_script_generation.saturating_add(1);
        let next_exec_generation = row.exec_generation.saturating_add(1);
        let (new_snapshot, new_hash) = workflow_snapshot::write_snapshot(
            &self.inner.paths,
            run_id,
            next_script_generation,
            &prepared.script,
        )
        .map_err(|error| WorkflowError::new("internal", "internal", error))?;
        let display = workflow_snapshot::display_path(&self.inner.paths, &new_snapshot);
        let journal = EventLog::workflow(self.inner.store.journal(), run_id);
        let store = self.inner.store.clone();
        let run_id_owned = run_id.to_string();
        let hole_id_owned = hole_id.to_string();
        let display_clone = display.clone();
        let expected_generation = expected_script_generation as i64;
        let transaction = store.transaction(|tx| {
            let (status, generation): (String, i64) = tx
                .query_row(
                    "SELECT status, script_generation FROM workflow_run WHERE run_id = ?1",
                    rusqlite::params![&run_id_owned],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .map_err(|e| e.to_string())?;
            if status != "running" || generation != expected_generation {
                return Err("fill_aborted_run_changed".to_string());
            }
            let hole_state: String = tx
                .query_row(
                    "SELECT state FROM workflow_hole WHERE run_id = ?1 AND hole_id = ?2",
                    rusqlite::params![&run_id_owned, &hole_id_owned],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if hole_state != "filling" {
                return Err("fill_aborted_hole_changed".to_string());
            }
            tx.execute(
                "UPDATE workflow_run SET script_generation = ?2, exec_generation = ?3, \
                 snapshot_path = ?4, snapshot_hash = ?5, updated_at = ?6 WHERE run_id = ?1",
                rusqlite::params![
                    run_id_owned,
                    next_script_generation as i64,
                    next_exec_generation as i64,
                    display_clone,
                    new_hash,
                    rclaw_store::db::now_ms()
                ],
            )
            .map_err(|e| e.to_string())?;
            workflow_store::insert_script_version_tx(
                tx,
                &run_id_owned,
                next_script_generation,
                &display_clone,
                &new_hash,
                Some(&row.snapshot_hash),
                "hole_fill",
                true,
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE workflow_hole SET state = 'filled', updated_at = ?3 WHERE run_id = ?1 AND hole_id = ?2",
                rusqlite::params![run_id_owned, hole_id_owned, rclaw_store::db::now_ms()],
            )
            .map_err(|e| e.to_string())?;
            // 同一代里其它仍在等待的 hole（并行分支各停一处）随脚本代次切换一并失效：新 exec
            // 重放到那里会以新代 hole_id 重新打开并再次通知；旧 id 再 Fill 一律 hole_not_waiting。
            let stale: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT hole_id FROM workflow_hole WHERE run_id = ?1 AND hole_id <> ?2 \
                         AND state IN ('waiting', 'filling')",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![run_id_owned, hole_id_owned], |row| {
                        row.get::<_, String>(0)
                    })
                    .map_err(|e| e.to_string())?;
                rows.collect::<std::result::Result<Vec<_>, _>>()
                    .map_err(|e| e.to_string())?
            };
            for stale_id in stale {
                journal
                    .append_tx(
                        tx,
                        &json!({
                            "event": "hole_aborted",
                            "hole_id": stale_id,
                            "reason": "script_generation_changed",
                        }),
                    )
                    .map_err(|e| e.to_string())?;
                tx.execute(
                    "UPDATE workflow_hole SET state = 'aborted', updated_at = ?3 \
                     WHERE run_id = ?1 AND hole_id = ?2",
                    rusqlite::params![run_id_owned, stale_id, rclaw_store::db::now_ms()],
                )
                .map_err(|e| e.to_string())?;
            }
            // 代次切换：旧代 pending 问题一律失效。
            tx.execute(
                "UPDATE workflow_question SET state = 'aborted' WHERE run_id = ?1                  AND state = 'pending' AND exec_generation <> ?2",
                rusqlite::params![run_id_owned, next_exec_generation as i64],
            )
            .map_err(|e| e.to_string())?;
            journal
                .append_tx(
                    tx,
                    &json!({
                        "event": "hole_filled",
                        "hole_id": hole_id_owned,
                        "script_generation": next_script_generation,
                        "fill_hash": body_hash,
                        "snapshot_path": display_clone,
                        "snapshot_hash": new_hash,
                    }),
                )
                .map_err(|e| e.to_string())?;
            Ok(())
        });
        if let Err(error) = transaction {
            if error.contains("fill_aborted") {
                // run / hole 已被 Cancel、Amend 改走：hole 跟着作废。
                fill_guard.armed = false;
                let _ = self.inner.store.set_hole_state(run_id, hole_id, "aborted");
                return Err(WorkflowError::new(
                    "fill_aborted",
                    "state",
                    "run / hole 状态已变化",
                ));
            }
            // 事务本身失败（什么都没提交）：按 §3.5.7 由守卫放回 waiting 让调用方重试，置
            // aborted 会让 run 留在 running 却没有 driver、也没法再填。
            return Err(WorkflowError::new("internal", "internal", error));
        }
        fill_guard.armed = false;
        self.start_driver(run_id, next_exec_generation, None);
        Ok(json!({
            "status": "filled",
            "run_id": run_id,
            "hole_id": hole_id,
            "script_generation": next_script_generation,
            "exec_generation": next_exec_generation,
            "snapshot_path": display,
            "phases_added": [],
        }))
    }

    // -----------------------------------------------------------------------
    // List / Get
    // -----------------------------------------------------------------------

    pub(crate) fn list(
        &self,
        limit: usize,
        statuses: Vec<String>,
        include_legacy: bool,
        channel: &str,
        chat_id: &str,
        tool_access: ToolAccess,
    ) -> Result<Value, WorkflowError> {
        let session = session_key(channel, chat_id);
        let cwd = self.inner.paths.project_key.clone();
        let rows = self
            .inner
            .store
            .list_runs(
                &cwd,
                limit,
                &statuses,
                include_legacy && tool_access == ToolAccess::Unrestricted,
            )
            .map_err(internal_error)?;
        let now = rclaw_store::db::now_ms();
        let mut filtered = 0usize;
        let runs: Vec<Value> = rows
            .into_iter()
            .filter_map(|row| {
                let config = row.execution_config();
                let snapshot_access = access_from_config(&config);
                if tool_access < snapshot_access {
                    filtered += 1;
                    return None;
                }
                Some(json!({
                    "run_id": row.run_id,
                    "label": row.label,
                    "status": row.status,
                    "stop_reason": row.stop_reason,
                    "resumed_from": row.resumed_from,
                    "superseded_by": row.superseded_by,
                    "owned_by_this_session": row.session_key == session,
                    "possibly_interrupted": row.status == STATUS_RUNNING && now - row.heartbeat_at > 45_000,
                    "legacy": row.legacy,
                    "resumable": row.resumable && row.superseded_by.is_none(),
                    "created_at": row.created_at,
                    "updated_at": row.updated_at,
                    "spent_tokens": row.spent_tokens,
                }))
            })
            .collect();
        Ok(json!({"runs": runs, "truncated": filtered > 0, "filtered": filtered}))
    }

    /// 从 answer / ask_replayed 事件收集未解决错误披露（去重、保序）。
    fn collect_unresolved_disclosure(event: &Value, out: &mut Vec<String>) {
        let disclosure = event
            .get("unresolved_disclosure")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !disclosure.is_empty() && !out.iter().any(|seen| seen == disclosure) {
            out.push(disclosure.to_string());
        }
    }

    pub(crate) fn get(
        &self,
        run_id: &str,
        options: GetOptions,
        channel: &str,
        chat_id: &str,
        tool_access: ToolAccess,
    ) -> Result<Value, WorkflowError> {
        let GetOptions {
            include_legacy,
            include_effects,
            reports_cursor,
            effects_cursor,
            artifacts_cursor,
            log_cursor,
        } = options;
        let row = self.load_run_checked(run_id, tool_access, channel, chat_id, false)?;
        if row.legacy && !(include_legacy && tool_access == ToolAccess::Unrestricted) {
            return Err(WorkflowError::new(
                "forbidden_project",
                "permission",
                "legacy run 需要 Unrestricted + include_legacy",
            ));
        }
        let journal = EventLog::workflow(self.inner.store.journal(), run_id);
        let events = journal.load().map_err(internal_error)?;
        let mut logs: Vec<Value> = Vec::new();
        // report 按 seq 去重：resume / fill 后脚本从头重跑，`report()` 调用会以同样的 seq 再
        // 记一遍（ask 走重放，seq 稳定），投影只保留每个 seq 最后一代写入的内容。
        let mut reports: BTreeMap<u64, Value> = BTreeMap::new();
        let mut phase_events: Vec<(u32, String, i64)> = Vec::new();
        let mut artifacts: Vec<Value> = Vec::new();
        let mut artifact_seen: HashSet<(String, u32)> = HashSet::new();
        let mut ask_total = 0u64;
        let mut ask_done = 0u64;
        let mut ask_replayed = 0u64;
        let mut active_agents = 0usize;
        let mut completed_result: Option<Value> = None;
        let mut failure: Option<Value> = None;
        // ask 子代理的未解决错误披露（answer / ask_replayed 事件带独立字段，去重保序）。
        let mut unresolved_disclosures: Vec<String> = Vec::new();
        for (_, event) in &events {
            match event.get("event").and_then(Value::as_str) {
                // 每次执行（首跑 / resume / fill）都以生命周期事件开头，脚本从头重跑：ask 计数
                // 只统计当前这一代，否则 resume 后 asks_total 翻倍、上一代未应答的 ask 还会
                // 被算成在途子代理。
                Some("run_started") | Some("run_resumed") => {
                    ask_total = 0;
                    ask_done = 0;
                    ask_replayed = 0;
                    active_agents = 0;
                }
                Some("ask") => {
                    ask_total += 1;
                    active_agents += 1;
                }
                Some("answer") => {
                    ask_done += 1;
                    active_agents = active_agents.saturating_sub(1);
                    Self::collect_unresolved_disclosure(event, &mut unresolved_disclosures);
                }
                Some("ask_replayed") => {
                    ask_total += 1;
                    ask_done += 1;
                    ask_replayed += 1;
                    Self::collect_unresolved_disclosure(event, &mut unresolved_disclosures);
                }
                Some("log") | Some("log_truncated") | Some("phase_truncated") => {
                    logs.push(json!({
                        "message": event.get("message").and_then(Value::as_str).unwrap_or_default(),
                    }));
                }
                Some("report") => {
                    let seq = event.get("seq").and_then(Value::as_u64).unwrap_or(0);
                    reports.insert(
                        seq,
                        json!({
                            "seq": seq,
                            "item_preview": truncate_json(event.get("item").cloned().unwrap_or(Value::Null), 2_000),
                        }),
                    );
                }
                Some("phase") => {
                    let name = event
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let at = event.get("at").and_then(Value::as_i64).unwrap_or(0);
                    let generation = event
                        .get("exec_generation")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as u32;
                    phase_events.push((generation, name, at));
                }
                Some("artifact") => {
                    let id = event
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let version = event.get("version").and_then(Value::as_u64).unwrap_or(1) as u32;
                    if artifact_seen.insert((id.clone(), version)) {
                        artifacts.push(json!({
                            "id": id,
                            "kind": event.get("kind").and_then(Value::as_str).unwrap_or_default(),
                            "title": event.get("title").and_then(Value::as_str).unwrap_or_default(),
                            "version": version,
                            "primary": event.get("primary").and_then(Value::as_bool).unwrap_or(false),
                            "uri": event.get("path").and_then(Value::as_str).unwrap_or_default(),
                        }));
                    }
                }
                Some("run_completed") => {
                    completed_result = event.get("result_json").cloned();
                    // 完成事件里的披露数组是兜底账本：answer 事件经 append_lossy 写入
                    // 失败时脚本仍可成功完成，投影必须合并这里已持久化的披露
                    // （codex 第 8 轮 P2）。
                    if let Some(items) = event
                        .get("unresolved_disclosures")
                        .and_then(Value::as_array)
                    {
                        for disclosure in items.iter().filter_map(Value::as_str) {
                            if !disclosure.is_empty()
                                && !unresolved_disclosures.iter().any(|seen| seen == disclosure)
                            {
                                unresolved_disclosures.push(disclosure.to_string());
                            }
                        }
                    }
                }
                Some("run_failed") => {
                    failure = event.get("error").cloned();
                }
                _ => {}
            }
        }
        let questions = self
            .inner
            .store
            .pending_questions(run_id, 50)
            .map_err(internal_error)?;
        let holes = self
            .inner
            .store
            .list_holes(run_id)
            .map_err(internal_error)?;
        let notifications = self
            .inner
            .store
            .list_notifications(run_id, 50)
            .map_err(internal_error)?;
        // 当前 exec 的进程树记录：`stopping(interrupted)` 要靠 UI 用这些 process_id 做人工确认
        // （Windows 没有 OS 侧探测），没有它们卡片连参数都拼不出来。
        let processes = self
            .inner
            .store
            .list_processes(run_id, row.exec_generation)
            .map_err(internal_error)?;
        let effects_all = if include_effects {
            self.inner
                .store
                .list_effects(run_id)
                .map_err(internal_error)?
                .into_iter()
                .map(|effect| {
                    json!({
                        "effect_key": effect.effect_key,
                        "op": effect.op,
                        "is_write": effect.is_write,
                        "status": effect.status,
                        "args_preview": effect.args_preview,
                        "result_preview": truncate_json(
                            effect.result.as_deref()
                                .and_then(|text| serde_json::from_str(text).ok())
                                .unwrap_or(Value::Null),
                            2_000
                        ),
                        "decision": effect.decision,
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let result_text = row.result_json.clone().or_else(|| {
            completed_result.map(|value| match value {
                Value::String(text) => text,
                other => other.to_string(),
            })
        });
        let reports_all: Vec<Value> = reports.into_values().collect();
        let phases = project_phases(&phase_events, &row.status);
        let error = failure
            .map(|value| truncate_json(value, 4_000))
            .or_else(|| {
                row.error
                    .as_deref()
                    .map(|text| json!(truncate_chars(text, 4_000)))
            });
        let pending_questions = questions
            .iter()
            .map(|question| {
                json!({
                    "question_id": question.question_id,
                    "actor": question.actor,
                    "question": truncate_chars(&question.question, 2_000),
                })
            })
            .collect::<Vec<_>>();
        let holes = holes
            .iter()
            .take(50)
            .map(|hole| {
                json!({
                    "hole_id": hole.hole_id,
                    "name": hole.name,
                    "prompt": hole.prompt.as_deref().map(|prompt| truncate_chars(prompt, 2_000)),
                    "state": hole.state,
                    "script_generation": hole.script_generation,
                })
            })
            .collect::<Vec<_>>();
        let notifications = notifications
            .iter()
            .map(|notification| {
                json!({
                    "id": notification.id,
                    "state": notification.state,
                    "attempts": notification.attempts,
                    "last_error": notification.last_error,
                })
            })
            .collect::<Vec<_>>();
        let processes = processes
            .iter()
            .map(|(process_id, root_pid, state)| {
                json!({
                    "process_id": process_id,
                    "root_pid": root_pid,
                    "state": state,
                })
            })
            .collect::<Vec<_>>();
        // log_tail：默认最后 LOG_PAGE 条；`log_cursor` 指向更早一页的结束下标（续页向前翻）。
        let log_end = log_cursor.unwrap_or(logs.len()).min(logs.len());
        // 统一截断顺序（§5.5）：先缩 reports / artifacts 页并给 cursor，再缩 log_tail，最后把
        // result 转成句柄；每一步重新量总长度，保证正文 ≤ GET_BUDGET_CHARS。
        let mut page = PageSizes {
            reports: GET_PAGE,
            effects: GET_PAGE,
            artifacts: GET_ARTIFACTS_MAX,
            log: LOG_PAGE,
            spill_result: false,
        };
        let mut result_handle_cache: Option<Value> = None;
        loop {
            let (reports_page, reports_next) = paginate(&reports_all, reports_cursor, page.reports);
            let (artifacts_page, artifacts_next) =
                paginate(&artifacts, artifacts_cursor, page.artifacts);
            let (effects_page, effects_next) = paginate(&effects_all, effects_cursor, page.effects);
            let log_start = log_end.saturating_sub(page.log);
            let log_tail: Vec<Value> = logs[log_start..log_end].to_vec();
            let log_next = (log_start > 0).then_some(log_start);
            let (result, result_handle) = match (&result_text, page.spill_result) {
                (Some(text), false) if text.chars().count() <= GET_BUDGET_CHARS => {
                    (Some(Value::String(text.clone())), None)
                }
                (Some(text), _) => {
                    let handle = result_handle_cache.get_or_insert_with(|| {
                        self.spill_result(&row.run_id, row.exec_generation, text)
                    });
                    (None, Some(handle.clone()))
                }
                (None, _) => (None, None),
            };
            let body = json!({
                "run_id": row.run_id,
                "label": truncate_chars(&row.label, 200),
                "status": row.status,
                "stop_reason": row.stop_reason,
                "stop_origin": row.stop_origin,
                "created_at": row.created_at,
                "updated_at": row.updated_at,
                "spent_tokens": row.spent_tokens,
                "snapshot_path": row.snapshot_path,
                "snapshot_hash": row.snapshot_hash,
                "script_generation": row.script_generation,
                "exec_generation": row.exec_generation,
                "legacy": row.legacy,
                "resumable": row.resumable && row.superseded_by.is_none(),
                "progress": {
                    "asks_done": ask_done,
                    "asks_total": ask_total,
                    "active_agents": active_agents,
                },
                "ask_stats": {
                    "asks_total": ask_total,
                    "asks_done": ask_done,
                    "asks_replayed": ask_replayed,
                    "exec_generation": row.exec_generation,
                },
                "log_tail": log_tail,
                "log_cursor": log_next,
                "pending_questions": pending_questions,
                "holes": holes,
                "phases": phases,
                "reports": reports_page,
                "reports_cursor": reports_next,
                "artifacts": artifacts_page,
                "artifacts_cursor": artifacts_next,
                "effects": effects_page,
                "effects_cursor": effects_next,
                "result": result,
                "result_handle": result_handle,
                // ask 子代理的未解决错误披露（answer / ask_replayed 事件独立字段，
                // 去重保序）——父代理经 WorkflowRun 投影必然可见。
                "unresolved_disclosures": unresolved_disclosures,
                "error": error,
                "recovery": recovery_actions(&row),
                "notifications": notifications,
                "processes": processes,
            });
            if body.to_string().chars().count() <= GET_BUDGET_CHARS || !page.shrink() {
                return Ok(body);
            }
        }
    }

    /// `result` 超预算：全文写到 `<workflow-artifacts>/<run_id>/result-g<exec>.txt`，正文只回
    /// 句柄（路径 + 头尾预览），模型用 `read` 工具按 offset/limit 分页取回。落盘失败不让一次
    /// 成功的 Get 变失败：句柄里只带预览、`path` 为 null。
    fn spill_result(&self, run_id: &str, exec_generation: u32, text: &str) -> Value {
        let preview = rclaw_tools::truncate::head_tail_truncate(text, 4_096).text;
        let path = self
            .inner
            .paths
            .artifacts_dir
            .join(run_id)
            .join(format!("result-g{exec_generation}.txt"));
        let written = super::workflow_paths::write_atomic(&path, text.as_bytes());
        if let Err(error) = &written {
            tracing::warn!(run_id, %error, "工作流 result 落盘失败，句柄只带预览");
        }
        json!({
            "path": written.is_ok().then(|| path.to_string_lossy().replace('\\', "/")),
            "bytes": text.len(),
            "chars": text.chars().count(),
            "preview": preview,
            "hint": "result 超过 48k 字符；用 read 工具按 offset/limit 分页读取 path",
        })
    }

    // -----------------------------------------------------------------------
    // Question / effects / notifications（UI / 特权入口与 Resolve 共用）
    // -----------------------------------------------------------------------

    pub(crate) fn resolve_question(
        &self,
        question_id: &str,
        answer: &str,
        channel: &str,
        chat_id: &str,
        tool_access: ToolAccess,
    ) -> Result<Value, WorkflowError> {
        // 只读实例不能替另一实例的 run 落答案 / 改裁决：那边的 driver 看不到这边的内存唤醒，
        // 落了库也只会让两个实例的投影打架。
        self.check_writable()?;
        let Some((run_id, exec_generation)) = parse_question_id(question_id) else {
            return Err(WorkflowError::new(
                "unknown_question",
                "not_found",
                "question_id 形如 dwfq-<run_id>-<exec_generation>-<seq>",
            ));
        };
        let row = self.load_run_checked(&run_id, tool_access, channel, chat_id, true)?;
        if row.status != STATUS_RUNNING {
            return Err(WorkflowError::new(
                "run_not_in_flight",
                "state",
                "run 不在运行中",
            ));
        }
        if row.exec_generation != exec_generation {
            return Err(WorkflowError::new(
                "run_not_in_flight",
                "state",
                "代次已失效（resume / hole fill 后旧 qid 不可再答）",
            ));
        }
        let question = self
            .inner
            .store
            .get_question(question_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::new("unknown_question", "not_found", "未知问题"))?;
        if question.state == "answered" {
            return Err(WorkflowError::new(
                "already_resolved",
                "state",
                format!("已回答：{}", question.answer.unwrap_or_default()),
            ));
        }
        let accepted = self
            .inner
            .store
            .answer_question_cas(question_id, answer)
            .map_err(internal_error)?;
        if !accepted {
            return Err(WorkflowError::new(
                "already_resolved",
                "state",
                "并发回答已生效",
            ));
        }
        let journal = EventLog::workflow(self.inner.store.journal(), &run_id);
        append_workflow_event(
            &journal,
            &json!({
                "event": "question_answered",
                "question_id": question_id,
                "answer": answer,
                "answered_by": "main",
            }),
        );
        let delivered = self.inner.runtime.resolve_escalation(question_id, answer);
        Ok(json!({
            "status": "answered",
            "question_id": question_id,
            "run_id": run_id,
            "delivered_live": delivered,
        }))
    }

    pub(crate) fn confirm_effects(
        &self,
        run_id: &str,
        exec_generation: u32,
        decisions: &[Value],
        decided_by: &str,
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let row = self
            .inner
            .store
            .get_run(run_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::not_found(run_id))?;
        let pending: Vec<_> = self
            .inner
            .store
            .list_effects(run_id)
            .map_err(internal_error)?
            .into_iter()
            .filter(|effect| effect.blocks(exec_generation))
            .collect();
        let mut covered: HashSet<String> = HashSet::new();
        for decision in decisions {
            let key = decision
                .get("effect_key")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let action = decision
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(action, "skip" | "rerun") {
                return Err(WorkflowError::new(
                    "args_invalid",
                    "validation",
                    "action 只能是 skip / rerun",
                ));
            }
            if action == "skip" && decision.get("substitute_result").is_none() {
                return Err(WorkflowError::new(
                    "effect_result_required",
                    "validation",
                    "skip 必须提供 substitute_result",
                ));
            }
            self.inner
                .store
                .decide_effect(
                    run_id,
                    key,
                    action,
                    exec_generation,
                    decision
                        .get("substitute_result")
                        .map(|value| value.to_string())
                        .as_deref(),
                    decided_by,
                )
                .map_err(internal_error)?;
            covered.insert(key.to_string());
            append_workflow_event(
                &EventLog::workflow(self.inner.store.journal(), run_id),
                &json!({
                    "event": "effect_decided",
                    "run_id": run_id,
                    "effect_key": key,
                    "decision": action,
                    "decision_exec_generation": exec_generation,
                    "substitute_result": decision.get("substitute_result"),
                    "decided_by": decided_by,
                }),
            );
        }
        let missing: Vec<String> = pending
            .iter()
            .map(|effect| effect.effect_key.clone())
            .filter(|key| !covered.contains(key))
            .collect();
        if !missing.is_empty() {
            return Err(WorkflowError {
                code: "effects_unconfirmed".to_string(),
                message: format!("仍有 {} 个副作用未裁决", missing.len()),
                kind: "state".to_string(),
                retryable: true,
                recovery: missing.join(", "),
            });
        }
        let _ = row;
        Ok(json!({"status":"confirmed","run_id":run_id,"exec_generation":exec_generation}))
    }

    pub(crate) fn confirm_amend(&self, run_id: &str) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        // 绑定到具体 run：不存在的 run_id 不该留下一把永久有效的「允许写 effect 修订」钥匙。
        self.inner
            .store
            .get_run(run_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::not_found(run_id))?;
        self.inner
            .runtime
            .journal_store()
            .kv_set(
                &format!("workflow-amend-confirm/{run_id}"),
                &json!({"confirmed": true, "at": rclaw_store::db::now_ms()}),
            )
            .map_err(internal_error)?;
        Ok(json!({"status":"confirmed","run_id":run_id,"scope":"amend_write_effects"}))
    }

    pub(crate) fn confirm_process_gone(
        &self,
        run_id: &str,
        exec_generation: u32,
        process_ids: &[String],
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let row = self
            .inner
            .store
            .get_run(run_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::not_found(run_id))?;
        if row.exec_generation != exec_generation {
            return Err(WorkflowError::invalid_state("exec_generation 不符"));
        }
        let running = self
            .inner
            .store
            .running_processes(run_id, exec_generation)
            .map_err(internal_error)?;
        for process_id in &running {
            if !process_ids.contains(process_id) {
                return Err(WorkflowError {
                    code: "process_unconfirmed".to_string(),
                    message: format!("缺少进程 {process_id} 的确认"),
                    kind: "state".to_string(),
                    retryable: true,
                    recovery: "Pass every running process id.".to_string(),
                });
            }
        }
        for process_id in process_ids {
            self.inner
                .store
                .mark_process_exited(run_id, exec_generation, process_id)
                .map_err(internal_error)?;
        }
        append_workflow_event(
            &EventLog::workflow(self.inner.store.journal(), run_id),
            &json!({
                "event": "process_confirmed",
                "run_id": run_id,
                "exec_generation": exec_generation,
                "process_ids": process_ids,
                "method": "ui",
                "checked_at": rclaw_store::db::now_ms(),
            }),
        );
        if row.status == STATUS_STOPPING {
            // 保留行上已有的 stop_origin / fresh-auth（用户取消后崩溃的 run 不因确认进程树而
            // 降级成模型可自行 Resume）。
            let origin = row
                .stop_origin
                .clone()
                .unwrap_or_else(|| STOP_INTERRUPTED.to_string());
            let requires_fresh_auth = row.requires_fresh_auth || origin == STOP_USER;
            let event = json!({
                "event": "run_stopped",
                "exec_generation": row.exec_generation,
                "reason": STOP_INTERRUPTED,
                "stop_origin": origin,
                "requires_fresh_auth": requires_fresh_auth as i64,
                "detail": "process tree confirmed gone by the user",
            });
            self.terminal_transaction(
                &row,
                STATUS_STOPPED,
                &event,
                Some(STOP_INTERRUPTED),
                Some(&origin),
                requires_fresh_auth,
                Some(true),
                None,
            )
            .map_err(internal_error)?;
            // 前驱（stopping(superseded) 后崩溃）由 UI 确认收口：claimed 的后继在这里补启；
            // 同步模式没有后台执行者，残留的 queued 后继按同步 Amend 超时同样回滚（否则它
            // 刚从终态事务接过许可、却永远没人启动）。
            if self.inner.opts.background {
                self.start_claimed_successor(run_id);
            } else if let Ok(Some(successor)) = self.inner.store.successor_claimed_by(run_id) {
                if let Ok(Some(successor)) = self.inner.store.get_run(&successor) {
                    self.rollback_sync_successor(run_id, &successor, "", "");
                }
            }
        }
        Ok(json!({"status":"confirmed","run_id":run_id}))
    }

    pub(crate) fn resend_notification(
        &self,
        notification_id: &str,
    ) -> Result<Value, WorkflowError> {
        self.check_writable()?;
        let changed = self
            .inner
            .store
            .resend_notification(notification_id)
            .map_err(internal_error)?;
        if !changed {
            return Err(WorkflowError::new(
                "not_found",
                "not_found",
                "只有 dead 通知可以重投",
            ));
        }
        Ok(json!({"status":"rescheduled","id":notification_id}))
    }

    // -----------------------------------------------------------------------
    // 状态 / 配额 / driver 协作
    // -----------------------------------------------------------------------

    pub(crate) fn load_run_checked(
        &self,
        run_id: &str,
        tool_access: ToolAccess,
        channel: &str,
        chat_id: &str,
        write: bool,
    ) -> Result<RunRow, WorkflowError> {
        let row = self
            .inner
            .store
            .get_run(run_id)
            .map_err(internal_error)?
            .ok_or_else(|| WorkflowError::not_found(run_id))?;
        let project_key = self.inner.paths.project_key.clone();
        let legacy_read = row.legacy && !write && tool_access == ToolAccess::Unrestricted;
        if row.cwd != project_key && !legacy_read {
            return Err(WorkflowError::new(
                "forbidden_project",
                "permission",
                "run 属于另一个项目",
            ));
        }
        let snapshot_access = access_from_config(&row.execution_config());
        if tool_access < snapshot_access {
            return Err(WorkflowError::new(
                "permission_downgraded",
                "permission",
                "当前会话权限低于 run 的启动权限",
            ));
        }
        if write {
            let session = session_key(channel, chat_id);
            let privileged = matches!(channel, "cli" | "desktop" | "tui");
            if !privileged && row.session_key != session {
                return Err(WorkflowError::new(
                    "forbidden_session",
                    "permission",
                    "只有发起会话或特权会话可以修改 run",
                ));
            }
        }
        Ok(row)
    }

    pub(crate) fn acquire_capacity(&self, run_id: &str) -> Result<bool, WorkflowError> {
        let mut capacity = self
            .inner
            .capacity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if capacity.len() >= self.inner.opts.max_active && !capacity.contains(run_id) {
            return Ok(false);
        }
        capacity.insert(run_id.to_string());
        self.inner
            .store
            .set_capacity_owner(run_id, Some(run_id))
            .map_err(internal_error)?;
        Ok(true)
    }

    pub(crate) fn release_capacity(&self, run_id: &str) {
        {
            let mut capacity = self
                .inner
                .capacity
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            capacity.remove(run_id);
        }
        if let Err(error) = self.inner.store.set_capacity_owner(run_id, None) {
            tracing::warn!("释放工作流许可失败: {error}");
        }
    }

    pub(crate) fn transfer_capacity(&self, from: &str, to: &str) {
        {
            // 无条件 remove + insert：前驱终态事务可能已经把许可归还 / 转给后继（内存里
            // 已没有 from），这里仍要保证 to 在集合里，否则后继跑着却不计数。
            let mut capacity = self
                .inner
                .capacity
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            capacity.remove(from);
            capacity.insert(to.to_string());
        }
        if let Err(error) = self.inner.store.transfer_capacity(from, to) {
            tracing::warn!("交接工作流许可失败: {error}");
        }
    }

    pub(crate) fn is_active(&self, run_id: &str) -> bool {
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .contains_key(run_id)
    }

    pub(crate) fn cancel_token(&self, run_id: &str) -> Option<CancellationToken> {
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .get(run_id)
            .map(|active| active.cancel.clone())
    }

    pub(crate) fn register_active(
        &self,
        run_id: &str,
        cancel: CancellationToken,
        retune_slot: Arc<StdMutex<Option<super::workflow::RetuneHandle>>>,
    ) -> bool {
        let mut active = self
            .inner
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if active.contains_key(run_id) {
            return false;
        }
        active.insert(
            run_id.to_string(),
            ActiveRun {
                cancel,
                retune_slot,
            },
        );
        true
    }

    pub(crate) fn unregister_active(&self, run_id: &str) {
        self.inner
            .active
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(run_id);
    }

    pub(crate) fn start_driver(
        &self,
        run_id: &str,
        exec_generation: u32,
        import_from: Option<&str>,
    ) {
        self.ensure_outbox();
        let cancel = CancellationToken::new();
        let retune_slot: Arc<StdMutex<Option<super::workflow::RetuneHandle>>> =
            Arc::new(StdMutex::new(None));
        if !self.register_active(run_id, cancel.clone(), Arc::clone(&retune_slot)) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("工作流 driver 启动失败：当前线程不在 tokio runtime 内");
            self.unregister_active(run_id);
            return;
        };
        let service = self.clone();
        let run_id = run_id.to_string();
        let import_from = import_from.map(str::to_string);
        handle.spawn(async move {
            // driver 单独 spawn：任务 panic 时拿到 JoinError 补写 errored 终态并释放配额，
            // 否则 run 永久停在 running（心跳停止）且名额泄漏，只能等进程重启才能收口。
            let driven = tokio::spawn(super::workflow_driver::drive(
                service.clone(),
                run_id.clone(),
                exec_generation,
                import_from,
                cancel,
                retune_slot,
            ))
            .await;
            if let Err(error) = driven {
                tracing::error!("工作流 {run_id} driver 任务异常退出: {error}");
                super::workflow_driver::fail_active_run(
                    &service,
                    &run_id,
                    "driver_panicked",
                    format!("workflow driver 任务异常退出: {error}"),
                )
                .await;
            }
            service.unregister_active(&run_id);
            // 后继交接的统一出口：不管 driver 从哪条路径退出（正常终态 / hole 等待中被取消 /
            // fail_run / panic 兜底），前驱已终态且有 claimed 的 queued 后继就在这里启动它。
            service.start_claimed_successor(&run_id);
        });
    }

    /// §5.2 模式 B 第 5 步：前驱终态后把许可交给 claimed 的 queued 后继并启动它。
    /// driver 退出、进程树确认、租约扫描收口等所有写前驱终态的路径都经这里，否则后继会
    /// 永久停在 queued 且占着名额。前驱非终态 / 没有 claimed 后继 / 同步模式（Amend 在同一
    /// 调用内等前驱退出后自己跑后继，§13.8）都不动。
    pub(crate) fn start_claimed_successor(&self, predecessor: &str) {
        if !self.inner.opts.background {
            return;
        }
        let terminal = self
            .inner
            .store
            .get_run(predecessor)
            .ok()
            .flatten()
            .is_some_and(|row| row.is_terminal());
        if !terminal {
            return;
        }
        let Ok(Some(successor)) = self.inner.store.successor_claimed_by(predecessor) else {
            return;
        };
        self.transfer_capacity(predecessor, &successor);
        self.start_driver(&successor, 1, Some(predecessor));
    }

    /// 同步模式 Amend 等不到前驱退出时的回滚（§13.8 不持久化 queued）：
    /// 1. 后继直接 stopped(interrupted)——它从未启动，lineage 撤回前驱；
    /// 2. 撤销前驱的 `superseded_by`（CAS，只撤本次的后继），仍在 stopping 的前驱停止原因改成
    ///    `interrupted`（不是真的被取代，收口后可 Resume，也不要求 fresh-auth），同事务记
    ///    `run_superseded{by:null}` / `run_stopping{interrupted}` 让投影重放得到同样结果；
    /// 3. 幂等记录置 failed：错误里建议的「同一 request_id 重试」要真的重做，而不是回放一份
    ///    指向死后继的 `backgrounded`。
    ///
    /// 先收口后继再撤标记：反过来一旦中途失败，会留下无 claim 的 queued 后继被重启恢复拉起。
    fn rollback_sync_successor(
        &self,
        predecessor: &str,
        successor: &RunRow,
        scope: &str,
        request_id: &str,
    ) {
        let detail = "sync amend rolled back: predecessor did not stop in time";
        let event = json!({
            "event": "run_stopped",
            "exec_generation": successor.exec_generation,
            "reason": STOP_INTERRUPTED,
            "stop_origin": STOP_INTERRUPTED,
            "requires_fresh_auth": 0,
            "detail": detail,
        });
        let _ = self.inner.store.set_capacity_claim(&successor.run_id, None);
        if let Err(error) = self.terminal_transaction(
            successor,
            STATUS_STOPPED,
            &event,
            Some(STOP_INTERRUPTED),
            Some(STOP_INTERRUPTED),
            false,
            Some(false),
            Some(detail),
        ) {
            tracing::warn!("同步 Amend 回滚后继失败: {error}");
            return;
        }
        let journal = EventLog::workflow(self.inner.store.journal(), predecessor);
        let successor_id = successor.run_id.clone();
        let predecessor_id = predecessor.to_string();
        let scope = scope.to_string();
        let request_id = request_id.to_string();
        let outcome = self.inner.store.transaction(|tx| {
            let now = rclaw_store::db::now_ms();
            // 先读前驱当前态：仍 stopping → 停止原因改 interrupted（driver 退出时据此收口）；
            // 已 stopped(superseded)（等待窗口最后几十毫秒才退出 / 重启恢复路径）→ 同样改成
            // interrupted，否则它既没有后继又因 `stop_reason = superseded` 不可 Resume。
            let before: Option<(String, Option<String>, i64)> = tx
                .query_row(
                    "SELECT status, stop_reason, exec_generation FROM workflow_run \
                     WHERE run_id = ?1 AND superseded_by = ?2",
                    rusqlite::params![&predecessor_id, &successor_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some((status, stop_reason, exec_generation)) = before {
                let rewrite_reason =
                    status == STATUS_STOPPING || stop_reason.as_deref() == Some(STOP_SUPERSEDED);
                tx.execute(
                    "UPDATE workflow_run SET superseded_by = NULL, \
                     stop_reason = CASE WHEN ?4 THEN ?5 ELSE stop_reason END, \
                     stop_origin = CASE WHEN ?4 THEN ?5 ELSE stop_origin END, \
                     updated_at = ?3 WHERE run_id = ?1 AND superseded_by = ?2",
                    rusqlite::params![
                        &predecessor_id,
                        &successor_id,
                        now,
                        rewrite_reason,
                        STOP_INTERRUPTED
                    ],
                )
                .map_err(|e| e.to_string())?;
                journal
                    .append_tx(
                        tx,
                        &json!({"event": "run_superseded", "run_id": predecessor_id, "by": Value::Null}),
                    )
                    .map_err(|e| e.to_string())?;
                if status == STATUS_STOPPING {
                    journal
                        .append_tx(
                            tx,
                            &json!({
                                "event": "run_stopping",
                                "exec_generation": exec_generation,
                                "reason": STOP_INTERRUPTED,
                                "stop_origin": STOP_INTERRUPTED,
                            }),
                        )
                        .map_err(|e| e.to_string())?;
                } else if rewrite_reason {
                    // 终态已写过：再记一条 run_stopped(interrupted) 让投影重放收敛到同一结果。
                    journal
                        .append_tx(
                            tx,
                            &json!({
                                "event": "run_stopped",
                                "exec_generation": exec_generation,
                                "reason": STOP_INTERRUPTED,
                                "stop_origin": STOP_INTERRUPTED,
                                "requires_fresh_auth": 0,
                                "detail": "sync amend rolled back after the predecessor stopped",
                            }),
                        )
                        .map_err(|e| e.to_string())?;
                }
            }
            // 幂等记录：知道 scope / request_id 时按键置 failed；重启 / 确认进程树的回滚路径
            // 不知道键，按指向该后继的 done 记录置 failed，同一 request_id 重试才会真的重做。
            if scope.is_empty() {
                tx.execute(
                    "UPDATE workflow_request SET state = 'failed', response = NULL, run_id = NULL, \
                     updated_at = ?2 WHERE run_id = ?1 AND state = 'done'",
                    rusqlite::params![&successor_id, now],
                )
                .map_err(|e| e.to_string())?;
            } else {
                tx.execute(
                    "UPDATE workflow_request SET state = 'failed', response = NULL, run_id = NULL, \
                     updated_at = ?3 WHERE scope = ?1 AND request_id = ?2",
                    rusqlite::params![&scope, &request_id, now],
                )
                .map_err(|e| e.to_string())?;
            }
            Ok(())
        });
        if let Err(error) = outcome {
            tracing::warn!("同步 Amend 撤销前驱 superseded 标记失败: {error}");
        }
    }

    /// driver 终态事务：journal 事件 + run 行 + outbox + 许可归还 / 交接同事务。
    ///
    /// 许可：终态 run 一律不再持有许可（`capacity_owner = NULL`）；若有 queued 后继正等着
    /// 接手（amend 的 claim），许可在同一事务里直接转给后继，不留“先归还再交接”的窗口。
    /// 事务提交后同步内存 `capacity` 集合——每条终态路径（driver 收尾、Cancel 无 driver、
    /// 重启收口、进程树确认、queued 取消）都经过这里，不会再有写了终态却不还名额的分支。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn terminal_transaction(
        &self,
        row: &RunRow,
        status: &str,
        event: &Value,
        stop_reason: Option<&str>,
        stop_origin: Option<&str>,
        requires_fresh_auth: bool,
        resumable: Option<bool>,
        error: Option<&str>,
    ) -> Result<(), String> {
        self.terminal_transaction_if(
            row,
            None,
            status,
            event,
            stop_reason,
            stop_origin,
            requires_fresh_auth,
            resumable,
            error,
        )
        .map(|_| ())
    }

    /// 带状态围栏的终态事务：`expected_status` 给定时只在行仍处于该状态才写（CAS），否则整个
    /// 事务回滚（journal 事件不落）并返回 Ok(false)。queued 取消用它：读行之后后继 driver
    /// 可能已把 queued→running，无条件写 stopped 会让引擎在"已停止"的行上继续跑完。
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn terminal_transaction_if(
        &self,
        row: &RunRow,
        expected_status: Option<&str>,
        status: &str,
        event: &Value,
        stop_reason: Option<&str>,
        stop_origin: Option<&str>,
        requires_fresh_auth: bool,
        resumable: Option<bool>,
        error: Option<&str>,
    ) -> Result<bool, String> {
        const TERMINAL_SKIPPED: &str = "__terminal_skipped__";
        let expected_status = expected_status.map(str::to_string);
        let journal = EventLog::workflow(self.inner.store.journal(), &row.run_id);
        let store = self.inner.store.clone();
        let run_id = row.run_id.clone();
        let status = status.to_string();
        let stop_reason = stop_reason.map(str::to_string);
        let stop_origin = stop_origin.map(str::to_string);
        let error = error.map(str::to_string);
        let (channel, chat_id) = split_session_key(&row.session_key);
        let payload = json!({
            "kind": status,
            "run_id": &run_id,
            "label": &row.label,
            "stop_reason": &stop_reason,
            "error": &error,
        });
        let event = event.clone();
        // §13.8：同步模式（run 的 execution_config.background_mode=false）终态不唤醒——结果就是
        // 工具返回值，再投一条通知会让调用方 turn 结束后又被叫醒一次重复汇报。
        let background_mode = row
            .execution_config()
            .get("background_mode")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let successor = store.transaction(|tx| {
            journal
                .append_tx(tx, &event)
                .map_err(|e| e.to_string())?;
            // 通知 id 用行里当前的 exec_generation：driver 启动时读到的 row 可能还是上一代
            // （Resume 不预先 bump），沿用会与上一次 stopped 通知同 id 而被 INSERT OR IGNORE 吞掉。
            let current_generation: i64 = tx
                .query_row(
                    "SELECT exec_generation FROM workflow_run WHERE run_id = ?1",
                    rusqlite::params![&run_id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            let notification_id = format!("wf-note-{}-{}-{}", run_id, current_generation, status);
            // requires_fresh_auth 只升不降：用户取消的要求一旦立下，只有 driver 凭新授权启动
            // （生命周期事务）才清零；终态路径传 false 不能把它抹掉。
            let changed = tx
                .execute(
                    "UPDATE workflow_run SET status = ?2, stop_reason = coalesce(?3, stop_reason), \
                     stop_origin = coalesce(?4, stop_origin), \
                     requires_fresh_auth = max(?5, requires_fresh_auth), \
                     resumable = coalesce(?6, resumable), error = coalesce(?7, error), updated_at = ?8, \
                     lease_until = NULL, capacity_owner = NULL \
                     WHERE run_id = ?1 AND (?9 IS NULL OR status = ?9)",
                    rusqlite::params![
                        run_id,
                        status,
                        stop_reason,
                        stop_origin,
                        requires_fresh_auth as i64,
                        resumable.map(|value| value as i64),
                        error,
                        rclaw_store::db::now_ms(),
                        expected_status
                    ],
                )
                .map_err(|e| e.to_string())?;
            if changed == 0 {
                // 围栏不满足：回滚（上面追加的事件一并撤销）。
                return Err(TERMINAL_SKIPPED.to_string());
            }
            let successor: Option<String> = tx
                .query_row(
                    "SELECT run_id FROM workflow_run WHERE resumed_from = ?1 AND status = 'queued' \
                     AND capacity_claim_for IS NOT NULL ORDER BY created_at ASC LIMIT 1",
                    rusqlite::params![&run_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some(successor) = &successor {
                tx.execute(
                    "UPDATE workflow_run SET capacity_owner = ?1, updated_at = ?2 WHERE run_id = ?1",
                    rusqlite::params![successor, rclaw_store::db::now_ms()],
                )
                .map_err(|e| e.to_string())?;
            }
            if background_mode {
                workflow_store::enqueue_notification_tx(
                    tx,
                    &notification_id,
                    &run_id,
                    &row.session_key,
                    &channel,
                    &chat_id,
                    &payload,
                )
                .map_err(|e| e.to_string())?;
            }
            // §13.3：终态只保留这一条 pending 通知，还没投出去的进度 / 问题 / hole 通知标 dead
            // （它们携带的 qid / hole_id 下面马上就作废了）。
            tx.execute(
                "UPDATE workflow_notification SET state = 'dead', \
                 last_error = 'superseded by terminal notification' \
                 WHERE run_id = ?1 AND id <> ?2 AND state = 'pending'",
                rusqlite::params![&run_id, &notification_id],
            )
            .map_err(|e| e.to_string())?;
            // 等待对象清理与终态同一事务：问题 / hole 一律 abort。
            let pending_questions: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT question_id FROM workflow_question WHERE run_id = ?1 AND state = 'pending'",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![&run_id], |row| row.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(|e| e.to_string())?);
                }
                out
            };
            for question_id in pending_questions {
                tx.execute(
                    "UPDATE workflow_question SET state = 'aborted' WHERE question_id = ?1 AND state = 'pending'",
                    rusqlite::params![&question_id],
                )
                .map_err(|e| e.to_string())?;
                journal
                    .append_tx(tx, &json!({"event": "question_aborted", "question_id": question_id}))
                    .map_err(|e| e.to_string())?;
            }
            let active_holes: Vec<String> = {
                let mut stmt = tx
                    .prepare(
                        "SELECT hole_id FROM workflow_hole WHERE run_id = ?1 AND state IN ('waiting','filling')",
                    )
                    .map_err(|e| e.to_string())?;
                let rows = stmt
                    .query_map(rusqlite::params![&run_id], |row| row.get::<_, String>(0))
                    .map_err(|e| e.to_string())?;
                let mut out = Vec::new();
                for row in rows {
                    out.push(row.map_err(|e| e.to_string())?);
                }
                out
            };
            for hole_id in active_holes {
                tx.execute(
                    "UPDATE workflow_hole SET state = 'aborted' WHERE hole_id = ?1 AND run_id = ?2",
                    rusqlite::params![&hole_id, &run_id],
                )
                .map_err(|e| e.to_string())?;
                journal
                    .append_tx(tx, &json!({"event": "hole_aborted", "hole_id": hole_id}))
                    .map_err(|e| e.to_string())?;
            }
            Ok(successor)
        });
        let successor = match successor {
            Ok(successor) => successor,
            // `WorkflowStore::transaction` 会把闭包错误包一层前缀，哨兵按包含判断。
            Err(error) if error.contains(TERMINAL_SKIPPED) => return Ok(false),
            Err(error) => return Err(error),
        };
        let mut capacity = self
            .inner
            .capacity
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        capacity.remove(&row.run_id);
        if let Some(successor) = successor {
            capacity.insert(successor);
        }
        Ok(true)
    }

    fn ensure_outbox(&self) {
        if self
            .inner
            .outbox_started
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let service = self.clone();
        handle.spawn(async move {
            // 5s 一拍：outbox 每拍投一次；租约扫描每 12 拍（60s）一次（§3.6）。
            let mut tick: u64 = 0;
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if let Err(error) = service.pump_notifications().await {
                    tracing::warn!("工作流通知 outbox 投递失败: {error}");
                }
                tick += 1;
                if tick.is_multiple_of(12) {
                    if let Err(error) = service.lease_scan() {
                        tracing::warn!("工作流租约扫描失败: {error}");
                    }
                }
            }
        });
    }

    /// 租约扫描（§3.6 / §13.1 第 5 步）：本实例 owner 但心跳停滞 > 120s 的 run 先转
    /// `stopping(interrupted)` 并撤销执行（取消令牌、不再派发 ask），**不能直接标 `stopped`**；
    /// driver 仍在就等它自己以 stopped 收口；driver 已不在，核验命令进程树全部退出后才写
    /// `stopped(interrupted)` 并释放配额，未确认则保持 stopping、继续持有许可，等
    /// `workflow_confirm_process_gone`。
    fn lease_scan(&self) -> Result<(), String> {
        // 只读 / 降级（状态库不可信）时不动行。
        if self.check_writable().is_err() {
            return Ok(());
        }
        let now = rclaw_store::db::now_ms();
        for row in self
            .inner
            .store
            .stalled_runs(&self.inner.instance_id, now - 120_000)?
        {
            // hole 等待（waiting / filling）期间 driver 按设计退出、没有心跳：不是停滞。只对
            // running 行成立——stopping 且停在 hole 上的行是 Cancel/Amend 恰好落在 driver 退出
            // 窗口里的产物，没有任何执行者会再为它收口，必须在这里按 interrupted 收口。
            let parked_at_hole = row.status == STATUS_RUNNING
                && self
                    .inner
                    .store
                    .list_holes(&row.run_id)?
                    .iter()
                    .any(|hole| hole.state == "waiting" || hole.state == "filling");
            if parked_at_hole {
                continue;
            }
            if row.status == STATUS_RUNNING {
                tracing::warn!(
                    "工作流 {} 心跳停滞 {}s，转 stopping(interrupted)",
                    row.run_id,
                    (now - row.heartbeat_at) / 1000
                );
                self.inner.store.begin_stopping(
                    &row.run_id,
                    row.exec_generation,
                    STOP_INTERRUPTED,
                    STOP_INTERRUPTED,
                )?;
            }
            if let Some(token) = self.cancel_token(&row.run_id) {
                token.cancel();
            }
            if self.is_active(&row.run_id) {
                // driver 还在：令牌已取消，它退出时按 stopping 行上的 reason 写 stopped。
                continue;
            }
            let Some(current) = self.inner.store.get_run(&row.run_id)? else {
                continue;
            };
            if current.status != STATUS_STOPPING {
                continue;
            }
            let running_processes = self
                .inner
                .store
                .running_processes(&current.run_id, current.exec_generation)?;
            if running_processes.is_empty() || process_tree_proven_gone(&running_processes) {
                self.finish_interrupted(&current)?;
                self.start_claimed_successor(&current.run_id);
            }
        }
        Ok(())
    }

    async fn pump_notifications(&self) -> Result<(), String> {
        let now = rclaw_store::db::now_ms();
        let _ = self
            .inner
            .store
            .requeue_stale_notifications(now - 60_000)
            .map_err(|e| e.to_string())?;
        let rows = self
            .inner
            .store
            .claim_notifications(20, now)
            .map_err(|e| e.to_string())?;
        for row in rows {
            // §3.6：重试 24h 仍未被确认 → dead + last_error，交给 UI `workflow_resend_notification`
            // 重投；不能让 outbox 每分钟永远重发同一条。
            if row.created_at > 0 && now - row.created_at > 24 * 3_600_000 {
                self.inner
                    .store
                    .dead_notification(&row.id, "notification not consumed within 24h")
                    .map_err(|e| e.to_string())?;
                continue;
            }
            let payload: Value = serde_json::from_str(&row.payload).unwrap_or(Value::Null);
            let content = notification_text(&payload);
            // 唤醒 turn 继承 run 启动时的权限快照：通知是发起会话工作的延续，默认 Sandboxed 会让
            // 被叫醒的模型连 GetWorkflowRun 都吃 `permission_downgraded`（Unrestricted 创建的 run）。
            // 但快照只能向下对齐发起会话【此刻】的权限：用户已把会话降回 Sandboxed（driver 正是
            // 因此 permission_revoked 收口）时，被叫醒的 turn 不能再拿着旧快照无沙箱跑 shell /
            // write——通知正文里带着子代理 / 文件内容，是注入面。宿主不认识该会话才退回快照。
            let snapshot = self
                .inner
                .store
                .get_run(&row.run_id)
                .ok()
                .flatten()
                .map(|run| access_from_config(&run.execution_config()))
                .unwrap_or(ToolAccess::Sandboxed);
            let tool_access = rclaw_tools::access::session_access(&row.channel, &row.chat_id)
                .map_or(snapshot, |current| current.min(snapshot));
            let message = rclaw_core::BusMessage::new_workflow_with_id(
                &row.channel,
                &row.chat_id,
                content,
                &row.id,
            )
            .with_tool_access(tool_access);
            let sent = self
                .inner
                .runtime
                .background_wake()
                .send_message(message)
                .await;
            if !sent {
                // 投出去的保持 scheduled，接收端认领后回执 consumed，超时未确认会重发。
                self.inner
                    .store
                    .reschedule_notification(&row.id, now + 30_000, "inbound not ready")
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    fn allowed_models(&self, script: &str, subagent_model: Option<&str>) -> Vec<String> {
        let mut models: Vec<String> = Vec::new();
        models.push(self.inner.runtime.provider_name());
        if let Some(model) = subagent_model {
            models.push(model.to_string());
        }
        for model in super::workflow_saved::model_literals(script) {
            models.push(model);
        }
        models.sort();
        models.dedup();
        models
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// 子代理 → 编排者提问（workflow 上下文）
// ---------------------------------------------------------------------------

/// workflow ask 内可用的提问上下文（EscalateTool 通过 task-local 读取）。
#[derive(Clone)]
pub(crate) struct WorkflowAskContext {
    pub(crate) run_id: String,
    pub(crate) exec_generation: u32,
    pub(crate) actor: String,
    pub(crate) ask_identity: String,
    pub(crate) store: WorkflowStore,
    pub(crate) journal: EventLog,
    pub(crate) cancel: CancellationToken,
    pub(crate) runtime: AgentToolRuntime,
    pub(crate) effects: Arc<super::workflow_effects::EffectTracker>,
    pub(crate) next_seq: Arc<std::sync::atomic::AtomicU64>,
    pub(crate) question_occurrence: Arc<std::sync::atomic::AtomicU64>,
}

tokio::task_local! {
    static WORKFLOW_ASK: Arc<WorkflowAskContext>;
}

/// 在 workflow ask 作用域内运行子代理 future。
pub(crate) async fn scope_workflow_ask<F: std::future::Future>(
    context: Arc<WorkflowAskContext>,
    future: F,
) -> F::Output {
    WORKFLOW_ASK.scope(context, future).await
}

/// 当前 ask 的 workflow 上下文（非 workflow 子代理返回 None）。
pub(crate) fn current_workflow_ask() -> Option<Arc<WorkflowAskContext>> {
    WORKFLOW_ASK.try_with(Clone::clone).ok()
}

impl WorkflowAskContext {
    /// 提交问题并等待编排者回答。
    pub(crate) async fn ask(&self, question: String) -> Result<(String, String), String> {
        let occurrence = self
            .question_occurrence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let question_key = format!(
            "{:016x}",
            fnv1a64(&format!("{}:{occurrence}", self.ask_identity))
        );
        if let Some(answer) = self
            .store
            .find_answered_by_key(&self.run_id, &question_key)
            .map_err(|error| error.to_string())?
        {
            return Ok(("cached-question".to_string(), answer));
        }
        let seq = self
            .next_seq
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        let question_id = format!("dwfq-{}-{}-{}", self.run_id, self.exec_generation, seq);
        let event = json!({
            "event": "question_opened",
            "question_id": question_id,
            "run_id": self.run_id,
            "exec_generation": self.exec_generation,
            "actor": self.actor,
            "question": truncate_chars(&question, 2_000),
            "question_key": question_key,
        });
        let store = self.store.clone();
        let journal = self.journal.clone();
        let run_id = self.run_id.clone();
        let actor = self.actor.clone();
        // 先登记等待者，再落库：避免“落库后、登记前”到达的答案被错过。
        let mut rx = self.runtime.register_escalation(&question_id);
        let (channel, chat_id) = crate::turn::rewind::current_chat()
            .unwrap_or_else(|| ("cli".to_string(), "console".to_string()));
        let session = format!("{channel}/{chat_id}");
        let notification_id = format!("wf-question-{question_id}");
        let notification = json!({
            "kind": "question",
            "run_id": run_id,
            "label": run_id,
            "question_id": question_id,
        });
        let inserted = store.transaction(|tx| {
            journal
                .append_tx(tx, &event)
                .map_err(|error| error.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO workflow_question (question_id, run_id, exec_generation, \
                     actor, question, state, created_at, question_key) \
                     VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
                rusqlite::params![
                    question_id,
                    run_id,
                    self.exec_generation as i64,
                    actor,
                    truncate_chars(&question, 2_000),
                    rclaw_store::db::now_ms(),
                    question_key
                ],
            )
            .map_err(|error| error.to_string())?;
            workflow_store::enqueue_notification_tx(
                tx,
                &notification_id,
                &run_id,
                &session,
                &channel,
                &chat_id,
                &notification,
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        });
        if let Err(error) = inserted {
            self.runtime.cancel_escalation(&question_id);
            return Err(error.to_string());
        }
        // 落库与登记之间的答案：统一从持久化状态对账。
        if let Ok(Some(question)) = self.store.get_question(&question_id) {
            if question.state == "answered" {
                if let Some(answer) = question.answer {
                    self.runtime.cancel_escalation(&question_id);
                    return Ok((question_id, answer));
                }
            }
        }
        // 只等回答或取消，不设固定超时（与 zcode 的 escalation deferred 一致）：超时把“等人
        // 回答”变成 ask 失败，子代理会凭空作答并被 journal 永久缓存，或留下无人消费的 pending
        // 问题。整体时长由 run 的 max_run_secs 兜底。
        tokio::select! {
            biased;
            _ = self.cancel.cancelled() => {
                self.runtime.cancel_escalation(&question_id);
                Err("workflow 已取消（问题保持 pending）".to_string())
            }
            answer = &mut rx => {
                let answer = answer.map_err(|_| "问题等待通道关闭".to_string())?;
                let accepted = self
                    .store
                    .answer_question_cas(&question_id, &answer)
                    .map_err(|error| error.to_string())?;
                if accepted {
                    append_workflow_event(
                        &self.journal,
                        &json!({
                            "event": "question_answered",
                            "question_id": question_id,
                            "answer": answer,
                            "answered_by": "main",
                        }),
                    );
                }
                Ok((question_id, answer))
            }
        }
    }
}

fn fnv1a64(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Fill 的 `filling` 守卫：未提交就退出时把 hole 放回 waiting（见 `fill_hole`）。
/// 同步模式 Amend 等前驱退出期间的丢弃守卫：工具 future 被丢弃（父 turn /cancel）时回滚
/// 后继（见 `rollback_sync_successor`，全是同步 SQLite 写）；等待正常结束后解除。
struct SyncAmendGuard {
    service: WorkflowService,
    predecessor: String,
    successor: RunRow,
    scope: String,
    request_id: String,
    armed: bool,
}

impl Drop for SyncAmendGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.service.rollback_sync_successor(
            &self.predecessor,
            &self.successor,
            &self.scope,
            &self.request_id,
        );
    }
}

struct FillGuard {
    store: WorkflowStore,
    run_id: String,
    hole_id: String,
    armed: bool,
}

impl Drop for FillGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(error) = self.store.revert_hole_filling(&self.run_id, &self.hole_id) {
            tracing::warn!("hole {} 回退 waiting 失败: {error}", self.hole_id);
        }
    }
}

/// 解析 `hole#<safe_name>-<script_generation>-<seq>`；`seq` 原样返回（注入的源码序号
/// 从 0 起，不能抬成 1，否则第一个 hole 会被定位到第二个调用点）。
fn parse_hole_id(hole_id: &str) -> Option<(String, u32)> {
    let rest = hole_id.strip_prefix("hole#")?;
    let (rest, seq) = rest.rsplit_once('-')?;
    let seq = seq.parse::<u32>().ok()?;
    let (name, generation) = rest.rsplit_once('-')?;
    let _ = generation.parse::<u32>().ok()?;
    Some((name.to_string(), seq))
}

fn internal_error(error: impl std::fmt::Display) -> WorkflowError {
    WorkflowError::new("internal", "internal", error.to_string())
}

/// 当前 turn 的 `(channel, chat_id)`；无 turn 作用域（嵌入直调 / 测试）回落 `cli/console`。
pub(crate) fn current_session() -> (String, String) {
    crate::turn::rewind::current_chat()
        .unwrap_or_else(|| ("cli".to_string(), "console".to_string()))
}

fn access_name(access: ToolAccess) -> &'static str {
    match access {
        ToolAccess::Sandboxed => "sandboxed",
        ToolAccess::Unrestricted => "unrestricted",
    }
}

fn access_from_config(config: &Value) -> ToolAccess {
    match config.get("tool_access").and_then(Value::as_str) {
        Some("unrestricted") => ToolAccess::Unrestricted,
        _ => ToolAccess::Sandboxed,
    }
}

fn digest_json(value: &Value) -> String {
    let text = value.to_string();
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// `request_id`（§5.1：≤128）：空白视为未传并生成；超长 → `args_invalid`。
fn normalize_request_id(request_id: Option<String>) -> Result<String, WorkflowError> {
    match request_id.filter(|id| !id.trim().is_empty()) {
        Some(id) if id.chars().count() > 128 => Err(WorkflowError::new(
            "args_invalid",
            "validation",
            "request_id 超过 128 字符",
        )),
        Some(id) => Ok(id),
        None => Ok(format!("req-{:016x}", rand::random::<u64>())),
    }
}

fn normalize_parallel(
    max_parallel_asks: Option<u32>,
    max_concurrency: Option<u32>,
    default: usize,
) -> Result<usize, WorkflowError> {
    let value = match (max_parallel_asks, max_concurrency) {
        (Some(left), Some(right)) if left != right => {
            return Err(WorkflowError::new(
                "alias_conflict",
                "validation",
                "max_parallel_asks 与 max_concurrency 不一致",
            ));
        }
        (Some(value), _) | (_, Some(value)) => value,
        (None, None) => default as u32,
    };
    if !(1..=MAX_MAX_PARALLEL_ASKS).contains(&value) {
        return Err(WorkflowError::new(
            "args_invalid",
            "validation",
            format!("max_parallel_asks 必须在 1..={MAX_MAX_PARALLEL_ASKS}"),
        ));
    }
    Ok(value as usize)
}

pub(crate) fn resolve_snapshot(
    paths: &WorkflowPaths,
    row: &RunRow,
) -> Result<std::path::PathBuf, WorkflowError> {
    if row.snapshot_path.is_empty() {
        return Err(WorkflowError::new(
            "snapshot_missing",
            "state",
            "该 run 没有不可变快照（legacy）",
        ));
    }
    let path = std::path::PathBuf::from(&row.snapshot_path);
    Ok(if path.is_absolute() {
        path
    } else {
        paths.project_root.join(path)
    })
}

pub(crate) fn split_session_key(session: &str) -> (String, String) {
    session
        .split_once('/')
        .map(|(channel, chat)| (channel.to_string(), chat.to_string()))
        .unwrap_or_else(|| (session.to_string(), String::new()))
}

fn parse_question_id(question_id: &str) -> Option<(String, u32)> {
    let rest = question_id.strip_prefix("dwfq-")?;
    let mut parts = rest.rsplitn(3, '-');
    let _seq = parts.next()?;
    let generation = parts.next()?.parse::<u32>().ok()?;
    let run_id = parts.next()?.to_string();
    Some((run_id, generation))
}

fn notification_text(payload: &Value) -> String {
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("update");
    let run_id = payload.get("run_id").and_then(Value::as_str).unwrap_or("");
    let label = payload
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("workflow");
    match kind {
        "completed" => format!(
            "[workflow] 「{label}」已完成（run {run_id}）。用 GetWorkflowRun 查看结果；运行卡片可打开产物。"
        ),
        "errored" => format!(
            "[workflow] 「{label}」失败（run {run_id}）：{}。用 GetWorkflowRun 查看 error / recovery。",
            payload.get("error").and_then(Value::as_str).unwrap_or("unknown")
        ),
        "stopped" => format!(
            "[workflow] 「{label}」已停止（run {run_id}，原因 {}）。",
            payload.get("stop_reason").and_then(Value::as_str).unwrap_or("unknown")
        ),
        "hole" => format!(
            "[workflow] 「{label}」在 hole {} 等待填充（run {run_id}）。用 FillWorkflowHole 补全函数体。",
            payload.get("hole_id").and_then(Value::as_str).unwrap_or("")
        ),
        "question" => format!(
            "[workflow] 「{label}」提出问题 {}（run {run_id}）。用 ResolveWorkflowQuestion 回答。",
            payload.get("question_id").and_then(Value::as_str).unwrap_or("")
        ),
        _ => format!("[workflow] 「{label}」状态更新（run {run_id}）。"),
    }
}

/// phases 投影：`(exec_generation, name, at)`。resume / fill 后脚本从头重跑会把 phase 再记
/// 一遍，这里只取最新一代的序列（它才是当前执行真实走到的阶段），每个阶段的 `started_at`
/// 取该名字在所有代里最早的时间戳（重放段瞬间跑完，不然早期阶段全挤在恢复那一刻）。
fn project_phases(events: &[(u32, String, i64)], status: &str) -> Vec<Value> {
    if events.is_empty() {
        return Vec::new();
    }
    let final_status = match status {
        STATUS_COMPLETED => "completed",
        STATUS_ERRORED => "failed",
        STATUS_STOPPED => "aborted",
        _ => "running",
    };
    let latest_generation = events
        .iter()
        .map(|(generation, _, _)| *generation)
        .max()
        .unwrap_or(0);
    let mut first_seen: HashMap<&str, i64> = HashMap::new();
    for (_, name, at) in events {
        let entry = first_seen.entry(name.as_str()).or_insert(*at);
        *entry = (*entry).min(*at);
    }
    let sequence: Vec<(&str, i64)> = events
        .iter()
        .filter(|(generation, _, _)| *generation == latest_generation)
        .map(|(_, name, at)| (name.as_str(), *at))
        .collect();
    // §6 phase 生命周期：遇到下一个 phase / 脚本结束 / run 终态关闭当前区间；同名 phase 的多个
    // 区间合并——总耗时相加，状态取优先级 failed > aborted > completed > running > pending。
    fn rank(status: &str) -> u8 {
        match status {
            "failed" => 4,
            "aborted" => 3,
            "completed" => 2,
            "running" => 1,
            _ => 0,
        }
    }
    struct Merged<'a> {
        status: &'a str,
        started_at: i64,
        finished_at: i64,
        duration_ms: i64,
    }
    let now = rclaw_store::db::now_ms();
    let mut order: Vec<&str> = Vec::new();
    let mut merged: HashMap<&str, Merged<'_>> = HashMap::new();
    for (index, &(name, at)) in sequence.iter().enumerate() {
        let finished_at = sequence
            .get(index + 1)
            .map(|(_, next)| *next)
            .unwrap_or(now);
        let interval_status = if index + 1 < sequence.len() {
            "completed"
        } else {
            final_status
        };
        let entry = merged.entry(name).or_insert_with(|| {
            order.push(name);
            Merged {
                status: "pending",
                started_at: first_seen.get(name).copied().unwrap_or(at),
                finished_at,
                duration_ms: 0,
            }
        });
        entry.duration_ms += (finished_at - at).max(0);
        entry.finished_at = entry.finished_at.max(finished_at);
        if rank(interval_status) > rank(entry.status) {
            entry.status = interval_status;
        }
    }
    order
        .iter()
        .take(200)
        .filter_map(|name| merged.get(name).map(|phase| (name, phase)))
        .map(|(name, phase)| {
            json!({
                "name": name,
                "status": phase.status,
                "started_at": phase.started_at,
                "finished_at": phase.finished_at,
                "duration_ms": phase.duration_ms,
            })
        })
        .collect()
}

/// §5.12：`recovery` 只给一句下一步（ZCode 的 recoveryActions 多动作数组不在本期），无则 null。
fn recovery_actions(row: &RunRow) -> Value {
    match row.status.as_str() {
        STATUS_ERRORED => json!("Call AmendWorkflow with a corrected script."),
        STATUS_STOPPED if row.superseded_by.is_none() && row.resumable => {
            json!("Call ResumeWorkflowRun to continue, or AmendWorkflow to change the script.")
        }
        STATUS_STOPPED if row.superseded_by.is_none() => {
            json!("Resolve the blocking effects in the desktop UI, then call ResumeWorkflowRun.")
        }
        STATUS_STOPPING => json!("Wait for the stopped notification, then call GetWorkflowRun."),
        _ => Value::Null,
    }
}

fn truncate_json(value: Value, max_chars: usize) -> Value {
    match value {
        Value::String(text) => Value::String(truncate_chars(&text, max_chars)),
        other => {
            let text = other.to_string();
            if text.chars().count() > max_chars {
                Value::String(truncate_chars(&text, max_chars))
            } else {
                other
            }
        }
    }
}

/// 进程树确认：当前只信任 OS 探测（Unix `kill(-pgid, 0)`）；Windows Job Object
/// 的句柄随宿主退出由 OS 回收，无法在重启后探测，返回 false（要求 UI 确认）。
fn process_tree_proven_gone(process_ids: &[String]) -> bool {
    #[cfg(unix)]
    {
        for process_id in process_ids {
            let Some(pgid) = process_id.strip_prefix("pgid:") else {
                return false;
            };
            let Ok(pgid) = pgid.parse::<i32>() else {
                return false;
            };
            let alive = unsafe { libc::kill(-pgid, 0) == 0 };
            if alive {
                return false;
            }
        }
        true
    }
    #[cfg(not(unix))]
    {
        let _ = process_ids;
        false
    }
}
