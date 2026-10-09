//! Error types for gbrain-core

use thiserror::Error;

/// Top-level error type for gbrain operations
#[derive(Error, Debug, PartialEq)]
pub enum GBrainError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("Page not found: {0}")]
    PageNotFound(String),

    #[error("Slug already exists: {0}")]
    SlugConflict(String),

    #[error("Invalid slug: {0}")]
    InvalidSlug(String),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Search error: {0}")]
    Search(String),

    #[error("File error: {0}")]
    FileError(String),

    #[error("Security violation: {0}")]
    Security(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Not connected")]
    NotConnected,

    #[error("Migration error: {0}")]
    Migration(String),

    #[error("IO error: {0}")]
    Io(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("OCR post-writeback error: {0}")]
    OcrPostWriteback(String),

    #[error("Transcription error: {0}")]
    Transcription(String),

    #[error("LLM error: {0}")]
    LLM(String),

    #[error("Internal error: {0}")]
    Internal(String),
}

/// Operation-level error with context about which operation failed.
/// Mirrors TS OperationError with actionable fix suggestions and docs links.
#[derive(Error, Debug)]
pub enum OperationError {
    #[error("Operation '{operation}' failed: {message}")]
    Failed {
        operation: String,
        message: String,
        suggestion: Option<String>,
        docs_url: Option<String>,
    },

    #[error("Operation '{operation}' not permitted: {message}")]
    Forbidden {
        operation: String,
        message: String,
        suggestion: Option<String>,
        docs_url: Option<String>,
    },

    #[error("Operation '{operation}' validation error: {message}")]
    Validation {
        operation: String,
        message: String,
        suggestion: Option<String>,
        docs_url: Option<String>,
    },

    #[error("Operation '{operation}' not found: {message}")]
    NotFound {
        operation: String,
        message: String,
        suggestion: Option<String>,
        docs_url: Option<String>,
    },
}

impl OperationError {
    pub fn failed(op: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Failed {
            operation: op.into(),
            message: msg.into(),
            suggestion: None,
            docs_url: None,
        }
    }

    pub fn forbidden(op: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Forbidden {
            operation: op.into(),
            message: msg.into(),
            suggestion: None,
            docs_url: None,
        }
    }

    pub fn validation(op: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::Validation {
            operation: op.into(),
            message: msg.into(),
            suggestion: None,
            docs_url: None,
        }
    }

    pub fn not_found(op: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::NotFound {
            operation: op.into(),
            message: msg.into(),
            suggestion: None,
            docs_url: None,
        }
    }

    pub fn with_suggestion(mut self, suggestion: impl Into<String>) -> Self {
        let s = Some(suggestion.into());
        match &mut self {
            Self::Failed {
                suggestion: ref mut sug,
                ..
            }
            | Self::Forbidden {
                suggestion: ref mut sug,
                ..
            }
            | Self::Validation {
                suggestion: ref mut sug,
                ..
            }
            | Self::NotFound {
                suggestion: ref mut sug,
                ..
            } => *sug = s,
        }
        self
    }

    pub fn with_docs(mut self, docs_url: impl Into<String>) -> Self {
        let d = Some(docs_url.into());
        match &mut self {
            Self::Failed {
                docs_url: ref mut docs,
                ..
            }
            | Self::Forbidden {
                docs_url: ref mut docs,
                ..
            }
            | Self::Validation {
                docs_url: ref mut docs,
                ..
            }
            | Self::NotFound {
                docs_url: ref mut docs,
                ..
            } => *docs = d,
        }
        self
    }
}

impl GBrainError {
    /// Convert this engine-level error into an OperationError for structured
    /// MCP error responses. Maps all GBrainError variants to OperationError::Failed
    /// with an "INTERNAL" operation code (P2-9).
    pub fn to_operation_error(&self) -> OperationError {
        // Map specific GBrainError variants to OperationError variants
        // with appropriate suggestions and docs links where possible.
        match self {
            GBrainError::PageNotFound(slug) => OperationError::NotFound {
                operation: "get_page".to_string(),
                message: format!("Page not found: {}", slug),
                suggestion: Some("Check the slug spelling or use resolve_slugs to find valid slugs".to_string()),
                docs_url: Some("https://github.com/gbrain/gbrain#page-operations".to_string()),
            },
            GBrainError::InvalidSlug(msg) => OperationError::Validation {
                operation: "validate".to_string(),
                message: msg.clone(),
                suggestion: Some("Slugs must use allowed prefixes (people/, companies/, etc.) with lowercase alphanumeric + hyphens".to_string()),
                docs_url: None,
            },
            GBrainError::InvalidInput(msg) => OperationError::Validation {
                operation: "input".to_string(),
                message: msg.clone(),
                suggestion: None,
                docs_url: None,
            },
            GBrainError::Security(msg) => OperationError::Forbidden {
                operation: "security".to_string(),
                message: msg.clone(),
                suggestion: Some("This operation is restricted for remote (MCP) callers".to_string()),
                docs_url: None,
            },
            GBrainError::SlugConflict(slug) => OperationError::Validation {
                operation: "put_page".to_string(),
                message: format!("Slug already exists: {}", slug),
                suggestion: Some("Use a different slug or update the existing page".to_string()),
                docs_url: None,
            },
            // All other errors map to Failed with the original error message
            _ => OperationError::Failed {
                operation: "INTERNAL".to_string(),
                message: self.to_string(),
                suggestion: None,
                docs_url: None,
            },
        }
    }
}

impl From<GBrainError> for OperationError {
    fn from(err: GBrainError) -> Self {
        err.to_operation_error()
    }
}

impl From<rusqlite::Error> for GBrainError {
    fn from(err: rusqlite::Error) -> Self {
        GBrainError::Database(err.to_string())
    }
}

impl From<serde_json::Error> for GBrainError {
    fn from(err: serde_json::Error) -> Self {
        GBrainError::Serialization(err.to_string())
    }
}

impl From<std::io::Error> for GBrainError {
    fn from(err: std::io::Error) -> Self {
        GBrainError::Io(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, GBrainError>;
