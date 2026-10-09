//! Token synonym lookup for query expansion (E6 design).
//!
//! Runtime path: O(log n) SQLite lookup, zero embedding API calls.
//! Synonyms are mined offline by `mine_synonyms` (PR-3).

use rusqlite::{params, Connection};
use std::collections::HashMap;
use tracing::{info, warn};

use crate::embedding::Embedder;
use crate::error::{GBrainError, Result};
// #8 修复：复用 crate 共享工具函数，消除重复实现
use crate::kb::pipeline::embedding_to_blob;
use crate::search::vector::cosine_similarity;

/// 校验 SQL 标识符只包含字母、数字和下划线，防止注入。
fn validate_sql_identifier(name: &str) {
    assert!(
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "SQL 标识符必须非空且只包含字母数字和下划线，got: {:?}",
        name
    );
}

fn token_vec_table_ddl(table: &str, dimensions: i32) -> String {
    format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS {} USING vec0(\
         embedding float[{}] distance_metric=cosine)",
        table, dimensions,
    )
}

fn with_savepoint<T>(
    conn: &Connection,
    name: &str,
    f: impl FnOnce(&Connection) -> Result<T>,
) -> Result<T> {
    validate_sql_identifier(name);
    conn.execute(&format!("SAVEPOINT {}", name), [])
        .map_err(|e| GBrainError::Database(e.to_string()))?;
    match f(conn) {
        Ok(value) => {
            conn.execute(&format!("RELEASE {}", name), [])
                .map_err(|e| GBrainError::Database(e.to_string()))?;
            Ok(value)
        }
        Err(e) => {
            let _ = conn.execute(&format!("ROLLBACK TO {}", name), []);
            let _ = conn.execute(&format!("RELEASE {}", name), []);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Runtime lookup (PR-2)
// ---------------------------------------------------------------------------

/// Maximum number of synonyms returned per token at query time.
pub const MAX_RUNTIME_SYNONYMS: usize = 3;

/// 返回指定 library（或全局）的 active embedding index ID。
///
/// # 问题 #1 修复：跨库 embedding 索引不一致
/// 新增 `library_id` 参数。当为 `Some` 时限定在该 library 内查找，
/// 避免不同 library 使用不同 embedding model 时注入错误模型的同义词。
pub fn active_embedding_index_id(conn: &Connection, library_id: Option<i64>) -> Option<i64> {
    match library_id {
        Some(lib_id) => conn
            .query_row(
                "SELECT id FROM kb_embedding_indexes WHERE library_id = ?1 AND is_active = 1 LIMIT 1",
                params![lib_id],
                |row| row.get::<_, i64>(0),
            )
            .ok(),
        None => conn
            .query_row(
                "SELECT id FROM kb_embedding_indexes WHERE is_active = 1 LIMIT 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .ok(),
    }
}

/// Look up synonym expansions for `token` from `kb_token_synonyms`.
///
/// Returns up to `limit` synonyms ordered by descending cosine similarity.
/// Gracefully returns an empty `Vec` when:
/// - no active embedding index exists (cold start)
/// - the `kb_token_synonyms` table does not yet exist
/// - no synonyms have been mined for this token
pub fn lookup_token_synonyms(
    conn: &Connection,
    token: &str,
    limit: usize,
    library_id: Option<i64>,
) -> Vec<String> {
    let Some(active_idx_id) = active_embedding_index_id(conn, library_id) else {
        return Vec::new();
    };
    let Ok(mut stmt) = conn.prepare(
        "SELECT synonym FROM kb_token_synonyms \
         WHERE embedding_index_id = ?1 AND token = ?2 \
         ORDER BY score DESC LIMIT ?3",
    ) else {
        return Vec::new();
    };
    let rows = stmt.query_map(params![active_idx_id, token, limit as i64], |row| {
        row.get::<_, String>(0)
    });
    match rows {
        Ok(iter) => iter.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

/// 批量查找多个 token 的同义词扩展。
///
/// # 问题 #13 修复：N+1 查询优化
/// 只解析一次 active embedding index，用单次 `WHERE token IN (...)` 查询，
/// 避免逐个 token 调用 `lookup_token_synonyms` 的 N+1 SQL round-trip。
pub fn batch_lookup_token_synonyms(
    conn: &Connection,
    tokens: &[String],
    limit_per_token: usize,
    library_id: Option<i64>,
) -> HashMap<String, Vec<String>> {
    let Some(active_idx_id) = active_embedding_index_id(conn, library_id) else {
        return HashMap::new();
    };
    if tokens.is_empty() {
        return HashMap::new();
    }

    // 去重（保留首次出现顺序），避免重复参数传入 SQL IN
    let mut seen = std::collections::HashSet::new();
    let unique_tokens: Vec<&String> = tokens.iter().filter(|t| seen.insert(*t)).collect();
    if unique_tokens.is_empty() {
        return HashMap::new();
    }

    // 构建 IN 子句
    let placeholders: Vec<&str> = unique_tokens.iter().map(|_| "?").collect();
    let sql = format!(
        "SELECT token, synonym FROM kb_token_synonyms \
         WHERE embedding_index_id = ? AND token IN ({}) \
         ORDER BY token, score DESC",
        placeholders.join(",")
    );

    let Ok(mut stmt) = conn.prepare(&sql) else {
        return HashMap::new();
    };

    // 参数：active_idx_id + 各去重 token
    let params: Vec<&dyn rusqlite::ToSql> = std::iter::once(&active_idx_id as &dyn rusqlite::ToSql)
        .chain(unique_tokens.iter().map(|t| *t as &dyn rusqlite::ToSql))
        .collect();

    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    });

    let mut result: HashMap<String, Vec<String>> = HashMap::new();
    if let Ok(iter) = rows {
        for row in iter.flatten() {
            let (token, synonym) = row;
            let entry = result.entry(token).or_default();
            if entry.len() < limit_per_token {
                entry.push(synonym);
            }
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Offline mining (PR-3)
// ---------------------------------------------------------------------------

/// Default mining parameters — zero-config usable.
pub const DEFAULT_MIN_DOC_FREQ: usize = 10;
pub const DEFAULT_MAX_DOC_FREQ_RATIO: f64 = 0.3;
pub const DEFAULT_MIN_TOKEN_CHAR_LEN: usize = 2;
pub const DEFAULT_KNN_K: usize = 6;
pub const DEFAULT_SIMILARITY_THRESHOLD: f64 = 0.75;
pub const DEFAULT_MAX_SYNONYMS_PER_TOKEN: usize = 5;
pub const DEFAULT_MINING_BATCH_SIZE: usize = 200;
/// 并发 embed batch 数
pub const DEFAULT_MINING_CONCURRENCY: usize = 4;
/// embedding batch 最大重试次数
const MAX_EMBED_RETRIES: usize = 3;
/// brute-force KNN 的 token 数量上限（#10 修复：防止 O(n²) 阻塞 worker）
const BRUTE_FORCE_MAX_TOKENS: usize = 10_000;

/// Options controlling the offline synonym mining job.
#[derive(Debug)]
pub struct MineSynonymsOpts {
    pub library_id: Option<i64>,
    pub full: bool,
    pub min_doc_freq: usize,
    pub max_doc_freq_ratio: f64,
    pub min_token_char_len: usize,
    pub knn_k: usize,
    pub similarity_threshold: f64,
    pub max_synonyms_per_token: usize,
    pub batch_size: usize,
    /// 并发 embed batch 数（默认 4）
    pub concurrency: usize,
}

impl Default for MineSynonymsOpts {
    fn default() -> Self {
        Self {
            library_id: None,
            full: false,
            min_doc_freq: DEFAULT_MIN_DOC_FREQ,
            max_doc_freq_ratio: DEFAULT_MAX_DOC_FREQ_RATIO,
            min_token_char_len: DEFAULT_MIN_TOKEN_CHAR_LEN,
            knn_k: DEFAULT_KNN_K,
            similarity_threshold: DEFAULT_SIMILARITY_THRESHOLD,
            max_synonyms_per_token: DEFAULT_MAX_SYNONYMS_PER_TOKEN,
            batch_size: DEFAULT_MINING_BATCH_SIZE,
            concurrency: DEFAULT_MINING_CONCURRENCY,
        }
    }
}

/// Statistics returned by `mine_synonyms`.
#[derive(Debug, Default)]
pub struct MineSynonymsStats {
    pub candidates: usize,
    pub new_embeddings: usize,
    pub total_embeddings: usize,
    pub synonyms_written: usize,
}

// -- helpers ----------------------------------------------------------------

fn is_substring_or_superstring(a: &str, b: &str) -> bool {
    // 短 token（长度 <= 2）太容易误命中，跳过子串/超串检查
    if a.chars().count() <= 2 || b.chars().count() <= 2 {
        return false;
    }
    a.contains(b) || b.contains(a)
}

/// 将 BLOB 反序列化为 f32 向量。
/// 编码使用共享的 `pipeline::embedding_to_blob`。
fn blob_to_f32(blob: &[u8]) -> Vec<f32> {
    if !blob.len().is_multiple_of(4) {
        tracing::warn!("blob 长度 {} 不是 4 的倍数，尾部字节将被丢弃", blob.len());
    }
    blob.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

// -- step 1: extract candidate tokens with DF --------------------------------

/// # 问题 #9 修复：流式处理
/// 逐行读取 content 并立即 tokenize，不再将全量 content 加载到 Vec<String>，
/// 避免大量节点时占用过多内存。
fn extract_candidate_tokens(
    conn: &Connection,
    opts: &MineSynonymsOpts,
) -> Result<Vec<(String, usize)>> {
    let lib_id = opts.library_id;
    let mut df: HashMap<String, usize> = HashMap::new();
    let mut total_nodes: usize = 0;

    // 统一 SQL，流式处理避免全量加载到内存
    let sql = match lib_id {
        Some(_) => "SELECT content FROM kb_document_nodes WHERE library_id = ?1 AND level = 0",
        None => "SELECT content FROM kb_document_nodes WHERE level = 0",
    };

    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| GBrainError::Database(e.to_string()))?;

    // 使用 stmt.query() 而非 query_map() 避免闭包类型不匹配
    let mut rows = match lib_id {
        Some(id) => stmt.query(params![id]),
        None => stmt.query([]),
    }
    .map_err(|e| GBrainError::Database(e.to_string()))?;

    // 逐行 tokenize，content 用完即释放
    while let Ok(Some(row)) = rows.next() {
        let content: String = row
            .get(0)
            .map_err(|e| GBrainError::Database(e.to_string()))?;
        let tokens_str = crate::nlp::chinese::tokenize_content(&content);
        let unique: std::collections::HashSet<String> = tokens_str
            .split_whitespace()
            .map(|s| s.to_string())
            .collect();
        for token in &unique {
            *df.entry(token.clone()).or_insert(0) += 1;
        }
        total_nodes += 1;
    }

    info!(
        total_nodes,
        unique_tokens = df.len(),
        "Token extraction complete"
    );

    let min_df = opts.min_doc_freq;
    let max_df = ((total_nodes as f64) * opts.max_doc_freq_ratio) as usize;
    let min_chars = opts.min_token_char_len;

    let mut candidates: Vec<(String, usize)> = df
        .into_iter()
        .filter(|(_, freq)| *freq >= min_df && *freq <= max_df)
        .filter(|(token, _)| token.chars().count() >= min_chars)
        .collect();

    // 重要词排前面
    candidates.sort_by_key(|b| std::cmp::Reverse(b.1));
    Ok(candidates)
}

// -- step 2: load existing embeddings ----------------------------------------

fn load_existing_token_embeddings(
    conn: &Connection,
    index_id: i64,
) -> Result<HashMap<String, Vec<f32>>> {
    let Ok(mut stmt) = conn
        .prepare("SELECT token, embedding FROM kb_token_embeddings WHERE embedding_index_id = ?1")
    else {
        return Ok(HashMap::new());
    };
    let rows = stmt
        .query_map(params![index_id], |row| {
            let token: String = row.get(0)?;
            let blob: Vec<u8> = row.get(1)?;
            Ok((token, blob))
        })
        .map_err(|e| GBrainError::Database(e.to_string()))?;

    let mut map = HashMap::new();
    for row in rows {
        let (token, blob) = row.map_err(|e| GBrainError::Database(e.to_string()))?;
        map.insert(token, blob_to_f32(&blob));
    }
    Ok(map)
}

// -- step 3: embed and store (带重试 + 并发) ----------------------------------

/// # 问题 #8 修复：使用 crate::backoff 替代手写重试
/// 使用 `with_backoff_sync` 提供带 jitter 的指数退避，避免惊群效应。
fn embed_batch_with_retry(
    embedder: &Embedder,
    rt: &tokio::runtime::Runtime,
    texts: &[&str],
) -> Option<Vec<Vec<f32>>> {
    crate::backoff::with_backoff_sync(
        || rt.block_on(embedder.embed_batch(texts)),
        crate::backoff::BackoffOpts {
            max_retries: MAX_EMBED_RETRIES as u32,
            base_ms: 1000,
            max_ms: 30_000,
            jitter: true,
        },
    )
    .ok()
}

/// embed 新 token 并写入 kb_token_embeddings。
///
/// - 按 `concurrency` 分组并发 embed（`std::thread::scope`）
/// - 写入 DB 仍然串行（`Connection` 非线程安全）
/// - 每个 batch 失败时自动重试最多 `MAX_EMBED_RETRIES` 次
///
/// # 问题 #12 修复
/// 返回新嵌入的 token→vector 映射，调用方可直接 merge 而非全量重载。
///
/// # 问题 #14 修复
/// 批量 INSERT 包裹在事务内，减少 WAL commit 次数。
fn embed_and_store_tokens(
    conn: &Connection,
    embedder: &Embedder,
    rt: &tokio::runtime::Runtime,
    tokens: &[(String, usize)],
    index_id: i64,
    batch_size: usize,
    concurrency: usize,
) -> Result<(usize, HashMap<String, Vec<f32>>)> {
    let chunks: Vec<_> = tokens.chunks(batch_size).collect();
    let mut embedded = 0usize;
    // #12: 收集新嵌入结果，供调用方 merge
    let mut new_embeddings: HashMap<String, Vec<f32>> = HashMap::new();

    with_savepoint(conn, "sp_embed_token_embeddings", |conn| {
        // 按 concurrency 分组处理
        for group in chunks.chunks(concurrency.max(1)) {
            // 并发 embed：每组内各 chunk 同时请求 embedding API
            let group_results: Vec<Option<Vec<Vec<f32>>>> = if concurrency > 1 {
                std::thread::scope(|s| {
                    group
                        .iter()
                        .map(|chunk| {
                            let texts: Vec<String> = chunk.iter().map(|(t, _)| t.clone()).collect();
                            s.spawn(move || {
                                let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
                                embed_batch_with_retry(embedder, rt, &refs)
                            })
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|h| h.join().unwrap_or(None))
                        .collect()
                })
            } else {
                // concurrency=1：直接在当前线程执行，避免不必要的线程创建
                group
                    .iter()
                    .map(|chunk| {
                        let texts: Vec<&str> = chunk.iter().map(|(t, _)| t.as_str()).collect();
                        embed_batch_with_retry(embedder, rt, &texts)
                    })
                    .collect()
            };

            // 串行写入 DB（Connection 非线程安全）
            for (chunk, result) in group.iter().zip(group_results.iter()) {
                match result {
                    None => {
                        warn!(count = chunk.len(), "Batch embedding 最终失败，跳过该批");
                        continue;
                    }
                    Some(vectors) => {
                        for ((token, doc_freq), vec) in chunk.iter().zip(vectors.iter()) {
                            let blob = embedding_to_blob(vec);
                            conn.execute(
                                "INSERT OR REPLACE INTO kb_token_embeddings \
                                 (token, embedding_index_id, embedding, doc_freq) \
                                 VALUES (?1, ?2, ?3, ?4)",
                                params![token, index_id, blob, *doc_freq as i64],
                            )
                            .map_err(|e| GBrainError::Database(e.to_string()))?;
                            embedded += 1;
                            new_embeddings.insert(token.clone(), vec.clone());
                        }
                        info!(batch = chunk.len(), embedded, "Embedded token batch");
                    }
                }
            }
        }
        Ok(())
    })?;

    Ok((embedded, new_embeddings))
}

// -- step 4: KNN mining (sqlite-vec + 内存回退) ------------------------------

/// token 向量虚表名（与节点向量 `vec_kb_{id}` 区分）
fn token_vec_table_name(index_id: i64) -> String {
    format!("vec_token_{}", index_id)
}

/// 内存暴力 KNN（O(n²)），用于测试和 sqlite-vec 不可用时的回退。
///
/// # 修复汇总
/// - #4: 修正 off-by-one（self 已排除，truncate 用 knn_k 而非 knn_k-1）
/// - #5: substring 过滤前置到候选收集阶段，避免浪费邻居槽位
/// - #10: token 数量超限时跳过 brute-force，防止 O(n²) 阻塞 worker
fn knn_mine_brute_force(
    all: &HashMap<String, Vec<f32>>,
    opts: &MineSynonymsOpts,
) -> Vec<(String, String, f32)> {
    // #10: brute-force O(n²) 上限保护
    if all.len() > BRUTE_FORCE_MAX_TOKENS {
        warn!(
            tokens = all.len(),
            limit = BRUTE_FORCE_MAX_TOKENS,
            "token 数量超限，跳过 brute-force KNN（改用 sqlite-vec）"
        );
        return Vec::new();
    }

    let tokens: Vec<&String> = all.keys().collect();
    let mut pairs = Vec::new();

    for i in 0..tokens.len() {
        let token_a = tokens[i];
        let emb_a = &all[token_a];
        let mut neighbors: Vec<(f32, usize)> = Vec::new();

        for (j, &token_b) in tokens.iter().enumerate() {
            if i == j {
                continue;
            }
            // #5 修复：substring 过滤前置，避免浪费邻居槽位
            if is_substring_or_superstring(token_a, token_b) {
                continue;
            }
            let sim = cosine_similarity(emb_a, &all[token_b]);
            if sim >= opts.similarity_threshold as f32 {
                neighbors.push((sim, j));
            }
        }

        neighbors.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        // #4 修复：修正 off-by-one — self 已在循环中排除，直接用 knn_k
        neighbors.truncate(opts.knn_k);

        for (score, j) in neighbors {
            pairs.push((token_a.clone(), tokens[j].clone(), score));
        }
    }
    pairs
}

/// 使用 sqlite-vec 虚表做 KNN 挖掘。
///
/// 流程：创建临时虚表 → 批量 INSERT → 逐 token KNN 查询 → 清理虚表。
///
/// # 修复汇总
/// - #6: rowid 越界检查，防止中断重启后 panic
/// - #11: 语句 prepare 提到循环外，避免每轮重复 SQL 解析
/// - #14: 虚表批量 INSERT 包裹在事务内
fn knn_mine_via_vec(
    conn: &Connection,
    index_id: i64,
    dimensions: i32,
    all: &HashMap<String, Vec<f32>>,
    opts: &MineSynonymsOpts,
) -> Result<Vec<(String, String, f32)>> {
    let table = token_vec_table_name(index_id);
    validate_sql_identifier(&table);

    // 1. 创建 cosine 距离度量的 sqlite-vec 虚表
    conn.execute_batch(&token_vec_table_ddl(&table, dimensions))
        .map_err(|e| GBrainError::Database(e.to_string()))?;

    // guard：无论如何都清理虚表，防止 KNN 查询阶段出错导致虚表残留
    struct DropGuard<'a> {
        conn: &'a Connection,
        table: String,
        armed: bool,
    }
    impl Drop for DropGuard<'_> {
        fn drop(&mut self) {
            if self.armed {
                let _ = self
                    .conn
                    .execute_batch(&format!("DROP TABLE IF EXISTS {}", self.table));
            }
        }
    }
    let mut guard = DropGuard {
        conn,
        table: table.clone(),
        armed: true,
    };

    let tokens: Vec<String> = all.keys().cloned().collect();

    with_savepoint(conn, "sp_write_token_vec_table", |conn| {
        // 清空旧数据并批量插入；失败时回滚到旧虚表内容。
        conn.execute_batch(&format!("DELETE FROM {}", table))
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        let mut stmt = conn
            .prepare(&format!(
                "INSERT INTO {} (rowid, embedding) VALUES (?1, ?2)",
                table
            ))
            .map_err(|e| GBrainError::Database(e.to_string()))?;

        for (i, token) in tokens.iter().enumerate() {
            let blob = embedding_to_blob(&all[token]);
            stmt.execute(params![(i + 1) as i64, blob])
                .map_err(|e| GBrainError::Database(e.to_string()))?;
        }
        Ok(())
    })?;

    // 3. 对每个 token 执行 KNN 查询
    //    cosine 距离 = 1 - cosine_similarity
    let max_distance = 1.0_f32 - opts.similarity_threshold as f32;
    let k = opts.knn_k + 1; // +1 因为查询自身会出现在结果中
    let mut pairs = Vec::new();

    // #11 修复：将语句 prepare 提到循环外，避免每轮重复 SQL 解析
    let mut knn_stmt = conn
        .prepare(&format!(
            "SELECT rowid, distance FROM {} \
             WHERE embedding MATCH ?1 AND k = ?2 \
             ORDER BY distance",
            table
        ))
        .map_err(|e| GBrainError::Database(e.to_string()))?;

    for (i, token_a) in tokens.iter().enumerate() {
        let query_blob = embedding_to_blob(&all[token_a]);

        let neighbors: Vec<(i64, f32)> = knn_stmt
            .query_map(params![query_blob, k as i64], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, f32>(1)?))
            })
            .map_err(|e| GBrainError::Database(e.to_string()))?
            .filter_map(|r| r.ok())
            .filter(|(rowid, dist)| {
                // 排除自身（rowid = i+1），过滤低于阈值的
                *rowid != (i as i64) + 1 && *dist <= max_distance
            })
            .collect();

        for (rowid, distance) in neighbors {
            // #6 修复：rowid 边界检查 — 防止中断重启后虚表不一致导致 panic
            let idx = (rowid - 1) as usize;
            if idx >= tokens.len() {
                warn!(
                    rowid,
                    tokens_len = tokens.len(),
                    "KNN 返回的 rowid 超出 tokens 范围，跳过"
                );
                continue;
            }
            let token_b = &tokens[idx];
            let score = 1.0 - distance; // cosine distance → similarity
            if is_substring_or_superstring(token_a, token_b) {
                continue;
            }
            pairs.push((token_a.clone(), token_b.clone(), score));
        }
    }

    // 4. 显式清理虚表；DROP 成功后再 disarm guard，避免失败时 guard 不重试
    drop(knn_stmt); // 先释放 KNN 语句句柄，避免 DROP TABLE 时表仍被引用
    conn.execute_batch(&format!("DROP TABLE IF EXISTS {}", table))
        .map_err(|e| GBrainError::Database(e.to_string()))?;
    guard.armed = false; // DROP 成功后才 disarm

    Ok(pairs)
}

/// KNN 同义词挖掘入口。
///
/// 优先使用 sqlite-vec 虚表（可处理大规模 token），不可用时回退到内存暴力搜索。
fn knn_mine(
    conn: &Connection,
    index_id: i64,
    dimensions: i32,
    all: &HashMap<String, Vec<f32>>,
    opts: &MineSynonymsOpts,
) -> Vec<(String, String, f32)> {
    match knn_mine_via_vec(conn, index_id, dimensions, all, opts) {
        Ok(pairs) => pairs,
        Err(e) => {
            warn!(error = %e, "sqlite-vec KNN 失败，回退到内存暴力搜索");
            let pairs = knn_mine_brute_force(all, opts);
            if pairs.is_empty() && all.len() > BRUTE_FORCE_MAX_TOKENS {
                warn!(
                    tokens = all.len(),
                    "brute-force 回退也因 token 超限返回空结果，同义词挖掘被跳过"
                );
            }
            pairs
        }
    }
}

// -- step 5: write synonyms bidirectionally ----------------------------------

/// # 问题 #14 修复：事务包裹批量 INSERT
fn write_synonyms_bidirectional(
    conn: &Connection,
    pairs: Vec<(String, String, f32)>,
    index_id: i64,
    max_per_token: usize,
) -> Result<usize> {
    let mut by_token: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    for (token, synonym, score) in &pairs {
        by_token
            .entry(token.clone())
            .or_default()
            .push((synonym.clone(), *score));
        // 反向
        by_token
            .entry(synonym.clone())
            .or_default()
            .push((token.clone(), *score));
    }

    let mut written = 0usize;
    with_savepoint(conn, "sp_write_synonym_pairs", |conn| {
        for (token, mut synonyms) in by_token {
            synonyms.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
            synonyms.truncate(max_per_token);
            for (synonym, score) in synonyms {
                conn.execute(
                    "INSERT OR REPLACE INTO kb_token_synonyms \
                     (token, synonym, score, embedding_index_id) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![token, synonym, score, index_id],
                )
                .map_err(|e| GBrainError::Database(e.to_string()))?;
                written += 1;
            }
        }
        Ok(())
    })?;

    Ok(written)
}

// -- main entry point --------------------------------------------------------

/// Run the offline synonym mining pipeline.
///
/// Steps: extract tokens → embed → KNN → write synonyms.
/// Requires an active embedding index and a configured `Embedder`.
/// The `rt` tokio runtime is used for async embedding calls.
///
/// # 修复汇总
/// - #3: full=true 时清除旧 synonym 表，避免低分旧 pair 残留
/// - #7: dimensions 不匹配时报错而非静默覆盖
/// - #12: 合并新 embedding 而非全量重载
pub fn mine_synonyms(
    conn: &Connection,
    embedder: &Embedder,
    dimensions: i32,
    rt: &tokio::runtime::Runtime,
    opts: &MineSynonymsOpts,
) -> Result<MineSynonymsStats> {
    let mut stats = MineSynonymsStats::default();

    // 1. 按库查找 active embedding index
    let (index_id, dims): (i64, i32) = {
        let sql = match opts.library_id {
            Some(_) => {
                "SELECT id, dimensions FROM kb_embedding_indexes \
                        WHERE library_id = ?1 AND is_active = 1 LIMIT 1"
            }
            None => {
                "SELECT id, dimensions FROM kb_embedding_indexes \
                     WHERE is_active = 1 LIMIT 1"
            }
        };
        let r = match opts.library_id {
            Some(lib_id) => conn.query_row(sql, params![lib_id], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?))
            }),
            None => conn.query_row(sql, [], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i32>(1)?))
            }),
        };
        r.map_err(|e| {
            GBrainError::Database(format!(
                "No active embedding index found (configure GBRAIN_EMBEDDING_MODEL): {}",
                e
            ))
        })?
    };

    // #7 修复：验证调用方 dimensions 与 DB 一致，不匹配时报错
    if dimensions > 0 && dimensions != dims {
        return Err(GBrainError::Database(format!(
            "维度不一致: 调用方={}, 数据库={}. 请检查 GBRAIN_EMBEDDING_MODEL 配置.",
            dimensions, dims
        )));
    }
    let effective_dims = if dimensions > 0 { dimensions } else { dims };
    info!(index_id, dims = effective_dims, "Starting synonym mining");

    // 2. Extract candidate tokens with DF
    let candidates = extract_candidate_tokens(conn, opts)?;
    stats.candidates = candidates.len();
    info!(candidates = candidates.len(), "Extracted candidate tokens");
    if candidates.is_empty() {
        info!("No candidate tokens found, nothing to mine");
        return Ok(stats);
    }

    // 3. Load existing embeddings
    let mut all_embeddings = load_existing_token_embeddings(conn, index_id)?;

    // 4. Filter new tokens (unless full rebuild)
    let new_tokens: Vec<(String, usize)> = if opts.full {
        candidates
    } else {
        candidates
            .into_iter()
            .filter(|(t, _)| !all_embeddings.contains_key(t))
            .collect()
    };
    info!(
        new_tokens = new_tokens.len(),
        existing = all_embeddings.len(),
        "Token breakdown"
    );

    // 5. Embed new tokens
    if !new_tokens.is_empty() {
        let (embedded, new_embs) = embed_and_store_tokens(
            conn,
            embedder,
            rt,
            &new_tokens,
            index_id,
            opts.batch_size,
            opts.concurrency,
        )?;
        stats.new_embeddings = embedded;
        // #12 修复：合并新向量到已有 embedding，避免全量重载
        for (token, vec) in new_embs {
            all_embeddings.insert(token, vec);
        }
    }
    stats.total_embeddings = all_embeddings.len();

    // 6. KNN mine（优先 sqlite-vec，回退到内存暴力搜索）
    let pairs = knn_mine(conn, index_id, effective_dims, &all_embeddings, opts);
    info!(candidate_pairs = pairs.len(), "KNN mining complete");

    // #3 修复：full=true 时先清除该 index 的旧 synonym，避免低分旧 pair 残留
    if opts.full {
        conn.execute(
            "DELETE FROM kb_token_synonyms WHERE embedding_index_id = ?1",
            params![index_id],
        )
        .map_err(|e| GBrainError::Database(e.to_string()))?;
        info!(index_id, "已清除旧 synonym 表（full=true 全量重挖）");
    }

    // 7. Write synonyms bidirectionally
    let written = write_synonyms_bidirectional(conn, pairs, index_id, opts.max_synonyms_per_token)?;
    stats.synonyms_written = written;
    info!(written, "Synonym mining complete");

    Ok(stats)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(schema::SCHEMA_DDL).unwrap();
        conn
    }

    #[test]
    fn token_vec_table_ddl_uses_sqlite_vec_distance_metric_syntax() {
        let ddl = token_vec_table_ddl("vec_token_synonyms_1", 1536);
        assert!(ddl.contains("embedding float[1536] distance_metric=cosine"));
        assert!(!ddl.contains("metric cosine"));
    }

    /// 插入 library + active embedding index，返回 index id
    fn insert_active_index(conn: &Connection, library_id: i64) -> i64 {
        conn.execute(
            "INSERT OR IGNORE INTO kb_libraries (id, name) VALUES (?1, 'test')",
            params![library_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_embedding_indexes \
             (id, library_id, provider, model, dimensions, index_type, is_active) \
             VALUES (1, ?1, 'openai', 'text-embedding-3-large', 1536, 'dense', 1)",
            params![library_id],
        )
        .unwrap();
        1
    }

    // -- PR-2 lookup tests ---------------------------------------------------

    #[test]
    fn empty_when_no_active_index() {
        let conn = test_conn();
        let result = lookup_token_synonyms(&conn, "积分", 3, None);
        assert!(result.is_empty());
    }

    #[test]
    fn empty_when_table_empty() {
        let conn = test_conn();
        insert_active_index(&conn, 1);
        let result = lookup_token_synonyms(&conn, "积分", 3, None);
        assert!(result.is_empty());
    }

    #[test]
    fn returns_matching_synonyms() {
        let conn = test_conn();
        let idx_id = insert_active_index(&conn, 1);
        conn.execute(
            "INSERT INTO kb_token_synonyms (token, synonym, score, embedding_index_id) \
             VALUES ('积分', 'points', 0.92, ?1)",
            params![idx_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_token_synonyms (token, synonym, score, embedding_index_id) \
             VALUES ('积分', 'lp', 0.88, ?1)",
            params![idx_id],
        )
        .unwrap();

        let result = lookup_token_synonyms(&conn, "积分", 3, None);
        assert_eq!(result, vec!["points", "lp"]);
    }

    #[test]
    fn respects_limit() {
        let conn = test_conn();
        let idx_id = insert_active_index(&conn, 1);
        conn.execute(
            "INSERT INTO kb_token_synonyms (token, synonym, score, embedding_index_id) \
             VALUES ('积分', 'points', 0.92, ?1)",
            params![idx_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_token_synonyms (token, synonym, score, embedding_index_id) \
             VALUES ('积分', 'lp', 0.88, ?1)",
            params![idx_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_token_synonyms (token, synonym, score, embedding_index_id) \
             VALUES ('积分', 'credit', 0.85, ?1)",
            params![idx_id],
        )
        .unwrap();

        let result = lookup_token_synonyms(&conn, "积分", 2, None);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0], "points"); // 最高分在前
    }

    #[test]
    fn no_match_returns_empty() {
        let conn = test_conn();
        insert_active_index(&conn, 1);
        let result = lookup_token_synonyms(&conn, "nonexistent_token", 3, None);
        assert!(result.is_empty());
    }

    #[test]
    fn index_isolation() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO kb_libraries (id, name) VALUES (1, 'test1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_embedding_indexes \
             (id, library_id, provider, model, dimensions, index_type, is_active) \
             VALUES (1, 1, 'openai', 'text-embedding-3-large', 1536, 'dense', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_libraries (id, name) VALUES (2, 'test2')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_embedding_indexes \
             (id, library_id, provider, model, dimensions, index_type, is_active) \
             VALUES (2, 2, 'openai', 'text-embedding-3-large', 1536, 'dense', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kb_token_synonyms \
             (token, synonym, score, embedding_index_id) VALUES ('积分', 'points', 0.92, 2)",
            [],
        )
        .unwrap();

        let result = lookup_token_synonyms(&conn, "积分", 3, None);
        assert!(result.is_empty());
    }

    /// #1 修复测试：验证按 library_id 查找 active index
    #[test]
    fn library_scoped_index_lookup() {
        let conn = test_conn();
        // Library 1: active index (id=1, dims=1536)
        conn.execute("INSERT INTO kb_libraries (id, name) VALUES (1, 'lib1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO kb_embedding_indexes \
             (id, library_id, provider, model, dimensions, index_type, is_active) \
             VALUES (1, 1, 'openai', 'text-embedding-3-large', 1536, 'dense', 1)",
            [],
        )
        .unwrap();
        // Library 2: active index (id=2, dims=768) — 不同模型
        conn.execute("INSERT INTO kb_libraries (id, name) VALUES (2, 'lib2')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO kb_embedding_indexes \
             (id, library_id, provider, model, dimensions, index_type, is_active) \
             VALUES (2, 2, 'openai', 'text-embedding-3-small', 768, 'dense', 1)",
            [],
        )
        .unwrap();

        // 按 library_id=1 查找 → 应返回 index 1
        assert_eq!(active_embedding_index_id(&conn, Some(1)), Some(1));
        // 按 library_id=2 查找 → 应返回 index 2
        assert_eq!(active_embedding_index_id(&conn, Some(2)), Some(2));
        // 全局查找 → 返回任意一个（行为未定，只验证不 panic）
        assert!(active_embedding_index_id(&conn, None).is_some());
    }

    // -- PR-3 mining tests ---------------------------------------------------

    #[test]
    fn cosine_similarity_basic() {
        let a = vec![1.0_f32, 0.0, 0.0];
        let b = vec![0.0_f32, 1.0, 0.0];
        assert!((cosine_similarity(&a, &b) - 0.0).abs() < 1e-6);

        let c = vec![1.0_f32, 0.0, 0.0];
        assert!((cosine_similarity(&a, &c) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn substring_filter() {
        // 短 token（长度 <= 2）跳过子串检查，避免误过滤
        assert!(!is_substring_or_superstring("积分", "分")); // "分" 长度=1，跳过
        assert!(is_substring_or_superstring("transaction", "trans"));
        assert!(!is_substring_or_superstring("积分", "points"));
    }

    #[test]
    fn blob_roundtrip() {
        let original = vec![1.0_f32, -2.5, 0.0, std::f32::consts::PI];
        let blob = embedding_to_blob(&original);
        let restored = blob_to_f32(&blob);
        assert_eq!(original, restored);
    }

    #[test]
    fn write_bidirectional_creates_both_directions() {
        let conn = test_conn();
        let idx_id = insert_active_index(&conn, 1);

        let pairs = vec![("积分".to_string(), "points".to_string(), 0.92_f32)];
        let written = write_synonyms_bidirectional(&conn, pairs, idx_id, 5).unwrap();
        assert_eq!(written, 2); // A→B + B→A

        // 验证双向
        let ab: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT synonym FROM kb_token_synonyms WHERE token = '积分' AND embedding_index_id = ?1")
                .unwrap();
            stmt.query_map(params![idx_id], |row| row.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        assert_eq!(ab, vec!["points"]);

        let ba: Vec<String> = {
            let mut stmt = conn
                .prepare("SELECT synonym FROM kb_token_synonyms WHERE token = 'points' AND embedding_index_id = ?1")
                .unwrap();
            stmt.query_map(params![idx_id], |row| row.get::<_, String>(0))
                .unwrap()
                .flatten()
                .collect()
        };
        assert_eq!(ba, vec!["积分"]);
    }

    #[test]
    fn write_bidirectional_respects_max_per_token() {
        let conn = test_conn();
        let idx_id = insert_active_index(&conn, 1);

        let pairs = vec![
            ("a".to_string(), "b".to_string(), 0.9),
            ("a".to_string(), "c".to_string(), 0.8),
            ("a".to_string(), "d".to_string(), 0.7),
        ];
        let written = write_synonyms_bidirectional(&conn, pairs, idx_id, 2).unwrap();
        // a 有 3 个 synonym → 截断到 2 (b, c)
        // b 有 1 个 synonym (a) → 1
        // c 有 1 个 synonym (a) → 1
        // d 有 1 个 synonym (a) → 1（反向独立于正向截断）
        assert_eq!(written, 5); // a→b, a→c, b→a, c→a, d→a
    }

    #[test]
    fn knn_mine_basic() {
        // 构造 embedding：a 和 b 相似，c 正交
        let all: HashMap<String, Vec<f32>> = HashMap::from([
            ("a".to_string(), vec![1.0, 0.0]),
            ("b".to_string(), vec![0.95, 0.05]), // 与 a 非常接近
            ("c".to_string(), vec![0.0, 1.0]),   // 正交
        ]);
        let opts = MineSynonymsOpts {
            knn_k: 3,
            similarity_threshold: 0.9,
            ..Default::default()
        };
        let pairs = knn_mine_brute_force(&all, &opts);
        // a↔b 应被发现，c 无高于 0.9 的 pair
        assert!(pairs.iter().any(|(a, b, _)| a == "a" && b == "b"));
        assert!(pairs.iter().any(|(a, b, _)| a == "b" && b == "a"));
        assert!(!pairs.iter().any(|(a, _, _)| a == "c"));
    }

    #[test]
    fn knn_mine_filters_substrings() {
        let all: HashMap<String, Vec<f32>> = HashMap::from([
            ("trans".to_string(), vec![1.0, 0.0]),
            ("transaction".to_string(), vec![0.99, 0.01]), // 相似且包含关系
        ]);
        let opts = MineSynonymsOpts {
            knn_k: 3,
            similarity_threshold: 0.9,
            ..Default::default()
        };
        let pairs = knn_mine_brute_force(&all, &opts);
        assert!(pairs.is_empty()); // "trans" ⊂ "transaction" 被过滤
    }

    /// #4 修复测试：验证 brute-force 返回 knn_k 个而非 knn_k-1 个
    #[test]
    fn knn_mine_brute_force_returns_k_neighbors() {
        let all: HashMap<String, Vec<f32>> = HashMap::from([
            ("a".to_string(), vec![1.0, 0.0]),
            ("b".to_string(), vec![0.99, 0.01]),
            ("c".to_string(), vec![0.98, 0.02]),
            ("d".to_string(), vec![0.97, 0.03]),
        ]);
        let opts = MineSynonymsOpts {
            knn_k: 3,
            similarity_threshold: 0.9,
            ..Default::default()
        };
        let pairs = knn_mine_brute_force(&all, &opts);
        // token "a" 应有 3 个 neighbor (b, c, d)，而非旧版的 2 个 (off-by-one)
        let a_neighbors: Vec<_> = pairs.iter().filter(|(t, _, _)| t == "a").collect();
        assert_eq!(a_neighbors.len(), 3, "knn_k=3 应返回 3 个邻居，而非 2 个");
    }

    #[test]
    fn extract_candidates_respects_df_bounds() {
        let conn = test_conn();
        insert_active_index(&conn, 1);
        // 插入父文档（FK 约束）
        conn.execute(
            "INSERT INTO kb_documents \
             (id, library_id, original_name, content_hash, extension, mime_type) \
             VALUES (1, 1, 'test', 'hash', 'txt', 'text/plain')",
            [],
        )
        .unwrap();
        // 10 个节点含 "积分"（常见词），3 个节点含 "生僻"（罕见词），
        // 5 个节点不含任何关键词（背景噪声）
        for i in 0..10u32 {
            conn.execute(
                "INSERT INTO kb_document_nodes (library_id, document_id, content, level) \
                 VALUES (1, 1, ?1, 0)",
                params![format!("积分系统使用说明第{}页", i)],
            )
            .unwrap();
        }
        for i in 0..3u32 {
            conn.execute(
                "INSERT INTO kb_document_nodes (library_id, document_id, content, level) \
                 VALUES (1, 1, ?1, 0)",
                params![format!("生僻概念解释说明第{}页", i)],
            )
            .unwrap();
        }
        for i in 0..5u32 {
            conn.execute(
                "INSERT INTO kb_document_nodes (library_id, document_id, content, level) \
                 VALUES (1, 1, ?1, 0)",
                params![format!("通用背景文档第{}页", i)],
            )
            .unwrap();
        }

        let opts = MineSynonymsOpts {
            min_doc_freq: 5,         // "生僻" 不通过 (DF=3)
            max_doc_freq_ratio: 0.9, // "积分" 通过 (DF=10/18=56%)
            min_token_char_len: 2,
            ..Default::default()
        };
        let tokens = extract_candidate_tokens(&conn, &opts).unwrap();
        let token_set: Vec<&str> = tokens.iter().map(|(t, _)| t.as_str()).collect();
        // "积分" 应出现 (DF=10 >= 5, 10/18=56% <= 90%)
        assert!(
            token_set.iter().any(|t| t.contains("积分")),
            "expected 积分 in {:?}",
            token_set
        );
        // "生僻" 不应出现 (DF=3 < min_doc_freq=5)
        assert!(
            !token_set.iter().any(|t| t.contains("生僻")),
            "生僻 should have been filtered by min_doc_freq"
        );
    }
}
