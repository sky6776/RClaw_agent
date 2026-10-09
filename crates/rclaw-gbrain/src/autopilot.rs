//! Autopilot / self-maintaining brain
//! Mirrors gbrain's src/commands/autopilot.ts
//!
//! Periodic background task that:
//! - Embeds new/stale content
//! - Runs integrity checks (dead links, orphans)
//! - Reports brain health

use crate::config::Config;
use crate::embedding::Embedder;
use crate::engine::BrainEngine;
use crate::sqlite_engine::SqliteEngine;
use tracing::{debug, info, warn};

/// Autopilot orchestrates periodic brain maintenance tasks
pub struct Autopilot<'a> {
    engine: &'a SqliteEngine,
    _config: Config,
    embedder: Option<Embedder>,
}

impl<'a> Autopilot<'a> {
    pub fn new(engine: &'a SqliteEngine, config: Config) -> Self {
        let embedder = config.embedding_api_key.as_deref().map(|api_key| {
            Embedder::new(
                api_key,
                config
                    .embedding_base_url
                    .as_deref()
                    .expect("GBRAIN_EMBEDDING_BASE_URL 已在启动校验"),
                &config.embedding_model,
                config.embedding_dimensions,
            )
        });

        Self {
            engine,
            _config: config,
            embedder,
        }
    }

    /// Run one maintenance cycle
    pub fn run_once(&self) -> Result<(), crate::error::GBrainError> {
        info!("Autopilot: starting maintenance cycle");

        // 1. Embed new/stale chunks
        if let Some(ref embedder) = self.embedder {
            match self.embed_unembedded_chunks(embedder) {
                Ok(count) => info!(count, "Autopilot: embedded new chunks"),
                Err(e) => warn!(error = %e, "Autopilot: embedding step failed"),
            }
        } else {
            debug!("Autopilot: no embedder configured, skipping embedding step");
        }

        // 2. Integrity check
        match self.run_integrity_check() {
            Ok((orphans, dead_links)) => {
                if orphans > 0 || dead_links > 0 {
                    info!(
                        orphans,
                        dead_links, "Autopilot: integrity check found issues"
                    );
                } else {
                    info!("Autopilot: integrity check passed");
                }
            }
            Err(e) => warn!(error = %e, "Autopilot: integrity check failed"),
        }

        // 3. Health report
        match self.engine.get_health() {
            Ok(health) => {
                info!(
                    brain_score = %health.brain_score,
                    embed_coverage = %health.embed_coverage,
                    stale_pages = health.stale_pages,
                    orphan_pages = health.orphan_pages,
                    dead_links = health.dead_links,
                    "Autopilot: health report"
                );
            }
            Err(e) => warn!(error = %e, "Autopilot: health check failed"),
        }

        info!("Autopilot: maintenance cycle complete");
        Ok(())
    }

    /// Run maintenance in a loop with a given interval.
    ///
    /// C7 fix: 接受 `shutdown` 信号，每次循环迭代顶部检查标志。
    /// 当外部设置 `shutdown` 为 true 时，循环优雅退出，允许线程被 join。
    /// MCP 服务器关闭时调用方应设置此标志，避免分离线程在已关闭的数据库上继续运行。
    pub fn run_loop(&self, interval_secs: u64, shutdown: &std::sync::atomic::AtomicBool) {
        info!(interval_secs, "Autopilot: starting daemon mode");
        while !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            if let Err(e) = self.run_once() {
                warn!(error = %e, "Autopilot: cycle failed, will retry");
            }
            // 分段 sleep 以便更快响应 shutdown 信号
            let sleep_step = std::cmp::min(interval_secs, 5);
            let mut remaining = interval_secs;
            while remaining > 0 && !shutdown.load(std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(std::time::Duration::from_secs(std::cmp::min(
                    sleep_step, remaining,
                )));
                remaining = remaining.saturating_sub(sleep_step);
            }
        }
        info!("Autopilot: daemon mode stopped (shutdown signal received)");
    }

    /// Embed chunks that don't yet have embeddings
    fn embed_unembedded_chunks(
        &self,
        embedder: &Embedder,
    ) -> Result<usize, crate::error::GBrainError> {
        let stale = self.engine.list_stale_chunks(Some(1000))?;
        if stale.is_empty() {
            debug!("Autopilot: no stale chunks found, nothing to embed");
            return Ok(0);
        }
        let rt = crate::runtime::shared_runtime();
        let mut embedded = 0;
        for batch in stale.chunks(50) {
            let texts: Vec<&str> = batch.iter().map(|c| c.chunk_text.as_str()).collect();
            let embeddings = rt.block_on(embedder.embed_batch(&texts))?;
            let mut by_slug: std::collections::HashMap<String, Vec<crate::types::ChunkInput>> =
                std::collections::HashMap::new();
            for (row, embedding) in batch.iter().zip(embeddings) {
                let existing = self.engine.get_chunk_by_id(row.chunk_id)?;
                by_slug
                    .entry(row.slug.clone())
                    .or_default()
                    .push(crate::types::ChunkInput {
                        chunk_index: row.chunk_index,
                        chunk_text: row.chunk_text.clone(),
                        source: row.source.clone(),
                        token_count: row.token_count,
                        embedding: Some(embedding),
                        model: Some(
                            row.model
                                .clone()
                                .unwrap_or_else(|| "text-embedding-3-large".to_string()),
                        ),
                        language: existing.as_ref().and_then(|c| c.language.clone()),
                        symbol_name: existing.as_ref().and_then(|c| c.symbol_name.clone()),
                        symbol_type: existing.as_ref().and_then(|c| c.symbol_type.clone()),
                        start_line: existing.as_ref().and_then(|c| c.start_line),
                        end_line: existing.as_ref().and_then(|c| c.end_line),
                        parent_symbol_path: existing
                            .as_ref()
                            .and_then(|c| c.parent_symbol_path.clone()),
                        symbol_name_qualified: existing
                            .as_ref()
                            .and_then(|c| c.symbol_name_qualified.clone()),
                        doc_comment: existing.as_ref().and_then(|c| c.doc_comment.clone()),
                    });
            }
            for (slug, chunks) in by_slug {
                embedded += self.engine.upsert_chunks(&slug, &chunks)?;
            }
        }
        Ok(embedded)
    }

    /// Run integrity checks: detect orphans and dead links
    fn run_integrity_check(&self) -> Result<(i64, i64), crate::error::GBrainError> {
        let health = self.engine.get_health()?;
        Ok((health.orphan_pages, health.dead_links))
    }
}

/// 在独立后台线程中启动 autopilot 维护循环
///
/// 创建独立的数据库连接，按配置的间隔周期运行 `Autopilot::run_once`。
/// 与 KB worker 模式一致：取 db_path 和 config 的所有权，
/// 在闭包内创建 engine，避免跨线程借用问题。
const MIN_INTERVAL_SECS: u64 = 60;

/// 返回 shutdown 信号句柄，调用方设置 `store(true)` 即可通知线程优雅退出。
pub fn spawn_autopilot_thread(
    db_path: std::path::PathBuf,
    config: Config,
    interval_secs: u64,
) -> Option<std::sync::Arc<std::sync::atomic::AtomicBool>> {
    if !config.autopilot_enabled {
        tracing::info!("Autopilot is disabled, not spawning background thread");
        return None;
    }
    let interval = if interval_secs < MIN_INTERVAL_SECS {
        tracing::warn!(
            "Autopilot: interval {}s < minimum {}s, clamping to {}s",
            interval_secs,
            MIN_INTERVAL_SECS,
            MIN_INTERVAL_SECS
        );
        MIN_INTERVAL_SECS
    } else {
        interval_secs
    };
    let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();
    std::thread::Builder::new()
        .name("autopilot".to_string())
        .spawn(move || {
            let mut engine =
                crate::sqlite_engine::SqliteEngine::with_config(&db_path, config.clone());
            if let Err(e) = engine.connect() {
                tracing::warn!(error = %e, "Autopilot: 数据库连接失败");
                return;
            }
            if let Err(e) = engine.init_schema() {
                tracing::warn!(error = %e, "Autopilot: 初始化 schema 失败");
                return;
            }
            tracing::info!(interval, "Autopilot: 后台线程已启动");
            let autopilot = Autopilot::new(&engine, config);
            autopilot.run_loop(interval, &shutdown_clone);
        })
        .expect("spawn autopilot thread");
    Some(shutdown)
}
