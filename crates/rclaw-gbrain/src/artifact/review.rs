//! 建议变更 — promotion 的用户友好包装（设计文档 §4.1.5）
//!
//! 对外暴露为 "suggested changes" / "review" 概念，
//! 内部仍然使用 promotion_candidates 表。

use crate::artifact::promotion;
use crate::artifact::types::{ArtifactReviewItem, PromotionCandidate, ReviewCandidateInput};
use crate::error::Result;
use rusqlite::Connection;
use tracing::{debug, info, warn};

/// 列出建议变更
pub fn list_suggested_changes(
    conn: &Connection,
    status: Option<&str>,
    target_slug: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ArtifactReviewItem>> {
    let candidates = promotion::list_candidates(conn, status, None, target_slug, limit, offset)?;
    debug!(
        "list_suggested_changes: status={:?}, target_slug={:?}, count={}",
        status,
        target_slug,
        candidates.len()
    );
    Ok(candidates
        .into_iter()
        .map(candidate_to_review_item)
        .collect())
}

/// 获取建议变更详情
pub fn get_suggested_change(
    conn: &Connection,
    change_id: i64,
) -> Result<Option<ArtifactReviewItem>> {
    let candidate = promotion::find_candidate_by_id(conn, change_id)?;
    Ok(candidate.map(candidate_to_review_item))
}

/// 应用建议变更
///
/// P1-9 修复：对 pending 变更执行 accept+apply 一体化流程。
/// 公开 facade artifact_review_apply 没有 accept 命令，
/// 但内部 apply_candidate 要求候选状态必须是 accepted。
/// 修复：如果候选状态是 pending，先自动 accept 再 apply，
/// 使普通用户通过 artifact_review_apply 即可应用 pending suggested change。
pub fn apply_suggested_change(conn: &Connection, change_id: i64) -> Result<PromotionCandidate> {
    // P1-9 修复：如果候选是 pending 状态，先自动 accept
    let candidate = promotion::find_candidate_by_id(conn, change_id)?.ok_or_else(|| {
        crate::error::GBrainError::PageNotFound(format!("候选 {} 不存在", change_id))
    })?;
    warn!(
        "apply_suggested_change: change_id={}, target_slug={}, status={}",
        change_id, candidate.target_slug, candidate.status
    );

    if candidate.status == "pending" {
        // 自动 accept：将状态改为 accepted，记录审核人和备注
        let now = chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
        conn.execute(
            "UPDATE promotion_candidates SET status = 'accepted', reviewer = 'auto_via_apply', review_notes = '通过 artifact_review_apply 自动接受', updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, change_id],
        ).map_err(|e| crate::error::GBrainError::Database(format!("自动接受候选失败: {}", e)))?;
    }

    promotion::apply_candidate(conn, change_id)
}

/// 拒绝建议变更
pub fn reject_suggested_change(
    conn: &Connection,
    input: &ReviewCandidateInput,
) -> Result<PromotionCandidate> {
    info!("reject_suggested_change: change_id={}", input.candidate_id);
    promotion::review_candidate(conn, input)
}

/// 回滚已应用的建议变更
pub fn rollback_suggested_change(conn: &Connection, change_id: i64) -> Result<PromotionCandidate> {
    warn!("rollback_suggested_change: change_id={}", change_id);
    promotion::rollback_candidate(conn, change_id)
}

/// 将内部 PromotionCandidate 转换为用户友好的 ArtifactReviewItem
fn candidate_to_review_item(c: PromotionCandidate) -> ArtifactReviewItem {
    ArtifactReviewItem {
        change_id: c.id,
        target_slug: c.target_slug.clone(),
        status: c.status.clone(),
        risk_level: c.risk_level.clone(),
        summary: c.title.clone(),
        evidence: if c.evidence_json.is_empty() {
            None
        } else {
            serde_json::from_str::<serde_json::Value>(&c.evidence_json).ok()
        },
        created_at: Some(c.created_at.clone()),
    }
}
