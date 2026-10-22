//! 工作流副作用（effect）台账：身份、重放、写边界与裁决。
//!
//! effect 身份不含代次：`fnv1a64("<call_site>:<invocation_index>:<op>:<args_hash>")`。
//! RClaw 的 facade 每个 op 只有单一宿主包装（`call_site` 就是 op 名），`invocation_index`
//! 是同一 `(op,args_hash)` 在本代脚本里的第几次执行——确定性重放可复现。`world.run` 给了
//! `idempotency_key` 时改用 `fnv1a64("idem:" + key)`，复用前核对 op/args_hash。
//!
//! 语义：
//! - `completed`：同代 / 前缀重放可跳过并复用结果；
//! - `failed` / `started` / `unknown`：必须经 UI 逐项裁决（skip + substitute / rerun）；
//! - 任一 live 写开始前原子关闭 amend 缓存（`run_cache_closed`）。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::{json, Value};

use super::workflow_store::WorkflowStore;
use super::EventLog;

fn fnv1a64(data: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in data.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 参数摘要：serde_json 的确定性键序（BTreeMap）。
pub fn args_hash(args: &Value) -> String {
    fnv1a64(&args.to_string()).to_string()
}

/// 一次 effect 的开始裁决。
pub enum BeginEffect {
    /// 第一次执行（或 rerun 已授权）：继续真正执行。
    Execute { key: String },
    /// 命中已完成结果 / skip 裁决：直接复用。
    Replay { value: Value },
}

/// 单代 effect 条数 / 结果累计字节上限：effect 终态事件把完整结果写进 journal（投影重建
/// 与重放缓存都靠它），`for` 里狂 `files.read` 2MB 文件的脚本不能把 journal 无限撑大。
const MAX_EFFECTS_PER_EXEC: u64 = 10_000;
const MAX_EFFECT_RESULT_BYTES: u64 = 64 * 1024 * 1024;

struct Inner {
    store: WorkflowStore,
    journal: EventLog,
    run_id: String,
    script_generation: u32,
    exec_generation: u32,
    next_call: AtomicU64,
    result_bytes: AtomicU64,
    occurrences: StdMutex<HashMap<(String, String), u32>>,
    cache_closed: AtomicBool,
}

/// effect 台账句柄（Clone 廉价）。
#[derive(Clone)]
pub struct EffectTracker {
    inner: Arc<Inner>,
}

impl EffectTracker {
    pub fn new(
        store: WorkflowStore,
        journal: EventLog,
        run_id: String,
        script_generation: u32,
        exec_generation: u32,
        cache_closed: bool,
    ) -> Self {
        EffectTracker {
            inner: Arc::new(Inner {
                store,
                journal,
                run_id,
                script_generation,
                exec_generation,
                next_call: AtomicU64::new(0),
                result_bytes: AtomicU64::new(0),
                occurrences: StdMutex::new(HashMap::new()),
                cache_closed: AtomicBool::new(cache_closed),
            }),
        }
    }

    /// 开始一次 effect：命中 completed / skip 直接返回复用值。
    pub fn begin(
        &self,
        op: &str,
        is_write: bool,
        args: &Value,
        idempotency_key: Option<&str>,
    ) -> Result<BeginEffect, String> {
        let inner = &self.inner;
        let hash = args_hash(args);
        if inner.next_call.load(Ordering::SeqCst) >= MAX_EFFECTS_PER_EXEC {
            return Err(format!(
                "effect_limit: 本代 effect 超过 {MAX_EFFECTS_PER_EXEC} 条上限"
            ));
        }
        if inner.result_bytes.load(Ordering::SeqCst) >= MAX_EFFECT_RESULT_BYTES {
            return Err(format!(
                "effect_limit: 本代 effect 结果累计超过 {MAX_EFFECT_RESULT_BYTES} 字节上限"
            ));
        }
        let call_index = inner.next_call.fetch_add(1, Ordering::SeqCst);
        let key = if let Some(idem) = idempotency_key.filter(|key| !key.trim().is_empty()) {
            format!("{:016x}", fnv1a64(&format!("idem:{idem}")))
        } else {
            let mut occurrences = inner
                .occurrences
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let index = occurrences
                .entry((op.to_string(), hash.clone()))
                .or_insert(0);
            let invocation = *index;
            *index += 1;
            format!(
                "{:016x}",
                fnv1a64(&format!("{op}:{invocation}:{op}:{hash}"))
            )
        };
        if let Some(existing) = inner.store.get_effect(&inner.run_id, &key)? {
            if existing.op != op || existing.args_hash != hash {
                return Err(format!(
                    "effect_key_conflict: {key} 已绑定 {}（args {}），本次为 {op}（args {hash}）",
                    existing.op, existing.args_hash
                ));
            }
            if existing.status == "completed" {
                let value = existing
                    .result
                    .as_deref()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(Value::Null);
                return Ok(BeginEffect::Replay { value });
            }
            if existing.blocks(inner.exec_generation) {
                return Err(format!(
                    "effect_blocked: {key} 状态为 {}，需要先由 UI 逐项裁决（skip/rerun）",
                    existing.status
                ));
            }
            if existing.decision.as_deref() == Some("skip") {
                let value = existing
                    .substitute_result
                    .as_deref()
                    .and_then(|text| serde_json::from_str(text).ok())
                    .unwrap_or(Value::Null);
                return Ok(BeginEffect::Replay { value });
            }
        }
        let preview = serde_json::to_string(args)
            .map(|text| rclaw_store::redact_sensitive_text(&text))
            .unwrap_or_default();
        let preview = super::truncate_chars(&preview, 2048);
        let close_cache = is_write
            && !inner.cache_closed.load(Ordering::SeqCst)
            && inner
                .cache_closed
                .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok();
        let event = json!({
            "event": "effect",
            "effect_key": &key,
            "call_index": call_index,
            "op": op,
            "is_write": is_write,
            "args_hash": hash,
            "status": "started",
            "script_generation": inner.script_generation,
            "exec_generation": inner.exec_generation,
        });
        inner.store.transaction(|tx| {
            let seq = inner
                .journal
                .append_tx(
                    tx,
                    &event,
                )
                .map_err(|error| format!("journal effect 写入失败: {error}"))?;
            tx.execute(
                "UPDATE workflow_effect SET decision = NULL, decision_exec_generation = NULL, \
                 substitute_result = NULL, decided_by = NULL \
                 WHERE run_id = ?1 AND effect_key = ?2 AND decision = 'rerun'",
                rusqlite::params![&inner.run_id, &key],
            )
            .map_err(|error| error.to_string())?;
            tx.execute(
                "INSERT INTO workflow_effect (run_id, effect_key, first_script_generation, \
                 last_exec_generation, call_index, op, is_write, args_hash, args_preview, status, \
                 created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'started', ?10, ?10) \
                 ON CONFLICT(run_id, effect_key) DO UPDATE SET last_exec_generation = excluded.last_exec_generation, \
                 status = 'started', result = NULL, updated_at = excluded.updated_at",
                rusqlite::params![
                    &inner.run_id,
                    &key,
                    inner.script_generation as i64,
                    inner.exec_generation as i64,
                    call_index as i64,
                    op,
                    is_write as i64,
                    hash,
                    preview,
                    rclaw_store::db::now_ms()
                ],
            )
            .map_err(|error| error.to_string())?;
            if close_cache {
                tx.execute(
                    "UPDATE workflow_run SET cache_closed = 1, updated_at = ?2 WHERE run_id = ?1",
                    rusqlite::params![&inner.run_id, rclaw_store::db::now_ms()],
                )
                .map_err(|error| error.to_string())?;
                inner
                    .journal
                    .append_tx(
                        tx,
                        &json!({
                            "event": "run_cache_closed",
                            "run_id": &inner.run_id,
                            "event_id": seq,
                            "effect_key": &key,
                        }),
                    )
                    .map_err(|error| format!("journal cache_closed 写入失败: {error}"))?;
            }
            Ok(())
        })?;
        Ok(BeginEffect::Execute { key })
    }

    /// 记录终态；`status` 为 completed / failed / unknown。
    ///
    /// 结果累计上限在这里（而不只是 `begin`）生效：`begin` 只看得到此前已结算的字节数，一次
    /// 扇出的 N 个 `files.read` 都能通过开始检查，再各自带 2MiB 结果回来。超限的结果不落
    /// journal / DB：读 effect 记 `failed`（无副作用，不阻塞 resume），写 effect 已经执行、
    /// 结果却没留住，记 `unknown` 交 UI 裁决；返回 Err 让宿主把错误交给脚本——不能记成没有
    /// 结果的 completed，重放会拿到 null。
    pub fn finish(&self, key: &str, status: &str, result: &Value) -> Result<(), String> {
        let inner = &self.inner;
        let text = serde_json::to_string(result).unwrap_or_else(|_| "null".to_string());
        let bytes = text.len() as u64;
        let total = inner.result_bytes.fetch_add(bytes, Ordering::SeqCst) + bytes;
        let (status, result, text, over_limit) = if total > MAX_EFFECT_RESULT_BYTES {
            inner.result_bytes.fetch_sub(bytes, Ordering::SeqCst);
            let error = format!(
                "effect_limit: 本代 effect 结果累计超过 {MAX_EFFECT_RESULT_BYTES} 字节上限，本次结果未保留"
            );
            let is_write = inner
                .store
                .get_effect(&inner.run_id, key)
                .ok()
                .flatten()
                .is_some_and(|row| row.is_write);
            let status = match status {
                "completed" if is_write => "unknown",
                "completed" => "failed",
                other => other,
            };
            let result = json!({"error": error});
            let text = result.to_string();
            inner
                .result_bytes
                .fetch_add(text.len() as u64, Ordering::SeqCst);
            (status, result, text, Some(error))
        } else {
            (status, result.clone(), text, None)
        };
        let result_hash = args_hash(&result);
        let event = json!({
            "event": "effect",
            "effect_key": &key,
            "status": status,
            "result": result,
            "result_hash": result_hash,
            "script_generation": inner.script_generation,
            "exec_generation": inner.exec_generation,
        });
        let outcome = inner.store.transaction(|tx| {
            inner
                .journal
                .append_tx(tx, &event)
                .map_err(|error| format!("journal effect 写入失败: {error}"))?;
            tx.execute(
                "UPDATE workflow_effect SET status = ?3, result = ?4, updated_at = ?5 \
                 WHERE run_id = ?1 AND effect_key = ?2",
                rusqlite::params![&inner.run_id, &key, status, text, rclaw_store::db::now_ms()],
            )
            .map_err(|error| error.to_string())?;
            Ok(())
        });
        if let Err(error) = outcome {
            tracing::warn!(
                "工作流 effect 终态落库失败（保留 started，resume 会要求裁决）: {error}"
            );
        }
        match over_limit {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// 无法证明进程已退出 / 超时 / 取消：记 unknown，阻止自动 resume。
    pub fn mark_unknown(&self, key: &str, detail: &str) {
        let _ = self.finish(key, "unknown", &json!({"detail": detail}));
    }
}
