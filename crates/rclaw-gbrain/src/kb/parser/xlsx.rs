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
/// xlsx 内单个 zip 条目**实际**解压大小上限：calamine 会把 sharedStrings 整体读入并拆成
/// String 列表、把 xl/media 全部 read_to_end 常驻内存，而 zip 的 `ZipFile::read` 并不校验
/// 声明的 size()，1 MB 的 zip 炸弹就能膨胀出 GB 级 XML。行数 / 单元格数另有上限，
/// 64 MiB 以上的 sharedStrings 本就超出可解析范围。
const MAX_XLSX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;
/// 内嵌图片解压后的总量上限：calamine 全部常驻内存，随后还要 base64 复制一份。
const MAX_XLSX_MEDIA_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// 试解压的全档预算下限与相对文件大小的倍数：zip 中央目录允许多个条目指向同一段数据，
/// 一个 65 KB 的 64 MiB 炸弹流被几万个条目复用，就能让逐条目试解压跑上几个小时。
const MAX_XLSX_INFLATED_TOTAL_FLOOR_BYTES: u64 = 512 * 1024 * 1024;
const MAX_XLSX_INFLATED_TOTAL_RATIO: u64 = 16;
/// zip 条目数上限：真实工作簿（含图片 / 图表 / 关系文件）远少于此，超出多半是伪造的中央目录。
const MAX_XLSX_ENTRIES: usize = 20_000;
/// calamine 0.26 `read_pictures` 会整体读入内存的图片扩展名（大小写敏感，与其源码一致）
const CALAMINE_PICTURE_EXTS: [&str; 12] = [
    "emf", "wmf", "pict", "jpeg", "jpg", "png", "dib", "gif", "tiff", "eps", "bmp", "wpg",
];
/// OLE2 / CFB 复合文档魔数（legacy .xls BIFF 容器）
const CFB_MAGIC: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

/// calamine 打开工作簿时会 `read_to_end` 常驻内存的图片条目：
/// xlsx / xlsb 按 `starts_with("xl/media")`（无斜杠，`xl/media1.png` 也算），
/// ods 按 `starts_with("Pictures")`，并且扩展名要在 `CALAMINE_PICTURE_EXTS` 里。
fn is_calamine_picture_entry(name: &str) -> bool {
    (name.starts_with("xl/media") || name.starts_with("Pictures"))
        && name
            .split('.')
            .next_back()
            .is_some_and(|ext| CALAMINE_PICTURE_EXTS.contains(&ext))
}

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

/// 只计数不保存的 Write 目标，用于限额试解压
#[derive(Default)]
struct CountingSink(u64);

impl std::io::Write for CountingSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 对 xlsx 容器逐条目做限额试解压（最多读 上限 + 1 字节），按实际解压字节数拒绝炸弹。
fn reject_oversized_xlsx_entries(data: &[u8]) -> Result<(), GBrainError> {
    use std::io::Read as _;
    let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(data)) else {
        return Ok(()); // 非 zip 交给 calamine 自行报错
    };
    // 只接受 OOXML 容器：calamine 的自动识别在 xlsx / xlsb 都打不开时会退到 ODS 路径，
    // 而 Ods::new 打开时就按 table:number-columns-repeated 逐个 push 单元格，几百字节的
    // content.xml 就能申请几十 GB（分配失败是 abort）。KB 本就不支持 .ods，直接拒绝。
    // calamine 查 xl/workbook.xml 时忽略 ASCII 大小写（xlsb 的 workbook.bin 则是精确匹配）
    let has_workbook_part = archive
        .file_names()
        .any(|n| n.eq_ignore_ascii_case("xl/workbook.xml") || n == "xl/workbook.bin");
    if !has_workbook_part {
        return Err(GBrainError::InvalidInput(
            "不支持的电子表格容器（缺少 xl/workbook.xml），仅支持 .xlsx / .xlsb".to_string(),
        ));
    }
    if archive.len() > MAX_XLSX_ENTRIES {
        return Err(GBrainError::InvalidInput(format!(
            "spreadsheet 包含 {} 个 zip 条目，超过上限 {MAX_XLSX_ENTRIES}",
            archive.len()
        )));
    }
    let mut media_total: u64 = 0;
    let mut inflate_budget = MAX_XLSX_INFLATED_TOTAL_FLOOR_BYTES
        .max((data.len() as u64).saturating_mul(MAX_XLSX_INFLATED_TOTAL_RATIO));
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name().replace('\\', "/");
        if entry.size() > MAX_XLSX_ENTRY_BYTES {
            return Err(GBrainError::InvalidInput(format!(
                "spreadsheet 条目 {name} 解压后过大 ({} 字节 > 上限 {MAX_XLSX_ENTRY_BYTES})",
                entry.size()
            )));
        }
        // 解压错误（CRC / 截断）交给 calamine 报错，这里只关心已解出的字节数
        let mut sink = CountingSink::default();
        let _ = std::io::copy(&mut entry.take(MAX_XLSX_ENTRY_BYTES + 1), &mut sink);
        if sink.0 > MAX_XLSX_ENTRY_BYTES {
            return Err(GBrainError::InvalidInput(format!(
                "spreadsheet 条目 {name} 解压后过大 (> 上限 {MAX_XLSX_ENTRY_BYTES})"
            )));
        }
        inflate_budget = inflate_budget.checked_sub(sink.0).ok_or_else(|| {
            GBrainError::InvalidInput("spreadsheet 全部条目解压总量超过上限，拒绝解析".to_string())
        })?;
        if is_calamine_picture_entry(&name) {
            media_total = media_total.saturating_add(sink.0);
            if media_total > MAX_XLSX_MEDIA_TOTAL_BYTES {
                return Err(GBrainError::InvalidInput(format!(
                    "spreadsheet 内嵌图片总量超过上限 {MAX_XLSX_MEDIA_TOTAL_BYTES} 字节"
                )));
            }
        }
    }
    Ok(())
}

impl DocumentParser for XlsxParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        // legacy .xls（BIFF/CFB）：calamine 0.26 的 xls/cfb 读取按文件里声明的
        // 行列范围 / SST 条数 / FAT 长度直接 reserve，几 KB 的伪造文件就能申请几十 GB，
        // 分配失败是 abort（不是 panic，catch_unwind 拦不住），整个进程被杀。
        // calamine 按魔数而非扩展名选路径，所以必须按内容拒绝。
        if data.starts_with(&CFB_MAGIC) {
            return Err(GBrainError::InvalidInput(
                "暂不支持 legacy .xls（BIFF）格式，请用 Excel / LibreOffice 另存为 .xlsx 后重试"
                    .to_string(),
            ));
        }
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

    // ---- 容器预检（reject_oversized_xlsx_entries）----

    fn build_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use std::io::Write as _;
        let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            writer.start_file(*name, opts).unwrap();
            writer.write_all(body).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    fn err_text(r: Result<(), GBrainError>) -> String {
        r.err().map(|e| e.to_string()).unwrap_or_default()
    }

    #[test]
    fn container_check_accepts_minimal_ooxml_and_small_media() {
        let data = build_zip(&[
            ("[Content_Types].xml", b"<Types/>"),
            ("xl/Workbook.xml", b"<workbook/>"),
            ("xl/media/image1.png", &[0x89, b'P', b'N', b'G']),
            ("xl/media1.png", &[0x89, b'P', b'N', b'G']),
        ]);
        assert!(reject_oversized_xlsx_entries(&data).is_ok());
    }

    #[test]
    fn container_check_rejects_non_ooxml_zip() {
        // ODS 结构的 zip：calamine 会退到 Ods::new 按 number-columns-repeated 逐个分配单元格
        let data = build_zip(&[
            (
                "mimetype",
                b"application/vnd.oasis.opendocument.spreadsheet",
            ),
            ("content.xml", b"<office:document-content/>"),
        ]);
        assert!(err_text(reject_oversized_xlsx_entries(&data)).contains("容器"));
    }

    #[test]
    fn container_check_rejects_inflated_media_total_without_slash_prefix() {
        // 与 calamine 一致：`xl/media` 前缀不要求斜杠，每个条目都低于单条目上限，但总量超限
        let big = vec![0u8; (MAX_XLSX_MEDIA_TOTAL_BYTES / 2 + 1024 * 1024) as usize];
        let data = build_zip(&[
            ("xl/workbook.xml", b"<workbook/>"),
            ("xl/media1.png", &big),
            ("xl/media2.png", &big),
        ]);
        assert!(err_text(reject_oversized_xlsx_entries(&data)).contains("图片总量"));
    }

    #[test]
    fn container_check_rejects_entry_inflating_past_cap() {
        let bomb = vec![0u8; (MAX_XLSX_ENTRY_BYTES + 1024 * 1024) as usize];
        let data = build_zip(&[
            ("xl/workbook.xml", b"<workbook/>"),
            ("xl/sharedStrings.xml", &bomb),
        ]);
        assert!(err_text(reject_oversized_xlsx_entries(&data)).contains("解压后过大"));
    }

    #[test]
    fn parse_rejects_cfb_legacy_xls_by_magic() {
        let mut data = CFB_MAGIC.to_vec();
        data.extend(std::iter::repeat_n(0u8, 4096));
        let err = XlsxParser::new()
            .parse(&data)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        assert!(err.contains("legacy .xls"), "{err}");
    }
}
