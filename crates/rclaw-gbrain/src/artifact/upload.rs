//! 统一上传引擎 — upload_source 的核心实现
//!
//! 负责：
//! 1. 计算文件 SHA256 去重
//! 2. 写入 Artifact Store（按 hash 命名）
//! 3. 创建/复用 source_artifacts 记录
//! 4. 创建 artifact_occurrences 记录
//! 5. 根据路由计划创建各投影（KB / gbrain / 文件存储）
//! 6. 创建影子页面

use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info};

use crate::artifact::projection;
use crate::error::{GBrainError, Result};
use crate::security::validate_page_slug;

use super::store;
use super::types::*;

/// 计算内容的 SHA256 哈希
pub fn compute_sha256(content: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content);
    format!("{:x}", hasher.finalize())
}

/// 生成 artifact_uid（art_ + 12位hash前缀 + 4位随机后缀）
pub fn generate_artifact_uid(sha256: &str) -> String {
    let prefix = &sha256[..12.min(sha256.len())];
    // 使用时间戳低 16 位作为伪随机后缀
    let suffix = format!(
        "{:04x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 65536
    );
    format!("art_{}{}", prefix, suffix)
}

/// 生成 occurrence_uid（occ_ + 12位hash前缀 + 4位随机后缀）
pub fn generate_occurrence_uid(artifact_uid: &str) -> String {
    let suffix = format!(
        "{:04x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 65536
    );
    format!(
        "occ_{}{}",
        &artifact_uid[..16.min(artifact_uid.len())],
        suffix
    )
}

/// 获取当前时间字符串
fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// 写入 Artifact Store（按 hash 去重，路径: $GBRAIN_DIR/artifacts/<sha256[0..2]>/<sha256>.<ext>）
pub fn write_artifact_file(
    content: &[u8],
    sha256: &str,
    extension: &str,
    artifact_dir: &Path,
) -> Result<PathBuf> {
    // 按前两位 hash 分桶
    let bucket = &sha256[..2.min(sha256.len())];
    let dir = artifact_dir.join(bucket);
    fs::create_dir_all(&dir)
        .map_err(|e| GBrainError::FileError(format!("创建 artifact 目录失败: {}", e)))?;

    let filename = if extension.is_empty() {
        sha256.to_string()
    } else {
        format!("{}.{}", sha256, extension)
    };
    let path = dir.join(&filename);

    // write-if-absent：hash 命名文件已存在时必须校验内容，避免复用损坏文件。
    if path.exists() {
        if verify_artifact_integrity(&path, sha256)? {
            debug!("Artifact 文件已存在，跳过写入: {:?}", path);
            return Ok(path);
        }
        return Err(GBrainError::FileError(format!(
            "artifact 文件已存在但内容 hash 不匹配: {}",
            path.display()
        )));
    }

    // 原子 no-clobber 写入：先写唯一临时文件，再用 hard_link 创建目标。
    // hard_link 在目标已存在时失败，不会像 Unix rename 那样覆盖并发写入的目标。
    let tmp_path = dir.join(format!(
        "{}.{}.tmp",
        sha256,
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| GBrainError::FileError(format!("创建临时文件失败: {}", e)))?;
    file.write_all(content)
        .map_err(|e| GBrainError::FileError(format!("写入临时文件失败: {}", e)))?;
    file.sync_all()
        .map_err(|e| GBrainError::FileError(format!("同步临时文件失败: {}", e)))?;
    drop(file);

    match fs::hard_link(&tmp_path, &path) {
        Ok(()) => {
            let _ = fs::remove_file(&tmp_path);
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let _ = fs::remove_file(&tmp_path);
            if verify_artifact_integrity(&path, sha256)? {
                return Ok(path);
            }
            return Err(GBrainError::FileError(format!(
                "并发写入后 artifact 内容 hash 不匹配: {}",
                path.display()
            )));
        }
        Err(e) => {
            // hard_link 在部分 Android/FUSE 文件系统上会被 SELinux 拒绝或不受支持。
            // 回退到 rename：同盘 rename 是原子的，
            // 文件名即 sha256，并发写入同名的另一进程内容必然相同，rename 即使覆盖也安全。
            if let Err(rerr) = fs::rename(&tmp_path, &path) {
                let _ = fs::remove_file(&tmp_path);
                return Err(GBrainError::FileError(format!(
                    "安装 artifact 文件失败: hard_link={}, rename={}",
                    e, rerr
                )));
            }
        }
    }

    Ok(path)
}

/// 验证 artifact 文件完整性
pub fn verify_artifact_integrity(path: &Path, expected_sha256: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let content = fs::read(path)
        .map_err(|e| GBrainError::FileError(format!("读取 artifact 文件失败: {}", e)))?;
    let actual_sha256 = compute_sha256(&content);
    Ok(actual_sha256 == expected_sha256)
}

/// 统一上传核心逻辑
///
/// 步骤：
/// 1. 计算 SHA256
/// 2. 查找已有 artifact（去重）
/// 3. 写入 artifact store
/// 4. 创建/复用 source_artifacts 记录
/// 5. 创建 artifact_occurrences 记录
/// 6. 根据路由计划创建投影
#[allow(clippy::too_many_arguments)]
pub fn upload_source(
    conn: &Connection,
    input: &UploadSourceInput,
    artifact_dir: &Path,
    _gbrain_dir: &Path,
    config_default_library_id: Option<i64>,
    config_embedding_model: &str,
    config_embedding_dimensions: usize,
    config_default_promotion_policy: &str,
    auto_create_inbox: bool,
) -> Result<UploadSourceOutput> {
    info!(
        "upload_source start: filename={}, intent={}, content_len={}, dry_run={}",
        input.original_name,
        input.intent,
        input.content.len(),
        input.dry_run
    );
    // 1. 计算 SHA256
    let sha256 = compute_sha256(&input.content);
    debug!("upload_source: sha256={}", sha256);

    // 2. 推断扩展名和 MIME 类型
    let extension = infer_extension(&input.original_name);
    let mime_type = infer_mime_type(&extension);

    // 3. 推断路由计划
    let route_plan = infer_route_plan(&extension, &mime_type, &input.intent);

    // 应用 promotion 策略：用户显式指定 > config 默认值 > intent 推断
    let route_plan = apply_promotion_policy(
        route_plan,
        &input.promotion_policy,
        config_default_promotion_policy,
    );

    // dry_run 模式：仅返回路由计划，不实际写入（必须在任何落库/落盘之前判断）
    if input.dry_run {
        return Ok(UploadSourceOutput {
            artifact_id: 0,
            artifact_uid: String::new(),
            occurrence_id: 0,
            occurrence_uid: String::new(),
            sha256,
            is_new: true,
            route_plan,
            projections: Vec::new(),
        });
    }

    // 4. 查找已有 artifact
    let existing = store::find_artifact_by_sha256(conn, &sha256)
        .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?;

    let (artifact_id, artifact_uid, is_new) = if let Some(existing) = existing {
        // 修复：复用已软删除的 artifact 时需要 reactivation，
        // 否则新 occurrence/projection 会挂到 deleted artifact 上，
        // 列表查不到，GC 又把它当孤儿投影处理
        if existing.status == "deleted" {
            store::reactivate_artifact(conn, existing.id)
                .map_err(|e| GBrainError::Database(format!("重新激活 artifact 失败: {}", e)))?;
        } else {
            store::touch_artifact(conn, existing.id)
                .map_err(|e| GBrainError::Database(format!("更新 artifact 失败: {}", e)))?;
        }
        (existing.id, existing.artifact_uid.clone(), false)
    } else {
        // 5. 写入 artifact store
        let storage_path = write_artifact_file(&input.content, &sha256, &extension, artifact_dir)?;
        let storage_path_str = storage_path.to_string_lossy().to_string();

        // 6. 生成 UID 和 slug
        let artifact_uid = generate_artifact_uid(&sha256);
        let canonical_slug = make_canonical_slug(&input.original_name, &sha256);

        let now = now_str();
        let artifact = SourceArtifact {
            id: 0,
            artifact_uid: artifact_uid.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            last_seen_at: Some(now.clone()),
            sha256: sha256.clone(),
            original_name: input.original_name.clone(),
            extension: extension.clone(),
            mime_type: mime_type.clone(),
            size_bytes: input.content.len() as i64,
            storage_path: storage_path_str,
            canonical_slug: canonical_slug.clone(),
            status: "active".to_string(),
            metadata_json: input
                .metadata
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_else(|| "{}".to_string()),
            deleted_at: None,
            purged_at: None,
        };

        let id = store::insert_artifact(conn, &artifact)
            .map_err(|e| GBrainError::Database(format!("插入 artifact 失败: {}", e)))?;

        // 记录 artifact_created 事件（§7.6）
        let _ = store::record_event(
            conn,
            Some(id),
            None,
            "artifact_created",
            "upload_source",
            &serde_json::json!({"sha256": sha256, "original_name": input.original_name})
                .to_string(),
        );

        (id, artifact_uid, true)
    };

    // 7. 创建 occurrence
    let occurrence_uid = generate_occurrence_uid(&artifact_uid);
    let now = now_str();

    // 设计文档 §17: 如果 library_id 为 None 但路由计划需要 KB，
    // 将在投影创建阶段自动解析默认库，occurrence 中记录原始输入的 library_id
    let occurrence = ArtifactOccurrence {
        id: 0,
        occurrence_uid: occurrence_uid.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id,
        source_kind: input.source_kind.to_string(),
        source_uri: input.source_uri.clone(),
        // 修复：从 input.path 或 source_uri 传入 original_path，
        // 之前固定写空字符串，导致 KB document 的 original_path 也继承空值，
        // 削弱后续审计、追溯和恢复诊断能力
        original_path: input
            .path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| input.source_uri.clone()),
        original_name: input.original_name.clone(),
        owner_ref: input.owner_ref.clone().unwrap_or_default(),
        intent: input.intent.to_string(),
        target_slug: input.target_slug.clone().unwrap_or_default(),
        page_slug: input.page_slug.clone().unwrap_or_default(),
        library_id: input.library_id,
        folder_id: input.folder_id,
        // 修复：使用最终决定的 promotion（可能是 intent 推断的或用户显式指定的）
        promotion_policy: route_plan.promotion.to_string(),
        status: "active".to_string(),
        stale_reason: String::new(),
        metadata_json: input
            .metadata
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "{}".to_string()),
    };

    let occurrence_id = store::insert_occurrence(conn, &occurrence)
        .map_err(|e| GBrainError::Database(format!("插入 occurrence 失败: {}", e)))?;

    // 记录 occurrence_created 事件（§7.6）
    let _ = store::record_event(
        conn,
        Some(artifact_id),
        Some(occurrence_id),
        "occurrence_created",
        "upload_source",
        &serde_json::json!({"source_kind": input.source_kind, "intent": input.intent}).to_string(),
    );

    // 9. 创建投影
    let mut projections = Vec::new();
    let mut kb_document_id_for_shadow: Option<i64> = None;

    // KB 投影 — 设计文档 §17 默认 library 策略优先级链：
    // 1. 用户显式 library_id
    // 2. config: default_kb_library_id
    // 3. 名为 Default 或 Inbox 的 library
    // 4. 不存在则自动创建 Inbox
    if route_plan.to_kb {
        let resolved_library_id = resolve_default_library(
            conn,
            input.library_id,
            config_default_library_id,
            config_embedding_model,
            config_embedding_dimensions,
            auto_create_inbox,
        )?;
        let proj_key = format!("library:{}", resolved_library_id);
        // 使用 projection::create_kb_projection 创建 KB 投影和 kb_document
        // 它会自动更新 projection_ref 为实际的 kb_document:{id}
        let proj_ref =
            projection::create_kb_projection(conn, artifact_id, occurrence_id, resolved_library_id)
                .map_err(|e| GBrainError::Database(format!("创建 KB 投影失败: {}", e)))?;
        // 修复：从 proj_ref 解析 kb_document_id，传给 shadow page frontmatter
        kb_document_id_for_shadow = proj_ref
            .strip_prefix("kb_document:")
            .and_then(|s| s.parse::<i64>().ok());
        projections.push(ProjectionResult {
            projection_type: ProjectionType::KbDocument,
            projection_key: proj_key,
            projection_ref: proj_ref,
            created: true,
            status: "active".to_string(),
        });
    }

    // 影子页面投影 — 设计文档 §9/§15.1: 使用 put_page 创建 documents/<slug> 页面
    // 影子页面在 upload_source 阶段直接写入 pages 表（MVP 最小追溯链要求）
    let mut _shadow_page_slug: Option<String> = None;
    if route_plan.to_shadow {
        let slug = format!(
            "documents/{}",
            make_canonical_slug(&input.original_name, &sha256)
        );
        // 验证 slug
        if validate_page_slug(&slug).is_ok() {
            let proj_key = format!("slug:{}", slug);
            let proj_ref = format!("slug:{}", slug);
            let now = now_str();
            let proj = ArtifactProjection {
                id: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                artifact_id,
                occurrence_id: Some(occurrence_id),
                projection_type: ProjectionType::BrainShadowPage.to_string(),
                projection_key: proj_key.clone(),
                projection_ref: proj_ref.clone(),
                status: "active".to_string(),
                version_hash: String::new(),
                stale_reason: String::new(),
                metadata_json: "{}".to_string(),
                superseded_by: None,
            };
            store::insert_projection(conn, &proj)
                .map_err(|e| GBrainError::Database(format!("插入影子页面投影失败: {}", e)))?;
            projections.push(ProjectionResult {
                projection_type: ProjectionType::BrainShadowPage,
                projection_key: proj_key,
                projection_ref: proj_ref,
                created: true,
                status: "active".to_string(),
            });

            // 实际写入 gbrain pages 表（§9.2/§9.3 frontmatter + body 模板）
            // §15.2 强制规则: shadow page 中候选实体不使用 wikilink
            let artifact = store::find_artifact_by_id(conn, artifact_id)
                .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?
                .ok_or_else(|| {
                    GBrainError::PageNotFound(format!("artifact {} 不存在", artifact_id))
                })?;
            let (title, frontmatter, body) =
                create_shadow_page_content(&artifact, &occurrence, kb_document_id_for_shadow);

            // 修复：补齐 title_tokens、compiled_truth_tokens、content_hash，
            // 与 put_page 保持一致，确保中文分词索引正确
            let title_tokens = crate::nlp::chinese::tokenize_content(&title);
            let truth_tokens = crate::nlp::chinese::tokenize_content(&body);
            let content_hash = {
                use sha2::{Digest, Sha256};
                let mut hasher = Sha256::new();
                hasher.update(title.as_bytes());
                hasher.update(body.as_bytes());
                format!("{:x}", hasher.finalize())
            };

            // 修复：之前使用 INSERT OR REPLACE，重复上传同一 artifact 时会覆盖
            // 人工维护过的 shadow page 内容和 page id。
            // 改为 INSERT OR IGNORE：已存在时不覆盖，只补充缺失的 frontmatter/metadata
            let rows_affected = conn.execute(
                "INSERT OR IGNORE INTO pages (slug, title, compiled_truth, page_type, frontmatter, content_hash, title_tokens, compiled_truth_tokens, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'source', ?4, ?5, ?6, ?7, datetime('now'), datetime('now'))",
                rusqlite::params![slug, title, body, frontmatter, content_hash, title_tokens, truth_tokens],
            ).map_err(|e| GBrainError::Database(format!("写入影子页面失败: {}", e)))?;

            if rows_affected > 0 {
                debug!("shadow page created: {}", slug);
                // P3 修复：使用 chunker 对全文分块，而非只取前 800 字插入单个 chunk。
                // shadow page 超过 800 字后，后续内容不会进入 chunk 索引，
                // 导致长文档后半段的事实无法被搜索命中。
                let chunks = crate::chunker::chunk_page_content_transactional(
                    &body,
                    &crate::types::PageType::Source,
                );
                for chunk in &chunks {
                    let chunk_text_tokens =
                        crate::nlp::chinese::tokenize_content(&chunk.chunk_text);
                    let token_count = chunk_text_tokens.split_whitespace().count() as i64;
                    conn.execute(
                        "INSERT INTO chunks (page_id, chunk_index, chunk_text, chunk_text_tokens, token_count, chunk_source, created_at)
                         VALUES ((SELECT id FROM pages WHERE slug = ?1), ?2, ?3, ?4, ?5, 'body', datetime('now'))",
                        rusqlite::params![slug, chunk.chunk_index, chunk.chunk_text, chunk_text_tokens, token_count],
                    )
                    .map_err(|e| GBrainError::Database(format!("创建影子页面 chunk 失败: {}", e)))?;
                }
            } else {
                // 页面已存在，只更新 frontmatter 和 content_hash（不覆盖 compiled content）
                debug!("shadow page 已存在，补充 frontmatter: {}", slug);
                // L1: 补充 frontmatter 是最佳努力操作，但失败时仍需记录
                if let Err(e) = conn.execute(
                    "UPDATE pages SET frontmatter = ?1, content_hash = ?2, updated_at = datetime('now')
                     WHERE slug = ?3 AND (frontmatter IS NULL OR frontmatter = '' OR content_hash IS NULL OR content_hash = '')",
                    rusqlite::params![frontmatter, content_hash, slug],
                ) {
                    tracing::warn!(slug = %slug, error = %e, "补充影子页面 frontmatter 失败");
                }
            }

            _shadow_page_slug = Some(slug);
        }
    }

    // 文件存储投影
    if route_plan.to_file {
        if let Some(page_slug) = &input.page_slug {
            let proj_key = format!("page:{}:file:{}", page_slug, input.original_name);
            let proj_ref = format!("file:{}", artifact_id);
            let now = now_str();
            let proj = ArtifactProjection {
                id: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                artifact_id,
                occurrence_id: Some(occurrence_id),
                projection_type: ProjectionType::FileAttachment.to_string(),
                projection_key: proj_key.clone(),
                projection_ref: proj_ref.clone(),
                status: "active".to_string(),
                version_hash: String::new(),
                stale_reason: String::new(),
                metadata_json: "{}".to_string(),
                superseded_by: None,
            };
            store::insert_projection(conn, &proj)
                .map_err(|e| GBrainError::Database(format!("插入文件投影失败: {}", e)))?;
            projections.push(ProjectionResult {
                projection_type: ProjectionType::FileAttachment,
                projection_key: proj_key,
                projection_ref: proj_ref,
                created: true,
                status: "active".to_string(),
            });
        }
    }

    // gbrain page 投影
    if route_plan.to_brain {
        if let Some(target_slug) = &input.target_slug {
            let proj_key = format!("slug:{}", target_slug);
            let proj_ref = format!("slug:{}", target_slug);
            let now = now_str();
            let proj = ArtifactProjection {
                id: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                artifact_id,
                occurrence_id: Some(occurrence_id),
                projection_type: ProjectionType::BrainPageUpdate.to_string(),
                projection_key: proj_key.clone(),
                projection_ref: proj_ref.clone(),
                status: "active".to_string(),
                version_hash: String::new(),
                stale_reason: String::new(),
                metadata_json: "{}".to_string(),
                superseded_by: None,
            };
            store::insert_projection(conn, &proj)
                .map_err(|e| GBrainError::Database(format!("插入 brain page 投影失败: {}", e)))?;
            projections.push(ProjectionResult {
                projection_type: ProjectionType::BrainPageUpdate,
                projection_key: proj_key,
                projection_ref: proj_ref,
                created: true,
                status: "active".to_string(),
            });
        }
    }

    let output = UploadSourceOutput {
        artifact_id,
        artifact_uid,
        occurrence_id,
        occurrence_uid,
        sha256,
        is_new,
        route_plan,
        projections,
    };
    info!(
        "upload_source complete: artifact_id={}, artifact_uid={}, is_new={}",
        output.artifact_id, output.artifact_uid, output.is_new
    );
    Ok(output)
}

/// 创建影子页面
///
/// 使用现有 put_page 创建 `documents/<slug>` 页面，
/// frontmatter 包含 artifact_id、occurrence_id、kb_document_id、source_hash 等。
pub fn create_shadow_page_content(
    artifact: &SourceArtifact,
    occurrence: &ArtifactOccurrence,
    kb_document_id: Option<i64>,
) -> (String, String, String) {
    // 标题
    let title = if artifact.original_name.is_empty() {
        artifact.canonical_slug.clone()
    } else {
        artifact.original_name.clone()
    };

    // 修复：frontmatter 使用 JSON 格式（operations.rs:1232 按 JSON 解析 frontmatter）
    // 之前写成 YAML 格式（---key: value---），导致 frontmatter 解析失败
    let frontmatter = serde_json::json!({
        "page_type": "source",
        "artifact_id": artifact.artifact_uid,
        "artifact_occurrence_id": occurrence.occurrence_uid,
        "kb_document_id": kb_document_id.map_or(serde_json::Value::Null, |id| serde_json::json!(id)),
        "source_hash": format!("sha256:{}", artifact.sha256),
        "source_type": "uploaded_document",
        "source_mime_type": artifact.mime_type,
        "source_original_name": artifact.original_name,
        "source_ref": format!("artifact://{}", artifact.artifact_uid),
        "promotion_status": "auto_summary",
        "projection_status": "active",
    }).to_string();

    // body
    let size_mb = artifact.size_bytes as f64 / 1024.0 / 1024.0;
    let body = format!(
        "# {}\n\n\
         ## Summary\n\n\
         Pending KB processing\n\n\
         ## Key Details\n\n\
         - Type: {}\n\
         - Size: {:.1} MB\n\
         - KB Document: {}\n\
         - Artifact: {}\n\
         - Hash: sha256:{}\n\n\
         ## Entities\n\n\
         Pending review\n\n\
         ## Candidate Timeline\n\n\
         Pending review\n\n\
         ## Source\n\n\
         - Original: artifact://{}\n\
         - Occurrence: occurrence://{}\n\
         - KB: kb_document://{}",
        title,
        artifact.extension.to_uppercase(),
        size_mb,
        kb_document_id.map_or("null".to_string(), |id| id.to_string()),
        artifact.artifact_uid,
        artifact.sha256,
        artifact.artifact_uid,
        occurrence.occurrence_uid,
        kb_document_id.map_or("null".to_string(), |id| id.to_string()),
    );

    (title, frontmatter, body)
}

/// 解析默认 library ID
///
/// 设计文档 §17 优先级链：
/// 1. 用户显式 library_id（已传入）
/// 2. config: default_kb_library_id
/// 3. 名为 Default 或 Inbox 的 library
/// 4. auto_create_inbox=true 时自动创建 Inbox，否则返回错误
fn resolve_default_library(
    conn: &Connection,
    explicit_library_id: Option<i64>,
    config_default_library_id: Option<i64>,
    config_embedding_model: &str,
    config_embedding_dimensions: usize,
    auto_create_inbox: bool,
) -> Result<i64> {
    // 1. 用户显式传入
    if let Some(id) = explicit_library_id {
        ensure_default_library_embedding_index(
            conn,
            id,
            config_embedding_model,
            config_embedding_dimensions,
        )?;
        return Ok(id);
    }

    // 2. config 配置的默认库
    if let Some(id) = config_default_library_id {
        // 验证该库存在且未删除
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM kb_libraries WHERE id = ?1 AND deleted_at IS NULL",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap_or(false);
        if exists {
            ensure_default_library_embedding_index(
                conn,
                id,
                config_embedding_model,
                config_embedding_dimensions,
            )?;
            return Ok(id);
        }
    }

    // 3. 从 kb_libraries 表查找名为 "Inbox" 或 "Default" 的库
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM kb_libraries WHERE name IN ('Inbox', 'Default') AND deleted_at IS NULL ORDER BY name LIMIT 1",
            [],
            |row| row.get(0),
        )
        .ok();

    if let Some(id) = existing {
        ensure_default_library_embedding_index(
            conn,
            id,
            config_embedding_model,
            config_embedding_dimensions,
        )?;
        return Ok(id);
    }

    // 4. 没有找到默认库：根据 auto_create_inbox 配置决定是否自动创建
    if !auto_create_inbox {
        return Err(GBrainError::InvalidInput(
            "未找到默认 KB 库且 artifact_auto_create_inbox_library=false，请先创建 KB 库或配置 default_kb_library_id".to_string(),
        ));
    }

    let now = now_str();
    let embedding_model = normalized_embedding_model(config_embedding_model);
    let embedding_dimensions = normalized_embedding_dimensions(config_embedding_dimensions)?;
    conn.execute(
        "INSERT INTO kb_libraries
            (name, embedding_provider, embedding_model, embedding_dimensions, raptor_enabled, created_at, updated_at)
         VALUES ('Inbox', 'openai', ?1, ?2, 1, ?3, ?3)",
        rusqlite::params![embedding_model, embedding_dimensions, now],
    )
    .map_err(|e| GBrainError::Database(format!("自动创建 Inbox 库失败: {}", e)))?;

    let id = conn.last_insert_rowid();
    ensure_default_library_embedding_index(
        conn,
        id,
        config_embedding_model,
        config_embedding_dimensions,
    )?;
    info!("自动创建默认 Inbox 库: id={}", id);
    Ok(id)
}

fn normalized_embedding_model(config_embedding_model: &str) -> &str {
    let model = config_embedding_model.trim();
    if model.is_empty() {
        "text-embedding-3-large"
    } else {
        model
    }
}

fn normalized_embedding_dimensions(config_embedding_dimensions: usize) -> Result<i32> {
    let dimensions = if config_embedding_dimensions == 0 {
        1536
    } else {
        config_embedding_dimensions
    };
    i32::try_from(dimensions)
        .map_err(|_| GBrainError::InvalidInput("embedding_dimensions 超出 i32 范围".to_string()))
}

fn ensure_default_library_embedding_index(
    conn: &Connection,
    library_id: i64,
    config_embedding_model: &str,
    config_embedding_dimensions: usize,
) -> Result<()> {
    let has_active: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM kb_embedding_indexes
             WHERE library_id = ?1 AND is_active = 1",
            rusqlite::params![library_id],
            |row| row.get(0),
        )
        .unwrap_or(false);
    if has_active {
        return Ok(());
    }

    let model = normalized_embedding_model(config_embedding_model);
    let dimensions = normalized_embedding_dimensions(config_embedding_dimensions)?;

    conn.execute(
        "UPDATE kb_libraries
         SET embedding_provider = CASE
                 WHEN embedding_provider IS NULL OR embedding_provider = '' THEN 'openai'
                 ELSE embedding_provider
             END,
             embedding_model = CASE
                 WHEN embedding_model IS NULL OR embedding_model = '' THEN ?2
                 ELSE embedding_model
             END,
             embedding_dimensions = CASE
                 WHEN embedding_dimensions IS NULL OR embedding_dimensions <= 0 THEN ?3
                 ELSE embedding_dimensions
             END
         WHERE id = ?1",
        rusqlite::params![library_id, model, dimensions],
    )
    .map_err(|e| GBrainError::Database(format!("更新默认 KB 库 embedding 配置失败: {}", e)))?;

    let reusable_index_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM kb_embedding_indexes
             WHERE library_id = ?1 AND model = ?2 AND dimensions = ?3
             ORDER BY id DESC LIMIT 1",
            rusqlite::params![library_id, model, dimensions],
            |row| row.get(0),
        )
        .ok();

    let index_id = match reusable_index_id {
        Some(id) => id,
        None => crate::kb::embedding_index::create_embedding_index(
            conn, library_id, "openai", model, dimensions, "vec0",
        )?,
    };
    crate::kb::embedding_index::activate_index(conn, index_id)?;

    info!(
        library_id,
        index_id, model, dimensions, "确保默认 KB 库存在 active embedding index"
    );

    Ok(())
}

/// 手动写入长期记忆（设计文档 §8.6）
///
/// 创建 text/manual artifact 并投影到 gbrain 页面。
/// P1-2 修复：接收 route_plan 参数，根据 plan 决定创建哪些投影，
/// 而不是固定走 manual memory 路径。确保 dry-run 预览与实际写入一致。
///
/// 步骤：
/// 1. 将 title/content 规范化为 markdown 文本
/// 2. 计算 hash
/// 3. 写 artifact store
/// 4. 创建 source artifact 和 occurrence
/// 5. 根据 route_plan 创建投影（brain_page_update / shadow / KB / file）
/// 6. 可选入 KB
#[allow(clippy::too_many_arguments)]
pub fn put_manual_memory(
    conn: &Connection,
    slug: &str,
    content: &str,
    title: Option<&str>,
    intent: Option<&str>,
    artifact_dir: &Path,
    _gbrain_dir: &Path,
    config_default_library_id: Option<i64>,
    config_embedding_model: &str,
    config_embedding_dimensions: usize,
    // P1-2 修复后不再使用 config_promotion_policy，occurrence 的 promotion_policy
    // 改为从 route_plan.promotion 获取，确保与 dry-run 预览一致
    _config_promotion_policy: &str,
    manual_memory_to_kb: bool,
    auto_create_inbox: bool,
    route_plan: &RoutePlan,
) -> Result<UploadSourceOutput> {
    info!(
        "put_manual_memory: slug={}, intent={:?}, to_brain={}, to_shadow={}, to_kb={}",
        slug, intent, route_plan.to_brain, route_plan.to_shadow, route_plan.to_kb
    );
    // 解析意图
    let resolved_intent = intent.unwrap_or("memory");
    // 1. 规范化为 markdown
    let page_title = title.unwrap_or(slug);
    let md_content = if content.starts_with("---\n") || content.starts_with("---\r\n") {
        content.to_string()
    } else {
        format!("# {}\n\n{}", page_title, content)
    };
    let content_bytes = md_content.as_bytes();

    // 2. 计算 SHA256
    let sha256 = compute_sha256(content_bytes);

    // 3. 写入 artifact store
    let storage_path = write_artifact_file(content_bytes, &sha256, "md", artifact_dir)?;
    let storage_path_str = storage_path.to_str().unwrap_or("unknown").to_string();

    // 4. 创建/复用 source_artifacts 记录
    // 修复：复用已软删除的 artifact 时需要 reactivation，
    // 否则新 occurrence/projection 会挂到 deleted artifact 上，
    // 列表查不到，GC 又把它当孤儿投影处理
    let now = now_str();
    let existing = store::find_artifact_by_sha256(conn, &sha256)?;
    let (artifact_id, artifact_uid, is_new) = if let Some(existing) = existing {
        debug!(
            "复用已有 artifact: id={}, uid={}",
            existing.id, existing.artifact_uid
        );
        if existing.status == "deleted" {
            store::reactivate_artifact(conn, existing.id)?;
            info!("重新激活已删除 artifact: id={}", existing.id);
        } else {
            store::touch_artifact(conn, existing.id)?;
        }
        (existing.id, existing.artifact_uid.clone(), false)
    } else {
        let artifact_uid = generate_artifact_uid(&sha256);
        let artifact = SourceArtifact {
            id: 0,
            artifact_uid: artifact_uid.clone(),
            created_at: now.clone(),
            updated_at: now.clone(),
            last_seen_at: Some(now.clone()),
            sha256: sha256.clone(),
            original_name: format!("{}.md", slug.replace('/', "-")),
            extension: "md".to_string(),
            mime_type: "text/markdown".to_string(),
            size_bytes: content_bytes.len() as i64,
            storage_path: storage_path_str,
            canonical_slug: slug.to_string(),
            status: "active".to_string(),
            metadata_json: serde_json::json!({
                "source_kind": "manual",
                "input_mode": "content",
                "target_slug": slug,
                "created_by": "artifact_put"
            })
            .to_string(),
            deleted_at: None,
            purged_at: None,
        };
        let id = store::insert_artifact(conn, &artifact)?;
        info!(
            "创建手动 artifact: id={}, uid={}, slug={}",
            id, artifact_uid, slug
        );
        (id, artifact_uid, true)
    };

    // 5. 创建 occurrence
    let occurrence_uid = generate_occurrence_uid(&artifact_uid);
    let occurrence = ArtifactOccurrence {
        id: 0,
        occurrence_uid: occurrence_uid.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
        artifact_id,
        source_kind: "manual".to_string(),
        source_uri: format!("artifact://{}", artifact_uid),
        original_path: slug.to_string(),
        original_name: format!("{}.md", slug.replace('/', "-")),
        owner_ref: "cli".to_string(),
        intent: resolved_intent.to_string(),
        target_slug: slug.to_string(),
        page_slug: slug.to_string(),
        library_id: config_default_library_id,
        folder_id: None,
        // P1-2 修复：occurrence 的 promotion_policy 应与最终 route plan 对齐，
        // 而非使用配置默认值。否则 dry-run 预览与实际 worker 行为不一致。
        promotion_policy: route_plan.promotion.to_string(),
        status: "active".to_string(),
        stale_reason: String::new(),
        metadata_json: "{}".to_string(),
    };
    let occurrence_id = store::insert_occurrence(conn, &occurrence)?;

    // 6. 根据 route_plan 创建投影
    // P1-2 修复：不再固定创建 brain_page_update，
    // 根据 route_plan 决定创建哪些投影，确保 intent 路由与实际写入一致
    let mut projections = Vec::new();

    // brain_page_update 投影 — 仅当 route_plan.to_brain=true 时创建
    if route_plan.to_brain {
        let proj_key = format!("page_update:{}", slug);
        let proj_ref = format!("brain_page:{}", slug);
        let proj = ArtifactProjection {
            id: 0,
            created_at: now.clone(),
            updated_at: now.clone(),
            artifact_id,
            occurrence_id: Some(occurrence_id),
            projection_type: ProjectionType::BrainPageUpdate.to_string(),
            projection_key: proj_key.clone(),
            projection_ref: proj_ref.clone(),
            status: "active".to_string(),
            version_hash: sha256.clone(),
            stale_reason: String::new(),
            metadata_json: "{}".to_string(),
            superseded_by: None,
        };
        store::insert_projection(conn, &proj)?;
        projections.push(ProjectionResult {
            projection_type: ProjectionType::BrainPageUpdate,
            projection_key: proj_key,
            projection_ref: proj_ref,
            created: is_new,
            status: "active".to_string(),
        });
    }

    // shadow page 投影 — 仅当 route_plan.to_shadow=true 时创建
    // P1/P2 修复：manual promote 不仅要创建 shadow projection，
    // 还要实际写入 shadow page 到 pages 表，否则后续 review apply
    // 调用 update_shadow_page_section 时会因 page 不存在而返回 PageNotFound
    if route_plan.to_shadow {
        let shadow_slug = format!("documents/{}", slug);
        if validate_page_slug(&shadow_slug).is_ok() {
            let proj_key = format!("slug:{}", shadow_slug);
            let proj_ref = format!("slug:{}", shadow_slug);
            let proj = ArtifactProjection {
                id: 0,
                created_at: now.clone(),
                updated_at: now.clone(),
                artifact_id,
                occurrence_id: Some(occurrence_id),
                projection_type: ProjectionType::BrainShadowPage.to_string(),
                projection_key: proj_key.clone(),
                projection_ref: proj_ref.clone(),
                status: "active".to_string(),
                version_hash: String::new(),
                stale_reason: String::new(),
                metadata_json: "{}".to_string(),
                superseded_by: None,
            };
            store::insert_projection(conn, &proj)?;
            projections.push(ProjectionResult {
                projection_type: ProjectionType::BrainShadowPage,
                projection_key: proj_key,
                projection_ref: proj_ref,
                created: is_new,
                status: "active".to_string(),
            });

            // 实际写入 shadow page 内容到 pages 表
            let artifact_ref = store::find_artifact_by_id(conn, artifact_id)
                .map_err(|e| GBrainError::Database(format!("查找 artifact 失败: {}", e)))?
                .ok_or_else(|| {
                    GBrainError::PageNotFound(format!("artifact {} 不存在", artifact_id))
                })?;
            let (title, frontmatter, body) =
                create_shadow_page_content(&artifact_ref, &occurrence, None);
            let title_tokens = crate::nlp::chinese::tokenize_content(&title);
            let truth_tokens = crate::nlp::chinese::tokenize_content(&body);
            let content_hash = {
                let mut hasher = Sha256::new();
                hasher.update(title.as_bytes());
                hasher.update(body.as_bytes());
                format!("{:x}", hasher.finalize())
            };
            // P1 修复：同 slug 不同内容更新时，shadow page 需要更新 frontmatter/body/chunks，
            // 而非 INSERT OR IGNORE 跳过。否则新 artifact 的 active shadow projection
            // 指向旧 page 内容，破坏 source tracing 和 review apply 语义。
            //
            // 重要：不能使用 INSERT OR REPLACE，因为它对已存在行是 DELETE+INSERT，
            // 会导致 page_id 变化，级联删除 page_versions/tags/timeline/raw_data/chunks，
            // 以及 vec_chunks 向量孤儿行。
            // 改为先检查页面是否存在，存在则 UPDATE（保留 page_id），不存在则 INSERT。
            let existing_page_id: Option<i64> = conn
                .query_row(
                    "SELECT id FROM pages WHERE slug = ?1 AND deleted_at IS NULL",
                    rusqlite::params![shadow_slug],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            if let Some(page_id) = existing_page_id {
                // 页面已存在 → 先创建版本快照，再原地 UPDATE（保留 page_id）
                // P1 修复：page_versions 表只有 (page_id, compiled_truth, frontmatter, title, page_type, snapshot_at)，
                // 之前用了不存在的 slug/created_at 列导致快照静默丢失。改为与 schema 对齐的 SELECT 形式，
                // 与 promotion.rs:1556 和 sqlite_engine.rs:3003 保持一致。
                conn.execute(
                    "INSERT INTO page_versions (page_id, compiled_truth, frontmatter, title, page_type)
                     SELECT id, compiled_truth, frontmatter, title, page_type FROM pages WHERE id = ?1",
                    rusqlite::params![page_id],
                ).map_err(|e| GBrainError::Database(format!("创建 shadow page 版本快照失败: {}", e)))?;
                conn.execute(
                    "UPDATE pages SET title = ?1, compiled_truth = ?2, frontmatter = ?3,
                     content_hash = ?4, title_tokens = ?5, compiled_truth_tokens = ?6,
                     updated_at = datetime('now')
                     WHERE id = ?7",
                    rusqlite::params![
                        title,
                        body,
                        frontmatter,
                        content_hash,
                        title_tokens,
                        truth_tokens,
                        page_id
                    ],
                )
                .map_err(|e| {
                    GBrainError::Database(format!("更新手动 put shadow page 失败: {}", e))
                })?;
            } else {
                // 页面不存在 → INSERT 新行
                conn.execute(
                    "INSERT INTO pages (slug, title, compiled_truth, page_type, frontmatter, content_hash, title_tokens, compiled_truth_tokens, created_at, updated_at)
                     VALUES (?1, ?2, ?3, 'source', ?4, ?5, ?6, ?7, datetime('now'), datetime('now'))",
                    rusqlite::params![shadow_slug, title, body, frontmatter, content_hash, title_tokens, truth_tokens],
                ).map_err(|e| GBrainError::Database(format!("插入手动 put shadow page 失败: {}", e)))?;
            }

            // 重建 chunks：先收集旧 chunk ids，清理 vec_chunks/chunk_embeddings，再删除旧 chunks，最后插入新 chunk
            let page_id_for_chunk: i64 = conn
                .query_row(
                    "SELECT id FROM pages WHERE slug = ?1",
                    rusqlite::params![shadow_slug],
                    |row| row.get(0),
                )
                .map_err(|e| GBrainError::Database(format!("查询 shadow page id 失败: {}", e)))?;

            // 清理向量索引（vec_chunks 和 chunk_embeddings）
            // L1: 向量索引清理失败时记录警告，避免数据残留无声漏过
            if let Err(e) = conn.execute(
                "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT id FROM chunks WHERE page_id = ?1)",
                rusqlite::params![page_id_for_chunk],
            ) {
                tracing::warn!(page_id = page_id_for_chunk, error = %e, "清理 vec_chunks 失败");
            }
            if let Err(e) = conn.execute(
                "DELETE FROM chunk_embeddings WHERE chunk_id IN (SELECT id FROM chunks WHERE page_id = ?1)",
                rusqlite::params![page_id_for_chunk],
            ) {
                tracing::warn!(page_id = page_id_for_chunk, error = %e, "清理 chunk_embeddings 失败");
            }
            // 删除旧 chunks
            conn.execute(
                "DELETE FROM chunks WHERE page_id = ?1",
                rusqlite::params![page_id_for_chunk],
            )
            .map_err(|e| {
                GBrainError::Database(format!("清理手动 put shadow page 旧 chunk 失败: {}", e))
            })?;

            // P3 修复：使用 chunker 对全文分块，而非只取前 800 字插入单个 chunk。
            // shadow page 超过 800 字后，后续内容不会进入 chunk 索引，
            // 导致长文档后半段的事实无法被搜索命中。
            let chunks = crate::chunker::chunk_page_content_transactional(
                &body,
                &crate::types::PageType::Source,
            );
            for chunk in &chunks {
                let chunk_text_tokens = crate::nlp::chinese::tokenize_content(&chunk.chunk_text);
                let token_count = chunk_text_tokens.split_whitespace().count() as i64;
                conn.execute(
                    "INSERT INTO chunks (page_id, chunk_index, chunk_text, chunk_text_tokens, token_count, chunk_source, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'body', datetime('now'))",
                    rusqlite::params![page_id_for_chunk, chunk.chunk_index, chunk.chunk_text, chunk_text_tokens, token_count],
                ).map_err(|e| GBrainError::Database(format!("创建手动 put shadow page chunk 失败: {}", e)))?;
            }
        }
    }

    // 7. KB 投影 — 仅当 route_plan.to_kb=true 时创建
    // P1-2 修复：KB 入库由 route_plan.to_kb 控制，不再由 manual_memory_to_kb 单独决定
    let resolved_library_id = if route_plan.to_kb && manual_memory_to_kb {
        Some(resolve_default_library(
            conn,
            None,
            config_default_library_id,
            config_embedding_model,
            config_embedding_dimensions,
            auto_create_inbox,
        )?)
    } else {
        None
    };

    if let Some(library_id) = resolved_library_id {
        let kb_proj_ref = crate::artifact::projection::create_kb_projection(
            conn,
            artifact_id,
            occurrence_id,
            library_id,
        )?;
        let kb_doc_id = kb_proj_ref
            .strip_prefix("kb_document:")
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        projections.push(ProjectionResult {
            projection_type: ProjectionType::KbDocument,
            projection_key: format!("library:{}", library_id),
            projection_ref: kb_proj_ref,
            created: is_new,
            status: "active".to_string(),
        });
        info!(
            "手动 put KB 投影: artifact_id={}, kb_doc_id={}",
            artifact_id, kb_doc_id
        );
    }

    // P1-2 修复：返回实际使用的 route_plan，而不是固定的 manual memory plan
    // to_kb 由 route_plan.to_kb 和 resolved_library_id 共同决定
    let actual_route_plan = RoutePlan {
        to_kb: route_plan.to_kb && resolved_library_id.is_some(),
        to_brain: route_plan.to_brain,
        to_shadow: route_plan.to_shadow,
        to_file: route_plan.to_file,
        promotion: route_plan.promotion.clone(),
    };

    Ok(UploadSourceOutput {
        artifact_id,
        artifact_uid,
        occurrence_id,
        occurrence_uid,
        sha256,
        is_new,
        route_plan: actual_route_plan,
        projections,
    })
}
