//! CSV parser with structured row output (P2-011)

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use std::collections::HashMap;

pub struct CsvParser {
    delimiter: u8,
}

impl Default for CsvParser {
    fn default() -> Self {
        Self::new()
    }
}

impl CsvParser {
    pub fn new() -> Self {
        Self { delimiter: b',' }
    }
}

impl DocumentParser for CsvParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(self.delimiter)
            .flexible(true)
            .has_headers(false)
            .from_reader(data);

        let mut rows = Vec::new();
        let mut headers: Vec<String> = Vec::new();
        // CSV 行数上限，防止超大文件耗尽内存
        const MAX_CSV_ROWS: usize = 100_000;
        // C4 fix: 存储 serde_json::Value 而非预序列化的 JSON 字符串，
        // 避免最终 serde_json::to_string(&row_records) 产生双重转义。
        let mut row_records: Vec<serde_json::Value> = Vec::new();
        let mut data_row_count: usize = 0;

        for result in reader.records() {
            match result {
                Ok(record) => {
                    if data_row_count >= MAX_CSV_ROWS {
                        return Err(GBrainError::InvalidInput(format!(
                            "CSV 行数超过上限 {}，请拆分文件后重试",
                            MAX_CSV_ROWS
                        )));
                    }
                    let fields: Vec<String> = record.iter().map(|s| s.to_string()).collect();
                    if headers.is_empty() && !fields.is_empty() {
                        headers = normalize_headers(&fields);
                        rows.push(headers.join("\t"));
                        continue;
                    }
                    let row_map: HashMap<String, String> = headers
                        .iter()
                        .enumerate()
                        .map(|(i, h)| (h.clone(), fields.get(i).cloned().unwrap_or_default()))
                        .collect();
                    // 直接存为 Value，不再提前序列化
                    row_records.push(
                        serde_json::to_value(&row_map)
                            .expect("JSON 序列化不应失败: HashMap<String, String> 始终可序列化"),
                    );
                    rows.push(fields.join("\t"));
                    data_row_count += 1;
                }
                Err(e) => {
                    // M50: 畸形 CSV 行跳过时记录 debug 日志，便于排查数据质量问题
                    tracing::debug!(error = %e, "CSV 行解析错误，跳过");
                    continue;
                }
            }
        }

        let content = rows.join("\n");
        let mut metadata = HashMap::new();
        metadata.insert("row_count".to_string(), data_row_count.to_string());
        metadata.insert("column_count".to_string(), headers.len().to_string());
        if !headers.is_empty() {
            metadata.insert("headers".to_string(), headers.join(", "));
            metadata.insert(
                "row_json_list".to_string(),
                serde_json::to_string(&row_records).unwrap_or_default(),
            );
        }

        let blocks = if !headers.is_empty() {
            let sheet_meta = serde_json::json!({
                "name": "CSV",
                "headers": headers,
                "row_count": data_row_count as i32,
                "rows": row_records,
            });
            Some(vec![crate::kb::types::ParsedBlock {
                text: content.clone(),
                title_path: "CSV".to_string(),
                page_number: None,
                source_start: Some(0),
                source_end: Some(content.chars().count() as i32),
                block_type: "table".to_string(),
                metadata: serde_json::to_string(&sheet_meta).unwrap_or_default(),
            }])
        } else {
            None
        };

        Ok(ParsedDocument {
            content,
            metadata,
            blocks,
            media_refs: Vec::new(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["csv"]
    }
}

fn normalize_headers(cells: &[String]) -> Vec<String> {
    let mut seen: HashMap<String, usize> = HashMap::new();
    cells
        .iter()
        .enumerate()
        .map(|(i, cell)| {
            let base = if cell.trim().is_empty() {
                format!("column_{}", i + 1)
            } else {
                cell.trim().to_string()
            };
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            if *count == 1 {
                base
            } else {
                format!("{}_{}", base, count)
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kb::parser::DocumentParser;

    #[test]
    fn csv_parser_outputs_table_block_without_header_as_data_row() {
        let parser = CsvParser::new();
        let parsed = parser
            .parse(b"name,age\nAlice,30\nBob,40\n")
            .expect("parse csv");

        assert_eq!(parsed.metadata.get("row_count").unwrap(), "2");
        let blocks = parsed.blocks.expect("table block");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "table");
        let meta: serde_json::Value = serde_json::from_str(&blocks[0].metadata).unwrap();
        assert_eq!(meta["name"], "CSV");
        assert_eq!(meta["row_count"], 2);
        assert_eq!(meta["headers"][0], "name");
        assert_eq!(meta["rows"][0]["name"], "Alice");
    }
}
