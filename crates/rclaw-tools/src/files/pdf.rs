//! PDF 读取路由（设计决策见 docs/adr/0003-pdf-read-local-extraction.md）。
//!
//! `read` 工具对 `.pdf` 的处理分两条路：
//! - 文本型 PDF 本地提取为文本：零额外 token 成本、任何模型可读、可 offset/limit 行分页；
//! - 扫描/图像/混合型走既有附件路径（多模态附件，并提示可激活 glmocr skill）；
//! - 永不硬失败：检测/提取的任何失败一律降级为附件路径，原因写进结果头。
//!
//! 模型不需要理解 PDF 类型学——路由由代码做。本文件含「检测→路由」纯决策、
//! 提取文本的内存分页，以及把两者串起来的主入口 [`read_pdf`]（IO 调用与结果组装）。

use super::*;
use pdf_inspector::PdfType;

/// `read` 对一个 PDF 的处理路由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PdfRoute {
    /// 文本型：本地提取为文本（行分页返回）。
    Extract,
    /// 扫描/图像/混合型（以及后续一切降级场景）：走既有多模态附件路径。
    Attach,
}

/// 按 pdf-inspector 的类型判定结果选择路由。
///
/// 路由规则（ADR-0003）：`TextBased` → 本地提取；其余类型（扫描/图像/混合）→ 附件路径。
/// 「文本型但编码损坏」「检测/提取失败」「提取结果为空」等降级场景不在此函数的输入域内，
/// 由调用方兜底处理。
pub fn route_pdf(pdf_type: &pdf_inspector::PdfType) -> PdfRoute {
    match pdf_type {
        pdf_inspector::PdfType::TextBased => PdfRoute::Extract,
        pdf_inspector::PdfType::Scanned
        | pdf_inspector::PdfType::ImageBased
        | pdf_inspector::PdfType::Mixed => PdfRoute::Attach,
    }
}

/// PDF 结果头里的类型标签（中文）。
fn pdf_type_label(pdf_type: &PdfType) -> &'static str {
    match pdf_type {
        pdf_inspector::PdfType::TextBased => "文本型",
        pdf_inspector::PdfType::Scanned => "扫描型",
        pdf_inspector::PdfType::ImageBased => "图像型",
        pdf_inspector::PdfType::Mixed => "混合型",
    }
}

/// 附件路径的结果头。
///
/// 告知模型该 PDF 的类型/页数（并附需 OCR 的页码，若有），以便其判断是否需要激活
/// glmocr skill 做精确提取。OCR 页列表超过 20 个时只列前 20 个再报总数，避免结果头
/// 挤占上下文。接受原始参数（而非 `PdfProcessResult`）是为了便于单测。
pub fn attach_header(pdf_type: &PdfType, page_count: u32, ocr_pages: &[u32]) -> String {
    let mut s = format!(
        "[PDF: {} {page_count}页 → 已作为多模态附件附给模型；若当前模型不支持视觉或需精确表格/长文档提取，可激活 glmocr skill",
        pdf_type_label(pdf_type)
    );
    if !ocr_pages.is_empty() {
        s.push_str("；需 OCR 页: ");
        // 页码为 1 索引（pdf-inspector 口径）。>20 个只列前 20 个再报总数。
        let listed = if ocr_pages.len() <= 20 {
            ocr_pages
        } else {
            &ocr_pages[..20]
        };
        s.push_str(
            &listed
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        );
        if ocr_pages.len() > 20 {
            s.push_str(&format!("...等共 {} 页", ocr_pages.len()));
        }
    }
    s.push(']');
    s
}

/// `read_pdf` 检测阶段把 `pdf_inspector::detect_pdf` 的成功观测折算成的领域类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectObserved {
    /// 检测成功。
    Detected {
        pdf_type: PdfType,
        has_encoding_issues: bool,
    },
}

/// 检测阶段的行动。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DetectAction {
    /// 文本型且编码完好：继续本地提取。
    Extract,
    /// 非文本型：附件路径，头用 [`attach_header`]。
    AttachWithHeader,
    /// 文本型但编码损坏：降级附件（建议激活 glmocr skill）。
    AttachDegrade,
}

/// 「检测观测 → 行动」纯决策（降级矩阵的可测接缝，IO 留在 [`read_pdf`] 外壳）。
fn decide_detect_action(obs: DetectObserved) -> DetectAction {
    match obs {
        DetectObserved::Detected {
            pdf_type,
            has_encoding_issues,
        } => match route_pdf(&pdf_type) {
            PdfRoute::Extract if has_encoding_issues => DetectAction::AttachDegrade,
            PdfRoute::Extract => DetectAction::Extract,
            PdfRoute::Attach => DetectAction::AttachWithHeader,
        },
    }
}

/// `read_pdf` 提取阶段把 `pdf_inspector::process_pdf` 的成功观测折算成的领域类型。
#[derive(Debug, PartialEq, Eq)]
enum ExtractObserved {
    /// 提取完成，携带提取文本（`None`/纯空白 = 未提取到）。
    Done(Option<String>),
}

/// 提取阶段的行动。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExtractAction {
    /// 有可用文本：以该文本作正文走行分页。
    ReturnText(String),
    /// 未提取到文本：降级附件。
    AttachDegrade,
}

/// 「提取观测 → 行动」纯决策（降级矩阵的可测接缝，IO 留在 [`read_pdf`] 外壳）。
fn decide_extract_action(obs: ExtractObserved) -> ExtractAction {
    match obs {
        // 未提取到文本（None/纯空白）→ 降级。
        ExtractObserved::Done(md) => match md.filter(|m| !m.trim().is_empty()) {
            Some(markdown) => ExtractAction::ReturnText(markdown),
            None => ExtractAction::AttachDegrade,
        },
    }
}

/// 附件路径（含降级）的统一组装：读为 data URI 附件，并把结果头前置到 content。
///
/// 结果头是给模型看的「发生了什么」，不占 [`READ_MAX_OUTPUT_BYTES`] 的正文预算
/// （预算由 `window_lines_mem` 对提取正文执行）。附件读取自身的失败（过大/不可读）
/// 仍由 `read_media_as_data_uri` 报错，头前置在其 content 之前。
fn attach_with_header(path: &str, kind: MediaKind, mime: &str, header: String) -> ToolResult {
    let mut r = read_media_as_data_uri(path, kind, mime);
    r.content = format!("{header}\n{}", r.content);
    r
}

/// 内存分页的返回窗口。
#[derive(Debug)]
pub(super) struct MemWindow {
    /// 选中的行（以 `'\n'` 连接，末尾不留换行）。
    pub(super) content: String,
    /// 提取文本总行数（含窗口外被跳过的行）。
    pub(super) total_lines: usize,
    /// 实际返回的行数。
    pub(super) lines_returned: usize,
}

/// 对已在内存中的 PDF 提取文本做行分页。
///
/// 与 [`super::helpers::read_lines_window`]（流式读文件）不同，提取文本已整篇在内存，
/// 只需在其上滑窗：`offset` 行 0 基，窗口外行仅跳过但仍计入 `total_lines`，供提示文案
/// 报告总量；`limit` 为最多返回行数。字节预算用与流式读取相同的
/// [`READ_MAX_OUTPUT_BYTES`]——达到即停止收集（其后行整行不入窗）但仍统计剩余行数；
/// 首行豁免预算（否则单行超预算 → 空响应，翻页无法推进）。
pub(super) fn window_lines_mem(content: &str, offset: usize, limit: usize) -> MemWindow {
    let mut lines: Vec<&str> = Vec::new();
    let mut total = 0usize;
    let mut kept = 0usize;
    let mut bytes_out = 0usize;
    let mut collecting = true;

    for (idx, line) in content.lines().enumerate() {
        total += 1;
        if !collecting || idx < offset || kept >= limit {
            continue; // 窗口外 / 预算用尽：跳过，但 total 仍统计
        }
        if kept > 0 && bytes_out + line.len() + 1 > READ_MAX_OUTPUT_BYTES {
            collecting = false; // 字节预算触顶：停止收集，其后行只计数
            continue;
        }
        lines.push(line);
        kept += 1;
        bytes_out += line.len() + 1; // +1 计入 '\n' 分隔符（与流式读取的计量口径一致）
    }

    MemWindow {
        content: lines.join("\n"),
        total_lines: total,
        lines_returned: kept,
    }
}

/// 为 PDF 文本的分页结果拼「翻页提示」。
///
/// 返回 `None` 表示整篇一次读完、无需提示（不占额外上下文）。行号为 1 基闭区间展示，
/// 「继续读取」的 offset 与流式读取口径一致 = `offset + lines_returned`（0 基）。
pub(super) fn pdf_window_note(win: &MemWindow, offset: usize) -> Option<String> {
    if win.lines_returned == 0 {
        // limit=0 或空输入也会折叠到此分支(schema minimum=1 + 空提取走附件降级,生产不可达)。
        return Some(format!(
            "[read: offset={offset} 超出 PDF 文本范围（共 {} 行）]",
            win.total_lines
        ));
    }
    let start = offset + 1;
    let end = offset + win.lines_returned;
    let total = win.total_lines;
    if end >= total {
        if offset == 0 {
            return None; // 整篇一次读完，不加提示
        }
        return Some(format!(
            "[read: PDF 文本第 {start}–{end} 行，已到末尾（共 {total} 行）]"
        ));
    }
    Some(format!(
        "[read: PDF 文本第 {start}–{end} 行，仍有后续；继续读取请传 offset={end}]"
    ))
}

/// `read` 工具的 PDF 主入口：检测 → 纯决策分派 → 提取分页 / 附件（含降级）。
///
/// 永不硬失败：检测/提取的任何失败、以及「文本型但没提取到文本」，一律降级为
/// 多模态附件路径，原因写进结果头。
pub(super) fn read_pdf(
    path: &str,
    kind: MediaKind,
    mime: &str,
    offset: usize,
    limit: usize,
) -> ToolResult {
    // 前置校验：检测与提取都会把整份 PDF 载入内存，必须先按普通文件 + 附件同一上限把关
    // ——否则一个超大 .pdf（或改名成 .pdf 的任意大文件）在任何分支被整份读入前就能把
    // 进程内存打爆；超限文件即便走降级附件路径最终也会被同一上限拒绝。
    let meta = match regular_file_meta(path) {
        Ok(meta) => meta,
        Err(error) => return error,
    };
    if meta.len() > MAX_MEDIA_FILE_SIZE {
        return ToolResult::err(format!(
            "PDF 文件过大 ({} 字节 > 上限 {MAX_MEDIA_FILE_SIZE})，不读取。",
            meta.len()
        ));
    }
    // 检测（仅元数据，便宜）。
    let info = match pdf_inspector::detect_pdf(path) {
        Ok(info) => info,
        Err(e) => {
            tracing::warn!(path, error = %e, "PDF 检测失败，回退为多模态附件");
            return attach_with_header(
                path,
                kind,
                mime,
                format!("[PDF: 检测失败（{e}） → 已回退为多模态附件]"),
            );
        }
    };

    // 检测阶段纯决策：提取 / 附件 / 编码损坏降级。
    match decide_detect_action(DetectObserved::Detected {
        pdf_type: info.pdf_type,
        has_encoding_issues: info.has_encoding_issues,
    }) {
        DetectAction::AttachWithHeader => {
            return attach_with_header(
                path,
                kind,
                mime,
                attach_header(&info.pdf_type, info.page_count, &info.pages_needing_ocr),
            );
        }
        DetectAction::AttachDegrade => {
            tracing::warn!(
                path,
                page_count = info.page_count,
                "文本型 PDF 编码损坏，回退为多模态附件"
            );
            return attach_with_header(
                path,
                kind,
                mime,
                format!(
                    "[PDF: 文本型 {}页但检测到编码损坏 → 已回退为多模态附件，建议激活 glmocr skill]",
                    info.page_count
                ),
            );
        }
        DetectAction::Extract => {}
    }

    // 文本型：本地提取（提取文本整篇在内存，交给 window_lines_mem 分页）。
    let markdown = match pdf_inspector::process_pdf(path) {
        Ok(res) => res.markdown,
        Err(e) => {
            tracing::warn!(path, page_count = info.page_count, error = %e, "PDF 文本提取失败，回退为多模态附件");
            return attach_with_header(
                path,
                kind,
                mime,
                format!("[PDF: 提取失败（{e}） → 已回退为多模态附件]"),
            );
        }
    };

    // 提取阶段纯决策：有文本 → 分页返回；没文本 → 降级附件。
    match decide_extract_action(ExtractObserved::Done(markdown)) {
        ExtractAction::ReturnText(markdown) => {
            let win = window_lines_mem(&markdown, offset, limit);
            let mut out = format!("[PDF: 文本型 {}页 → 已本地提取为文本]\n", info.page_count);
            out.push_str(&win.content);
            if let Some(note) = pdf_window_note(&win, offset) {
                out.push('\n');
                out.push_str(&note);
            }
            ToolResult::ok(out)
        }
        ExtractAction::AttachDegrade => {
            tracing::warn!(
                path,
                page_count = info.page_count,
                "文本型 PDF 未提取到文本，回退为多模态附件"
            );
            attach_with_header(
                path,
                kind,
                mime,
                format!(
                    "[PDF: 文本型 {}页但未提取到文本 → 已回退为多模态附件]",
                    info.page_count
                ),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdf_inspector::PdfType;

    #[test]
    fn route_pdf_maps_types_to_routes() {
        assert_eq!(route_pdf(&PdfType::TextBased), PdfRoute::Extract);
        assert_eq!(route_pdf(&PdfType::Scanned), PdfRoute::Attach);
        assert_eq!(route_pdf(&PdfType::ImageBased), PdfRoute::Attach);
        assert_eq!(route_pdf(&PdfType::Mixed), PdfRoute::Attach);
    }

    #[test]
    fn mem_window_full_read_has_no_note() {
        let win = window_lines_mem("a\nb\nc", 0, 2000);
        assert_eq!(win.content, "a\nb\nc");
        assert_eq!(win.total_lines, 3);
        assert_eq!(win.lines_returned, 3);
        assert_eq!(pdf_window_note(&win, 0), None);
    }

    #[test]
    fn mem_window_offset_pagination_notes_next_offset() {
        let win = window_lines_mem("l0\nl1\nl2\nl3\nl4", 1, 2);
        assert_eq!(win.content, "l1\nl2");
        assert_eq!(win.total_lines, 5);
        assert_eq!(win.lines_returned, 2);
        let note = pdf_window_note(&win, 1).expect("分页窗口应有提示");
        assert!(note.contains("第 2–3 行"), "实际: {note}");
        // 0 基 offset 的下一段起点 = offset + lines_returned。
        assert!(note.contains("offset=3"), "实际: {note}");
    }

    #[test]
    fn mem_window_offset_out_of_range_notes_total() {
        let win = window_lines_mem("x\ny\nz", 10, 2000);
        assert_eq!(win.content, "");
        assert_eq!(win.total_lines, 3);
        assert_eq!(win.lines_returned, 0);
        let note = pdf_window_note(&win, 10).expect("超界应有提示");
        assert!(note.contains("超出 PDF 文本范围"), "实际: {note}");
        assert!(note.contains("共 3 行"), "实际: {note}");
    }

    #[test]
    fn mem_window_byte_budget_stops_collecting_keeps_counting() {
        let line = "x".repeat(100 * 1024);
        let content = format!("{line}\n{line}\n{line}");
        let win = window_lines_mem(&content, 0, 2000);
        assert_eq!(win.lines_returned, 2);
        assert_eq!(win.total_lines, 3);
        let note = pdf_window_note(&win, 0).expect("字节截断窗口应有提示");
        assert!(note.contains("offset=2"), "实际: {note}");
    }

    #[test]
    fn mem_window_first_line_exempt_from_byte_budget() {
        // 单行超预算：首行豁免，仍完整返回（否则翻页无法推进）。
        let line = "x".repeat(READ_MAX_OUTPUT_BYTES + 1024);
        let win = window_lines_mem(&line, 0, 2000);
        assert_eq!(win.content, line);
        assert_eq!(win.total_lines, 1);
        assert_eq!(win.lines_returned, 1);
        assert_eq!(pdf_window_note(&win, 0), None);
    }

    #[test]
    fn mem_window_limit_truncation_notes_next_offset() {
        let win = window_lines_mem("a\nb\nc\nd", 0, 2);
        assert_eq!(win.content, "a\nb");
        assert_eq!(win.total_lines, 4);
        assert_eq!(win.lines_returned, 2);
        let note = pdf_window_note(&win, 0).expect("limit 截断应有提示");
        assert!(note.contains("仍有后续"), "实际: {note}");
        assert!(note.contains("offset=2"), "实际: {note}");
    }

    #[test]
    fn mem_window_offset_equals_total_lines_folds_into_out_of_range() {
        // offset 恰等于总行数：无行可选（idx < offset 对所有行成立）→ 折叠到超界分支。
        let win = window_lines_mem("x\ny\nz", 3, 2000);
        assert_eq!(win.content, "");
        assert_eq!(win.lines_returned, 0);
        let note = pdf_window_note(&win, 3).expect("offset 恰等于总行数应走超界分支");
        assert!(note.contains("超出 PDF 文本范围"), "实际: {note}");
    }

    #[test]
    fn mem_window_normalizes_crlf_to_lf() {
        // std lines() 剥掉 '\r'，CRLF 输入与 LF 同口径。
        let win = window_lines_mem("a\r\nb", 0, 2000);
        assert_eq!(win.content, "a\nb");
        assert_eq!(win.total_lines, 2);
        assert_eq!(win.lines_returned, 2);
    }

    // ===== attach_header（结果头格式化）=====

    #[test]
    fn attach_header_text_based_without_ocr_pages() {
        let h = attach_header(&PdfType::TextBased, 3, &[]);
        assert_eq!(
            h,
            "[PDF: 文本型 3页 → 已作为多模态附件附给模型；若当前模型不支持视觉或需精确表格/长文档提取，可激活 glmocr skill]"
        );
    }

    #[test]
    fn attach_header_uses_chinese_label_per_type() {
        assert!(attach_header(&PdfType::Scanned, 12, &[]).starts_with("[PDF: 扫描型 12页 → "));
        assert!(attach_header(&PdfType::ImageBased, 1, &[]).starts_with("[PDF: 图像型 1页 → "));
        assert!(attach_header(&PdfType::Mixed, 5, &[]).starts_with("[PDF: 混合型 5页 → "));
    }

    #[test]
    fn attach_header_lists_ocr_pages_when_present() {
        let h = attach_header(&PdfType::Mixed, 10, &[3, 7, 9]);
        assert!(h.contains("；需 OCR 页: 3, 7, 9]"), "实际: {h}");
    }

    #[test]
    fn attach_header_truncates_ocr_page_list_over_twenty() {
        let pages: Vec<u32> = (1..=25).collect();
        let h = attach_header(&PdfType::Mixed, 25, &pages);
        let head =
            "；需 OCR 页: 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20";
        assert!(h.contains(head), "实际: {h}");
        assert!(h.ends_with("...等共 25 页]"), "实际: {h}");
        // 前 20 个之后不得再列页码。
        assert!(!h.contains(" 21,"), "实际: {h}");
    }

    // ===== 降级矩阵（纯决策接缝）=====

    #[test]
    fn degrade_matrix_detect_stage() {
        // TextBased 但编码损坏。
        assert_eq!(
            decide_detect_action(DetectObserved::Detected {
                pdf_type: PdfType::TextBased,
                has_encoding_issues: true,
            }),
            DetectAction::AttachDegrade
        );
        // Scanned / ImageBased / Mixed → 附件路径（带类型头）。
        for t in [PdfType::Scanned, PdfType::ImageBased, PdfType::Mixed] {
            assert_eq!(
                decide_detect_action(DetectObserved::Detected {
                    pdf_type: t,
                    has_encoding_issues: false,
                }),
                DetectAction::AttachWithHeader
            );
        }
        // TextBased 正常 → 本地提取。
        assert_eq!(
            decide_detect_action(DetectObserved::Detected {
                pdf_type: PdfType::TextBased,
                has_encoding_issues: false,
            }),
            DetectAction::Extract
        );
    }

    #[test]
    fn degrade_matrix_extract_stage() {
        // TextBased 但 markdown 空（None 或纯空白）。
        assert_eq!(
            decide_extract_action(ExtractObserved::Done(None)),
            ExtractAction::AttachDegrade
        );
        assert_eq!(
            decide_extract_action(ExtractObserved::Done(Some("  \n".to_string()))),
            ExtractAction::AttachDegrade
        );
        // 提取到文本。
        assert_eq!(
            decide_extract_action(ExtractObserved::Done(Some("正文".to_string()))),
            ExtractAction::ReturnText("正文".to_string())
        );
    }

    // ===== read_pdf 主入口（IO 组装）=====

    #[test]
    fn read_pdf_degrades_corrupt_bytes_to_attachment() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("corrupt.pdf");
        std::fs::write(&path, b"not a pdf").unwrap();
        let path = path.to_string_lossy().replace('\\', "/");
        let r = read_pdf(&path, MediaKind::File, "application/pdf", 0, 2000);
        assert!(!r.is_error, "实际: {}", r.content);
        assert_eq!(r.attachments.len(), 1);
        assert!(r.content.contains("检测失败"), "实际: {}", r.content);
        assert!(r.content.contains("回退"), "实际: {}", r.content);
    }

    // ===== 端到端（经 Read::execute 真实入口；fixture 用 lopdf 现场构造，不提交二进制）=====

    use lopdf::dictionary;
    use serde_json::json;

    /// 测试 base 目录（tempdir + forget，与 files/tests.rs 的 base() 同模式）。
    fn e2e_base() -> PathBuf {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("agent");
        fs::create_dir_all(&p).unwrap();
        std::mem::forget(dir);
        p
    }

    /// 写入任意字节并返回以 / 分隔的绝对路径。
    fn write_bytes(base: &Path, name: &str, bytes: &[u8]) -> String {
        let path = base.join(name);
        fs::write(&path, bytes).unwrap();
        path.to_string_lossy().replace('\\', "/")
    }

    /// 用 lopdf 构造多页 PDF 写入 base 下，返回以 / 分隔的绝对路径。
    ///
    /// `pages` 的每个元素是一页的文本行（每行产出一个 `Tj` 文本算子）。页内凑足
    /// ≥3 个文本算子 + ≥5 个不同字符以满足 pdf-inspector 的 TextBased 判定
    /// （min_text_ops_per_page=3）；行距 30pt 使各文本项分属不同行。
    fn write_pdf(base: &Path, name: &str, pages: &[&[&str]]) -> String {
        let mut doc = lopdf::Document::with_version("1.4");
        let font_id = doc.add_object(lopdf::dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Helvetica",
            "Encoding" => "WinAnsiEncoding",
        });
        // 预留 Pages 对象 id：Page 的 /Parent 必须先于 Pages 对象本身被引用。
        let pages_id = doc.new_object_id();
        let mut kids = Vec::new();
        for lines in pages {
            let mut ops = String::from("BT\n/F1 24 Tf\n72 720 Td\n");
            for (i, line) in lines.iter().enumerate() {
                if i > 0 {
                    ops.push_str("0 -30 Td\n");
                }
                ops.push_str(&format!("({line}) Tj\n"));
            }
            ops.push_str("ET\n");
            let content_id =
                doc.add_object(lopdf::Stream::new(lopdf::dictionary! {}, ops.into_bytes()));
            let page_id = doc.add_object(lopdf::dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
                "Resources" => lopdf::dictionary! {
                    "Font" => lopdf::dictionary! { "F1" => font_id },
                },
            });
            kids.push(lopdf::Object::Reference(page_id));
        }
        doc.set_object(
            pages_id,
            lopdf::dictionary! {
                "Type" => "Pages",
                "Kids" => kids,
                "Count" => pages.len() as i64,
            },
        );
        let catalog_id = doc.add_object(lopdf::dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
        });
        doc.trailer.set("Root", catalog_id);
        doc.max_id = doc.objects.len() as u32;
        let path = base.join(name);
        doc.save(&path).expect("保存 lopdf 构造的 PDF fixture");
        path.to_string_lossy().replace('\\', "/")
    }

    #[tokio::test]
    async fn read_routes_text_pdf_to_local_extraction() {
        let b = e2e_base();
        let tool = Read::new(b.clone());
        let path = write_pdf(
            &b,
            "text.pdf",
            &[&["Hello RClaw PDF", "second line here", "third line"]],
        );
        let r = tool.execute(json!({"path": path})).await;
        assert!(!r.is_error, "实际: {}", r.content);
        assert!(r.content.starts_with("[PDF: 文本型"), "实际: {}", r.content);
        assert!(r.content.contains("Hello RClaw PDF"), "实际: {}", r.content);
        assert!(r.attachments.is_empty(), "实际: {}", r.content);
    }

    #[tokio::test]
    async fn read_pdf_text_supports_offset_limit_pagination() {
        let b = e2e_base();
        let tool = Read::new(b.clone());
        let pages: &[&[&str]] = &[
            &["p1 one", "p1 two", "p1 three", "p1 four"],
            &["p2 one", "p2 two", "p2 three", "p2 four"],
            &["p3 one", "p3 two", "p3 three", "p3 four"],
        ];
        let path = write_pdf(&b, "multi.pdf", pages);
        let r = tool
            .execute(json!({"path": path, "offset": 1, "limit": 1}))
            .await;
        assert!(!r.is_error, "实际: {}", r.content);
        // 弱断言：只要锚定「翻页提示存在且给出下一页 offset」（页边界保证总行数 ≥ 3，
        // offset=1/limit=1 必有后续），不锚定提取出的具体行数。
        assert!(r.content.contains("offset="), "实际: {}", r.content);
    }

    #[tokio::test]
    async fn read_routes_corrupt_pdf_to_attachment_with_fallback_note() {
        let b = e2e_base();
        let tool = Read::new(b.clone());
        let path = write_bytes(&b, "corrupt.pdf", b"not a pdf");
        let r = tool.execute(json!({"path": path})).await;
        assert!(!r.is_error, "实际: {}", r.content);
        assert_eq!(r.attachments.len(), 1);
        assert!(r.content.contains("回退"), "实际: {}", r.content);
    }

    #[tokio::test]
    async fn read_routes_empty_content_stream_pdf_to_attachment() {
        let b = e2e_base();
        let tool = Read::new(b.clone());
        // 空内容流页面（无任何文本算子）：不断言具体类型——分类器可能判 Scanned 或
        // ImageBased，只锚定「走附件分支」。
        let path = write_pdf(&b, "empty.pdf", &[&[]]);
        let r = tool.execute(json!({"path": path})).await;
        assert!(!r.attachments.is_empty(), "实际: {}", r.content);
        assert!(r.content.contains("附件"), "实际: {}", r.content);
    }

    #[tokio::test]
    async fn read_non_pdf_media_still_uses_attachment_path() {
        let b = e2e_base();
        let tool = Read::new(b.clone());
        let path = write_bytes(&b, "x.png", b"\x89PNG\r\n\x1a\nnot really a png");
        let r = tool.execute(json!({"path": path})).await;
        assert!(!r.is_error, "实际: {}", r.content);
        assert_eq!(r.attachments.len(), 1);
    }

    #[tokio::test]
    async fn read_media_path_rejects_invalid_offset_as_param_error() {
        // I1 注记锚定：offset/limit 解析在媒体分支之前（ADR-0003 的既定副作用），
        // 无效 offset 在媒体路径上报参数错误，而非被媒体分支静默忽略。
        let b = e2e_base();
        let tool = Read::new(b.clone());
        let path = write_bytes(&b, "x.png", b"\x89PNG\r\n\x1a\nnot really a png");
        let r = tool
            .execute(json!({"path": path, "offset": "not-a-number"}))
            .await;
        assert!(r.is_error, "实际: {}", r.content);
        assert!(r.content.contains("offset"), "实际: {}", r.content);
        // 未走到附件分支：不产生附件。
        assert!(r.attachments.is_empty(), "实际: {}", r.content);
    }

    #[tokio::test]
    async fn read_pdf_branch_does_not_emit_read_metadata() {
        let b = e2e_base();
        let tool = Read::new(b.clone());
        let path = write_pdf(
            &b,
            "meta.pdf",
            &[&["Hello RClaw PDF", "second line here", "third line"]],
        );
        let r = tool
            .execute(json!({"path": path, "include_metadata": true}))
            .await;
        assert!(!r.is_error, "实际: {}", r.content);
        // PDF 分支不走文本读取元数据（sha256/bytes），也不报参数错误。
        assert!(
            !r.content.contains("[read-metadata:"),
            "实际: {}",
            r.content
        );
        assert!(r.content.starts_with("[PDF: 文本型"), "实际: {}", r.content);
    }
}
