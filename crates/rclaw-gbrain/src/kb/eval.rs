//! 搜索评测模块 (P4-017~P4-022)
//!
//! 离线评测命令、Recall@K/MRR@K/NDCG@K 指标计算、搜索日志、反馈 API。

use crate::error::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// 评测查询条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalQuery {
    pub id: i64,
    pub query_text: String,
    pub query_type: String,
    pub expected_document_ids: Vec<i64>,
}

/// 评测结果汇总
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalResult {
    pub total_queries: usize,
    pub recall_at_20: f64,
    pub mrr_at_10: f64,
    pub ndcg_at_10: f64,
    pub no_result_rate: f64,
    pub p95_latency_ms: u64,
}

/// 计算 Recall@K
pub fn recall_at_k(expected: &[i64], retrieved: &[i64], k: usize) -> f64 {
    if expected.is_empty() {
        return 1.0;
    }
    let retrieved_set: std::collections::HashSet<_> = retrieved.iter().take(k).copied().collect();
    let hits = expected
        .iter()
        .filter(|id| retrieved_set.contains(id))
        .count();
    hits as f64 / expected.len() as f64
}

/// 计算 MRR@K (Mean Reciprocal Rank)
pub fn mrr_at_k(expected: &[i64], retrieved: &[i64], k: usize) -> f64 {
    for (rank, id) in retrieved.iter().take(k).enumerate() {
        if expected.contains(id) {
            return 1.0 / ((rank + 1) as f64);
        }
    }
    0.0
}

/// 计算 NDCG@K (Normalized Discounted Cumulative Gain)
/// 简化为二值相关性（命中=1，未命中=0）
pub fn ndcg_at_k(expected: &[i64], retrieved: &[i64], k: usize) -> f64 {
    let expected_set: std::collections::HashSet<_> = expected.iter().collect();
    let ideal_count = expected.len().min(k) as f64;
    if ideal_count == 0.0 {
        return 1.0;
    }

    let mut dcg = 0.0f64;
    for (rank, id) in retrieved.iter().take(k).enumerate() {
        if expected_set.contains(id) {
            let gain = 1.0f64;
            dcg += gain / ((rank + 2) as f64).log2();
        }
    }

    let mut idcg = 0.0f64;
    for i in 0..ideal_count as usize {
        idcg += 1.0f64 / ((i + 2) as f64).log2();
    }

    if idcg == 0.0 {
        1.0
    } else {
        dcg / idcg
    }
}

/// 计算汇总评测指标
pub fn compute_eval_summary(queries: &[(Vec<i64>, Vec<i64>, u64)]) -> EvalResult {
    let total = queries.len();
    if total == 0 {
        return EvalResult {
            total_queries: 0,
            recall_at_20: 0.0,
            mrr_at_10: 0.0,
            ndcg_at_10: 0.0,
            no_result_rate: 0.0,
            p95_latency_ms: 0,
        };
    }

    let mut recall_sum = 0.0;
    let mut mrr_sum = 0.0;
    let mut ndcg_sum = 0.0;
    let mut no_result = 0;
    let mut latencies: Vec<u64> = Vec::new();

    for (expected, retrieved, latency) in queries {
        recall_sum += recall_at_k(expected, retrieved, 20);
        mrr_sum += mrr_at_k(expected, retrieved, 10);
        ndcg_sum += ndcg_at_k(expected, retrieved, 10);
        if retrieved.is_empty() {
            no_result += 1;
        }
        latencies.push(*latency);
    }

    latencies.sort_unstable();
    let p95_idx = ((latencies.len() as f64) * 0.95).ceil() as usize - 1;

    EvalResult {
        total_queries: total,
        recall_at_20: recall_sum / total as f64,
        mrr_at_10: mrr_sum / total as f64,
        ndcg_at_10: ndcg_sum / total as f64,
        no_result_rate: no_result as f64 / total as f64,
        p95_latency_ms: latencies.get(p95_idx).copied().unwrap_or(0),
    }
}

/// 记录搜索日志
#[allow(clippy::too_many_arguments)]
pub fn log_search(
    conn: &Connection,
    query_normalized: &str,
    library_ids: &[i64],
    profile: &str,
    planner_type: &str,
    result_count: usize,
    latency_ms: u64,
    cache_hit: bool,
    embedding_index_id: Option<i64>,
    result_document_ids: &[i64],
) -> Result<i64> {
    conn.execute(
        "INSERT INTO kb_search_logs (query_normalized, library_ids, profile, planner_type, \
         result_count, latency_ms, cache_hit, embedding_index_id, result_document_ids) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            query_normalized,
            serde_json::to_string(library_ids).unwrap_or_default(),
            profile,
            planner_type,
            result_count as i32,
            latency_ms as i32,
            cache_hit as i32,
            embedding_index_id,
            serde_json::to_string(result_document_ids).unwrap_or_default(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 添加搜索反馈
pub fn add_search_feedback(
    conn: &Connection,
    search_log_id: Option<i64>,
    document_id: Option<i64>,
    node_id: Option<i64>,
    rating: i32,
    comment: &str,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO kb_search_feedback (search_log_id, document_id, node_id, rating, comment) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            search_log_id,
            document_id,
            node_id,
            rating.clamp(0, 5),
            comment
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 添加评测查询
pub fn add_eval_query(
    conn: &Connection,
    library_id: i64,
    query_text: &str,
    query_type: &str,
    expected_document_ids: &[i64],
) -> Result<i64> {
    let ids_json = serde_json::to_string(expected_document_ids).unwrap_or_default();
    conn.execute(
        "INSERT INTO kb_search_eval_queries (library_id, query_text, query_type, expected_document_ids) \
         VALUES (?1, ?2, ?3, ?4)",
        params![library_id, query_text, query_type, ids_json],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 获取某个 library 的所有评测查询
pub fn list_eval_queries(conn: &Connection, library_id: i64) -> Result<Vec<EvalQuery>> {
    let mut stmt = conn.prepare(
        "SELECT id, query_text, query_type, expected_document_ids \
         FROM kb_search_eval_queries WHERE library_id = ?1",
    )?;
    let rows = stmt.query_map(params![library_id], |row| {
        let ids_str: String = row.get(3)?;
        Ok(EvalQuery {
            id: row.get(0)?,
            query_text: row.get(1)?,
            query_type: row.get(2)?,
            expected_document_ids: serde_json::from_str(&ids_str).unwrap_or_default(),
        })
    })?;
    let results: Vec<EvalQuery> = rows.filter_map(|r| r.ok()).collect();
    Ok(results)
}

// ---------------------------------------------------------------------------
// P5-014: Embedding 模型灰度评测 — 对比两个 embedding index 的搜索质量
// ---------------------------------------------------------------------------

/// Embedding index 对比报告
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingComparisonReport {
    pub index_id_1: i64,
    pub index_id_2: i64,
    pub model_1: String,
    pub model_2: String,
    pub result_1: EvalResult,
    pub result_2: EvalResult,
    pub recall_delta: f64,
    pub mrr_delta: f64,
    pub ndcg_delta: f64,
}

/// 对比两个 embedding index 的搜索质量。
///
/// 使用同一评测集分别对两个 index 执行搜索评测，
/// 输出 Recall/MRR/NDCG 差异报告。
pub fn compare_embedding_indexes(
    conn: &Connection,
    index_id_1: i64,
    index_id_2: i64,
) -> Result<String> {
    // 读取两个 index 的元数据
    let get_model = |idx_id: i64| -> String {
        conn.query_row(
            "SELECT model FROM kb_embedding_indexes WHERE id=?1",
            params![idx_id],
            |row| row.get::<_, String>(0),
        )
        .unwrap_or_default()
    };
    let model_1 = get_model(index_id_1);
    let model_2 = get_model(index_id_2);

    // 读取所有评测查询
    let mut stmt = conn.prepare(
        "SELECT id, library_id, query_text, query_type, expected_document_ids \
         FROM kb_search_eval_queries LIMIT 100",
    )?;
    let queries: Vec<(i64, i64, String, String, Vec<i64>)> = stmt
        .query_map([], |row| {
            let ids_str: String = row.get(4)?;
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                serde_json::from_str(&ids_str).unwrap_or_default(),
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    if queries.is_empty() {
        return Ok(format!(
            "Embedding index 对比:\n  index {} (model: {})\n  index {} (model: {})\n\n无评测查询数据，请先添加评测查询。",
            index_id_1, model_1, index_id_2, model_2
        ));
    }

    // 对每个查询，分别用两个 index 搜索并计算指标
    // 简化实现：基于已有搜索日志统计（不实际执行搜索，避免需要 embedding API）
    let mut results_1: Vec<(Vec<i64>, Vec<i64>, u64)> = Vec::new();
    let mut results_2: Vec<(Vec<i64>, Vec<i64>, u64)> = Vec::new();

    for (_id, _lib_id, query_text, _query_type, expected) in &queries {
        // 对 index 1: 从搜索日志获取历史结果
        let retrieved_1 = get_search_results_for_index(conn, index_id_1, query_text);
        results_1.push((expected.clone(), retrieved_1, 0));

        // 对 index 2: 同上
        let retrieved_2 = get_search_results_for_index(conn, index_id_2, query_text);
        results_2.push((expected.clone(), retrieved_2, 0));
    }

    let eval_1 = compute_eval_summary(&results_1);
    let eval_2 = compute_eval_summary(&results_2);

    let recall_delta = eval_2.recall_at_20 - eval_1.recall_at_20;
    let mrr_delta = eval_2.mrr_at_10 - eval_1.mrr_at_10;
    let ndcg_delta = eval_2.ndcg_at_10 - eval_1.ndcg_at_10;

    let report = EmbeddingComparisonReport {
        index_id_1,
        index_id_2,
        model_1: model_1.clone(),
        model_2: model_2.clone(),
        result_1: eval_1,
        result_2: eval_2,
        recall_delta,
        mrr_delta,
        ndcg_delta,
    };

    Ok(format!(
        "Embedding index 对比:\n\
         \n  index {} (model: {})\n    Recall@20: {:.4}  MRR@10: {:.4}  NDCG@10: {:.4}\n\
         \n  index {} (model: {})\n    Recall@20: {:.4}  MRR@10: {:.4}  NDCG@10: {:.4}\n\
         \n  差异 (index2 - index1):\n    Recall@20: {:+.4}  MRR@10: {:+.4}  NDCG@10: {:+.4}\n\
         \n  评测查询数: {}",
        index_id_1,
        model_1,
        report.result_1.recall_at_20,
        report.result_1.mrr_at_10,
        report.result_1.ndcg_at_10,
        index_id_2,
        model_2,
        report.result_2.recall_at_20,
        report.result_2.mrr_at_10,
        report.result_2.ndcg_at_10,
        recall_delta,
        mrr_delta,
        ndcg_delta,
        queries.len(),
    ))
}

/// 从搜索日志获取某 index 的历史搜索结果（document_id 列表）
fn get_search_results_for_index(conn: &Connection, index_id: i64, query: &str) -> Vec<i64> {
    // 按 embedding_index_id + query 从搜索日志中查找匹配的结果
    let sql = "SELECT result_document_ids FROM kb_search_logs \
               WHERE embedding_index_id=?1 AND query_normalized=?2 LIMIT 1";
    conn.query_row(sql, params![index_id, query], |row| {
        let ids_str: String = row.get(0)?;
        Ok(serde_json::from_str(&ids_str).unwrap_or_default())
    })
    .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_recall_perfect() {
        assert!((recall_at_k(&[1, 2, 3], &[1, 2, 3, 4, 5], 20) - 1.0).abs() < 0.001);
    }

    #[test]
    fn test_recall_partial() {
        assert!((recall_at_k(&[1, 2, 3], &[1, 4, 5, 6, 7], 20) - (1.0 / 3.0)).abs() < 0.001);
    }

    #[test]
    fn test_mrr_first_hit() {
        let mrr = mrr_at_k(&[2], &[1, 2, 3], 10);
        assert!((mrr - 0.5).abs() < 0.001);
    }

    #[test]
    fn test_mrr_no_hit() {
        assert!((mrr_at_k(&[10], &[1, 2, 3], 10)).abs() < 0.001);
    }

    #[test]
    fn test_ndcg_perfect() {
        assert!((ndcg_at_k(&[1, 2, 3], &[1, 2, 3, 4], 10) - 1.0).abs() < 0.001);
    }
}
