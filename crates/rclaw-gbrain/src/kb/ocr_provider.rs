//! OCR provider trait 与通用类型定义

use serde::{Deserialize, Serialize};

/// OCR provider trait — 所有 OCR 引擎的统一接口
pub trait OcrProvider: Send + Sync {
    /// provider 名称标识
    fn name(&self) -> &'static str;

    /// 执行 OCR 识别，返回页级结果
    fn recognize(
        &self,
        input: &OcrInput,
        options: &OcrOptions,
    ) -> crate::error::Result<Vec<OcrPageResult>>;
}

/// OCR 输入
#[derive(Debug, Clone)]
pub enum OcrInput {
    /// PDF 页段输入
    PdfRange {
        /// 文件内容（URL 或 base64）
        file: OcrFilePayload,
        /// 请求中的起始页 ID（子文件内页码或原始页码）
        request_start_page_id: i32,
        /// 请求中的结束页 ID
        request_end_page_id: i32,
        /// 原始 PDF 中的起始页码
        source_start_page: i32,
        /// 原始 PDF 中的结束页码
        source_end_page: i32,
        /// 文档 ID（用于生成稳定 request_id）
        document_id: i64,
        /// 处理运行 ID（用于生成稳定 request_id）
        run_id: String,
    },
    /// 单张图片输入（GLM-OCR 支持 JPG/PNG）。
    Image {
        /// 图片内容（URL 或 base64）。
        file: OcrFilePayload,
        /// 图片 MIME 类型：image/png 或 image/jpeg。
        mime_type: String,
        /// 文档 ID（用于生成稳定 request_id）。
        document_id: i64,
        /// 处理运行 ID（用于生成稳定 request_id）。
        run_id: String,
    },
}

/// OCR 文件载体
#[derive(Debug, Clone)]
pub enum OcrFilePayload {
    /// 通过 URL 传递文件
    Url(String),
    /// 通过 base64 编码传递文件
    Base64(String),
}

/// OCR 请求选项
#[derive(Debug, Clone)]
pub struct OcrOptions {
    pub model: String,
    pub base_url: String,
    pub timeout_seconds_per_page: u64,
    pub mode: OcrMode,
    pub submit_mode: OcrSubmitMode,
    pub enable_layout: bool,
    pub return_crop_images: bool,
    pub need_layout_visualization: bool,
    pub max_pages_per_request: usize,
    pub max_pdf_bytes_per_request: usize,
    /// OCR profile: auto/general/table/formula/handwriting（只影响后处理增强，不丢弃 block）
    pub ocr_profile: String,
}

/// OCR 模式
#[derive(Debug, Clone, PartialEq)]
pub enum OcrMode {
    /// 自动检测：本地判定后再决定是否调用 OCR
    Auto,
    /// 强制全页 OCR
    AllPages,
}

impl OcrMode {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "all_pages" => Self::AllPages,
            _ => Self::Auto,
        }
    }
}

/// OCR 提交模式
#[derive(Debug, Clone, PartialEq)]
pub enum OcrSubmitMode {
    /// 优先直接提交原 PDF
    PdfFirst,
    /// 强制按 PDF 页段拆分提交
    PdfRange,
}

impl OcrSubmitMode {
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Self {
        match s {
            "pdf_range" => Self::PdfRange,
            _ => Self::PdfFirst,
        }
    }
}

/// OCR 页级结果（扩展版，包含版面块和结构化信息）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrPageResult {
    /// 页码（1-based，原始 PDF 页码）
    pub page_number: i32,
    /// 该页 OCR 识别出的纯文本
    pub text: String,
    /// 该页 OCR 识别出的 markdown 格式文本
    pub markdown: String,
    /// 版面块列表
    pub blocks: Vec<OcrLayoutBlock>,
    /// 版面可视化 URL
    pub layout_visualization_url: Option<String>,
    /// 原始响应 JSON
    pub raw_response_json: serde_json::Value,
    /// 请求 ID
    pub request_id: Option<String>,
    /// 置信度（预留字段，当前 GLM-OCR 不返回）
    pub confidence: Option<f64>,
    /// provider 名称
    pub provider: String,
    /// 模型名称
    pub model: String,
    /// OCR 服务返回的页面宽度
    pub ocr_page_width: Option<u32>,
    /// OCR 服务返回的页面高度
    pub ocr_page_height: Option<u32>,
}

/// 版面块结构
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLayoutBlock {
    /// 页码
    pub page_number: i32,
    /// 块索引（阅读顺序）
    pub index: Option<i32>,
    /// 块标签
    pub label: OcrBlockLabel,
    /// 归一化边界框 [x1, y1, x2, y2]
    pub bbox_2d: Option<[f64; 4]>,
    /// 块文本内容
    pub content: String,
    /// 块宽度
    pub width: Option<u32>,
    /// 块高度
    pub height: Option<u32>,
}

/// 版面块标签
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum OcrBlockLabel {
    Text,
    Image,
    Formula,
    Table,
    Unknown(String),
}

impl OcrBlockLabel {
    /// 从 GLM-OCR label 字符串解析
    pub fn from_glm_label(label: &str) -> Self {
        match label {
            "text" => Self::Text,
            "image" => Self::Image,
            "formula" => Self::Formula,
            "table" => Self::Table,
            other => Self::Unknown(other.to_string()),
        }
    }

    /// 转为字符串用于存储
    pub fn as_str(&self) -> &str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Formula => "formula",
            Self::Table => "table",
            Self::Unknown(s) => s.as_str(),
        }
    }
}

/// OCR 页级状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrPageStatus {
    Pending,
    Processing,
    Done,
    Failed,
    Skipped,
    EmptyOcr,
}

impl OcrPageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Processing => "processing",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::EmptyOcr => "empty_ocr",
        }
    }
}
