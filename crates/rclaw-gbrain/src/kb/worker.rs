//! KB 作业 worker — 后台轮询并处理 KB 文档作业
//!
//! 轮询 `claim_next_kb_job`，调用 `process_document_async`，
//! 然后标记作业完成或失败。可通过 CLI 子命令或 MCP 内置后台线程运行。
//! 支持多 worker 并发处理 (P5-023)。

use crate::config::Config;
use crate::embedding::Embedder;
use crate::engine::BrainEngine;
use crate::error::{GBrainError, Result};
use crate::kb::engine::KbEngine;
use crate::kb::jobs::{
    claim_next_kb_cmd_job, claim_next_kb_job, claim_next_ocr_job, complete_kb_job, fail_kb_job,
    KbIndexCommand, KbProcessPayload,
};
use crate::kb::pipeline::{cleanup_retired_version, process_document_async};
use crate::kb::raptor::RaptorConfig;
use crate::sqlite_engine::SqliteEngine;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use std::time::Duration;
use tracing::{debug, info, warn};

/// 运行一次 KB 作业处理循环：认领一个待处理作业并执行。
/// 返回是否处理了一个作业。
pub fn run_kb_worker_once(engine: &SqliteEngine, config: &Config) -> Result<bool> {
    let conn = engine.connection()?;

    // 认领下一个待处理作业
    let claimed = claim_next_kb_job(conn)?;
    let Some((job_db_id, payload)) = claimed else {
        debug!("KB worker: 无待处理作业");
        return Ok(false);
    };

    info!(
        job_db_id,
        document_id = payload.document_id,
        "KB worker: 认领作业"
    );

    // P1 修复: 创建 embedder 时优先使用库 active embedding index 的模型/维度，
    // 而不是全局 config 的默认值。这确保生成的向量与库的索引维度一致。
    let embedder: Option<Arc<Embedder>> = config.embedding_api_key.as_deref().map(|api_key| {
        let (model, dims): (String, usize) =
            match crate::kb::embedding_index::get_active_index_for_library(conn, payload.library_id)
            {
                Ok(Some(idx)) => {
                    tracing::debug!(
                        library_id = payload.library_id,
                        index_id = idx.id,
                        model = idx.model.as_str(),
                        dimensions = idx.dimensions,
                        "KB worker: 使用库 active embedding index 配置 embedder"
                    );
                    (idx.model, idx.dimensions as usize)
                }
                Ok(None) => {
                    tracing::debug!(
                        library_id = payload.library_id,
                        "KB worker: 库无 active embedding index，回退到全局 config"
                    );
                    (config.embedding_model.clone(), config.embedding_dimensions)
                }
                Err(e) => {
                    tracing::warn!(
                        library_id = payload.library_id,
                        error = %e,
                        "KB worker: 解析 active embedding index 失败，回退到全局 config"
                    );
                    (config.embedding_model.clone(), config.embedding_dimensions)
                }
            };
        Arc::new(Embedder::new(
            api_key,
            config
                .embedding_base_url
                .as_deref()
                .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
            &model,
            dims,
        ))
    });

    // 构建 RaptorConfig（使用默认值）
    let raptor_config = RaptorConfig::default();

    // 获取全局共享 tokio 运行时执行异步管道
    let rt = crate::runtime::shared_runtime();

    // 执行文档处理管道。
    //
    // P2 修复：不再在 worker 提前解析 RAPTOR 配置——resolved_raptor_config 直接传 None。
    // 之前只要 library.raptor_enabled，worker 就会先 raptor_config_resolved()?，导致
    // RAPTOR-enabled 库里的小文档（最终只有 1-2 个节点，本不会构建 RAPTOR）也因缺失
    // GBRAIN_KB_RAPTOR_* 而失败。
    //
    // 现在 RAPTOR LLM 配置由 pipeline 在真正需要时才解析：raptor::resolve_raptor_llm_config
    // 在 resolved_config 为 None 时，会自行读取 GBRAIN_KB_RAPTOR_* 环境变量（与
    // Config::raptor_config_resolved 读取同一组 env，仅是时机延后）。两个使用点均带条件守卫：
    //   - augmentation：library.augmentation_enabled && !nodes.is_empty()（解析失败时 if let Ok 静默跳过）
    //   - RAPTOR 树：  library.raptor_enabled && nodes.len() >= 3（解析失败时 match Err 静默跳过）
    // 因此小文档（不满足上述条件）不会再被缺失的 RAPTOR env 阻断；即便真正构建 RAPTOR 时
    // env 缺失，pipeline 也只是跳过摘要、不会让 process_document_async 返回 Err。
    //
    // 注意：claim_next_kb_job 已把 job 置为 running，此处到下方 match result 之间不得出现
    // 会提前返回的 `?`，否则 job 会卡在 running。当前路径唯一的 Result 来自 process_document_async，
    // 由 match 统一处理（Ok→complete，Err→fail_kb_job）。
    // panic 防护：管线内任何 panic 都会沿 block_on 展开，直接杀死 kb-worker
    // 线程且不写 tracing 日志，job 将永久卡 running（孤儿作业无人认领）。
    // catch_unwind 把 panic 转成 Err，让下方 match 正常走 fail 路径。
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rt.block_on(process_document_async(
            conn,
            &payload,
            embedder,
            Some(&raptor_config),
            config.kb_raptor_secret_ref.as_deref(),
            config.kb_raptor_base_url.as_deref(),
            if config.kb_raptor_model.is_empty() {
                None
            } else {
                Some(config.kb_raptor_model.as_str())
            },
            None,
            None,
        ))
    }))
    .unwrap_or_else(|panic_payload| {
        let detail = panic_payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic_payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| "unknown panic".to_string());
        Err(GBrainError::Internal(format!("文档处理 panic: {}", detail)))
    });

    match result {
        Ok(process_result) => {
            // P2 修复：使用显式 deferred_ocr 标记替代 word_total==0 && split_total==0 判断
            // 之前的隐式判断会误匹配合法空文档（空解析结果也会产生 0/0）
            if process_result.deferred_ocr {
                info!(
                    job_db_id,
                    document_id = payload.document_id,
                    "KB worker: PDF OCR 已异步入队，shadow/promotion 延后到 OCR 完成后执行"
                );
                complete_kb_job(conn, job_db_id)?;
                return Ok(true);
            }

            info!(
                job_db_id,
                document_id = payload.document_id,
                word_total = process_result.word_total,
                split_total = process_result.split_total,
                "KB worker: 文档处理完成"
            );

            if let Err(e) = update_shadow_pages_after_kb_success(
                conn,
                payload.document_id,
                &payload.processing_run_id,
            ) {
                warn!(
                    document_id = payload.document_id,
                    error = %e,
                    "KB worker: 更新 artifact shadow page 状态失败"
                );
            }

            // 修复：先入队 promotion 再 complete job，避免 enqueue 失败时
            // job 已完成不会重试，导致 artifact_promote_extract 永久丢失。
            // 之前先 complete_kb_job 再 enqueue，一旦 INSERT/COMMIT/查询出现
            // 瞬时 DB 错误，KB job 已完成且不会重试，promotion 入队永久丢失。
            // 现在先尝试入队，入队成功后再 complete job；
            // 入队失败时 job 保持可认领状态，下次轮询可重试。
            // 注意：enqueue_artifact_promote_if_linked 内部已用
            // BEGIN IMMEDIATE 事务保护 run_id 校验和 INSERT，不存在竞态窗口。
            if let Err(e) = enqueue_artifact_promote_if_linked(
                conn,
                payload.document_id,
                &payload.processing_run_id,
            ) {
                warn!(
                    document_id = payload.document_id,
                    error = %e,
                    "KB worker: 触发 artifact promotion 失败，job 保持可重试状态"
                );
                // 入队失败时用 fail_kb_job 标记失败（而非 complete），
                // 让 job 可被人工重试或后续恢复逻辑处理，
                // 避免无限循环重试同一 job
                fail_kb_job(conn, job_db_id, &format!("promotion 入队失败: {}", e))?;
                return Ok(true);
            }

            complete_kb_job(conn, job_db_id)?;

            // P1 修复：索引完成后自动入队摘要生成 job，实现 summary 检索链路自动闭环。
            // pipeline 不负责生成摘要，仅通过此处的 enqueue 异步触发 SummarizeDocument。
            let summary_cmd = KbIndexCommand::SummarizeDocument {
                document_id: payload.document_id,
                processing_run_id: String::new(),
            };
            if let Err(e) = crate::kb::jobs::enqueue_kb_cmd_job(conn, &summary_cmd) {
                warn!(
                    document_id = payload.document_id,
                    error = %e,
                    "KB worker: SummarizeDocument 入队失败（非致命）"
                );
            }

            // E6: enqueue synonym mining job after successful document processing.
            // The dedup guard inside ensures at most one mining job is pending at any time.
            if let Err(e) =
                crate::kb::jobs::enqueue_mine_synonyms_job(conn, Some(payload.library_id), false)
            {
                warn!(
                    document_id = payload.document_id,
                    error = %e,
                    "KB worker: synonym mining job 入队失败（非致命）"
                );
            }

            Ok(true)
        }
        Err(e) => {
            warn!(
                job_db_id,
                document_id = payload.document_id,
                error = %e,
                "KB worker: 文档处理失败"
            );
            // 修复：直接用 update_document_status_with_run_guard 做条件更新，
            // 消除 ensure_document_run_current + update_document_status 两步间的竞态。
            // 旧代码先检查 run_id 再无守卫更新，两步之间新 run 可能更新 processing_run_id，
            // 导致旧 job 把新 run 文档标为 failed。
            // 带守卫的 UPDATE 在 SQL 层面原子检查 processing_run_id，不存在竞态窗口。
            let kb = KbEngine::new(conn);
            let _ = kb.update_document_status_with_run_guard(
                payload.document_id,
                Some(crate::kb::types::STATUS_FAILED),
                None,
                Some(&e.to_string()),
                None,
                None,
                None,
                Some(&payload.processing_run_id),
            );
            if let Err(update_error) = update_shadow_pages_after_kb_failure(
                conn,
                payload.document_id,
                &payload.processing_run_id,
                &e.to_string(),
            ) {
                warn!(
                    document_id = payload.document_id,
                    error = %update_error,
                    "KB worker: 更新失败状态到 artifact shadow page 失败"
                );
            }
            // 检测 stale job：processing_run_id 不匹配意味着文档已被重新处理，
            // 重试必然继续失败（run_id 永远不会再次匹配），直接跳过重试，永久失败。
            let err_msg = e.to_string();
            if err_msg.contains("stale KB processing job") {
                // 非关键操作：即使 UPDATE 失败也要确保 fail_kb_job 被执行，
                // 否则 job 会永久卡在 running 状态。
                if let Err(e2) = conn.execute(
                    "UPDATE jobs SET max_attempts = attempts, updated_at = datetime('now') WHERE id = ?1",
                    rusqlite::params![job_db_id],
                ) {
                    tracing::warn!(
                        job_db_id,
                        error = %e2,
                        "KB worker: 设置 stale job max_attempts 失败，重试仍可能发生"
                    );
                }
            }
            fail_kb_job(conn, job_db_id, &err_msg)?;
            Ok(true)
        }
    }
}

/// P0 修复: 运行一次 KbIndexCommand 作业处理循环。
/// 认领并处理一个 KbIndexCommand（CleanupRetiredVersion / DeleteDocumentIndex 等）。
/// 返回是否处理了一个作业。
pub fn run_kb_cmd_worker_once(engine: &SqliteEngine, config: &Config) -> Result<bool> {
    let conn = engine.connection()?;

    let claimed = claim_next_kb_cmd_job(conn)?;
    let Some((job_db_id, cmd)) = claimed else {
        return Ok(false);
    };

    tracing::info!(
        job_db_id,
        cmd_type = cmd.job_type(),
        "KB cmd worker: 认领命令作业"
    );

    let result = match cmd {
        KbIndexCommand::CleanupRetiredVersion { version_id } => {
            match cleanup_retired_version(conn, version_id) {
                Ok(()) => {
                    tracing::info!(job_db_id, version_id, "KB cmd worker: 退役版本清理完成");
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        job_db_id,
                        version_id,
                        error = %e,
                        "KB cmd worker: 退役版本清理失败"
                    );
                    Err(e)
                }
            }
        }
        KbIndexCommand::DeleteDocumentIndex { document_id } => {
            // P2 修复: DeleteDocumentIndex — 清理文档的节点/向量/版本
            match crate::kb::pipeline::delete_document_index(conn, document_id) {
                Ok(()) => {
                    tracing::info!(job_db_id, document_id, "KB cmd worker: 文档索引删除完成");
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        job_db_id,
                        document_id,
                        error = %e,
                        "KB cmd worker: 文档索引删除失败"
                    );
                    Err(e)
                }
            }
        }
        KbIndexCommand::ReconcileIndexStatus { library_id } => {
            // P2 修复: ReconcileIndexStatus — 巡检修复 library 的 index_status
            match crate::kb::pipeline::reconcile_library_index_status(conn, library_id) {
                Ok(()) => {
                    tracing::info!(
                        job_db_id,
                        library_id,
                        "KB cmd worker: index_status 巡检完成"
                    );
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        job_db_id,
                        library_id,
                        error = %e,
                        "KB cmd worker: index_status 巡检失败"
                    );
                    Err(e)
                }
            }
        }
        KbIndexCommand::UpsertDocumentVersion {
            document_id,
            library_id,
            ref processing_run_id,
        } => {
            // 入队异步 pipeline job，避免同步 process_document 拒绝 PDF/image。
            // 异步 pipeline 会完成 parse → split → embed → persist 全流程。
            match conn.query_row(
                "SELECT storage_path, extension FROM kb_documents WHERE id = ?1",
                rusqlite::params![document_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            ) {
                Ok((storage_path, extension)) => {
                    let payload = KbProcessPayload {
                        kind: "kb_process_document".into(),
                        document_id,
                        library_id,
                        processing_run_id: processing_run_id.clone(),
                        storage_path,
                        extension,
                    };
                    match crate::kb::jobs::enqueue_kb_process_job(conn, &payload) {
                        Ok(job_id) => {
                            tracing::info!(
                                job_db_id,
                                document_id,
                                async_job_id = job_id,
                                "KB cmd worker: UpsertDocumentVersion 已入队异步 pipeline"
                            );
                            Ok(())
                        }
                        Err(e) => {
                            tracing::warn!(
                                job_db_id,
                                document_id,
                                error = %e,
                                "KB cmd worker: UpsertDocumentVersion 入队失败"
                            );
                            Err(e)
                        }
                    }
                }
                Err(e) => {
                    let msg = format!("无法查询文档信息: {}", e);
                    tracing::warn!(job_db_id, document_id, "{}", msg);
                    Err(GBrainError::InvalidInput(msg))
                }
            }
        }
        KbIndexCommand::SummarizeDocument {
            document_id,
            processing_run_id: _,
        } => {
            // 轻量摘要刷新：基于当前版本节点内容生成文档摘要。
            // 不走完整 pipeline（避免 PDF/image 拒绝 / 重复 chunk/embed）。
            // summarize_current_version 内部会清理旧摘要后写入新摘要。
            match crate::kb::pipeline::summarize_current_version(conn, document_id) {
                Ok(count) => {
                    tracing::info!(
                        job_db_id,
                        document_id,
                        summary_count = count,
                        "KB cmd worker: SummarizeDocument 完成"
                    );
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        job_db_id,
                        document_id,
                        error = %e,
                        "KB cmd worker: SummarizeDocument 失败"
                    );
                    Err(e)
                }
            }
        }
        KbIndexCommand::EmbedNodes {
            document_id,
            library_id,
            ref processing_run_id,
        } => match config
            .embedding_api_key
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(api_key) => {
                // P1 修复: 使用库 active embedding index 的模型/维度创建 embedder
                let (model, dims): (String, usize) =
                    match crate::kb::embedding_index::get_active_index_for_library(conn, library_id)
                    {
                        Ok(Some(idx)) => (idx.model, idx.dimensions as usize),
                        Ok(None) => (config.embedding_model.clone(), config.embedding_dimensions),
                        Err(_) => (config.embedding_model.clone(), config.embedding_dimensions),
                    };
                let embedder = Arc::new(Embedder::new(
                    api_key,
                    config
                        .embedding_base_url
                        .as_deref()
                        .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
                    &model,
                    dims,
                ));
                let rt = crate::runtime::shared_runtime();
                // panic 防护：与 run_kb_worker_once 相同，panic 沿 block_on
                // 展开会杀死 worker 线程并把 job 永久留在 running。
                let embed_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    rt.block_on(crate::kb::pipeline::embed_nodes_for_document_version(
                        conn,
                        document_id,
                        library_id,
                        processing_run_id,
                        embedder,
                    ))
                }))
                .unwrap_or_else(|panic_payload| {
                    let detail = panic_payload
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic_payload.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "unknown panic".to_string());
                    Err(GBrainError::Internal(format!("重嵌入 panic: {}", detail)))
                });
                match embed_result {
                    Ok(count) => {
                        tracing::info!(
                            job_db_id,
                            document_id,
                            embedded_nodes = count,
                            "KB cmd worker: EmbedNodes 完成"
                        );
                        Ok(())
                    }
                    Err(e) => {
                        tracing::warn!(
                            job_db_id,
                            document_id,
                            error = %e,
                            "KB cmd worker: EmbedNodes 失败"
                        );
                        Err(e)
                    }
                }
            }
            None => {
                let e =
                    GBrainError::InvalidInput("EmbedNodes 需要配置 embedding_api_key".to_string());
                tracing::warn!(
                    job_db_id,
                    document_id,
                    error = %e,
                    "KB cmd worker: EmbedNodes 失败"
                );
                Err(e)
            }
        },
        KbIndexCommand::FinalizeIndex {
            document_id,
            library_id,
            ref processing_run_id,
        } => {
            match crate::kb::pipeline::finalize_index_version(
                conn,
                document_id,
                library_id,
                processing_run_id,
            ) {
                Ok(version_id) => {
                    tracing::info!(
                        job_db_id,
                        document_id,
                        version_id,
                        "KB cmd worker: FinalizeIndex 完成"
                    );
                    let summary_cmd = KbIndexCommand::SummarizeDocument {
                        document_id,
                        processing_run_id: String::new(),
                    };
                    if let Err(e) = crate::kb::jobs::enqueue_kb_cmd_job(conn, &summary_cmd) {
                        tracing::warn!(
                            job_db_id,
                            document_id,
                            error = %e,
                            "KB cmd worker: FinalizeIndex 后入队 SummarizeDocument 失败（非致命）"
                        );
                    }
                    Ok(())
                }
                Err(e) => {
                    tracing::warn!(
                        job_db_id,
                        document_id,
                        error = %e,
                        "KB cmd worker: FinalizeIndex 失败"
                    );
                    Err(e)
                }
            }
        }
    };

    match result {
        Ok(()) => {
            complete_kb_job(conn, job_db_id)?;
            Ok(true)
        }
        Err(e) => {
            fail_kb_job(conn, job_db_id, &e.to_string())?;
            Ok(true)
        }
    }
}

fn update_shadow_pages_after_kb_success(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
) -> Result<usize> {
    with_current_document_run(conn, document_id, run_id, || {
        let (embedding_status, embedding_error, word_total, split_total): (i32, String, i64, i64) =
            conn.query_row(
                "SELECT embedding_status, embedding_error, word_total, split_total
             FROM kb_documents WHERE id = ?1",
                rusqlite::params![document_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| GBrainError::Database(format!("查询 KB 文档状态失败: {}", e)))?;

        let summary = if embedding_status == crate::kb::types::STATUS_FAILED {
            format!(
                "KB text extracted and keyword indexed, but embedding failed: {}",
                truncate_shadow_status_detail(&embedding_error)
            )
        } else {
            format!(
                "KB processing completed. Text extracted and indexed. Chunks: {}. Words: {}.",
                split_total, word_total
            )
        };

        update_shadow_pages_for_kb_document(conn, document_id, &summary)
    })
}

fn update_shadow_pages_after_kb_failure(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    error: &str,
) -> Result<usize> {
    let summary = format!(
        "KB processing failed: {}",
        truncate_shadow_status_detail(error)
    );
    with_current_document_run(conn, document_id, run_id, || {
        update_shadow_pages_for_kb_document(conn, document_id, &summary)
    })
}

fn finalize_artifact_after_kb_success(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    context: &str,
) -> Result<()> {
    if let Err(e) = update_shadow_pages_after_kb_success(conn, document_id, run_id) {
        warn!(
            document_id,
            error = %e,
            context = context,
            "KB worker: 更新 artifact shadow page 状态失败"
        );
    }

    enqueue_artifact_promote_if_linked(conn, document_id, run_id)
}

fn finalize_ocr_writeback_reembed_if_needed(
    conn: &Connection,
    job_db_id: i64,
    payload: &serde_json::Value,
    document_id: i64,
    context: &str,
) -> Result<bool> {
    if payload.get("source").and_then(|v| v.as_str()) != Some("ocr_writeback") {
        return Ok(false);
    }

    let Some(run_id) = payload.get("processing_run_id").and_then(|v| v.as_str()) else {
        fail_kb_job(
            conn,
            job_db_id,
            "ocr_writeback re-embed payload missing processing_run_id",
        )?;
        return Ok(true);
    };

    if let Err(e) = finalize_artifact_after_kb_success(conn, document_id, run_id, context) {
        warn!(
            document_id,
            error = %e,
            "re-embed worker: 触发 artifact promotion 失败，job 保持可重试状态"
        );
        fail_kb_job(conn, job_db_id, &format!("promotion 入队失败: {}", e))?;
        return Ok(true);
    }

    Ok(false)
}

fn with_current_document_run<F>(
    conn: &Connection,
    document_id: i64,
    run_id: &str,
    update: F,
) -> Result<usize>
where
    F: FnOnce() -> Result<usize>,
{
    conn.execute("BEGIN IMMEDIATE", [])
        .map_err(|e| GBrainError::Database(format!("开启 shadow page 状态回写事务失败: {}", e)))?;

    let result = (|| -> Result<usize> {
        if !document_run_is_current(conn, document_id, run_id)? {
            debug!(
                document_id,
                expected_run_id = run_id,
                "KB worker: shadow page 状态回写时 run_id 已过期，跳过"
            );
            return Ok(0);
        }

        update()
    })();

    match result {
        Ok(updated) => {
            conn.execute("COMMIT", []).map_err(|e| {
                let _ = conn.execute("ROLLBACK", []);
                GBrainError::Database(format!("提交 shadow page 状态回写失败: {}", e))
            })?;
            Ok(updated)
        }
        Err(e) => {
            let _ = conn.execute("ROLLBACK", []);
            Err(e)
        }
    }
}

fn document_run_is_current(conn: &Connection, document_id: i64, run_id: &str) -> Result<bool> {
    let current_run_id: Option<String> = match conn.query_row(
        "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
        rusqlite::params![document_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(current_run_id) => Some(current_run_id),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            return Err(GBrainError::Database(format!(
                "查询 KB 文档 run_id 失败: {}",
                e
            )))
        }
    };

    Ok(current_run_id.as_deref() == Some(run_id))
}

fn update_shadow_pages_for_kb_document(
    conn: &Connection,
    document_id: i64,
    summary: &str,
) -> Result<usize> {
    let projection_ref = format!("kb_document:{}", document_id);
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT sp.projection_ref
             FROM artifact_projections kb
             JOIN artifact_projections sp
               ON sp.artifact_id = kb.artifact_id
              AND COALESCE(sp.occurrence_id, -1) = COALESCE(kb.occurrence_id, -1)
             WHERE kb.projection_type = 'kb_document'
               AND kb.projection_ref = ?1
               AND kb.status = 'active'
               AND sp.projection_type = 'brain_shadow_page'
               AND sp.status = 'active'",
        )
        .map_err(|e| GBrainError::Database(format!("查询 shadow page 投影失败: {}", e)))?;

    let rows = stmt
        .query_map(rusqlite::params![projection_ref], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|e| GBrainError::Database(format!("遍历 shadow page 投影失败: {}", e)))?;

    let mut updated = 0usize;
    for row in rows {
        let projection_ref = row.map_err(|e| GBrainError::Database(e.to_string()))?;
        let slug = projection_ref
            .strip_prefix("slug:")
            .unwrap_or(&projection_ref)
            .to_string();
        if update_shadow_page_summary(conn, &slug, summary)? {
            updated += 1;
        }
    }

    Ok(updated)
}

fn update_shadow_page_summary(conn: &Connection, slug: &str, summary: &str) -> Result<bool> {
    let page: Option<(i64, String, String)> = match conn.query_row(
        "SELECT id, title, compiled_truth FROM pages
         WHERE slug = ?1 AND deleted_at IS NULL",
        rusqlite::params![slug],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ) {
        Ok(page) => Some(page),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            return Err(GBrainError::Database(format!(
                "查询 shadow page 失败: {}",
                e
            )))
        }
    };

    let Some((page_id, title, body)) = page else {
        return Ok(false);
    };

    let updated_body = set_shadow_summary(&body, summary);
    if updated_body == body {
        return Ok(false);
    }

    let truth_tokens = crate::nlp::chinese::tokenize_content(&updated_body);
    let content_hash = page_content_hash(&title, &updated_body);
    conn.execute(
        "UPDATE pages
         SET compiled_truth = ?1,
             compiled_truth_tokens = ?2,
             content_hash = ?3,
             updated_at = datetime('now')
         WHERE id = ?4",
        rusqlite::params![updated_body, truth_tokens, content_hash, page_id],
    )
    .map_err(|e| GBrainError::Database(format!("更新 shadow page 状态失败: {}", e)))?;

    rebuild_shadow_page_chunks(conn, page_id, &updated_body)?;
    Ok(true)
}

fn set_shadow_summary(body: &str, summary: &str) -> String {
    const HEADING: &str = "## Summary";
    let summary = summary.trim();
    let Some(start) = body.find(HEADING) else {
        return format!("{}\n\n{}\n\n{}", body.trim_end(), HEADING, summary);
    };

    let content_start = start + HEADING.len();
    let suffix = body[content_start..]
        .find("\n\n## ")
        .map(|offset| &body[content_start + offset..])
        .unwrap_or("");
    format!("{}\n\n{}{}", &body[..content_start], summary, suffix)
}

fn rebuild_shadow_page_chunks(conn: &Connection, page_id: i64, body: &str) -> Result<()> {
    // L1: 清理旧的 vec/embedding 数据，失败时 warn 而非静默吞错
    if let Err(e) = conn.execute(
        "DELETE FROM vec_chunks
         WHERE chunk_id IN (SELECT id FROM chunks WHERE page_id = ?1)",
        rusqlite::params![page_id],
    ) {
        tracing::warn!(page_id, error = %e, "清理 vec_chunks 失败");
    }
    if let Err(e) = conn.execute(
        "DELETE FROM chunk_embeddings
         WHERE chunk_id IN (SELECT id FROM chunks WHERE page_id = ?1)",
        rusqlite::params![page_id],
    ) {
        tracing::warn!(page_id, error = %e, "清理 chunk_embeddings 失败");
    }
    conn.execute(
        "DELETE FROM chunks WHERE page_id = ?1",
        rusqlite::params![page_id],
    )
    .map_err(|e| GBrainError::Database(format!("删除 shadow page 旧 chunk 失败: {}", e)))?;

    let chunks =
        crate::chunker::chunk_page_content_transactional(body, &crate::types::PageType::Source);
    for chunk in &chunks {
        let chunk_text_tokens = crate::nlp::chinese::tokenize_content(&chunk.chunk_text);
        conn.execute(
            "INSERT INTO chunks
                (page_id, chunk_index, chunk_text, chunk_text_tokens, token_count, chunk_source, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'body', datetime('now'))",
            rusqlite::params![
                page_id,
                chunk.chunk_index,
                chunk.chunk_text,
                chunk_text_tokens,
                chunk.token_count
            ],
        )
        .map_err(|e| GBrainError::Database(format!("创建 shadow page chunk 失败: {}", e)))?;
    }

    Ok(())
}

fn page_content_hash(title: &str, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(title.as_bytes());
    hasher.update(body.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn truncate_shadow_status_detail(detail: &str) -> String {
    let mut truncated: String = detail.chars().take(500).collect();
    if detail.chars().count() > 500 {
        truncated.push_str("...");
    }
    truncated
}

/// 运行一次 re-embed 作业处理循环：认领一个 kb_reembed 或 kb_reembed_node 作业并处理。
/// 返回是否处理了一个作业。
pub fn run_reembed_worker_once(engine: &SqliteEngine, config: &Config) -> Result<bool> {
    let conn = engine.connection()?;
    let claimed = crate::kb::jobs::claim_next_reembed_job(conn)?;
    let Some((job_db_id, job_type, payload)) = claimed else {
        return Ok(false);
    };

    let rt = crate::runtime::shared_runtime();

    // P1 修复: 在创建 embedder 之前，先解析目标库的 active embedding index，
    // 使用库实际的 model/dimensions 而非全局 config。否则目标 index 与全局配置
    // 不一致时会把错模型/错维度的向量写入索引。
    let resolve_embedder_config =
        |conn: &Connection, library_id: i64| -> (Option<Arc<Embedder>>, String) {
            let (model, dims): (String, usize) =
                match crate::kb::embedding_index::get_active_index_for_library(conn, library_id) {
                    Ok(Some(idx)) => {
                        tracing::debug!(
                            library_id,
                            index_id = idx.id,
                            model = idx.model.as_str(),
                            dimensions = idx.dimensions,
                            "re-embed worker: 使用库 active embedding index 配置 embedder"
                        );
                        (idx.model, idx.dimensions as usize)
                    }
                    Ok(None) => {
                        tracing::debug!(
                            library_id,
                            "re-embed worker: 库无 active embedding index，回退到全局 config"
                        );
                        (config.embedding_model.clone(), config.embedding_dimensions)
                    }
                    Err(e) => {
                        tracing::warn!(
                            library_id,
                            error = %e,
                            "re-embed worker: 解析 active embedding index 失败，回退到全局 config"
                        );
                        (config.embedding_model.clone(), config.embedding_dimensions)
                    }
                };
            let embedder = config.embedding_api_key.as_deref().map(|api_key| {
                Arc::new(Embedder::new(
                    api_key,
                    config
                        .embedding_base_url
                        .as_deref()
                        .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
                    &model,
                    dims,
                ))
            });
            (embedder, model)
        };

    let result = match job_type.as_str() {
        "kb_reembed_node" => {
            // 单节点修复：读取 node_id，嵌入内容，写入 kb_node_embeddings
            let node_id: i64 = payload.get("node_id").and_then(|v| v.as_i64()).unwrap_or(0);
            if node_id == 0 {
                fail_kb_job(
                    conn,
                    job_db_id,
                    "missing node_id in kb_reembed_node payload",
                )?;
                return Ok(true);
            }

            // P1 修复: 从节点所在库解析 active index 的模型/维度，而非用全局 config
            let library_id: i64 = conn
                .query_row(
                    "SELECT library_id FROM kb_document_nodes WHERE id = ?1",
                    rusqlite::params![node_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let (embedder, model) = resolve_embedder_config(conn, library_id);
            reembed_single_node(conn, &embedder, rt, node_id, None, &model)
        }
        "kb_reembed" => {
            // 文档级重嵌入：读取所有无 embedding 的节点，逐个嵌入
            let doc_id: i64 = payload
                .get("document_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            let target_index_id: i64 = payload
                .get("target_embedding_index_id")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);

            // run_id 守卫：如果 payload 携带 processing_run_id，校验是否与文档当前 run 一致。
            // 文档被重新处理时 run_id 会变更，旧的 re-embed job 应被丢弃。
            if let Some(job_run_id) = payload.get("processing_run_id").and_then(|v| v.as_str()) {
                let current_run_id: String = conn
                    .query_row(
                        "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                        rusqlite::params![doc_id],
                        |row| row.get::<_, String>(0),
                    )
                    .unwrap_or_default();
                if current_run_id != job_run_id {
                    tracing::warn!(
                        document_id = doc_id,
                        expected = %job_run_id,
                        actual = %current_run_id,
                        "re-embed worker: run_id 已过期，跳过"
                    );
                    complete_kb_job(conn, job_db_id)?;
                    return Ok(true);
                }
            }

            // P1 修复: 根据 target_embedding_index_id 决定用哪个 index 的模型/维度创建 embedder。
            // target>0 时查目标 index 的 model/dimensions，避免用 active index 的模型把向量写错维度。
            // target==0 时才解析文档当前 active index。
            let library_id: i64 = conn
                .query_row(
                    "SELECT library_id FROM kb_documents WHERE id = ?1",
                    rusqlite::params![doc_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);
            let (embedder, model, target_dims) = if target_index_id > 0 {
                // 显式指定目标 index：按目标 index 的 model/dimensions 创建 embedder
                match conn.query_row(
                    "SELECT model, dimensions FROM kb_embedding_indexes WHERE id = ?1",
                    rusqlite::params![target_index_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i32>(1)?)),
                ) {
                    Ok((target_model, target_dimensions)) => {
                        tracing::debug!(
                            target_index_id,
                            model = target_model.as_str(),
                            dimensions = target_dimensions,
                            "re-embed worker: 使用目标 embedding index 的模型/维度"
                        );
                        let embedder = config.embedding_api_key.as_deref().map(|api_key| {
                            Arc::new(Embedder::new(
                                api_key,
                                config
                                    .embedding_base_url
                                    .as_deref()
                                    .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
                                &target_model,
                                target_dimensions as usize,
                            ))
                        });
                        (embedder, target_model, Some(target_dimensions))
                    }
                    Err(_) => {
                        // P2 修复: 目标 index 不存在时直接 fail job，不静默回退。
                        // 回退到 active index 会写错索引，且 job 仍 complete——payload 明确
                        // 指定的目标丢失了，属于不可恢复的配置错误。
                        let err_msg = format!(
                            "re-embed worker: 目标 embedding index (id={}) 不存在，job 失败",
                            target_index_id
                        );
                        tracing::error!(target_index_id, "{}", err_msg);
                        fail_kb_job(conn, job_db_id, &err_msg)?;
                        return Ok(true);
                    }
                }
            } else {
                // target==0：解析文档当前 active index
                let (embedder, model) = resolve_embedder_config(conn, library_id);
                (embedder, model, None)
            };

            // 外部 embedding 始终允许，不再检查库级策略

            // 未配置 embedding API key 时，跳过嵌入并标记为 STATUS_SKIPPED。
            // 节点已有内容可用于关键词检索，不应让文档卡在 pending/processing。
            if embedder.is_none() {
                tracing::warn!(
                    document_id = doc_id,
                    "re-embed worker: 未配置 embedding API key，标记为 skipped（keyword_only）"
                );
                let (word_total, split_total): (i32, i32) = conn
                    .query_row(
                        "SELECT word_total, split_total FROM kb_documents WHERE id = ?1",
                        rusqlite::params![doc_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .unwrap_or((0, 0));
                let kb = KbEngine::new(conn);
                if let Err(e) = kb.update_document_stats_with_run_guard(
                    doc_id,
                    word_total,
                    split_total,
                    Some(crate::kb::types::STATUS_SKIPPED),
                    payload.get("processing_run_id").and_then(|v| v.as_str()),
                ) {
                    warn!(
                        document_id = doc_id,
                        error = %e,
                        "re-embed worker: keyword_only 状态更新失败"
                    );
                    fail_kb_job(
                        conn,
                        job_db_id,
                        &format!("keyword_only 状态更新失败: {}", e),
                    )?;
                    return Ok(true);
                }
                if finalize_ocr_writeback_reembed_if_needed(
                    conn,
                    job_db_id,
                    &payload,
                    doc_id,
                    "reembed_worker_missing_embedding_key",
                )? {
                    return Ok(true);
                }
                complete_kb_job(conn, job_db_id)?;
                return Ok(true);
            }

            reembed_document_nodes(
                conn,
                &embedder,
                rt,
                doc_id,
                target_index_id,
                &model,
                target_dims,
            )
        }
        _ => Err(GBrainError::InvalidInput(format!(
            "未知 re-embed 作业类型: {}",
            job_type
        ))),
    };

    match result {
        Ok(count) => {
            info!(job_db_id, node_count = count, "re-embed worker: 处理完成");

            // 修复：kb_reembed 成功后更新文档 embedding 状态为 COMPLETED。
            // OCR writeback 会将 embedding_status 标记为 STATUS_PENDING 并入队此 job，
            // 此处补齐向量后应将状态更新为 COMPLETED，否则文档永远停在 "processing"。
            if job_type == "kb_reembed" {
                let doc_id: i64 = payload
                    .get("document_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                if doc_id > 0 {
                    // 读取当前 word_total/split_total，避免传 0 覆盖已有值
                    let (word_total, split_total): (i32, i32) = conn
                        .query_row(
                            "SELECT word_total, split_total FROM kb_documents WHERE id = ?1",
                            rusqlite::params![doc_id],
                            |row| Ok((row.get(0)?, row.get(1)?)),
                        )
                        .unwrap_or((0, 0));
                    let kb = KbEngine::new(conn);
                    let run_id = payload.get("processing_run_id").and_then(|v| v.as_str());
                    if let Err(e) = kb.update_document_stats_with_run_guard(
                        doc_id,
                        word_total,
                        split_total,
                        Some(crate::kb::types::STATUS_COMPLETED),
                        run_id,
                    ) {
                        // run guard 失败意味着文档已有新 run，不应继续操作
                        tracing::warn!(
                            document_id = doc_id,
                            error = %e,
                            "re-embed worker: run guard 失败，跳过后续操作（stale job）"
                        );
                        complete_kb_job(conn, job_db_id)?;
                        return Ok(true);
                    }

                    // re-embed 后重建 RAPTOR 树。
                    // OCR writeback 只创建叶节点，re-embed 补齐向量，
                    // 但 RAPTOR 父节点（摘要/聚类节点）需要在此步重建。
                    let library_id: i64 = conn
                        .query_row(
                            "SELECT library_id FROM kb_document_nodes WHERE document_id = ?1 LIMIT 1",
                            rusqlite::params![doc_id],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap_or(0);
                    if library_id > 0 {
                        let kb2 = KbEngine::new(conn);
                        if let Ok(library) = kb2.get_library(library_id) {
                            // P0 修复: 查询当前版本 ID，显式传入 rebuild
                            let version_id: Option<i64> = conn
                                .query_row(
                                    "SELECT current_version_id FROM kb_documents WHERE id = ?1",
                                    rusqlite::params![doc_id],
                                    |row| row.get(0),
                                )
                                .ok();
                            if let Err(e) = rebuild_raptor_after_reembed(
                                conn, rt, doc_id, version_id, &library, config, run_id,
                            ) {
                                tracing::warn!(
                                    document_id = doc_id,
                                    error = %e,
                                    "re-embed worker: RAPTOR 重建失败（不影响 embedding 结果）"
                                );
                            }
                        }
                    }

                    if finalize_ocr_writeback_reembed_if_needed(
                        conn,
                        job_db_id,
                        &payload,
                        doc_id,
                        "reembed_worker",
                    )? {
                        return Ok(true);
                    }
                }
            }

            // P3 修复: reembed 成功写入后递增检索缓存版本。
            // 缓存 TTL 为 30 秒，不递增会让重嵌入后立即查询仍可能复用旧候选集。
            if count > 0 {
                if let Err(e) =
                    crate::kb::embedding_index::increment_index_version(conn, "retrieval_cache")
                {
                    tracing::warn!(
                        error = %e,
                        "re-embed worker: 递增检索缓存版本失败，缓存可能返回过期结果"
                    );
                }
            }

            complete_kb_job(conn, job_db_id)?;
            Ok(true)
        }
        Err(e) => {
            warn!(job_db_id, error = %e, "re-embed worker: 处理失败");
            fail_kb_job(conn, job_db_id, &e.to_string())?;
            Ok(true)
        }
    }
}

/// 运行一次 artifact 作业处理循环（设计文档 §8.8）：
/// 认领 `artifact_promote_extract` 作业并执行 promotion extraction。
/// 返回是否处理了一个作业。
///
/// 旧名 `run_artifact_worker_once` 已重命名，
/// 对外不再暴露 "promote" 命名细节。
pub fn run_artifact_worker_once(engine: &SqliteEngine, _config: &Config) -> Result<bool> {
    let conn = engine.connection()?;
    let queue = crate::jobs::JobQueue::new(conn);

    // 认领下一个 artifact_promote_extract 作业
    let job = queue
        .dequeue_by_type("artifact_promote_extract")
        .map_err(|e| GBrainError::Database(format!("认领 artifact_promote_extract 失败: {}", e)))?;

    let Some(job) = job else {
        return Ok(false);
    };

    let artifact_id: i64 = job
        .payload
        .get("artifact_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let kb_document_id: i64 = job
        .payload
        .get("kb_document_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    // 修复：优先使用 payload 中的稳定值，避免旧 job 被归属到新 occurrence 后串策略
    // payload 中的 occurrence_id 和 promotion_policy 是入队时绑定的，不会被后续上传覆盖
    let payload_occurrence_id: i64 = job
        .payload
        .get("occurrence_id")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let payload_promotion_policy: Option<String> = job
        .payload
        .get("promotion_policy")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    info!(
        job_id = job.id,
        artifact_id, kb_document_id, payload_occurrence_id, "artifact promote worker: 认领作业"
    );

    if artifact_id == 0 || kb_document_id == 0 {
        fail_kb_job(conn, job.id, "artifact_id 或 kb_document_id 无效")?;
        return Ok(true);
    }

    // 修复：优先使用 payload 中的 occurrence_id（入队时绑定的稳定值），
    // 仅在 payload 未携带时才从 projection 反查（兼容旧 job）
    let occurrence_id = if payload_occurrence_id > 0 {
        // 校验 projection 仍匹配 payload 中的值
        let proj_ref = format!("kb_document:{}", kb_document_id);
        let current_occ: Option<i64> = conn
            .query_row(
                "SELECT occurrence_id FROM artifact_projections
                 WHERE artifact_id = ?1 AND projection_type = 'kb_document' AND projection_ref = ?2 AND status = 'active'
                 LIMIT 1",
                rusqlite::params![artifact_id, proj_ref],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten();
        if current_occ != Some(payload_occurrence_id) {
            warn!(
                artifact_id,
                payload_occurrence_id,
                current_occ,
                "artifact promote worker: projection occurrence_id 已变更，使用 payload 中的稳定值"
            );
        }
        payload_occurrence_id
    } else {
        // 兼容旧 job（payload 未携带 occurrence_id），从 projection 反查
        let proj_ref = format!("kb_document:{}", kb_document_id);
        conn
            .query_row(
                "SELECT occurrence_id FROM artifact_projections
                 WHERE artifact_id = ?1 AND projection_type = 'kb_document' AND projection_ref = ?2 AND status = 'active'
                 LIMIT 1",
                rusqlite::params![artifact_id, proj_ref],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten()
            .unwrap_or(0)
    };

    // 执行 promotion extraction
    let result = crate::artifact::promotion::extract_promotion_candidates(
        conn,
        artifact_id,
        occurrence_id,
        kb_document_id,
    );

    match result {
        Ok(candidates) => {
            info!(
                job_id = job.id,
                artifact_id,
                candidate_count = candidates.len(),
                "artifact promote worker: 提取完成"
            );

            // 修复：先执行 auto_apply 再 complete job，与 KB worker 的 promotion 入队逻辑一致。
            // 之前先 complete_kb_job 再 auto_apply，如果 auto_apply 失败，
            // job 已完成不会重试，低风险候选永远不会被自动应用。
            // 现在先尝试 auto_apply，成功后再 complete job；
            // auto_apply 失败时用 fail_kb_job 标记失败，确保可重试。
            let promotion_policy = payload_promotion_policy.unwrap_or_else(|| {
                if occurrence_id > 0 {
                    crate::artifact::store::find_occurrence_by_id(conn, occurrence_id)
                        .ok()
                        .flatten()
                        .map(|o| o.promotion_policy)
                        .unwrap_or_else(|| "candidate".to_string())
                } else {
                    get_promotion_policy(conn, artifact_id)
                }
            });
            match promotion_policy.as_str() {
                "auto_accept_low_risk" => {
                    // 修复：传入 occurrence_id，只自动应用本次上传产生的候选，
                    // 避免旧候选被后续重复上传自动提升
                    if let Err(e) = crate::artifact::promotion::auto_apply_candidates(
                        conn,
                        artifact_id,
                        Some(kb_document_id),
                        Some(occurrence_id),
                    ) {
                        warn!(
                            artifact_id,
                            error = %e,
                            "artifact promote worker: 自动应用低风险候选失败，job 保持可重试状态"
                        );
                        // auto_apply 失败时标记 job 为失败，确保可重试
                        fail_kb_job(conn, job.id, &format!("自动应用低风险候选失败: {}", e))?;
                        return Ok(true);
                    }
                }
                "auto_apply" | "auto-apply" | "auto_all" | "auto-all" | "auto-apply-all" => {
                    if let Err(e) = crate::artifact::promotion::auto_apply_all_candidates(
                        conn,
                        artifact_id,
                        Some(kb_document_id),
                        Some(occurrence_id),
                    ) {
                        warn!(
                            artifact_id,
                            error = %e,
                            "artifact promote worker: 自动应用全部候选失败，job 保持可重试状态"
                        );
                        fail_kb_job(conn, job.id, &format!("自动应用全部候选失败: {}", e))?;
                        return Ok(true);
                    }
                }
                _ => {}
            }

            complete_kb_job(conn, job.id)?;

            Ok(true)
        }
        Err(e) => {
            warn!(job_id = job.id, artifact_id, error = %e, "artifact promote worker: 提取失败");
            fail_kb_job(conn, job.id, &e.to_string())?;
            Ok(true)
        }
    }
}

// ---------------------------------------------------------------------------
// E6: Token synonym mining worker
// ---------------------------------------------------------------------------

/// Process a single synonym mining job, if one is pending.
/// Returns `true` if a job was processed, `false` if no job was available.
pub fn run_mine_synonyms_worker_once(engine: &SqliteEngine, config: &Config) -> Result<bool> {
    let conn = engine.connection()?;
    let Some((job_db_id, payload)) = crate::kb::jobs::claim_next_mine_synonyms_job(conn)? else {
        return Ok(false);
    };

    info!(job_db_id, "Synonym mining worker: 认领作业");

    let Some(api_key) = config.embedding_api_key.as_deref() else {
        crate::kb::jobs::fail_kb_job(conn, job_db_id, "No embedding API key configured")?;
        return Ok(true);
    };

    let embedder = Embedder::new(
        api_key,
        config
            .embedding_base_url
            .as_deref()
            .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
        &config.embedding_model,
        config.embedding_dimensions,
    );

    let rt = crate::runtime::shared_runtime();

    let opts = crate::kb::synonyms::MineSynonymsOpts {
        library_id: payload.library_id,
        full: payload.full,
        ..Default::default()
    };

    match crate::kb::synonyms::mine_synonyms(
        conn,
        &embedder,
        config.embedding_dimensions as i32,
        rt,
        &opts,
    ) {
        Ok(stats) => {
            info!(
                job_db_id,
                candidates = stats.candidates,
                new_embeddings = stats.new_embeddings,
                total_embeddings = stats.total_embeddings,
                synonyms_written = stats.synonyms_written,
                "Synonym mining worker: 挖掘完成"
            );
            crate::kb::jobs::complete_kb_job(conn, job_db_id)?;
            Ok(true)
        }
        Err(e) => {
            warn!(job_db_id, error = %e, "Synonym mining worker: 挖掘失败");
            crate::kb::jobs::fail_kb_job(conn, job_db_id, &e.to_string())?;
            Ok(true)
        }
    }
}
// ---------------------------------------------------------------------------
// Phase 3: OCR 异步作业处理
// ---------------------------------------------------------------------------

// 运行一次 OCR 作业处理循环：认领一个 kb_ocr_document 作业并执行。
// 返回是否处理了一个作业。
//
// 流程：
// 1. 认领 kb_ocr_document 作业
// 2. 校验 processing_run_id 仍为当前值（防止旧 job 覆盖新上传）
// 3. 读取 PDF 文件并解析文本层
// 4. 规划 OCR 请求（按 pages 列表和大小限制拆分）

// ---------------------------------------------------------------------------
// OCR 资源限流：全局并发信号量与临时目录空间预算
// ---------------------------------------------------------------------------

/// 当前活跃的 OCR 请求数（全局共享）。
static OCR_ACTIVE_COUNT: LazyLock<Mutex<usize>> = LazyLock::new(|| Mutex::new(0));
static OCR_ACTIVE_CONDVAR: LazyLock<Condvar> = LazyLock::new(Condvar::new);

/// OCR 并发 permit RAII 守卫。drop 时释放一个并发槽位并通知等待者。
pub(crate) struct OcrPermit;

impl Drop for OcrPermit {
    fn drop(&mut self) {
        if let Ok(mut count) = OCR_ACTIVE_COUNT.lock() {
            *count = count.saturating_sub(1);
        }
        OCR_ACTIVE_CONDVAR.notify_one();
    }
}

/// 尝试获取 OCR 并发 permit，最多等待 `timeout`。
/// 若在超时前获取到槽位则返回 Some(OcrPermit)，否则返回 None。
pub(crate) fn try_acquire_ocr_permit(
    max_concurrency: usize,
    timeout: Duration,
) -> Option<OcrPermit> {
    let start = std::time::Instant::now();
    let mut count = OCR_ACTIVE_COUNT.lock().ok()?;
    loop {
        if *count < max_concurrency {
            *count += 1;
            return Some(OcrPermit);
        }
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return None;
        }
        let remaining = timeout - elapsed;
        let result = OCR_ACTIVE_CONDVAR.wait_timeout(count, remaining).ok()?;
        count = result.0;
        if result.1.timed_out() {
            return None;
        }
    }
}

/// 5. 逐块调用 GLM-OCR，持久化页级/块级结果
/// 6. 合并文本层与 OCR 结果
/// 7. 调用 writeback_ocr_results 执行 split/embed/persist
/// 8. 更新文档 OCR 状态
pub fn run_ocr_worker_once(engine: &SqliteEngine, config: &Config) -> Result<bool> {
    // 获取 OCR 并发 permit，最多等待 5 秒。
    // 若超时未获取到槽位，跳过本次 OCR 处理，让 worker 尝试其他作业类型。
    let _ocr_permit =
        match try_acquire_ocr_permit(config.ocr_max_concurrency, Duration::from_secs(5)) {
            Some(permit) => permit,
            None => return Ok(false),
        };

    // 创建 embedder（用于语义分割，与 kb worker 一致）
    let embedder: Option<Arc<Embedder>> = config.embedding_api_key.as_deref().map(|api_key| {
        Arc::new(Embedder::new(
            api_key,
            config
                .embedding_base_url
                .as_deref()
                .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
            &config.embedding_model,
            config.embedding_dimensions,
        ))
    });

    let conn = engine.connection()?;

    // 认领 OCR 作业
    let claimed = claim_next_ocr_job(conn)?;
    let Some((job_db_id, payload)) = claimed else {
        return Ok(false);
    };

    info!(
        job_db_id,
        document_id = payload.document_id,
        pages = ?payload.pages,
        "OCR worker: 认领作业"
    );

    // 校验 processing_run_id 仍为当前值
    let current_run_id: String = match conn.query_row(
        "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
        rusqlite::params![payload.document_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(run_id) => run_id,
        Err(e) => {
            warn!(
                document_id = payload.document_id,
                error = %e,
                "OCR worker: 查询文档 run_id 失败"
            );
            fail_kb_job(conn, job_db_id, &format!("查询文档失败: {}", e))?;
            return Ok(true);
        }
    };

    if current_run_id != payload.processing_run_id {
        warn!(
            document_id = payload.document_id,
            expected = %payload.processing_run_id,
            actual = %current_run_id,
            "OCR worker: run_id 已过期，跳过"
        );
        // 旧 run 的 OCR job，直接完成不执行
        complete_kb_job(conn, job_db_id)?;
        return Ok(true);
    }

    // 更新文档 OCR 状态为 processing（带 run guard，防止 stale job 覆盖新 run 的状态）
    let kb = KbEngine::new(conn);
    kb.update_document_ocr_with_run_guard(
        payload.document_id,
        crate::kb::ocr::OcrStatus::Processing.as_str(),
        0.0,
        Some(&payload.processing_run_id),
    )?;

    // 外部 OCR 始终允许，脱敏已关闭 — 不再检查库级策略
    let _library = kb.get_library(payload.library_id)?;

    // 全局 OCR 开关检查（作为最终边界，防止已入队任务绕过全局开关）
    if !config.ocr_enabled {
        warn!(
            document_id = payload.document_id,
            "OCR worker: 全局 OCR 已关闭 (GBRAIN_OCR_ENABLED=false)，跳过"
        );
        crate::kb::ocr::update_ocr_pages_status(
            conn,
            payload.document_id,
            &payload.pages,
            "needed",
            "全局 OCR 已关闭 (GBRAIN_OCR_ENABLED=false)",
            &payload.provider,
            &payload.model,
            &payload.processing_run_id,
        )?;
        return complete_ocr_with_native_text_fallback(
            conn,
            job_db_id,
            &payload,
            embedder.clone(),
            "ocr_globally_disabled",
        );
    }

    // OCR API key 检查（缺少 key 时标记为 failed，与 pipeline 行为一致）
    if config.ocr_api_key.is_none() {
        warn!(
            document_id = payload.document_id,
            "OCR worker: 未配置 OCR API key，跳过"
        );
        crate::kb::ocr::update_ocr_pages_status(
            conn,
            payload.document_id,
            &payload.pages,
            "failed",
            "未配置 OCR API key (GBRAIN_OCR_API_KEY)",
            &payload.provider,
            &payload.model,
            &payload.processing_run_id,
        )?;
        return complete_ocr_with_native_text_fallback(
            conn,
            job_db_id,
            &payload,
            embedder.clone(),
            "ocr_api_key_missing",
        );
    }

    // 执行 OCR 处理
    let result = execute_ocr_job(conn, &payload, config, embedder.clone());

    match result {
        Ok(ocr_result) => {
            info!(
                job_db_id,
                document_id = payload.document_id,
                coverage = ocr_result.coverage,
                "OCR worker: 处理完成"
            );
            if !ocr_result.reembed_enqueued {
                if let Err(e) = finalize_artifact_after_kb_success(
                    conn,
                    payload.document_id,
                    &payload.processing_run_id,
                    "ocr_worker",
                ) {
                    warn!(
                        document_id = payload.document_id,
                        error = %e,
                        "OCR worker: 触发 artifact promotion 失败，job 保持可重试状态"
                    );
                    fail_kb_job(conn, job_db_id, &format!("promotion 入队失败: {}", e))?;
                    return Ok(true);
                }
            }
            // OCR 回写已更新文档 OCR 状态，此处根据最终状态处理文档级错误：
            // 终态为 Failed 时保存脱敏错误原因，非失败终态才清空错误。
            {
                let doc_total_pages = {
                    let kb = KbEngine::new(conn);
                    kb.get_document(payload.document_id)
                        .map(|d| d.page_count)
                        .unwrap_or(payload.pages.len() as i32)
                };
                let (final_ocr_status, _) = crate::kb::ocr::compute_ocr_status(
                    conn,
                    payload.document_id,
                    doc_total_pages,
                    Some(&payload.processing_run_id),
                )?;
                let kb = KbEngine::new(conn);
                if final_ocr_status == crate::kb::ocr::OcrStatus::Failed {
                    kb.update_document_status_with_run_guard(
                        payload.document_id,
                        None,
                        None,
                        Some("OCR 全部页面处理失败"),
                        None,
                        None,
                        None,
                        Some(&payload.processing_run_id),
                    )?;
                } else {
                    kb.update_document_status_with_run_guard(
                        payload.document_id,
                        None,
                        None,
                        Some(""),
                        None,
                        None,
                        None,
                        Some(&payload.processing_run_id),
                    )?;
                }
            }
            complete_kb_job(conn, job_db_id)?;
            Ok(true)
        }
        Err(GBrainError::OcrPostWriteback(msg)) => {
            warn!(
                job_db_id,
                document_id = payload.document_id,
                error = %msg,
                "OCR worker: OCR 回写已完成，但后续处理失败"
            );
            // OCR 页级结果和节点已经成功写入；这里只让 job 失败以便告警/重试，
            // 不再把已完成的 OCR 页覆盖成 failed。
            fail_kb_job(conn, job_db_id, &msg)?;
            Ok(true)
        }
        Err(e) => {
            let error_message = e.to_string();
            // 统一脱敏：provider HTTP 响应正文可能包含 API key 或内部地址，
            // 在写入数据库和日志前清洗，防止敏感信息泄漏
            let safe_error_message = crate::kb::ocr::sanitize_error_text_with_secret(
                &error_message,
                config.ocr_api_key.as_deref(),
            );
            warn!(
                job_db_id,
                document_id = payload.document_id,
                error = %safe_error_message,
                "OCR worker: 处理失败"
            );
            // 标记失败页（使用脱敏后的错误信息）。
            // 仅标记实际尝试处理的页：超出 max_pages 上限的尾部页面已在
            // execute_ocr_job 内标记为 skipped，不应在此处覆盖为 failed。
            let max_pages = config.ocr_max_pages_per_document.max(1);
            let attempted_pages: Vec<i32> = if payload.pages.len() > max_pages {
                payload.pages[..max_pages].to_vec()
            } else {
                payload.pages.clone()
            };
            crate::kb::ocr::update_ocr_pages_status(
                conn,
                payload.document_id,
                &attempted_pages,
                "failed",
                &safe_error_message,
                &payload.provider,
                &payload.model,
                &payload.processing_run_id,
            )?;
            // 更新文档 OCR 状态（使用文档总页数，而非本次 payload 页数，避免重试场景 coverage 错误）
            let doc_total_pages = {
                let kb = KbEngine::new(conn);
                kb.get_document(payload.document_id)
                    .map(|d| d.page_count)
                    .unwrap_or(payload.pages.len() as i32)
            };
            let final_ocr_status = crate::kb::ocr::update_document_ocr_status(
                conn,
                payload.document_id,
                doc_total_pages,
                Some(&payload.processing_run_id),
            )?;
            // 全部 OCR 失败时，将脱敏后的错误写入文档级 parsing_error，
            // 确保文档状态接口可以展示失败原因。
            if final_ocr_status == crate::kb::ocr::OcrStatus::Failed {
                let kb = KbEngine::new(conn);
                kb.update_document_status_with_run_guard(
                    payload.document_id,
                    None,
                    None,
                    Some(&safe_error_message),
                    None,
                    None,
                    None,
                    Some(&payload.processing_run_id),
                )?;
            }

            // OCR 任务耗尽重试后，PDF 仍可用原文本层兜底；图片没有原文本层，只更新 shadow 失败摘要。
            // 在还有重试机会时保留 ocr_pending，避免每次失败都重复重建节点。
            if !ocr_job_has_attempts_remaining(conn, job_db_id)? {
                if is_image_ocr_payload(&payload) {
                    // 图片无原文本层兜底，OCR 耗尽重试即文档彻底失败；
                    // 必须更新 document_status/index_status，否则文档卡在 ocr_pending
                    conn.execute(
                        "UPDATE kb_documents SET document_status = 'failed', index_status = 'failed', \
                         parsing_status = 3, parsing_progress = 100, \
                         updated_at = datetime('now') WHERE id = ?1 AND processing_run_id = ?2",
                        rusqlite::params![payload.document_id, payload.processing_run_id],
                    )?;
                    if let Err(update_error) = update_shadow_pages_after_kb_failure(
                        conn,
                        payload.document_id,
                        &payload.processing_run_id,
                        &safe_error_message,
                    ) {
                        warn!(
                            document_id = payload.document_id,
                            error = %update_error,
                            "OCR worker: failed to update artifact shadow page after image OCR failure"
                        );
                    }
                } else {
                    match writeback_native_text_layer(conn, &payload, embedder.clone()) {
                        Ok(fallback_result) => {
                            if !fallback_result.reembed_enqueued {
                                if let Err(finalize_error) = finalize_artifact_after_kb_success(
                                    conn,
                                    payload.document_id,
                                    &payload.processing_run_id,
                                    "ocr_permanent_failure_native_fallback",
                                ) {
                                    fail_kb_job(
                                        conn,
                                        job_db_id,
                                        &format!(
                                            "{}; 原文本层已写回但 artifact promotion 入队失败: {}",
                                            safe_error_message, finalize_error
                                        ),
                                    )?;
                                    return Ok(true);
                                }
                            }
                        }
                        Err(fallback_error) => {
                            fail_kb_job(
                                conn,
                                job_db_id,
                                &format!(
                                    "{}; 原文本层 fallback 失败: {}",
                                    safe_error_message, fallback_error
                                ),
                            )?;
                            return Ok(true);
                        }
                    }
                }
            }

            fail_kb_job(conn, job_db_id, &safe_error_message)?;
            Ok(true)
        }
    }
}

/// OCR 请求最大重试次数
const MAX_OCR_RETRIES: u32 = 3;
/// OCR 请求初始退避时间（秒），每次重试翻倍
const INITIAL_RETRY_BACKOFF_SECS: u64 = 2;
/// GLM-OCR accepts more PDFs reliably after physical rewrite into small ranges.
const GLM_OCR_SAFE_PAGES_PER_REQUEST: usize = 5;
// M-8 修复：MAX_OCR_IMAGE_BYTES 统一定义在 ocr.rs，此处引用
use crate::kb::ocr::MAX_OCR_IMAGE_BYTES;

fn is_glm_ocr_payload(provider: &str, model: &str) -> bool {
    provider.eq_ignore_ascii_case("glm_ocr") || model.eq_ignore_ascii_case("glm-ocr")
}

fn effective_glm_ocr_pages_per_request(configured: usize) -> usize {
    configured.clamp(1, GLM_OCR_SAFE_PAGES_PER_REQUEST)
}

fn ocr_payload_extension(payload: &crate::kb::jobs::KbOcrPayload) -> String {
    std::path::Path::new(&payload.storage_path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_lowercase()
}

fn is_image_ocr_payload(payload: &crate::kb::jobs::KbOcrPayload) -> bool {
    crate::artifact::types::is_ocr_image_file(&ocr_payload_extension(payload))
}

#[derive(Debug, Clone, Copy)]
struct OcrExecutionResult {
    coverage: f64,
    reembed_enqueued: bool,
}

fn ocr_job_has_attempts_remaining(conn: &Connection, job_db_id: i64) -> Result<bool> {
    let (attempts, max_attempts): (i32, i32) = conn
        .query_row(
            "SELECT attempts, max_attempts FROM jobs WHERE id = ?1 AND status = 'running'",
            rusqlite::params![job_db_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| GBrainError::Database(format!("查询 OCR job 重试状态失败: {}", e)))?;
    Ok(attempts < max_attempts)
}

fn complete_ocr_with_native_text_fallback(
    conn: &Connection,
    job_db_id: i64,
    payload: &crate::kb::jobs::KbOcrPayload,
    embedder: Option<Arc<Embedder>>,
    context: &str,
) -> Result<bool> {
    if is_image_ocr_payload(payload) {
        return complete_image_ocr_without_text_fallback(conn, job_db_id, payload, context);
    }

    match writeback_native_text_layer(conn, payload, embedder) {
        Ok(result) => {
            info!(
                job_db_id,
                document_id = payload.document_id,
                "OCR worker: 外部 OCR 不可执行，已写回 PDF 原文本层索引"
            );
            // 检查 OCR 终态：若所有页均标记为 failed，需将脱敏错误写入文档级
            // parsing_error，避免出现 ocr_status=failed 但无失败原因的状态；
            // 非失败终态（needed/partial）时清空旧错误，防止残留上次重试的 provider 失败原因
            {
                let doc_total_pages = {
                    let kb = KbEngine::new(conn);
                    kb.get_document(payload.document_id)
                        .map(|d| d.page_count)
                        .unwrap_or(payload.pages.len() as i32)
                };
                let (final_ocr_status, _) = crate::kb::ocr::compute_ocr_status(
                    conn,
                    payload.document_id,
                    doc_total_pages,
                    Some(&payload.processing_run_id),
                )?;
                let kb = KbEngine::new(conn);
                if final_ocr_status == crate::kb::ocr::OcrStatus::Failed {
                    kb.update_document_status_with_run_guard(
                        payload.document_id,
                        None,
                        None,
                        Some("外部 OCR 不可执行，全部页面处理失败"),
                        None,
                        None,
                        None,
                        Some(&payload.processing_run_id),
                    )?;
                } else {
                    kb.update_document_status_with_run_guard(
                        payload.document_id,
                        None,
                        None,
                        Some(""),
                        None,
                        None,
                        None,
                        Some(&payload.processing_run_id),
                    )?;
                }
            }
            if !result.reembed_enqueued {
                if let Err(e) = finalize_artifact_after_kb_success(
                    conn,
                    payload.document_id,
                    &payload.processing_run_id,
                    context,
                ) {
                    warn!(
                        document_id = payload.document_id,
                        error = %e,
                        "OCR worker: 原文本层 fallback 触发 artifact promotion 失败，job 保持可重试状态"
                    );
                    fail_kb_job(conn, job_db_id, &format!("promotion 入队失败: {}", e))?;
                    return Ok(true);
                }
            }
            complete_kb_job(conn, job_db_id)?;
            Ok(true)
        }
        Err(GBrainError::OcrPostWriteback(msg)) => {
            warn!(
                job_db_id,
                document_id = payload.document_id,
                error = %msg,
                "OCR worker: 原文本层已写回，但后续 re-embed 入队失败"
            );
            fail_kb_job(conn, job_db_id, &msg)?;
            Ok(true)
        }
        Err(e) => {
            warn!(
                job_db_id,
                document_id = payload.document_id,
                error = %e,
                "OCR worker: 写回 PDF 原文本层 fallback 失败"
            );
            fail_kb_job(conn, job_db_id, &e.to_string())?;
            Ok(true)
        }
    }
}

fn complete_image_ocr_without_text_fallback(
    conn: &Connection,
    job_db_id: i64,
    payload: &crate::kb::jobs::KbOcrPayload,
    context: &str,
) -> Result<bool> {
    let doc_total_pages = {
        let kb = KbEngine::new(conn);
        kb.get_document(payload.document_id)
            .map(|d| d.page_count)
            .unwrap_or(payload.pages.len().max(1) as i32)
            .max(1)
    };
    let final_ocr_status = crate::kb::ocr::update_document_ocr_status(
        conn,
        payload.document_id,
        doc_total_pages,
        Some(&payload.processing_run_id),
    )?;
    let message = format!(
        "image OCR is unavailable ({}); no native text layer exists for fallback",
        context
    );
    let kb = KbEngine::new(conn);
    kb.update_document_status_with_run_guard(
        payload.document_id,
        Some(crate::kb::types::STATUS_FAILED),
        Some(100),
        Some(&message),
        None,
        None,
        None,
        Some(&payload.processing_run_id),
    )?;
    // 图片没有原文本层兜底，OCR 不可用即文档彻底失败；必须同时更新 document_status/index_status，
    // 否则文档会卡在 queued/ocr_pending 状态
    conn.execute(
        "UPDATE kb_documents SET document_status = 'failed', index_status = 'failed', \
         updated_at = datetime('now') WHERE id = ?1 AND processing_run_id = ?2",
        rusqlite::params![payload.document_id, payload.processing_run_id],
    )?;
    if let Err(e) = update_shadow_pages_after_kb_failure(
        conn,
        payload.document_id,
        &payload.processing_run_id,
        &message,
    ) {
        warn!(
            job_db_id,
            document_id = payload.document_id,
            error = %e,
            "OCR worker: failed to update artifact shadow page after image OCR fallback miss"
        );
    }
    info!(
        job_db_id,
        document_id = payload.document_id,
        ocr_status = final_ocr_status.as_str(),
        "OCR worker: image OCR cannot use native text fallback"
    );
    complete_kb_job(conn, job_db_id)?;
    Ok(true)
}

fn writeback_native_text_layer(
    conn: &Connection,
    payload: &crate::kb::jobs::KbOcrPayload,
    embedder: Option<Arc<Embedder>>,
) -> Result<OcrExecutionResult> {
    if is_image_ocr_payload(payload) {
        return Err(GBrainError::InvalidInput(
            "image OCR payload has no native text layer fallback".to_string(),
        ));
    }

    let file_data = std::fs::read(&payload.storage_path).map_err(|e| {
        GBrainError::FileError(format!(
            "读取 PDF 原文本层失败 {}: {}",
            payload.storage_path, e
        ))
    })?;
    let parsed = crate::kb::parser::ParserRegistry::new().parse("pdf", &file_data)?;
    let total_pages: i32 = parsed
        .metadata
        .get("total_pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if total_pages == 0 {
        return Err(GBrainError::InvalidInput(
            "PDF 总页数为 0，无法写回原文本层索引".to_string(),
        ));
    }

    crate::kb::ocr::check_ocr_run_guard(conn, payload.document_id, &payload.processing_run_id)?;

    let page_analyses: Vec<serde_json::Value> = parsed
        .metadata
        .get("page_analyses")
        .and_then(|value| serde_json::from_str(value).ok())
        .unwrap_or_default();
    let native_pages: Vec<crate::kb::ocr::OcrWritebackPage> = page_analyses
        .iter()
        .filter_map(|page| {
            let page_number = page.get("page_number")?.as_i64()? as i32;
            if page_number <= 0 {
                return None;
            }
            Some(crate::kb::ocr::OcrWritebackPage {
                page_number,
                text: page
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or("")
                    .to_string(),
            })
        })
        .collect();
    if native_pages.is_empty() {
        return Err(GBrainError::InvalidInput(
            "PDF 解析结果缺少页级文本，无法写回原文本层索引".to_string(),
        ));
    }

    // fallback 可能发生在 partial 文档的重试任务上。保留当前 run 已成功写入的
    // OCR 页内容，仅对无可用 OCR 结果的页使用原文本层。
    let page_analyses: Vec<crate::kb::ocr_detector::PdfPageAnalysis> = native_pages
        .iter()
        .map(|page| crate::kb::ocr_detector::PdfPageAnalysis {
            page_number: page.page_number,
            text: page.text.clone(),
            text_blocks: vec![],
            char_count: page.text.chars().count(),
            image_regions: vec![],
            image_area_ratio: 0.0,
            has_vector_or_unknown_objects: false,
            width: None,
            height: None,
            content_parse_failed: false,
            has_vector_drawing_ops: false,
            has_invisible_text: false,
            font_encoding_suspected: false,
        })
        .collect();
    let (persisted_ocr_results, attempted_ocr_pages) = load_current_ocr_merge_state(conn, payload)?;
    let fallback_pages: Vec<crate::kb::ocr::OcrWritebackPage> =
        crate::kb::ocr_merge::merge_text_and_ocr(
            &page_analyses,
            &persisted_ocr_results,
            &attempted_ocr_pages,
        )
        .into_iter()
        .map(|page| crate::kb::ocr::OcrWritebackPage {
            page_number: page.page_number,
            text: page.text,
        })
        .collect();

    writeback_pages_and_enqueue_reembed(conn, payload, &fallback_pages, total_pages, embedder)
}

fn writeback_pages_and_enqueue_reembed(
    conn: &Connection,
    payload: &crate::kb::jobs::KbOcrPayload,
    pages: &[crate::kb::ocr::OcrWritebackPage],
    total_pages: i32,
    embedder: Option<Arc<Embedder>>,
) -> Result<OcrExecutionResult> {
    let doc_title: String = conn
        .query_row(
            "SELECT title FROM kb_documents WHERE id = ?1",
            rusqlite::params![payload.document_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default();
    let (chunk_size, chunk_overlap, title_weight): (usize, usize, f32) = conn
        .query_row(
            "SELECT chunk_size, chunk_overlap, COALESCE(title_weight, 0.2) FROM kb_libraries WHERE id = ?1",
            rusqlite::params![payload.library_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)? as usize,
                    row.get::<_, i64>(1)? as usize,
                    row.get::<_, f32>(2)?,
                ))
            },
        )
        .unwrap_or((1024, 200, 0.2));

    let writeback_result = crate::kb::ocr::writeback_ocr_results(
        conn,
        payload.document_id,
        payload.library_id,
        pages,
        chunk_size,
        chunk_overlap,
        &doc_title,
        total_pages,
        Some(&payload.processing_run_id),
        embedder,
        title_weight,
    )?;

    let mut reembed_enqueued = false;
    if writeback_result.nodes_created > 0 {
        let queue = crate::jobs::JobQueue::new(conn);
        queue
            .enqueue(crate::jobs::JobInput {
                job_type: "kb_reembed".to_string(),
                payload: serde_json::json!({
                    "document_id": payload.document_id,
                    "processing_run_id": payload.processing_run_id,
                    "source": "ocr_writeback",
                }),
                priority: Some(0),
                max_attempts: Some(3),
            })
            .map_err(|e| {
                GBrainError::OcrPostWriteback(format!(
                    "OCR 回写已完成但入队 kb_reembed job 失败，文档仍处于 embedding pending: {}",
                    e
                ))
            })?;
        reembed_enqueued = true;
    }

    Ok(OcrExecutionResult {
        coverage: writeback_result.ocr_text_coverage,
        reembed_enqueued,
    })
}

fn load_current_ocr_merge_state(
    conn: &Connection,
    payload: &crate::kb::jobs::KbOcrPayload,
) -> Result<(Vec<crate::kb::ocr_provider::OcrPageResult>, Vec<i32>)> {
    let mut stmt = conn.prepare(
        "SELECT page_number, status, text, markdown, layout_json, \
         layout_visualization_url, raw_response_json, request_id, confidence, provider, model, \
         ocr_page_width, ocr_page_height \
         FROM kb_document_ocr_pages \
         WHERE document_id = ?1 AND processing_run_id = ?2 ORDER BY page_number",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![payload.document_id, &payload.processing_run_id],
        |row| {
            Ok((
                row.get::<_, i32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<f64>>(8)?,
                row.get::<_, String>(9)?,
                row.get::<_, String>(10)?,
                row.get::<_, Option<u32>>(11)?,
                row.get::<_, Option<u32>>(12)?,
            ))
        },
    )?;

    let mut persisted_results = Vec::new();
    let mut attempted_pages = Vec::new();
    for row in rows {
        let (
            page_number,
            status,
            text,
            markdown,
            layout_json,
            visualization_url,
            raw_response_json,
            request_id,
            confidence,
            provider,
            model,
            ocr_page_width,
            ocr_page_height,
        ) = row?;
        attempted_pages.push(page_number);
        let has_stored_ocr_text = !text.trim().is_empty() || !markdown.trim().is_empty();
        if status != "done" && status != "empty_ocr" && !has_stored_ocr_text {
            continue;
        }

        persisted_results.push(crate::kb::ocr_provider::OcrPageResult {
            page_number,
            text,
            markdown,
            blocks: serde_json::from_str(&layout_json).unwrap_or_default(),
            layout_visualization_url: if visualization_url.is_empty() {
                None
            } else {
                Some(visualization_url)
            },
            raw_response_json: serde_json::from_str(&raw_response_json)
                .unwrap_or_else(|_| serde_json::json!({})),
            request_id: if request_id.is_empty() {
                None
            } else {
                Some(request_id)
            },
            confidence,
            provider,
            model,
            ocr_page_width,
            ocr_page_height,
        });
    }

    Ok((persisted_results, attempted_pages))
}

/// 判断 OCR 错误是否可重试（429/503/timeout/网络错误）
fn is_retryable_ocr_error(error: &str) -> bool {
    let lower = error.to_lowercase();
    lower.contains("429")
        || lower.contains("rate limit")
        || lower.contains("503")
        || lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection")
        || lower.contains("hyper")
}

fn execute_image_ocr_job(
    conn: &Connection,
    payload: &crate::kb::jobs::KbOcrPayload,
    config: &Config,
    embedder: Option<std::sync::Arc<crate::embedding::Embedder>>,
) -> Result<OcrExecutionResult> {
    use crate::kb::ocr_glm::{build_ocr_options_from_config, GlmOcrProvider};
    use crate::kb::ocr_provider::{OcrFilePayload, OcrInput};
    use base64::Engine;

    let ext = ocr_payload_extension(payload);
    let file_data = std::fs::read(&payload.storage_path).map_err(|e| {
        GBrainError::FileError(format!(
            "read image OCR file failed {}: {}",
            payload.storage_path, e
        ))
    })?;
    // 在任何写入前先校验 run guard，防止旧 OCR job 覆盖新一轮处理的文档状态
    crate::kb::ocr::check_ocr_run_guard(conn, payload.document_id, &payload.processing_run_id)?;

    // 基于文件内容校验 MIME，防止伪装扩展名的文件或已被替换的文件进入外部 OCR
    let mime_type = match crate::kb::security::detect_and_validate_mime(&file_data, &ext) {
        Ok(mime) => mime,
        Err(e) => {
            let reason = format!("OCR worker MIME 校验失败: {}", e);
            let _ = crate::kb::ocr::update_ocr_page_status(
                conn,
                payload.document_id,
                1,
                "failed",
                &reason,
                &payload.provider,
                &payload.model,
                &payload.processing_run_id,
            );
            // L1: 文档状态标记为失败是关键清理操作，失败时记录警告
            if let Err(e2) = conn.execute(
                "UPDATE kb_documents SET document_status = 'failed', parsing_error = ?1, \
                 parsing_status = 3, parsing_progress = 100, updated_at = datetime('now') \
                 WHERE id = ?2 AND processing_run_id = ?3",
                rusqlite::params![reason, payload.document_id, payload.processing_run_id],
            ) {
                tracing::warn!(doc_id = payload.document_id, error = %e2, "标记文档为 failed 失败");
            }
            return Err(e);
        }
    };
    if file_data.len() > MAX_OCR_IMAGE_BYTES {
        let reason = format!(
            "GLM-OCR image input exceeds 10MB limit ({} bytes)",
            file_data.len()
        );
        crate::kb::ocr::update_ocr_page_status(
            conn,
            payload.document_id,
            1,
            "failed",
            &reason,
            &payload.provider,
            &payload.model,
            &payload.processing_run_id,
        )?;
        // M-10 修复：大小超限时同步更新文档状态为 failed，与 MIME 校验失败路径保持一致，
        // 防止文档停留在 PROCESSING 状态
        if let Err(e2) = conn.execute(
            "UPDATE kb_documents SET document_status = 'failed', parsing_error = ?1, \
             parsing_status = 3, parsing_progress = 100, updated_at = datetime('now') \
             WHERE id = ?2 AND processing_run_id = ?3",
            rusqlite::params![reason, payload.document_id, payload.processing_run_id],
        ) {
            tracing::warn!(doc_id = payload.document_id, error = %e2, "标记文档为 failed 失败");
        }
        return Err(GBrainError::InvalidInput(reason));
    }

    crate::kb::ocr::check_ocr_run_guard(conn, payload.document_id, &payload.processing_run_id)?;

    let mut options = build_ocr_options_from_config(config);
    options.model = payload.model.clone();
    options.return_crop_images = payload.return_crop_images;
    options.need_layout_visualization = payload.need_layout_visualization;

    let api_key = config.ocr_api_key.as_deref().unwrap_or("");
    let provider = GlmOcrProvider::new(api_key);
    let input = OcrInput::Image {
        file: OcrFilePayload::Base64(base64::engine::general_purpose::STANDARD.encode(&file_data)),
        mime_type: mime_type.to_string(),
        document_id: payload.document_id,
        run_id: payload.processing_run_id.clone(),
    };

    let mut retry_count = 0u32;
    let results = loop {
        let call_started = std::time::Instant::now();
        let recognition = provider.recognize_off_async_context(&input, &options);
        let latency_ms = call_started.elapsed().as_millis().min(i32::MAX as u128) as i32;

        match recognition {
            Ok(results) => {
                let mut results =
                    crate::kb::ocr::sanitize_ocr_page_results(&results, Some(api_key));
                results.retain(|result| result.page_number == 1);
                crate::kb::ocr::log_ocr_external_model_call(
                    conn,
                    payload.library_id,
                    payload.document_id,
                    &payload.provider,
                    &payload.model,
                    latency_ms,
                    true,
                    "",
                    &results,
                    Some(api_key),
                );
                if results.is_empty() {
                    let reason = "image OCR request returned no page 1 result";
                    crate::kb::ocr::update_ocr_page_status(
                        conn,
                        payload.document_id,
                        1,
                        "failed",
                        reason,
                        &payload.provider,
                        &payload.model,
                        &payload.processing_run_id,
                    )?;
                    return Err(GBrainError::InvalidInput(reason.to_string()));
                }
                break results;
            }
            Err(e) => {
                let error_str = e.to_string();
                let safe_error =
                    crate::kb::ocr::sanitize_error_text_with_secret(&error_str, Some(api_key));
                crate::kb::ocr::log_ocr_external_model_call(
                    conn,
                    payload.library_id,
                    payload.document_id,
                    &payload.provider,
                    &payload.model,
                    latency_ms,
                    false,
                    &error_str,
                    &[],
                    Some(api_key),
                );
                if is_retryable_ocr_error(&error_str) && retry_count < MAX_OCR_RETRIES {
                    retry_count += 1;
                    let backoff_secs = INITIAL_RETRY_BACKOFF_SECS * 2u64.pow(retry_count - 1);
                    tracing::warn!(
                        document_id = payload.document_id,
                        retry = retry_count,
                        max_retries = MAX_OCR_RETRIES,
                        backoff_secs,
                        error = %safe_error,
                        "image OCR request hit retryable error; backing off"
                    );
                    std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                    continue;
                }

                crate::kb::ocr::update_ocr_page_status(
                    conn,
                    payload.document_id,
                    1,
                    "failed",
                    &safe_error,
                    &payload.provider,
                    &payload.model,
                    &payload.processing_run_id,
                )?;
                return Err(GBrainError::Http(format!(
                    "image OCR execution failed: {}",
                    safe_error
                )));
            }
        }
    };

    crate::kb::ocr::persist_ocr_page_results(
        conn,
        payload.document_id,
        &payload.processing_run_id,
        &results,
        Some(api_key),
    )?;
    crate::kb::ocr::persist_ocr_blocks(
        conn,
        payload.document_id,
        &payload.processing_run_id,
        &results,
        Some(api_key),
    )?;

    let (persisted_ocr_results, _) = load_current_ocr_merge_state(conn, payload)?;
    let ocr_pages: Vec<crate::kb::ocr::OcrWritebackPage> = persisted_ocr_results
        .into_iter()
        .filter(|result| result.page_number == 1)
        .map(|result| {
            let text = if result.text.trim().is_empty() {
                result.markdown
            } else {
                result.text
            };
            crate::kb::ocr::OcrWritebackPage {
                page_number: 1,
                text,
            }
        })
        .collect();
    if ocr_pages.is_empty() {
        return Err(GBrainError::InvalidInput(
            "image OCR result was not persisted for page 1".to_string(),
        ));
    }

    // 图片没有 PDF 原文本兜底：若 OCR 返回全空，直接返回错误让外层 Err 处理器统一接管，
    // 避免走 Ok 分支触发 finalize_artifact_after_kb_success（错误地 promotion）
    // 以及 compute_ocr_status 返回 Partial 后被清空 parsing_error
    if ocr_pages.iter().all(|p| p.text.trim().is_empty()) {
        return Err(GBrainError::InvalidInput(
            "图片 OCR 无文本：API 返回 page 1 但 text/markdown 均为空，无可提取内容".to_string(),
        ));
    }

    writeback_pages_and_enqueue_reembed(conn, payload, &ocr_pages, 1, embedder)
}

/// 执行 OCR 作业核心逻辑：读取 PDF → 规划 → 调用 GLM-OCR → 合并 → 回写
fn execute_ocr_job(
    conn: &Connection,
    payload: &crate::kb::jobs::KbOcrPayload,
    config: &Config,
    embedder: Option<std::sync::Arc<crate::embedding::Embedder>>,
) -> Result<OcrExecutionResult> {
    if is_image_ocr_payload(payload) {
        return execute_image_ocr_job(conn, payload, config, embedder);
    }

    use crate::kb::ocr_glm::{build_ocr_options_from_config, pdf_to_base64, GlmOcrProvider};
    use crate::kb::ocr_merge::merge_text_and_ocr;
    use crate::kb::ocr_planner::plan_ocr_requests;
    use crate::kb::ocr_provider::{OcrFilePayload, OcrInput, OcrSubmitMode};

    // 读取 PDF 文件
    let file_data = std::fs::read(&payload.storage_path).map_err(|e| {
        GBrainError::FileError(format!("读取 PDF 文件失败 {}: {}", payload.storage_path, e))
    })?;

    // 解析 PDF 获取文本层页级分析
    let registry = crate::kb::parser::ParserRegistry::new();
    let parsed = registry.parse("pdf", &file_data)?;

    let total_pages: i32 = parsed
        .metadata
        .get("total_pages")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if total_pages == 0 {
        // 无法确认 PDF 页数，可能是解析异常，返回错误以触发失败标记
        // 而非静默 Ok(0.0) 让文档停在 processing/queued
        return Err(GBrainError::InvalidInput(
            "PDF 总页数为 0，无法执行 OCR".to_string(),
        ));
    }

    // 防御性检查：限制 OCR 页数上限
    // 处理前先校验 run guard，防止 stale job 写入页级状态
    if let Err(e) =
        crate::kb::ocr::check_ocr_run_guard(conn, payload.document_id, &payload.processing_run_id)
    {
        tracing::warn!(
            document_id = payload.document_id,
            error = %e,
            "OCR worker: 初始 run guard 失败，跳过处理（stale job）"
        );
        return Err(e);
    }

    let ocr_pages = {
        let max_pages = config.ocr_max_pages_per_document.max(1);
        if payload.pages.len() > max_pages {
            tracing::warn!(
                document_id = payload.document_id,
                page_count = payload.pages.len(),
                max_pages,
                "OCR worker: 页数超过单文档上限，截断为前 {} 页",
                max_pages
            );
            // 超出上限的页标记为 skipped，确保有可见状态
            let skipped: &[i32] = &payload.pages[max_pages..];
            crate::kb::ocr::update_ocr_pages_status(
                conn,
                payload.document_id,
                skipped,
                "skipped",
                &format!("超出单文档 OCR 页数上限 ({})", max_pages),
                &payload.provider,
                &payload.model,
                &payload.processing_run_id,
            )?;
            tracing::info!(
                document_id = payload.document_id,
                skipped_count = skipped.len(),
                "已将超出上限的 {} 页标记为 skipped",
                skipped.len()
            );
            payload.pages[..max_pages].to_vec()
        } else {
            payload.pages.clone()
        }
    };

    // 从 metadata 构建 PdfPageAnalysis
    let page_analyses_raw = parsed
        .metadata
        .get("page_analyses")
        .and_then(|v| serde_json::from_str::<Vec<serde_json::Value>>(v).ok())
        .unwrap_or_default();

    let page_analyses: Vec<crate::kb::ocr_detector::PdfPageAnalysis> = page_analyses_raw
        .iter()
        .map(|pa| {
            let page_number = pa.get("page_number").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
            let text = pa
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let char_count = pa.get("char_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let image_area_ratio = pa
                .get("image_area_ratio")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let image_count = pa.get("image_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let has_vector = pa
                .get("has_vector_or_unknown_objects")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            let image_regions = if image_count > 0 {
                (0..image_count)
                    .map(|_| crate::kb::ocr_detector::PdfImageRegion {
                        bbox: None,
                        area_ratio: image_area_ratio / image_count as f64,
                    })
                    .collect()
            } else if image_area_ratio > 0.0 {
                vec![crate::kb::ocr_detector::PdfImageRegion {
                    bbox: None,
                    area_ratio: image_area_ratio,
                }]
            } else {
                vec![]
            };

            crate::kb::ocr_detector::PdfPageAnalysis {
                page_number,
                text,
                text_blocks: vec![],
                char_count,
                image_regions,
                image_area_ratio,
                has_vector_or_unknown_objects: has_vector,
                width: pa.get("width").and_then(|v| v.as_u64()).map(|v| v as u32),
                height: pa.get("height").and_then(|v| v.as_u64()).map(|v| v as u32),
                content_parse_failed: pa
                    .get("content_parse_failed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                has_vector_drawing_ops: pa
                    .get("has_vector_drawing_ops")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                has_invisible_text: pa
                    .get("has_invisible_text")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                font_encoding_suspected: pa
                    .get("font_encoding_suspected")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            }
        })
        .collect();

    // 从 config 构建基础 options，再用 job payload 中携带的配置覆盖
    // 确保 job 自包含，环境变量变更不会影响已入队 job 的行为
    let mut options = build_ocr_options_from_config(config);
    options.model = payload.model.clone();
    options.return_crop_images = payload.return_crop_images;
    options.need_layout_visualization = payload.need_layout_visualization;
    let mut submit_mode = OcrSubmitMode::from_str(&payload.submit_mode);
    if is_glm_ocr_payload(&payload.provider, &payload.model) {
        let configured_pages = options.max_pages_per_request;
        let configured_submit_mode = submit_mode.clone();
        submit_mode = OcrSubmitMode::PdfRange;
        options.max_pages_per_request =
            effective_glm_ocr_pages_per_request(options.max_pages_per_request);

        if configured_submit_mode != submit_mode
            || configured_pages != options.max_pages_per_request
        {
            tracing::info!(
                document_id = payload.document_id,
                configured_submit_mode = ?configured_submit_mode,
                effective_submit_mode = ?submit_mode,
                configured_pages_per_request = configured_pages,
                effective_pages_per_request = options.max_pages_per_request,
                "GLM-OCR PDF requests use small physical ranges for compatibility"
            );
        }
    }

    // 创建临时目录：包含 run_id 避免同一文档的并发 job 相互覆盖/删除
    // 使用 RAII 守卫确保任何退出路径都自动清理
    let mut temp_guard = crate::kb::temp_guard::TempOcrDir::create(
        &format!(
            "gbrain_ocr_{}_{}",
            payload.document_id, payload.processing_run_id
        ),
        file_data.len() as u64,
        config.ocr_temp_dir_max_bytes,
    )?;

    // 规划 OCR 请求（内部按实际输出文件字节数申请临时目录预算）
    let plan = plan_ocr_requests(
        payload.document_id,
        &payload.processing_run_id,
        &file_data,
        total_pages,
        &ocr_pages,
        options.max_pages_per_request,
        options.max_pdf_bytes_per_request,
        &submit_mode,
        config.ocr_temp_dir_max_bytes,
        &mut temp_guard,
    )?;

    // 执行每个请求块
    let api_key = config.ocr_api_key.as_deref().unwrap_or("");
    let provider = GlmOcrProvider::new(api_key);
    let mut all_ocr_results: Vec<crate::kb::ocr_provider::OcrPageResult> = Vec::new();
    // 跟踪失败的页段，用于在所有页段处理完后决定是否返回错误触发队列重试
    let mut any_chunk_error: Option<String> = None;

    for chunk in &plan.chunks {
        // 每个页段处理前校验 run guard，防止 stale job 写入新 run 的页/块数据
        if let Err(e) = crate::kb::ocr::check_ocr_run_guard(
            conn,
            payload.document_id,
            &payload.processing_run_id,
        ) {
            tracing::warn!(
                document_id = payload.document_id,
                error = %e,
                "OCR worker: run guard 失败，中止剩余页段处理（stale job）"
            );
            break;
        }

        // 拆分失败的页段：跳过 OCR 请求，直接标记为 failed
        if chunk.split_failed {
            warn!(
                document_id = payload.document_id,
                start = chunk.source_start_page,
                end = chunk.source_end_page,
                "OCR worker: 页段拆分失败，跳过 OCR 并标记为 failed"
            );
            // 持久化失败时向上返回错误，避免静默产生不可信状态
            for page_num in chunk.source_start_page..=chunk.source_end_page {
                crate::kb::ocr::update_ocr_page_status(
                    conn,
                    payload.document_id,
                    page_num,
                    "failed",
                    "PDF 页段拆分失败，无法提交 OCR",
                    &payload.provider,
                    &payload.model,
                    &payload.processing_run_id,
                )?;
            }
            // 计入失败聚合：确保全部页段拆分失败时能触发错误返回，
            // 避免空 all_ocr_results + 空 any_chunk_error 走入成功收尾并清空文档错误。
            if any_chunk_error.is_none() {
                any_chunk_error = Some("PDF 页段拆分失败，无法提交 OCR".to_string());
            }
            continue;
        }

        let file_payload = if let Some(ref split_path) = chunk.split_pdf_path {
            let split_data = std::fs::read(split_path)
                .map_err(|e| GBrainError::FileError(format!("读取拆分 PDF 失败: {}", e)))?;
            OcrFilePayload::Base64(pdf_to_base64(&split_data))
        } else {
            OcrFilePayload::Base64(pdf_to_base64(&file_data))
        };

        let input = OcrInput::PdfRange {
            file: file_payload,
            request_start_page_id: chunk.request_start_page_id,
            request_end_page_id: chunk.request_end_page_id,
            source_start_page: chunk.source_start_page,
            source_end_page: chunk.source_end_page,
            document_id: payload.document_id,
            run_id: payload.processing_run_id.clone(),
        };

        // 指数退避重试：对可重试错误（429/503/timeout/网络）进行最多 MAX_OCR_RETRIES 次重试
        let mut retry_count = 0u32;
        loop {
            let call_started = std::time::Instant::now();
            let recognition = provider.recognize_off_async_context(&input, &options);
            let latency_ms = call_started.elapsed().as_millis().min(i32::MAX as u128) as i32;

            match recognition {
                Ok(results) => {
                    let results =
                        crate::kb::ocr::sanitize_ocr_page_results(&results, Some(api_key));
                    crate::kb::ocr::log_ocr_external_model_call(
                        conn,
                        payload.library_id,
                        payload.document_id,
                        &payload.provider,
                        &payload.model,
                        latency_ms,
                        true,
                        "",
                        &results,
                        Some(api_key),
                    );
                    // 持久化 OCR 结果失败时向上返回错误，避免页面结果丢失但文档显示完成
                    if !results.is_empty() {
                        crate::kb::ocr::persist_ocr_page_results(
                            conn,
                            payload.document_id,
                            &payload.processing_run_id,
                            &results,
                            Some(api_key),
                        )?;
                        crate::kb::ocr::persist_ocr_blocks(
                            conn,
                            payload.document_id,
                            &payload.processing_run_id,
                            &results,
                            Some(api_key),
                        )?;
                    }
                    // 标记未返回的页为 failed（处理多页请求部分失败的情况）
                    let returned_pages: std::collections::HashSet<i32> =
                        results.iter().map(|r| r.page_number).collect();
                    for page_num in chunk.source_start_page..=chunk.source_end_page {
                        if !returned_pages.contains(&page_num) {
                            crate::kb::ocr::update_ocr_page_status(
                                conn,
                                payload.document_id,
                                page_num,
                                "failed",
                                "OCR 请求未返回该页结果（部分失败）",
                                &payload.provider,
                                &payload.model,
                                &payload.processing_run_id,
                            )?;
                        }
                    }
                    all_ocr_results.extend(results);
                    break;
                }
                Err(e) => {
                    let error_str = e.to_string();
                    let safe_error =
                        crate::kb::ocr::sanitize_error_text_with_secret(&error_str, Some(api_key));
                    crate::kb::ocr::log_ocr_external_model_call(
                        conn,
                        payload.library_id,
                        payload.document_id,
                        &payload.provider,
                        &payload.model,
                        latency_ms,
                        false,
                        &error_str,
                        &[],
                        Some(api_key),
                    );
                    // 判断是否可重试（429/503/timeout/网络错误）
                    if is_retryable_ocr_error(&error_str) && retry_count < MAX_OCR_RETRIES {
                        retry_count += 1;
                        let backoff_secs = INITIAL_RETRY_BACKOFF_SECS * 2u64.pow(retry_count - 1);
                        tracing::warn!(
                            document_id = payload.document_id,
                            start = chunk.source_start_page,
                            end = chunk.source_end_page,
                            retry = retry_count,
                            max_retries = MAX_OCR_RETRIES,
                            backoff_secs,
                            error = %safe_error,
                            "OCR 请求遇到可重试错误，指数退避重试"
                        );
                        std::thread::sleep(std::time::Duration::from_secs(backoff_secs));
                        continue;
                    }
                    // 不可重试或超过重试次数，标记该段所有页为 failed
                    warn!(
                        document_id = payload.document_id,
                        start = chunk.source_start_page,
                        end = chunk.source_end_page,
                        error = %safe_error,
                        "OCR 请求失败"
                    );
                    for page_num in chunk.source_start_page..=chunk.source_end_page {
                        crate::kb::ocr::update_ocr_page_status(
                            conn,
                            payload.document_id,
                            page_num,
                            "failed",
                            &safe_error,
                            &payload.provider,
                            &payload.model,
                            &payload.processing_run_id,
                        )?;
                    }
                    // 记录首个错误，用于最终决定是否触发队列重试
                    if any_chunk_error.is_none() {
                        any_chunk_error = Some(safe_error);
                    }
                    break;
                }
            }
        }
    }

    // 如果全部页段失败（无任何成功结果），返回错误以触发队列重试
    // 部分成功时继续执行合并与回写，确保成功页进入索引
    if all_ocr_results.is_empty() {
        if let Some(ref err_msg) = any_chunk_error {
            tracing::warn!(
                document_id = payload.document_id,
                "OCR 全部页段失败，返回错误以触发队列重试"
            );
            // temp_guard 在此处 drop，自动清理临时目录
            return Err(GBrainError::Http(format!("OCR 执行全部失败: {}", err_msg)));
        }
    } else if any_chunk_error.is_some() {
        // 部分成功部分失败：成功页已有结果，失败页已在上面标记为 failed
        // 继续执行合并与回写，让成功页进入索引
        tracing::warn!(
            document_id = payload.document_id,
            success_count = all_ocr_results.len(),
            "OCR 部分页段失败，成功页结果继续回写"
        );
    }

    // 合并当前 run 的全部已持久化 OCR 结果，而非仅本次请求返回页。
    // 对 partial 文档重试失败页时，这可保留此前成功页的 OCR 正文。
    let (persisted_ocr_results, attempted_ocr_pages) = load_current_ocr_merge_state(conn, payload)?;
    let merged = merge_text_and_ocr(&page_analyses, &persisted_ocr_results, &attempted_ocr_pages);

    // temp_guard 在此处 drop，自动清理临时目录
    drop(temp_guard);

    // 回写前重新校验 run_id，防止过期 OCR 覆盖新上传产生的节点
    let current_run_id_before_writeback: String = match conn.query_row(
        "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
        rusqlite::params![payload.document_id],
        |row| row.get::<_, String>(0),
    ) {
        Ok(run_id) => run_id,
        Err(e) => {
            return Err(GBrainError::Database(format!(
                "OCR writeback 前查询 run_id 失败: {}",
                e
            )));
        }
    };
    if current_run_id_before_writeback != payload.processing_run_id {
        warn!(
            document_id = payload.document_id,
            expected = %payload.processing_run_id,
            actual = %current_run_id_before_writeback,
            "OCR worker: writeback 前 run_id 已过期，跳过回写"
        );
        return Ok(OcrExecutionResult {
            coverage: 0.0,
            reembed_enqueued: false,
        });
    }

    // 将合并结果（文本层 + OCR）回写到 KB 索引
    let ocr_pages: Vec<crate::kb::ocr::OcrWritebackPage> = merged
        .iter()
        .map(|r| crate::kb::ocr::OcrWritebackPage {
            page_number: r.page_number,
            text: r.text.clone(),
        })
        .collect();

    writeback_pages_and_enqueue_reembed(conn, payload, &ocr_pages, total_pages, embedder)
}

/// 对单个节点执行 re-embed，返回嵌入向量维度数
///
/// `target_index_id` 为 0 时自动解析为该节点所属 library 的 active index。
fn reembed_single_node(
    conn: &Connection,
    embedder: &Option<Arc<Embedder>>,
    rt: &tokio::runtime::Runtime,
    node_id: i64,
    target_index_id: Option<i64>,
    model: &str,
) -> Result<usize> {
    // 解析 target_index_id：0 → active index
    let resolved_index_id = resolve_target_index(conn, node_id, target_index_id.unwrap_or(0))?;

    let (content, embedding_text): (String, String) = conn
        .query_row(
            "SELECT content, embedding_text FROM kb_document_nodes WHERE id=?1",
            rusqlite::params![node_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .map_err(|e| GBrainError::Database(format!("节点 {} 不存在: {}", node_id, e)))?;

    let text_to_embed = if embedding_text.is_empty() {
        &content
    } else {
        &embedding_text
    };
    let embedder = embedder
        .as_ref()
        .ok_or_else(|| GBrainError::InvalidInput("未配置 embedding API key".into()))?;
    let vectors = rt.block_on(embedder.embed_batch(&[text_to_embed]))?;
    let vec = vectors
        .into_iter()
        .next()
        .ok_or_else(|| GBrainError::InvalidInput("embedding 返回空结果".into()))?;
    let dims = vec.len();

    crate::kb::embedding_index::upsert_node_embedding_for_index(
        conn,
        node_id,
        resolved_index_id,
        &vec,
        dims as i32,
        model,
    )?;
    Ok(dims)
}

/// 对文档中所有缺失 embedding 的节点执行批量 re-embed
///
/// `target_index_id` 为 0 时自动解析为该文档所属 library 的 active index。
/// `target_dims` 为显式目标索引的维度（target>0 时查 kb_embedding_indexes 得到），
/// 用于在写入前校验向量维度一致性。
fn reembed_document_nodes(
    conn: &Connection,
    embedder: &Option<Arc<Embedder>>,
    rt: &tokio::runtime::Runtime,
    document_id: i64,
    target_index_id: i64,
    model: &str,
    target_dims: Option<i32>,
) -> Result<usize> {
    // 解析 target_index_id：0 → active index（通过文档的当前版本节点查找所属 library）
    let resolved_index_id = if target_index_id > 0 {
        target_index_id
    } else {
        conn.query_row(
            "SELECT ei.id FROM kb_embedding_indexes ei \
             JOIN kb_document_nodes n ON n.library_id = ei.library_id \
             JOIN kb_documents d ON d.id = n.document_id \
             WHERE n.document_id = ?1 AND ei.is_active = 1 \
             AND d.current_version_id IS NOT NULL \
             AND n.version_id = d.current_version_id \
             AND n.retired_at IS NULL \
             LIMIT 1",
            rusqlite::params![document_id],
            |row| row.get::<_, i64>(0),
        )
        .map_err(|_| {
            GBrainError::InvalidInput(format!(
                "文档 {} 所属 library 没有 active embedding index",
                document_id
            ))
        })?
    };

    // 查找文档中无目标 index embedding 的当前版本节点
    // 只处理 current_version_id 且未退役的节点，避免浪费 embedding 调用在旧版本上
    let node_ids: Vec<(i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT n.id, n.content, n.embedding_text FROM kb_document_nodes n \
             JOIN kb_documents d ON d.id = n.document_id \
             WHERE n.document_id = ?1 \
             AND d.current_version_id IS NOT NULL \
             AND n.version_id = d.current_version_id \
             AND n.retired_at IS NULL \
             AND n.id NOT IN (SELECT node_id FROM kb_node_embeddings \
                              WHERE embedding_index_id = ?2)",
        )?;
        let rows = stmt.query_map(rusqlite::params![document_id, resolved_index_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.filter_map(|r: rusqlite::Result<_>| r.ok()).collect()
    };

    if node_ids.is_empty() {
        return Ok(0);
    }

    let embedder = embedder
        .as_ref()
        .ok_or_else(|| GBrainError::InvalidInput("未配置 embedding API key".into()))?;

    // 批量嵌入（每批最多 20 个节点），始终写入 embedding_index_id
    for chunk in node_ids.chunks(20) {
        let texts: Vec<&str> = chunk
            .iter()
            .map(|(_, c, et)| {
                if et.is_empty() {
                    c.as_str()
                } else {
                    et.as_str()
                }
            })
            .collect();
        let vectors = rt.block_on(embedder.embed_batch(&texts))?;
        for ((node_id, _, _), vec) in chunk.iter().zip(vectors.iter()) {
            let dims = vec.len() as i32;
            // P1 修复: 写入前校验向量维度与目标 index 一致。
            // 避免用 active index 的模型生成的向量写到指定 target index 时维度不匹配。
            if let Some(expected_dims) = target_dims {
                if dims != expected_dims {
                    return Err(GBrainError::InvalidInput(format!(
                        "生成的 embedding 向量维度 ({}) 与目标 index (id={}, dims={}) 不一致，\
                         请检查 embedding 配置: model={}",
                        dims, resolved_index_id, expected_dims, model
                    )));
                }
            }
            crate::kb::embedding_index::upsert_node_embedding_for_index(
                conn,
                *node_id,
                resolved_index_id,
                vec,
                dims,
                model,
            )?;
        }
    }
    Ok(node_ids.len())
}

/// 解析 target_index_id：0 → 查询节点所属 library 的 active index。
/// 找不到 active index 时返回明确错误，不再 fallback 到无效的 0。
fn resolve_target_index(conn: &Connection, node_id: i64, target_index_id: i64) -> Result<i64> {
    if target_index_id > 0 {
        return Ok(target_index_id);
    }
    conn.query_row(
        "SELECT ei.id FROM kb_embedding_indexes ei \
         JOIN kb_document_nodes n ON n.library_id = ei.library_id \
         WHERE n.id = ?1 AND ei.is_active = 1 LIMIT 1",
        rusqlite::params![node_id],
        |row| row.get(0),
    )
    .map_err(|_| {
        GBrainError::InvalidInput(format!(
            "节点 {} 所属 library 没有 active embedding index",
            node_id
        ))
    })
}

/// OCR re-embed 后重建 RAPTOR 树。
///
/// 加载文档叶节点及其嵌入向量，构建 RAPTOR 父节点并持久化到 DB。
/// 仅在库启用 RAPTOR 且允许外部摘要时执行。
/// P0 修复: 按版本隔离进行 RAPTOR 重建。
///
/// 传入 `version_id` 后，叶节点查询、父节点删除/重置、新父节点插入全部限定
/// 同一 version_id，避免：
/// - 叶节点混入退休版本
/// - 删除/重置误伤其他版本的父节点
/// - 新父节点 version_id = NULL 被严格检索过滤掉
fn rebuild_raptor_after_reembed(
    conn: &Connection,
    rt: &tokio::runtime::Runtime,
    doc_id: i64,
    version_id: Option<i64>,
    library: &crate::kb::types::Library,
    config: &Config,
    run_id: Option<&str>,
) -> Result<()> {
    if !library.raptor_enabled {
        return Ok(());
    }

    // 在事务内校验 run_id，防止 stale job 操纵新 run 的 RAPTOR 节点
    if let Some(rid) = run_id {
        let current_run: String = conn
            .query_row(
                "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                rusqlite::params![doc_id],
                |row| row.get(0),
            )
            .unwrap_or_default();
        if current_run != rid {
            tracing::warn!(
                document_id = doc_id,
                expected = rid,
                actual = %current_run,
                "RAPTOR 重建: run_id 不匹配，跳过（stale job）"
            );
            return Ok(());
        }
    }

    // 解析 active embedding index
    let index_id: i64 = conn
        .query_row(
            "SELECT ei.id FROM kb_embedding_indexes ei \
             JOIN kb_document_nodes n ON n.library_id = ei.library_id \
             WHERE n.document_id = ?1 AND ei.is_active = 1 LIMIT 1",
            rusqlite::params![doc_id],
            |row| row.get(0),
        )
        .map_err(|_| GBrainError::InvalidInput("无 active embedding index".into()))?;

    // 加载叶节点 + 嵌入向量（限定版本，避免混入退休版本节点）
    let mut nodes: Vec<crate::kb::types::RaptorNode> = {
        let version_filter = if version_id.is_some() {
            " AND n.version_id = ?3 "
        } else {
            " "
        };
        let sql = format!(
            "SELECT n.id, n.library_id, n.document_id, n.content, n.chunk_order, \
                    n.title_path, n.page_number, n.source_start, n.source_end, \
                    n.node_metadata, n.embedding_text, e.embedding \
             FROM kb_document_nodes n \
             LEFT JOIN kb_node_embeddings e ON e.node_id = n.id AND e.embedding_index_id = ?1 \
             WHERE n.document_id = ?2 AND n.level = 0{} \
             AND n.retired_at IS NULL \
             ORDER BY n.chunk_order",
            version_filter,
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
            vec![Box::new(index_id), Box::new(doc_id)];
        if let Some(vid) = version_id {
            params.push(Box::new(vid));
        }
        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(param_refs.as_slice(), |row| {
            let id: i64 = row.get(0)?;
            let library_id: i64 = row.get(1)?;
            let document_id: i64 = row.get(2)?;
            let content: String = row.get(3)?;
            let chunk_order: i32 = row.get(4)?;
            let title_path: String = row.get(5)?;
            let page_number: Option<i32> = row.get(6)?;
            let source_start: Option<i32> = row.get(7)?;
            let source_end: Option<i32> = row.get(8)?;
            let node_metadata: String = row.get(9)?;
            let embedding_text: String = row.get(10)?;
            let embedding_blob: Option<Vec<u8>> = row.get(11)?;

            let vector = embedding_blob.map(|blob| {
                blob.chunks_exact(4)
                    .filter_map(|chunk| {
                        let bytes: [u8; 4] = chunk.try_into().ok()?;
                        Some(f32::from_le_bytes(bytes))
                    })
                    .collect::<Vec<f32>>()
            });

            Ok(crate::kb::types::RaptorNode {
                id,
                library_id,
                document_id,
                content,
                level: 0,
                parent_id: None,
                chunk_order,
                vector,
                title_path,
                page_number,
                source_start,
                source_end,
                node_metadata,
                embedding_text,
            })
        })?;
        rows.filter_map(|r| r.ok()).collect()
    };

    let raptor_config = crate::kb::raptor::RaptorConfig::default();

    if nodes.len() < raptor_config.min_nodes {
        return Ok(());
    }

    // 所有节点必须有向量（RAPTOR 聚类依赖向量）
    if nodes.iter().any(|n| n.vector.is_none()) {
        tracing::warn!(
            document_id = doc_id,
            "部分叶节点无嵌入向量，跳过 RAPTOR 重建"
        );
        return Ok(());
    }

    // 解析 RAPTOR LLM 配置
    // P3 修复: 传入完整的 resolved RAPTOR config（合并 config 文件+环境变量），
    // 保证 base_url/model 也来自已加载 config 而非仅环境变量。
    let resolved_raptor_cfg = config.raptor_config_resolved()?;
    let llm_config = crate::kb::raptor::resolve_raptor_llm_config(
        Some(library),
        config.kb_raptor_secret_ref.as_deref(),
        config.kb_raptor_base_url.as_deref(),
        if config.kb_raptor_model.is_empty() {
            None
        } else {
            Some(config.kb_raptor_model.as_str())
        },
        Some(&resolved_raptor_cfg),
    )?;

    let max_tokens = raptor_config.max_tokens_per_summary;
    let llm_cfg = llm_config.clone();

    // 构建 RAPTOR 树（原地修改 nodes）
    rt.block_on(crate::kb::raptor::build_raptor_tree(
        &raptor_config,
        &mut nodes,
        |cluster| {
            let cfg = llm_cfg.clone();
            let cluster_texts: Vec<String> = cluster.iter().map(|n| n.content.clone()).collect();
            async move {
                let temp_nodes: Vec<crate::kb::types::RaptorNode> = cluster_texts
                    .iter()
                    .enumerate()
                    .map(|(i, content)| crate::kb::types::RaptorNode {
                        id: i as i64,
                        library_id: 0,
                        document_id: 0,
                        content: content.clone(),
                        level: 0,
                        parent_id: None,
                        chunk_order: i as i32,
                        vector: None,
                        title_path: String::new(),
                        page_number: None,
                        source_start: None,
                        source_end: None,
                        node_metadata: String::new(),
                        embedding_text: String::new(),
                    })
                    .collect();
                let refs: Vec<&crate::kb::types::RaptorNode> = temp_nodes.iter().collect();
                crate::kb::raptor::summarize_cluster(&refs, &cfg, max_tokens).await
            }
        },
    ))?;

    // 幂等持久化 RAPTOR 父节点：先清理旧父节点，再插入新的。
    // 使用事务确保清理+重建原子性，避免重试产生重复父节点。
    {
        let tx = conn.unchecked_transaction()?;

        // 事务内再次校验 run_id，防止加载/LLM 构建期间新 run 启动后旧 job 仍写入
        if let Some(rid) = run_id {
            let current_run: String = conn
                .query_row(
                    "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
                    rusqlite::params![doc_id],
                    |row| row.get(0),
                )
                .unwrap_or_default();
            if current_run != rid {
                // run_id 不匹配，回滚事务并返回
                tracing::warn!(
                    document_id = doc_id,
                    expected = rid,
                    actual = %current_run,
                    "RAPTOR 重建事务: run_id 不匹配，跳过写入（stale job）"
                );
                // 不提交事务，直接返回（unchecked_transaction 需要显式 rollback）
                let _ = tx.rollback();
                return Ok(());
            }
        }

        // 1. 删除旧 RAPTOR 父节点（level > 0）及其 embeddings
        //    P0 修复: 限定同一 version_id（含 NULL 以清理遗留数据），
        //    避免误伤其他版本的父节点。
        let old_parent_ids: Vec<i64> = {
            let version_filter = if version_id.is_some() {
                "AND (version_id = ?2 OR version_id IS NULL)"
            } else {
                ""
            };
            let sql = format!(
                "SELECT id FROM kb_document_nodes WHERE document_id = ?1 AND level > 0 {}",
                version_filter,
            );
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = if let Some(vid) = version_id {
                vec![Box::new(doc_id), Box::new(vid)]
            } else {
                vec![Box::new(doc_id)]
            };
            let param_refs: Vec<&dyn rusqlite::types::ToSql> =
                params.iter().map(|p| p.as_ref()).collect();
            let rows = stmt.query_map(param_refs.as_slice(), |row| row.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };
        for &pid in &old_parent_ids {
            crate::kb::engine::cleanup_node_vectors(conn, pid);
        }
        if !old_parent_ids.is_empty() {
            let version_filter = if version_id.is_some() {
                "AND (version_id = ?2 OR version_id IS NULL)"
            } else {
                ""
            };
            let sql = format!(
                "DELETE FROM kb_document_nodes WHERE document_id = ?1 AND level > 0 {}",
                version_filter,
            );
            if let Some(vid) = version_id {
                conn.execute(&sql, rusqlite::params![doc_id, vid])?;
            } else {
                conn.execute(&sql, rusqlite::params![doc_id])?;
            }
        }

        // 2. 重置该版本叶节点的 parent_id（清除指向已删除旧父节点的引用）
        //    P0 修复: 限定同一 version_id，避免误清零其他版本叶子的 parent_id。
        {
            let version_filter = if version_id.is_some() {
                "AND version_id = ?2"
            } else {
                ""
            };
            let sql = format!(
                "UPDATE kb_document_nodes SET parent_id = NULL WHERE document_id = ?1 AND level = 0 {}",
                version_filter,
            );
            if let Some(vid) = version_id {
                conn.execute(&sql, rusqlite::params![doc_id, vid])?;
            } else {
                conn.execute(&sql, rusqlite::params![doc_id])?;
            }
        }

        // 3. 插入新 RAPTOR 父节点
        let mut id_map: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for node in &nodes {
            if node.level == 0 {
                id_map.insert(node.id, node.id);
            }
        }

        for node in &nodes {
            if node.level == 0 {
                continue;
            }

            let content_tokens = crate::nlp::chinese::tokenize_content(&node.content);
            // P0 修复: 插入父节点时写入 version_id，避免被严格检索过滤掉
            conn.execute(
                "INSERT INTO kb_document_nodes \
                 (library_id, document_id, version_id, content, content_tokens, level, chunk_order, \
                  title_path, page_number, source_start, source_end, node_metadata, embedding_text) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    node.library_id,
                    doc_id,
                    version_id,
                    node.content,
                    content_tokens,
                    node.level,
                    node.chunk_order,
                    node.title_path,
                    node.page_number,
                    node.source_start,
                    node.source_end,
                    node.node_metadata,
                    node.embedding_text,
                ],
            )?;
            let db_id = conn.last_insert_rowid();
            id_map.insert(node.id, db_id);

            // 写入父节点嵌入向量（子节点向量的平均值）
            if let Some(ref vector) = node.vector {
                crate::kb::embedding_index::upsert_node_embedding_for_index(
                    conn,
                    db_id,
                    index_id,
                    vector,
                    vector.len() as i32,
                    &config.embedding_model,
                )?;
            }
        }

        // 4. 更新叶节点的 parent_id
        for node in &nodes {
            if node.level == 0 {
                if let Some(parent_temp_id) = node.parent_id {
                    if let Some(&parent_db_id) = id_map.get(&parent_temp_id) {
                        conn.execute(
                            "UPDATE kb_document_nodes SET parent_id = ?1 WHERE id = ?2",
                            rusqlite::params![parent_db_id, node.id],
                        )?;
                    }
                }
            }
        }

        tx.commit()?;
    }

    let parent_count = nodes.iter().filter(|n| n.level > 0).count();
    tracing::info!(
        document_id = doc_id,
        parent_count,
        "re-embed worker: RAPTOR 重建完成"
    );

    Ok(())
}

/// # 问题 #2 + #15 修复：扁平化 worker 调度
/// 按优先级依次调度：kb → ocr → reembed → artifact → synonyms。
/// 每个 worker 同级调度，任意 worker 出错不影响后续 worker 执行。
/// 旧版将 synonym mining 嵌套在 artifact 的 Ok(false) 分支内，
/// artifact 出错时 synonym mining 永远不会被调度（饥饿问题）。
///
/// L2: 优先级策略说明 —— kb(文档解析/切分) > ocr(异步OCR) > reembed(重嵌入修复) > artifact(投影) > synonyms(同义词挖掘)。
/// kb 排最前因为后续所有 worker 都依赖解析完成的文档数据；
/// ocr 紧随其后因为 OCR 结果需写回文档再触发重新解析；
/// synonyms 排最后因为它是最耗时的后台任务，优先级最低。
#[allow(clippy::type_complexity)]
fn run_priority_workers(engine: &SqliteEngine, config: &Config) -> bool {
    let workers: &[(&str, fn(&SqliteEngine, &Config) -> Result<bool>)] = &[
        ("kb", run_kb_worker_once),
        ("kb_cmd", run_kb_cmd_worker_once),
        ("ocr", run_ocr_worker_once),
        ("reembed", run_reembed_worker_once),
        ("artifact", run_artifact_worker_once),
        ("synonyms", run_mine_synonyms_worker_once),
    ];

    for (name, worker_fn) in workers {
        // panic 防护上提到调度层：内部只有 kb/kb_cmd 两条路径各自包了
        // catch_unwind，reembed / ocr 等其余 worker 的 rt::block_on 一旦 panic
        // 会直接杀死 kb-worker 线程且不留日志，job 永久卡 running（孤儿回收
        // 只在进程启动时跑）。统一在调用层把 panic 转成 Err 走可恢复告警路径。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_fn(engine, config)
        }))
        .unwrap_or_else(|panic_payload| {
            let detail = panic_payload
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic_payload.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown panic".to_string());
            Err(crate::error::GBrainError::Internal(format!(
                "{name} worker panic: {detail}"
            )))
        });
        match outcome {
            Ok(true) => return true, // 有作业被处理，立即返回重新轮询
            Ok(false) => continue,   // 无作业，尝试下一个
            Err(e) => {
                // m-23 修复：区分致命错误与可恢复错误。
                // 数据库连接断开等致命错误应提前退出，避免后续 worker 在无效连接上反复失败。
                if matches!(e, crate::error::GBrainError::Database(_))
                    || matches!(e, crate::error::GBrainError::NotConnected)
                {
                    warn!(worker = *name, error = %e, "致命错误，提前退出 worker 调度");
                    return false;
                }
                // 可恢复错误：记录警告后继续尝试下一个 worker
                warn!(worker = *name, error = %e, "worker 处理循环出错");
            }
        }
    }
    false
}

/// running 作业超过该分钟数未完成即视为孤儿（进程崩溃/线程 panic 遗留）。
/// 120 分钟：大扫描 PDF 的 OCR（5 页/请求 + 重试）可能超 30 分钟，阈值过短
/// 会把另一进程正在跑的合法长作业误回收成双重执行。
const STALE_RUNNING_JOB_MINUTES: i64 = 120;

/// worker pool 空轮期间两次孤儿回收之间的最小间隔（秒）。
const STALE_RECOVER_INTERVAL_SECS: u64 = 300;

/// worker 启动时（及空闲期周期性）回收上次进程遗留的孤儿 running 作业，
/// 并顺带清扫已死进程遗留的 OCR 临时目录。
fn recover_stale_jobs_once(engine: &SqliteEngine) {
    crate::kb::temp_guard::sweep_orphaned_dirs("gbrain_ocr_");
    let Ok(conn) = engine.connection() else {
        return;
    };
    if let Err(e) = crate::kb::jobs::recover_stale_running_jobs(conn, STALE_RUNNING_JOB_MINUTES) {
        warn!(error = %e, "KB worker: 回收孤儿 running 作业失败");
    }
}

/// 以守护进程模式运行 KB worker：持续轮询并处理所有类型的 KB 作业。
/// 包括 kb_process_document（文档解析/切分/嵌入）、kb_ocr_document（异步 OCR）、
/// kb_reembed（文档级重嵌入）、kb_reembed_node（单节点修复）、kb_mine_synonyms（同义词挖掘）。
///
/// `interval_secs` 为无作业时的轮询间隔。
pub fn run_kb_worker_loop(engine: &SqliteEngine, config: &Config, interval_secs: u64) -> ! {
    info!(interval_secs, "KB worker: 启动守护进程模式");
    recover_stale_jobs_once(engine);
    loop {
        let had_work = run_priority_workers(engine, config);
        if !had_work {
            // 空轮时周期性回收孤儿 running 作业：此前只在启动时跑一次，运行期
            // 崩溃遗留的卡死作业要等下次重启才能恢复。
            recover_stale_jobs_once(engine);
            // 无待处理作业，等待后重试
            std::thread::sleep(std::time::Duration::from_secs(interval_secs));
        }
    }
}

/// 在独立线程中启动 KB worker 守护进程。
pub fn spawn_kb_worker_thread(db_path: PathBuf, config: Config, interval_secs: u64) {
    // 防御性检查：kb_enabled=false 时不启动 worker
    if !config.kb_enabled {
        info!("KB subsystem is disabled, worker thread not spawned");
        return;
    }
    std::thread::Builder::new()
        .name("kb-worker".to_string())
        .spawn(move || {
            // 在 worker 线程中创建独立的数据库连接（使用与主线程相同的 Config）
            let mut engine = SqliteEngine::with_config(&db_path, config.clone());
            if let Err(e) = engine.connect() {
                warn!(error = %e, "KB worker: 数据库连接失败");
                return;
            }
            if let Err(e) = engine.init_schema() {
                warn!(error = %e, "KB worker: 初始化 schema 失败");
                return;
            }
            info!("KB worker: 后台线程已启动");
            run_kb_worker_loop(&engine, &config, interval_secs);
        })
        .expect("无法创建 KB worker 线程");
}

// ---------------------------------------------------------------------------
// P5-023: 多 worker 并发处理
// ---------------------------------------------------------------------------

/// 启动多个并发 KB worker 线程。
///
/// 每个 worker 拥有独立的数据库连接。`claim_next_kb_job` 的原子认领
/// 保证同一作业不会被两个 worker 同时处理。
///
/// `worker_count` 为并发 worker 数量（建议不超过 CPU 核心数）。
/// `shutdown` 信号用于优雅停止所有 worker。
pub fn spawn_kb_worker_pool(
    db_path: PathBuf,
    config: Config,
    interval_secs: u64,
    worker_count: usize,
    shutdown: Arc<AtomicBool>,
) {
    if !config.kb_enabled {
        info!("KB subsystem is disabled, worker pool not spawned");
        return;
    }
    if worker_count == 0 {
        info!("KB worker pool: worker_count=0, nothing to spawn");
        return;
    }
    info!(
        worker_count,
        "KB worker pool: 启动 {} 个 worker", worker_count
    );

    for i in 0..worker_count {
        let db_path = db_path.clone();
        let config = config.clone();
        let shutdown = shutdown.clone();
        let thread_name = format!("kb-worker-{}", i);

        std::thread::Builder::new()
            .name(thread_name.clone())
            .spawn(move || {
                let mut engine = SqliteEngine::with_config(&db_path, config.clone());
                if let Err(e) = engine.connect() {
                    warn!(worker = i, error = %e, "KB worker: 数据库连接失败");
                    return;
                }
                if let Err(e) = engine.init_schema() {
                    warn!(worker = i, error = %e, "KB worker: 初始化 schema 失败");
                    return;
                }
                info!(worker = i, "KB worker: 后台线程已启动");
                if i == 0 {
                    // 只让 0 号 worker 做启动回收，避免多线程重复扫
                    recover_stale_jobs_once(&engine);
                }
                // 0 号 worker 在空轮时周期性回收：进程被 kill 时遗留的 running 作业
                // 若距 started_at 不足阈值，启动回收会放过它，之后若不再扫描就永久卡死
                // （run_kb_worker_loop 有此逻辑，APK 使用的 pool 此前没有）。
                let mut idle_secs_since_recover: u64 = 0;

                loop {
                    if shutdown.load(Ordering::Relaxed) {
                        info!(worker = i, "KB worker: 收到停止信号，退出");
                        break;
                    }
                    // #15 修复：使用扁平化调度，消除 5 层嵌套 match
                    let had_work = run_priority_workers(&engine, &config);
                    if had_work {
                        continue;
                    }
                    if i == 0 {
                        idle_secs_since_recover =
                            idle_secs_since_recover.saturating_add(interval_secs.max(1));
                        if idle_secs_since_recover >= STALE_RECOVER_INTERVAL_SECS {
                            idle_secs_since_recover = 0;
                            recover_stale_jobs_once(&engine);
                        }
                    }
                    // 无作业时短暂等待，但提前检查 shutdown
                    for _ in 0..interval_secs {
                        if shutdown.load(Ordering::Relaxed) {
                            info!(worker = i, "KB worker: 收到停止信号，退出");
                            return;
                        }
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                }
            })
            .unwrap_or_else(|_| panic!("无法创建 {} 线程", thread_name));
    }
}

/// KB 处理完成后，检查 kb_document 是否有对应的 artifact 投影，
/// 如果有且 promotion_policy 允许，则入队 `artifact_promote_extract` 作业。
///
/// 修复：入队前校验 kb_documents 的 processing_run_id 仍与传入的 run_id 匹配，
/// 防止旧 job 在 stats 成功后、入队 promotion 前，新 run 已通过 projection.rs
/// 抢占当前投影，导致旧 job 给新 occurrence 入队 promotion。
fn enqueue_artifact_promote_if_linked(
    conn: &Connection,
    kb_document_id: i64,
    run_id: &str,
) -> Result<()> {
    // 修复：将 run-id 校验、projection 查询和 job 插入合入同一 BEGIN IMMEDIATE 事务，
    // 消除竞态窗口。旧代码用 unchecked_transaction()（deferred 事务），
    // 并发上传在 promotion 入队阶段抢到写锁时可能在 INSERT job 时失败，
    // 而 KB job 已标记完成，promotion 入队只 warn 不会重试。
    // BEGIN IMMEDIATE 在 BEGIN 时即获取 RESERVED 锁，阻止其他写事务并发修改。
    conn.execute("BEGIN IMMEDIATE", [])
        .map_err(|e| GBrainError::Database(format!("开启 BEGIN IMMEDIATE 事务失败: {}", e)))?;

    // 校验 processing_run_id 仍匹配，防止旧 job 的 promotion 入队串到新 occurrence
    let current_run_id: String = conn
        .query_row(
            "SELECT processing_run_id FROM kb_documents WHERE id = ?1",
            rusqlite::params![kb_document_id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|e| {
            conn.execute("ROLLBACK", []).ok();
            GBrainError::Database(format!("查询 processing_run_id 失败: {}", e))
        })?;
    if current_run_id != run_id {
        tracing::warn!(
            kb_document_id,
            expected_run_id = run_id,
            actual_run_id = %current_run_id,
            "KB worker: promotion 入队时 run_id 已过期，跳过入队"
        );
        let _ = conn.execute("ROLLBACK", []);
        return Ok(());
    }

    // 查找 kb_document 对应的 artifact 投影
    let proj_ref = format!("kb_document:{}", kb_document_id);
    // 修复：不再用 .ok() 吞掉所有数据库错误。
    // .ok() 把真实查询/prepare/step 错误当成"无 projection"跳过，
    // 但 KB job 已标记 completed，promotion 入队永久丢失。
    // 现在只把 QueryReturnedNoRows 转为 None（确实无投影），其它错误 rollback 后返回 Err。
    let result: Option<(i64, i64, String)> = match conn.query_row(
        "SELECT ap.artifact_id, ap.occurrence_id, ao.promotion_policy
         FROM artifact_projections ap
         JOIN artifact_occurrences ao ON ao.id = ap.occurrence_id
         WHERE ap.projection_ref = ?1 AND ap.status = 'active'
         LIMIT 1",
        rusqlite::params![proj_ref],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        },
    ) {
        Ok(tuple) => Some(tuple),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(e) => {
            conn.execute("ROLLBACK", []).ok();
            return Err(GBrainError::Database(format!(
                "查询 artifact 投影失败: {}",
                e
            )));
        }
    };

    let Some((artifact_id, occurrence_id, promotion_policy)) = result else {
        debug!(
            kb_document_id,
            "KB document 无关联 artifact 投影，跳过 promotion"
        );
        let _ = conn.execute("ROLLBACK", []);
        return Ok(());
    };

    // 根据 promotion_policy 决定是否入队：
    // - none: 不触发 promotion
    // - shadow: 仅影子页面，不生成候选
    // - candidate / auto_accept_low_risk / auto_apply: 触发 promotion extraction
    match promotion_policy.as_str() {
        "none" => {
            debug!(
                artifact_id,
                kb_document_id, "promotion_policy=none，跳过 promotion"
            );
            let _ = conn.execute("ROLLBACK", []);
            return Ok(());
        }
        "shadow" => {
            debug!(
                artifact_id,
                kb_document_id, "promotion_policy=shadow，仅影子页面，不生成候选"
            );
            let _ = conn.execute("ROLLBACK", []);
            return Ok(());
        }
        _ => {} // candidate / auto_accept_low_risk / auto_apply → 继续入队
    }

    // 修复：入队时把 occurrence_id 和 promotion_policy 写进 payload，
    // worker 只使用 payload 中的稳定值，避免旧 job 被归属到新 occurrence 后串策略
    conn.execute(
        "INSERT INTO jobs (job_type, payload, status, priority, created_at)
         VALUES ('artifact_promote_extract', ?1, 'pending', 0, datetime('now'))",
        rusqlite::params![serde_json::json!({
            "artifact_id": artifact_id,
            "kb_document_id": kb_document_id,
            "occurrence_id": occurrence_id,
            "promotion_policy": promotion_policy,
        })
        .to_string()],
    )
    .map_err(|e| {
        conn.execute("ROLLBACK", []).ok();
        GBrainError::Database(format!("入队 artifact_promote_extract 失败: {}", e))
    })?;

    // 修复：COMMIT 失败时事务可能仍保持打开，必须 ROLLBACK 防止连接状态污染。
    // SQLite COMMIT 返回 busy/错误时，事务不会自动回滚，后续复用同一连接
    // 会处于事务中，且 promotion 已无重试入口。
    conn.execute("COMMIT", []).map_err(|e| {
        let _ = conn.execute("ROLLBACK", []);
        GBrainError::Database(format!("提交 promotion 入队事务失败: {}", e))
    })?;

    info!(
        artifact_id,
        kb_document_id,
        occurrence_id,
        promotion_policy = %promotion_policy,
        "已入队 artifact_promote_extract 作业"
    );
    Ok(())
}

/// 获取 artifact 的 promotion_policy
///
/// 从 artifact_occurrences 表查询 promotion_policy，
/// 找不到时降级为 "candidate"（需要手动审核）。
fn get_promotion_policy(conn: &Connection, artifact_id: i64) -> String {
    conn.query_row(
        "SELECT ao.promotion_policy
         FROM artifact_occurrences ao
         WHERE ao.artifact_id = ?1 AND ao.status = 'active'
         LIMIT 1",
        rusqlite::params![artifact_id],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "candidate".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_glm_ocr_payload_by_provider_or_model() {
        assert!(is_glm_ocr_payload("glm_ocr", ""));
        assert!(is_glm_ocr_payload("GLM_OCR", ""));
        assert!(is_glm_ocr_payload("", "glm-ocr"));
        assert!(is_glm_ocr_payload("other", "GLM-OCR"));
        assert!(!is_glm_ocr_payload("openai", "glm-4.5"));
    }

    #[test]
    fn caps_glm_ocr_pages_per_request_to_safe_range() {
        assert_eq!(effective_glm_ocr_pages_per_request(0), 1);
        assert_eq!(effective_glm_ocr_pages_per_request(1), 1);
        assert_eq!(effective_glm_ocr_pages_per_request(5), 5);
        assert_eq!(effective_glm_ocr_pages_per_request(100), 5);
    }
}
