//! Search quality evaluation framework
//! Mirrors gbrain's src/core/search/eval.ts
//!
//! Supports P@k, R@k, MRR, nDCG@k metrics and A/B comparison.

use crate::engine::BrainEngine;
use crate::error::Result;
use crate::search::hybrid::{hybrid_search, HybridOpts};
use crate::search::keyword::build_fts_query;
use crate::sqlite_engine::SqliteEngine;
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// A single evaluation query with ground truth
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalQrel {
    pub query: String,
    pub relevant: Vec<String>,
    #[serde(default)]
    pub irrelevant: Vec<String>,
}

/// Evaluation configuration
#[derive(Debug, Clone)]
pub struct EvalConfig {
    pub strategy: EvalStrategy,
    pub rrf_k: usize,
    pub k: usize,
    pub expand: bool,
}

impl Default for EvalConfig {
    fn default() -> Self {
        Self {
            strategy: EvalStrategy::Hybrid,
            rrf_k: 60,
            k: 5,
            expand: false,
        }
    }
}

/// Search strategy for evaluation
#[derive(Debug, Clone, Copy)]
pub enum EvalStrategy {
    Hybrid,
    Keyword,
    Vector,
}

/// Per-query evaluation metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryResult {
    pub query: String,
    pub retrieved: Vec<String>,
    #[serde(rename = "pAtK")]
    pub p_at_k: f64,
    #[serde(rename = "rAtK")]
    pub r_at_k: f64,
    pub mrr: f64,
    #[serde(rename = "ndcgAtK")]
    pub ndcg_at_k: f64,
}

/// Full evaluation report
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalReport {
    pub config_label: String,
    pub k: usize,
    pub num_queries: usize,
    pub avg_p_at_k: f64,
    pub avg_r_at_k: f64,
    pub avg_mrr: f64,
    pub avg_ndcg_at_k: f64,
    pub results: Vec<QueryResult>,
}

/// Precision at k
pub fn precision_at_k(relevant: &HashSet<String>, retrieved: &[String], k: usize) -> f64 {
    if k == 0 || retrieved.is_empty() {
        return 0.0;
    }
    let top_k = &retrieved[..k.min(retrieved.len())];
    let hits = top_k.iter().filter(|s| relevant.contains(*s)).count() as f64;
    hits / k as f64
}

/// Recall at k
pub fn recall_at_k(relevant: &HashSet<String>, retrieved: &[String], k: usize) -> f64 {
    if relevant.is_empty() || k == 0 || retrieved.is_empty() {
        return 0.0;
    }
    let top_k = &retrieved[..k.min(retrieved.len())];
    let hits = top_k.iter().filter(|s| relevant.contains(*s)).count() as f64;
    hits / relevant.len() as f64
}

/// Mean Reciprocal Rank
pub fn mrr(relevant: &HashSet<String>, retrieved: &[String]) -> f64 {
    for (i, doc) in retrieved.iter().enumerate() {
        if relevant.contains(doc) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// Normalized Discounted Cumulative Gain at k
pub fn ndcg_at_k(relevant: &HashSet<String>, retrieved: &[String], k: usize) -> f64 {
    if relevant.is_empty() || k == 0 || retrieved.is_empty() {
        return 0.0;
    }
    let top_k = &retrieved[..k.min(retrieved.len())];
    let dcg: f64 = top_k
        .iter()
        .enumerate()
        .map(|(i, doc)| {
            let gain = if relevant.contains(doc) { 1.0 } else { 0.0 };
            if i == 0 {
                gain
            } else {
                gain / (((i + 2) as f64).ln() / std::f64::consts::LN_2)
            }
        })
        .sum();
    let ideal_dcg: f64 = (0..relevant.len().min(k))
        .map(|i| {
            let gain = 1.0;
            if i == 0 {
                gain
            } else {
                gain / (((i + 2) as f64).ln() / std::f64::consts::LN_2)
            }
        })
        .sum();
    if ideal_dcg == 0.0 {
        0.0
    } else {
        let score = dcg / ideal_dcg;
        // M54: 防御性检查 NaN/Inf（理论上不应出现，但浮点运算可能产生异常值）
        if score.is_finite() {
            score
        } else {
            0.0
        }
    }
}

/// Run evaluation
pub fn run_eval(
    engine: &SqliteEngine,
    qrels: &[EvalQrel],
    config: &EvalConfig,
) -> Result<EvalReport> {
    let mut results = Vec::new();
    let mut sum_p = 0.0;
    let mut sum_r = 0.0;
    let mut sum_mrr = 0.0;
    let mut sum_ndcg = 0.0;
    let hybrid_opts = HybridOpts {
        rrf_k: config.rrf_k,
        ..HybridOpts::default()
    };

    for qrel in qrels {
        let relevant_set: HashSet<String> = qrel.relevant.iter().cloned().collect();
        let search_opts = SearchOpts {
            limit: Some(config.k * 3),
            detail_level: Some(DetailLevel::Medium),
            ..SearchOpts::default()
        };
        let hits = match config.strategy {
            EvalStrategy::Keyword => {
                let fts_query = build_fts_query(&qrel.query);
                if fts_query.is_empty() {
                    Vec::new()
                } else {
                    engine.search_keyword(&fts_query, search_opts)?
                }
            }
            EvalStrategy::Vector => {
                // Vector search requires an embedding provider; return an error
                // instead of silently returning empty results
                return Err(crate::error::GBrainError::InvalidInput(
                    "Vector evaluation requires an embedding provider; use keyword or hybrid strategy instead".into()
                ));
            }
            EvalStrategy::Hybrid => {
                hybrid_search(engine, &qrel.query, None, search_opts, hybrid_opts.clone())?.results
            }
        };
        let retrieved: Vec<String> = hits.iter().map(|r| r.slug.clone()).collect();
        let p = precision_at_k(&relevant_set, &retrieved, config.k);
        let r = recall_at_k(&relevant_set, &retrieved, config.k);
        let m = mrr(&relevant_set, &retrieved);
        let n = ndcg_at_k(&relevant_set, &retrieved, config.k);
        sum_p += p;
        sum_r += r;
        sum_mrr += m;
        sum_ndcg += n;
        results.push(QueryResult {
            query: qrel.query.clone(),
            retrieved,
            p_at_k: p,
            r_at_k: r,
            mrr: m,
            ndcg_at_k: n,
        });
    }
    let n = qrels.len().max(1) as f64;
    Ok(EvalReport {
        config_label: format!("{:?}", config.strategy),
        k: config.k,
        num_queries: qrels.len(),
        avg_p_at_k: sum_p / n,
        avg_r_at_k: sum_r / n,
        avg_mrr: sum_mrr / n,
        avg_ndcg_at_k: sum_ndcg / n,
        results,
    })
}

/// A/B comparison
pub fn run_ab_eval(
    engine: &SqliteEngine,
    qrels: &[EvalQrel],
    config_a: &EvalConfig,
    config_b: &EvalConfig,
) -> Result<(EvalReport, EvalReport)> {
    Ok((
        run_eval(engine, qrels, config_a)?,
        run_eval(engine, qrels, config_b)?,
    ))
}

/// Parse qrels from JSON string or file path
pub fn parse_qrels(json: &str) -> std::result::Result<Vec<EvalQrel>, String> {
    if let Ok(qrels) = serde_json::from_str::<Vec<EvalQrel>>(json) {
        return Ok(qrels);
    }
    if let Ok(content) = std::fs::read_to_string(json) {
        serde_json::from_str(&content).map_err(|e| format!("Failed to parse qrels file: {}", e))
    } else {
        Err("Invalid qrels: not valid JSON and not a readable file path".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_precision_at_k() {
        let relevant: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let retrieved: Vec<String> = ["a", "x", "b", "y", "c"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert!((precision_at_k(&relevant, &retrieved, 3) - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_recall_at_k() {
        let relevant: HashSet<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        let retrieved: Vec<String> = ["a", "x", "b"].iter().map(|s| s.to_string()).collect();
        assert!((recall_at_k(&relevant, &retrieved, 3) - 2.0 / 3.0).abs() < 0.01);
    }

    #[test]
    fn test_mrr() {
        let relevant: HashSet<String> = ["b"].iter().map(|s| s.to_string()).collect();
        let retrieved: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert!((mrr(&relevant, &retrieved) - 0.5).abs() < 0.01);
    }

    #[test]
    fn test_parse_qrels_inline() {
        let json = r#"[{"query": "test", "relevant": ["a", "b"], "irrelevant": ["c"]}]"#;
        let qrels = parse_qrels(json).unwrap();
        assert_eq!(qrels.len(), 1);
    }
}
