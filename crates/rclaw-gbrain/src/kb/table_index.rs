//! 表格专用索引 — 为 XLSX/CSV 建立结构化行索引
//!
//! P2-013: 写入 kb_tables 表
//! P2-014: 写入 kb_table_rows 表
//! P2-015: 为 row_text 生成 FTS token
//! P2-016: 为表格行生成 embedding 输入

use crate::error::Result;
use rusqlite::{params, Connection};

/// 写入表格元信息到 kb_tables
///
/// 表格标题（sheet_name）和列名（headers）会在写入行 token 时拼入索引文本，
/// 使表格检索可以命中“表名/列名/单元格值”三类查询。
/// P1 修复：增加 version_id 参数，使表格索引与 active version 原子绑定。
pub fn insert_table(
    conn: &Connection,
    document_id: i64,
    version_id: i64,
    sheet_name: &str,
    headers: &[String],
    row_count: i32,
) -> Result<i64> {
    let headers_json = serde_json::to_string(headers).unwrap_or_else(|_| "[]".to_string());
    conn.execute(
        "INSERT INTO kb_tables (document_id, version_id, sheet_name, headers, column_count, row_count) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            document_id,
            version_id,
            sheet_name,
            headers_json,
            headers.len() as i32,
            row_count
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 写入单行数据到 kb_table_rows
pub fn insert_table_row(
    conn: &Connection,
    table_id: i64,
    row_index: i32,
    row_text: &str,
    row_json: &str,
) -> Result<i64> {
    let token_source = table_row_token_source(conn, table_id, row_text);
    let row_tokens = crate::nlp::chinese::tokenize_content(&token_source);
    conn.execute(
        "INSERT INTO kb_table_rows (table_id, row_index, row_text, row_tokens, row_json) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![table_id, row_index, row_text, row_tokens, row_json],
    )?;
    Ok(conn.last_insert_rowid())
}

fn table_row_token_source(conn: &Connection, table_id: i64, row_text: &str) -> String {
    let table_context: Option<(String, String)> = conn
        .query_row(
            "SELECT sheet_name, headers FROM kb_tables WHERE id = ?1",
            params![table_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .ok();

    if let Some((sheet_name, headers_json)) = table_context {
        let headers = serde_json::from_str::<Vec<String>>(&headers_json)
            .unwrap_or_default()
            .join(" ");
        format!("{} {} {}", sheet_name, headers, row_text)
    } else {
        row_text.to_string()
    }
}

/// 构建用于 embedding 的表格行文本
pub fn build_table_row_embedding_text(
    sheet_name: &str,
    headers: &[String],
    row_json: &str,
) -> String {
    let headers_str = headers.join(", ");
    format!(
        "表格：{}\n表头：{}\n行数据：{}",
        sheet_name, headers_str, row_json
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_embedding_text() {
        let text = build_table_row_embedding_text(
            "Sheet1",
            &["Name".into(), "Age".into()],
            r#"{"Name":"Alice","Age":"30"}"#,
        );
        assert!(text.contains("表格：Sheet1"));
        assert!(text.contains("表头：Name, Age"));
        assert!(text.contains("行数据："));
    }
}
