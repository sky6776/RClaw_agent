//! DOCX parser with heading style detection and structure preservation
//!
//! P2-007: Identifies Word heading styles (Heading1-6) and generates heading_path
//! P2-008: Preserves paragraph/list/table structure blocks

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use crate::kb::types::{MediaRef, ParsedBlock};
use std::collections::HashMap;
use std::io::Read;

pub struct DocxParser;

type DocxText = (String, Vec<String>, Vec<ParsedBlock>);
type DocxStructured = (String, Vec<String>, Vec<ParsedBlock>, Vec<MediaRef>);

impl Default for DocxParser {
    fn default() -> Self {
        Self::new()
    }
}

impl DocxParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for DocxParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        let (content, headings, blocks, media_refs) = extract_docx_text_structured(data)?;
        let mut metadata = HashMap::new();
        if !headings.is_empty() {
            metadata.insert("headings".to_string(), headings.join(" > "));
        }
        if !blocks.is_empty() {
            metadata.insert("table_count".to_string(), blocks.len().to_string());
        }
        Ok(ParsedDocument {
            content,
            metadata,
            blocks: (!blocks.is_empty()).then_some(blocks),
            media_refs,
        })
    }

    fn extensions(&self) -> &[&str] {
        &["docx"]
    }
}

/// P2-007: 判断是否是标题样式并返回层级
fn heading_level(style: &str) -> Option<u32> {
    let s = style.to_lowercase();
    for level in 1..=6 {
        if s.contains(&format!("heading{}", level)) || s.contains(&format!("heading {}", level)) {
            return Some(level);
        }
    }
    if s.contains("heading") || s.contains("title") {
        return Some(1);
    }
    None
}

/// P2-007 + P2-008: 结构化提取 DOCX 文本，同时收集标题层级
fn extract_docx_text_structured(data: &[u8]) -> Result<DocxStructured, GBrainError> {
    let reader = std::io::Cursor::new(data);
    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| GBrainError::FileError(format!("DOCX open failed: {}", e)))?;

    let mut xml_content = String::new();
    {
        let mut file = archive
            .by_name("word/document.xml")
            .map_err(|_| GBrainError::FileError("DOCX missing word/document.xml".to_string()))?;
        file.read_to_string(&mut xml_content)
            .map_err(|e| GBrainError::FileError(format!("DOCX read failed: {}", e)))?;
    }

    let (content, headings, blocks) = extract_docx_text_from_document_xml(&xml_content)?;
    let media_refs = extract_docx_embedded_images(&mut archive);
    Ok((content, headings, blocks, media_refs))
}

fn extract_docx_embedded_images<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Vec<MediaRef> {
    let mut refs = Vec::new();
    for i in 0..archive.len() {
        let Ok(mut file) = archive.by_index(i) else {
            continue;
        };
        let name = file.name().replace('\\', "/");
        if !name.starts_with("word/media/") || name.ends_with('/') {
            continue;
        }
        let mut bytes = Vec::new();
        if file.read_to_end(&mut bytes).is_err() {
            continue;
        }
        if let Some(media) =
            super::embedded_media::embedded_image_ref(format!("embedded://docx/{}", name), bytes)
        {
            refs.push(media);
        }
    }
    refs
}

fn extract_docx_text_from_document_xml(xml_content: &str) -> Result<DocxText, GBrainError> {
    let mut parts = Vec::new();
    let mut headings = Vec::new();
    let mut blocks = Vec::new();
    let mut current_text = String::new();
    let mut in_t_tag = false;
    let mut in_pstyle = false;
    let mut current_heading_level: Option<u32> = None;
    let mut in_table = false;
    let mut in_table_cell = false;
    let mut current_cell = String::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut table_index = 0usize;

    let mut reader = quick_xml::Reader::from_str(xml_content);
    let mut buf = Vec::new();

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(ref e)) => {
                let local = e.local_name();
                let name = local.as_ref();
                if name == b"tbl" && !in_table {
                    in_table = true;
                    table_rows.clear();
                } else if name == b"tr" && in_table {
                    current_row.clear();
                } else if name == b"tc" && in_table {
                    in_table_cell = true;
                    current_cell.clear();
                }
                if name == b"t" {
                    in_t_tag = true;
                }
                if name == b"pStyle" && !in_table {
                    in_pstyle = true;
                }
                // P2-007: 从 pStyle 的 w:val 属性提取样式名
                if name == b"pStyle" && !in_table {
                    for attr in e.attributes().flatten() {
                        let attr_local = attr.key.local_name();
                        if attr_local.as_ref() == b"val" {
                            let style = String::from_utf8_lossy(&attr.value).to_string();
                            current_heading_level = heading_level(&style);
                        }
                    }
                }
            }
            Ok(quick_xml::events::Event::Text(ref e)) if in_t_tag || in_pstyle => {
                if let Ok(text) = e.unescape() {
                    if in_table && in_table_cell && in_t_tag {
                        append_cell_text(&mut current_cell, &text);
                    } else if in_pstyle {
                        // pStyle 的样式名主要来自属性；文本内容无需保留。
                    } else {
                        current_text.push_str(&text);
                    }
                }
            }
            Ok(quick_xml::events::Event::End(e)) => {
                let local = e.local_name();
                let name = local.as_ref();
                if name == b"t" {
                    in_t_tag = false;
                } else if name == b"pStyle" {
                    in_pstyle = false;
                } else if name == b"p" {
                    if !in_table {
                        let trimmed = current_text.trim().to_string();
                        if !trimmed.is_empty() {
                            if let Some(level) = current_heading_level {
                                let prefix = "#".repeat(level as usize);
                                headings.push(format!("{} {}", prefix, trimmed));
                                parts.push(format!("{} {}", prefix, trimmed));
                            } else {
                                parts.push(trimmed);
                            }
                        }
                    }
                    current_text.clear();
                    current_heading_level = None;
                } else if name == b"tc" && in_table_cell {
                    current_row.push(current_cell.trim().to_string());
                    current_cell.clear();
                    in_table_cell = false;
                } else if name == b"tr" && in_table {
                    if current_row.iter().any(|cell| !cell.trim().is_empty()) {
                        table_rows.push(current_row.clone());
                    }
                    current_row.clear();
                } else if name == b"tbl" && in_table {
                    if let Some((table_text, block)) =
                        build_docx_table_block(table_index, &table_rows)
                    {
                        parts.push(table_text);
                        blocks.push(block);
                        table_index += 1;
                    }
                    table_rows.clear();
                    in_table = false;
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(e) => {
                return Err(GBrainError::FileError(format!(
                    "DOCX XML parse error: {}",
                    e
                )))
            }
            _ => {}
        }
        buf.clear();
    }

    Ok((parts.join("\n\n"), headings, blocks))
}

fn append_cell_text(cell: &mut String, text: &str) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return;
    }
    if !cell.is_empty() {
        cell.push(' ');
    }
    cell.push_str(trimmed);
}

fn build_docx_table_block(
    table_index: usize,
    rows: &[Vec<String>],
) -> Option<(String, crate::kb::types::ParsedBlock)> {
    if rows.is_empty() {
        return None;
    }
    let column_count = rows.iter().map(Vec::len).max().unwrap_or(0);
    if column_count == 0 {
        return None;
    }

    let headers = normalize_headers(
        rows.first()
            .map(|r| pad_row(r, column_count))
            .unwrap_or_default()
            .as_slice(),
    );
    let data_rows: Vec<Vec<String>> = rows
        .iter()
        .skip(1)
        .map(|r| pad_row(r, column_count))
        .collect();
    let table_name = format!("Table {}", table_index + 1);
    let markdown = table_to_markdown(&headers, &data_rows);
    let row_values: Vec<serde_json::Value> = data_rows
        .iter()
        .map(|row| {
            let map: HashMap<String, String> = headers
                .iter()
                .enumerate()
                .map(|(i, h)| (h.clone(), row.get(i).cloned().unwrap_or_default()))
                .collect();
            serde_json::to_value(map).unwrap_or_else(|_| serde_json::json!({}))
        })
        .collect();
    let metadata = serde_json::json!({
        "name": table_name,
        "headers": headers,
        "row_count": row_values.len() as i32,
        "rows": row_values,
    });
    let block = crate::kb::types::ParsedBlock {
        text: markdown.clone(),
        title_path: metadata
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Table")
            .to_string(),
        page_number: None,
        source_start: None,
        source_end: None,
        block_type: "table".to_string(),
        metadata: serde_json::to_string(&metadata).unwrap_or_default(),
    };
    Some((markdown, block))
}

fn pad_row(row: &[String], column_count: usize) -> Vec<String> {
    (0..column_count)
        .map(|i| row.get(i).cloned().unwrap_or_default())
        .collect()
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

fn table_to_markdown(headers: &[String], rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    out.push_str("| ");
    out.push_str(
        &headers
            .iter()
            .map(|h| escape_markdown_cell(h))
            .collect::<Vec<_>>()
            .join(" | "),
    );
    out.push_str(" |\n| ");
    out.push_str(&vec!["---"; headers.len()].join(" | "));
    out.push_str(" |\n");
    for row in rows {
        out.push_str("| ");
        out.push_str(
            &row.iter()
                .map(|c| escape_markdown_cell(c))
                .collect::<Vec<_>>()
                .join(" | "),
        );
        out.push_str(" |\n");
    }
    out.trim_end().to_string()
}

fn escape_markdown_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_heading_level() {
        assert_eq!(heading_level("Heading1"), Some(1));
        assert_eq!(heading_level("heading 3"), Some(3));
        assert_eq!(heading_level("Title"), Some(1));
        assert!(heading_level("Normal").is_none());
    }

    #[test]
    fn docx_xml_table_outputs_markdown_and_table_block() {
        let xml = r#"
            <w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main">
              <w:body>
                <w:p><w:r><w:t>Intro</w:t></w:r></w:p>
                <w:tbl>
                  <w:tr>
                    <w:tc><w:p><w:r><w:t>Name</w:t></w:r></w:p></w:tc>
                    <w:tc><w:p><w:r><w:t>Age</w:t></w:r></w:p></w:tc>
                  </w:tr>
                  <w:tr>
                    <w:tc><w:p><w:r><w:t>Alice</w:t></w:r></w:p></w:tc>
                    <w:tc><w:p><w:r><w:t>30</w:t></w:r></w:p></w:tc>
                  </w:tr>
                </w:tbl>
              </w:body>
            </w:document>
        "#;

        let (content, _, blocks) = extract_docx_text_from_document_xml(xml).expect("parse xml");
        assert!(content.contains("Intro"));
        assert!(content.contains("| Name | Age |"));
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].block_type, "table");
        let meta: serde_json::Value = serde_json::from_str(&blocks[0].metadata).unwrap();
        assert_eq!(meta["headers"][0], "Name");
        assert_eq!(meta["rows"][0]["Name"], "Alice");
    }

    #[test]
    fn docx_parser_extracts_embedded_images() {
        let mut docx = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut docx);
            let opts = zip::write::SimpleFileOptions::default();
            writer.start_file("word/document.xml", opts).unwrap();
            writer
                .write_all(
                    br#"<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>Hello</w:t></w:r></w:p></w:body></w:document>"#,
                )
                .unwrap();
            writer.start_file("word/media/image1.png", opts).unwrap();
            writer.write_all(&[0x89, b'P', b'N', b'G']).unwrap();
            writer.finish().unwrap();
        }

        let parsed = DocxParser::new().parse(&docx.into_inner()).expect("parse");

        assert_eq!(parsed.media_refs.len(), 1);
        assert_eq!(parsed.media_refs[0].media_type, "image");
        assert_eq!(parsed.media_refs[0].mime_type.as_deref(), Some("image/png"));
        assert!(parsed.media_refs[0].embedded_data_base64.is_some());
    }
}
