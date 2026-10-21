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

/// 单个 sheet 允许的稠密单元格数（行跨度 × 列跨度）。calamine 的 `worksheet_range`
/// 会按包围盒做 `vec![Data::default(); rows*cols]` 稠密分配，一个只含 A1 与
/// XFD1048576 两个单元格的几 KB xlsx 就是 1.7e10 个元素，直接把进程打死。
const MAX_SHEET_DENSE_CELLS: u64 = 2_000_000;
/// 单个 sheet 的数据行上限（与 CSV 解析一致）。
const MAX_SHEET_ROWS: usize = 100_000;
/// xlsx 内单个 zip 条目声明的解压大小上限（sharedStrings / worksheets / media 在 open
/// 阶段即被整体读入内存）。声明值可伪造，只作快速拒绝。
const MAX_XLSX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;

/// 流式扫一遍 xlsx sheet 的单元格，返回其包围盒面积；非 xlsx 或读取失败返回 None。
fn xlsx_sheet_dense_cells<RS: std::io::Read + std::io::Seek>(
    workbook: &mut calamine::Sheets<RS>,
    sheet_name: &str,
) -> Option<u64> {
    let calamine::Sheets::Xlsx(xlsx) = workbook else {
        return None;
    };
    let mut reader = xlsx.worksheet_cells_reader(sheet_name).ok()?;
    let (mut min_r, mut max_r, mut min_c, mut max_c) = (u32::MAX, 0u32, u32::MAX, 0u32);
    let mut any = false;
    while let Ok(Some(cell)) = reader.next_cell() {
        let (r, c) = cell.get_position();
        any = true;
        min_r = min_r.min(r);
        max_r = max_r.max(r);
        min_c = min_c.min(c);
        max_c = max_c.max(c);
        let area = (u64::from(max_r - min_r) + 1) * (u64::from(max_c - min_c) + 1);
        if area > MAX_SHEET_DENSE_CELLS {
            return Some(area);
        }
    }
    if !any {
        return Some(0);
    }
    Some((u64::from(max_r - min_r) + 1) * (u64::from(max_c - min_c) + 1))
}

/// 对 xlsx 容器做 zip 条目声明大小的快速检查。
fn reject_oversized_xlsx_entries(data: &[u8]) -> Result<(), GBrainError> {
    let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(data)) else {
        return Ok(()); // 非 zip（如 BIFF xls）交给 calamine 自行报错
    };
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index_raw(i) else {
            continue;
        };
        if entry.size() > MAX_XLSX_ENTRY_BYTES {
            return Err(GBrainError::InvalidInput(format!(
                "spreadsheet 条目 {} 解压后过大 ({} 字节 > 上限 {MAX_XLSX_ENTRY_BYTES})",
                entry.name(),
                entry.size()
            )));
        }
    }
    Ok(())
}

impl DocumentParser for XlsxParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        reject_oversized_xlsx_entries(data)?;
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
            if xlsx_sheet_dense_cells(&mut workbook, sheet_name)
                .is_some_and(|area| area > MAX_SHEET_DENSE_CELLS)
            {
                tracing::warn!(
                    sheet = %sheet_name,
                    "spreadsheet sheet 单元格范围超过上限 {MAX_SHEET_DENSE_CELLS}，跳过该 sheet"
                );
                continue;
            }
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
                    if row_count >= MAX_SHEET_ROWS {
                        tracing::warn!(
                            sheet = %sheet_name,
                            "spreadsheet sheet 行数超过上限 {MAX_SHEET_ROWS}，截断后续行"
                        );
                        break;
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
