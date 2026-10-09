//! Spreadsheet parser using calamine with structured sheet/row output (P2-012)
//! FIX9-04: 为每个 sheet block 写入 source span
//! FIX9-18: 表头行不写入数据行

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use calamine::{open_workbook_auto_from_rs, Data, Reader};
use std::collections::HashMap;

pub struct XlsxParser;

impl Default for XlsxParser {
    fn default() -> Self {
        Self::new()
    }
}

impl XlsxParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for XlsxParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let cursor = std::io::Cursor::new(data);
        let mut workbook = open_workbook_auto_from_rs(cursor)
            .map_err(|e| GBrainError::FileError(format!("spreadsheet open failed: {}", e)))?;

        let sheets = workbook.sheet_names().to_vec();
        let mut parts = Vec::new();
        let mut blocks: Vec<crate::kb::types::ParsedBlock> = Vec::new();
        // FIX9-04: 记录每个 sheet block 在全文中的起止偏移
        let mut sheet_spans: Vec<(usize, usize)> = Vec::new();
        let mut global_offset: usize = 0;

        for sheet_name in &sheets {
            let sheet_header = format!("=== {} ===", sheet_name);
            // FIX9-04: 记录此 sheet 在全文中的起始偏移
            let start = global_offset;
            parts.push(sheet_header.clone());
            // FIX10-08: 统一使用字符偏移（chars().count()），禁止混用 byte 长度
            global_offset += sheet_header.chars().count() + 1; // 加 "\n" 分隔符

            let mut headers: Vec<String> = Vec::new();
            // FIX9-18: 分离表头行和数据行，表头不写入 row_data
            let mut row_data: Vec<serde_json::Value> = Vec::new();
            let mut row_count: usize = 0;
            let mut is_header_row = true;

            if let Ok(range) = workbook.worksheet_range(sheet_name) {
                for row in range.rows() {
                    let cells: Vec<String> = row
                        .iter()
                        .map(|cell| match cell {
                            Data::String(s) => s.clone(),
                            Data::Int(i) => i.to_string(),
                            Data::Float(f) => f.to_string(),
                            Data::Bool(b) => b.to_string(),
                            Data::DateTime(dt) => dt.to_string(),
                            _ => String::new(),
                        })
                        .collect();

                    // FIX9-18: 第一行作为表头，不写入 row_data
                    if is_header_row && !cells.is_empty() {
                        headers = cells.clone();
                        is_header_row = false;
                        continue; // 表头行不参与数据行计数
                    }

                    // 存储为 JSON object 而非 JSON string
                    let row_obj: serde_json::Value = serde_json::to_value(
                        headers
                            .iter()
                            .enumerate()
                            .map(|(i, h)| (h.clone(), cells.get(i).cloned().unwrap_or_default()))
                            .collect::<HashMap<_, _>>(),
                    )
                    .unwrap_or(serde_json::Value::Object(Default::default()));
                    row_data.push(row_obj);
                    parts.push(cells.join("\t"));
                    row_count += 1;
                    // FIX10-08: 统一使用字符偏移（chars().count()），禁止混用 byte 长度
                    global_offset += cells.join("\t").chars().count() + 1;
                }
            }

            // FIX9-04: 记录此 sheet 在全文中的结束偏移
            let end = global_offset;
            sheet_spans.push((start, end));

            // 每个 sheet 一个 ParsedBlock，metadata 包含完整 sheet 数据
            // FIX9-18: row_count 已不含表头行，不再需要 saturating_sub(1)
            let sheet_meta = serde_json::json!({
                "name": sheet_name,
                "headers": headers,
                "row_count": row_count as i32,
                "rows": row_data,
            });
            // FIX9-04: 写入真实的 source span
            let (s_start, s_end) = sheet_spans.last().copied().unwrap_or((0, 0));
            blocks.push(crate::kb::types::ParsedBlock {
                text: parts[parts.len() - row_count - 1..].join("\n"),
                title_path: sheet_name.clone(),
                page_number: None,
                source_start: Some(s_start as i32),
                source_end: Some(s_end as i32),
                block_type: "table".to_string(),
                metadata: serde_json::to_string(&sheet_meta).unwrap_or_default(),
            });
        }

        let content = parts.join("\n");
        let mut metadata = HashMap::new();
        metadata.insert("sheet_count".to_string(), sheets.len().to_string());
        let media_refs = spreadsheet_picture_refs(workbook.pictures().unwrap_or_default());

        Ok(ParsedDocument {
            content,
            metadata,
            blocks: Some(blocks),
            media_refs,
        })
    }

    fn extensions(&self) -> &[&str] {
        &["xlsx", "xls"]
    }
}

fn spreadsheet_picture_refs(pictures: Vec<(String, Vec<u8>)>) -> Vec<crate::kb::types::MediaRef> {
    pictures
        .into_iter()
        .enumerate()
        .filter_map(|(idx, (ext, bytes))| {
            let ext = ext.trim_start_matches('.').trim().to_ascii_lowercase();
            let ext = if ext.is_empty() {
                "bin".to_string()
            } else {
                ext
            };
            super::embedded_media::embedded_image_ref(
                format!("embedded://spreadsheet/media/image{}.{}", idx + 1, ext),
                bytes,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spreadsheet_parser_accepts_legacy_xls_extension() {
        let parser = XlsxParser::new();

        assert!(parser.extensions().contains(&"xlsx"));
        assert!(parser.extensions().contains(&"xls"));
    }

    #[test]
    fn spreadsheet_picture_refs_keep_embedded_bytes_for_ocr() {
        let refs = spreadsheet_picture_refs(vec![("jpg".to_string(), vec![0xff, 0xd8, 0xff])]);

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].mime_type.as_deref(), Some("image/jpeg"));
        assert_eq!(refs[0].byte_size, Some(3));
        assert!(refs[0].embedded_data_base64.is_some());
    }
}
