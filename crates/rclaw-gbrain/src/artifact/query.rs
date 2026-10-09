//! 统一查询引擎 — BrainFirst / EvidenceFirst / Provenance / TimelineFirst 四种策略
//!
//! 负责：
//! 1. 根据查询意图自动选择策略
//! 2. BrainFirst: 先查 gbrain，再查 KB 补充
//! 3. EvidenceFirst: 先查 KB 证据，再查 gbrain 上下文
//! 4. Provenance: 仅追溯来源链
//! 5. TimelineFirst: 先查时间线事件，再查 gbrain 上下文（§11.1/§11.2）

use rusqlite::{params, Connection};
use std::collections::{HashMap, HashSet};
use tracing::{debug, info};

use crate::config::Config;
use crate::error::{GBrainError, Result};
use crate::search::hybrid::{hybrid_search, HybridOpts};
use crate::search::intent::{classify_intent, Intent};

use super::projection;
use super::provenance;
use super::store;
use super::types::*;

/// 根据查询文本自动推断策略
pub fn infer_query_strategy(query: &str) -> QueryStrategy {
    // 使用现有意图分类器推断
    let intent = classify_intent(query);

    // 根据意图映射到策略
    let strategy = match intent.intent {
        // "是谁/是什么" 类问题 → BrainFirst
        Intent::Entity => QueryStrategy::BrainFirst,
        // "时间线/最近发生了什么" 类问题 → TimelineFirst（§11.2）
        Intent::Temporal => QueryStrategy::TimelineFirst,
        // "事件" 类问题 → TimelineFirst（§11.2: 事件与时间线紧密关联）
        Intent::Event => QueryStrategy::TimelineFirst,
        // 默认 → BrainFirst + KB fallback
        Intent::General => QueryStrategy::BrainFirst,
    };
    debug!(
        "infer_query_strategy: query={}, strategy={}",
        query, strategy
    );
    strategy
}

/// 执行统一查询
pub fn unified_query(
    conn: &Connection,
    input: &UnifiedQueryInput,
    engine: &crate::sqlite_engine::SqliteEngine,
    config: &crate::config::Config,
) -> Result<UnifiedQueryResult> {
    let strategy = if input.strategy == QueryStrategy::BrainFirst {
        // 尝试自动推断
        infer_query_strategy(&input.query)
    } else {
        input.strategy.clone()
    };

    info!("统一查询: query={}, strategy={}", input.query, strategy);

    // 修复：限制 limit 在 1..=100 范围内，防止负数导致 usize 溢出产生无限 SQL LIMIT
    let limit = input.limit.unwrap_or(10).clamp(1, 100);

    let mut brain_hits = Vec::new();
    let mut evidence_hits = Vec::new();
    let mut provenance_records = Vec::new();
    let mut timeline_hits = Vec::new();

    match strategy {
        QueryStrategy::BrainFirst => {
            // 1. 先查 gbrain
            // 修复：把 filter_slug 下推到 query_brain，在搜索阶段就限定 slug，
            // 避免先全局 LIMIT 再后置 retain 导致目标 slug 的命中被挤掉
            brain_hits = query_brain(
                conn,
                &input.query,
                limit,
                engine,
                config,
                input.filter_slug.as_deref(),
            )?;

            // 2. 如果 gbrain 结果不足，查 KB 补充
            if brain_hits.len() < limit as usize && input.include_evidence {
                evidence_hits = query_kb_evidence(
                    conn,
                    &input.query,
                    limit - brain_hits.len() as i64,
                    input.filter_slug.as_deref(),
                    config,
                )?;
                // 修复：BrainFirst 分支同样需要补全 KB evidence 的 artifact / shadow_page_slug，
                // 否则调用方拿到 passage_id 后无法继续 artifact_get focused 读取上下文
                enrich_evidence_with_artifact_metadata(
                    conn,
                    &mut evidence_hits,
                    input.filter_slug.as_deref(),
                )?;
            }

            // 3. 给 brain hit 附加 provenance
            // 修复：只收集 filter_slug 匹配的 brain_hits 的 provenance，
            // 避免提前收集非目标 slug 的来源记录
            if input.include_provenance {
                for hit in &brain_hits {
                    let prov = provenance::find_provenance_by_brain_slug(conn, &hit.slug)?;
                    provenance_records.extend(prov);
                }
            }
        }
        QueryStrategy::EvidenceFirst => {
            // 1. 先查 KB
            // 修复：把 filter_slug 下推到 query_kb_evidence
            evidence_hits = query_kb_evidence(
                conn,
                &input.query,
                limit,
                input.filter_slug.as_deref(),
                config,
            )?;

            // 2. 给 KB hit 附加 artifact 和 shadow page 信息
            // 修复：抽出 enrich_evidence_with_artifact_metadata helper，
            // 让 BrainFirst 的 evidence fallback 也能复用同样的补全逻辑
            enrich_evidence_with_artifact_metadata(
                conn,
                &mut evidence_hits,
                input.filter_slug.as_deref(),
            )?;

            // 3. 查 gbrain 上下文
            if brain_hits.len() < limit as usize {
                // 修复：把 filter_slug 下推到 query_brain
                brain_hits = query_brain(
                    conn,
                    &input.query,
                    limit / 2,
                    engine,
                    config,
                    input.filter_slug.as_deref(),
                )?;
            }

            // 4. 给 brain hit 附加 provenance
            // 修复：只收集 filter_slug 匹配的 brain_hits 的 provenance
            if input.include_provenance {
                for hit in &brain_hits {
                    let prov = provenance::find_provenance_by_brain_slug(conn, &hit.slug)?;
                    provenance_records.extend(prov);
                }
            }
        }
        QueryStrategy::Provenance => {
            // 仅追溯来源链
            if let Some(slug) = &input.filter_slug {
                provenance_records = provenance::find_provenance_by_brain_slug(conn, slug)?;
            } else {
                // 尝试从查询中推断 slug
                let brain_hits_tmp = query_brain(conn, &input.query, 1, engine, config, None)?;
                if let Some(hit) = brain_hits_tmp.first() {
                    provenance_records =
                        provenance::find_provenance_by_brain_slug(conn, &hit.slug)?;
                }
            }
        }
        QueryStrategy::TimelineFirst => {
            // §11.2: "最近发生了什么 / 时间线" → 先查时间线事件，再查 gbrain 上下文
            // 1. 查询已接受的 timeline_event 候选
            // 修复：把 filter_slug 下推到 query_timeline_events
            timeline_hits =
                query_timeline_events(conn, &input.query, limit, input.filter_slug.as_deref())?;

            // 2. 给时间线命中附加 shadow page 信息
            for hit in &mut timeline_hits {
                let shadow_slug = projection::find_shadow_page_slug(conn, hit.artifact_id)?;
                hit.shadow_page_slug = shadow_slug;
            }

            // 3. 查 gbrain 上下文补充
            if timeline_hits.len() < limit as usize {
                let remaining = limit - timeline_hits.len() as i64;
                // 修复：把 filter_slug 下推到 query_brain
                brain_hits = query_brain(
                    conn,
                    &input.query,
                    remaining,
                    engine,
                    config,
                    input.filter_slug.as_deref(),
                )?;
            }

            // 4. 给 brain hit 附加 provenance
            // 修复：只收集 filter_slug 匹配的 brain_hits 的 provenance
            if input.include_provenance {
                for hit in &brain_hits {
                    let prov = provenance::find_provenance_by_brain_slug(conn, &hit.slug)?;
                    provenance_records.extend(prov);
                }
            }
        }
    }

    // M26+M52 修复：预计算 filter_slug 的所有变体，避免在每次 retain 调用中重复计算
    // filter_slug 已下推到各查询函数，此处作为防御性兜底
    if let Some(slug) = &input.filter_slug {
        let filter_variants: HashSet<String> = slug_value_variants(slug).into_iter().collect();
        brain_hits.retain(|hit| slug_matches_filter_cached(&hit.slug, &filter_variants));
        evidence_hits.retain(|hit| {
            hit.shadow_page_slug
                .as_deref()
                .map(|s| slug_matches_filter_cached(s, &filter_variants))
                .unwrap_or(false)
        });
        timeline_hits.retain(|hit| {
            hit.shadow_page_slug
                .as_deref()
                .map(|s| slug_matches_filter_cached(s, &filter_variants))
                .unwrap_or(false)
        });
        // 同步过滤 provenance_records，只保留目标 slug 的来源记录
        provenance_records
            .retain(|rec| slug_matches_filter_cached(&rec.brain_slug, &filter_variants));
    }

    let total_hits =
        brain_hits.len() as i64 + evidence_hits.len() as i64 + timeline_hits.len() as i64;

    Ok(UnifiedQueryResult {
        strategy: strategy.to_string(),
        brain_hits,
        evidence_hits,
        timeline_hits,
        provenance_records,
        total_hits,
    })
}

pub(crate) fn slug_value_variants(slug: &str) -> Vec<String> {
    let normalized = slug.strip_prefix("slug:").unwrap_or(slug).trim_matches('/');
    let without_documents = normalized.strip_prefix("documents/").unwrap_or(normalized);
    let mut variants = vec![normalized.to_string()];
    if without_documents != normalized {
        variants.push(without_documents.to_string());
    }
    let documents_slug = format!("documents/{}", without_documents);
    if !variants.iter().any(|v| v == &documents_slug) {
        variants.push(documents_slug);
    }
    variants
}

fn slug_ref_variants(slug: &str) -> (String, String) {
    let values = slug_value_variants(slug);
    let first = values.first().cloned().unwrap_or_default();
    let second = values.get(1).cloned().unwrap_or_else(|| first.clone());
    (format!("slug:{}", first), format!("slug:{}", second))
}

/// M26+M52 修复：使用预计算的 filter 变体集合进行匹配，避免重复计算
fn slug_matches_filter_cached(candidate: &str, filter_variants: &HashSet<String>) -> bool {
    let candidate_variants = slug_value_variants(candidate);
    candidate_variants
        .iter()
        .any(|cv| filter_variants.contains(cv))
}

/// 查询 gbrain
/// 修复：当提供 filter_slug 时，通过 SearchOpts.include_slugs 下推精确 slug 过滤到搜索层，
/// 让搜索引擎只返回目标 slug 的页面，避免先全局 LIMIT 再后置 retain 导致假阴性。
fn query_brain(
    _conn: &Connection,
    query: &str,
    limit: i64,
    engine: &crate::sqlite_engine::SqliteEngine,
    _config: &crate::config::Config,
    filter_slug: Option<&str>,
) -> Result<Vec<BrainHit>> {
    // 修复：使用 include_slugs 精确限定搜索范围，不再扩大 limit * 3 后后置过滤
    let search_opts = crate::types::SearchOpts {
        limit: Some(limit as usize),
        include_slugs: filter_slug.map(slug_value_variants),
        ..Default::default()
    };
    let hybrid_opts = HybridOpts::default();

    let search_result = hybrid_search(engine, query, None, search_opts, hybrid_opts)
        .map_err(|e| GBrainError::Search(format!("gbrain 搜索失败: {}", e)))?;

    let hits: Vec<BrainHit> = search_result
        .results
        .into_iter()
        .map(|r| BrainHit {
            slug: r.slug,
            title: r.title,
            snippet: r.chunk_text,
            relevance: r.score,
            provenance: Vec::new(),
        })
        .collect();

    debug!(
        "query_brain: query={}, filter_slug={:?}, count={}",
        query,
        filter_slug,
        hits.len()
    );
    Ok(hits)
}

/// 查询 KB 证据
/// 修复：增加 filter_slug 参数，下推 slug 过滤到 SQL 查询阶段，
/// 通过 JOIN artifact_projections 限定只返回目标 slug 的 KB 文档，
/// 避免先全局 LIMIT 再后置 retain 导致目标 slug 的命中被挤掉
fn query_kb_evidence(
    conn: &Connection,
    query: &str,
    limit: i64,
    filter_slug: Option<&str>,
    config: &Config,
) -> Result<Vec<EvidenceHit>> {
    let plan = EvidenceQueryPlan::from_query(query);
    if plan.relaxed_match.is_empty() && plan.strict_match.is_none() {
        return Ok(Vec::new());
    }

    let limit_usize = limit.clamp(1, 100) as usize;
    let fetch_k = (limit_usize * 8).clamp(50, 500);

    let mut routes: Vec<Vec<EvidenceCandidate>> = Vec::new();

    if let Some(strict_match) = &plan.strict_match {
        let strict_nodes = query_node_candidates(conn, strict_match, fetch_k, filter_slug, 1.3)?;
        ensure_passages_for_candidates(conn, &strict_nodes)?;
        routes.push(strict_nodes);
        routes.push(query_passage_candidates(
            conn,
            strict_match,
            fetch_k,
            filter_slug,
            1.6,
        )?);
    }

    let relaxed_nodes =
        query_node_candidates(conn, &plan.relaxed_match, fetch_k, filter_slug, 0.8)?;
    ensure_passages_for_candidates(conn, &relaxed_nodes)?;
    routes.push(relaxed_nodes);
    routes.push(query_passage_candidates(
        conn,
        &plan.relaxed_match,
        fetch_k,
        filter_slug,
        1.0,
    )?);
    routes.push(query_kb_hybrid_candidates(
        conn,
        query,
        fetch_k,
        filter_slug,
        1.2,
    )?);

    let mut candidates = merge_evidence_routes(routes);
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    for candidate in &mut candidates {
        focus_evidence_candidate_content(candidate, &plan);
        candidate.local_score = score_evidence_candidate(candidate, &plan);
    }

    candidates = rerank_evidence_candidates(conn, query, candidates, &plan, config)?;
    candidates = dedup_evidence_by_document(candidates, 3);
    candidates.truncate(limit_usize);

    debug!(
        "query_kb_evidence: query={}, filter_slug={:?}, count={}, core_terms={:?}",
        query,
        filter_slug,
        candidates.len(),
        plan.core_terms
    );

    let mut hits: Vec<EvidenceHit> = candidates
        .into_iter()
        .map(|candidate| candidate.into_hit(conn, query, filter_slug, &plan))
        .collect::<Result<_>>()?;

    // P3: 为证据命中扩展同 section 前后 chunk 上下文
    expand_evidence_context(conn, &mut hits);

    Ok(hits)
}

/// P3: 为证据命中扩展同 section 前后 chunk 上下文。
///
/// 对每个有 passage_id 的证据命中，通过 kb_passage_spans 反查 node_id，
/// 再查询同 document_id + title_path 的前后 chunk，填充 context_before/context_after。
/// 每个邻居最多取 EVIDENCE_CTX_CHARS_PER_SIDE 字符（当前 200）。
fn expand_evidence_context(conn: &Connection, hits: &mut [EvidenceHit]) {
    /// 证据上下文每侧最大字符数（与 search.rs 的 context_before/context_after 默认值对齐）
    const EVIDENCE_CTX_CHARS_PER_SIDE: usize = 200;
    for hit in hits.iter_mut() {
        let Some(passage_id) = hit.passage_id else {
            continue;
        };
        // 通过 passage → node 反查 node_id
        let node_id: Option<i64> = conn
            .query_row(
                "SELECT node_id FROM kb_passage_spans WHERE id = ?1",
                rusqlite::params![passage_id],
                |row| row.get(0),
            )
            .ok();
        let Some(node_id) = node_id else {
            continue;
        };
        // 获取命中节点的 chunk_order、version_id、title_path
        let (chunk_order, version_id, title_path): (i32, Option<i64>, String) = match conn
            .query_row(
                "SELECT chunk_order, version_id, COALESCE(title_path, '') \
                 FROM kb_document_nodes WHERE id = ?1",
                rusqlite::params![node_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let doc_id = hit.kb_document_id;
        // 查找前一个 chunk（同 title_path + 同 version）
        let before = if chunk_order > 0 {
            neighbor_content(conn, doc_id, chunk_order - 1, version_id, &title_path).map(|s| {
                let chars: Vec<char> = s.chars().collect();
                let start = chars.len().saturating_sub(EVIDENCE_CTX_CHARS_PER_SIDE);
                chars[start..].iter().collect()
            })
        } else {
            None
        };
        // 查找后一个 chunk
        let after =
            neighbor_content(conn, doc_id, chunk_order + 1, version_id, &title_path).map(|s| {
                let chars: Vec<char> = s.chars().collect();
                let end = EVIDENCE_CTX_CHARS_PER_SIDE.min(chars.len());
                chars[..end].iter().collect()
            });
        hit.context_before = before;
        hit.context_after = after;
    }
}

/// 查找邻居 chunk 内容（同 document_id + chunk_order + 可选 version_id + title_path）
///
/// 注意：此函数与 search.rs 的 `find_neighbor` 实现相同模式。
/// 如需修改 SQL 构建逻辑，请同步更新两处。
fn neighbor_content(
    conn: &Connection,
    document_id: i64,
    target_order: i32,
    version_id: Option<i64>,
    title_path: &str,
) -> Option<String> {
    let mut sql = String::from(
        "SELECT content FROM kb_document_nodes WHERE document_id = ?1 AND chunk_order = ?2",
    );
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> =
        vec![Box::new(document_id), Box::new(target_order)];
    if let Some(vid) = version_id {
        sql.push_str(" AND version_id = ?3");
        params.push(Box::new(vid));
    }
    // P3: 同 title_path 确保上下文来自同一 section
    if !title_path.is_empty() {
        let idx = params.len() + 1;
        sql.push_str(&format!(" AND title_path = ?{}", idx));
        params.push(Box::new(title_path.to_string()));
    }
    sql.push_str(" AND retired_at IS NULL");
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    conn.query_row(&sql, param_refs.as_slice(), |row| row.get::<_, String>(0))
        .ok()
}

/// 修复：把 evidence 的 artifact / shadow_page_slug 补全抽成 helper，
/// 让 BrainFirst 与 EvidenceFirst 分支共享同一份补全逻辑。
///
/// 调用契约：
/// - 当 `hit.artifact` 已通过 SQL JOIN 填出（即 `EvidenceCandidate.into_hit` 内部
///   `artifact_id > 0` 的路径）时跳过，避免被无约束的 projection 反查覆盖；
/// - 当 `hit.shadow_page_slug` 已由 filter_slug 决策或 SQL 显式回填时跳过，
///   避免同一 kb_document 被多个 artifact 复用时拿到错误投影；
/// - 其余情况按 `kb_document_id` 反查活跃 `kb_document` 投影，并据此补出 artifact
///   与 shadow_page_slug，使调用方可以继续 `artifact_get(... content_mode="focused")`。
fn enrich_evidence_with_artifact_metadata(
    conn: &Connection,
    evidence_hits: &mut [EvidenceHit],
    filter_slug: Option<&str>,
) -> Result<()> {
    // TODO(m-6): 当前对每个 evidence hit 逐个调用 find_projection_by_ref + find_artifact_by_id，
    // 形成经典的 N+1 查询模式。当 evidence_hits 较多时（例如 20 条），可能产生 40+ 次 SQL 查询。
    // 优化方向：
    //   1. 先收集所有需要补全的 (kb_document_id, needs_artifact, needs_shadow) 三元组；
    //   2. 批量查询：SELECT * FROM artifact_projections WHERE projection_type='kb_document'
    //      AND projection_ref IN ('kb_document:1', 'kb_document:2', ...) AND status='active'；
    //   3. 根据 projection 结果再批量查询 artifact：SELECT * FROM artifacts WHERE id IN (...)；
    //   4. 最后遍历 evidence_hits 用 HashMap 填充，将 O(N) 次查询降为 O(1)（常量 2-3 次）。
    // 注意：store 层目前缺少批量查询接口，需要先添加 batch_find_projections_by_refs 等方法。
    for hit in evidence_hits.iter_mut() {
        if hit.kb_document_id <= 0 {
            continue;
        }
        // 已经具备 artifact 且 shadow_page_slug 也已就位，无需再做无约束反查
        if hit.artifact.is_some() && hit.shadow_page_slug.is_some() {
            continue;
        }
        // 仅在缺失对应字段时补全：
        // - 若 SQL JOIN 已经填出 artifact（filter_slug 路径），仅补 shadow_page_slug；
        // - 否则两者一起补。
        let kb_doc_id = hit.kb_document_id;
        let needs_artifact = hit.artifact.is_none();
        let needs_shadow = hit.shadow_page_slug.is_none();
        if !needs_artifact && !needs_shadow {
            continue;
        }

        let proj = store::find_projection_by_ref(
            conn,
            "kb_document",
            &format!("kb_document:{}", kb_doc_id),
        )
        .map_err(|e| GBrainError::Database(format!("查找 KB 投影失败: {}", e)))?;
        let Some(proj) = proj else {
            continue;
        };

        if needs_artifact {
            let artifact = store::find_artifact_by_id(conn, proj.artifact_id)
                .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?;
            hit.artifact = artifact;
        }

        if needs_shadow {
            // filter_slug 存在时优先使用过滤值（已规范化），避免被无约束 projection 覆盖
            if let Some(slug) = filter_slug {
                let normalized = slug_value_variants(slug)
                    .into_iter()
                    .next()
                    .unwrap_or_else(|| slug.to_string());
                hit.shadow_page_slug = Some(normalized);
            } else {
                hit.shadow_page_slug = projection::find_shadow_page_slug(conn, proj.artifact_id)?;
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub(crate) struct FocusedContentCandidate {
    pub snippet: String,
    pub score: f64,
    pub kb_document_id: Option<i64>,
    pub passage_id: Option<i64>,
    pub view_type: Option<String>,
    pub source_start: Option<i64>,
    pub source_end: Option<i64>,
}

pub(crate) fn query_focused_content_for_artifact(
    conn: &Connection,
    artifact_id: i64,
    query: Option<&str>,
    max_chars: usize,
    passage_id: Option<i64>,
    limit: usize,
) -> Result<Vec<FocusedContentCandidate>> {
    let document_ids = active_kb_document_ids_for_artifact(conn, artifact_id)?;
    if document_ids.is_empty() {
        return Ok(Vec::new());
    }

    let max_chars = max_chars.clamp(200, 10_000);
    let limit = limit.clamp(1, 10);

    if let Some(passage_id) = passage_id {
        return load_focused_passage_by_id(conn, &document_ids, passage_id, query, max_chars);
    }

    let Some(query) = query.map(str::trim).filter(|q| !q.is_empty()) else {
        return Ok(Vec::new());
    };

    for document_id in &document_ids {
        crate::kb::passage::ensure_document_passages(conn, *document_id)?;
    }

    let plan = EvidenceQueryPlan::from_query(query);
    let mut routes = Vec::new();
    if let Some(strict_match) = &plan.strict_match {
        routes.push(query_passage_candidates_for_document_ids(
            conn,
            strict_match,
            &document_ids,
            limit * 8,
            artifact_id,
            1.6,
        )?);
    }
    routes.push(query_passage_candidates_for_document_ids(
        conn,
        &plan.relaxed_match,
        &document_ids,
        limit * 8,
        artifact_id,
        1.0,
    )?);

    let mut candidates = merge_evidence_routes(routes);
    for candidate in &mut candidates {
        focus_evidence_candidate_content(candidate, &plan);
        candidate.local_score = score_evidence_candidate(candidate, &plan);
        candidate.final_score = candidate.local_score * 0.85 + candidate.route_score * 0.15;
    }
    candidates.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut out = Vec::new();
    let mut used_chars = 0usize;
    let mut seen_passages = HashSet::new();
    for mut candidate in candidates {
        if out.len() >= limit || used_chars >= max_chars {
            break;
        }
        if let Some(pid) = candidate.passage_id {
            if !seen_passages.insert(pid) {
                continue;
            }
        }

        let remaining = max_chars.saturating_sub(used_chars);
        if candidate.content.chars().count() > remaining {
            if let Some(excerpt) =
                query_focused_excerpt_details(&candidate.content, &plan.core_terms, remaining)
            {
                let base_start = candidate.source_start.unwrap_or(0);
                candidate.source_start = Some(base_start + excerpt.start as i64);
                candidate.source_end = Some(base_start + excerpt.end as i64);
                candidate.content = excerpt.text;
            } else {
                // 兜底截断：当 core_terms 为空或无法定位时（如查询只有弱词），
                // 直接截取前 remaining 字符，避免完整 passage 超出 max_chars。
                // remaining 足够容纳省略号时预留 3 字符预算；过小则不追加省略号
                let (budget, add_ellipsis) = if remaining > 3 {
                    (remaining - 3, true)
                } else {
                    (remaining.max(1), false)
                };
                let truncated: String = candidate.content.chars().take(budget).collect();
                let base_start = candidate.source_start.unwrap_or(0);
                candidate.source_start = Some(base_start);
                candidate.source_end = Some(base_start + truncated.chars().count() as i64);
                candidate.content = if add_ellipsis {
                    format!("{}...", truncated)
                } else {
                    truncated
                };
            }
        }
        // 分隔符预算：最终 join 用 "\n\n---\n\n"（7字符），此处按 8 预留（含安全余量）
        used_chars += candidate.content.chars().count() + 8;
        out.push(FocusedContentCandidate {
            snippet: candidate.content,
            score: candidate.final_score.max(candidate.local_score),
            kb_document_id: Some(candidate.kb_document_id),
            passage_id: candidate.passage_id,
            view_type: Some(candidate.view_type),
            source_start: candidate.source_start,
            source_end: candidate.source_end,
        });
    }

    Ok(out)
}

#[derive(Debug, Clone)]
struct EvidenceQueryPlan {
    core_terms: Vec<String>,
    weak_terms: Vec<String>,
    relaxed_match: String,
    strict_match: Option<String>,
}

impl EvidenceQueryPlan {
    fn from_query(query: &str) -> Self {
        let relaxed_match = crate::nlp::chinese::build_fts_match_query(query);
        let original_lower = query.to_lowercase();
        let mut seen = HashSet::new();
        let mut core_terms = Vec::new();
        let mut weak_terms = Vec::new();

        for token in crate::nlp::chinese::tokenize_content(query).split_whitespace() {
            let token = crate::nlp::chinese::normalize_token(token);
            if token.is_empty() || !seen.insert(token.clone()) {
                continue;
            }
            if is_weak_query_token(&token) {
                weak_terms.push(token);
                continue;
            }
            let has_chinese = crate::nlp::chinese::has_chinese(&token);
            let appears_as_ascii = !has_chinese && original_lower.contains(&token);
            if has_chinese || appears_as_ascii || is_domain_abbreviation(&token) {
                let char_len = token.chars().count();
                if char_len >= 2 || is_domain_abbreviation(&token) {
                    core_terms.push(token);
                }
            }
        }

        let strict_match = if core_terms.len() >= 2 {
            Some(build_and_match_query(&core_terms))
        } else {
            None
        };

        Self {
            core_terms,
            weak_terms,
            relaxed_match,
            strict_match,
        }
    }

    fn core_query(&self) -> Option<String> {
        (!self.core_terms.is_empty()).then(|| self.core_terms.join(" "))
    }

    /// # 问题 #1 + #13 修复
    /// 新增 `library_id` 参数用于按库查找同义词。
    /// 使用 `batch_lookup_token_synonyms` 一次 SQL 查询替代 N+1 round-trip。
    fn expanded_core_terms(&self, conn: &Connection, library_id: Option<i64>) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut terms = Vec::new();

        // #13: 批量查找同义词 — 一次 SQL 查询替代逐个 token 调用
        let synonym_map = crate::kb::synonyms::batch_lookup_token_synonyms(
            conn,
            &self.core_terms,
            crate::kb::synonyms::MAX_RUNTIME_SYNONYMS,
            library_id,
        );

        for term in &self.core_terms {
            if seen.insert(term.clone()) {
                terms.push(term.clone());
            }
            if let Some(synonyms) = synonym_map.get(term) {
                for syn in synonyms {
                    if seen.insert(syn.clone()) {
                        terms.push(syn.clone());
                    }
                }
            }
        }
        terms
    }
}

#[derive(Debug, Clone)]
pub(crate) struct QueryFallbackPlan {
    pub core_terms: Vec<String>,
    pub core_query: Option<String>,
    pub expanded_query: Option<String>,
    pub expanded_display_query: Option<String>,
}

/// # 问题 #1 修复：新增 `library_id` 参数
/// 用于按库查找 active embedding index，避免跨库索引不一致。
pub(crate) fn build_query_fallback_plan(
    query: &str,
    conn: &Connection,
    library_id: Option<i64>,
) -> QueryFallbackPlan {
    let plan = EvidenceQueryPlan::from_query(query);
    let expanded_terms = plan.expanded_core_terms(conn, library_id);
    QueryFallbackPlan {
        core_terms: plan.core_terms.clone(),
        core_query: plan.core_query(),
        expanded_query: (!expanded_terms.is_empty()).then(|| expanded_terms.join(" ")),
        expanded_display_query: (!expanded_terms.is_empty()).then(|| expanded_terms.join(" OR ")),
    }
}

#[derive(Debug, Clone)]
struct EvidenceCandidate {
    candidate_id: i64,
    passage_id: Option<i64>,
    kb_document_id: i64,
    library_id: i64,
    title: String,
    content: String,
    level: i64,
    artifact_id: i64,
    view_type: String,
    source_start: Option<i64>,
    source_end: Option<i64>,
    was_truncated: bool,
    route_score: f64,
    local_score: f64,
    final_score: f64,
}

impl EvidenceCandidate {
    fn into_hit(
        self,
        conn: &Connection,
        query: &str,
        filter_slug: Option<&str>,
        plan: &EvidenceQueryPlan,
    ) -> Result<EvidenceHit> {
        let artifact = if self.artifact_id > 0 {
            super::store::find_artifact_by_id(conn, self.artifact_id)
                .ok()
                .flatten()
        } else {
            None
        };

        // 规范化 filter_slug：去掉 slug: 前缀，避免带前缀的原始值写入 shadow_page_slug
        // 导致后续 slug_matches_filter 比较时 candidate 侧也需额外规范化
        let normalized_filter_slug = filter_slug.map(|s| {
            slug_value_variants(s)
                .into_iter()
                .next()
                .unwrap_or_else(|| s.to_string())
        });
        let shadow_page_slug = normalized_filter_slug.or_else(|| {
            artifact
                .as_ref()
                .and_then(|a| projection::find_shadow_page_slug(conn, a.id).ok().flatten())
        });

        let matched_terms = matched_terms(&self.content, &self.title, plan);

        // 生成 snippet 并同步调整 source_start/source_end，
        // 使偏移量与实际片段对应，避免后续高亮/定位偏移
        let snippet_excerpt = query_centered_snippet_ex(&self.content, query, &plan.core_terms);
        let snippet_source_start = self
            .source_start
            .map(|base| base + snippet_excerpt.start as i64);
        let snippet_source_end = self
            .source_start
            .map(|base| base + snippet_excerpt.end as i64);

        Ok(EvidenceHit {
            kb_document_id: self.kb_document_id,
            title: self.title,
            snippet: snippet_excerpt.text,
            relevance: self.final_score.max(self.local_score).max(self.route_score),
            matched_terms,
            passage_id: self.passage_id,
            view_type: Some(self.view_type),
            source_start: snippet_source_start,
            source_end: snippet_source_end,
            needs_more_context: self.was_truncated
                || self.content.chars().count() > EVIDENCE_SNIPPET_CHARS,
            // P3: 上下文扩展字段，稍后由 expand_evidence_context 填充
            context_before: None,
            context_after: None,
            artifact,
            shadow_page_slug,
            projections: Vec::new(),
        })
    }
}

// TODO(m-4): query_node_candidates 和 query_passage_candidates 的整体结构高度相似：
// 1. 空 query 检查 → 2. 根据 filter_slug 选择 SQL → 3. prepare + query_map → 4. 结果遍历。
// 差异点：
//   - FTS 表不同：kb_doc_fts vs kb_passage_fts
//   - SQL 列数不同：node 返回 10 列（不含 node_id），passage 返回 12 列
//   - candidate_id 语义不同：node 用 -node_id，passage 直接用 passage_id
//   - passage_id 字段不同：node 为 None，passage 为 Some(passage_id)
//   - view_type 来源不同：node 固定 "node"，passage 来自 ps.view_type 列
// 重构方向：提取辅助函数 `query_fts_candidates(conn, sql_with_slug, sql_without_slug,
//   fts_query, fetch_k, filter_slug, route_weight, map_fn)`，
//   将 SQL 选择、参数绑定、row 遍历逻辑集中，两个上层函数只负责构造 SQL 和 map_row 闭包。
// 预计可减少约 40 行重复代码。当前保持独立是因为 SQL 和 row mapping 的差异散布多处，
// 需要设计合适的闭包签名来避免引入新的泛型复杂度。
fn query_node_candidates(
    conn: &Connection,
    fts_query: &str,
    fetch_k: usize,
    filter_slug: Option<&str>,
    route_weight: f64,
) -> Result<Vec<EvidenceCandidate>> {
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let sql = if filter_slug.is_some() {
        "SELECT dn.id, dn.document_id, dn.library_id, dn.content, dn.level,
            d.original_name, d.title, ap_kb.artifact_id, dn.source_start, dn.source_end,
            bm25(kb_doc_fts) AS bm25_score
         FROM kb_doc_fts fts
         JOIN kb_document_nodes dn ON dn.id = fts.rowid
         JOIN kb_documents d ON d.id = dn.document_id
         JOIN artifact_projections ap_kb ON ap_kb.projection_type = 'kb_document'
              AND ap_kb.projection_ref = 'kb_document:' || dn.document_id
              AND ap_kb.status = 'active'
         JOIN artifact_projections ap_sp ON ap_sp.artifact_id = ap_kb.artifact_id
              AND ap_sp.projection_type = 'brain_shadow_page'
               AND (ap_sp.projection_ref = ?3 OR ap_sp.projection_ref = ?4)
              AND ap_sp.status = 'active'
         WHERE kb_doc_fts MATCH ?1
           AND d.document_status != 'deleted'
           AND d.deleted_at IS NULL
           AND d.current_version_id IS NOT NULL
           AND dn.version_id = d.current_version_id
           AND dn.retired_at IS NULL
           AND d.index_status = 'ready'
         ORDER BY bm25_score ASC
         LIMIT ?2"
    } else {
        "SELECT dn.id, dn.document_id, dn.library_id, dn.content, dn.level,
            d.original_name, d.title, 0 AS artifact_id, dn.source_start, dn.source_end,
            bm25(kb_doc_fts) AS bm25_score
         FROM kb_doc_fts fts
         JOIN kb_document_nodes dn ON dn.id = fts.rowid
         JOIN kb_documents d ON d.id = dn.document_id
         WHERE kb_doc_fts MATCH ?1
           AND d.document_status != 'deleted'
           AND d.deleted_at IS NULL
           AND d.current_version_id IS NOT NULL
           AND dn.version_id = d.current_version_id
           AND dn.retired_at IS NULL
           AND d.index_status = 'ready'
         ORDER BY bm25_score ASC
         LIMIT ?2"
    };

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| GBrainError::Database(format!("准备 KB node 搜索失败: {}", e)))?;

    let map_row = |rank: usize, row: &rusqlite::Row<'_>| -> rusqlite::Result<EvidenceCandidate> {
        let node_id: i64 = row.get(0)?;
        let kb_document_id: i64 = row.get(1)?;
        let library_id: i64 = row.get(2)?;
        let content: String = row.get(3)?;
        let level: i64 = row.get(4)?;
        let original_name: String = row.get(5)?;
        let doc_title: String = row.get(6)?;
        let artifact_id: i64 = row.get(7)?;
        let source_start: Option<i64> = row.get(8)?;
        let source_end: Option<i64> = row.get(9)?;
        let title = if doc_title.is_empty() {
            original_name
        } else {
            doc_title
        };
        Ok(EvidenceCandidate {
            candidate_id: -node_id,
            passage_id: None,
            kb_document_id,
            library_id,
            title,
            content,
            level,
            artifact_id,
            view_type: "node".to_string(),
            source_start,
            source_end,
            was_truncated: false,
            route_score: route_weight / (60.0 + rank as f64 + 1.0),
            local_score: 0.0,
            final_score: 0.0,
        })
    };

    let mut out = Vec::new();
    if let Some(slug) = filter_slug {
        let (slug_ref, documents_slug_ref) = slug_ref_variants(slug);
        let mut rows = stmt
            .query(params![
                fts_query,
                fetch_k as i64,
                slug_ref,
                documents_slug_ref
            ])
            .map_err(|e| GBrainError::Database(format!("KB node 搜索失败: {}", e)))?;
        let mut rank = 0;
        while let Some(row) = rows.next()? {
            out.push(map_row(rank, row)?);
            rank += 1;
        }
    } else {
        let mut rows = stmt
            .query(params![fts_query, fetch_k as i64])
            .map_err(|e| GBrainError::Database(format!("KB node 搜索失败: {}", e)))?;
        let mut rank = 0;
        while let Some(row) = rows.next()? {
            out.push(map_row(rank, row)?);
            rank += 1;
        }
    }
    Ok(out)
}

fn query_passage_candidates(
    conn: &Connection,
    fts_query: &str,
    fetch_k: usize,
    filter_slug: Option<&str>,
    route_weight: f64,
) -> Result<Vec<EvidenceCandidate>> {
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let sql = if filter_slug.is_some() {
        "SELECT ps.id, ps.node_id, ps.document_id, ps.library_id, ps.content,
            ps.view_type, dn.level, d.original_name, d.title, ps.source_start, ps.source_end,
            ap_kb.artifact_id, bm25(kb_passage_fts) AS bm25_score
         FROM kb_passage_fts fts
         JOIN kb_passage_spans ps ON ps.id = fts.rowid
         JOIN kb_document_nodes dn ON dn.id = ps.node_id
         JOIN kb_documents d ON d.id = ps.document_id
         JOIN artifact_projections ap_kb ON ap_kb.projection_type = 'kb_document'
              AND ap_kb.projection_ref = 'kb_document:' || ps.document_id
              AND ap_kb.status = 'active'
         JOIN artifact_projections ap_sp ON ap_sp.artifact_id = ap_kb.artifact_id
              AND ap_sp.projection_type = 'brain_shadow_page'
               AND (ap_sp.projection_ref = ?3 OR ap_sp.projection_ref = ?4)
              AND ap_sp.status = 'active'
         WHERE kb_passage_fts MATCH ?1
           AND d.document_status != 'deleted'
           AND d.deleted_at IS NULL
           AND d.current_version_id IS NOT NULL
           AND dn.version_id = d.current_version_id
           AND dn.retired_at IS NULL
           AND d.index_status = 'ready'
         ORDER BY bm25_score ASC
         LIMIT ?2"
    } else {
        "SELECT ps.id, ps.node_id, ps.document_id, ps.library_id, ps.content,
            ps.view_type, dn.level, d.original_name, d.title, ps.source_start, ps.source_end,
            0 AS artifact_id, bm25(kb_passage_fts) AS bm25_score
         FROM kb_passage_fts fts
         JOIN kb_passage_spans ps ON ps.id = fts.rowid
         JOIN kb_document_nodes dn ON dn.id = ps.node_id
         JOIN kb_documents d ON d.id = ps.document_id
         WHERE kb_passage_fts MATCH ?1
           AND d.document_status != 'deleted'
           AND d.deleted_at IS NULL
           AND d.current_version_id IS NOT NULL
           AND dn.version_id = d.current_version_id
           AND dn.retired_at IS NULL
           AND d.index_status = 'ready'
         ORDER BY bm25_score ASC
         LIMIT ?2"
    };

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| GBrainError::Database(format!("准备 KB passage 搜索失败: {}", e)))?;

    let map_row = |rank: usize, row: &rusqlite::Row<'_>| -> rusqlite::Result<EvidenceCandidate> {
        let passage_id: i64 = row.get(0)?;
        let _node_id: i64 = row.get(1)?;
        let kb_document_id: i64 = row.get(2)?;
        let library_id: i64 = row.get(3)?;
        let content: String = row.get(4)?;
        let view_type: String = row.get(5)?;
        let level: i64 = row.get(6)?;
        let original_name: String = row.get(7)?;
        let doc_title: String = row.get(8)?;
        let source_start: Option<i64> = row.get(9)?;
        let source_end: Option<i64> = row.get(10)?;
        let artifact_id: i64 = row.get(11)?;
        let title = if doc_title.is_empty() {
            original_name
        } else {
            doc_title
        };
        Ok(EvidenceCandidate {
            candidate_id: passage_id,
            passage_id: Some(passage_id),
            kb_document_id,
            library_id,
            title,
            content,
            level,
            artifact_id,
            view_type,
            source_start,
            source_end,
            was_truncated: false,
            route_score: route_weight / (60.0 + rank as f64 + 1.0),
            local_score: 0.0,
            final_score: 0.0,
        })
    };

    let mut out = Vec::new();
    if let Some(slug) = filter_slug {
        let (slug_ref, documents_slug_ref) = slug_ref_variants(slug);
        let mut rows = stmt
            .query(params![
                fts_query,
                fetch_k as i64,
                slug_ref,
                documents_slug_ref
            ])
            .map_err(|e| GBrainError::Database(format!("KB passage 搜索失败: {}", e)))?;
        let mut rank = 0;
        while let Some(row) = rows.next()? {
            out.push(map_row(rank, row)?);
            rank += 1;
        }
    } else {
        let mut rows = stmt
            .query(params![fts_query, fetch_k as i64])
            .map_err(|e| GBrainError::Database(format!("KB passage 搜索失败: {}", e)))?;
        let mut rank = 0;
        while let Some(row) = rows.next()? {
            out.push(map_row(rank, row)?);
            rank += 1;
        }
    }
    Ok(out)
}

fn query_kb_hybrid_candidates(
    conn: &Connection,
    query: &str,
    fetch_k: usize,
    filter_slug: Option<&str>,
    route_weight: f64,
) -> Result<Vec<EvidenceCandidate>> {
    let document_filter = if let Some(slug) = filter_slug {
        active_kb_document_ids_for_slug(conn, slug)?
    } else {
        Vec::new()
    };
    if filter_slug.is_some() && document_filter.is_empty() {
        return Ok(Vec::new());
    }
    let document_filter_set: HashSet<i64> = document_filter.iter().copied().collect();
    let library_ids = if document_filter.is_empty() {
        Vec::new()
    } else {
        library_ids_for_documents(conn, &document_filter)?
    };

    let input = crate::kb::types::KbSearchInput {
        library_ids,
        document_ids: document_filter,
        query: query.to_string(),
        top_k: fetch_k.clamp(20, 500),
        profile: Some("recall".to_string()),
        max_chunks_per_doc: Some(3),
        ..Default::default()
    };

    let results = crate::kb::search::kb_search(conn, &input, None)?;
    let mut out = Vec::new();
    for (rank, result) in results.into_iter().enumerate() {
        if !document_filter_set.is_empty() && !document_filter_set.contains(&result.document_id) {
            continue;
        }
        let (source_start, source_end) = node_source_span(conn, result.node_id);
        let artifact_id = artifact_id_for_kb_document(conn, result.document_id, filter_slug);
        out.push(EvidenceCandidate {
            candidate_id: -result.node_id,
            passage_id: None,
            kb_document_id: result.document_id,
            library_id: result.library_id,
            title: result.document_name,
            content: result.content,
            level: result.level as i64,
            artifact_id,
            view_type: result.matched_by.unwrap_or_else(|| "kb_hybrid".to_string()),
            source_start,
            source_end,
            was_truncated: false,
            route_score: route_weight / (60.0 + rank as f64 + 1.0),
            local_score: 0.0,
            final_score: 0.0,
        });
    }
    Ok(out)
}

fn active_kb_document_ids_for_slug(conn: &Connection, slug: &str) -> Result<Vec<i64>> {
    let (slug_ref, documents_slug_ref) = slug_ref_variants(slug);
    let mut stmt = conn.prepare(
        "SELECT DISTINCT CAST(substr(ap_kb.projection_ref, length('kb_document:') + 1) AS INTEGER)
         FROM artifact_projections ap_sp
         JOIN artifact_projections ap_kb ON ap_kb.artifact_id = ap_sp.artifact_id
              AND ap_kb.projection_type = 'kb_document'
              AND ap_kb.projection_ref LIKE 'kb_document:%'
              AND ap_kb.status = 'active'
         JOIN kb_documents d
              ON d.id = CAST(substr(ap_kb.projection_ref, length('kb_document:') + 1) AS INTEGER)
         WHERE ap_sp.projection_type = 'brain_shadow_page'
           AND (ap_sp.projection_ref = ?1 OR ap_sp.projection_ref = ?2)
           AND ap_sp.status = 'active'
           AND d.document_status != 'deleted'
           AND d.deleted_at IS NULL
           AND d.index_status = 'ready'",
    )?;
    let rows = stmt.query_map(params![slug_ref, documents_slug_ref], |row| {
        row.get::<_, i64>(0)
    })?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn library_ids_for_documents(conn: &Connection, document_ids: &[i64]) -> Result<Vec<i64>> {
    if document_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: Vec<String> = document_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect();
    let sql = format!(
        "SELECT DISTINCT library_id FROM kb_documents WHERE id IN ({})",
        placeholders.join(",")
    );
    let params: Vec<&dyn rusqlite::types::ToSql> = document_ids
        .iter()
        .map(|id| id as &dyn rusqlite::types::ToSql)
        .collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, i64>(0))?;
    Ok(rows.filter_map(|r| r.ok()).collect())
}

fn node_source_span(conn: &Connection, node_id: i64) -> (Option<i64>, Option<i64>) {
    conn.query_row(
        "SELECT source_start, source_end FROM kb_document_nodes WHERE id = ?1",
        params![node_id],
        |row| {
            Ok((
                row.get::<_, Option<i32>>(0)?.map(i64::from),
                row.get::<_, Option<i32>>(1)?.map(i64::from),
            ))
        },
    )
    .unwrap_or((None, None))
}

fn artifact_id_for_kb_document(
    conn: &Connection,
    document_id: i64,
    filter_slug: Option<&str>,
) -> i64 {
    if let Some(slug) = filter_slug {
        let (slug_ref, documents_slug_ref) = slug_ref_variants(slug);
        return conn
            .query_row(
                "SELECT ap_kb.artifact_id
                 FROM artifact_projections ap_kb
                 JOIN artifact_projections ap_sp ON ap_sp.artifact_id = ap_kb.artifact_id
                      AND ap_sp.projection_type = 'brain_shadow_page'
                      AND (ap_sp.projection_ref = ?2 OR ap_sp.projection_ref = ?3)
                      AND ap_sp.status = 'active'
                 WHERE ap_kb.projection_type = 'kb_document'
                   AND ap_kb.projection_ref = ?1
                   AND ap_kb.status = 'active'
                 LIMIT 1",
                params![
                    format!("kb_document:{}", document_id),
                    slug_ref,
                    documents_slug_ref
                ],
                |row| row.get::<_, i64>(0),
            )
            .unwrap_or(0);
    }

    conn.query_row(
        "SELECT artifact_id FROM artifact_projections
         WHERE projection_type = 'kb_document'
           AND projection_ref = ?1
           AND status = 'active'
         LIMIT 1",
        params![format!("kb_document:{}", document_id)],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
}

fn active_kb_document_ids_for_artifact(conn: &Connection, artifact_id: i64) -> Result<Vec<i64>> {
    let mut stmt = conn
        .prepare(
            "SELECT CAST(substr(projection_ref, length('kb_document:') + 1) AS INTEGER)
             FROM artifact_projections
             WHERE artifact_id = ?1
               AND projection_type = 'kb_document'
               AND projection_ref LIKE 'kb_document:%'
               AND status = 'active'",
        )
        .map_err(|e| GBrainError::Database(format!("准备 artifact KB 文档查询失败: {}", e)))?;
    let rows = stmt
        .query_map(params![artifact_id], |row| row.get::<_, i64>(0))
        .map_err(|e| GBrainError::Database(format!("查询 artifact KB 文档失败: {}", e)))?;
    let mut ids = Vec::new();
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

fn load_focused_passage_by_id(
    conn: &Connection,
    document_ids: &[i64],
    passage_id: i64,
    query: Option<&str>,
    max_chars: usize,
) -> Result<Vec<FocusedContentCandidate>> {
    // 修复：直接通过 passage_id 读取时必须像常规 passage 搜索那样过滤已软删的 KB 文档，
    // 否则即便 artifact projection 仍记录某文档为 active，软删后该文档的 passage 仍可被读到，
    // 与常规 focused 检索路径行为不一致，造成"幽灵证据"。
    let mut stmt = conn
        .prepare(
            "SELECT ps.id, ps.document_id, ps.content, ps.view_type, \
                    ps.source_start, ps.source_end, ps.quality_score \
             FROM kb_passage_spans ps \
             JOIN kb_documents d ON d.id = ps.document_id \
             WHERE ps.id = ?1 \
               AND d.document_status != 'deleted' \
               AND d.deleted_at IS NULL",
        )
        .map_err(|e| GBrainError::Database(format!("准备 KB passage 读取失败: {}", e)))?;
    let row = stmt.query_row(params![passage_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<i64>>(4)?,
            row.get::<_, Option<i64>>(5)?,
            row.get::<_, f64>(6)?,
        ))
    });

    let Ok((pid, document_id, mut content, view_type, mut source_start, mut source_end, score)) =
        row
    else {
        return Ok(Vec::new());
    };
    if !document_ids.contains(&document_id) {
        return Ok(Vec::new());
    }

    let terms = query
        .map(EvidenceQueryPlan::from_query)
        .map(|plan| plan.core_terms)
        .unwrap_or_default();
    if content.chars().count() > max_chars {
        if let Some(excerpt) = query_focused_excerpt_details(&content, &terms, max_chars) {
            let base_start = source_start.unwrap_or(0);
            source_start = Some(base_start + excerpt.start as i64);
            source_end = Some(base_start + excerpt.end as i64);
            content = excerpt.text;
        } else {
            // 兜底截断：预留 3 字符给后缀省略号，确保不超 max_chars
            let budget = max_chars.saturating_sub(3).max(1);
            let truncated: String = content.chars().take(budget).collect();
            source_start = source_start.or(Some(0));
            source_end = source_start.map(|start| start + truncated.chars().count() as i64);
            content = format!("{}...", truncated);
        }
    }

    Ok(vec![FocusedContentCandidate {
        snippet: content,
        score,
        kb_document_id: Some(document_id),
        passage_id: Some(pid),
        view_type: Some(view_type),
        source_start,
        source_end,
    }])
}

fn query_passage_candidates_for_document_ids(
    conn: &Connection,
    fts_query: &str,
    document_ids: &[i64],
    fetch_k: usize,
    artifact_id: i64,
    route_weight: f64,
) -> Result<Vec<EvidenceCandidate>> {
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for document_id in document_ids {
        let mut stmt = conn
            .prepare(
                "SELECT ps.id, ps.node_id, ps.document_id, ps.library_id, ps.content,
                    ps.view_type, dn.level, d.original_name, d.title, ps.source_start, ps.source_end,
                    bm25(kb_passage_fts) AS bm25_score
                 FROM kb_passage_fts fts
                 JOIN kb_passage_spans ps ON ps.id = fts.rowid
                 JOIN kb_document_nodes dn ON dn.id = ps.node_id
                 JOIN kb_documents d ON d.id = ps.document_id
                 WHERE kb_passage_fts MATCH ?1
                   AND ps.document_id = ?2
                   AND d.document_status != 'deleted'
                   AND d.deleted_at IS NULL
                   AND d.current_version_id IS NOT NULL
                   AND dn.version_id = d.current_version_id
                   AND dn.retired_at IS NULL
                   AND d.index_status = 'ready'
                 ORDER BY bm25_score ASC
                 LIMIT ?3",
            )
            .map_err(|e| {
                GBrainError::Database(format!("准备 artifact focused passage 搜索失败: {}", e))
            })?;
        let mut rows = stmt
            .query(params![fts_query, *document_id, fetch_k as i64])
            .map_err(|e| GBrainError::Database(format!("artifact focused 搜索失败: {}", e)))?;
        let mut rank = 0;
        while let Some(row) = rows.next()? {
            let passage_id: i64 = row.get(0)?;
            let kb_document_id: i64 = row.get(2)?;
            let library_id: i64 = row.get(3)?;
            let content: String = row.get(4)?;
            let view_type: String = row.get(5)?;
            let level: i64 = row.get(6)?;
            let original_name: String = row.get(7)?;
            let doc_title: String = row.get(8)?;
            let source_start: Option<i64> = row.get(9)?;
            let source_end: Option<i64> = row.get(10)?;
            let title = if doc_title.is_empty() {
                original_name
            } else {
                doc_title
            };
            out.push(EvidenceCandidate {
                candidate_id: passage_id,
                passage_id: Some(passage_id),
                kb_document_id,
                library_id,
                title,
                content,
                level,
                artifact_id,
                view_type,
                source_start,
                source_end,
                was_truncated: false,
                route_score: route_weight / (60.0 + rank as f64 + 1.0),
                local_score: 0.0,
                final_score: 0.0,
            });
            rank += 1;
        }
    }
    Ok(out)
}

fn ensure_passages_for_candidates(
    conn: &Connection,
    candidates: &[EvidenceCandidate],
) -> Result<()> {
    let mut docs = HashSet::new();
    for candidate in candidates {
        docs.insert(candidate.kb_document_id);
    }
    for doc_id in docs {
        crate::kb::passage::ensure_document_passages(conn, doc_id)?;
    }
    Ok(())
}

fn merge_evidence_routes(routes: Vec<Vec<EvidenceCandidate>>) -> Vec<EvidenceCandidate> {
    let mut merged: HashMap<i64, EvidenceCandidate> = HashMap::new();
    for route in routes {
        for candidate in route {
            merged
                .entry(candidate.candidate_id)
                .and_modify(|existing| {
                    existing.route_score += candidate.route_score;
                    if candidate.content.chars().count() < existing.content.chars().count()
                        && candidate.passage_id.is_some()
                    {
                        existing.content = candidate.content.clone();
                        existing.view_type = candidate.view_type.clone();
                        existing.passage_id = candidate.passage_id;
                        existing.source_start = candidate.source_start;
                        existing.source_end = candidate.source_end;
                    }
                })
                .or_insert(candidate);
        }
    }
    let mut out: Vec<EvidenceCandidate> = merged.into_values().collect();
    out.sort_by(|a, b| {
        b.route_score
            .partial_cmp(&a.route_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// C-3 修复：按文档保留 top-N 个候选，而非只保留第一个。
/// 同一文档可能有多个不同 passage 都命中查询关键词，全部丢弃会丢失有价值的证据。
/// max_per_doc 控制每个文档最多保留的候选数量，推荐值 3。
fn dedup_evidence_by_document(
    candidates: Vec<EvidenceCandidate>,
    max_per_doc: usize,
) -> Vec<EvidenceCandidate> {
    let mut counts: HashMap<i64, usize> = HashMap::new();
    candidates
        .into_iter()
        .filter(|c| {
            let count = counts.entry(c.kb_document_id).or_insert(0usize);
            if *count < max_per_doc {
                *count += 1;
                true
            } else {
                false
            }
        })
        .collect()
}

fn focus_evidence_candidate_content(candidate: &mut EvidenceCandidate, plan: &EvidenceQueryPlan) {
    if candidate.content.chars().count() <= RERANK_EXCERPT_CHARS {
        return;
    }
    if let Some(excerpt) =
        query_focused_excerpt_details(&candidate.content, &plan.core_terms, RERANK_EXCERPT_CHARS)
    {
        let base_start = candidate.source_start.unwrap_or(0);
        candidate.source_start = Some(base_start + excerpt.start as i64);
        candidate.source_end = Some(base_start + excerpt.end as i64);
        candidate.was_truncated = excerpt.truncated;
        // 使用不含省略号的纯文本，避免后续 snippet 偏移被人工字符污染
        candidate.content = excerpt.raw_text;
    }
}

fn rerank_evidence_candidates(
    conn: &Connection,
    query: &str,
    mut candidates: Vec<EvidenceCandidate>,
    plan: &EvidenceQueryPlan,
    config: &Config,
) -> Result<Vec<EvidenceCandidate>> {
    candidates.sort_by(|a, b| {
        let b_score = b.local_score + b.route_score * 5.0;
        let a_score = a.local_score + a.route_score * 5.0;
        b_score
            .partial_cmp(&a_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // P1: 读取库级 rerank_enabled / rerank_provider，与 kb_search 策略保持一致
    // 外部 rerank 始终允许，脱敏始终关闭
    let (_, _, lib_rerank_enabled, rerank_provider_str) =
        resolve_evidence_rerank_policy(conn, &candidates)?;
    let rerank_cfg = crate::kb::rerank::RerankConfig {
        model_rerank_enabled: lib_rerank_enabled,
        rerank_provider: if rerank_provider_str.is_empty() {
            "chat_completions".to_string()
        } else {
            rerank_provider_str
        },
        rerank_model: config.expansion_model.clone(),
        rerank_timeout_ms: 5000,
        rerank_max_candidates: 50,
    };

    let local_candidates: Vec<(i64, crate::kb::rerank::LocalRankSignals)> = candidates
        .iter()
        .map(|c| {
            (
                c.candidate_id,
                crate::kb::rerank::LocalRankSignals {
                    fts_score: c.route_score,
                    exact_match_score: c.local_score,
                    granularity_score: view_type_score(&c.view_type),
                    ..Default::default()
                },
            )
        })
        .collect();

    let candidate_texts: Vec<crate::kb::rerank::RerankCandidate> = candidates
        .iter()
        .take(rerank_cfg.rerank_max_candidates)
        .map(|c| {
            // 脱敏已关闭，直接使用原文
            crate::kb::rerank::RerankCandidate {
                doc_id: c.candidate_id,
                text: c.content.clone(),
            }
        })
        .collect();

    let api_key = config.expansion_api_key_resolved().unwrap_or("");
    let base_url = config
        .expansion_base_url_resolved()
        .filter(|s| !s.is_empty())
        .unwrap_or("");
    // P2 修复：与 KB search (kb::search) 保持一致——只有 api_key 与 base_url 都非空时
    // 才调用模型 rerank。否则 try_model_rerank_simple 在“有 key 但缺 base_url”时会走
    // fallback，并把 model_rerank_attempted 标为 true，导致下方误写“已发起外部请求”的
    // 外部调用审计。这里缺 base_url 时直接本地 rerank，并标记 attempted=false。
    let has_api_key = !api_key.trim().is_empty();
    let has_base_url = !base_url.trim().is_empty();
    let weights = vec![0.25, 0.0, 0.0, 0.65, 0.0, 0.0];
    // H3 fix: 使用全局共享运行时，避免每次证据重排创建新运行时
    let rt = crate::runtime::shared_runtime();
    let (scored, rerank_info) = if rerank_cfg.model_rerank_enabled && has_api_key && has_base_url {
        rt.block_on(crate::kb::rerank::try_model_rerank_simple(
            &rerank_cfg,
            query,
            &local_candidates,
            &candidate_texts,
            &weights,
            None,
            base_url,
            api_key,
        ))
    } else {
        // 未配置 api_key 或 base_url：未实际发起外部请求，直接本地 rerank，
        // 标记 model_rerank_attempted=false，避免误写外部调用审计。
        (
            crate::kb::rerank::local_rerank(&local_candidates, &weights),
            crate::kb::rerank::RerankResult {
                model_rerank_attempted: false,
                model_rerank_succeeded: false,
                fallback_used: true,
                fallback_reason: Some(crate::kb::rerank::FallbackReason::NotConfigured),
                provider: "local".into(),
                candidates_reranked: local_candidates.len(),
            },
        )
    };

    // P2+P3: 在调用完成后按实际结果写审计，每个涉及的库独立记录一条
    let should_audit = rerank_cfg.model_rerank_enabled
        && !api_key.is_empty()
        && rerank_info.model_rerank_attempted;
    if should_audit {
        // 仅收集实际发给外部模型的候选库（与 candidate_texts 截断一致）
        let distinct_library_ids: HashSet<i64> = candidates
            .iter()
            .take(rerank_cfg.rerank_max_candidates)
            .map(|c| c.library_id)
            .collect();
        let succeeded = rerank_info.model_rerank_succeeded;
        let error_msg = if succeeded {
            ""
        } else {
            rerank_info
                .fallback_reason
                .as_ref()
                .map(|r| r.as_str())
                .unwrap_or("unknown")
        };
        for lib_id in distinct_library_ids {
            let _ = crate::kb::privacy::log_external_model_call(
                conn,
                Some(lib_id),
                None,
                "rerank",
                &rerank_cfg.rerank_provider,
                &rerank_cfg.rerank_model,
                query.len() as i32,
                candidate_texts.len() as i32,
                0,
                0.0,
                succeeded,
                error_msg,
            );
        }
    }

    let model_succeeded = rerank_info.model_rerank_succeeded;
    let score_by_id: HashMap<i64, f64> = scored.into_iter().collect();
    let (local_min, local_max) = score_bounds(candidates.iter().map(|c| c.local_score));
    let (route_min, route_max) = score_bounds(candidates.iter().map(|c| c.route_score));
    for candidate in &mut candidates {
        candidate.final_score = if model_succeeded {
            if let Some(model_score) = score_by_id.get(&candidate.candidate_id).copied() {
                let local = normalize_score(candidate.local_score, local_min, local_max);
                let route = normalize_score(candidate.route_score, route_min, route_max);
                model_score.clamp(0.0, 1.0) * 0.75 + local * 0.20 + route * 0.05
            } else {
                // 模型未返回该候选分数（如部分 batch 失败），保留本地排序分
                candidate.local_score * 0.80 + candidate.route_score * 0.20
            }
        } else {
            candidate.local_score * 0.80 + candidate.route_score * 0.20
        };
    }

    candidates.sort_by(|a, b| {
        b.final_score
            .partial_cmp(&a.final_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    apply_quality_guard(&mut candidates, plan);
    Ok(candidates)
}

fn score_bounds(scores: impl Iterator<Item = f64>) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for score in scores {
        if score < min {
            min = score;
        }
        if score > max {
            max = score;
        }
    }
    if min.is_finite() && max.is_finite() {
        (min, max)
    } else {
        (0.0, 0.0)
    }
}

fn normalize_score(score: f64, min: f64, max: f64) -> f64 {
    let width = max - min;
    if width.abs() < f64::EPSILON {
        return if score > 0.0 { 1.0 } else { 0.0 };
    }
    ((score - min) / width).clamp(0.0, 1.0)
}

/// 查询库级 rerank 配置（rerank_enabled, rerank_provider）。
/// 外部 rerank 始终允许，脱敏始终关闭，不再查询已删除的列。
fn resolve_evidence_rerank_policy(
    conn: &Connection,
    candidates: &[EvidenceCandidate],
) -> Result<(bool, bool, bool, String)> {
    // 返回: (外部 rerank 允许=true, 脱敏=false, rerank_enabled, rerank_provider)
    let library_ids: HashSet<i64> = candidates.iter().map(|c| c.library_id).collect();
    if library_ids.is_empty() {
        return Ok((true, false, true, String::new()));
    }

    // 构造 WHERE id IN (?, ?, ...) 子句
    let placeholders: Vec<String> = library_ids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect();
    let sql = format!(
        "SELECT rerank_enabled, rerank_provider \
         FROM kb_libraries WHERE id IN ({})",
        placeholders.join(",")
    );
    let params: Vec<Box<dyn rusqlite::types::ToSql>> = library_ids
        .iter()
        .map(|id| Box::new(*id) as Box<dyn rusqlite::types::ToSql>)
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| GBrainError::Database(format!("准备库策略批量查询失败: {}", e)))?;
    let rows = stmt
        .query_map(param_refs.as_slice(), |row| {
            Ok((row.get::<_, i32>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|e| GBrainError::Database(format!("库策略批量查询失败: {}", e)))?;

    let mut rerank_enabled = true;
    let mut rerank_provider = String::new();
    for row in rows {
        let (re_enabled, re_provider) =
            row.map_err(|e| GBrainError::Database(format!("读取库策略行失败: {}", e)))?;
        // 任一库禁用 rerank 则全局禁用，取最保守策略
        if re_enabled == 0 {
            rerank_enabled = false;
        }
        if rerank_provider.is_empty() && !re_provider.is_empty() {
            rerank_provider = re_provider;
        }
    }
    Ok((true, false, rerank_enabled, rerank_provider))
}

const RERANK_EXCERPT_CHARS: usize = 1200;

#[derive(Debug, Clone)]
struct FocusedExcerpt {
    /// 含前后省略号的展示文本
    text: String,
    /// 不含省略号的纯切片文本，用于后续偏移计算
    raw_text: String,
    start: usize,
    end: usize,
    truncated: bool,
}

fn query_focused_excerpt_details(
    content: &str,
    terms: &[String],
    limit: usize,
) -> Option<FocusedExcerpt> {
    if terms.is_empty() {
        return None;
    }

    // 修复：当 limit 极小（< 7）时，预留 6 字符省略号后会出现 text 长度反而 > limit
    // 的逆转（如 limit=1 时返回 "...x..." 长度 7）。
    // 直接返回 None 让上层走兜底截断路径，由其保证最终长度 <= limit。
    if limit < 7 {
        return None;
    }

    // m-2 注释：folded 使用 to_ascii_lowercase()，该方法只修改 ASCII 字母（a-z → A-Z），
    // 不会改变非 ASCII 字节（包括 UTF-8 多字节字符），因此 UTF-8 字节布局保持不变。
    // 这意味着 folded 上的 str::find() 返回的是字节偏移（byte offset），
    // 后续通过 content[..byte_pos].chars().count() 转换为字符偏移（char offset）。
    // 所有基于 folded.find() 的计算（byte_pos、search_from）都是字节级别的，
    // 而最终 start/end 输出和 chars[] 下标是字符级别的。
    let folded = content.to_ascii_lowercase();
    let chars: Vec<char> = content.chars().collect();
    let content_len = chars.len();
    if content_len <= limit {
        return Some(FocusedExcerpt {
            text: content.to_string(),
            raw_text: content.to_string(),
            start: 0,
            end: content_len,
            truncated: false,
        });
    }

    // 为前后省略号预留空间（前后各 "..." = 3+3 = 6 字符），确保最终 text 不超过 limit
    let budget = limit.saturating_sub(6).max(1);

    let mut positions = Vec::new();
    for term in terms {
        let term = term.to_ascii_lowercase();
        if term.is_empty() {
            continue;
        }

        let mut search_from = 0;
        let mut hits_for_term = 0;
        while search_from < folded.len() && hits_for_term < 32 {
            let Some(relative) = folded[search_from..].find(&term) else {
                break;
            };
            let byte_pos = search_from + relative;
            let char_pos = content[..byte_pos].chars().count();
            positions.push(char_pos);
            search_from = byte_pos + term.len();
            hits_for_term += 1;
        }
    }
    if positions.is_empty() {
        return None;
    }

    let mut best_start = 0usize;
    let mut best_score = f64::MIN;
    for pos in positions {
        let mut start = pos.saturating_sub(budget / 2);
        if start + budget > content_len {
            start = content_len.saturating_sub(budget);
        }
        let end = (start + budget).min(content_len);
        let excerpt: String = chars[start..end].iter().collect();
        let folded_excerpt = excerpt.to_ascii_lowercase();
        let coverage = terms
            .iter()
            .filter(|term| folded_excerpt.contains(&term.to_ascii_lowercase()))
            .count();
        let center = start + (end - start) / 2;
        let distance = center.abs_diff(pos) as f64;
        let score = coverage as f64 * 1000.0 - distance;
        if score > best_score {
            best_score = score;
            best_start = start;
        }
    }

    let end = (best_start + budget).min(content_len);
    let excerpt: String = chars[best_start..end].iter().collect();
    let has_prefix = best_start > 0;
    let has_suffix = end < content_len;
    Some(FocusedExcerpt {
        text: format!(
            "{}{}{}",
            if has_prefix { "..." } else { "" },
            &excerpt,
            if has_suffix { "..." } else { "" }
        ),
        raw_text: excerpt,
        start: best_start,
        end,
        truncated: has_prefix || has_suffix,
    })
}

fn score_evidence_candidate(candidate: &EvidenceCandidate, plan: &EvidenceQueryPlan) -> f64 {
    let content = candidate.content.to_lowercase();
    let title = candidate.title.to_lowercase();
    let mut score = candidate.route_score * 5.0;

    let mut covered = 0usize;
    let mut positions = Vec::new();
    for term in &plan.core_terms {
        let term_lower = term.to_lowercase();
        let content_pos = content.find(&term_lower);
        let title_hit = title.contains(&term_lower);
        if content_pos.is_some() || title_hit {
            covered += 1;
            score += 2.5 + (term.chars().count() as f64 * 0.15).min(0.8);
            if title_hit {
                score += 1.0;
            }
            if let Some(pos) = content_pos {
                positions.push(pos);
            }
        }
    }

    if !plan.core_terms.is_empty() {
        score += 4.0 * (covered as f64 / plan.core_terms.len() as f64);
    }
    if covered >= 2 {
        score += 2.5;
    }
    if covered == plan.core_terms.len() && covered > 0 {
        score += 2.0;
    }
    if let Some(span) = position_span(&positions) {
        if span <= 120 {
            score += 3.0;
        } else if span <= 300 {
            score += 1.5;
        }
    }

    for term in &plan.weak_terms {
        if content.contains(term) || title.contains(term) {
            score += 0.15;
        }
    }

    score += view_type_score(&candidate.view_type);
    score += 1.0 / (candidate.level as f64 + 1.0);

    let len = candidate.content.chars().count();
    if len > 2500 {
        score -= 1.5;
    } else if len <= 900 {
        score += 0.8;
    }

    score.max(0.0)
}

fn apply_quality_guard(candidates: &mut [EvidenceCandidate], plan: &EvidenceQueryPlan) {
    if plan.core_terms.len() < 2 || candidates.len() < 2 {
        return;
    }
    let required = plan.core_terms.len().min(2);
    let top_coverage = core_coverage(&candidates[0].content, &candidates[0].title, plan);
    if top_coverage >= required {
        return;
    }
    if let Some((idx, _)) = candidates
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(_, c)| core_coverage(&c.content, &c.title, plan) >= required)
        .max_by(|(_, a), (_, b)| {
            a.local_score
                .partial_cmp(&b.local_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    {
        candidates.swap(0, idx);
    }
}

fn core_coverage(content: &str, title: &str, plan: &EvidenceQueryPlan) -> usize {
    let content = content.to_lowercase();
    let title = title.to_lowercase();
    plan.core_terms
        .iter()
        .filter(|term| {
            let term = term.to_lowercase();
            content.contains(&term) || title.contains(&term)
        })
        .count()
}

fn position_span(positions: &[usize]) -> Option<usize> {
    if positions.len() < 2 {
        return None;
    }
    let min = positions.iter().min()?;
    let max = positions.iter().max()?;
    Some(max.saturating_sub(*min))
}

fn view_type_score(view_type: &str) -> f64 {
    match view_type {
        "atomic" => 1.4,
        "window" => 1.0,
        "clean" => 0.8,
        "node" => 0.2,
        _ => 0.5,
    }
}

fn build_and_match_query(terms: &[String]) -> String {
    terms
        .iter()
        .take(5)
        .filter_map(|term| {
            let escaped = crate::nlp::chinese::escape_fts5_token(term);
            (!escaped.is_empty()).then(|| format!("{}*", escaped))
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// M53 修复：领域缩写列表 — 集中管理，可在此处扩展或改为从配置加载
const DOMAIN_ABBREVIATIONS: &[&str] = &["lp", "gp", "vi", "api", "sdk"];

fn is_domain_abbreviation(token: &str) -> bool {
    DOMAIN_ABBREVIATIONS.contains(&token)
}

fn matched_terms(content: &str, title: &str, plan: &EvidenceQueryPlan) -> Vec<String> {
    let content = content.to_lowercase();
    let title = title.to_lowercase();
    plan.core_terms
        .iter()
        .filter(|term| {
            let term = term.to_lowercase();
            content.contains(&term) || title.contains(&term)
        })
        .cloned()
        .collect()
}

fn is_weak_query_token(token: &str) -> bool {
    matches!(
        token,
        "请" | "告诉"
            | "告诉我"
            | "我"
            | "你"
            | "一下"
            | "相关"
            | "逻辑"
            | "相关逻辑"
            | "请问"
            | "什么"
            | "怎么"
            | "怎样"
            | "如何"
            | "是否"
            | "有关"
            | "内容"
            | "信息"
            | "的"
            | "是"
            | "和"
            | "与"
            | "及"
    )
}

const EVIDENCE_SNIPPET_CHARS: usize = 700;

/// 返回 evidence 命中文本附近的片段，同时返回字符偏移量。
///
/// KB 节点可能包含整个导入文档。直接返回前 N 个字符会隐藏
/// 文档后部的命中，使有效命中看起来与查询无关。
/// 返回 FocusedExcerpt 以便调用方同步调整 source_start/source_end。
fn query_centered_snippet_ex(content: &str, query: &str, terms: &[String]) -> FocusedExcerpt {
    let content_len = content.chars().count();
    if content_len <= EVIDENCE_SNIPPET_CHARS {
        return FocusedExcerpt {
            text: content.to_string(),
            raw_text: content.to_string(),
            start: 0,
            end: content_len,
            truncated: false,
        };
    }

    if let Some(excerpt) = query_focused_excerpt_details(content, terms, EVIDENCE_SNIPPET_CHARS) {
        return excerpt;
    }

    let folded_content = content.to_ascii_lowercase();
    let folded_query = query.trim().to_ascii_lowercase();
    let exact_match = (!folded_query.is_empty())
        .then(|| {
            folded_content
                .find(&folded_query)
                .map(|start| (start, folded_query.len()))
        })
        .flatten();

    let token_match = || {
        crate::nlp::chinese::tokenize_content(query)
            .split_whitespace()
            .filter_map(|term| {
                let term = term.to_ascii_lowercase();
                folded_content
                    .find(&term)
                    .map(|start| (start, term.len(), term.chars().count()))
            })
            .max_by_key(|(_, _, chars)| *chars)
            .map(|(start, len, _)| (start, len))
    };

    let Some((match_byte_start, match_byte_len)) = exact_match.or_else(token_match) else {
        // 无匹配时从头部截取，预留 3 字符给后缀 "..."
        let budget = EVIDENCE_SNIPPET_CHARS.saturating_sub(3).max(1);
        let truncated: String = content.chars().take(budget).collect();
        return FocusedExcerpt {
            text: format!("{}...", truncated),
            raw_text: truncated,
            start: 0,
            end: budget,
            truncated: true,
        };
    };

    // to_ascii_lowercase preserves byte offsets, so these byte boundaries are
    // also valid in the original UTF-8 content.
    // 为前后省略号预留 6 字符（前后各 "..."），确保 text 不超过 EVIDENCE_SNIPPET_CHARS
    let budget = EVIDENCE_SNIPPET_CHARS.saturating_sub(6).max(1);
    let match_char_start = content[..match_byte_start].chars().count();
    let match_char_len = content[match_byte_start..match_byte_start + match_byte_len]
        .chars()
        .count()
        .min(budget);
    let before_match = (budget - match_char_len) / 2;
    let mut start = match_char_start.saturating_sub(before_match);
    if start + budget > content_len {
        start = content_len.saturating_sub(budget);
    }
    let end = (start + budget).min(content_len);
    let excerpt: String = content.chars().skip(start).take(end - start).collect();

    let has_prefix = start > 0;
    let has_suffix = end < content_len;
    FocusedExcerpt {
        text: format!(
            "{}{}{}",
            if has_prefix { "..." } else { "" },
            &excerpt,
            if has_suffix { "..." } else { "" }
        ),
        raw_text: excerpt,
        start,
        end,
        truncated: has_prefix || has_suffix,
    }
}

/// 查询时间线事件（§11.1 TimelineFirst 策略）
///
/// 从已接受或已应用的 timeline_event 候选中提取时间线事件，
/// 按事件日期降序排列，并关联 artifact 和 KB 文档信息。
///
/// 修复：之前只查 status='accepted'，但 apply_candidate 会把候选状态改成 'applied'，
/// 导致时间线候选刚应用到 gbrain 后反而从 TimelineFirst 结果里消失。
/// 现在查 status IN ('accepted', 'applied')：
/// - accepted: 已批准但尚未写入 gbrain
/// - applied: 已写入 gbrain，仍应出现在时间线查询中
///   修复：增加 filter_slug 参数，下推 slug 过滤到 SQL 查询阶段，
///   通过 JOIN artifact_projections 限定只返回目标 slug 的时间线事件，
///   避免先全局 LIMIT 再后置 retain 导致目标 slug 的命中被挤掉
fn query_timeline_events(
    conn: &Connection,
    query: &str,
    limit: i64,
    filter_slug: Option<&str>,
) -> Result<Vec<TimelineHit>> {
    // 修复：转义 LIKE 通配符 % 和 _，避免用户输入中的这些字符
    // 被解释为 SQL LIKE 通配符导致匹配意外结果
    let escaped_query = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");

    // 修复：当提供 filter_slug 时，JOIN artifact_projections 匹配 brain_shadow_page 投影
    let sql = if filter_slug.is_some() {
        "SELECT pc.id, pc.proposed_payload, pc.artifact_id, pc.kb_document_id,
                d.original_name, d.title
         FROM promotion_candidates pc
         LEFT JOIN kb_documents d ON d.id = pc.kb_document_id
         JOIN artifact_projections ap ON ap.artifact_id = pc.artifact_id
              AND ap.projection_type = 'brain_shadow_page'
              AND (ap.projection_ref = ?3 OR ap.projection_ref = ?4)
              AND ap.status = 'active'
         WHERE pc.candidate_type = 'timeline_event'
           AND pc.status IN ('accepted', 'applied')
           AND (pc.proposed_payload LIKE '%' || ?1 || '%' ESCAPE '\\'
                OR d.original_name LIKE '%' || ?1 || '%' ESCAPE '\\'
                OR d.title LIKE '%' || ?1 || '%' ESCAPE '\\')
         ORDER BY pc.created_at DESC
         LIMIT ?2"
    } else {
        "SELECT pc.id, pc.proposed_payload, pc.artifact_id, pc.kb_document_id,
                d.original_name, d.title
         FROM promotion_candidates pc
         LEFT JOIN kb_documents d ON d.id = pc.kb_document_id
         WHERE pc.candidate_type = 'timeline_event'
           AND pc.status IN ('accepted', 'applied')
           AND (pc.proposed_payload LIKE '%' || ?1 || '%' ESCAPE '\\'
                OR d.original_name LIKE '%' || ?1 || '%' ESCAPE '\\'
                OR d.title LIKE '%' || ?1 || '%' ESCAPE '\\')
         ORDER BY pc.created_at DESC
         LIMIT ?2"
    };

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| GBrainError::Database(format!("准备时间线查询失败: {}", e)))?;

    // 修复：filter_slug 参数格式为 'slug:{slug}'，与 artifact_projections.projection_ref 匹配
    // 使用统一闭包避免 Rust 闭包类型不匹配问题
    #[allow(clippy::type_complexity)]
    let map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<(i64, String, i64, Option<i64>, String, Option<String>)> {
        let candidate_id: i64 = row.get(0)?;
        let payload: String = row.get(1)?;
        let artifact_id: i64 = row.get(2)?;
        let kb_document_id: Option<i64> = row.get(3)?;
        let original_name: String = row.get(4)?;
        let doc_title: Option<String> = row.get(5)?;
        Ok((candidate_id, payload, artifact_id, kb_document_id, original_name, doc_title))
    };

    let rows = if let Some(slug) = filter_slug {
        let (slug_ref, documents_slug_ref) = slug_ref_variants(slug);
        stmt.query_map(
            params![escaped_query, limit, slug_ref, documents_slug_ref],
            map_row,
        )
        .map_err(|e| GBrainError::Database(format!("时间线查询失败: {}", e)))?
    } else {
        stmt.query_map(params![escaped_query, limit], map_row)
            .map_err(|e| GBrainError::Database(format!("时间线查询失败: {}", e)))?
    };

    let mut hits = Vec::new();
    for row in rows {
        let (candidate_id, payload, artifact_id, kb_document_id, original_name, doc_title) =
            row.map_err(|e| GBrainError::Database(format!("映射时间线行失败: {}", e)))?;

        // 从 payload JSON 中提取 event_date 和 description
        let (event_date, description) = parse_timeline_payload(&payload);

        let source_title = doc_title.unwrap_or(original_name);

        hits.push(TimelineHit {
            candidate_id,
            event_date,
            description,
            artifact_id,
            kb_document_id,
            shadow_page_slug: None, // 后续填充
            source_title,
        });
    }

    Ok(hits)
}

/// 从 timeline_event 候选的 payload JSON 中提取 event_date 和 description
fn parse_timeline_payload(payload: &str) -> (String, String) {
    let mut event_date = String::new();
    let mut description = String::new();

    if let Ok(val) = serde_json::from_str::<serde_json::Value>(payload) {
        if let Some(date) = val.get("event_date").and_then(|v| v.as_str()) {
            event_date = date.to_string();
        } else if let Some(date) = val.get("date").and_then(|v| v.as_str()) {
            event_date = date.to_string();
        }
        if let Some(desc) = val.get("description").and_then(|v| v.as_str()) {
            description = desc.to_string();
        } else if let Some(desc) = val.get("text").and_then(|v| v.as_str()) {
            description = desc.to_string();
        } else if let Some(desc) = val.get("content").and_then(|v| v.as_str()) {
            description = desc.to_string();
        }
    }

    // 如果 JSON 解析失败，使用原始 payload 作为描述
    if description.is_empty() && event_date.is_empty() {
        // 修复：按字符截断而非字节，避免中文等多字节字符在 UTF-8 边界 panic
        description = if payload.chars().count() > 200 {
            format!("{}...", payload.chars().take(200).collect::<String>())
        } else {
            payload.to_string()
        };
    }

    (event_date, description)
}

/// 健康检查 — 检查 artifact 投影一致性
pub fn check_artifact_health(conn: &Connection) -> Result<ArtifactHealthReport> {
    let total_artifacts = store::count_total_artifacts(conn)
        .map_err(|e| GBrainError::Database(format!("统计 artifact 总数失败: {}", e)))?;
    let active_artifacts = store::count_active_artifacts(conn)
        .map_err(|e| GBrainError::Database(format!("统计活跃 artifact 失败: {}", e)))?;

    let orphan_projections = store::find_orphan_projections(conn)
        .map_err(|e| GBrainError::Database(format!("查找孤立投影失败: {}", e)))?
        .len() as i64;
    let stale_projections = store::count_stale_projections(conn)
        .map_err(|e| GBrainError::Database(format!("统计过期投影失败: {}", e)))?;
    let pending_candidates = crate::artifact::promotion::count_pending_candidates(conn)?;
    let active_provenance = provenance::count_active_provenance(conn)?;
    let stale_provenance = provenance::count_stale_provenance(conn)?;

    let mut issues = Vec::new();

    // 检查孤立投影
    if orphan_projections > 0 {
        issues.push(HealthIssue {
            severity: "warning".to_string(),
            issue_type: "orphan_projection".to_string(),
            description: format!(
                "{} 个投影的 artifact 已删除但投影仍标记为 active",
                orphan_projections
            ),
            suggestion: "运行 doctor --fix-artifacts 修复孤立投影".to_string(),
        });
    }

    // 检查过期投影
    if stale_projections > 0 {
        issues.push(HealthIssue {
            severity: "info".to_string(),
            issue_type: "stale_projection".to_string(),
            description: format!("{} 个投影已标记为 stale", stale_projections),
            suggestion: "检查 stale 投影是否需要重新处理".to_string(),
        });
    }

    // 检查待审核候选
    if pending_candidates > 100 {
        issues.push(HealthIssue {
            severity: "warning".to_string(),
            issue_type: "pending_candidates".to_string(),
            description: format!("{} 个候选变更待审核", pending_candidates),
            suggestion: "运行 artifact review list --status pending 查看并审核".to_string(),
        });
    }

    // 检查 artifact 文件完整性（分页全量检查）
    let page_size = 500;
    let mut offset = 0;
    loop {
        let artifacts = store::list_active_artifacts(conn, page_size, offset)
            .map_err(|e| GBrainError::Database(format!("列出 artifact 失败: {}", e)))?;
        if artifacts.is_empty() {
            break;
        }
        for artifact in &artifacts {
            let path = std::path::PathBuf::from(&artifact.storage_path);
            if !path.exists() {
                issues.push(HealthIssue {
                    severity: "error".to_string(),
                    issue_type: "missing_artifact_file".to_string(),
                    description: format!(
                        "Artifact {} 文件不存在: {}",
                        artifact.artifact_uid, artifact.storage_path
                    ),
                    suggestion: "检查 artifact store 目录是否完整".to_string(),
                });
            }
        }
        offset += page_size;
    }

    // 检查 KB 作业卡住（超过 24 小时仍在 queued/processing）
    let stale_jobs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM kb_documents WHERE document_status IN ('queued', 'processing')
             AND updated_at < datetime('now', '-24 hours') AND deleted_at IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0);
    if stale_jobs > 0 {
        issues.push(HealthIssue {
            severity: "warning".to_string(),
            issue_type: "stale_kb_jobs".to_string(),
            description: format!("{} KB 文档处理作业超过 24 小时未完成", stale_jobs),
            suggestion: "检查 KB worker 是否正常运行".to_string(),
        });
    }

    let report = ArtifactHealthReport {
        total_artifacts,
        active_artifacts,
        orphan_projections,
        stale_projections,
        pending_candidates,
        active_provenance,
        stale_provenance,
        issues,
    };
    info!("check_artifact_health: total={}, active={}, orphans={}, stale_projections={}, pending={}, issues={}",
        report.total_artifacts, report.active_artifacts, report.orphan_projections,
        report.stale_projections, report.pending_candidates, report.issues.len());
    Ok(report)
}
