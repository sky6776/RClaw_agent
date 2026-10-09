//! 候选变更管理 — 从 KB 证据抽取候选，审核/应用/拒绝
//!
//! 负责：
//! 1. KB 处理完成后触发 promotion extraction
//! 2. 从 kb_documents metadata 提取 document_summary / entity / keyword / timeline 候选
//! 3. 候选的 CRUD 和审核流程
//! 4. apply 时写入 gbrain page 并记录 provenance

use rusqlite::{params, Connection, Row};
use tracing::{debug, error, info, warn};

use crate::error::{GBrainError, Result};

use super::provenance;
use super::store;
use super::types::*;

/// 从 KB 文档提取候选变更
///
/// 在 KB 处理完成后调用，从 kb_documents 和 kb_document_nodes 提取：
/// - document_summary: 文档摘要
/// - entity: 实体
/// - keyword: 关键词
/// - timeline: 时间线事件
pub fn extract_promotion_candidates(
    conn: &Connection,
    artifact_id: i64,
    occurrence_id: i64,
    kb_document_id: i64,
) -> Result<Vec<i64>> {
    let mut candidate_ids = Vec::new();

    // 1. 获取 KB 文档信息
    let kb_doc = get_kb_document_info(conn, kb_document_id)?;

    // 2. 获取 KB 文档节点
    let nodes = get_kb_document_nodes(conn, kb_document_id)?;

    // 修复：target_slug 应使用影子页面的实际 slug（来自 artifact_projections），
    // 而不是 KB 的 canonical_slug（来自 name_tokens/original_name）。
    // 影子页面创建在 documents/{artifact_slug_with_hash}，
    // 但 KB 的 canonical_slug 不含 hash 后缀，导致 apply 时拼出的
    // documents/{candidate.target_slug} 找不到页面。
    let target_slug = resolve_shadow_page_slug(conn, artifact_id)
        .unwrap_or_else(|| kb_doc.canonical_slug.clone());

    // 3. 提取 document_summary 候选
    if let Some(summary) = &kb_doc.summary {
        if !summary.is_empty() {
            let id = create_candidate(
                conn,
                CreateCandidateInput {
                    artifact_id,
                    occurrence_id: Some(occurrence_id),
                    kb_document_id: Some(kb_document_id),
                    kb_node_id: None,
                    candidate_type: CandidateType::DocumentSummary,
                    target_slug: target_slug.clone(),
                    target_field: "summary".to_string(),
                    title: format!("Summary of {}", kb_doc.original_name),
                    proposed_payload: serde_json::json!({ "summary": summary }).to_string(),
                    evidence_json: serde_json::json!({ "kb_document_id": kb_document_id })
                        .to_string(),
                    confidence: 0.8,
                    risk_level: RiskLevel::Low,
                },
            )?;
            candidate_ids.push(id);
        }
    }

    // 4. 提取 keyword 候选 — 每个关键词生成单独的 FactClaim 候选
    // 修复：之前写成 { "keywords": [...] } 批量 payload，但 apply_fact_claim_candidate
    // 读取 subject_slug/predicate/object_text，导致候选即使被接受也产生空内容
    if let Some(keywords) = &kb_doc.keywords {
        for keyword in keywords {
            let id = create_candidate(
                conn,
                CreateCandidateInput {
                    artifact_id,
                    occurrence_id: Some(occurrence_id),
                    kb_document_id: Some(kb_document_id),
                    kb_node_id: None,
                    candidate_type: CandidateType::FactClaim,
                    target_slug: target_slug.clone(),
                    target_field: "keywords".to_string(),
                    title: format!("Keyword: {} (from {})", keyword, kb_doc.original_name),
                    proposed_payload: serde_json::json!({
                        "subject_slug": target_slug,
                        "predicate": "keyword",
                        "object_text": keyword,
                    })
                    .to_string(),
                    evidence_json: serde_json::json!({ "kb_document_id": kb_document_id })
                        .to_string(),
                    confidence: 0.7,
                    risk_level: RiskLevel::Low,
                },
            )?;
            candidate_ids.push(id);
        }
    }

    // 5. 提取 entity 候选 — 每个实体生成单独的 EntityMention 候选
    // 修复：之前写成 { "entities": [...] } 批量 payload，但 apply_entity_candidate
    // 读取 entity_name/suggested_slug，导致候选即使被接受也产生空内容
    if let Some(entities) = &kb_doc.entity_names {
        for entity_name in entities {
            let suggested_slug = entity_name
                .to_lowercase()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let id = create_candidate(
                conn,
                CreateCandidateInput {
                    artifact_id,
                    occurrence_id: Some(occurrence_id),
                    kb_document_id: Some(kb_document_id),
                    kb_node_id: None,
                    candidate_type: CandidateType::EntityMention,
                    target_slug: target_slug.clone(),
                    target_field: "entities".to_string(),
                    title: format!("Entity: {} (from {})", entity_name, kb_doc.original_name),
                    proposed_payload: serde_json::json!({
                        "entity_name": entity_name,
                        "suggested_slug": suggested_slug,
                    })
                    .to_string(),
                    evidence_json: serde_json::json!({ "kb_document_id": kb_document_id })
                        .to_string(),
                    confidence: 0.75,
                    risk_level: RiskLevel::Medium,
                },
            )?;
            candidate_ids.push(id);
        }
    }

    // 6. 从节点中提取 timeline_event 候选
    for node in &nodes {
        if let Some(node_type) = &node.node_type {
            if node_type == "timeline_event" || node_type == "date" {
                let id = create_candidate(
                    conn,
                    CreateCandidateInput {
                        artifact_id,
                        occurrence_id: Some(occurrence_id),
                        kb_document_id: Some(kb_document_id),
                        kb_node_id: Some(node.id),
                        candidate_type: CandidateType::TimelineEvent,
                        target_slug: target_slug.clone(),
                        target_field: "timeline".to_string(),
                        title: format!("Timeline event from {}", kb_doc.original_name),
                        proposed_payload: serde_json::json!({
                            "title": node.title,
                            "content": node.content,
                            "date": node.metadata.get("date"),
                        })
                        .to_string(),
                        evidence_json: serde_json::json!({
                            "kb_document_id": kb_document_id,
                            "kb_node_id": node.id,
                        })
                        .to_string(),
                        confidence: 0.6,
                        risk_level: RiskLevel::Medium,
                    },
                )?;
                candidate_ids.push(id);
            }
        }
    }

    // 7. 提取 link_suggestion 候选（从实体名称生成）
    if let Some(entities) = &kb_doc.entity_names {
        for entity_name in entities {
            let suggested_slug = entity_name
                .to_lowercase()
                .chars()
                .map(|c| {
                    if c.is_alphanumeric() || c == '-' || c == '_' {
                        c
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let id = create_candidate(
                conn,
                CreateCandidateInput {
                    artifact_id,
                    occurrence_id: Some(occurrence_id),
                    kb_document_id: Some(kb_document_id),
                    kb_node_id: None,
                    candidate_type: CandidateType::LinkSuggestion,
                    target_slug: target_slug.clone(),
                    target_field: "links".to_string(),
                    title: format!("Link suggestion: {} → {}", target_slug, suggested_slug),
                    proposed_payload: serde_json::json!({
                        "from_slug": target_slug,
                        "to_slug": suggested_slug,
                        "link_type": "related",
                        "entity_name": entity_name,
                    })
                    .to_string(),
                    evidence_json: serde_json::json!({ "kb_document_id": kb_document_id })
                        .to_string(),
                    confidence: 0.5,
                    risk_level: RiskLevel::Low,
                },
            )?;
            candidate_ids.push(id);
        }
    }

    // 8. 提取 fact_claim 候选（从摘要中提取关键事实）
    if let Some(summary) = &kb_doc.summary {
        if !summary.is_empty() {
            let id = create_candidate(
                conn,
                CreateCandidateInput {
                    artifact_id,
                    occurrence_id: Some(occurrence_id),
                    kb_document_id: Some(kb_document_id),
                    kb_node_id: None,
                    candidate_type: CandidateType::FactClaim,
                    target_slug: target_slug.clone(),
                    target_field: "compiled_truth".to_string(),
                    title: format!("Fact claim from {}", kb_doc.original_name),
                    proposed_payload: serde_json::json!({
                        "subject_slug": target_slug,
                        "predicate": "summary",
                        "object_text": summary,
                    })
                    .to_string(),
                    evidence_json: serde_json::json!({ "kb_document_id": kb_document_id })
                        .to_string(),
                    confidence: 0.6,
                    risk_level: RiskLevel::Medium,
                },
            )?;
            candidate_ids.push(id);
        }
    }

    info!(
        "从 KB 文档 {} 提取了 {} 个候选变更",
        kb_document_id,
        candidate_ids.len()
    );

    Ok(candidate_ids)
}

/// 创建候选变更记录
///
/// 修复：使用 candidate_fingerprint 做去重，INSERT OR IGNORE 防止重试路径重复创建。
/// fingerprint = SHA256(artifact_id|candidate_type|target_slug|target_field|proposed_payload)
/// 同一 artifact + 同一内容不应重复创建候选。重试时如果候选已存在（pending/accepted/applied），
/// INSERT OR IGNORE 会跳过，返回已有候选的 id，保证幂等。
///
pub struct CreateCandidateInput {
    pub artifact_id: i64,
    pub occurrence_id: Option<i64>,
    pub kb_document_id: Option<i64>,
    pub kb_node_id: Option<i64>,
    pub candidate_type: CandidateType,
    pub target_slug: String,
    pub target_field: String,
    pub title: String,
    pub proposed_payload: String,
    pub evidence_json: String,
    pub confidence: f64,
    pub risk_level: RiskLevel,
}

pub fn create_candidate(conn: &Connection, input: CreateCandidateInput) -> Result<i64> {
    // 修复：计算候选指纹用于去重，防止重试路径重复创建同一批候选。
    // fingerprint 由确定性字段组成：artifact_id + candidate_type + target_slug +
    // target_field + proposed_payload，不含 confidence/risk_level 等可能因
    // KB 重新解析而变化的字段。
    let fingerprint = compute_candidate_fingerprint(
        input.artifact_id,
        &input.candidate_type,
        &input.target_slug,
        &input.target_field,
        &input.proposed_payload,
    );

    let now = now_str();
    // 修复：使用 INSERT OR IGNORE + 唯一索引实现幂等插入。
    // 唯一索引 idx_promo_candidates_fingerprint 仅覆盖
    // candidate_fingerprint != '' AND status IN ('pending', 'accepted', 'applied')，
    // 已回滚/拒绝/过期的候选不在索引中，允许重新创建。
    // INSERT OR IGNORE 在指纹冲突时跳过插入（候选已存在），返回已有 id。
    let rows = conn.execute(
        "INSERT OR IGNORE INTO promotion_candidates
            (artifact_id, occurrence_id, kb_document_id, kb_node_id,
             candidate_type, target_slug, target_field,
             title, proposed_payload, evidence_json,
             confidence, risk_level, status, reviewer, review_notes, applied_at,
             candidate_fingerprint, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
        params![
            input.artifact_id,
            input.occurrence_id,
            input.kb_document_id,
            input.kb_node_id,
            input.candidate_type.to_string(),
            input.target_slug,
            input.target_field,
            input.title,
            input.proposed_payload,
            input.evidence_json,
            input.confidence,
            input.risk_level.to_string(),
            "pending",
            "",
            "",
            Option::<String>::None,
            &fingerprint,
            now,
            now,
        ],
    )
    .map_err(|e| GBrainError::Database(format!("插入候选变更失败: {}", e)))?;

    if rows == 0 {
        // 候选已存在（fingerprint 冲突），返回已有候选的 id
        let existing_id: i64 = conn
            .query_row(
                "SELECT id FROM promotion_candidates WHERE candidate_fingerprint = ?1 AND status IN ('pending', 'accepted', 'applied') LIMIT 1",
                params![&fingerprint],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(format!("查询已有候选失败: {}", e)))?;
        debug!(
            "候选已存在 (fingerprint={}), 返回已有 id={}",
            fingerprint, existing_id
        );
        Ok(existing_id)
    } else {
        Ok(conn.last_insert_rowid())
    }
}

/// 计算候选指纹 — SHA256(artifact_id|candidate_type|target_slug|target_field|proposed_payload)
///
/// 使用确定性字段组成指纹，不含 confidence/risk_level 等可能因 KB 重新解析而变化的字段。
/// 同一 artifact 的同一内容无论重试多少次，指纹始终相同。
fn compute_candidate_fingerprint(
    artifact_id: i64,
    candidate_type: &CandidateType,
    target_slug: &str,
    target_field: &str,
    proposed_payload: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let input = format!(
        "{}|{}|{}|{}|{}",
        artifact_id, candidate_type, target_slug, target_field, proposed_payload
    );
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 列出候选变更
pub fn list_candidates(
    conn: &Connection,
    status: Option<&str>,
    candidate_type: Option<&str>,
    target_slug: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<PromotionCandidate>> {
    let mut sql = String::from(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                candidate_type, target_slug, target_field,
                title, proposed_payload, evidence_json,
                confidence, risk_level, status, reviewer, review_notes, applied_at,
                candidate_fingerprint
         FROM promotion_candidates WHERE 1=1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut param_idx = 1;

    if let Some(s) = status {
        sql.push_str(&format!(" AND status = ?{}", param_idx));
        param_values.push(Box::new(s.to_string()));
        param_idx += 1;
    }
    if let Some(t) = candidate_type {
        sql.push_str(&format!(" AND candidate_type = ?{}", param_idx));
        param_values.push(Box::new(t.to_string()));
        param_idx += 1;
    }
    if let Some(s) = target_slug {
        sql.push_str(&format!(" AND target_slug = ?{}", param_idx));
        param_values.push(Box::new(s.to_string()));
        param_idx += 1;
    }

    sql.push_str(&format!(
        " ORDER BY created_at DESC LIMIT ?{} OFFSET ?{}",
        param_idx,
        param_idx + 1
    ));
    param_values.push(Box::new(limit));
    param_values.push(Box::new(offset));

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| GBrainError::Database(format!("准备查询失败: {}", e)))?;

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        row_to_promotion_candidate(row)
    })?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| GBrainError::Database(format!("映射行失败: {}", e)))?);
    }
    Ok(result)
}

/// 按 ID 查找候选
pub fn find_candidate_by_id(conn: &Connection, id: i64) -> Result<Option<PromotionCandidate>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                candidate_type, target_slug, target_field,
                title, proposed_payload, evidence_json,
                confidence, risk_level, status, reviewer, review_notes, applied_at,
                candidate_fingerprint
         FROM promotion_candidates WHERE id = ?1",
        )
        .map_err(|e| GBrainError::Database(format!("准备查询失败: {}", e)))?;

    let mut rows = stmt
        .query(params![id])
        .map_err(|e| GBrainError::Database(format!("查询候选失败: {}", e)))?;

    match rows
        .next()
        .map_err(|e| GBrainError::Database(format!("获取候选行失败: {}", e)))?
    {
        Some(row) => {
            Ok(Some(row_to_promotion_candidate(row).map_err(|e| {
                GBrainError::Database(format!("映射行失败: {}", e))
            })?))
        }
        None => Ok(None),
    }
}

/// 审核候选（approve / reject）
pub fn review_candidate(
    conn: &Connection,
    input: &ReviewCandidateInput,
) -> Result<PromotionCandidate> {
    let mut candidate = find_candidate_by_id(conn, input.candidate_id)?
        .ok_or_else(|| GBrainError::PageNotFound(format!("候选 {} 不存在", input.candidate_id)))?;

    if candidate.status != "pending" {
        return Err(GBrainError::InvalidInput(format!(
            "候选 {} 状态为 {}，无法审核",
            input.candidate_id, candidate.status
        )));
    }

    let new_status = match input.action.as_str() {
        "accept" => "accepted",
        "reject" => "rejected",
        _ => {
            return Err(GBrainError::InvalidInput(format!(
                "无效的审核动作: {}",
                input.action
            )))
        }
    };

    let now = now_str();
    conn.execute(
        "UPDATE promotion_candidates SET status = ?1, reviewer = ?2, review_notes = ?3, updated_at = ?4 WHERE id = ?5",
        params![new_status, input.reviewer, input.notes.as_deref().unwrap_or(""), now, input.candidate_id],
    ).map_err(|e| GBrainError::Database(format!("更新候选状态失败: {}", e)))?;

    candidate.status = new_status.to_string();
    candidate.reviewer = input.reviewer.clone();
    candidate.review_notes = input.notes.clone().unwrap_or_default();

    info!(
        "候选 {} 已被 {} 为 {}",
        input.candidate_id, input.action, new_status
    );

    Ok(candidate)
}

/// 在命名 savepoint 内执行操作，失败时自动回滚到 savepoint。
/// 用于隔离单个 candidate 的所有写入操作，避免部分写入导致数据不一致。
fn with_savepoint<T>(
    conn: &Connection,
    name: &str,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    conn.execute(&format!("SAVEPOINT {}", name), [])?;
    match f(conn) {
        Ok(result) => {
            conn.execute(&format!("RELEASE {}", name), [])?;
            Ok(result)
        }
        Err(e) => {
            // 回滚到 savepoint 并释放，记录错误但不掩盖原始错误
            let _ = conn.execute(&format!("ROLLBACK TO {}", name), []);
            let _ = conn.execute(&format!("RELEASE {}", name), []);
            Err(e)
        }
    }
}

/// 应用已审核的候选变更
///
/// 将候选写入 gbrain page，并记录 provenance。
pub fn apply_candidate(conn: &Connection, candidate_id: i64) -> Result<PromotionCandidate> {
    let mut candidate = find_candidate_by_id(conn, candidate_id)?
        .ok_or_else(|| GBrainError::PageNotFound(format!("候选 {} 不存在", candidate_id)))?;

    if candidate.status != "accepted" {
        return Err(GBrainError::InvalidInput(format!(
            "候选 {} 状态为 {}，必须先 accept 才能应用",
            candidate_id, candidate.status
        )));
    }

    // 用 SQLite savepoint 隔离每个 candidate 的所有写入，失败时自动回滚
    let sp_name = format!("sp_apply_{}", candidate_id);
    with_savepoint(conn, &sp_name, |conn| {
        apply_candidate_inner(conn, &mut candidate)?;
        Ok(candidate)
    })
}

/// apply_candidate 的内部实现，在 savepoint 保护下执行
fn apply_candidate_inner(conn: &Connection, candidate: &mut PromotionCandidate) -> Result<()> {
    // 修复：applied_at 必须在所有页面修改完成后才生成，
    // 否则 rollback 查 snapshot_at < applied_at 时会找不到修改前快照。
    // 之前 now 在修改前生成，但 page_versions 快照的 snapshot_at 用的是
    // 修改时的 datetime('now')，比 applied_at 更晚，rollback 查不到。
    // 现在先执行修改，再生成 applied_at，确保 snapshot_at < applied_at 成立。

    // 解析 candidate_type，只解析一次，后续 match 和 snapshot 共用
    let candidate_type: CandidateType = candidate.candidate_type.parse().map_err(|e| {
        GBrainError::Database(format!(
            "无效的 candidate_type '{}': {}",
            candidate.candidate_type, e
        ))
    })?;
    match candidate_type {
        CandidateType::DocumentSummary => {
            apply_summary_candidate(conn, candidate)?;
        }
        CandidateType::EntityMention => {
            apply_entity_candidate(conn, candidate)?;
        }
        CandidateType::LinkSuggestion => {
            apply_link_candidate(conn, candidate)?;
        }
        CandidateType::TimelineEvent => {
            apply_timeline_candidate(conn, candidate)?;
        }
        CandidateType::FactClaim => {
            apply_fact_claim_candidate(conn, candidate)?;
        }
        CandidateType::PageCreate => {
            apply_page_create_candidate(conn, candidate)?;
        }
        CandidateType::PageUpdate => {
            apply_page_update_candidate(conn, candidate)?;
        }
    }

    // 写入 provenance
    provenance::record_provenance_from_candidate(conn, candidate)?;

    // 修复：记录本次 apply 前创建的 page_versions.id 到 review_notes，
    // rollback 时按 version id 精确恢复，避免批量 apply 同秒多候选时
    // rollback 拿到同页其它候选的快照
    // 注意: candidate_type 已在函数顶部解析并校验，此处直接复用
    let snapshot_version_id: Option<i64> = conn
        .query_row(
            "SELECT MAX(id) FROM page_versions WHERE page_id = (SELECT id FROM pages WHERE slug = ?1)",
            params![&candidate.target_slug],
            |row| row.get(0),
        )
        .map_err(|e| GBrainError::Database(format!("查询页面快照失败: {}", e)))?;

    if candidate_type != CandidateType::PageCreate && snapshot_version_id.is_none() {
        return Err(GBrainError::Database(format!(
            "候选 {} 应用后未记录 page_versions 快照，拒绝标记为 applied",
            candidate.id
        )));
    }

    // 修复：applied_at 在所有修改完成后生成，确保晚于 page_versions 快照的 snapshot_at
    let applied_at = now_str();

    // 更新候选状态，同时记录 snapshot_version_id 到 review_notes
    // 修复：将 snapshot_version_id 附加到现有 review_notes 后面而非覆盖，
    // 保留审核备注信息（如"自动审核: 低风险高置信度"或"批量审核应用"）。
    // 格式: {原有审核备注}\nsnapshot_version_id:{id}
    // rollback 时按行解析首行提取 snapshot_version_id
    let review_notes = match snapshot_version_id {
        Some(vid) => {
            if candidate.review_notes.is_empty() {
                format!("snapshot_version_id:{}", vid)
            } else {
                format!("{}\nsnapshot_version_id:{}", candidate.review_notes, vid)
            }
        }
        None => candidate.review_notes.clone(),
    };
    conn.execute(
        "UPDATE promotion_candidates SET status = 'applied', applied_at = ?1, review_notes = ?2, updated_at = ?1 WHERE id = ?3",
        params![applied_at, review_notes, candidate.id],
    ).map_err(|e| GBrainError::Database(format!("更新候选状态失败: {}", e)))?;

    candidate.status = "applied".to_string();
    candidate.applied_at = Some(applied_at);
    candidate.review_notes = review_notes;

    info!("候选 {} 已应用", candidate.id);

    // 记录 promotion_applied 事件（§7.6）
    if let Err(e) = store::record_event(
        conn,
        Some(candidate.artifact_id),
        candidate.occurrence_id,
        "promotion_applied",
        "promotion",
        &serde_json::json!({"candidate_id": candidate.id, "type": candidate.candidate_type})
            .to_string(),
    ) {
        warn!("记录 promotion_applied 事件失败: {}", e);
    }

    Ok(())
}

/// 回滚已应用的候选变更（§31 rollback_candidate）
///
/// 撤销已应用的提升：
/// 1. 将候选状态改为 rolled_back
/// 2. 标记相关 provenance 为 stale（reason: rollback）
/// 3. 尝试恢复影子页面到应用前的版本（使用 pages_version_history）
pub fn rollback_candidate(conn: &Connection, candidate_id: i64) -> Result<PromotionCandidate> {
    let mut candidate = find_candidate_by_id(conn, candidate_id)?
        .ok_or_else(|| GBrainError::PageNotFound(format!("候选 {} 不存在", candidate_id)))?;

    if candidate.status != "applied" {
        return Err(GBrainError::InvalidInput(format!(
            "候选 {} 状态为 {}，只有 applied 状态的候选可以回滚",
            candidate_id, candidate.status
        )));
    }

    // P1 修复：校验当前 candidate 是否为该 slug 最新的 applied candidate。
    // 同一 slug 多次 apply 后，较早 candidate 的投影已被 superseded，
    // 回滚较早 candidate 会导致页面恢复旧内容但 active 投影仍指向新版本。
    let latest_applied_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM promotion_candidates
             WHERE target_slug = ?1 AND status = 'applied'
             ORDER BY updated_at DESC, id DESC
             LIMIT 1",
            params![candidate.target_slug],
            |row| row.get(0),
        )
        .ok();
    if let Some(latest_id) = latest_applied_id {
        if latest_id != candidate.id {
            return Err(GBrainError::InvalidInput(format!(
                "候选 {} 不是 slug '{}' 的最新 applied 候选（最新为 {}），请先回滚更新的变更",
                candidate.id, candidate.target_slug, latest_id
            )));
        }
    }

    // 用 savepoint 保护整个回滚流程，失败时自动回滚
    let sp_name = format!("sp_rollback_{}", candidate_id);
    with_savepoint(conn, &sp_name, |conn| {
        rollback_candidate_inner(conn, &mut candidate)?;
        Ok(candidate)
    })
}

/// rollback_candidate 的内部实现，在 savepoint 保护下执行
fn rollback_candidate_inner(conn: &Connection, candidate: &mut PromotionCandidate) -> Result<()> {
    let now = now_str();

    // 1. 标记相关 provenance 为 stale
    provenance::mark_provenance_stale_by_candidate(
        conn,
        candidate.id,
        &format!("rollback: 候选 {} 回滚", candidate.id),
    )?;

    let candidate_type = candidate
        .candidate_type
        .parse()
        .unwrap_or(CandidateType::FactClaim);

    // 2. 按候选类型执行回滚
    if candidate_type == CandidateType::PageCreate {
        // page_create 回滚：删除候选创建的新页面。
        // apply_page_create_candidate 直接 INSERT INTO pages 不存在页面，
        // 无 page_versions 历史可恢复，必须直接处理页面生命周期。
        rollback_page_create(conn, candidate)?;
    } else {
        // 尝试恢复影子页面到应用前的版本
        rollback_shadow_page_update(conn, candidate)?;

        // P1-12 修复：rollback 时同步处理 apply 创建的 brain_page_update 投影。
        // 仅 PageUpdate 类型会创建 brain_page_update 投影（见 apply_page_update_candidate），
        // 其它类型不创建该投影，无条件调用会导致找不到投影返回错误。
        if candidate_type == CandidateType::PageUpdate {
            rollback_page_update_projections(conn, candidate)?;
        }
    }

    // 3. 更新候选状态为 rolled_back
    // 修复：将回滚信息附加到 review_notes 而非覆盖，
    // 保留 snapshot_version_id 信息供后续精确恢复使用。
    // 格式: snapshot_version_id:{id}\n回滚于 {now}
    // 或（无 version_id 时）: 回滚于 {now}
    let new_review_notes = if candidate.review_notes.is_empty() {
        format!("回滚于 {}", now)
    } else {
        format!("{}\n回滚于 {}", candidate.review_notes, now)
    };
    conn.execute(
        "UPDATE promotion_candidates SET status = 'rolled_back', review_notes = ?1, updated_at = ?2 WHERE id = ?3",
        params![new_review_notes, now, candidate.id],
    ).map_err(|e| GBrainError::Database(format!("更新候选回滚状态失败: {}", e)))?;

    candidate.status = "rolled_back".to_string();
    candidate.review_notes = new_review_notes;

    info!("候选 {} 已回滚", candidate.id);

    // 记录 promotion_rolled_back 事件（§7.6）
    if let Err(e) = store::record_event(
        conn,
        Some(candidate.artifact_id),
        candidate.occurrence_id,
        "promotion_rolled_back",
        "rollback",
        &serde_json::json!({"candidate_id": candidate.id, "type": candidate.candidate_type})
            .to_string(),
    ) {
        warn!("记录 promotion_rolled_back 事件失败: {}", e);
    }

    Ok(())
}

/// 回滚页面创建候选 — 软删除候选新建的页面
///
/// apply_page_create_candidate 会 INSERT INTO pages 创建一个全新页面，
/// 没有 page_versions 历史可恢复。因此 page_create 回滚必须直接处理页面生命周期。
///
/// 安全约束：
/// - 通过比较页面当前各字段（title/content/hash/page_type/timeline/frontmatter）
///   与候选创建时的原始值来判断页面是否被修改。
/// - 所有字段匹配时，直接软删除页面。
/// - 任一字段不匹配时，拒绝回滚，提示人工审查。
fn rollback_page_create(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let slug = &candidate.target_slug;

    // P1修复：查询页面完整字段（含 page_type/timeline/frontmatter），用于内容比较替代时间戳比较
    let page_info = conn.query_row(
        "SELECT title, compiled_truth, content_hash, page_type, timeline, frontmatter, deleted_at \
         FROM pages WHERE slug = ?1",
        rusqlite::params![slug],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, Option<String>>(6)?,
            ))
        },
    );

    match page_info {
        Ok((_title, _content, _hash, _pt, _tl, _fm, Some(_deleted_at))) => {
            // 页面已被删除，与 rollback 目标一致，无需操作
            info!("页面 {} 已被删除，跳过 page_create 回滚的页面清理", slug);
        }
        Ok((
            current_title,
            current_content,
            current_hash,
            current_page_type,
            current_timeline,
            current_frontmatter,
            None,
        )) => {
            // P1修复：不再依赖秒级时间戳，比较当前页面各字段与候选创建时的内容
            // 包含 title/content/hash/page_type/timeline/frontmatter，防止仅 metadata 被修改时误删
            let payload: serde_json::Value =
                serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));
            // P2修复：title fallback 与 apply_page_create_candidate 保持一致
            let orig_title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or(&candidate.title);
            let orig_content = payload
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // page_create 初始化时的默认值：page_type='note', timeline='', frontmatter=''
            let orig_page_type = payload
                .get("page_type")
                .and_then(|v| v.as_str())
                .unwrap_or("note");
            let orig_timeline = payload
                .get("timeline")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let orig_frontmatter = payload
                .get("frontmatter")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // 修复：orig_hash 需与 apply_page_create_candidate 的 content_hash 算法一致，
            // 包含 title + page_type + content + timeline + frontmatter（canonical JSON）+ tags。
            // 上一轮改 apply 的 hash 后此处未同步更新，导致刚创建未修改的页面也 hash 不一致、无法回滚。
            let orig_frontmatter_value: serde_json::Value =
                serde_json::from_str(orig_frontmatter).unwrap_or(serde_json::json!({}));
            let orig_tags: Vec<String> = orig_frontmatter_value
                .as_object()
                .and_then(|obj| obj.get("tags"))
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            let orig_hash = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(orig_title.as_bytes());
                hasher.update(orig_page_type.as_bytes());
                hasher.update(orig_content.as_bytes());
                hasher.update(orig_timeline.as_bytes());
                let fm_str = serialize_canonical_json(&orig_frontmatter_value);
                hasher.update(fm_str.as_bytes());
                for tag in &orig_tags {
                    hasher.update(tag.as_bytes());
                }
                format!("{:x}", hasher.finalize())
            };

            if current_title != orig_title
                || current_content != orig_content
                || current_hash.as_deref() != Some(&orig_hash)
                || current_page_type != orig_page_type
                || current_timeline != orig_timeline
                || current_frontmatter != orig_frontmatter
            {
                return Err(GBrainError::InvalidInput(format!(
                    "页面 {} 自创建后被修改，无法安全回滚 page_create 候选 {}。请人工审查后手动处理。",
                    slug, candidate.id
                )));
            }
            // 软删除页面（与 SqliteEngine::delete_page 保持一致）
            let now = now_str();
            conn.execute(
                "UPDATE pages SET deleted_at = ?1, updated_at = ?1 WHERE slug = ?2",
                rusqlite::params![now, slug],
            )
            .map_err(|e| {
                GBrainError::Database(format!("回滚 page_create 软删除页面 {} 失败: {}", slug, e))
            })?;
            // 清理 slug 关联的链接和文件引用
            conn.execute(
                "DELETE FROM links WHERE from_slug = ?1 OR to_slug = ?1",
                rusqlite::params![slug],
            )
            .map_err(|e| GBrainError::Database(format!("清理页面链接失败: {}", e)))?;
            conn.execute(
                "DELETE FROM files WHERE page_slug = ?1",
                rusqlite::params![slug],
            )
            .map_err(|e| GBrainError::Database(format!("清理页面文件引用失败: {}", e)))?;
            info!("page_create 回滚：已软删除页面 {}", slug);
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            // 页面不存在，可能是已被其他途径删除
            info!("页面 {} 不存在，跳过 page_create 回滚的页面删除", slug);
        }
        Err(e) => {
            return Err(GBrainError::Database(format!(
                "回滚 page_create 查询页面 {} 状态失败: {}",
                slug, e
            )));
        }
    }

    Ok(())
}

/// 回滚影子页面更新 — 尝试恢复到应用前的版本
fn rollback_shadow_page_update(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let slug = &candidate.target_slug;

    // 按 snapshot_version_id 精确恢复，
    // 避免批量 apply 同秒多候选时 rollback 拿到同页其它候选的快照。
    // snapshot_version_id 存储在 review_notes 的最后一行，格式: snapshot_version_id:{id}
    // 按行解析取最后一行
    let snapshot_version_id: i64 = candidate
        .review_notes
        .lines()
        .last()
        .and_then(|line| line.strip_prefix("snapshot_version_id:"))
        .and_then(|s| s.parse::<i64>().ok())
        .ok_or_else(|| {
            GBrainError::Database(format!(
                "候选 {} 缺少 snapshot_version_id，无法安全回滚",
                candidate.id
            ))
        })?;

    let prev_content: String = conn
        .query_row(
            "SELECT compiled_truth FROM page_versions WHERE id = ?1",
            rusqlite::params![snapshot_version_id],
            |row| row.get(0),
        )
        .map_err(|e| GBrainError::Database(format!("查询页面快照失败: {}", e)))?;

    let now = now_str();
    conn.execute(
        "UPDATE pages SET compiled_truth = ?1, updated_at = ?2 WHERE slug = ?3",
        rusqlite::params![prev_content, now, slug],
    )
    .map_err(|e| GBrainError::Database(format!("恢复影子页面失败: {}", e)))?;
    rebuild_chunks_for_page(conn, slug)?;
    sync_page_tokens_and_hash(conn, slug)?;
    info!("影子页面 {} 已恢复到应用前版本", slug);

    Ok(())
}

/// P1-12 修复：rollback 时同步处理 apply 创建的 brain_page_update 投影。
///
/// apply 时会：(1) 创建 active brain_page_update 投影 (metadata_json 含 candidate_id)
///            (2) 将同 slug 旧 active 投影标记为 superseded (superseded_by = 新投影 id)
/// rollback 必须反向操作：
///   - 将 apply 创建的投影标记为 stale (stale_reason = 'rolled_back')
///   - 将被 superseded 的旧投影恢复为 active
///   - 将旧投影的 version_hash 更新为当前页面的 content_hash
///     （因为 rollback 已恢复页面内容，旧投影的 hash 需与页面一致，
///     否则后续冲突检测基线错误）
///
/// 否则已回滚内容仍被 active 投影引用，后续冲突检测基线错误。
fn rollback_page_update_projections(
    conn: &Connection,
    candidate: &PromotionCandidate,
) -> Result<()> {
    let slug = &candidate.target_slug;
    let proj_ref = format!("brain_page:{}", slug);

    // 1. 查找 apply 时创建的 brain_page_update 投影（metadata_json 含 candidate_id）
    // P2 修复：使用 Rust 侧 serde_json 精确比较 candidate_id，替代 LIKE 模糊匹配。
    // LIKE '%"candidate_id": 12%' 会误匹配 {"candidate_id": 123}，导致错误地
    // stale 另一个 candidate 的投影。
    let applied_proj_id: Option<i64> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, metadata_json FROM artifact_projections
                 WHERE projection_type = 'brain_page_update'
                   AND projection_ref = ?1
                   AND status = 'active'",
            )
            .map_err(|e| GBrainError::Database(format!("查询 active 投影失败: {}", e)))?;
        let rows: Vec<(i64, String)> = stmt
            .query_map(rusqlite::params![proj_ref], |row| {
                Ok((row.get(0)?, row.get::<_, String>(1).unwrap_or_default()))
            })
            .map_err(|e| GBrainError::Database(format!("遍历投影行失败: {}", e)))?
            .filter_map(|r| r.ok())
            .collect();
        rows.iter().find_map(|(id, meta_json)| {
            serde_json::from_str::<serde_json::Value>(meta_json)
                .ok()
                .and_then(|v| v.get("candidate_id")?.as_i64())
                .and_then(|cid| if cid == candidate.id { Some(*id) } else { None })
        })
    };

    if let Some(proj_id) = applied_proj_id {
        // 2. 将 apply 创建的投影标记为 stale
        let now = now_str();
        conn.execute(
            "UPDATE artifact_projections SET status = 'stale', stale_reason = 'rolled_back', updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, proj_id],
        ).map_err(|e| GBrainError::Database(format!("标记回滚投影 stale 失败: {}", e)))?;

        // 3. 获取当前页面的 content_hash，用于更新旧投影的 version_hash
        let current_page_hash: Option<String> = conn
            .query_row(
                "SELECT content_hash FROM pages WHERE slug = ?1",
                rusqlite::params![slug],
                |row| row.get(0),
            )
            .ok();

        // 4. 查找被该投影 superseded 的旧投影，恢复为 active
        // 这些旧投影在 apply 时被标记为 superseded_by = proj_id
        // 同时更新 version_hash 为当前页面的 content_hash
        let restored_count = if let Some(ref page_hash) = current_page_hash {
            conn.execute(
                "UPDATE artifact_projections SET status = 'active', stale_reason = '', superseded_by = NULL, version_hash = ?1, updated_at = ?2 WHERE superseded_by = ?3 AND projection_type = 'brain_page_update' AND projection_ref = ?4",
                rusqlite::params![page_hash, now, proj_id, proj_ref],
            ).map_err(|e| GBrainError::Database(format!("恢复旧投影 active 失败: {}", e)))?
        } else {
            conn.execute(
                "UPDATE artifact_projections SET status = 'active', stale_reason = '', superseded_by = NULL, updated_at = ?1 WHERE superseded_by = ?2 AND projection_type = 'brain_page_update' AND projection_ref = ?3",
                rusqlite::params![now, proj_id, proj_ref],
            ).map_err(|e| GBrainError::Database(format!("恢复旧投影 active 失败: {}", e)))?
        };

        info!(
            "候选 {} 回滚投影处理: 停用投影 id={}, 恢复旧投影数={}, 页面hash={:?}",
            candidate.id, proj_id, restored_count, current_page_hash
        );
    } else {
        // P1 修复：找不到 apply 创建的 active 投影时返回错误，
        // 避免"页面已恢复但投影未同步"的半逻辑成功。
        // 若候选状态为 applied 但无对应 active 投影，
        // 说明数据已不一致，不应继续标记 candidate 为 rolled_back。
        error!(
            "候选 {} 回滚时未找到对应 active brain_page_update 投影 (slug={})",
            candidate.id, slug
        );
        return Err(GBrainError::Database(format!(
            "回滚候选 {} 失败：未找到对应的 active brain_page_update 投影 (slug={})",
            candidate.id, slug
        )));
    }

    Ok(())
}

/// 同步更新页面的 compiled_truth_tokens 和 content_hash
///
/// 在 compiled_truth 变更后调用，确保中文分词索引和内容变更检测与实际内容一致。
/// apply 和 rollback 路径都应走此函数，避免 tokens/hash 与内容脱节。
fn sync_page_tokens_and_hash(conn: &Connection, slug: &str) -> Result<()> {
    let new_truth: String = conn
        .query_row(
            "SELECT compiled_truth FROM pages WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .map_err(|e| GBrainError::Database(format!("读取页面内容失败: {}", e)))?;
    let truth_tokens = crate::nlp::chinese::tokenize_content(&new_truth);
    let content_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(new_truth.as_bytes());
        format!("{:x}", hasher.finalize())
    };
    conn.execute(
        "UPDATE pages SET compiled_truth_tokens = ?1, content_hash = ?2 WHERE slug = ?3",
        params![truth_tokens, content_hash, slug],
    )
    .map_err(|e| GBrainError::Database(format!("更新页面 tokens/hash 失败: {}", e)))?;
    Ok(())
}

/// 自动应用低风险候选（promotion_policy = auto 时）
///
/// 修复：增加可选 kb_document_id 和 occurrence_id 参数，限定只自动应用本次提取产生的候选。
/// 同一 artifact 多次上传且 policy 不同时，按 artifact_id 过滤会把
/// 之前 candidate 策略产生的低风险候选也自动应用。
/// occurrence_id 进一步限定只应用本次 occurrence 产生的候选，避免旧候选被后续重复上传自动提升。
pub fn auto_apply_candidates(
    conn: &Connection,
    artifact_id: i64,
    kb_document_id: Option<i64>,
    occurrence_id: Option<i64>,
) -> Result<Vec<i64>> {
    auto_apply_candidates_scoped(conn, artifact_id, kb_document_id, occurrence_id, false)
}

/// 自动应用本次上传产生的所有候选（promotion_policy = auto_apply 时）
pub fn auto_apply_all_candidates(
    conn: &Connection,
    artifact_id: i64,
    kb_document_id: Option<i64>,
    occurrence_id: Option<i64>,
) -> Result<Vec<i64>> {
    auto_apply_candidates_scoped(conn, artifact_id, kb_document_id, occurrence_id, true)
}

fn auto_apply_candidates_scoped(
    conn: &Connection,
    artifact_id: i64,
    kb_document_id: Option<i64>,
    occurrence_id: Option<i64>,
    apply_all: bool,
) -> Result<Vec<i64>> {
    let mut applied = Vec::new();
    // 修复：收集所有候选级失败，有失败时返回 Err，
    // 让 worker 知道存在未完成的 auto-apply，不会 complete job 导致候选永久停在 pending。
    // 之前 SAVEPOINT 创建失败、review_candidate 失败、apply_candidate_inner 失败、
    // RELEASE 失败都被 rollback/跳过后继续，函数最终返回 Ok(applied)，
    // worker 认为 auto-apply 成功并 complete job，低风险候选永久停在 pending 不会重试。
    let mut failures: Vec<String> = Vec::new();

    loop {
        let candidates = list_auto_apply_candidates(
            conn,
            artifact_id,
            kb_document_id,
            occurrence_id,
            apply_all,
            1000,
        )?;
        if candidates.is_empty() {
            break;
        }

        for candidate in candidates {
            let sp_name = format!("sp_auto_apply_{}", candidate.id);
            let result = with_savepoint(conn, &sp_name, |conn| {
                // 外层 savepoint 覆盖整个 accept+apply 流程，失败时候选恢复为 pending 可重试。
                // 直接调用 apply_candidate_inner 避免嵌套 apply_candidate savepoint。
                let review_input = ReviewCandidateInput {
                    candidate_id: candidate.id,
                    action: "accept".to_string(),
                    reviewer: if apply_all { "auto_apply" } else { "auto" }.to_string(),
                    notes: Some(
                        if apply_all {
                            "自动审核: 全量自动应用"
                        } else {
                            "自动审核: 低风险高置信度"
                        }
                        .to_string(),
                    ),
                };
                let mut candidate_for_apply = review_candidate(conn, &review_input)?;
                apply_candidate_inner(conn, &mut candidate_for_apply)?;
                Ok(())
            });

            match result {
                Ok(()) => applied.push(candidate.id),
                Err(e) => {
                    debug!("自动应用候选 {} 失败: {}, savepoint 回滚", candidate.id, e);
                    failures.push(format!("候选 {} auto-apply 失败: {}", candidate.id, e));
                }
            }
        }

        if !failures.is_empty() {
            break;
        }
    }

    if !applied.is_empty() {
        if apply_all {
            info!("自动应用了 {} 个候选", applied.len());
        } else {
            info!("自动应用了 {} 个低风险候选", applied.len());
        }
    }

    // 修复：有候选级失败时返回 Err，让 worker 知道存在未完成的 auto-apply，
    // 不会 complete job 导致失败候选永久停在 pending 不会重试。
    // 部分成功时也返回 Err，因为失败候选需要重试。
    if !failures.is_empty() {
        warn!(
            "自动应用存在 {} 个失败: {}",
            failures.len(),
            failures.join("; ")
        );
        return Err(GBrainError::Database(format!(
            "自动应用候选部分失败 (policy={}, 成功={}, 失败={}): {}",
            if apply_all {
                "auto_apply"
            } else {
                "auto_accept_low_risk"
            },
            applied.len(),
            failures.len(),
            failures.join("; ")
        )));
    }

    Ok(applied)
}

fn list_auto_apply_candidates(
    conn: &Connection,
    artifact_id: i64,
    kb_document_id: Option<i64>,
    occurrence_id: Option<i64>,
    apply_all: bool,
    limit: i64,
) -> Result<Vec<PromotionCandidate>> {
    let mut sql = String::from(
        "SELECT id, created_at, updated_at,
                artifact_id, occurrence_id, kb_document_id, kb_node_id,
                candidate_type, target_slug, target_field,
                title, proposed_payload, evidence_json,
                confidence, risk_level, status, reviewer, review_notes, applied_at,
                candidate_fingerprint
         FROM promotion_candidates
         WHERE status = 'pending' AND artifact_id = ?1",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(artifact_id)];
    let mut param_idx = 2;

    if let Some(kb_doc_id) = kb_document_id {
        sql.push_str(&format!(" AND kb_document_id = ?{}", param_idx));
        param_values.push(Box::new(kb_doc_id));
        param_idx += 1;
    }

    if let Some(occ_id) = occurrence_id {
        sql.push_str(&format!(" AND occurrence_id = ?{}", param_idx));
        param_values.push(Box::new(occ_id));
        param_idx += 1;
    }

    if !apply_all {
        sql.push_str(" AND risk_level = 'low' AND confidence >= 0.8");
    }

    sql.push_str(&format!(
        " ORDER BY created_at ASC, id ASC LIMIT ?{}",
        param_idx
    ));
    param_values.push(Box::new(limit));

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| GBrainError::Database(format!("准备自动应用候选查询失败: {}", e)))?;
    let rows = stmt
        .query_map(params_refs.as_slice(), row_to_promotion_candidate)
        .map_err(|e| GBrainError::Database(format!("查询自动应用候选失败: {}", e)))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| GBrainError::Database(format!("映射候选失败: {}", e)))?);
    }
    Ok(result)
}

/// 统计待审核候选数
pub fn count_pending_candidates(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM promotion_candidates WHERE status = 'pending'",
        [],
        |row| row.get(0),
    )
    .map_err(|e| GBrainError::Database(format!("统计候选失败: {}", e)))
}

/// 批量应用候选（§10.5 promotion_apply_all）
///
/// 根据 artifact_id 和 risk 筛选条件，批量接受并应用所有匹配的 pending 候选。
/// 返回成功应用的数量和失败列表。
pub fn batch_apply_candidates(
    conn: &Connection,
    artifact_id: Option<i64>,
    risk_filter: Option<&str>,
    dry_run: bool,
) -> Result<BatchApplyResult> {
    // 查找所有 pending 候选
    let mut sql = String::from(
        "SELECT id, artifact_id, candidate_type, risk_level
         FROM promotion_candidates
         WHERE status = 'pending'",
    );
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    let mut param_idx = 1;

    if let Some(aid) = artifact_id {
        sql.push_str(&format!(" AND artifact_id = ?{}", param_idx));
        param_values.push(Box::new(aid));
        param_idx += 1;
    }

    if let Some(risk) = risk_filter {
        sql.push_str(&format!(" AND risk_level = ?{}", param_idx));
        param_values.push(Box::new(risk.to_string()));
    }

    sql.push_str(" ORDER BY created_at ASC");

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| GBrainError::Database(format!("准备批量查询失败: {}", e)))?;

    let rows = stmt
        .query_map(params_refs.as_slice(), |row| {
            Ok(BatchCandidateRow {
                id: row.get(0)?,
                artifact_id: row.get(1)?,
                candidate_type: row.get(2)?,
                risk_level: row.get(3)?,
            })
        })
        .map_err(|e| GBrainError::Database(format!("批量查询失败: {}", e)))?;

    let candidates: Vec<BatchCandidateRow> = rows.filter_map(|r| r.ok()).collect();

    let total = candidates.len();
    if dry_run {
        return Ok(BatchApplyResult {
            total_candidates: total,
            applied: 0,
            failed: 0,
            failures: Vec::new(),
            dry_run: true,
            candidates: candidates
                .iter()
                .map(|c| {
                    format!(
                        "id={} type={} risk={} artifact_id={}",
                        c.id, c.candidate_type, c.risk_level, c.artifact_id
                    )
                })
                .collect(),
        });
    }

    let mut applied = 0;
    let mut failed = 0;
    let mut failures = Vec::new();

    for candidate in &candidates {
        let sp_name = format!("sp_batch_apply_{}", candidate.id);
        let result = with_savepoint(conn, &sp_name, |conn| {
            // savepoint 确保失败时全部回滚（含 accept 状态变更），候选恢复为 pending 可重试。
            // 直接调用 apply_candidate_inner 避免嵌套 apply_candidate savepoint。
            let review_input = ReviewCandidateInput {
                candidate_id: candidate.id,
                action: "accept".to_string(),
                reviewer: "batch_apply".to_string(),
                notes: Some("批量审核应用".to_string()),
            };
            let mut candidate_for_apply = review_candidate(conn, &review_input)?;
            apply_candidate_inner(conn, &mut candidate_for_apply)?;
            Ok(())
        });

        match result {
            Ok(()) => {
                applied += 1;
                info!(
                    "批量应用候选成功: id={} type={}",
                    candidate.id, candidate.candidate_type
                );
            }
            Err(e) => {
                debug!("批量应用候选 {} 失败: {}, savepoint 回滚", candidate.id, e);
                failed += 1;
                failures.push(format!(
                    "候选 id={} ({}) batch apply 失败: {}",
                    candidate.id, candidate.candidate_type, e
                ));
            }
        }
    }

    info!(
        "批量应用完成: 总计={}, 成功={}, 失败={}",
        total, applied, failed
    );

    Ok(BatchApplyResult {
        total_candidates: total,
        applied,
        failed,
        failures,
        dry_run: false,
        candidates: Vec::new(),
    })
}

/// 批量查询候选行
struct BatchCandidateRow {
    id: i64,
    artifact_id: i64,
    candidate_type: String,
    risk_level: String,
}

// ============================================================================
// 内部辅助
// ============================================================================

/// 应用摘要候选 — 更新影子页面的 Summary 部分
fn apply_summary_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    if let Some(summary) = payload.get("summary").and_then(|v| v.as_str()) {
        // 修复：target_slug 已经是完整 slug（如 documents/{artifact_slug}），
        // 不需要再拼 documents/ 前缀，否则变成 documents/documents/...
        let slug = &candidate.target_slug;
        update_shadow_page_section(conn, slug, "Summary", summary)?;
    }

    Ok(())
}

/// 应用实体候选 — 更新影子页面的 Entities 部分
/// 使用 candidate:// 格式避免 wikilink 自动提取污染 link graph
fn apply_entity_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    // entity_mention payload: { entity_name, suggested_slug, relation }
    let entity_name = payload
        .get("entity_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let suggested_slug = payload
        .get("suggested_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // 使用 candidate:// 格式，避免 wikilink 自动抽链
    let entity_entry = if suggested_slug.is_empty() {
        format!("- candidate://{}", entity_name)
    } else {
        format!("- candidate://{} ({})", suggested_slug, entity_name)
    };

    // 修复：target_slug 已经是完整 slug，不需要再拼 documents/ 前缀
    let slug = &candidate.target_slug;
    update_shadow_page_section(conn, slug, "Entities", &entity_entry)?;

    Ok(())
}

/// 应用时间线候选 — 更新影子页面的 Timeline 部分
fn apply_timeline_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    let title = payload.get("title").and_then(|v| v.as_str()).unwrap_or("");
    let date = payload.get("date").and_then(|v| v.as_str()).unwrap_or("");
    let content = payload
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let timeline_entry = if date.is_empty() {
        format!("- **{}**: {}", title, content)
    } else {
        format!("- **{}** ({}): {}", title, date, content)
    };

    // 修复：target_slug 已经是完整 slug，不需要再拼 documents/ 前缀
    let slug = &candidate.target_slug;
    update_shadow_page_section(conn, slug, "Candidate Timeline", &timeline_entry)?;

    Ok(())
}

/// 应用链接建议候选 — 写入影子页面的 Links 部分
fn apply_link_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    let from = payload
        .get("from_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let to = payload
        .get("to_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let link_type = payload
        .get("link_type")
        .and_then(|v| v.as_str())
        .unwrap_or("related");

    let link_entry = format!(
        "- candidate://{} → candidate://{} ({})",
        from, to, link_type
    );
    // 修复：target_slug 已经是完整 slug，不需要再拼 documents/ 前缀
    let slug = &candidate.target_slug;
    update_shadow_page_section(conn, slug, "Suggested Links", &link_entry)?;

    Ok(())
}

/// 应用事实声明候选 — 写入目标页面的 compiled_truth
fn apply_fact_claim_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    let subject = payload
        .get("subject_slug")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let predicate = payload
        .get("predicate")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let object = payload
        .get("object_text")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let fact_entry = format!("- **{}**: {} = {}", subject, predicate, object);
    // 修复：target_slug 已经是完整 slug（如 documents/{artifact_slug}），
    // 不需要再拼 documents/ 前缀。空 target_slug 时用 subject_slug 兜底
    let slug = if candidate.target_slug.is_empty() {
        format!("documents/{}", subject)
    } else {
        candidate.target_slug.clone()
    };
    update_shadow_page_section(conn, &slug, "Fact Claims", &fact_entry)?;

    Ok(())
}

/// 应用页面创建候选 — 创建新的 gbrain 页面
fn apply_page_create_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    let title = payload
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or(&candidate.title);
    let content = payload
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // P2修复：从 payload 读取扩展字段，与 rollback_page_create 的比对逻辑保持一致
    let page_type = payload
        .get("page_type")
        .and_then(|v| v.as_str())
        .unwrap_or("note");
    let timeline = payload
        .get("timeline")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let frontmatter = payload
        .get("frontmatter")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    // 修复：补齐 title_tokens、compiled_truth_tokens、content_hash，
    // 与 put_page 保持一致，确保中文分词索引和内容变更检测正确
    let slug = &candidate.target_slug;
    let title_tokens = crate::nlp::chinese::tokenize_content(title);
    let truth_tokens = crate::nlp::chinese::tokenize_content(content);
    // P2修复：计算 timeline_tokens，确保中文时间线检索/FTS 权重正确索引
    let timeline_tokens = if timeline.is_empty() {
        String::new()
    } else {
        crate::nlp::chinese::tokenize_content(timeline)
    };
    // 修复：content_hash 包含 title + page_type + content + timeline + frontmatter + tags，
    // 与 src/operations.rs compute_content_hash 保持一致，确保冲突检测和版本基线语义正确
    let frontmatter_value: serde_json::Value =
        serde_json::from_str(frontmatter).unwrap_or(serde_json::json!({}));
    let tags: Vec<String> = frontmatter_value
        .as_object()
        .and_then(|obj| obj.get("tags"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let content_hash = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(title.as_bytes());
        // page_type 参与 hash，避免 page_type 变化但内容相同被误判为未变更
        hasher.update(page_type.as_bytes());
        hasher.update(content.as_bytes());
        hasher.update(timeline.as_bytes());
        // frontmatter 使用 sorted keys 的 canonical JSON 格式，与 operations.rs 保持一致
        let fm_str = serialize_canonical_json(&frontmatter_value);
        hasher.update(fm_str.as_bytes());
        for tag in &tags {
            hasher.update(tag.as_bytes());
        }
        format!("{:x}", hasher.finalize())
    };
    let now = now_str();
    // 修复：检查 INSERT 影响行数，页面已存在时返回错误而非静默跳过。
    // INSERT OR IGNORE 在 slug 已存在时不插入也不报错，但候选会被标记为 applied，
    // 导致"标记 applied 但实际页面未创建/更新"的不一致问题。
    // 与 update_shadow_page_section 保持一致：操作无效果时返回错误。
    let rows = conn.execute(
        "INSERT OR IGNORE INTO pages
         (slug, title, compiled_truth, page_type, title_tokens, compiled_truth_tokens, content_hash, created_at, updated_at, timeline, timeline_tokens, frontmatter)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9, ?10, ?11)",
        params![slug, title, content, page_type, title_tokens, truth_tokens, content_hash, now, timeline, timeline_tokens, frontmatter],
    ).map_err(|e| GBrainError::Database(format!("创建页面失败: {}", e)))?;

    if rows == 0 {
        // P2修复：INSERT OR IGNORE 未插入时，检查 slug 是否被软删除页面占用
        // 软删除行仍占用 UNIQUE 约束，但应允许新 page_create 候选复用恢复
        let deleted_at: Option<String> = match conn.query_row(
            "SELECT deleted_at FROM pages WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        ) {
            Ok(val) => val,
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => {
                return Err(GBrainError::Database(format!(
                    "查询页面 {} 状态失败: {}",
                    slug, e
                )));
            }
        };
        match deleted_at {
            Some(_) => {
                // P2修复：页面已被软删除，复用该行恢复为 active 页面
                // 从 payload 读取扩展字段（page_type/timeline/frontmatter），与普通创建路径保持一致
                conn.execute(
                    "UPDATE pages SET title=?1, compiled_truth=?2, page_type=?3, \
                     title_tokens=?4, compiled_truth_tokens=?5, content_hash=?6, \
                     updated_at=?7, deleted_at=NULL, \
                     frontmatter=?8, timeline=?9, timeline_tokens=?10 WHERE slug=?11",
                    params![
                        title,
                        content,
                        page_type,
                        title_tokens,
                        truth_tokens,
                        content_hash,
                        now,
                        frontmatter,
                        timeline,
                        timeline_tokens,
                        slug
                    ],
                )
                .map_err(|e| {
                    GBrainError::Database(format!("恢复软删除页面 {} 失败: {}", slug, e))
                })?;
                info!("页面创建候选已应用（恢复软删除页面）: slug={}", slug);
                return Ok(());
            }
            None => {
                return Err(GBrainError::InvalidInput(format!(
                    "页面 {} 已存在，无法创建（候选可能应使用 page_update 类型）",
                    slug
                )));
            }
        }
    }

    info!("页面创建候选已应用: slug={}", slug);
    Ok(())
}

/// 应用页面更新候选 — 更新已有 gbrain 页面，并创建 brain_page_update 投影
///
/// P2-9 修复：读取 payload.mode 字段，replace 时直接替换页面内容，
/// append 时追加 section。之前 mode 字段被忽略，实际总是追加。
fn apply_page_update_candidate(conn: &Connection, candidate: &PromotionCandidate) -> Result<()> {
    let payload: serde_json::Value =
        serde_json::from_str(&candidate.proposed_payload).unwrap_or(serde_json::json!({}));

    let field = payload
        .get("field")
        .and_then(|v| v.as_str())
        .unwrap_or(&candidate.target_field);
    let value = payload.get("value").and_then(|v| v.as_str()).unwrap_or("");
    let mode = payload
        .get("mode")
        .and_then(|v| v.as_str())
        .ok_or_else(|| GBrainError::InvalidInput("page_update payload 缺少 mode".to_string()))?;

    let slug = &candidate.target_slug;

    // P2-9 修复：根据 mode 决定更新策略
    if mode == "replace" && field == "compiled_truth" {
        // replace + compiled_truth → 直接替换页面全部内容
        // 先创建版本快照用于 rollback
        conn.execute(
            "INSERT INTO page_versions (page_id, compiled_truth, frontmatter, title, page_type)
             SELECT id, compiled_truth, frontmatter, title, page_type FROM pages WHERE slug = ?1",
            rusqlite::params![slug],
        )
        .map_err(|e| GBrainError::Database(format!("创建页面快照失败: {}", e)))?;

        conn.execute(
            "UPDATE pages SET compiled_truth = ?1, updated_at = datetime('now') WHERE slug = ?2",
            rusqlite::params![value, slug],
        )
        .map_err(|e| GBrainError::Database(format!("替换页面内容失败: {}", e)))?;

        // 同步 tokens/hash/chunks
        sync_page_tokens_and_hash(conn, slug)?;
        rebuild_chunks_for_page(conn, slug)?;
    } else {
        // append 或其它 field → 走现有追加 section 逻辑
        update_shadow_page_section(conn, slug, field, value)?;
    }

    // 创建 brain_page_update 投影记录
    let artifact_uid = super::store::find_artifact_by_id(conn, candidate.artifact_id)
        .ok()
        .flatten()
        .map(|a| a.artifact_uid)
        .unwrap_or_default();
    let fact_hash = make_fact_hash(
        slug,
        field,
        value,
        &artifact_uid,
        &candidate
            .kb_node_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
    );
    let proj_key = format!("page_update:{}:{}", slug, fact_hash);
    // P1-11 修复：projection_ref 必须使用 "brain_page:{slug}" 格式，
    // 与 upload.rs 创建投影和 store.rs baseline 查询保持一致。
    // 之前写成 "slug:{slug}"，导致 apply 后的投影无法被
    // find_latest_page_update_hash_by_slug 查到，后续冲突检测基线丢失。
    let proj_ref = format!("brain_page:{}", slug);
    let now = now_str();
    // P2-9 修复：version_hash 应为当前页面的 content_hash，
    // 而非空字符串。空 version_hash 会导致后续冲突检测基线丢失，
    // 下次同 slug artifact_put 无法检测页面是否被人工修改。
    let page_content_hash: String = conn
        .query_row(
            "SELECT content_hash FROM pages WHERE slug = ?1",
            rusqlite::params![slug],
            |row| row.get::<_, Option<String>>(0),
        )
        .ok()
        .flatten()
        .unwrap_or_default();

    let proj = ArtifactProjection {
        id: 0,
        created_at: now.clone(),
        updated_at: now,
        artifact_id: candidate.artifact_id,
        occurrence_id: candidate.occurrence_id,
        projection_type: ProjectionType::BrainPageUpdate.to_string(),
        projection_key: proj_key,
        projection_ref: proj_ref.clone(),
        status: "active".to_string(),
        version_hash: page_content_hash,
        stale_reason: String::new(),
        metadata_json: format!("{{\"candidate_id\": {}}}", candidate.id),
        superseded_by: None,
    };
    let new_proj_id = super::store::insert_projection_returning_id(conn, &proj)
        .map_err(|e| GBrainError::Database(format!("插入 brain_page_update 投影失败: {}", e)))?;

    // P2-11 修复：apply 新 brain_page_update 后，将同 slug 旧 active
    // brain_page_update 标记为 superseded，指向新投影。
    // 设计文档 §5.6 版本策略要求同 slug 不同内容时旧投影进入
    // superseded/stale，否则会出现多个 active 稳定页投影，
    // 与"同 slug 旧投影 stale/superseded"的设计语义不一致。
    let old_proj_ref = format!("brain_page:{}", slug);
    let old_active_projections: Vec<(i64, String)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, status FROM artifact_projections
             WHERE projection_type = 'brain_page_update'
               AND projection_ref = ?1
               AND status = 'active'
               AND id != ?2",
            )
            .map_err(|e| GBrainError::Database(e.to_string()))?;
        let rows = stmt
            .query_map(rusqlite::params![old_proj_ref, new_proj_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| GBrainError::Database(e.to_string()))?;
        let mut result = Vec::new();
        for row in rows {
            result.push(row.map_err(|e| GBrainError::Database(e.to_string()))?);
        }
        result
    };
    for (old_id, _old_status) in &old_active_projections {
        conn.execute(
            "UPDATE artifact_projections SET status = 'superseded', stale_reason = 'content_updated', superseded_by = ?1, updated_at = datetime('now') WHERE id = ?2",
            rusqlite::params![new_proj_id, old_id],
        ).map_err(|e| GBrainError::Database(format!("标记旧投影 superseded 失败: {}", e)))?;
    }

    info!(
        "页面更新候选已应用: slug={}, field={}, 旧投影superseded数={}",
        slug,
        field,
        old_active_projections.len()
    );
    Ok(())
}

/// 更新影子页面的某个 section
/// 返回修改前快照的 page_versions.id，用于 rollback 精确恢复
fn update_shadow_page_section(
    conn: &Connection,
    slug: &str,
    section: &str,
    content: &str,
) -> Result<Option<i64>> {
    // 查找 gbrain page
    let page_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pages WHERE slug = ?1",
            params![slug],
            |row| row.get(0),
        )
        .map_err(|e| GBrainError::Database(format!("查询页面是否存在失败: {}", e)))?;

    if page_exists {
        // 修复：在修改前创建 page_versions 快照，使 rollback 能可靠恢复
        // 之前直接 append compiled_truth，rollback 只能走脆弱的文本删除兜底
        // 修复：捕获 page_versions.id，用于 rollback 按 version id 精确恢复，
        // 避免批量 apply 同秒多候选时 rollback 拿到其它候选的快照
        conn.execute(
            "INSERT INTO page_versions (page_id, compiled_truth, frontmatter, title, page_type)
             SELECT id, compiled_truth, frontmatter, title, page_type FROM pages WHERE slug = ?1",
            params![slug],
        )
        .map_err(|e| GBrainError::Database(format!("创建页面快照失败: {}", e)))?;
        let snapshot_version_id: Option<i64> = conn
            .query_row(
                "SELECT MAX(id) FROM page_versions WHERE page_id = (SELECT id FROM pages WHERE slug = ?1)",
                params![slug],
                |row| row.get(0),
            )
            .map_err(|e| GBrainError::Database(format!("查询页面快照失败: {}", e)))?;

        // M28 修复：追加前检查是否已存在相同标题的节，避免重复追加
        let existing_content: String = conn
            .query_row(
                "SELECT compiled_truth FROM pages WHERE slug = ?1",
                params![slug],
                |row| row.get::<_, String>(0),
            )
            .unwrap_or_default();
        if existing_content.contains(&format!("## {}", section)) {
            // 已存在相同标题的节，跳过追加
            tracing::debug!(slug = %slug, section = %section, "影子页面已包含同名 section，跳过追加");
            return Ok(snapshot_version_id);
        }

        // 追加内容到 page 的 compiled_truth 列
        let append_text = format!("\n\n## {}\n\n{}", section, content);
        conn.execute(
            "UPDATE pages SET compiled_truth = compiled_truth || ?1, updated_at = datetime('now') WHERE slug = ?2",
            params![append_text, slug],
        ).map_err(|e| GBrainError::Database(format!("更新影子页面失败: {}", e)))?;

        // 修复：同步更新 compiled_truth_tokens 和 content_hash，
        // 与 put_page 保持一致，确保中文分词索引和内容变更检测正确。
        // 统一走 sync_page_tokens_and_hash，避免重复代码
        sync_page_tokens_and_hash(conn, slug)?;

        // 修复：更新 compiled_truth 后同步重建 chunk，确保 BrainFirst 搜索能找到新内容
        // 使用 rebuild_chunks_for_page 统一处理，避免列名不一致等重复错误
        rebuild_chunks_for_page(conn, slug)?;
        Ok(snapshot_version_id)
    } else {
        // 修复：找不到页面时返回错误，而不是静默返回 Ok。
        // 之前找不到页面只 debug 日志就返回 Ok，候选仍被标记为 applied，
        // 但实际内容没写入页面，导致"标记 applied 但实际没写入"的问题
        Err(GBrainError::PageNotFound(format!(
            "影子页面 {} 不存在，无法写入",
            slug
        )))
    }
}

/// KB 文档信息（简化版）
struct KbDocInfo {
    #[allow(dead_code)]
    id: i64,
    original_name: String,
    canonical_slug: String,
    summary: Option<String>,
    keywords: Option<Vec<String>>,
    entity_names: Option<Vec<String>>,
}

/// KB 文档节点信息（简化版）
struct KbNodeInfo {
    id: i64,
    /// 节点类型，从 node_metadata JSON 的 "node_type" 字段提取
    node_type: Option<String>,
    /// 标题路径（schema 中实际列名为 title_path）
    title: String,
    content: String,
    metadata: serde_json::Value,
}

/// 从 artifact_projections 中查找影子页面的实际 slug
///
/// 影子页面投影的 projection_ref 格式为 "slug:documents/{artifact_slug_with_hash}"，
/// 从中提取完整 slug（含 hash 后缀），作为候选的 target_slug。
/// 找不到时返回 None，调用方降级到 KB 的 canonical_slug。
fn resolve_shadow_page_slug(conn: &Connection, artifact_id: i64) -> Option<String> {
    let projections = super::store::find_projections_by_artifact(conn, artifact_id).ok()?;
    for proj in projections {
        if proj.projection_type == "brain_shadow_page" && proj.status == "active" {
            if let Some(slug) = proj.projection_ref.strip_prefix("slug:") {
                return Some(slug.to_string());
            }
        }
    }
    None
}

/// 获取 KB 文档信息
fn get_kb_document_info(conn: &Connection, kb_document_id: i64) -> Result<KbDocInfo> {
    conn.query_row(
        "SELECT id, original_name, name_tokens, title, summary, keywords, entity_names
         FROM kb_documents WHERE id = ?1",
        params![kb_document_id],
        |row| {
            let id: i64 = row.get(0)?;
            let original_name: String = row.get(1)?;
            let name_tokens: String = row.get(2)?;
            let _title: String = row.get(3)?;
            let summary_raw: String = row.get(4)?;
            let keywords_raw: String = row.get(5)?;
            let entity_names_raw: String = row.get(6)?;

            // Parse summary (may be empty string)
            let summary = if summary_raw.is_empty() {
                None
            } else {
                Some(summary_raw)
            };

            // Parse keywords (comma-separated or JSON array)
            let keywords: Option<Vec<String>> = if keywords_raw.is_empty() {
                None
            } else if keywords_raw.starts_with('[') {
                serde_json::from_str(&keywords_raw).ok()
            } else {
                Some(
                    keywords_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect(),
                )
            };

            // Parse entity_names (comma-separated or JSON array)
            let entity_names: Option<Vec<String>> = if entity_names_raw.is_empty() {
                None
            } else if entity_names_raw.starts_with('[') {
                serde_json::from_str(&entity_names_raw).ok()
            } else {
                Some(
                    entity_names_raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .collect(),
                )
            };

            // canonical_slug from name_tokens or original_name
            let canonical_slug = if name_tokens.is_empty() {
                original_name.replace('.', "-").to_lowercase()
            } else {
                name_tokens
            };

            Ok(KbDocInfo {
                id,
                original_name,
                canonical_slug,
                summary,
                keywords,
                entity_names,
            })
        },
    )
    .map_err(|e| GBrainError::Database(format!("获取 KB 文档信息失败: {}", e)))
}

/// 获取 KB 文档节点
fn get_kb_document_nodes(conn: &Connection, kb_document_id: i64) -> Result<Vec<KbNodeInfo>> {
    // 查询实际存在的列：id, title_path, content, node_metadata
    // node_type 不存在，需从 node_metadata JSON 中提取
    let mut stmt = conn
        .prepare(
            "SELECT id, title_path, content, node_metadata
         FROM kb_document_nodes WHERE document_id = ?1",
        )
        .map_err(|e| GBrainError::Database(format!("准备查询节点失败: {}", e)))?;

    let rows = stmt
        .query_map(params![kb_document_id], |row| {
            let id: i64 = row.get(0)?;
            let title_path: String = row.get(1)?;
            let content: String = row.get(2)?;
            let node_metadata: String = row.get::<_, String>(3)?;

            let metadata: serde_json::Value =
                serde_json::from_str(&node_metadata).unwrap_or(serde_json::json!({}));

            // 从 node_metadata JSON 中提取 node_type
            let node_type = metadata
                .get("node_type")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            Ok(KbNodeInfo {
                id,
                node_type,
                title: title_path,
                content,
                metadata,
            })
        })
        .map_err(|e| GBrainError::Database(format!("查询节点失败: {}", e)))?;

    let mut result = Vec::new();
    for row in rows {
        result.push(row.map_err(|e| GBrainError::Database(format!("映射节点失败: {}", e)))?);
    }
    Ok(result)
}

fn row_to_promotion_candidate(
    row: &Row,
) -> std::result::Result<PromotionCandidate, rusqlite::Error> {
    Ok(PromotionCandidate {
        id: row.get(0)?,
        created_at: row.get(1)?,
        updated_at: row.get(2)?,
        artifact_id: row.get(3)?,
        occurrence_id: row.get(4)?,
        kb_document_id: row.get(5)?,
        kb_node_id: row.get(6)?,
        candidate_type: row.get(7)?,
        target_slug: row.get(8)?,
        target_field: row.get(9)?,
        title: row.get(10)?,
        proposed_payload: row.get(11)?,
        evidence_json: row.get(12)?,
        confidence: row.get(13)?,
        risk_level: row.get(14)?,
        status: row.get(15)?,
        reviewer: row.get(16)?,
        review_notes: row.get(17)?,
        applied_at: row.get(18)?,
        candidate_fingerprint: row.get::<_, Option<String>>(19)?.unwrap_or_default(),
    })
}

fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 修复：页面内容变更后重建基础 chunk，确保 BrainFirst 搜索能找到更新后的内容
/// 之前直接 UPDATE compiled_truth 但不更新 chunk，导致搜索索引与页面内容脱节
///
/// 修复：使用 chunker 对全文分块重建，而非只取前 800 字插入单个 chunk。
/// shadow page 超过 800 字后，新追加的 facts/entities/events 不会进入 chunk 索引，
/// BrainFirst 搜索搜不到刚提升的内容。同时同步处理 chunk_text_tokens 和 vec_chunks。
fn rebuild_chunks_for_page(conn: &Connection, slug: &str) -> Result<()> {
    // 读取 page_id、compiled_truth 和 page_type
    let (page_id, content, page_type): (i64, Option<String>, String) = conn
        .query_row(
            "SELECT id, compiled_truth, page_type FROM pages WHERE slug = ?1",
            params![slug],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| GBrainError::Database(format!("查询页面失败: {}", e)))?;

    let content = match content {
        Some(c) if !c.is_empty() => c,
        _ => return Ok(()),
    };

    // 使用事务安全 page chunker 对全文分块
    let page_type = crate::types::PageType::from_str_lossy(&page_type);
    let chunks = crate::chunker::chunk_page_content_transactional(&content, &page_type);

    // 修复：先收集旧 chunk_id，再删 vec_chunks，最后删 chunks。
    // 之前先删 chunks 再用子查询删 vec_chunks，子查询已为空，旧向量行不会被删。
    let old_chunk_ids: Vec<i64> = conn
        .prepare("SELECT id FROM chunks WHERE page_id = ?1")
        .map_err(|e| GBrainError::Database(format!("查询旧 chunk_id 失败: {}", e)))?
        .query_map(params![page_id], |row| row.get::<_, i64>(0))
        .map_err(|e| GBrainError::Database(format!("遍历旧 chunk_id 失败: {}", e)))?
        .filter_map(|r| r.ok())
        .collect();

    // 先删除旧 vec_chunks（通过收集到的 chunk_id）
    // 修复：检测 vec_chunks 表是否存在，不存在时跳过（sqlite-vec 未加载的环境）
    // 之前 prepare 失败直接返回错误，但 sqlite_engine 初始化允许 vec_chunks 创建失败
    let has_vec_chunks = conn.prepare("SELECT 1 FROM vec_chunks LIMIT 0").is_ok();
    if has_vec_chunks && !old_chunk_ids.is_empty() {
        let placeholders: Vec<String> = old_chunk_ids.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "DELETE FROM vec_chunks WHERE chunk_id IN ({})",
            placeholders.join(",")
        );
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| GBrainError::Database(format!("准备删除 vec_chunks 失败: {}", e)))?;
        let params: Vec<&dyn rusqlite::ToSql> = old_chunk_ids
            .iter()
            .map(|id| id as &dyn rusqlite::ToSql)
            .collect();
        stmt.execute(params.as_slice()).ok(); // vec_chunks 表可能不存在，忽略错误
    }

    // 再删除旧 chunks
    conn.execute("DELETE FROM chunks WHERE page_id = ?1", params![page_id])
        .map_err(|e| GBrainError::Database(format!("删除旧 chunk 失败: {}", e)))?;

    for chunk in &chunks {
        // 中文分词，用于 FTS5 索引
        let chunk_text_tokens = crate::nlp::chinese::tokenize_content(&chunk.chunk_text);
        conn.execute(
            "INSERT INTO chunks (page_id, chunk_index, chunk_text, chunk_text_tokens, chunk_source, token_count, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, datetime('now'))",
            params![page_id, chunk.chunk_index, chunk.chunk_text, chunk_text_tokens, "compiled_truth", chunk.token_count],
        )
        .map_err(|e| GBrainError::Database(format!("重建 chunk 失败: {}", e)))?;
    }

    Ok(())
}

/// 将 serde_json::Value 转换为 canonical JSON 字符串（sorted keys），
/// 与 src/operations.rs canonical_json 逻辑一致，确保 page_create 的
/// content_hash 和 put_page 的 content_hash 对相同 frontmatter 产生相同结果。
///
/// C3 fix: 使用 `serde_json::to_string()` 对 key/value 进行正确的 JSON 转义，
/// 而非简单 `format!("{}:{}", k, v)` 拼接（当 key/value 含 `:`, `{`, `"` 等字符时
/// 会产生歧义输出，导致 hash 碰撞或假阴性）。
fn serialize_canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut sorted: Vec<_> = map.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(b.0));
            let pairs: Vec<String> = sorted
                .iter()
                .map(|(k, v)| {
                    let key = serde_json::to_string(k).unwrap_or_else(|_| k.to_string());
                    let val = serialize_canonical_json(v);
                    format!("{}:{}", key, val)
                })
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        serde_json::Value::Array(arr) => {
            let items: Vec<String> = arr.iter().map(serialize_canonical_json).collect();
            format!("[{}]", items.join(","))
        }
        // 对于 String/Number/Bool/Null，使用 serde_json::to_string 确保
        // 字符串带引号且转义，数字/布尔值原样输出
        other => serde_json::to_string(other).unwrap_or_else(|_| other.to_string()),
    }
}
