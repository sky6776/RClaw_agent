//! WorkflowDriver：把一次 run 的后台执行、心跳、effect、终态事务与 outbox 串起来。
//!
//! 取消令牌 **run 自有**：不继承 `turn_cancel()`、不挂 `drop_guard`。`CreateWorkflow`
//! 提前返回后引擎继续跑；父 turn `/cancel` 默认不波及（D7），只有 `CancelWorkflowRun`
//! 取消它。

use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::workflow::{
    self, EngineOutcome, EngineRequest, DEFAULT_MAX_PARALLEL_ASKS, MAX_MAX_RUN_SECS,
};
use super::workflow_effects::EffectTracker;
use super::workflow_service::WorkflowService;
use super::workflow_snapshot;
use super::workflow_store::{
    RunRow, STATUS_COMPLETED, STATUS_ERRORED, STATUS_STOPPED, STATUS_STOPPING, STOP_MODEL,
    STOP_USER,
};
use super::{truncate_chars, EventLog};

/// 生命周期 UPDATE 条件不满足时用来回滚事务的哨兵错误（不是真实的库错误）。
const LIFECYCLE_SKIPPED: &str = "lifecycle_skipped";

/// 执行一次 run（create / resume / hole fill / amend 后继共用）。
pub(crate) async fn drive(
    service: WorkflowService,
    run_id: String,
    exec_generation: u32,
    import_from: Option<String>,
    cancel: CancellationToken,
    retune_slot: Arc<std::sync::Mutex<Option<workflow::RetuneHandle>>>,
) {
    let row = match service.inner.store.get_run(&run_id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            tracing::warn!("工作流 driver 找不到 run {run_id}");
            return;
        }
        Err(error) => {
            tracing::warn!("工作流 driver 读取 run 失败: {error}");
            return;
        }
    };
    let config = row.execution_config();
    let tool_access = config
        .get("tool_access")
        .and_then(Value::as_str)
        .map(|value| {
            if value == "unrestricted" {
                rclaw_core::ToolAccess::Unrestricted
            } else {
                rclaw_core::ToolAccess::Sandboxed
            }
        })
        .unwrap_or(rclaw_core::ToolAccess::Sandboxed);
    let snapshot_path = match super::workflow_service::resolve_snapshot(&service.inner.paths, &row)
    {
        Ok(path) => path,
        Err(error) => {
            fail_run(&service, &row, &error.code, error.message).await;
            return;
        }
    };
    let script = match workflow_snapshot::read_snapshot(&snapshot_path, &row.snapshot_hash) {
        Ok(script) => script,
        Err(error) => {
            let error = super::workflow_service::snapshot_error(error);
            fail_run(&service, &row, &error.code, error.message).await;
            return;
        }
    };
    let prepared = match workflow::prepare_script(&script, false).await {
        Ok(prepared) => prepared,
        Err(error) => {
            fail_run(&service, &row, "script_diagnostics", error).await;
            return;
        }
    };
    let journal = EventLog::workflow(service.inner.store.journal(), &run_id);
    let is_amend_start =
        row.resumed_from.is_some() && exec_generation <= 1 && row.script_generation > 1;
    let is_resume = exec_generation > 1 || row.script_generation > 1;
    let lifecycle = if is_amend_start {
        json!({
            "event": "run_started",
            "run_id": run_id,
            "script_generation": row.script_generation,
            "exec_generation": exec_generation,
            "script_hash": row.snapshot_hash,
            "snapshot_path": row.snapshot_path,
            "execution_config": config,
            "label": row.label,
            "cwd": row.cwd,
            "session_key": row.session_key,
            "started_by": row.started_by,
            "amended_from": row.resumed_from,
            "typed_asks": prepared.typed_asks,
            "insight": prepared.insight,
        })
    } else if is_resume {
        json!({
            "event": "run_resumed",
            "run_id": run_id,
            "script_generation": row.script_generation,
            "exec_generation": exec_generation,
            "script_hash": row.snapshot_hash,
            "snapshot_path": row.snapshot_path,
        })
    } else {
        json!({
            "event": "run_started",
            "run_id": run_id,
            "script_generation": row.script_generation,
            "exec_generation": exec_generation,
            "script_hash": row.snapshot_hash,
            "snapshot_path": row.snapshot_path,
            "execution_config": config,
            "label": row.label,
            "cwd": row.cwd,
            "session_key": row.session_key,
            "started_by": row.started_by,
            // 静态洞察（typed ask 数 / actor 清单 / 依赖图）随 run_started 入 journal，供观测。
            "typed_asks": prepared.typed_asks,
            "insight": prepared.insight,
        })
    };
    let lifecycle_write = {
        let store = service.inner.store.clone();
        let run_id = run_id.clone();
        let lifecycle = lifecycle.clone();
        let instance = service.inner.instance_id.clone();
        store.transaction(|tx| {
            journal
                .append_tx(tx, &lifecycle)
                .map_err(|error| error.to_string())?;
            // 围栏按“谁启动了我”的期望写死，不从行自读（自读恒真，围不住任何人）：
            // - queued（Create/Amend 后继/重启补启）：exec 不回退即可；
            // - running（Fill 事务已把 exec_generation 推到新代）：必须正好等于行里的值；
            // - stopped（Resume）：必须正好是下一代。Cancel 在 driver 注册前把行写成 stopped
            //   时，Fill 启动的 driver 带的是当前代 → 条件不成立 → 不会把已取消的 run 复活。
            // 另：driver 凭新授权启动（Resume/Amend 已校验 auth_token），fresh-auth 要求清零，
            // 上一次停止的 reason/origin 一并清掉——否则这一代被中断收口时会把陈旧的
            // `stop_origin=user` 续上，重启后 Resume/Amend 被锁死。
            let changed = tx
                .execute(
                    "UPDATE workflow_run SET status = 'running', exec_generation = ?2, \
                 script_generation = ?3, snapshot_path = ?4, snapshot_hash = ?5, \
                 owner_instance = ?6, heartbeat_at = ?7, lease_until = ?8, updated_at = ?7, \
                 requires_fresh_auth = 0, stop_reason = NULL, stop_origin = NULL, \
                 queued_reason = NULL \
                 WHERE run_id = ?1 AND ( \
                   (status = 'queued' AND ?2 >= exec_generation) OR \
                   (status = 'running' AND ?2 = exec_generation) OR \
                   (status = 'stopped' AND ?2 = exec_generation + 1))",
                    rusqlite::params![
                        run_id,
                        exec_generation as i64,
                        row.script_generation as i64,
                        row.snapshot_path,
                        row.snapshot_hash,
                        instance,
                        rclaw_store::db::now_ms(),
                        rclaw_store::db::now_ms() + 60_000
                    ],
                )
                .map_err(|error| error.to_string())?;
            if changed == 0 {
                // 条件不满足：回滚，生命周期事件不能留在 journal 里（投影重放会把它折叠成
                // running）。
                return Err(LIFECYCLE_SKIPPED.to_string());
            }
            Ok(())
        })
    };
    match lifecycle_write {
        Ok(()) => {}
        Err(error) if error.contains(LIFECYCLE_SKIPPED) => {
            tracing::info!("工作流 {run_id} 在启动前已被取消 / 状态变化，放弃执行");
            // 注册 driver 与这里之间被 Cancel 抢先置成 stopping：没有 driver 会再为它收尾，
            // 这里直接写 stopped 终态并释放配额，否则 run 永久停在 stopping、名额泄漏。
            if let Ok(Some(current)) = service.inner.store.get_run(&run_id) {
                if current.status == STATUS_STOPPING {
                    let reason = current
                        .stop_reason
                        .clone()
                        .unwrap_or_else(|| STOP_MODEL.to_string());
                    let origin = current
                        .stop_origin
                        .clone()
                        .unwrap_or_else(|| reason.clone());
                    let event = json!({
                        "event": "run_stopped",
                        "run_id": run_id,
                        "exec_generation": current.exec_generation,
                        "reason": reason,
                        "stop_origin": origin,
                        "requires_fresh_auth": (origin == STOP_USER) as i64,
                        "detail": "cancelled before the driver started",
                    });
                    if let Err(error) = service.terminal_transaction(
                        &current,
                        STATUS_STOPPED,
                        &event,
                        Some(&reason),
                        Some(&origin),
                        origin == STOP_USER,
                        Some(true),
                        None,
                    ) {
                        tracing::warn!("工作流停止终态事务失败: {error}");
                    }
                    service.release_capacity(&run_id);
                } else if current.is_terminal() {
                    // 行已是终态（并发 Resume 的第二个 driver / Cancel 抢先收口）：本次启动前
                    // acquire 的名额没人再还，这里归还（终态行不持有许可，幂等）。
                    service.release_capacity(&run_id);
                }
            }
            return;
        }
        Err(error) => {
            tracing::warn!("工作流生命周期事件写入失败: {error}");
            // 尽力收口：run 仍是活动态而没有 driver，会永久挂在 running/queued 并占着名额。
            fail_active_run(
                &service,
                &run_id,
                "internal",
                format!("workflow 生命周期事件写入失败: {error}"),
            )
            .await;
            return;
        }
    }
    // 重放表：amend 从“前驱”导入 completed asks；resume / hole 用本 run journal。
    let import_replay = config
        .get("import_replay")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let replay = if let Some(predecessor) = import_from.as_ref().filter(|_| import_replay) {
        let predecessor_log = EventLog::workflow(service.inner.store.journal(), predecessor);
        match workflow::journal_replay(&predecessor_log) {
            Ok((_hash, mut table)) => {
                for entry in table.values_mut() {
                    entry.order = None;
                }
                table
            }
            Err(error) => {
                tracing::warn!("amend 前驱 journal 导入失败（按全新执行继续）: {error}");
                Default::default()
            }
        }
    } else if journal.exists().unwrap_or(false) {
        match workflow::journal_replay(&journal) {
            Ok((hash, table)) => {
                if Some(hash) != u64::from_str_radix(&row.snapshot_hash, 16).ok() {
                    fail_run(
                        &service,
                        &row,
                        "script_mismatch",
                        "journal 的 script_hash 与快照不一致",
                    )
                    .await;
                    return;
                }
                table
            }
            Err(error) => {
                tracing::warn!("工作流 journal 重放表加载失败（按全新执行继续）: {error}");
                Default::default()
            }
        }
    } else {
        Default::default()
    };
    let effects = Arc::new(EffectTracker::new(
        service.inner.store.clone(),
        journal.clone(),
        run_id.clone(),
        row.script_generation,
        exec_generation,
        row.cache_closed,
    ));
    let args = config.get("args").cloned().unwrap_or_else(|| json!({}));
    let max_parallel_asks = config
        .get("max_parallel_asks")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_MAX_PARALLEL_ASKS as u64) as usize;
    let subagent_model = config
        .get("subagent_model")
        .and_then(Value::as_str)
        .map(str::to_string);
    let allowed_models = config
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
    let max_run_secs = config
        .get("max_run_secs")
        .and_then(Value::as_u64)
        .unwrap_or(3600)
        .clamp(1, MAX_MAX_RUN_SECS);
    let remaining_ms = (max_run_secs as i64)
        .saturating_mul(1000)
        .saturating_sub(row.elapsed_runtime_ms);
    if remaining_ms <= 0 {
        fail_run(
            &service,
            &row,
            "budget_exhausted",
            "max_run_secs 已用完（多次续跑共享同一上限）",
        )
        .await;
        return;
    }
    let channel = config
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let chat_id = config
        .get("chat_id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let chat = (!channel.is_empty() && !chat_id.is_empty()).then_some((channel, chat_id));
    let inherited = service.inner.runtime.inherited_for(tool_access, chat);
    let progress = Arc::new(workflow::ProgressCounters::default());
    let request = EngineRequest {
        run_id: run_id.clone(),
        script: prepared.exec_script,
        script_hash: prepared.script_hash,
        script_generation: row.script_generation,
        exec_generation,
        journal: journal.clone(),
        // 子令牌：引擎在墙钟耗尽 / 硬超时时取消的是自己这份，run 令牌不受影响——否则下面
        // 无法区分“被取消（stopped）”与“预算耗尽（errored）”。
        cancel: cancel.child_token(),
        max_run: Duration::from_millis(remaining_ms as u64),
        max_parallel_asks,
        subagent_model,
        allowed_models,
        args,
        project_root: service.inner.paths.project_root.clone(),
        tool_access,
        store: service.inner.store.clone(),
        paths: service.inner.paths.clone(),
        effects: Arc::clone(&effects),
        replay,
        holes_allowed: service.inner.opts.background,
        inherited,
        retune_slot,
        progress: Arc::clone(&progress),
    };
    let started = Instant::now();
    let last_accounted = Arc::new(StdMutex::new(Instant::now()));
    let heartbeat_stop = CancellationToken::new();
    // driver 任务 panic 时也要停掉心跳：否则它会给已 errored 的 run 每 15s 追加一条
    // run_progress、永不停止。
    let _heartbeat_guard = heartbeat_stop.clone().drop_guard();
    {
        let service = service.clone();
        let run_id = run_id.clone();
        let stop = heartbeat_stop.clone();
        let cancel_hb = cancel.clone();
        let last_accounted = Arc::clone(&last_accounted);
        let progress = Arc::clone(&progress);
        // run_progress 记的是跨代累计耗时（前几代已记账 + 本代已跑）：它是 journal 里预算的
        // 唯一事实源，重启投影时按 max 折叠；只记本代的话一次 resume 就把前面的账抹掉了。
        let base_elapsed_ms = row.elapsed_runtime_ms.max(0) as u64;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = cancel_hb.cancelled() => return,
                    _ = tokio::time::sleep(Duration::from_secs(15)) => {}
                }
                let elapsed = {
                    let mut last = last_accounted
                        .lock()
                        .unwrap_or_else(|poison| poison.into_inner());
                    let elapsed = last.elapsed().as_millis() as i64;
                    *last = Instant::now();
                    elapsed
                };
                if let Err(error) = service.inner.store.heartbeat(&run_id, elapsed) {
                    tracing::warn!("工作流心跳写入失败: {error}");
                }
                let event = json!({
                    "event": "run_progress",
                    "run_id": run_id,
                    "exec_generation": exec_generation,
                    "asks_done": progress.asks_done.load(std::sync::atomic::Ordering::SeqCst),
                    "asks_total": progress.asks_total.load(std::sync::atomic::Ordering::SeqCst),
                    "active_agents": progress.active_agents.load(std::sync::atomic::Ordering::SeqCst),
                    "elapsed_runtime_ms": base_elapsed_ms + started.elapsed().as_millis() as u64,
                });
                EventLog::workflow(service.inner.store.journal(), &run_id).append_lossy(&event);
            }
        });
    }
    let outcome = workflow::run_engine(request, service.inner.runtime.clone()).await;
    heartbeat_stop.cancel();
    // 取消 / 被后继取代：driver 必须以 stopped 收尾，而不是把取消当成脚本失败。
    let current = service
        .inner
        .store
        .get_run(&run_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| row.clone());
    let was_stopping = current.status == STATUS_STOPPING;
    // FillWorkflowHole 会取消旧 exec 令牌让 driver 退出；这不是 run 级取消，
    // 不能写成 stopped。只有 hole 处于 filling 且 run 未被取消时才按“exec aborted”处理。
    let hole_filling = service
        .inner
        .store
        .list_holes(&run_id)
        .map(|holes| holes.iter().any(|hole| hole.state == "filling"))
        .unwrap_or(false);
    // 心跳已按区间累计；这里只补最后一个未记账区间，避免重复计入。
    let elapsed = {
        let mut last = last_accounted
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let elapsed = last.elapsed().as_millis() as i64;
        *last = Instant::now();
        elapsed
    };
    let _ = service.inner.store.add_elapsed(&run_id, elapsed);
    match outcome {
        EngineOutcome::Completed {
            result,
            asks_total,
            asks_replayed,
        } => {
            let result_json = result.clone();
            let event = json!({
                "event": "run_completed",
                "run_id": run_id,
                "exec_generation": exec_generation,
                "asks_total": asks_total,
                "asks_replayed": asks_replayed,
                "result_json": result_json,
            });
            let terminal = service.terminal_transaction(
                &row,
                STATUS_COMPLETED,
                &event,
                None,
                None,
                false,
                Some(false),
                None,
            );
            if let Err(error) = terminal {
                tracing::warn!("工作流终态事务失败: {error}");
            }
            if let Some(result) = result {
                let _ = service.inner.store.set_result(&run_id, Some(&result), None);
            }
            service.release_capacity(&run_id);
        }
        EngineOutcome::Failed {
            error,
            budget_exhausted,
            ..
        } => {
            if hole_filling && !was_stopping {
                // 旧 exec 为补洞让路：不写终态、不释放许可。
                return;
            }
            // 墙钟耗尽不是取消：引擎只取消了自己的子令牌，run 令牌仍完好；用户 / 模型已置
            // stopping 的 run 仍按 stopped 收尾（stop_origin 决定 fresh-auth，不能被覆盖）。
            if was_stopping || (cancel.is_cancelled() && !budget_exhausted) {
                // 行上没有 stop_origin 却被取消：后台模式只可能是 Fill 回退后的令牌取消（按
                // model 记）；同步模式令牌挂在父 turn 上，取消来自用户 /cancel，必须记成 user，
                // 否则模型可以不经授权 Resume 一个用户亲手停掉的 run。
                let fallback = if service.inner.opts.background {
                    STOP_MODEL
                } else {
                    STOP_USER
                };
                let reason = current
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| fallback.to_string());
                let origin = current
                    .stop_origin
                    .clone()
                    .unwrap_or_else(|| reason.clone());
                let event = json!({
                    "event": "run_stopped",
                    "run_id": run_id,
                    "exec_generation": exec_generation,
                    "reason": reason,
                    "stop_origin": origin,
                    "requires_fresh_auth": (origin == STOP_USER) as i64,
                    "detail": truncate_chars(&error, 1_000),
                });
                let terminal = service.terminal_transaction(
                    &row,
                    STATUS_STOPPED,
                    &event,
                    Some(&reason),
                    Some(&origin),
                    origin == STOP_USER,
                    Some(true),
                    None,
                );
                if let Err(error) = terminal {
                    tracing::warn!("工作流停止终态事务失败: {error}");
                }
            } else {
                let code = if budget_exhausted {
                    "budget_exhausted"
                } else {
                    "workflow_failed"
                };
                let structured = json!({
                    "code": code,
                    "message": truncate_chars(&error, 4_000),
                    "kind": "state",
                });
                let event = json!({
                    "event": "run_failed",
                    "run_id": run_id,
                    "exec_generation": exec_generation,
                    "error": structured,
                });
                let terminal = service.terminal_transaction(
                    &row,
                    STATUS_ERRORED,
                    &event,
                    None,
                    None,
                    false,
                    Some(false),
                    Some(&truncate_chars(&error, 4_000)),
                );
                if let Err(error) = terminal {
                    tracing::warn!("工作流失败终态事务失败: {error}");
                }
            }
            service.release_capacity(&run_id);
        }
        EngineOutcome::HoleWaiting { hole, .. } => {
            if hole_filling && !was_stopping {
                return;
            }
            // 只有 run 级取消（Cancel / amend 取代都先把 run 置 stopping）才写 stopped；
            // 令牌被取消而 run 仍是 running，只可能是 Fill 等旧 exec 超时后回退了 waiting
            // ——run 继续停在 hole 上等下一次 Fill，不能把它写成 stopped。
            // 退出前再读一次：Cancel/Amend 恰好落在上面读 `current` 之后，这里不收口的话就
            // 没有执行者会再为它收口（剩余的更小窗口由租约扫描兜底）。
            let current = service
                .inner
                .store
                .get_run(&run_id)
                .ok()
                .flatten()
                .unwrap_or(current);
            if was_stopping || current.status == STATUS_STOPPING {
                let reason = current
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| STOP_MODEL.to_string());
                let origin = current
                    .stop_origin
                    .clone()
                    .unwrap_or_else(|| reason.clone());
                let event = json!({
                    "event": "run_stopped",
                    "run_id": run_id,
                    "exec_generation": exec_generation,
                    "reason": reason,
                    "stop_origin": origin,
                    "requires_fresh_auth": (origin == STOP_USER) as i64,
                    "detail": "cancelled while waiting at hole",
                });
                let _ = service.terminal_transaction(
                    &row,
                    STATUS_STOPPED,
                    &event,
                    Some(&reason),
                    Some(&origin),
                    origin == STOP_USER,
                    Some(true),
                    None,
                );
                service.release_capacity(&run_id);
                return;
            }
            // 不写终态、不释放许可；driver 退出，等 FillWorkflowHole（通知已在 hole 登记
            // 事务里入队，见 workflow::host_hole）。
            tracing::debug!(
                run_id = %run_id,
                hole_id = %hole.hole_id,
                "工作流停在 hole 等待填充"
            );
        }
    }
    // 后继交接不在这里做：driver 的每条退出路径（含 fail_run / panic 兜底 / hole 等待中被
    // 取消）退出后都由 `start_driver` 包装层统一调用 `start_claimed_successor`。
}

/// driver 任务异常退出（panic）后的兜底：run 仍在活动态才写 errored 终态并释放配额；
/// 已经终态（driver 在 panic 前已收尾）则不动。
pub(crate) async fn fail_active_run(
    service: &WorkflowService,
    run_id: &str,
    code: &str,
    message: impl Into<String>,
) {
    match service.inner.store.get_run(run_id) {
        Ok(Some(row)) if row.is_active() => fail_run(service, &row, code, message).await,
        Ok(_) => {}
        Err(error) => tracing::warn!("工作流 {run_id} 异常收尾读取 run 失败: {error}"),
    }
}

async fn fail_run(service: &WorkflowService, row: &RunRow, code: &str, message: impl Into<String>) {
    let message = message.into();
    let event = json!({
        "event": "run_failed",
        "run_id": row.run_id,
        "exec_generation": row.exec_generation,
        "error": {"code": code, "message": truncate_chars(&message, 4_000), "kind": "state"},
    });
    let _ = service.terminal_transaction(
        row,
        STATUS_ERRORED,
        &event,
        None,
        None,
        false,
        Some(false),
        Some(&message),
    );
    service.release_capacity(&row.run_id);
}

/// 同步模式的放弃守卫：工具 future 被 drop（父 turn 取消）时触发 run 令牌并补写 `stopped(user)`。
struct SyncRunGuard {
    service: WorkflowService,
    run_id: String,
    cancel: CancellationToken,
    armed: bool,
}

impl Drop for SyncRunGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // 引擎跑在 spawn_blocking 的内嵌 runtime 上，丢弃外层 future 不会连带停掉它：
        // 必须显式触发 run 令牌，在途 ask 子代理（子令牌）才会随之终止，不再后台空转。
        self.cancel.cancel();
        self.service.unregister_active(&self.run_id);
        let service = self.service.clone();
        let run_id = self.run_id.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            let Ok(Some(row)) = service.inner.store.get_run(&run_id) else {
                return;
            };
            if !row.is_active() {
                return;
            }
            let event = json!({
                "event": "run_stopped",
                "run_id": run_id,
                "exec_generation": row.exec_generation,
                "reason": STOP_USER,
                "stop_origin": STOP_USER,
                "requires_fresh_auth": 1,
                "detail": "sync run abandoned by the calling turn",
            });
            if let Err(error) = service.terminal_transaction(
                &row,
                STATUS_STOPPED,
                &event,
                Some(STOP_USER),
                Some(STOP_USER),
                true,
                Some(true),
                None,
            ) {
                tracing::warn!("同步工作流放弃收尾失败: {error}");
            }
            service.release_capacity(&run_id);
        });
    }
}

/// 同步模式：跑完再返回（`workflow_background=false`）。
pub(crate) async fn run_sync(
    service: WorkflowService,
    run_id: &str,
    exec_generation: u32,
    import_from: Option<&str>,
) -> Value {
    // 同步模式保留父 turn 取消语义（后台模式才与 turn_cancel 解耦）。
    let cancel = rclaw_tools::turn_state::turn_cancel()
        .map(|parent| parent.child_token())
        .unwrap_or_default();
    let retune_slot = Arc::new(std::sync::Mutex::new(None));
    let mut registered = false;
    for _ in 0..100 {
        if service.register_active(run_id, cancel.clone(), Arc::clone(&retune_slot)) {
            registered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    if !registered {
        return json!({
            "status": "errored",
            "run_id": run_id,
            "error": "run 仍有活动执行者（前驱尚未退出）",
        });
    }
    // 同步模式整个 drive 跑在工具 future 里：父 turn `/cancel` 会直接 drop 这个 future，
    // 后面的 unregister / 终态都不会再执行。守卫在 Drop 里补做：摘掉 active 登记，并另起
    // 任务把 run 写成 stopped(user) 归还名额；正常走完则解除。
    let mut guard = SyncRunGuard {
        service: service.clone(),
        run_id: run_id.to_string(),
        cancel: cancel.clone(),
        armed: true,
    };
    drive(
        service.clone(),
        run_id.to_string(),
        exec_generation,
        import_from.map(str::to_string),
        cancel,
        retune_slot,
    )
    .await;
    guard.armed = false;
    drop(guard);
    service.unregister_active(run_id);
    match service.inner.store.get_run(run_id) {
        Ok(Some(row)) => {
            // 终态事件：终态事务里 run_completed 之后还可能跟 question_aborted / hole_aborted，
            // 从尾部找最近一条生命周期终态事件（不是简单取 latest）。
            let event = EventLog::workflow(service.inner.store.journal(), run_id)
                .load()
                .ok()
                .and_then(|events| {
                    events
                        .into_iter()
                        .rev()
                        .map(|(_, event)| event)
                        .find(|event| {
                            matches!(
                                event.get("event").and_then(Value::as_str),
                                Some("run_completed" | "run_failed" | "run_stopped")
                            )
                        })
                });
            match row.status.as_str() {
                // §6.0 同步契约：`{status, run_id, result, asks_total, asks_replayed}`——
                // 问答计数取自终态事件 run_completed。
                STATUS_COMPLETED => json!({
                    "status": "completed",
                    "run_id": run_id,
                    "result": row.result_json,
                    "asks_total": event
                        .as_ref()
                        .and_then(|event| event.get("asks_total"))
                        .cloned()
                        .unwrap_or(json!(0)),
                    "asks_replayed": event
                        .as_ref()
                        .and_then(|event| event.get("asks_replayed"))
                        .cloned()
                        .unwrap_or(json!(0)),
                    "script_generation": row.script_generation,
                    "exec_generation": row.exec_generation,
                }),
                STATUS_ERRORED => json!({
                    "status": "errored",
                    "run_id": run_id,
                    "error": row.error,
                }),
                STATUS_STOPPED | STATUS_STOPPING => json!({
                    "status": "stopped",
                    "run_id": run_id,
                    "stop_reason": row.stop_reason,
                }),
                _ => json!({
                    "status": "completed",
                    "run_id": run_id,
                    "result": event.and_then(|event| event.get("result_json").cloned()),
                }),
            }
        }
        _ => json!({"status":"errored","run_id":run_id,"error":"run 缺失"}),
    }
}
