//! Artifact 应用服务 — 统一编排入口（设计文档 §3.2）
//!
//! 所有知识操作（写入、查询、来源管理、审核变更）通过此服务编排。
//! KB、gbrain、file attachment 等内部模块不直接对外暴露。

use crate::artifact::projection;
use crate::artifact::promotion;
use crate::artifact::provenance;
use crate::artifact::query;
use crate::artifact::store;
use crate::artifact::types::{
    ArtifactHealthReport, ArtifactListItem, ArtifactQueryInput, ArtifactQueryOutput,
    ArtifactReviewActionOutput, CandidateType, DeleteImpactPreview, ReviewCandidateInput,
    RiskLevel, RoutePlan, SourceArtifact, UnifiedQueryInput, UnifiedQueryResult, UploadSourceInput,
    UploadSourceOutput,
};
use crate::config::Config;
use crate::error::{GBrainError, Result};
use crate::kb::parser::ParserRegistry;
use crate::operations::OpContext;
use crate::sqlite_engine::SqliteEngine;
use crate::types::ChunkInput;
use rusqlite::Connection;
use tracing::{debug, info, warn};

/// P2-12 修复：artifact_put --file 的内容大小上限常量（1MB），
/// 与 put_memory 的内容长度限制保持一致。
/// 之前 artifact_put --file 使用 kb_max_file_size_mb（默认 50MB），
/// 导致 1MB~50MB 的文本文件先完整读入，再在 service 层被拒绝。
pub const MAX_PUT_MEMORY_CONTENT_BYTES: usize = 1024 * 1024;

/// P2-12 修复：artifact_put --file 文本文件专用扩展名白名单。
/// artifact_put 语义是"把文件内容作为手动长期记忆写入"，
/// 只接受纯文本格式，不接受 pdf/docx/xls/xlsx 等 KB 文档类型
/// （这些应走 artifact_upload 路径）。
pub const TEXT_FILE_EXTENSIONS: &[&str] = &[
    "txt", "md", "markdown", "rst", "csv", "tsv", "json", "xml", "yaml", "yml", "toml", "html",
    "htm",
];

#[derive(Debug, Clone, Default)]
pub struct ArtifactContentOptions<'a> {
    pub content_query: Option<&'a str>,
    pub content_mode: Option<&'a str>,
    pub max_chars: Option<usize>,
    pub passage_id: Option<i64>,
}

impl ArtifactContentOptions<'_> {
    fn requests_focused_content(&self) -> bool {
        // content_mode="full" 明确要求全文，优先级最高，不进入 focused 模式
        if matches!(self.content_mode, Some("full")) {
            return false;
        }
        // 只有非空 content_query 才视为 focused 请求；
        // 空字符串（常见于 MCP/表单调用）不应触发 focused 模式，
        // 否则 include_content=true 也拿不到全文
        let has_non_empty_query = self
            .content_query
            .map(|q| !q.trim().is_empty())
            .unwrap_or(false);
        matches!(self.content_mode, Some("focused"))
            || has_non_empty_query
            || self.passage_id.is_some()
    }
}

/// put_memory 冲突检测与解析的上下文数据
#[allow(dead_code)]
struct PutResolutionContext {
    existing_artifact: Option<SourceArtifact>,
    page_conflict_detected: bool,
    last_page_hash: Option<String>,
}

/// Artifact 应用服务 — 统一知识操作编排入口
pub struct ArtifactService<'a> {
    engine: &'a SqliteEngine,
    config: &'a Config,
    ctx: OpContext,
}

impl<'a> ArtifactService<'a> {
    pub fn new(engine: &'a SqliteEngine, ctx: OpContext, config: &'a Config) -> Self {
        Self {
            engine,
            config,
            ctx,
        }
    }

    // ===== put_memory 辅助结构体与私有函数 =====

    /// 构建页面内容并计算 SHA256 哈希
    ///
    /// 如果内容不以 YAML frontmatter 开头，则在前面添加标题标题行。
    /// 返回 (处理后的页面内容, 内容 SHA256 哈希)
    fn build_page_content(content: &str, title: &str) -> (String, String) {
        let md_content = if content.starts_with("---\n") || content.starts_with("---\r\n") {
            content.to_string()
        } else {
            format!("# {}\n\n{}", title, content)
        };
        let content_sha256 = {
            use sha2::{Digest, Sha256};
            let mut hasher = Sha256::new();
            hasher.update(md_content.as_bytes());
            format!("{:x}", hasher.finalize())
        };
        (md_content, content_sha256)
    }

    /// 查询现有 artifact 并检测人工修改冲突
    ///
    /// 执行只读阶段：查找同 slug 的已有 artifact、最新 brain_page_update 投影哈希，
    /// 判断页面是否被人工修改（conflict detection）。
    /// force=true 时跳过冲突检测。
    fn resolve_existing_artifact(
        conn: &Connection,
        slug: &str,
        route_plan: &RoutePlan,
        force: bool,
    ) -> Result<PutResolutionContext> {
        let existing = store::find_artifact_by_slug(conn, slug)
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        // P1 修复：人工修改冲突检测（设计文档 §5.6）
        // P1-9 修复：冲突检测应查找同 slug 下所有 artifact 的活跃 brain_page_update 投影，
        // 而非仅依赖 find_artifact_by_slug 返回的"最新 artifact"。
        // 因为冲突分支的 pending artifact 没有 brain_page_update 投影（to_brain=false），
        // 但旧稳定 artifact 仍保留该投影作为冲突检测基线。
        // force=true 时跳过冲突检测，允许强制覆盖。
        let page_conflict_detected = if route_plan.to_brain && !force {
            // 查询当前页面的 content_hash
            let page_hash: Option<String> = conn
                .query_row(
                    "SELECT content_hash FROM pages WHERE slug = ?1 AND deleted_at IS NULL",
                    rusqlite::params![slug],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            // P2-10 修复：直接按 slug 查找最新活跃 brain_page_update 投影的 version_hash，
            // 按 artifact_projections.updated_at DESC, id DESC 排序，
            // 确保取到最近一次成功写入稳定页面时的 page hash 作为基线。
            let last_artifact_page_hash = store::find_latest_page_update_hash_by_slug(conn, slug)
                .map_err(|e| GBrainError::Database(e.to_string()))?;

            match (page_hash, last_artifact_page_hash) {
                (Some(current), Some(last)) => current != last,
                // 页面存在但无上次记录 → 无法判断是否被人工修改，不冲突
                (Some(_), None) => false,
                // 页面不存在 → 新建，不冲突
                (None, _) => false,
            }
        } else {
            false
        };

        // 查找最新 page update 哈希用于返回（可能被后续 evidence 构建使用）
        let last_page_hash = store::find_latest_page_update_hash_by_slug(conn, slug)
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        Ok(PutResolutionContext {
            existing_artifact: existing,
            page_conflict_detected,
            last_page_hash,
        })
    }

    /// 根据 artifact 状态、冲突检测结果和内容哈希判断操作类型
    ///
    /// P1-10 修复：conflict 优先级高于 no_op。
    /// 冲突后如果用户再次提交与 pending artifact 完全相同的内容，
    /// 系统不应返回 no_op（因为稳定页面仍是人工编辑后的内容，
    /// 并没有应用这份 pending 内容，用户会误以为内容已存在/无需处理）。
    /// 只有当页面当前 hash 与该 artifact 已应用 projection 的 version_hash 匹配时，
    /// 才能安全返回 no_op（说明页面内容确实与 artifact 所反映的一致）。
    fn compute_put_resolution(
        existing: Option<&SourceArtifact>,
        page_conflict_detected: bool,
        content_sha256: &str,
    ) -> &'static str {
        match existing {
            // 冲突状态优先：页面被人工修改过，即使内容相同也不能返回 no_op
            Some(a) if a.status == "active" && page_conflict_detected => "conflict",
            // 无冲突 + 内容相同 → 安全的幂等 no_op
            Some(a) if a.sha256 == content_sha256 && a.status == "active" => "no_op",
            Some(a) if a.status == "active" => "update",
            _ => "create",
        }
    }

    /// 执行 put_memory 写入事务
    ///
    /// 在事务内根据 resolution 执行 conflict/no_op/update/create 分支。
    /// 所有写入操作在同一事务内完成，保证原子性。
    #[allow(clippy::too_many_arguments)]
    fn execute_put_transaction(
        engine: &SqliteEngine,
        ctx: &OpContext,
        config: &Config,
        slug: &str,
        content: &str,
        title: Option<&str>,
        resolved_intent: &str,
        content_sha256: &str,
        page_conflict_detected: bool,
        route_plan: &RoutePlan,
        artifact_dir: &std::path::Path,
        precomputed_page_chunks: Option<Vec<ChunkInput>>,
    ) -> Result<serde_json::Value> {
        let conn = engine.connection()?;

        // 在事务内重新查询，确保数据一致性
        let existing_in_txn = store::find_artifact_by_slug(conn, slug)
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        // P1-1 修复 + P1 冲突检测：在事务内完成幂等/更新/冲突/创建写入
        // P1-10 修复：事务内 conflict 优先级高于 no_op，与只读阶段 resolution 计算一致。
        // 冲突状态下即使内容相同也不能返回 no_op，否则用户会误以为页面已包含该内容。
        match existing_in_txn {
            // 冲突状态优先
            Some(ref a) if a.status == "active" && page_conflict_detected => {
                // P1 修复：人工修改冲突 — 页面被人工修改过，不应无提示覆盖。
                // 设计文档 §5.6 要求：默认生成 review change，而不是直接覆盖。
                // 当前实现：仍创建 artifact/occurrence 保存新内容，基于该 artifact
                // 创建 promotion_candidates（suggested change），目标页为当前 slug，
                // 风险等级至少 medium。返回 change_id / review_status: "pending"，
                // 同时继续保证默认不覆盖页面。
                //
                // P1-8 修复：冲突分支不应创建 active brain_page_update 投影。
                // 原始 route_plan.to_brain=true 会让 put_manual_memory 创建
                // brain_page_update 投影，但冲突分支没有写入稳定页面，
                // 该投影的 version_hash 是 artifact sha256 而非 page content_hash，
                // 误导后续生命周期逻辑认为页面已被更新。
                // 修复：冲突分支使用 to_brain=false 的 route_plan，
                // 只保存 artifact/occurrence/KB/shadow，不创建 brain_page_update。
                // 只有 artifact_review_apply 真正写入页面后才创建该投影。
                let conflict_route_plan = RoutePlan {
                    to_kb: route_plan.to_kb,
                    to_brain: false, // 冲突分支不写稳定页面，不创建 brain_page_update 投影
                    to_shadow: route_plan.to_shadow,
                    to_file: route_plan.to_file,
                    promotion: route_plan.promotion.clone(),
                };

                let current_page_hash: Option<String> = conn
                    .query_row(
                        "SELECT content_hash FROM pages WHERE slug = ?1 AND deleted_at IS NULL",
                        rusqlite::params![slug],
                        |row| row.get(0),
                    )
                    .ok()
                    .flatten();

                // P1-9 修复：冲突分支不应 stale 旧 artifact 的 brain_page_update 投影。
                // 旧 artifact 的 brain_page_update.version_hash 是冲突检测的稳定基线，
                // stale 后后续同 slug put 会找不到基线，导致人工修改保护失效。
                // 只 stale brain_shadow_page（冲突分支会创建新的 shadow）。
                let old_projections = store::find_projections_by_artifact(conn, a.id)
                    .map_err(|e| GBrainError::Database(e.to_string()))?;
                for p in old_projections {
                    if p.status == "active" && p.projection_type == "brain_shadow_page" {
                        store::mark_projection_stale(conn, p.id, "content_updated")
                            .map_err(|e| GBrainError::Database(e.to_string()))?;
                    }
                }

                // 创建新 artifact/occurrence 保存用户提交的新内容
                // P1-8 修复：使用 conflict_route_plan（to_brain=false），
                // 不创建 brain_page_update 投影
                let manual_output = crate::artifact::upload::put_manual_memory(
                    conn,
                    slug,
                    content,
                    title,
                    Some(resolved_intent),
                    artifact_dir,
                    &engine.gbrain_dir(),
                    config.default_kb_library_id,
                    &config.embedding_model,
                    config.embedding_dimensions,
                    &config.upload_default_promotion_policy,
                    config.artifact_manual_memory_to_kb,
                    config.artifact_auto_create_inbox_library,
                    &conflict_route_plan,
                )?;

                // 基于新 artifact 创建 suggested change（promotion_candidate）
                // P1-8 修复：使用 PageUpdate 类型替代 FactClaim，
                // PageUpdate 的 apply handler 读取 payload.field 和 payload.value，
                // 与当前 payload 格式对齐，apply 后能正确写入用户提交的新内容。
                // FactClaim 的 apply handler 读取 subject_slug/predicate/object_text，
                // 与当前 payload 的 slug/content/title 不匹配，apply 后只追加空行。
                let proposed_payload = serde_json::json!({
                    "field": "compiled_truth",
                    "value": content,
                    "mode": "replace",
                })
                .to_string();
                // P2-10 修复：last_artifact_hash 使用专用查询获取最新已应用投影的 version_hash，
                // 与冲突检测基线和 evidence 内部记录保持一致。
                // 旧方案基于 existing（可能是 pending conflict artifact）查找，
                // 当 existing 没有 brain_page_update 时返回 null，
                // 导致冲突响应与 evidence 内部记录不一致。
                let last_artifact_hash_for_evidence =
                    store::find_latest_page_update_hash_by_slug(conn, slug)
                        .map_err(|e| GBrainError::Database(e.to_string()))?;
                let evidence = serde_json::json!({
                    "conflict_reason": "human_edit_detected",
                    "current_page_hash": current_page_hash,
                    "last_artifact_hash": last_artifact_hash_for_evidence,
                })
                .to_string();

                let change_id = promotion::create_candidate(
                    conn,
                    promotion::CreateCandidateInput {
                        artifact_id: manual_output.artifact_id,
                        occurrence_id: Some(manual_output.occurrence_id),
                        kb_document_id: None,
                        kb_node_id: None,
                        candidate_type: CandidateType::PageUpdate,
                        target_slug: slug.to_string(),
                        target_field: "compiled_truth".to_string(),
                        title: format!("人工修改冲突 — 建议更新 {}", slug),
                        proposed_payload,
                        evidence_json: evidence,
                        confidence: 0.7,
                        risk_level: RiskLevel::Medium,
                    },
                )
                .map_err(|e| GBrainError::Database(format!("创建冲突候选变更失败: {}", e)))?;

                return Ok(serde_json::json!({
                    "resolution": "conflict",
                    "artifact_id": manual_output.artifact_id,
                    "artifact_uid": manual_output.artifact_uid,
                    "slug": slug,
                    "change_id": change_id,
                    "review_status": "pending",
                    "detail": "页面已被人工修改，新内容已保存为 suggested change，等待审核。使用 --force 可强制覆盖，或通过 artifact_review_apply 应用变更。",
                    "current_page_hash": current_page_hash,
                    // P2-10 修复：复用 last_artifact_hash_for_evidence，
                    // 与 evidence 内部记录保持一致，避免返回 null
                    "last_artifact_hash": last_artifact_hash_for_evidence,
                }));
            }
            // P1-10 修复：no_op 在 conflict 之后匹配，确保冲突状态下不会误返回 no_op
            Some(ref a) if a.sha256 == content_sha256 && a.status == "active" => {
                // 相同内容 + 无冲突 → 幂等 no-op：touch last_seen_at，返回现有信息
                store::touch_artifact(conn, a.id)
                    .map_err(|e| GBrainError::Database(e.to_string()))?;
                return Ok(serde_json::json!({
                    "resolution": "no_op",
                    "artifact_id": a.id,
                    "artifact_uid": a.artifact_uid,
                    "slug": slug,
                    "detail": "内容完全相同，幂等返回已有 artifact",
                }));
            }
            Some(ref a) if a.status == "active" => {
                // 不同内容 → 标记旧投影为 superseded/stale，继续创建新 artifact
                // P1 修复：同 slug 不同内容更新时，不仅 stale brain_page_update，
                // 也 stale brain_shadow_page，避免新 artifact 的 active shadow projection
                // 指向旧 page 内容（设计文档 §5.6 版本策略）
                let old_projections = store::find_projections_by_artifact(conn, a.id)
                    .map_err(|e| GBrainError::Database(e.to_string()))?;
                for p in old_projections {
                    if p.status == "active"
                        && (p.projection_type == "brain_page_update"
                            || p.projection_type == "brain_shadow_page")
                    {
                        store::mark_projection_stale(conn, p.id, "content_updated")
                            .map_err(|e| GBrainError::Database(e.to_string()))?;
                    }
                }
            }
            _ => {} // 无已有 artifact 或已删除 → 正常创建
        }

        let manual_output = crate::artifact::upload::put_manual_memory(
            conn,
            slug,
            content,
            title,
            Some(resolved_intent),
            artifact_dir,
            &engine.gbrain_dir(),
            config.default_kb_library_id,
            &config.embedding_model,
            config.embedding_dimensions,
            &config.upload_default_promotion_policy,
            config.artifact_manual_memory_to_kb,
            config.artifact_auto_create_inbox_library,
            route_plan,
        )?;

        // P1-2 修复：只在 route_plan.to_brain=true 时写入 gbrain page，
        // intent=evidence 不应写 gbrain page
        if route_plan.to_brain {
            let page_title = title.unwrap_or(slug);
            let ops = crate::operations::Operations::with_config_in_transaction(
                engine,
                ctx.clone(),
                config.clone(),
            );
            let page = ops.put_page_with_precomputed_chunks(
                slug,
                page_title,
                content,
                None,
                None,
                precomputed_page_chunks,
            )?;

            // P1 修复：写入页面后，将页面的 content_hash 存储到 brain_page_update 投影的
            // version_hash 中，以便下次 artifact_put 时能检测页面是否被人工修改。
            // 之前 version_hash 存的是 artifact 的 sha256，无法与 page content_hash 比较。
            // 现在改为存储 page 的 content_hash，使冲突检测能精确判断：
            // 如果当前 page_hash != 上次写入时的 page_hash → 页面被人工修改过。
            if let Some(ref page_hash) = page.content_hash {
                // 查找刚创建的 brain_page_update 投影并更新 version_hash
                let new_projections =
                    store::find_projections_by_artifact(conn, manual_output.artifact_id)
                        .map_err(|e| GBrainError::Database(e.to_string()))?;
                // 只更新指向本页面的投影：同一 artifact 若还投影到其它 slug，
                // 用本页 hash 覆盖它们的基线会让那些页面之后全部误判为冲突。
                let page_ref = format!("brain_page:{}", slug);
                for p in new_projections {
                    if p.status == "active"
                        && p.projection_type == "brain_page_update"
                        && p.projection_ref == page_ref
                    {
                        store::update_projection_version_hash(conn, p.id, page_hash)
                            .map_err(|e| GBrainError::Database(e.to_string()))?;
                    }
                }
            }
        }

        Ok(serde_json::to_value(manual_output)
            .unwrap_or_else(|_| serde_json::json!({"status": "ok"})))
    }

    /// 手动写入长期记忆（设计文档 §4.1.2）
    ///
    /// 创建 text/manual artifact 并投影到 gbrain 页面。
    /// 步骤：
    /// 1. 创建 artifact store 记录和 projection（在事务内）
    /// 2. 调用原 gbrain put_page 写入页面内容（设计文档 §8.6 步骤 6）
    pub fn put_memory(
        &self,
        slug: &str,
        content: &str,
        title: Option<&str>,
        intent: Option<&str>,
        dry_run: bool,
        force: bool,
    ) -> Result<serde_json::Value> {
        info!(
            "put_memory start: slug={}, content_len={}, intent={:?}, dry_run={}, force={}",
            slug,
            content.len(),
            intent,
            dry_run,
            force
        );
        // 安全校验：内容长度限制（P2-12 修复：使用常量而非硬编码）
        if content.len() > MAX_PUT_MEMORY_CONTENT_BYTES {
            return Err(GBrainError::InvalidInput(format!(
                "内容长度 {} 超过上限 {} 字节",
                content.len(),
                MAX_PUT_MEMORY_CONTENT_BYTES
            )));
        }

        // 安全校验：slug 格式
        crate::security::validate_page_slug(slug)?;

        // 解析意图：用户指定 > 配置默认 > "memory"
        let resolved_intent = intent.unwrap_or(&self.config.artifact_default_intent);

        // 根据意图推断路由计划（manual=true 表示手动 put）
        let route_plan = crate::artifact::routing::infer_route_plan_from_artifact_intent(
            "md",
            "text/markdown",
            resolved_intent,
            true,
        )
        .map_err(GBrainError::InvalidInput)?;

        // 构建页面内容并计算哈希
        let page_title = title.unwrap_or(slug);
        let (_md_content, content_sha256) = Self::build_page_content(content, page_title);

        // 只读阶段：查询现有 artifact，计算 resolution
        let conn = self.engine.connection()?;
        let ctx = Self::resolve_existing_artifact(conn, slug, &route_plan, force)?;

        // 计算 resolution 类型
        let resolution = Self::compute_put_resolution(
            ctx.existing_artifact.as_ref(),
            ctx.page_conflict_detected,
            &content_sha256,
        );

        if resolution == "conflict" {
            warn!("put_memory conflict: slug={}, page modified by human", slug);
        }

        // P1-1 修复：dry_run 在任何写入之前返回，零副作用
        if dry_run {
            debug!(
                "put_memory dry_run: slug={}, resolution={}, to_brain={}, to_kb={}",
                slug, resolution, route_plan.to_brain, route_plan.to_kb
            );
            return Ok(serde_json::json!({
                "dry_run": true,
                "slug": slug,
                "intent": resolved_intent,
                "content_length": content.len(),
                "resolution": resolution,
                "route_plan": {
                    "to_kb": route_plan.to_kb,
                    "to_brain": route_plan.to_brain,
                    "to_shadow": route_plan.to_shadow,
                    "to_file": route_plan.to_file,
                    "promotion": route_plan.promotion.to_string(),
                },
            }));
        }

        // 非 dry_run：所有写入在同一个事务内完成
        let artifact_dir = self.config.artifact_dir();
        std::fs::create_dir_all(&artifact_dir)
            .map_err(|e| GBrainError::FileError(format!("创建 artifact 目录失败: {}", e)))?;
        let precomputed_page_chunks =
            if route_plan.to_brain && !matches!(resolution, "conflict" | "no_op") {
                let page_type = crate::markdown::infer_type(slug);
                Some(crate::chunker::chunk_page_content(
                    content,
                    self.config,
                    &page_type,
                ))
            } else {
                None
            };

        // 修复：将 put_page 和 artifact/projection 写入放进同一事务，
        // 避免 put_page 成功但后续 artifact/projection 失败时留下半写入状态。
        let output = self.engine.transaction_with_engine(|engine| {
            Self::execute_put_transaction(
                engine,
                &self.ctx,
                self.config,
                slug,
                content,
                title,
                resolved_intent,
                &content_sha256,
                ctx.page_conflict_detected,
                &route_plan,
                &artifact_dir,
                precomputed_page_chunks,
            )
        })?;

        info!(
            "put_memory complete: slug={}, resolution={}",
            slug,
            output
                .get("resolution")
                .unwrap_or(&serde_json::json!("unknown"))
        );
        Ok(serde_json::to_value(output).unwrap_or_else(|_| serde_json::json!({"status": "ok"})))
    }

    /// 上传文件作为知识源（设计文档 §4.1.1）
    /// 委托给现有 upload_source 逻辑
    pub fn upload_file(&self, input: UploadSourceInput) -> Result<UploadSourceOutput> {
        info!(
            "upload_file start: filename={}, intent={}, content_len={}, dry_run={}",
            input.original_name,
            input.intent,
            input.content.len(),
            input.dry_run
        );
        // P2修复：dry_run 早返回必须在任何文件系统副作用之前，
        // 避免 create_dir_all 在 dry-run 模式下仍创建 artifact 目录
        if input.dry_run {
            let sha256_hex = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(&input.content);
                format!("{:x}", hasher.finalize())
            };
            let extension = crate::artifact::types::infer_extension(&input.original_name);
            let mime_type = crate::artifact::types::infer_mime_type(&extension);
            // P2 修复：dry_run 的 route plan 需应用 promotion 策略，
            // 与 upload.rs:160-182 的真实执行路径保持一致。
            // 否则 `gbrain upload x --dry-run --promotion none` 预览仍显示 candidate。
            let route_plan = crate::artifact::types::apply_promotion_policy(
                crate::artifact::types::infer_route_plan(&extension, &mime_type, &input.intent),
                &input.promotion_policy,
                &self.config.upload_default_promotion_policy,
            );
            return Ok(UploadSourceOutput {
                artifact_id: 0,
                artifact_uid: String::new(),
                occurrence_id: 0,
                occurrence_uid: String::new(),
                sha256: sha256_hex,
                is_new: true,
                route_plan,
                projections: Vec::new(),
            });
        }

        let artifact_dir = self.config.artifact_dir();
        std::fs::create_dir_all(&artifact_dir)
            .map_err(|e| GBrainError::FileError(format!("创建 artifact 目录失败: {}", e)))?;

        self.engine
            .transaction_with_engine(|engine| {
                let conn = engine.connection()?;
                crate::artifact::upload::upload_source(
                    conn,
                    &input,
                    &artifact_dir,
                    &self.engine.gbrain_dir(),
                    self.config.default_kb_library_id,
                    &self.config.embedding_model,
                    self.config.embedding_dimensions,
                    &self.config.upload_default_promotion_policy,
                    self.config.artifact_auto_create_inbox_library,
                )
            })
            .inspect(|output| {
                info!(
                    "upload_file complete: artifact_id={}, artifact_uid={}, is_new={}",
                    output.artifact_id, output.artifact_uid, output.is_new
                );
            })
    }

    /// 统一知识查询（设计文档 §4.1.3）
    pub fn query(&self, input: UnifiedQueryInput) -> Result<UnifiedQueryResult> {
        info!(
            "query: strategy={}, limit={:?}",
            input.strategy, input.limit
        );
        let conn = self.engine.connection()?;
        query::unified_query(conn, &input, self.engine, self.config)
    }

    /// 统一查询 — 用户友好接口（设计文档 §7）
    /// 返回 ArtifactQueryOutput，隐藏内部 ID
    pub fn query_facade(&self, input: &ArtifactQueryInput) -> Result<ArtifactQueryOutput> {
        let start = std::time::Instant::now();
        let include_sources = input.include_sources.unwrap_or(false);
        let requested_mode = input.mode.as_deref().unwrap_or("auto");
        let strategy = facade_query_strategy(requested_mode)?;
        let conn = self.engine.connection()?;
        let fallback_plan = query::build_query_fallback_plan(&input.query, conn, None);
        let mut fallback_state = QueryFallbackState {
            core_terms: fallback_plan.core_terms.clone(),
            ..Default::default()
        };

        let mut query_text = input.query.clone();
        let mut mode_text = requested_mode.to_string();
        let mut internal_result = self.query(UnifiedQueryInput {
            query: query_text.clone(),
            strategy: strategy.clone(),
            limit: input.limit.map(|l| l as i64),
            filter_slug: input.filter_slug.clone(),
            include_evidence: true,
            include_provenance: include_sources,
        })?;
        fallback_state.record_query("original_query", &query_text);

        let mut candidates = Vec::new();
        let mut partial_result = has_partial_evidence_match(&internal_result).then(|| {
            (
                internal_result.clone(),
                query_text.clone(),
                mode_text.clone(),
            )
        });
        if !is_query_result_useful(&internal_result, &fallback_plan.core_terms) {
            let fallback_reason = if internal_result.total_hits == 0 {
                "primary_no_hits"
            } else {
                "primary_low_quality"
            };
            fallback_state.fallback_reason = Some(fallback_reason.to_string());

            let attempts = build_fallback_attempts(
                requested_mode,
                strategy.clone(),
                &fallback_plan,
                input.filter_slug.clone(),
            );

            for attempt in attempts {
                fallback_state.record_query(&attempt.stage, &attempt.display_query);
                let result = self.query(UnifiedQueryInput {
                    query: attempt.query.clone(),
                    strategy: attempt.strategy.clone(),
                    limit: input.limit.map(|l| l as i64),
                    filter_slug: input.filter_slug.clone(),
                    include_evidence: true,
                    include_provenance: include_sources,
                })?;
                if is_query_result_useful(&result, &fallback_plan.core_terms) {
                    fallback_state.fallback_used = true;
                    fallback_state.fallback_stage = Some(attempt.stage);
                    query_text = attempt.query;
                    mode_text = attempt.mode;
                    internal_result = result;
                    break;
                } else if has_partial_evidence_match(&result)
                    && partial_result
                        .as_ref()
                        .is_none_or(|(current, _, _)| result.total_hits > current.total_hits)
                {
                    partial_result = Some((result, attempt.query, attempt.mode));
                }
            }

            if !fallback_state.fallback_used
                && !is_query_result_useful(&internal_result, &fallback_plan.core_terms)
            {
                if let Some((partial, partial_query, partial_mode)) = partial_result {
                    fallback_state.fallback_used = true;
                    fallback_state.fallback_stage = Some("partial_results".to_string());
                    fallback_state.fallback_reason = Some("primary_low_quality".to_string());
                    query_text = partial_query;
                    mode_text = partial_mode;
                    internal_result = partial;
                } else {
                    let conn = self.engine.connection()?;
                    candidates = find_title_slug_candidates(
                        conn,
                        &fallback_plan.core_terms,
                        input.limit.unwrap_or(10).clamp(1, 25),
                        input.filter_slug.as_deref(),
                    )?;
                    if !candidates.is_empty() {
                        fallback_state.fallback_used = true;
                        fallback_state.fallback_stage = Some("title_slug".to_string());
                        fallback_state.needs_focused_context = true;
                        internal_result.brain_hits.clear();
                        internal_result.evidence_hits.clear();
                        internal_result.timeline_hits.clear();
                        internal_result.provenance_records.clear();
                        internal_result.total_hits = candidates.len() as i64;
                    } else {
                        fallback_state.fallback_used = true;
                        fallback_state.fallback_stage = Some("no_results".to_string());
                        fallback_state.no_results = true;
                        internal_result.brain_hits.clear();
                        internal_result.evidence_hits.clear();
                        internal_result.timeline_hits.clear();
                        internal_result.provenance_records.clear();
                        internal_result.total_hits = 0;
                    }
                }
            }
        }

        let elapsed = start.elapsed();

        // 将 provenance_records 转换为用户友好的 SourceRef 列表
        let conn = self.engine.connection()?;
        let all_sources: Vec<crate::artifact::types::SourceRef> = if include_sources {
            internal_result
                .provenance_records
                .iter()
                .filter(|p| p.status == "active")
                .map(|p| {
                    let artifact_info = p
                        .artifact_id
                        .and_then(|aid| store::find_artifact_by_id(conn, aid).ok().flatten());
                    crate::artifact::types::SourceRef {
                        artifact_uid: artifact_info
                            .as_ref()
                            .map(|a| a.artifact_uid.clone())
                            .unwrap_or_default(),
                        original_name: artifact_info
                            .as_ref()
                            .map(|a| Some(a.original_name.clone()))
                            .unwrap_or(None),
                        quote_text: Some(p.quote_text.clone()),
                        confidence: p.confidence,
                        brain_slug: Some(p.brain_slug.clone()),
                        brain_field: Some(p.brain_field.clone()),
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        // P2-2: used_vector 反映实际搜索策略
        let used_vector =
            !internal_result.brain_hits.is_empty() || !internal_result.evidence_hits.is_empty();

        // 转换为用户友好的输出格式
        let evidence_results: Vec<crate::artifact::types::EvidenceResult> = internal_result
            .evidence_hits
            .into_iter()
            .map(|e| {
                // P2-4 修复：evidence 的 fallback source 也受 include_sources 控制，
                // 当 include_sources=false 时不应返回任何 source 信息
                let hit_sources: Vec<crate::artifact::types::SourceRef> = if include_sources {
                    e.artifact
                        .as_ref()
                        .map(|a| {
                            let from_provenance: Vec<crate::artifact::types::SourceRef> =
                                all_sources
                                    .iter()
                                    .filter(|s| s.artifact_uid == a.artifact_uid)
                                    .cloned()
                                    .collect();
                            // 如果 provenance 已有此 artifact 的来源，直接用
                            if !from_provenance.is_empty() {
                                from_provenance
                            } else {
                                // 否则从 artifact 直接构造基础 SourceRef
                                vec![crate::artifact::types::SourceRef {
                                    artifact_uid: a.artifact_uid.clone(),
                                    original_name: Some(a.original_name.clone()),
                                    quote_text: None,
                                    confidence: 0.5,
                                    brain_slug: None,
                                    brain_field: None,
                                }]
                            }
                        })
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let artifact_uid = e.artifact.as_ref().map(|a| a.artifact_uid.clone());
                crate::artifact::types::EvidenceResult {
                    title: e.title,
                    snippet: e.snippet,
                    score: e.relevance,
                    matched_terms: e.matched_terms,
                    artifact_uid,
                    shadow_page_slug: e.shadow_page_slug,
                    passage_id: e.passage_id,
                    view_type: e.view_type,
                    source_start: e.source_start,
                    source_end: e.source_end,
                    needs_more_context: e.needs_more_context,
                    sources: hit_sources,
                }
            })
            .collect();
        let confidence = derive_query_confidence(
            internal_result.total_hits as usize,
            &evidence_results,
            candidates.len(),
            &fallback_state,
        );

        Ok(ArtifactQueryOutput {
            query: query_text,
            mode: mode_text,
            memories: internal_result
                .brain_hits
                .into_iter()
                .map(|h| {
                    let hit_sources: Vec<crate::artifact::types::SourceRef> = all_sources
                        .iter()
                        .filter(|s| s.brain_slug.as_deref() == Some(&h.slug))
                        .cloned()
                        .collect();
                    crate::artifact::types::MemoryResult {
                        slug: h.slug,
                        title: h.title,
                        summary: h.snippet,
                        score: h.relevance,
                        sources: hit_sources,
                    }
                })
                .collect(),
            evidence: evidence_results,
            timeline: internal_result
                .timeline_hits
                .into_iter()
                .map(|t| crate::artifact::types::TimelineEvent {
                    timestamp: t.event_date,
                    description: t.description,
                    slug: t.shadow_page_slug,
                })
                .collect(),
            graph: Vec::new(),
            candidates,
            meta: crate::artifact::types::QueryMeta {
                total: internal_result.total_hits as usize,
                elapsed_ms: elapsed.as_millis() as u64,
                used_vector,
                used_keyword: true,
                fallback_used: fallback_state.fallback_used,
                fallback_stage: fallback_state.fallback_stage,
                fallback_reason: fallback_state.fallback_reason,
                fallback_queries: if fallback_state.fallback_used {
                    fallback_state.fallback_queries
                } else {
                    Vec::new()
                },
                core_terms: fallback_state.core_terms,
                confidence: Some(confidence),
                needs_focused_context: fallback_state.needs_focused_context,
                no_results: fallback_state.no_results,
            },
            sources: all_sources,
        })
    }

    /// 列出建议变更（设计文档 §4.1.5）
    pub fn list_suggested_changes(
        &self,
        status: Option<&str>,
        target_slug: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<crate::artifact::types::ArtifactReviewItem>> {
        // 安全边界：clamp 参数在所有入口（CLI/MCP/测试）生效，防止负数绕过
        let limit = if limit <= 0 { 50 } else { limit.min(200) };
        let offset = offset.max(0);
        let conn = self.engine.connection()?;
        crate::artifact::review::list_suggested_changes(conn, status, target_slug, limit, offset)
    }

    /// 获取建议变更详情
    pub fn get_suggested_change(
        &self,
        change_id: i64,
    ) -> Result<Option<crate::artifact::types::ArtifactReviewItem>> {
        let conn = self.engine.connection()?;
        crate::artifact::review::get_suggested_change(conn, change_id)
    }

    /// 应用建议变更
    pub fn apply_suggested_change(&self, change_id: i64) -> Result<ArtifactReviewActionOutput> {
        info!("apply_suggested_change: change_id={}", change_id);
        self.engine
            .transaction_with_engine(|engine| {
                let conn = engine.connection()?;
                let c = crate::artifact::review::apply_suggested_change(conn, change_id)?;
                Ok(candidate_to_review_action_output(c, "applied"))
            })
            .inspect(|o| {
                info!(
                    "apply_suggested_change complete: change_id={}, target_slug={}",
                    o.change_id, o.target_slug
                );
            })
    }

    /// 拒绝建议变更
    pub fn reject_suggested_change(
        &self,
        input: ReviewCandidateInput,
    ) -> Result<ArtifactReviewActionOutput> {
        info!("reject_suggested_change: change_id={}", input.candidate_id);
        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            let c = crate::artifact::review::reject_suggested_change(conn, &input)?;
            Ok(candidate_to_review_action_output(c, "rejected"))
        })
    }

    /// 回滚已应用的建议变更
    pub fn rollback_suggested_change(&self, change_id: i64) -> Result<ArtifactReviewActionOutput> {
        info!("rollback_suggested_change: change_id={}", change_id);
        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            let c = crate::artifact::review::rollback_suggested_change(conn, change_id)?;
            Ok(candidate_to_review_action_output(c, "rolled_back"))
        })
    }

    /// 健康检查
    pub fn health_check(&self) -> Result<ArtifactHealthReport> {
        info!("health_check start");
        let conn = self.engine.connection()?;
        let report = query::check_artifact_health(conn)?;
        info!(
            "health_check complete: total={}, active={}, issues={}",
            report.total_artifacts,
            report.active_artifacts,
            report.issues.len()
        );
        Ok(report)
    }

    /// 获取 Artifact 详情
    pub fn get_artifact(&self, artifact_id: i64) -> Result<Option<SourceArtifact>> {
        let conn = self.engine.connection()?;
        store::find_artifact_by_id(conn, artifact_id)
            .map_err(|e| GBrainError::Database(e.to_string()))
    }

    /// 获取 Artifact 详情（按 UID）
    pub fn get_artifact_by_uid(&self, uid: &str) -> Result<Option<SourceArtifact>> {
        let conn = self.engine.connection()?;
        store::find_artifact_by_uid(conn, uid).map_err(|e| GBrainError::Database(e.to_string()))
    }

    /// 获取 Artifact 详情（用户友好 DTO）
    ///
    /// 按 artifact_id 查询 provenance（不是 canonical_slug），
    /// 包含 occurrences，隐藏内部字段。
    pub fn get_artifact_detail(
        &self,
        id_or_uid: &str,
        include_projections: bool,
        include_sources: bool,
        include_content: bool,
    ) -> Result<Option<crate::artifact::types::ArtifactDetailOutput>> {
        self.get_artifact_detail_with_content_options(
            id_or_uid,
            include_projections,
            include_sources,
            include_content,
            ArtifactContentOptions::default(),
        )
    }

    /// 获取 Artifact 详情，并支持 focused content 读取。
    pub fn get_artifact_detail_with_content_options(
        &self,
        id_or_uid: &str,
        include_projections: bool,
        include_sources: bool,
        include_content: bool,
        content_options: ArtifactContentOptions<'_>,
    ) -> Result<Option<crate::artifact::types::ArtifactDetailOutput>> {
        let artifact_id = self.resolve_artifact_id(id_or_uid)?;
        let conn = self.engine.connection()?;
        let artifact = store::find_artifact_by_id(conn, artifact_id)
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        match artifact {
            Some(a) => {
                let projections = if include_projections {
                    let projs = store::find_projections_by_artifact(conn, artifact_id)
                        .map_err(|e| GBrainError::Database(e.to_string()))?;
                    Some(
                        projs
                            .into_iter()
                            .map(|p| {
                                // P3-8 修复：使用语义映射替代直接赋值 projection_key/projection_ref
                                let (target, target_ref) =
                                    crate::artifact::types::map_projection_to_friendly(
                                        &p.projection_type,
                                        &p.projection_key,
                                        &p.projection_ref,
                                    );
                                crate::artifact::types::ArtifactProjectionSummary {
                                    projection_type: p.projection_type,
                                    target,
                                    target_ref,
                                    status: p.status,
                                }
                            })
                            .collect(),
                    )
                } else {
                    None
                };

                let sources = if include_sources {
                    let provs = provenance::find_provenance_by_artifact(conn, artifact_id)?;
                    Some(
                        provs
                            .into_iter()
                            .filter(|p| p.status == "active")
                            .map(|p| crate::artifact::types::SourceRef {
                                artifact_uid: a.artifact_uid.clone(),
                                original_name: Some(a.original_name.clone()),
                                quote_text: Some(p.quote_text),
                                confidence: p.confidence,
                                brain_slug: Some(p.brain_slug),
                                brain_field: Some(p.brain_field),
                            })
                            .collect(),
                    )
                } else {
                    None
                };

                let occurrences = Some(
                    store::find_occurrences_by_artifact(conn, artifact_id)
                        .map_err(|e| GBrainError::Database(e.to_string()))?
                        .into_iter()
                        .map(|o| crate::artifact::types::ArtifactOccurrenceSummary {
                            uid: o.occurrence_uid,
                            intent: Some(o.intent),
                            target_slug: Some(o.target_slug),
                            status: o.status,
                            created_at: o.created_at,
                        })
                        .collect(),
                );

                let mut content_matches = Vec::new();
                let mut content_mode = None;
                let mut content_query = content_options.content_query.map(str::to_string);
                let content = if content_options.requests_focused_content() {
                    let focused =
                        load_artifact_focused_content(conn, artifact_id, &content_options)?;
                    // 修复：默认 max_chars 省略时，必须用与内部检索预算一致的默认上限（1600），
                    // 否则 service 不做最终硬截断，joined content 仍可能超过内部 1600 预算，
                    // 且 content_matches 会携带未受预算约束的 snippet。
                    let effective_max = content_options
                        .max_chars
                        .unwrap_or(DEFAULT_FOCUSED_MAX_CHARS);
                    // 单 snippet 兜底截断：避免单条 snippet 超出整体预算（取整体预算为上限）。
                    let snippet_cap = effective_max.max(1);
                    content_matches = focused
                        .iter()
                        .map(|m| {
                            // P3 修复：service 侧二次截断时，原 query 返回的 source_start/source_end
                            // 描述的是未截断 snippet 在源文件中的范围，截断后两者不再一致。
                            // 调用方若按 source_end 去回切源文件会得到与 snippet 不匹配的区段。
                            // 策略：保留 source_start 作为起点引用，截断时把 source_end 清空，
                            // 让调用方明确知道"该范围被截断、不再精确"。
                            let original_len = m.snippet.chars().count();
                            let snippet = truncate_with_ellipsis(&m.snippet, snippet_cap);
                            let truncated = snippet.chars().count() != original_len;
                            let source_end = if truncated { None } else { m.source_end };
                            crate::artifact::types::FocusedContentMatch {
                                snippet,
                                score: m.score,
                                kb_document_id: m.kb_document_id,
                                passage_id: m.passage_id,
                                view_type: m.view_type.clone(),
                                source_start: m.source_start,
                                source_end,
                            }
                        })
                        .collect();
                    content_mode = Some("focused".to_string());
                    if content_query.is_none() {
                        if let Some(pid) = content_options.passage_id {
                            content_query = Some(format!("passage_id:{pid}"));
                        }
                    }
                    (!focused.is_empty()).then(|| {
                        let joined = focused
                            .iter()
                            .map(|m| m.snippet.as_str())
                            .collect::<Vec<_>>()
                            .join("\n\n---\n\n");
                        // 硬截断兜底：确保最终输出不超出 effective_max 预算。
                        // 截断时预留省略号长度（3字符），保证追加 "..." 后不超 max。
                        truncate_with_ellipsis(&joined, effective_max)
                    })
                } else if include_content {
                    content_mode = Some("full".to_string());
                    // 全文输出同样要有硬上限：KB 全部 chunk 拼接或重新解析原始文件可能是几十 MB，
                    // 原样塞进 MCP 响应会撑爆调用方上下文与进程内存；调用方可用 max_chars 放宽。
                    let full_cap = content_options
                        .max_chars
                        .unwrap_or(DEFAULT_FULL_MAX_CHARS)
                        .max(1);
                    load_artifact_content(conn, artifact_id, &a)?
                        .map(|text| truncate_with_ellipsis(&text, full_cap))
                } else {
                    None
                };

                Ok(Some(crate::artifact::types::ArtifactDetailOutput {
                    uid: a.artifact_uid,
                    slug: a.canonical_slug,
                    original_name: Some(a.original_name),
                    mime_type: Some(a.mime_type),
                    size_bytes: Some(a.size_bytes),
                    created_at: a.created_at,
                    updated_at: a.updated_at,
                    status: a.status,
                    extension: Some(a.extension),
                    projections,
                    sources,
                    occurrences,
                    content,
                    content_mode,
                    content_query,
                    content_matches,
                }))
            }
            None => Ok(None),
        }
    }

    /// 列出 Artifacts
    /// P2-3 修复：返回外部 DTO ArtifactListItem，隐藏内部 id/storage_path/raw metadata
    pub fn list_artifacts(&self, limit: i64, offset: i64) -> Result<Vec<ArtifactListItem>> {
        // 安全边界：clamp 参数在所有入口（CLI/MCP/测试）生效，防止负数绕过
        let limit = if limit <= 0 { 50 } else { limit.min(200) };
        let offset = offset.max(0);
        let conn = self.engine.connection()?;
        store::list_active_artifacts(conn, limit, offset)
            .map_err(|e| GBrainError::Database(e.to_string()))
            .map(|artifacts| {
                artifacts
                    .into_iter()
                    .map(|a| ArtifactListItem {
                        uid: a.artifact_uid,
                        slug: a.canonical_slug,
                        original_name: Some(a.original_name),
                        size_bytes: Some(a.size_bytes),
                        status: a.status,
                        updated_at: a.updated_at,
                    })
                    .collect()
            })
    }

    /// 软删除 Artifact
    pub fn delete_artifact(&self, artifact_id: i64) -> Result<()> {
        info!("delete_artifact: artifact_id={}", artifact_id);
        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;
            // 标记所有投影为 stale
            projection::mark_all_projections_stale(conn, artifact_id, "artifact_deleted")?;

            // 软删除关联的 occurrences
            store::soft_delete_occurrences_by_artifact(conn, artifact_id)
                .map_err(|e| GBrainError::Database(e.to_string()))?;

            // 软删除关联的 kb_documents（仍被其它活跃 artifact 共享的文档保留）
            let kb_doc_ids = projection::find_kb_document_ids_for_delete(conn, artifact_id)?;
            for kb_doc_id in kb_doc_ids {
                crate::kb::lifecycle::soft_delete_document(conn, kb_doc_id)?;
                provenance::mark_provenance_stale_by_kb_document(
                    conn,
                    kb_doc_id,
                    "kb_document_deleted",
                )?;
            }

            // 软删除 artifact
            store::soft_delete_artifact(conn, artifact_id)?;
            Ok(())
        })
    }

    /// 删除 dry-run — 返回影响预览
    pub fn delete_artifact_dry_run(&self, id_or_uid: &str) -> Result<DeleteImpactPreview> {
        let artifact_id = self.resolve_artifact_id(id_or_uid)?;
        let conn = self.engine.connection()?;
        let artifact = store::find_artifact_by_id(conn, artifact_id)
            .map_err(|e| GBrainError::Database(e.to_string()))?
            .ok_or_else(|| GBrainError::InvalidInput(format!("未找到 artifact {}", artifact_id)))?;
        let projection_count = store::find_projections_by_artifact(conn, artifact_id)
            .map_err(|e| GBrainError::Database(e.to_string()))?
            .len() as i64;
        let occurrence_count = store::find_occurrences_by_artifact(conn, artifact_id)
            .map_err(|e| GBrainError::Database(e.to_string()))?
            .len() as i64;
        let kb_document_count =
            projection::find_kb_document_ids_for_delete(conn, artifact_id)?.len() as i64;
        let provenance_count =
            provenance::find_provenance_by_artifact(conn, artifact_id)?.len() as i64;
        Ok(DeleteImpactPreview {
            artifact_id,
            artifact_uid: artifact.artifact_uid,
            artifact_status: artifact.status,
            projection_count,
            occurrence_count,
            kb_document_count,
            provenance_count,
        })
    }

    /// 获取 Provenance（按 brain slug）
    pub fn get_provenance(
        &self,
        brain_slug: &str,
    ) -> Result<Vec<crate::artifact::types::ProvenanceRecord>> {
        let conn = self.engine.connection()?;
        provenance::find_provenance_by_brain_slug(conn, brain_slug)
    }

    /// 移除知识源与某次使用的关联（设计文档 §4.1.4）
    ///
    /// 只 stale 目标页面相关 occurrence/projection，不删除 source artifact。
    /// 与 delete 的区别：detach 只解除特定 occurrence 的关联，artifact 本身保留。
    pub fn detach(
        &self,
        id_or_uid: &str,
        from_slug: &str,
        dry_run: bool,
    ) -> Result<serde_json::Value> {
        info!(
            "detach: id_or_uid={}, from_slug={}, dry_run={}",
            id_or_uid, from_slug, dry_run
        );
        // 解析 artifact ID 或 UID
        let artifact_id = self.resolve_artifact_id(id_or_uid)?;

        if dry_run {
            return Ok(serde_json::json!({
                "dry_run": true,
                "artifact_id": artifact_id,
                "from_slug": from_slug,
                "action": "detach",
                "description": format!("将 artifact {} 与 slug '{}' 的关联标记为 stale", artifact_id, from_slug),
            }));
        }

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;

            // 查找匹配的 occurrence（artifact_id + target_slug）
            let occurrences =
                store::find_active_occurrences_by_artifact_and_slug(conn, artifact_id, from_slug)?;

            if occurrences.is_empty() {
                return Err(GBrainError::InvalidInput(format!(
                    "未找到 artifact {} 与 slug '{}' 的关联",
                    artifact_id, from_slug
                )));
            }

            let mut detached_count = 0u64;
            for occ in &occurrences {
                // 将关联的 projection 标记为 stale
                projection::mark_projections_stale_by_occurrence(conn, occ.id, "detached_by_user")?;

                // P1-3 修复：将 occurrence 标记为 stale，写 stale_reason='detached_by_user'
                // 确保 restore 不会误恢复 detach 的 occurrence
                store::stale_occurrence(conn, occ.id, "detached_by_user")?;

                detached_count += 1;
            }

            Ok(serde_json::json!({
                "artifact_id": artifact_id,
                "from_slug": from_slug,
                "detached_occurrences": detached_count,
            }))
        })
    }

    /// 恢复已软删除的知识源（设计文档 §4.1.4）
    ///
    /// 重新激活 artifact 及其关联的 occurrence/projection。
    /// 同时恢复关联的 kb_documents（清除 deleted_at、重新入队处理）
    /// 和因 kb_document_deleted 而变 stale 的 provenance。
    pub fn restore(&self, id_or_uid: &str, dry_run: bool) -> Result<serde_json::Value> {
        info!("restore: id_or_uid={}, dry_run={}", id_or_uid, dry_run);
        let artifact_id = self.resolve_artifact_id(id_or_uid)?;

        if dry_run {
            return Ok(serde_json::json!({
                "dry_run": true,
                "artifact_id": artifact_id,
                "action": "restore",
                "description": format!("恢复 artifact {} 及其关联（含 KB 文档和 provenance）", artifact_id),
            }));
        }

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;

            // 检查 artifact 是否已删除
            let artifact = store::find_artifact_by_id(conn, artifact_id)?.ok_or_else(|| {
                GBrainError::InvalidInput(format!("未找到 artifact {}", artifact_id))
            })?;

            if artifact.status != "deleted" {
                return Err(GBrainError::InvalidInput(format!(
                    "artifact {} 状态为 '{}'，无需恢复",
                    artifact_id, artifact.status
                )));
            }

            // 恢复 artifact
            store::reactivate_artifact(conn, artifact_id)?;

            // 恢复关联的 occurrence
            let restored_occurrences =
                store::reactivate_occurrences_by_artifact(conn, artifact_id)?;

            // 恢复关联的 kb_documents（清除 deleted_at，重新入队处理）
            // 必须在 reactivate_projections_by_artifact 之前收集 KB doc IDs，
            // 因为 reactivate 会把 status='stale' AND stale_reason='artifact_deleted' 改成 active，
            // 之后 find_kb_document_ids_for_restore 就找不到匹配的 projection 了。
            let kb_doc_ids = projection::find_kb_document_ids_for_restore(conn, artifact_id)?;

            // 恢复关联的 projection
            let restored_projections =
                projection::reactivate_projections_by_artifact(conn, artifact_id)?;

            let mut restored_kb_documents = 0u64;
            let mut restored_provenance = 0u64;
            for kb_doc_id in kb_doc_ids {
                crate::kb::lifecycle::restore_document(conn, kb_doc_id)?;
                restored_kb_documents += 1;
                // 恢复因 kb_document_deleted 而变 stale 的 provenance
                restored_provenance +=
                    provenance::reactivate_provenance_by_kb_document(conn, kb_doc_id)?;
            }

            Ok(serde_json::json!({
                "artifact_id": artifact_id,
                "restored_occurrences": restored_occurrences,
                "restored_projections": restored_projections,
                "restored_kb_documents": restored_kb_documents,
                "restored_provenance": restored_provenance,
            }))
        })
    }

    /// 重新处理知识源（设计文档 §4.1.4）
    ///
    /// 将 artifact 的所有 projection 标记为 stale，然后根据旧投影类型
    /// 重新创建同类型投影（KB/brain/shadow/file）。
    ///
    /// 修复：之前用通用 intent 推断路由，manual memory 的 intent="memory"
    /// 映射到 to_brain=false, to_shadow=true，与原始写入
    /// brain_page_update(to_brain=true, to_shadow=false) 不一致。
    /// 现在基于旧投影类型重建，保证 reprocess 后投影类型与原始一致。
    pub fn reprocess(&self, id_or_uid: &str, dry_run: bool) -> Result<serde_json::Value> {
        info!("reprocess: id_or_uid={}, dry_run={}", id_or_uid, dry_run);
        let artifact_id = self.resolve_artifact_id(id_or_uid)?;

        if dry_run {
            return Ok(serde_json::json!({
                "dry_run": true,
                "artifact_id": artifact_id,
                "action": "reprocess",
                "description": format!("重新处理 artifact {} 的所有投影", artifact_id),
            }));
        }

        self.engine.transaction_with_engine(|engine| {
            let conn = engine.connection()?;

            // 检查 artifact 存在且活跃
            let artifact = store::find_artifact_by_id(conn, artifact_id)?.ok_or_else(|| {
                GBrainError::InvalidInput(format!("未找到 artifact {}", artifact_id))
            })?;

            if artifact.status != "active" {
                return Err(GBrainError::InvalidInput(format!(
                    "artifact {} 状态为 '{}'，无法重新处理",
                    artifact_id, artifact.status
                )));
            }

            // 步骤 1: 收集旧投影（在标 stale 之前），按 occurrence 分组
            let old_projections = store::find_projections_by_artifact(conn, artifact_id)
                .map_err(|e| GBrainError::Database(e.to_string()))?;
            // 按 (occurrence_id, projection_type) 分组，只保留 active 的
            let mut old_by_occ: std::collections::HashMap<
                (Option<i64>, String),
                Vec<&crate::artifact::types::ArtifactProjection>,
            > = std::collections::HashMap::new();
            for p in &old_projections {
                if p.status == "active" {
                    old_by_occ
                        .entry((p.occurrence_id, p.projection_type.clone()))
                        .or_default()
                        .push(p);
                }
            }

            // 步骤 2: 将所有活跃 projection 标记为 stale
            let stale_count =
                projection::mark_all_projections_stale(conn, artifact_id, "reprocess_requested")?;

            // 步骤 3: 按旧 active 投影逐条精确重建，直接复用 projection_key/projection_ref
            //
            // 修复 P1: KB 投影从旧 projection 的 projection_key=library:{id} 解析 library_id，
            // 不再依赖 occ.library_id.or(config.default_kb_library_id)（默认安装下两者均为 None）。
            // 修复 P2: brain_page_update / brain_shadow_page 直接复用旧 key/ref，
            // 不再用 page_slug/target_slug 重新构造（上传路径用 slug:{target_slug}，与 page_slug 不同）。
            // 修复 P3: file_attachment 只遍历 old_by_occ 中已过滤为 active 的投影，
            // 不再遍历 old_projections 全量（会复活历史 stale/superseded 投影）。
            let mut rebuilt_projections = Vec::new();

            for ((occ_id_opt, proj_type), projs) in &old_by_occ {
                let occ_id = match occ_id_opt {
                    Some(id) => *id,
                    None => continue,
                };

                // 确认对应 occurrence 仍为 active
                if let Some(occ) = store::find_occurrence_by_id(conn, occ_id)? {
                    if occ.status != "active" {
                        continue;
                    }
                } else {
                    continue;
                }

                for old_proj in projs {
                    match proj_type.as_str() {
                        "kb_document" => {
                            // 从旧 projection_key=library:{id} 解析 library_id
                            if let Some(id_str) = old_proj.projection_key.strip_prefix("library:") {
                                if let Ok(library_id) = id_str.parse::<i64>() {
                                    let proj_ref =
                                        crate::artifact::projection::create_kb_projection(
                                            conn,
                                            artifact_id,
                                            occ_id,
                                            library_id,
                                        )?;
                                    rebuilt_projections.push(serde_json::json!({
                                        "type": "kb_document",
                                        "occurrence_id": occ_id,
                                        "projection_ref": proj_ref,
                                    }));
                                }
                            }
                        }
                        "brain_page_update" | "brain_shadow_page" | "file_attachment" => {
                            // 直接复用旧投影的 projection_key 和 projection_ref
                            let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
                            let proj = crate::artifact::types::ArtifactProjection {
                                id: 0,
                                created_at: now.clone(),
                                updated_at: now,
                                artifact_id,
                                occurrence_id: Some(occ_id),
                                projection_type: proj_type.clone(),
                                projection_key: old_proj.projection_key.clone(),
                                projection_ref: old_proj.projection_ref.clone(),
                                status: "active".to_string(),
                                // brain_page_update.version_hash 是"上次写入时页面 content_hash"
                                // 这一冲突检测基线（见 resolve_put_context）。reprocess 并不重写页面，
                                // 所以原样沿用旧投影的基线：写成 artifact sha256 会让之后每次
                                // artifact_put 都误判为冲突；改成当前页面 hash 则会把 reprocess 之前
                                // 的人工修改当成基线，下一次 put 直接静默覆盖。
                                version_hash: if proj_type == "brain_page_update" {
                                    old_proj.version_hash.clone()
                                } else {
                                    String::new()
                                },
                                stale_reason: String::new(),
                                // 保留旧投影的 metadata：brain_page_update 投影靠其中的
                                // candidate_id 与候选关联，写成 "{}" 会让 reprocess 之后
                                // 该候选的 rollback 永远找不到投影而失败。
                                metadata_json: old_proj.metadata_json.clone(),
                                superseded_by: None,
                            };
                            store::insert_projection(conn, &proj)?;
                            rebuilt_projections.push(serde_json::json!({
                                "type": proj_type,
                                "occurrence_id": occ_id,
                                "projection_ref": old_proj.projection_ref,
                            }));
                        }
                        _ => {}
                    }
                }
            }

            Ok(serde_json::json!({
                "artifact_id": artifact_id,
                "stale_projections": stale_count,
                "rebuilt_projections": rebuilt_projections,
                "status": "reprocessed",
            }))
        })
    }

    /// 解析 artifact ID 或 UID
    ///
    /// 支持数字 ID 或字符串 UID 两种格式
    pub fn resolve_artifact_id(&self, id_or_uid: &str) -> Result<i64> {
        if let Ok(id) = id_or_uid.parse::<i64>() {
            return Ok(id);
        }
        // 按 UID 查找
        let conn = self.engine.connection()?;
        let artifact = store::find_artifact_by_uid(conn, id_or_uid)?
            .ok_or_else(|| GBrainError::InvalidInput(format!("未找到 artifact '{}'", id_or_uid)))?;
        Ok(artifact.id)
    }
}

#[derive(Default)]
struct QueryFallbackState {
    fallback_used: bool,
    fallback_stage: Option<String>,
    fallback_reason: Option<String>,
    fallback_queries: Vec<String>,
    core_terms: Vec<String>,
    needs_focused_context: bool,
    no_results: bool,
}

impl QueryFallbackState {
    fn record_query(&mut self, stage: &str, query: &str) {
        let entry = format!("{}: {}", stage, query);
        if !self.fallback_queries.iter().any(|q| q == &entry) {
            self.fallback_queries.push(entry);
        }
    }
}

struct FallbackAttempt {
    stage: String,
    query: String,
    display_query: String,
    mode: String,
    strategy: crate::artifact::types::QueryStrategy,
}

fn facade_query_strategy(mode: &str) -> Result<crate::artifact::types::QueryStrategy> {
    match mode {
        "memory" | "auto" => Ok(crate::artifact::types::QueryStrategy::BrainFirst),
        "evidence" => Ok(crate::artifact::types::QueryStrategy::EvidenceFirst),
        "timeline" => Ok(crate::artifact::types::QueryStrategy::TimelineFirst),
        "graph" => Err(crate::error::GBrainError::InvalidInput(
            "artifact_query mode=graph 尚未实现，请使用 mode=auto/memory/evidence/timeline".into(),
        )),
        other => Err(crate::error::GBrainError::InvalidInput(format!(
            "未知查询模式: {}，有效值: auto/memory/evidence/timeline",
            other
        ))),
    }
}

fn build_fallback_attempts(
    requested_mode: &str,
    original_strategy: crate::artifact::types::QueryStrategy,
    plan: &query::QueryFallbackPlan,
    filter_slug: Option<String>,
) -> Vec<FallbackAttempt> {
    let mut attempts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push_attempt =
        |stage: &str,
         query: Option<String>,
         display_query: Option<String>,
         mode: &str,
         strategy: crate::artifact::types::QueryStrategy| {
            let Some(query) = query.filter(|q| !q.trim().is_empty()) else {
                return;
            };
            let key = format!("{}|{}|{:?}|{:?}", stage, query, strategy, filter_slug);
            if !seen.insert(key) {
                return;
            }
            attempts.push(FallbackAttempt {
                stage: stage.to_string(),
                display_query: display_query.unwrap_or_else(|| query.clone()),
                query,
                mode: mode.to_string(),
                strategy,
            });
        };

    push_attempt(
        "core_terms",
        plan.core_query.clone(),
        plan.core_query.clone(),
        requested_mode,
        original_strategy.clone(),
    );
    push_attempt(
        "core_terms_or",
        plan.expanded_query.clone(),
        plan.expanded_display_query.clone(),
        requested_mode,
        original_strategy,
    );
    push_attempt(
        "evidence_mode",
        plan.expanded_query
            .clone()
            .or_else(|| plan.core_query.clone()),
        plan.expanded_display_query
            .clone()
            .or_else(|| plan.core_query.clone()),
        "evidence",
        crate::artifact::types::QueryStrategy::EvidenceFirst,
    );

    attempts
}

fn is_query_result_useful(result: &UnifiedQueryResult, core_terms: &[String]) -> bool {
    if result.total_hits <= 0 {
        return false;
    }
    if !result.brain_hits.is_empty() || !result.timeline_hits.is_empty() {
        return true;
    }
    if result.evidence_hits.is_empty() {
        return false;
    }
    if core_terms.len() < 2 {
        return true;
    }
    let required = core_terms.len().min(2);
    result.evidence_hits.iter().any(|hit| {
        hit.matched_terms.len() >= required
            || core_term_coverage(&hit.snippet, &hit.title, core_terms) >= required
    })
}

fn has_partial_evidence_match(result: &UnifiedQueryResult) -> bool {
    result
        .evidence_hits
        .iter()
        .any(|hit| !hit.matched_terms.is_empty())
}

fn core_term_coverage(text: &str, title: &str, core_terms: &[String]) -> usize {
    let combined = format!("{} {}", title, text).to_lowercase();
    core_terms
        .iter()
        .filter(|term| combined.contains(&term.to_lowercase()))
        .count()
}

fn find_title_slug_candidates(
    conn: &Connection,
    core_terms: &[String],
    limit: usize,
    filter_slug: Option<&str>,
) -> Result<Vec<crate::artifact::types::DocumentCandidate>> {
    if core_terms.is_empty() {
        return Ok(Vec::new());
    }

    // M-19 修复：当 filter_slug 为 None 时，无法将 core_terms 下推到 SQL 层做 FTS/LIKE 过滤，
    // 因为 source_artifacts 表没有 FTS 索引，只能先 LIMIT 拉取再在 Rust 侧做字符串匹配。
    // 这种情况下如果结果达到 LIMIT 上限（1000 条），说明可能有遗漏，需要 warn 提示。
    // 未来优化方向：为 source_artifacts 添加 FTS5 虚拟表，或使用 LIKE 子句下推 core_terms。
    let slug_variants: Vec<String> = filter_slug
        .map(query::slug_value_variants)
        .unwrap_or_default();

    let (where_extra, params): (String, Vec<Box<dyn rusqlite::types::ToSql>>) =
        if slug_variants.is_empty() {
            (String::new(), Vec::new())
        } else {
            let placeholders: Vec<String> = slug_variants
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect();
            let params: Vec<Box<dyn rusqlite::types::ToSql>> = slug_variants
                .iter()
                .map(|v| Box::new(v.clone()) as Box<dyn rusqlite::types::ToSql>)
                .collect();
            (
                format!(" AND sa.canonical_slug IN ({})", placeholders.join(",")),
                params,
            )
        };

    let sql = format!(
        "SELECT DISTINCT sa.artifact_uid, sa.canonical_slug, sa.original_name,
                COALESCE(d.title, ''), COALESCE(d.original_name, '')
         FROM source_artifacts sa
         LEFT JOIN artifact_projections ap
              ON ap.artifact_id = sa.id
             AND ap.projection_type = 'kb_document'
             AND ap.projection_ref LIKE 'kb_document:%'
             AND ap.status = 'active'
         LEFT JOIN kb_documents d
              ON d.id = CAST(substr(ap.projection_ref, length('kb_document:') + 1) AS INTEGER)
             AND d.deleted_at IS NULL
         WHERE sa.status = 'active'{where_extra}
         ORDER BY sa.updated_at DESC
         LIMIT 1000"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| GBrainError::Database(format!("准备标题候选查询失败: {}", e)))?;

    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .map_err(|e| GBrainError::Database(format!("标题候选查询失败: {}", e)))?;

    let required = if core_terms.len() >= 3 { 2 } else { 1 };
    let mut candidates = Vec::new();
    let mut raw_row_count = 0usize;
    for row in rows {
        raw_row_count += 1;
        let (artifact_uid, slug, artifact_name, doc_title, doc_name) = row?;
        let combined =
            format!("{} {} {} {}", slug, artifact_name, doc_title, doc_name).to_lowercase();
        let coverage = core_terms
            .iter()
            .filter(|term| combined.contains(&term.to_lowercase()))
            .count();
        if coverage < required {
            continue;
        }
        let title = if !doc_title.trim().is_empty() {
            doc_title
        } else if !doc_name.trim().is_empty() {
            doc_name.clone()
        } else {
            artifact_name.clone()
        };
        candidates.push(crate::artifact::types::DocumentCandidate {
            title,
            original_name: Some(if doc_name.is_empty() {
                artifact_name
            } else {
                doc_name
            }),
            artifact_uid: Some(artifact_uid.clone()),
            slug: Some(slug),
            score: coverage as f64 / core_terms.len().max(1) as f64,
            reason: "title_slug_original_name".to_string(),
            suggested_action: format!(
                "artifact_get(id_or_uid=\"{}\", content_mode=\"focused\", content_query=\"{}\")",
                artifact_uid,
                core_terms.join(" ")
            ),
        });
    }
    // M-19: 当无 filter_slug 且 SQL 返回行数达到 LIMIT 上限时，
    // 可能有匹配的候选被截断遗漏，打印 warn 便于排查
    if filter_slug.is_none() && raw_row_count >= 1000 {
        warn!(
            "find_title_slug_candidates: 无 filter_slug 时 SQL 返回 {} 行达到 LIMIT 上限，可能有遗漏候选",
            raw_row_count
        );
    }
    candidates.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(limit);
    Ok(candidates)
}

fn derive_query_confidence(
    total_hits: usize,
    evidence: &[crate::artifact::types::EvidenceResult],
    candidate_count: usize,
    fallback: &QueryFallbackState,
) -> String {
    if fallback.no_results || total_hits == 0 {
        return "none".to_string();
    }
    if candidate_count > 0 {
        return "low".to_string();
    }
    let required = fallback.core_terms.len().min(2);
    if required > 0
        && evidence
            .iter()
            .any(|e| e.matched_terms.len() >= required && !fallback.fallback_used)
    {
        return "high".to_string();
    }
    if fallback.fallback_stage.as_deref() == Some("core_terms_or")
        || fallback.fallback_stage.as_deref() == Some("evidence_mode")
    {
        return "medium".to_string();
    }
    "medium".to_string()
}

fn load_artifact_content(
    conn: &Connection,
    artifact_id: i64,
    artifact: &SourceArtifact,
) -> Result<Option<String>> {
    if let Some(content) = load_artifact_content_from_kb(conn, artifact_id)? {
        return Ok(Some(content));
    }
    load_artifact_content_from_storage(artifact)
}

/// 默认 focused content 总预算（字符数）。
/// 来源：与 `query::query_focused_content_for_artifact` 内部检索预算保持一致，
/// 调用方未显式传入 `max_chars` 时按此值兜底，避免输出失控。
const DEFAULT_FOCUSED_MAX_CHARS: usize = 1600;

/// `include_content=true` 全文模式的默认字符上限（未显式传 `max_chars` 时生效）。
const DEFAULT_FULL_MAX_CHARS: usize = 200_000;

fn load_artifact_focused_content(
    conn: &Connection,
    artifact_id: i64,
    options: &ArtifactContentOptions<'_>,
) -> Result<Vec<query::FocusedContentCandidate>> {
    query::query_focused_content_for_artifact(
        conn,
        artifact_id,
        options.content_query,
        options.max_chars.unwrap_or(DEFAULT_FOCUSED_MAX_CHARS),
        options.passage_id,
        5,
    )
}

/// 通用字符截断助手：超出 `max` 时保留 `max - 3` 字符并追加 "..."。
/// 当 `max < 4` 时直接截取 `max` 字符不追加省略号，避免长度反而超出。
fn truncate_with_ellipsis(text: &str, max: usize) -> String {
    let len = text.chars().count();
    if len <= max {
        return text.to_string();
    }
    if max < 4 {
        return text.chars().take(max).collect();
    }
    let budget = max.saturating_sub(3);
    let truncated: String = text.chars().take(budget).collect();
    format!("{}...", truncated)
}

fn load_artifact_content_from_kb(conn: &Connection, artifact_id: i64) -> Result<Option<String>> {
    let projections = store::find_projections_by_artifact(conn, artifact_id)
        .map_err(|e| GBrainError::Database(e.to_string()))?;

    // M30 修复：将 prepare 移到循环外，避免每次迭代重复编译 SQL 语句
    let mut stmt = conn
        .prepare(
            "SELECT n.content FROM kb_document_nodes n
             JOIN kb_documents d ON d.id = n.document_id
             WHERE n.document_id = ?1 AND n.level = 0
             AND d.current_version_id IS NOT NULL
             AND n.version_id = d.current_version_id
             AND n.retired_at IS NULL
             AND d.index_status = 'ready'
             ORDER BY n.chunk_order, n.id",
        )
        .map_err(|e| GBrainError::Database(e.to_string()))?;

    for projection in projections {
        if projection.status != "active" || projection.projection_type != "kb_document" {
            continue;
        }

        let Some(document_id) = projection
            .projection_ref
            .strip_prefix("kb_document:")
            .and_then(|id| id.parse::<i64>().ok())
        else {
            continue;
        };

        let rows = stmt
            .query_map(rusqlite::params![document_id], |row| {
                row.get::<_, String>(0)
            })
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        let mut chunks = Vec::new();
        for row in rows {
            let chunk = row.map_err(|e| GBrainError::Database(e.to_string()))?;
            if !chunk.trim().is_empty() {
                chunks.push(chunk);
            }
        }

        if !chunks.is_empty() {
            return Ok(Some(chunks.join("\n\n")));
        }
    }

    Ok(None)
}

fn load_artifact_content_from_storage(artifact: &SourceArtifact) -> Result<Option<String>> {
    let bytes = std::fs::read(&artifact.storage_path).map_err(|e| {
        GBrainError::FileError(format!(
            "读取 artifact 原始文件失败 '{}': {}",
            artifact.storage_path, e
        ))
    })?;

    match ParserRegistry::new().parse(&artifact.extension, &bytes) {
        Ok(parsed) => Ok(Some(parsed.content)),
        Err(parse_error) => match String::from_utf8(bytes) {
            Ok(text) => Ok(Some(text)),
            Err(_) => Err(parse_error),
        },
    }
}

/// 将 PromotionCandidate 转换为用户友好的 ArtifactReviewActionOutput
fn candidate_to_review_action_output(
    c: crate::artifact::types::PromotionCandidate,
    action: &str,
) -> crate::artifact::types::ArtifactReviewActionOutput {
    let evidence = if c.evidence_json.is_empty() {
        None
    } else {
        serde_json::from_str::<serde_json::Value>(&c.evidence_json).ok()
    };
    let action_description = format!(
        "{} 变更 #{} ({}) 到页面 {}",
        action, c.id, c.candidate_type, c.target_slug
    );
    crate::artifact::types::ArtifactReviewActionOutput {
        change_id: c.id,
        target_slug: c.target_slug,
        change_type: c.candidate_type,
        status: c.status,
        action_description,
        evidence,
        risk_level: Some(c.risk_level),
    }
}
