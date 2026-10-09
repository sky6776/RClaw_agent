//! 单入口多投影融合架构 — 核心类型定义
//!
//! 包含 5 张核心表对应的 Rust 结构体、输入/输出类型、枚举等。

use serde::{Deserialize, Serialize};
use std::fmt;

// ============================================================================
// 枚举类型
// ============================================================================

/// 原件状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatus {
    Active,
    Deleted,
    Purged,
}

impl fmt::Display for ArtifactStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArtifactStatus::Active => write!(f, "active"),
            ArtifactStatus::Deleted => write!(f, "deleted"),
            ArtifactStatus::Purged => write!(f, "purged"),
        }
    }
}

/// 上传来源类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    Upload,
    Sync,
    Link,
    Mcp,
    /// 手动输入（artifact put --content/--file）
    Manual,
}

impl fmt::Display for SourceKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceKind::Upload => write!(f, "upload"),
            SourceKind::Sync => write!(f, "sync"),
            SourceKind::Link => write!(f, "link"),
            SourceKind::Mcp => write!(f, "mcp"),
            SourceKind::Manual => write!(f, "manual"),
        }
    }
}

/// 上传意图（用户可见语义）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UploadIntent {
    /// 自动判断路由（根据文件类型和 MIME）
    Auto,
    /// 文档检索：KB + shadow page + candidate promotion
    Document,
    /// 仅附件：file attachment，不进 KB，不生成候选
    Attachment,
    /// 整理进记忆：KB + shadow page + candidate + 低风险自动应用
    Memory,
    /// 明确提升：KB + shadow page + proposed changes
    Promote,
}

impl fmt::Display for UploadIntent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UploadIntent::Auto => write!(f, "auto"),
            UploadIntent::Document => write!(f, "document"),
            UploadIntent::Attachment => write!(f, "attachment"),
            UploadIntent::Memory => write!(f, "memory"),
            UploadIntent::Promote => write!(f, "promote"),
        }
    }
}

impl std::str::FromStr for UploadIntent {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "auto" => Ok(UploadIntent::Auto),
            "evidence" | "document" => Ok(UploadIntent::Document),
            "attachment" => Ok(UploadIntent::Attachment),
            "memory" => Ok(UploadIntent::Memory),
            "promote" => Ok(UploadIntent::Promote),
            // 向后兼容旧值
            "kb_only" => Ok(UploadIntent::Document),
            "brain_only" => Ok(UploadIntent::Promote),
            "file_only" => Ok(UploadIntent::Attachment),
            "kb_and_brain" => Ok(UploadIntent::Promote),
            "all" => Ok(UploadIntent::Promote),
            _ => Err(format!(
                "未知上传意图: {}，有效值: auto/evidence/memory/attachment/promote",
                s
            )),
        }
    }
}

/// 投影类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionType {
    /// KB 文档投影
    KbDocument,
    /// gbrain 影子页面投影
    BrainShadowPage,
    /// 文件附件投影
    FileAttachment,
    /// 候选变更投影
    PromotionCandidate,
    /// gbrain 链接投影
    BrainLink,
    /// gbrain 时间线投影
    BrainTimeline,
    /// gbrain 页面更新投影
    BrainPageUpdate,
}

impl fmt::Display for ProjectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProjectionType::KbDocument => write!(f, "kb_document"),
            ProjectionType::BrainShadowPage => write!(f, "brain_shadow_page"),
            ProjectionType::FileAttachment => write!(f, "file_attachment"),
            ProjectionType::PromotionCandidate => write!(f, "promotion_candidate"),
            ProjectionType::BrainLink => write!(f, "brain_link"),
            ProjectionType::BrainTimeline => write!(f, "brain_timeline"),
            ProjectionType::BrainPageUpdate => write!(f, "brain_page_update"),
        }
    }
}

impl std::str::FromStr for ProjectionType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "kb_document" => Ok(ProjectionType::KbDocument),
            "brain_shadow_page" => Ok(ProjectionType::BrainShadowPage),
            "file_attachment" => Ok(ProjectionType::FileAttachment),
            "promotion_candidate" => Ok(ProjectionType::PromotionCandidate),
            "brain_link" => Ok(ProjectionType::BrainLink),
            "brain_timeline" => Ok(ProjectionType::BrainTimeline),
            "brain_page_update" => Ok(ProjectionType::BrainPageUpdate),
            // 兼容旧值
            "brain_page" => Ok(ProjectionType::BrainShadowPage),
            "shadow_page" => Ok(ProjectionType::BrainShadowPage),
            "file_store" => Ok(ProjectionType::FileAttachment),
            _ => Err(format!("unknown projection type: {}", s)),
        }
    }
}

/// 候选变更类型
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateType {
    /// 文档摘要
    DocumentSummary,
    /// 实体提及
    EntityMention,
    /// 链接建议
    LinkSuggestion,
    /// 时间线事件
    TimelineEvent,
    /// 事实声明
    FactClaim,
    /// 页面创建
    PageCreate,
    /// 页面更新
    PageUpdate,
}

impl fmt::Display for CandidateType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandidateType::DocumentSummary => write!(f, "document_summary"),
            CandidateType::EntityMention => write!(f, "entity_mention"),
            CandidateType::LinkSuggestion => write!(f, "link_suggestion"),
            CandidateType::TimelineEvent => write!(f, "timeline_event"),
            CandidateType::FactClaim => write!(f, "fact_claim"),
            CandidateType::PageCreate => write!(f, "page_create"),
            CandidateType::PageUpdate => write!(f, "page_update"),
        }
    }
}

impl std::str::FromStr for CandidateType {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "document_summary" => Ok(CandidateType::DocumentSummary),
            "entity_mention" => Ok(CandidateType::EntityMention),
            "link_suggestion" => Ok(CandidateType::LinkSuggestion),
            "timeline_event" => Ok(CandidateType::TimelineEvent),
            "fact_claim" => Ok(CandidateType::FactClaim),
            "page_create" => Ok(CandidateType::PageCreate),
            "page_update" => Ok(CandidateType::PageUpdate),
            // 兼容旧版序列化值（已废弃，仅用于反序列化兼容）
            "entity" => Ok(CandidateType::EntityMention),
            "keyword" => Ok(CandidateType::FactClaim),
            "timeline" => Ok(CandidateType::TimelineEvent),
            _ => Err(format!("未知的候选类型: {}（合法值: document_summary, entity_mention, link_suggestion, timeline_event, fact_claim, page_create, page_update）", s)),
        }
    }
}

/// 候选变更状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateStatus {
    /// 待审核
    Pending,
    /// 已接受
    Accepted,
    /// 已拒绝
    Rejected,
    /// 已应用
    Applied,
    /// 已回滚
    RolledBack,
    /// 已过期
    Stale,
    /// 已被取代
    Superseded,
}

impl fmt::Display for CandidateStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CandidateStatus::Pending => write!(f, "pending"),
            CandidateStatus::Accepted => write!(f, "accepted"),
            CandidateStatus::Rejected => write!(f, "rejected"),
            CandidateStatus::Applied => write!(f, "applied"),
            CandidateStatus::RolledBack => write!(f, "rolled_back"),
            CandidateStatus::Stale => write!(f, "stale"),
            CandidateStatus::Superseded => write!(f, "superseded"),
        }
    }
}

impl std::str::FromStr for CandidateStatus {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "pending" => Ok(CandidateStatus::Pending),
            "accepted" => Ok(CandidateStatus::Accepted),
            "rejected" => Ok(CandidateStatus::Rejected),
            "applied" => Ok(CandidateStatus::Applied),
            "rolled_back" => Ok(CandidateStatus::RolledBack),
            "stale" => Ok(CandidateStatus::Stale),
            "superseded" => Ok(CandidateStatus::Superseded),
            // 兼容旧版序列化值（已废弃，仅用于反序列化兼容）
            "approved" => Ok(CandidateStatus::Accepted),
            "expired" => Ok(CandidateStatus::Stale),
            _ => Err(format!(
                "未知的候选状态: {}（合法值: pending, accepted, rejected, applied, rolled_back, stale, superseded）",
                s
            )),
        }
    }
}

/// 风险等级
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    Low,
    Medium,
    High,
}

impl fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RiskLevel::Low => write!(f, "low"),
            RiskLevel::Medium => write!(f, "medium"),
            RiskLevel::High => write!(f, "high"),
        }
    }
}

/// 提升策略
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionPolicy {
    /// 不自动提升
    None,
    /// 仅记录影子页面，不生成候选
    Shadow,
    /// 生成候选，需要人工审核
    Candidate,
    /// 自动接受低风险候选
    AutoAcceptLowRisk,
    /// 自动接受并应用所有候选
    AutoApply,
}

impl fmt::Display for PromotionPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PromotionPolicy::None => write!(f, "none"),
            PromotionPolicy::Shadow => write!(f, "shadow"),
            PromotionPolicy::Candidate => write!(f, "candidate"),
            PromotionPolicy::AutoAcceptLowRisk => write!(f, "auto_accept_low_risk"),
            PromotionPolicy::AutoApply => write!(f, "auto_apply"),
        }
    }
}

impl std::str::FromStr for PromotionPolicy {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "none" => Ok(PromotionPolicy::None),
            "shadow" => Ok(PromotionPolicy::Shadow),
            "candidate" => Ok(PromotionPolicy::Candidate),
            "auto_accept_low_risk" => Ok(PromotionPolicy::AutoAcceptLowRisk),
            "auto_apply" => Ok(PromotionPolicy::AutoApply),
            // 兼容旧值
            "auto" => Ok(PromotionPolicy::AutoAcceptLowRisk),
            "auto-low-risk" => Ok(PromotionPolicy::AutoAcceptLowRisk),
            "auto-apply" | "auto_all" | "auto-all" | "auto-apply-all" => {
                Ok(PromotionPolicy::AutoApply)
            }
            _ => Err(format!("unknown promotion policy: {}", s)),
        }
    }
}

/// 来源追溯状态
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceStatus {
    Active,
    Stale,
    Superseded,
}

impl fmt::Display for ProvenanceStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProvenanceStatus::Active => write!(f, "active"),
            ProvenanceStatus::Stale => write!(f, "stale"),
            ProvenanceStatus::Superseded => write!(f, "superseded"),
        }
    }
}

/// 查询策略
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryStrategy {
    /// 先查 gbrain，再查 KB 补充
    BrainFirst,
    /// 先查 KB 证据，再查 gbrain 上下文
    EvidenceFirst,
    /// 仅追溯来源链
    Provenance,
    /// 先查时间线事件，再查 gbrain 上下文（§11.1/§11.2）
    TimelineFirst,
}

impl fmt::Display for QueryStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryStrategy::BrainFirst => write!(f, "brain_first"),
            QueryStrategy::EvidenceFirst => write!(f, "evidence_first"),
            QueryStrategy::Provenance => write!(f, "provenance"),
            QueryStrategy::TimelineFirst => write!(f, "timeline_first"),
        }
    }
}

// ============================================================================
// 数据库行结构体
// ============================================================================

/// source_artifacts 表行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceArtifact {
    pub id: i64,
    pub artifact_uid: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_seen_at: Option<String>,

    pub sha256: String,
    pub original_name: String,
    pub extension: String,
    pub mime_type: String,
    pub size_bytes: i64,

    pub storage_path: String,
    pub canonical_slug: String,
    pub status: String,
    pub metadata_json: String,

    pub deleted_at: Option<String>,
    pub purged_at: Option<String>,
}

/// artifact_occurrences 表行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactOccurrence {
    pub id: i64,
    pub occurrence_uid: String,
    pub created_at: String,
    pub updated_at: String,

    pub artifact_id: i64,
    pub source_kind: String,
    pub source_uri: String,
    pub original_path: String,
    pub original_name: String,
    pub owner_ref: String,

    pub intent: String,
    pub target_slug: String,
    pub page_slug: String,
    pub library_id: Option<i64>,
    pub folder_id: Option<i64>,
    pub promotion_policy: String,

    pub status: String,
    /// stale 原因：detached_by_user / artifact_deleted / reprocess_requested / content_updated 等
    pub stale_reason: String,
    pub metadata_json: String,
}

/// artifact_projections 表行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactProjection {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,

    pub artifact_id: i64,
    pub occurrence_id: Option<i64>,
    pub projection_type: String,
    pub projection_key: String,
    pub projection_ref: String,

    pub status: String,
    pub version_hash: String,
    pub stale_reason: String,
    pub metadata_json: String,
    /// 被哪个投影替代（§31 版本链）
    pub superseded_by: Option<i64>,
}

/// promotion_candidates 表行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromotionCandidate {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,

    pub artifact_id: i64,
    pub occurrence_id: Option<i64>,
    pub kb_document_id: Option<i64>,
    pub kb_node_id: Option<i64>,

    pub candidate_type: String,
    pub target_slug: String,
    pub target_field: String,

    pub title: String,
    pub proposed_payload: String,
    pub evidence_json: String,

    pub confidence: f64,
    pub risk_level: String,
    pub status: String,
    pub reviewer: String,
    pub review_notes: String,
    pub applied_at: Option<String>,
    /// 候选指纹 — SHA256(artifact_id|candidate_type|target_slug|target_field|proposed_payload)
    /// 用于重试路径去重，防止同一内容重复创建候选
    pub candidate_fingerprint: String,
}

/// provenance_ledger 表行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceRecord {
    pub id: i64,
    pub created_at: String,
    pub updated_at: String,

    pub artifact_id: Option<i64>,
    pub occurrence_id: Option<i64>,
    pub kb_document_id: Option<i64>,
    pub kb_node_id: Option<i64>,
    pub promotion_candidate_id: Option<i64>,

    pub brain_slug: String,
    pub brain_field: String,
    pub fact_hash: String,

    pub quote_text: String,
    pub quote_start: Option<i64>,
    pub quote_end: Option<i64>,
    pub page_number: Option<i64>,

    pub confidence: f64,
    pub status: String,
    pub stale_reason: String,
    pub metadata_json: String,
}

// ============================================================================
// 输入/输出类型
// ============================================================================

/// 上传源文件输入
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSourceInput {
    /// 文件内容（从 path 读取或直接传入）
    pub content: Vec<u8>,
    /// 本地文件路径（可选，用于 dry_run 和日志）
    pub path: Option<std::path::PathBuf>,
    /// 原始文件名
    pub original_name: String,
    /// 来源类型
    pub source_kind: SourceKind,
    /// 来源 URI
    pub source_uri: String,
    /// 上传意图
    pub intent: UploadIntent,
    /// 目标 slug（可选，用于关联 gbrain page）
    pub target_slug: Option<String>,
    /// 目标 page slug（可选）
    pub page_slug: Option<String>,
    /// 库 ID（可选）
    pub library_id: Option<i64>,
    /// 文件夹 ID（可选）
    pub folder_id: Option<i64>,
    /// 提升策略
    // 修复：改为 Option，仅在用户显式指定时覆盖 intent 推断的提升策略
    pub promotion_policy: Option<PromotionPolicy>,
    /// 所有者引用
    pub owner_ref: Option<String>,
    /// 额外元数据
    pub metadata: Option<serde_json::Value>,
    /// 仅返回路由计划，不实际写入
    pub dry_run: bool,
}

/// 上传源文件输出
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadSourceOutput {
    /// 原件 ID
    pub artifact_id: i64,
    /// 原件 UID
    pub artifact_uid: String,
    /// 事件 ID
    pub occurrence_id: i64,
    /// 事件 UID
    pub occurrence_uid: String,
    /// SHA256
    pub sha256: String,
    /// 是否为新增（vs 已存在）
    pub is_new: bool,
    /// 路由计划
    pub route_plan: RoutePlan,
    /// 投影结果
    pub projections: Vec<ProjectionResult>,
}

/// 路由计划
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutePlan {
    /// 是否投影到 KB
    pub to_kb: bool,
    /// 是否投影到 gbrain（正式页面）
    pub to_brain: bool,
    /// 是否投影到文件附件
    pub to_file: bool,
    /// 是否创建影子页面
    pub to_shadow: bool,
    /// 提升策略
    pub promotion: PromotionPolicy,
}

/// 投影结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectionResult {
    /// 投影类型
    pub projection_type: ProjectionType,
    /// 投影键
    pub projection_key: String,
    /// 投影引用（如 KB document ID、brain slug 等）
    pub projection_ref: String,
    /// 是否为新建
    pub created: bool,
    /// 状态
    pub status: String,
}

/// 候选审核输入
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewCandidateInput {
    /// 候选 ID
    pub candidate_id: i64,
    /// 审核动作：approve / reject
    pub action: String,
    /// 审核人
    pub reviewer: String,
    /// 审核备注
    pub notes: Option<String>,
}

/// 批量应用候选结果（§10.5 promotion_apply_all）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchApplyResult {
    /// 总候选数
    pub total_candidates: usize,
    /// 成功应用数
    pub applied: usize,
    /// 失败数
    pub failed: usize,
    /// 失败详情
    pub failures: Vec<String>,
    /// 是否 dry_run
    pub dry_run: bool,
    /// dry_run 模式下的候选预览
    pub candidates: Vec<String>,
}

/// 统一查询输入
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedQueryInput {
    /// 查询文本
    pub query: String,
    /// 查询策略
    pub strategy: QueryStrategy,
    /// 限制数量
    pub limit: Option<i64>,
    /// 过滤 slug（可选）
    pub filter_slug: Option<String>,
    /// 是否包含 KB 证据
    pub include_evidence: bool,
    /// 是否包含来源追溯
    pub include_provenance: bool,
}

/// 统一查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnifiedQueryResult {
    /// 查询策略
    pub strategy: String,
    /// gbrain 命中
    pub brain_hits: Vec<BrainHit>,
    /// KB 证据命中
    pub evidence_hits: Vec<EvidenceHit>,
    /// 时间线命中（§11.1 TimelineFirst 策略）
    pub timeline_hits: Vec<TimelineHit>,
    /// 来源追溯记录
    pub provenance_records: Vec<ProvenanceRecord>,
    /// 总命中数
    pub total_hits: i64,
}

/// gbrain 命中
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrainHit {
    /// page slug
    pub slug: String,
    /// page 标题
    pub title: String,
    /// page 内容片段
    pub snippet: String,
    /// 相关度
    pub relevance: f64,
    /// 来源追溯（如有）
    pub provenance: Vec<ProvenanceRecord>,
}

/// KB 证据命中
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceHit {
    /// KB 文档 ID
    pub kb_document_id: i64,
    /// KB 文档标题
    pub title: String,
    /// 内容片段
    pub snippet: String,
    /// 相关度
    pub relevance: f64,
    /// 命中的核心词
    pub matched_terms: Vec<String>,
    /// KB passage ID
    pub passage_id: Option<i64>,
    /// 证据视图类型
    pub view_type: Option<String>,
    /// 片段在源文本中的字符起点
    pub source_start: Option<i64>,
    /// 片段在源文本中的字符终点
    pub source_end: Option<i64>,
    /// 是否建议 focused 读取更多上下文
    pub needs_more_context: bool,
    /// P3: 同 section 前一个 chunk 的上下文文本
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_before: Option<String>,
    /// P3: 同 section 后一个 chunk 的上下文文本
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_after: Option<String>,
    /// 关联的原件信息
    pub artifact: Option<SourceArtifact>,
    /// 关联的影子页面 slug
    pub shadow_page_slug: Option<String>,
    /// 关联的投影
    pub projections: Vec<ArtifactProjection>,
}

/// 时间线命中（§11.1 TimelineFirst 策略）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineHit {
    /// 候选 ID
    pub candidate_id: i64,
    /// 事件日期
    pub event_date: String,
    /// 事件描述
    pub description: String,
    /// 关联的 artifact ID
    pub artifact_id: i64,
    /// 关联的 KB 文档 ID
    pub kb_document_id: Option<i64>,
    /// 关联的影子页面 slug
    pub shadow_page_slug: Option<String>,
    /// 来源文档标题
    pub source_title: String,
}

/// 健康检查结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactHealthReport {
    /// 原件总数
    pub total_artifacts: i64,
    /// 活跃原件数
    pub active_artifacts: i64,
    /// 孤立投影数（artifact 已删除但投影仍存在）
    pub orphan_projections: i64,
    /// 过期投影数
    pub stale_projections: i64,
    /// 待审核候选数
    pub pending_candidates: i64,
    /// 活跃来源追溯数
    pub active_provenance: i64,
    /// 过期来源追溯数
    pub stale_provenance: i64,
    /// 问题列表
    pub issues: Vec<HealthIssue>,
}

/// 健康问题
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthIssue {
    /// 问题级别
    pub severity: String,
    /// 问题类型
    pub issue_type: String,
    /// 问题描述
    pub description: String,
    /// 建议修复
    pub suggestion: String,
}

/// artifact_events 审计记录（§7.6）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactEvent {
    /// 事件 ID
    pub id: i64,
    /// 创建时间
    pub created_at: String,
    /// 关联的 artifact ID
    pub artifact_id: Option<i64>,
    /// 关联的 occurrence ID
    pub occurrence_id: Option<i64>,
    /// 事件类型（如 artifact_created, projection_created, promotion_applied 等）
    pub event_type: String,
    /// 执行者
    pub actor: String,
    /// 事件负载 JSON
    pub payload_json: String,
}

// ============================================================================
// 辅助函数
// ============================================================================

/// KB 支持的文档扩展名列表
///
/// 设计文档 §6.2: PDF/DOCX/XLS/XLSX/CSV/HTML/TXT/MD 走 KB 投影路径。
/// GLM-OCR 支持的 JPG/PNG 图片走 KB OCR 投影路径，其他图片走附件路径。
/// 修复：移除 doc — ParserRegistry 未注册对应 parser，
/// 旧版 Word 二进制文件走 text fallback 要求 UTF-8 会失败
pub const KB_SUPPORTED_EXTENSIONS: &[&str] = &[
    "pdf", "docx", "xls", "xlsx", "csv", "tsv", "html", "htm", "txt", "md", "markdown", "rst",
    "json", "xml", "yaml", "yml", "toml", "png", "jpg", "jpeg",
];

/// 代码文件扩展名列表
///
/// 设计文档 §6.2: 代码文件/repo 走现有 code import/sync 路径，不进 KB。
pub const CODE_EXTENSIONS: &[&str] = &[
    "rs", "py", "js", "ts", "tsx", "jsx", "go", "java", "c", "cpp", "h", "rb", "php", "sh", "bash",
    "zsh", "ps1", "sql", "r", "m", "swift", "kt", "scala", "lua", "vim", "el", "clj",
];

/// 图片扩展名列表
///
/// 默认不支持 OCR 的图片仍走附件路径。
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "bmp", "ico", "tiff", "tif", "webp", "avif",
];

/// GLM-OCR 图片输入支持的扩展名。
pub const OCR_IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg"];

/// 判断扩展名是否为 KB 支持的文档类型
pub fn is_kb_supported(extension: &str) -> bool {
    KB_SUPPORTED_EXTENSIONS.contains(&extension)
}

/// 判断扩展名是否为代码文件
pub fn is_code_file(extension: &str) -> bool {
    CODE_EXTENSIONS.contains(&extension)
}

/// 判断扩展名是否为图片文件
pub fn is_image_file(extension: &str) -> bool {
    IMAGE_EXTENSIONS.contains(&extension)
}

/// 判断扩展名是否为可直接提交给 GLM-OCR 的图片文件。
pub fn is_ocr_image_file(extension: &str) -> bool {
    OCR_IMAGE_EXTENSIONS.contains(&extension)
}

/// 根据意图、扩展名和 MIME 类型推断路由计划
///
/// 设计文档 §6.2 路由规则:
/// | 条件                                  | Artifact | KB | Shadow | File | Promotion |
/// |--------------------------------------|----------|----|--------|------|-----------|
/// | --intent attachment                  | yes      | no | no     | yes  | none      |
/// | --intent document                    | yes      | yes| yes    | no   | candidate |
/// | --intent memory                      | yes      | yes| yes    | no   | auto-low  |
/// | --intent promote                     | yes      | yes| yes    | no   | candidate |
/// | PDF/DOCX/XLS/XLSX/CSV/HTML/TXT auto | yes      | yes| yes    | no   | candidate |
/// | Raw Markdown with auto               | yes      | yes| yes    | no   | candidate |
/// | Markdown with gbrain frontmatter     | optional | no | no     | no   | direct put_page |
/// | Code file/repo with auto             | optional | no | no     | no   | code import/sync |
/// | JPG/PNG with auto                    | yes      | yes| yes    | no   | candidate |
/// | Other image/binary unsupported by KB | yes      | no | optional| yes | no        |
pub fn infer_route_plan(extension: &str, _mime_type: &str, intent: &UploadIntent) -> RoutePlan {
    match intent {
        // 明确意图：直接按意图路由，不根据文件类型调整
        UploadIntent::Attachment => RoutePlan {
            to_kb: false,
            to_shadow: false,
            to_file: true,
            to_brain: false,
            promotion: PromotionPolicy::None,
        },
        UploadIntent::Document => RoutePlan {
            to_kb: true,
            to_shadow: true,
            to_file: false,
            to_brain: false,
            promotion: PromotionPolicy::Candidate,
        },
        UploadIntent::Memory => RoutePlan {
            to_kb: true,
            to_shadow: true,
            to_file: false,
            to_brain: false,
            promotion: PromotionPolicy::AutoAcceptLowRisk,
        },
        UploadIntent::Promote => RoutePlan {
            to_kb: true,
            to_shadow: true,
            to_file: false,
            to_brain: true,
            promotion: PromotionPolicy::Candidate,
        },
        // Auto 意图：根据文件类型智能路由
        UploadIntent::Auto => {
            let ext = extension.to_lowercase();

            // 代码文件 → 不进 KB，走现有 import/sync 路径
            if is_code_file(&ext) {
                return RoutePlan {
                    to_kb: false,
                    to_shadow: false,
                    to_file: false,
                    to_brain: false,
                    promotion: PromotionPolicy::None,
                };
            }

            // GLM-OCR 支持的图片 → KB OCR + shadow + candidate
            if is_ocr_image_file(&ext) {
                return RoutePlan {
                    to_kb: true,
                    to_shadow: true,
                    to_file: false,
                    to_brain: false,
                    promotion: PromotionPolicy::Candidate,
                };
            }

            // 其他图片/二进制 → 不进 KB，走附件路径
            if is_image_file(&ext) || !is_kb_supported(&ext) {
                return RoutePlan {
                    to_kb: false,
                    to_shadow: false,
                    to_file: true,
                    to_brain: false,
                    promotion: PromotionPolicy::None,
                };
            }

            // KB 支持的文档类型 → KB + shadow + candidate
            RoutePlan {
                to_kb: true,
                to_shadow: true,
                to_file: false,
                to_brain: false,
                promotion: PromotionPolicy::Candidate,
            }
        }
    }
}

/// 应用 promotion 策略到路由计划。
///
/// 优先级：用户显式指定 > config 默认值 > intent 推断（已在 route_plan 中）
///
/// 此函数从 upload.rs 提取为公共 helper，供 service.rs 的 dry_run 早返回路径复用，
/// 确保 dry_run 预览的 route plan 与真实执行路径一致（P2 修复）。
pub fn apply_promotion_policy(
    route_plan: RoutePlan,
    explicit_policy: &Option<PromotionPolicy>,
    config_default_promotion_policy: &str,
) -> RoutePlan {
    if let Some(policy) = explicit_policy {
        RoutePlan {
            promotion: policy.clone(),
            ..route_plan
        }
    } else if !config_default_promotion_policy.is_empty()
        && config_default_promotion_policy != "candidate"
    {
        // 兼容旧配置：candidate 表示沿用 intent 推断出的候选审核流程，无需覆盖。
        if let Ok(policy) = config_default_promotion_policy.parse() {
            RoutePlan {
                promotion: policy,
                ..route_plan
            }
        } else {
            route_plan
        }
    } else {
        route_plan
    }
}

/// 生成投影键
///
/// 设计文档 projection_key 规则:
/// | projection_type    | projection_key                        |
/// |--------------------|---------------------------------------|
/// | kb_document        | library:{library_id}                  |
/// | brain_shadow_page  | slug:{brain_slug}                     |
/// | file_attachment    | page:{page_slug}:file:{filename}      |
/// | promotion_candidate| candidate:{candidate_id}              |
/// | brain_link         | link:{from}:{to}:{type}               |
/// | brain_timeline     | timeline:{slug}:{date}:{hash}         |
/// | brain_page_update  | page_update:{slug}:{fact_hash}        |
pub fn make_projection_key(proj_type: &str, ref_id: &str) -> String {
    format!("{}:{}", proj_type, ref_id)
}

/// 生成事实哈希（SHA256）
///
/// 设计文档: hash = sha256(brain_slug + field + normalized_payload + artifact_uid + kb_node_id)
pub fn make_fact_hash(
    brain_slug: &str,
    field: &str,
    payload: &str,
    artifact_uid: &str,
    kb_node_id: &str,
) -> String {
    use sha2::{Digest, Sha256};
    let input = format!(
        "{}|{}|{}|{}|{}",
        brain_slug, field, payload, artifact_uid, kb_node_id
    );
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 生成规范 slug（从文件名）
///
/// 产出必须通过 validate_page_slug 校验（仅允许 ASCII 小写+数字+-），
/// 因此 Unicode 字母和下划线都会转为 `-`。
/// 修复：之前允许 Unicode 字母和 `_`，但 validate_page_slug 只接受 ASCII，
/// 导致 my_doc.pdf、中文文件名等校验失败，shadow page 静默丢失。
pub fn make_canonical_slug(original_name: &str, sha256: &str) -> String {
    let stem = original_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(original_name);
    let slug = stem
        .to_lowercase()
        .chars()
        .map(|c| {
            // 仅保留 ASCII 小写字母、数字和 -，其余全部转为 -
            // _ 转为 -（validate_page_slug 不允许 _）
            // Unicode 字母转为 -（validate_page_slug 不允许非 ASCII）
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>();
    // 去除连续 - 和首尾 -
    let slug = slug
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    // 附加 sha256 前 8 位确保唯一
    format!("{}-{}", slug, &sha256[..8.min(sha256.len())])
}

/// 从文件名推断扩展名
pub fn infer_extension(filename: &str) -> String {
    filename
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_lowercase())
        .unwrap_or_default()
}

/// 从扩展名推断 MIME 类型
///
/// M44 修复：委托给统一的 mime_type_for_ext，保持 API 兼容（返回 String）。
pub fn infer_mime_type(extension: &str) -> String {
    crate::kb::types::mime_type_for_ext(extension).to_string()
}

// ============================================================================
// artifact 统一接口类型（设计文档 §4.2 / §5.1 / §6 / §7）
// ============================================================================

/// Artifact 意图 — 用户友好的意图表达（设计文档 §5.1）
///
/// 映射关系:
/// - memory → 内部走 Document + shadow 投影 + 低风险自动应用
/// - evidence → 内部走 Document（仅 KB 证据）
/// - promote → 内部走 Promote（明确提升）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactIntent {
    /// 整理进长期记忆（默认）：进 KB + shadow 投影 + 低风险自动应用
    Memory,
    /// 仅作为证据存入 KB，不自动投影
    Evidence,
    /// 明确提升到 gbrain 页面
    Promote,
}

/// artifact_put 请求参数（设计文档 §4.2）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactPutInput {
    /// 目标页面 slug
    pub slug: String,
    /// 直接输入的文本内容（与 file 二选一）
    pub content: Option<String>,
    /// 本地文件路径（与 content 二选一）
    pub file: Option<String>,
    /// 页面标题（可选）
    pub title: Option<String>,
    /// 意图（默认 Memory）
    pub intent: Option<ArtifactIntent>,
    /// 仅返回路由计划，不实际写入
    pub dry_run: Option<bool>,
}

/// artifact_query 请求参数（设计文档 §4.2）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactQueryInput {
    /// 查询文本
    pub query: String,
    /// 查询模式: auto/memory/evidence/timeline（graph 尚未实现）
    pub mode: Option<String>,
    /// 最大结果数
    pub limit: Option<usize>,
    /// 过滤到指定页面 slug
    pub filter_slug: Option<String>,
    /// 显示来源追溯
    pub include_sources: Option<bool>,
}

/// artifact_query 统一输出（设计文档 §7）
///
/// 合并 gbrain 记忆、KB 证据和时间线事件，
/// 隐藏内部 ID，提供统一的知识查询结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactQueryOutput {
    /// 查询文本
    pub query: String,
    /// 查询模式
    pub mode: String,
    /// 记忆结果（来自 gbrain）
    pub memories: Vec<MemoryResult>,
    /// 证据结果（来自 KB）
    pub evidence: Vec<EvidenceResult>,
    /// 时间线事件
    pub timeline: Vec<TimelineEvent>,
    /// 图谱关系
    pub graph: Vec<GraphRelation>,
    /// 内容未命中但标题/slug/original_name 明确相关的候选文档
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<DocumentCandidate>,
    /// 查询元信息
    pub meta: QueryMeta,
    /// 来源追溯（当 include_sources=true 时填充）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
}

/// 记忆查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryResult {
    /// 页面 slug
    pub slug: String,
    /// 页面标题
    pub title: String,
    /// 内容摘要
    pub summary: String,
    /// 相关度分数
    pub score: f64,
    /// 来源引用（当 include_sources=true 时填充）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
}

/// 证据查询结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceResult {
    /// 文档标题
    pub title: String,
    /// 内容片段
    pub snippet: String,
    /// 相关度分数
    pub score: f64,
    /// 命中的核心词
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_terms: Vec<String>,
    /// 来源 artifact UID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_uid: Option<String>,
    /// 关联的影子页面 slug
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shadow_page_slug: Option<String>,
    /// KB passage ID，可用于后续 focused 读取
    #[serde(skip_serializing_if = "Option::is_none")]
    pub passage_id: Option<i64>,
    /// 证据视图类型: atomic/window/node/raw
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_type: Option<String>,
    /// 片段在源文本中的字符起点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_start: Option<i64>,
    /// 片段在源文本中的字符终点
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_end: Option<i64>,
    /// 是否建议继续用 artifact_get focused 模式读取上下文
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_more_context: bool,
    /// 来源引用（当 include_sources=true 时填充）
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceRef>,
}

/// 标题/slug/original_name 命中的候选文档
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentCandidate {
    pub title: String,
    pub original_name: Option<String>,
    pub artifact_uid: Option<String>,
    pub slug: Option<String>,
    pub score: f64,
    pub reason: String,
    pub suggested_action: String,
}

/// 时间线事件
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// 事件时间
    pub timestamp: String,
    /// 事件描述
    pub description: String,
    /// 关联页面 slug
    pub slug: Option<String>,
}

/// 图谱关系
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphRelation {
    /// 源页面 slug
    pub from_slug: String,
    /// 关系类型
    pub relation: String,
    /// 目标页面 slug
    pub to_slug: String,
}

/// 查询元信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryMeta {
    /// 总结果数
    pub total: usize,
    /// 查询耗时（毫秒）
    pub elapsed_ms: u64,
    /// 是否使用了向量搜索
    pub used_vector: bool,
    /// 是否使用了关键词搜索
    pub used_keyword: bool,
    /// 是否使用了 fallback
    #[serde(default, skip_serializing_if = "is_false")]
    pub fallback_used: bool,
    /// 命中的 fallback 阶段
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_stage: Option<String>,
    /// fallback 原因
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// fallback 尝试过的查询
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallback_queries: Vec<String>,
    /// 从原 query 提取的核心词
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub core_terms: Vec<String>,
    /// 置信度: high/medium/low/none
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<String>,
    /// 仅标题/slug 命中时提示调用方应 focused 读取
    #[serde(default, skip_serializing_if = "is_false")]
    pub needs_focused_context: bool,
    /// 明确无可靠结果
    #[serde(default, skip_serializing_if = "is_false")]
    pub no_results: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 建议变更条目 — promotion 的用户友好包装（设计文档 §6）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReviewItem {
    /// 变更 ID
    pub change_id: i64,
    /// 目标页面 slug
    pub target_slug: String,
    /// 变更状态
    pub status: String,
    /// 风险等级
    pub risk_level: String,
    /// 变更摘要
    pub summary: String,
    /// 来源证据（结构化 JSON）
    pub evidence: Option<serde_json::Value>,
    /// 创建时间
    pub created_at: Option<String>,
}

// ============================================================================
// 新增 facade DTO 类型（二次审计 P0/P1/P2 修复）
// ============================================================================

/// 用户友好的来源引用（替代暴露内部 provenance row）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRef {
    pub artifact_uid: String,
    pub original_name: Option<String>,
    pub quote_text: Option<String>,
    pub confidence: f64,
    pub brain_slug: Option<String>,
    pub brain_field: Option<String>,
}

/// artifact_get 用户友好输出（替代直接返回 SourceArtifact row）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactDetailOutput {
    pub uid: String,
    pub slug: String,
    pub original_name: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<i64>,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
    pub extension: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub projections: Option<Vec<ArtifactProjectionSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<SourceRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub occurrences: Option<Vec<ArtifactOccurrenceSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_query: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub content_matches: Vec<FocusedContentMatch>,
}

/// artifact_get focused content 命中片段
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusedContentMatch {
    pub snippet: String,
    pub score: f64,
    pub kb_document_id: Option<i64>,
    pub passage_id: Option<i64>,
    pub view_type: Option<String>,
    pub source_start: Option<i64>,
    pub source_end: Option<i64>,
}

/// projection 摘要（用户友好字段，不暴露内部 projection_key/ref）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactProjectionSummary {
    /// 投影类型（如 brain_page_update、brain_shadow_page）
    pub projection_type: String,
    /// 投影目标描述（语义映射后的用户友好值，替代内部 projection_key）
    pub target: String,
    /// 投影目标引用（语义映射后的用户友好值，替代内部 projection_ref）
    pub target_ref: Option<String>,
    /// 投影状态
    pub status: String,
}

/// P3-8 修复：将内部 projection_key/projection_ref 语义映射为用户友好描述
///
/// 映射规则：
/// - brain_page_update + page_update:{slug} -> target:"stable_page", target_ref: slug
/// - brain_shadow_page + slug:{slug} -> target:"draft_shadow_page", target_ref: slug
/// - kb_document + library:{id} -> target:"searchable_evidence", target_ref: None（不暴露 library id）
/// - file_attachment -> target: 原始 key（保留，因为文件路径本身有语义）
/// - 其它 -> target: 原始 key（兜底）
pub fn map_projection_to_friendly(
    projection_type: &str,
    projection_key: &str,
    projection_ref: &str,
) -> (String, Option<String>) {
    match projection_type {
        // brain_page_update: page_update:{slug}:{hash} 或 page_update:{slug}
        "brain_page_update" => {
            // 从 projection_key 提取 slug（page_update: 之后的第一个段）
            let slug = projection_key
                .strip_prefix("page_update:")
                .and_then(|s| s.split(':').next())
                .unwrap_or(projection_key);
            ("stable_page".to_string(), Some(slug.to_string()))
        }
        // brain_shadow_page: slug:documents/{slug}
        "brain_shadow_page" => {
            let slug = projection_key
                .strip_prefix("slug:")
                .unwrap_or(projection_key);
            ("draft_shadow_page".to_string(), Some(slug.to_string()))
        }
        // kb_document: library:{id} — 不暴露 library id
        "kb_document" => ("searchable_evidence".to_string(), None),
        // file_attachment: page:{slug}:file:{name} — 保留原始 key 有语义
        "file_attachment" => (projection_key.to_string(), Some(projection_ref.to_string())),
        // 兜底：保留原始值
        _ => (projection_key.to_string(), Some(projection_ref.to_string())),
    }
}

/// occurrence 摘要
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactOccurrenceSummary {
    pub uid: String,
    pub intent: Option<String>,
    pub target_slug: Option<String>,
    pub status: String,
    pub created_at: String,
}

/// review 操作用户友好输出（替代直接返回 PromotionCandidate row）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactReviewActionOutput {
    pub change_id: i64,
    pub target_slug: String,
    /// 变更类型（如 fact_claim、document_summary 等）
    pub change_type: String,
    pub status: String,
    pub action_description: String,
    pub evidence: Option<serde_json::Value>,
    pub risk_level: Option<String>,
}

/// delete dry-run 影响预览
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteImpactPreview {
    pub artifact_id: i64,
    pub artifact_uid: String,
    pub artifact_status: String,
    pub projection_count: i64,
    pub occurrence_count: i64,
    pub kb_document_count: i64,
    pub provenance_count: i64,
}

/// artifact_list 列表项 DTO（隐藏内部 id/storage_path/raw metadata）
///
/// P2-3 修复：artifact_list 不再直接返回 SourceArtifact row，
/// 改用此 DTO 隐藏内部字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactListItem {
    pub uid: String,
    pub slug: String,
    pub original_name: Option<String>,
    pub size_bytes: Option<i64>,
    pub status: String,
    pub updated_at: String,
}

/// put 幂等/冲突检测结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PutResolution {
    pub action: String,
    pub artifact_id: Option<i64>,
    pub artifact_uid: Option<String>,
    pub detail: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_route_sends_glm_ocr_images_to_kb() {
        let plan = infer_route_plan("png", "image/png", &UploadIntent::Auto);
        assert!(plan.to_kb);
        assert!(plan.to_shadow);
        assert!(!plan.to_file);
        assert!(!plan.to_brain);
        assert_eq!(plan.promotion, PromotionPolicy::Candidate);

        let jpg_plan = infer_route_plan("jpg", "image/jpeg", &UploadIntent::Auto);
        assert!(jpg_plan.to_kb);
        assert!(jpg_plan.to_shadow);
    }

    #[test]
    fn auto_route_keeps_non_ocr_images_as_attachments() {
        let plan = infer_route_plan("webp", "image/webp", &UploadIntent::Auto);
        assert!(!plan.to_kb);
        assert!(!plan.to_shadow);
        assert!(plan.to_file);
        assert!(!plan.to_brain);
        assert_eq!(plan.promotion, PromotionPolicy::None);
    }

    #[test]
    fn promotion_policy_accepts_auto_apply_aliases() {
        assert_eq!(
            "auto-apply".parse::<PromotionPolicy>().unwrap(),
            PromotionPolicy::AutoApply
        );
        assert_eq!(
            "auto_apply".parse::<PromotionPolicy>().unwrap(),
            PromotionPolicy::AutoApply
        );
        assert_eq!(
            "auto_all".parse::<PromotionPolicy>().unwrap(),
            PromotionPolicy::AutoApply
        );
        assert_eq!(PromotionPolicy::AutoApply.to_string(), "auto_apply");
    }
}
