//! KB (Knowledge Base) subsystem for gbrain_rs
//!
//! Handles document upload, parsing, chunking, embedding, RAPTOR summarization,
//! and hybrid search (vector + FTS5 + RRF).

pub mod augment;
pub mod backup;
pub mod cache;
pub mod chinese;
pub mod context;
pub mod cost;
pub mod embedding_index;
pub mod engine;
pub mod eval;
pub mod granularity;
pub mod health;
pub mod jobs;
pub mod lifecycle;
pub mod metadata;
pub mod ocr;
pub mod ocr_detector;
pub mod ocr_glm;
pub mod ocr_merge;
pub mod ocr_pdf_splitter;
pub mod ocr_planner;
pub mod ocr_provider;
pub mod ocr_response;
pub mod parser;
pub mod passage;
pub mod pipeline;
pub mod planner;
pub mod privacy;
pub mod raptor;
pub mod rerank;
pub mod search;
pub mod security;
pub mod splitter;
pub mod sync;
pub mod synonyms;
pub mod table_index;
pub mod temp_guard;
pub mod token_counter;
pub mod types;
pub mod worker;

pub use engine::KbEngine;
pub use pipeline::{ingest_directory, process_document, process_document_async};
pub use search::kb_search;
pub use types::*;
pub use worker::{
    run_artifact_worker_once, run_kb_cmd_worker_once, run_kb_worker_loop, run_kb_worker_once,
    run_mine_synonyms_worker_once, run_ocr_worker_once, spawn_kb_worker_thread,
};
