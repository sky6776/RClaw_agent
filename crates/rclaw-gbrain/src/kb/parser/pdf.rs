//! PDF parser with page-level extraction, header/footer cleaning, and OCR tagging
//!
//! P2-004: Outputs page_number metadata per block
//! P2-005: Heuristic header/footer removal
//! P2-006: Text density detection → needs_ocr flag
//! Phase 1: 增强页级分析 — 图片检测、矢量对象、完整 OCR metadata

use super::{DocumentParser, ParsedDocument};
use crate::error::GBrainError;
use std::collections::HashMap;

pub struct PdfParser;

/// PDF 对象允许的最大嵌套深度（数组 `[` / 字典 `<<`）。
/// lopdf 0.34 的 `array()`/`dictionary()` 组合子无深度上限地递归，5000 层嵌套（10 KB 文件）
/// 就能在 2 MiB 的 worker 线程栈上溢出——栈溢出不是 panic，catch_unwind 拦不住，整个
/// APK 进程被 SIGSEGV 杀掉，且重试同一文档会再次崩溃。所以在任何 `load_mem` 之前先做
/// 一次与解析器无关的线性预扫描。
const MAX_PDF_OBJECT_NESTING: usize = 256;

/// 单个 PDF 允许解析的最大页数：逐页 `extract_text` 会让 lopdf 每次重建整棵页表，O(P²)，
/// 几 MB 的空白页 PDF 就能塞下 10 万页，把唯一的 KB worker 占住几小时。
const MAX_PDF_PAGES: usize = 5000;

/// 单个流（任意一级过滤器）解码后允许的最大字节数。lopdf 的 `decompressed_content()` 对
/// FlateDecode / LZWDecode 完全不设上限：1 MB 的 zlib 炸弹可膨胀到 1 GB，它既在 `load_mem`
/// 时解压对象流，也在逐页提取文本时解压内容流，Android 上直接被 LMK 杀掉。
const MAX_PDF_STREAM_INFLATED_BYTES: usize = 64 * 1024 * 1024;
/// 对象流（/Type /ObjStm）解压后会被 lopdf 常驻内存并解析成对象树，总量另设上限。
const MAX_PDF_OBJSTM_INFLATED_TOTAL_BYTES: usize = 64 * 1024 * 1024;
/// 预扫描阶段全部解压工作量的上限：取文件大小 16 倍与 256 MiB 的较大者，防止大量中等
/// 体积的流把唯一的 KB worker 占住。
const MAX_PDF_INFLATED_TOTAL_FLOOR_BYTES: usize = 256 * 1024 * 1024;
const MAX_PDF_INFLATED_TOTAL_RATIO: usize = 16;
/// PNG 预测器单行字节数上限：lopdf 按 Columns × 每像素字节数 `vec![0; n]` 分配两行缓冲，
/// 伪造的 /Columns 会让它直接申请 TB 级内存——分配失败是 abort，catch_unwind 拦不住。
const MAX_PDF_PREDICTOR_ROW_BYTES: usize = 1024 * 1024;
/// 第二遍补扫（从被字符串 / 注释 / 流体遮住的 `N G obj` 头开始）允许的总扫描字节数倍率。
const MAX_PDF_HIDDEN_OBJECT_SCAN_RATIO: usize = 4;

fn is_pdf_whitespace(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | b'\0' | b'\x0C')
}

fn is_pdf_delimiter(b: u8) -> bool {
    matches!(
        b,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%'
    )
}

fn is_pdf_regular(b: u8) -> bool {
    !is_pdf_whitespace(b) && !is_pdf_delimiter(b)
}

fn is_pdf_number(tok: &[u8]) -> bool {
    !tok.is_empty()
        && tok
            .iter()
            .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.'))
}

/// 跳过字面量字符串 `(...)`（处理转义与括号配对），返回字符串之后的位置
fn skip_pdf_literal_string(data: &[u8], end: usize, mut i: usize) -> usize {
    let mut paren = 1usize;
    i += 1;
    while i < end && paren > 0 {
        match data[i] {
            b'\\' => i += 1,
            b'(' => paren += 1,
            b')' => paren -= 1,
            _ => {}
        }
        i += 1;
    }
    i.min(end)
}

/// 跳过 `%` 注释直到行尾（不含 EOL）
fn skip_pdf_comment(data: &[u8], end: usize, mut i: usize) -> usize {
    while i < end && data[i] != b'\n' && data[i] != b'\r' {
        i += 1;
    }
    i
}

/// 跳过 `(空白 | 注释)*`，与 lopdf 的 `space` 组合子一致
fn skip_pdf_space(data: &[u8], end: usize, mut i: usize) -> usize {
    loop {
        while i < end && is_pdf_whitespace(data[i]) {
            i += 1;
        }
        if i < end && data[i] == b'%' {
            i = skip_pdf_comment(data, end, i);
        } else {
            return i;
        }
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn pdf_nesting_error() -> GBrainError {
    GBrainError::InvalidInput(format!(
        "PDF 对象嵌套深度超过上限 {MAX_PDF_OBJECT_NESTING}，拒绝解析"
    ))
}

fn pdf_inflate_error() -> GBrainError {
    GBrainError::InvalidInput(
        "PDF 流解压后体积超过上限（疑似解压炸弹），拒绝解析".to_string(),
    )
}

/// 预扫描的共享状态：解压预算与已经以 token 形式扫描到的 `obj` 关键字位置
struct PdfPrescan {
    inflate_remaining: usize,
    objstm_remaining: usize,
    seen_obj_keywords: std::collections::HashSet<usize>,
}

impl PdfPrescan {
    fn new(file_len: usize) -> Self {
        Self {
            inflate_remaining: MAX_PDF_INFLATED_TOTAL_FLOOR_BYTES
                .max(file_len.saturating_mul(MAX_PDF_INFLATED_TOTAL_RATIO)),
            objstm_remaining: MAX_PDF_OBJSTM_INFLATED_TOTAL_BYTES,
            seen_obj_keywords: std::collections::HashSet::new(),
        }
    }

    fn consume_inflate(&mut self, bytes: usize) -> Result<(), GBrainError> {
        self.inflate_remaining = self
            .inflate_remaining
            .checked_sub(bytes)
            .ok_or_else(pdf_inflate_error)?;
        Ok(())
    }

    fn consume_objstm(&mut self, bytes: usize) -> Result<(), GBrainError> {
        self.objstm_remaining = self
            .objstm_remaining
            .checked_sub(bytes)
            .ok_or_else(pdf_inflate_error)?;
        Ok(())
    }
}

/// 预扫描：在任何 `load_mem` 之前，按 lopdf 的解析 / 解码路径检查
/// 1. 对象嵌套深度（文件级语法、解码后的对象流与内容流）——防栈溢出；
/// 2. 流解压后的体积——防解压炸弹；
/// 3. PNG 预测器的行宽——防巨量分配 abort。
/// 这是与解析器无关的线性扫描，lopdf 本身没有任何上限。
pub(crate) fn check_pdf_nesting_depth(data: &[u8]) -> Result<(), GBrainError> {
    let mut ctx = PdfPrescan::new(data.len());
    scan_pdf_syntax(data, 0, data.len(), Some(&mut ctx), false)?;

    // 第二遍：lopdf 按 xref 偏移直接定位对象，偏移可以指向被字符串 / 注释 / 流体遮住的
    // 位置（第一遍把那些区域整体跳过了）。对第一遍没有以 token 形式经过的 `N G obj` 头
    // 逐个补扫，补扫在遇到已扫过的对象头时停下，正常文件没有这种头、零开销。
    let mut budget = data
        .len()
        .saturating_mul(MAX_PDF_HIDDEN_OBJECT_SCAN_RATIO)
        .saturating_add(1024 * 1024);
    for header in find_pdf_object_headers(data)? {
        if !ctx.seen_obj_keywords.insert(header) {
            continue;
        }
        let stop = scan_pdf_syntax(data, header + 3, data.len(), Some(&mut ctx), true)?;
        budget = budget.checked_sub(stop - header).ok_or_else(|| {
            GBrainError::InvalidInput("PDF 结构异常（大量被遮蔽的对象头），拒绝解析".to_string())
        })?;
    }
    Ok(())
}

/// 线性扫描 `data[start..end]` 的 PDF 对象语法，估算 `[` / `<<` 的最大嵌套深度。
/// 与 lopdf 的递归行为对齐：遇到非法 token（孤立定界符、未知关键字、二进制字节组成的
/// "关键字"等）时整条递归链解析失败并回退，深度从零重新累计——这同时避免压缩数据、
/// 字体等二进制内容里随机出现的 `[` 造成误判。
/// `ctx` 为 Some 时是文件级扫描：识别 `stream` 关键字并对流体做解压 / 嵌套检查、记录
/// `obj` 关键字位置；`stop_at_seen_obj` 为 true 时遇到已记录的 `obj` 头即返回其位置。
fn scan_pdf_syntax(
    data: &[u8],
    start: usize,
    end: usize,
    mut ctx: Option<&mut PdfPrescan>,
    stop_at_seen_obj: bool,
) -> Result<usize, GBrainError> {
    let mut depth: usize = 0;
    let mut i = start;
    // 当前顶层字典 `<<` 的位置，以及最近一个在顶层闭合的字典 [start, end)
    let mut dict_open_at: Option<usize> = None;
    let mut last_top_dict: Option<(usize, usize)> = None;
    while i < end {
        let b = data[i];
        if is_pdf_whitespace(b) {
            i += 1;
            continue;
        }
        let mut invalid = false;
        match b {
            b'%' => {
                i = skip_pdf_comment(data, end, i);
                continue;
            }
            b'(' => {
                i = skip_pdf_literal_string(data, end, i);
                continue;
            }
            b'[' => {
                depth += 1;
                i += 1;
            }
            b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            b'<' if i + 1 < end && data[i + 1] == b'<' => {
                if depth == 0 {
                    dict_open_at = Some(i);
                }
                depth += 1;
                i += 2;
            }
            b'<' => {
                // 十六进制字符串：只允许十六进制数字与空白，否则 lopdf 解析失败
                let mut j = i + 1;
                while j < end && (data[j].is_ascii_hexdigit() || is_pdf_whitespace(data[j])) {
                    j += 1;
                }
                if j < end && data[j] == b'>' {
                    i = j + 1;
                } else {
                    invalid = true;
                    i += 1;
                }
            }
            b'>' if i + 1 < end && data[i + 1] == b'>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    if let Some(open) = dict_open_at.take() {
                        last_top_dict = Some((open, i + 2));
                    }
                }
                i += 2;
            }
            b'/' => {
                // 名字
                i += 1;
                while i < end && is_pdf_regular(data[i]) {
                    i += 1;
                }
            }
            b')' | b'>' | b'{' | b'}' => {
                invalid = true;
                i += 1;
            }
            _ => {
                let tok_start = i;
                while i < end && is_pdf_regular(data[i]) {
                    i += 1;
                }
                let tok = &data[tok_start..i];
                if is_pdf_number(tok) || matches!(tok, b"true" | b"false" | b"null" | b"R") {
                    // 合法对象 / 引用后缀，不影响深度
                } else if tok == b"obj" {
                    if let Some(ctx) = ctx.as_deref_mut() {
                        if !ctx.seen_obj_keywords.insert(tok_start) && stop_at_seen_obj {
                            return Ok(tok_start);
                        }
                    }
                    invalid = true;
                } else if tok == b"stream" {
                    if let Some(ctx) = ctx.as_deref_mut() {
                        if let Some(resume) =
                            prescan_pdf_stream(data, end, last_top_dict, tok_start, i, ctx)?
                        {
                            i = resume;
                            depth = 0;
                            dict_open_at = None;
                            last_top_dict = None;
                            continue;
                        }
                    }
                    invalid = true;
                } else {
                    invalid = true;
                }
            }
        }
        if invalid {
            depth = 0;
            dict_open_at = None;
        }
        if depth > MAX_PDF_OBJECT_NESTING {
            return Err(pdf_nesting_error());
        }
    }
    Ok(end)
}

/// 文件级扫描遇到 `stream` 关键字：若它确实构成 lopdf 眼中的流对象（前面紧跟刚闭合的
/// 字典、关键字后是 EOL），按 lopdf 的解码路径检查流体并返回 `endstream` 之后的续扫
/// 位置；否则返回 None，由调用方按普通 token 处理（流体照常参与语法扫描）。
fn prescan_pdf_stream(
    data: &[u8],
    end: usize,
    last_top_dict: Option<(usize, usize)>,
    kw_start: usize,
    kw_end: usize,
    ctx: &mut PdfPrescan,
) -> Result<Option<usize>, GBrainError> {
    let Some((dict_start, dict_end)) = last_top_dict else {
        return Ok(None);
    };
    if skip_pdf_space(data, kw_start, dict_end) != kw_start {
        return Ok(None);
    }
    let body_start = if data[kw_end..end].starts_with(b"\r\n") {
        kw_end + 2
    } else if kw_end < end && (data[kw_end] == b'\n' || data[kw_end] == b'\r') {
        kw_end + 1
    } else {
        return Ok(None);
    };
    let info = parse_pdf_stream_dict(&data[dict_start..dict_end]);

    // 流体终点：优先采用直接给出且与 endstream 吻合的 /Length（lopdf 的取法），否则取第一个
    // endstream。解压阶段不依赖这个终点（见 check_pdf_stream_body），这里只决定续扫位置。
    let by_length = info
        .length
        .filter(|&len| len <= end - body_start)
        .and_then(|len| endstream_after(data, end, body_start + len).map(|resume| (body_start + len, resume)));
    let (body_end, resume) = match by_length {
        Some(found) => found,
        None => match find_subslice(&data[body_start..end], b"endstream") {
            Some(off) => {
                let mut body_end = body_start + off;
                if body_end > body_start && data[body_end - 1] == b'\n' {
                    body_end -= 1;
                }
                if body_end > body_start && data[body_end - 1] == b'\r' {
                    body_end -= 1;
                }
                (body_end, body_start + off + b"endstream".len())
            }
            None => (end, end),
        },
    };
    check_pdf_stream_body(data, body_start, body_end, end, &info, ctx)?;
    Ok(Some(resume))
}

/// `pos` 处（允许先有一个 EOL）是否紧跟 `endstream`；是则返回其后的位置
fn endstream_after(data: &[u8], end: usize, mut pos: usize) -> Option<usize> {
    if data[pos..end].starts_with(b"\r\n") {
        pos += 2;
    } else if pos < end && (data[pos] == b'\n' || data[pos] == b'\r') {
        pos += 1;
    }
    data[pos..end]
        .starts_with(b"endstream")
        .then_some(pos + b"endstream".len())
}

/// 按 lopdf 的解码路径检查一个流体：
/// - lopdf 会把**原始字节**当对象语法 / 内容流解析的情况：没有 /Filter、/Filter 无法解析或
///   含不支持的过滤器（`decompressed_content` 报错后 `get_page_content` 回退原始字节、对象流
///   `decompress()` 原地放弃）、/Subtype /Image（同样直接报错回退）、带 PNG 预测器（预测器解码
///   失败时回退）——这些都要对原始字节做嵌套检查；
/// - 过滤器链 lopdf 能解码时，逐级限额解码并对解码结果做嵌套检查。
fn check_pdf_stream_body(
    data: &[u8],
    body_start: usize,
    body_end: usize,
    file_end: usize,
    info: &PdfStreamDict,
    ctx: &mut PdfPrescan,
) -> Result<(), GBrainError> {
    let decodable = info.filters_valid
        && !info.filters.is_empty()
        && info.filters.iter().all(|f| {
            matches!(f.as_slice(), b"FlateDecode" | b"LZWDecode" | b"ASCII85Decode")
        })
        && !info.is_image;
    if !decodable || info.has_png_predictor() {
        scan_pdf_syntax(data, body_start, body_end, None, false)?;
    }
    if !decodable {
        return Ok(());
    }
    if info.has_png_predictor() && info.predictor_row_bytes() > MAX_PDF_PREDICTOR_ROW_BYTES {
        return Err(GBrainError::InvalidInput(
            "PDF 流的 PNG 预测器行宽超过上限（疑似恶意 /Columns），拒绝解析".to_string(),
        ));
    }

    // 第一级过滤器直接从流体起点读到文件尾：zlib / LZW 自带终止标记，而 /Length 为间接引用时
    // 无法可靠定位流尾，攻击者还可能在压缩数据里伪造 endstream 让扫描提前停下。
    let mut current: Option<Vec<u8>> = None;
    for filter in &info.filters {
        let decoded = match (filter.as_slice(), &current) {
            (b"FlateDecode", Some(prev)) => inflate_zlib_capped(prev)?,
            (b"FlateDecode", None) => inflate_zlib_capped(&data[body_start..file_end])?,
            (b"LZWDecode", Some(prev)) => inflate_lzw_capped(prev, info.early_change)?,
            (b"LZWDecode", None) => inflate_lzw_capped(&data[body_start..file_end], info.early_change)?,
            (_, Some(prev)) => decode_ascii85_capped(prev)?,
            // ASCII85 以 `~>` 结束，lopdf 的解码器遇到任何非法字符也会停下
            (_, None) => decode_ascii85_capped(&data[body_start..file_end])?,
        };
        let decoded = if filter.as_slice() != b"ASCII85Decode" && info.has_png_predictor() {
            // 与 lopdf 相同的预测器参数；解码失败时 lopdf 回退原始字节，上面已检查过
            match lopdf::filters::png::decode_frame(
                &decoded,
                info.predictor_bytes_per_pixel(),
                info.predictor_pixels_per_row(),
            ) {
                Ok(unpredicted) => unpredicted,
                Err(_) => return Ok(()),
            }
        } else {
            decoded
        };
        ctx.consume_inflate(decoded.len())?;
        current = Some(decoded);
    }
    let decoded = current.unwrap_or_default();
    if info.is_objstm {
        ctx.consume_objstm(decoded.len())?;
    }
    scan_pdf_syntax(&decoded, 0, decoded.len(), None, false)?;
    Ok(())
}

/// 读取至多 cap + 1 字节；超过上限视为解压炸弹。解码错误与 lopdf 一样保留部分输出。
fn read_capped(reader: impl std::io::Read) -> Result<Vec<u8>, GBrainError> {
    use std::io::Read as _;
    let mut out = Vec::new();
    let _ = reader
        .take(MAX_PDF_STREAM_INFLATED_BYTES as u64 + 1)
        .read_to_end(&mut out);
    if out.len() > MAX_PDF_STREAM_INFLATED_BYTES {
        return Err(pdf_inflate_error());
    }
    Ok(out)
}

fn inflate_zlib_capped(input: &[u8]) -> Result<Vec<u8>, GBrainError> {
    read_capped(flate2::read::ZlibDecoder::new(input))
}

#[derive(Default)]
struct CappedSink {
    buf: Vec<u8>,
    overflow: bool,
}

impl std::io::Write for CappedSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > MAX_PDF_STREAM_INFLATED_BYTES {
            self.overflow = true;
            return Err(std::io::Error::other("PDF 流解压超过上限"));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn inflate_lzw_capped(input: &[u8], early_change: bool) -> Result<Vec<u8>, GBrainError> {
    use weezl::{decode::Decoder, BitOrder};
    // 与 lopdf `decompress_lzw` 相同的参数（MIN_BITS 9 - 1）
    let mut decoder = if early_change {
        Decoder::with_tiff_size_switch(BitOrder::Msb, 8)
    } else {
        Decoder::new(BitOrder::Msb, 8)
    };
    let mut sink = CappedSink::default();
    let _ = decoder.into_stream(&mut sink).decode_all(input);
    if sink.overflow {
        return Err(pdf_inflate_error());
    }
    Ok(sink.buf)
}

/// 与 lopdf `decode_ascii85` 相同的解码规则（遇到首个非法字符停止，`z` 展开为 4 个零字节）
fn decode_ascii85_capped(input: &[u8]) -> Result<Vec<u8>, GBrainError> {
    let mut output = Vec::new();
    let mut buffer: u32 = 0;
    let mut count = 0usize;
    for &ch in input {
        if ch == b'z' && count == 0 {
            output.extend_from_slice(&[0, 0, 0, 0]);
        } else if ch.is_ascii_whitespace() {
            continue;
        } else if !(b'!'..=b'u').contains(&ch) {
            break;
        } else {
            buffer = buffer.wrapping_mul(85).wrapping_add(u32::from(ch - b'!'));
            count += 1;
            if count == 5 {
                output.extend_from_slice(&buffer.to_be_bytes());
                buffer = 0;
                count = 0;
            }
        }
        if output.len() > MAX_PDF_STREAM_INFLATED_BYTES {
            return Err(pdf_inflate_error());
        }
    }
    if count > 0 {
        for _ in count..5 {
            buffer = buffer.wrapping_mul(85).wrapping_add(84);
        }
        let bytes = buffer.to_be_bytes();
        output.extend_from_slice(&bytes[..count - 1]);
    }
    Ok(output)
}

/// 流字典里与解码路径相关的字段（按 lopdf 的取法：同名键后者覆盖前者、只看顶层键）
struct PdfStreamDict {
    is_objstm: bool,
    is_image: bool,
    /// /Filter 能否被 lopdf 的 `filters()` 解析（单个名字或名字数组）；否则 lopdf 不解码
    filters_valid: bool,
    filters: Vec<Vec<u8>>,
    /// 直接给出的 /Length（间接引用为 None）
    length: Option<usize>,
    predictor: i64,
    columns: i64,
    colors: i64,
    bits_per_component: i64,
    early_change: bool,
}

impl Default for PdfStreamDict {
    fn default() -> Self {
        Self {
            is_objstm: false,
            is_image: false,
            filters_valid: false,
            filters: Vec::new(),
            length: None,
            predictor: 1,
            columns: 1,
            colors: 1,
            bits_per_component: 8,
            early_change: true,
        }
    }
}

impl PdfStreamDict {
    fn has_png_predictor(&self) -> bool {
        (10..=15).contains(&self.predictor)
    }

    /// 与 lopdf `decompress_predictor` 相同的算法
    fn predictor_bytes_per_pixel(&self) -> usize {
        let colors = self.colors.max(1) as usize;
        let bits = self.bits_per_component.max(8) as usize;
        colors.saturating_mul(bits) / 8
    }

    fn predictor_pixels_per_row(&self) -> usize {
        self.columns.max(1) as usize
    }

    fn predictor_row_bytes(&self) -> usize {
        self.predictor_bytes_per_pixel()
            .saturating_mul(self.predictor_pixels_per_row())
    }

    fn reset_decode_parms(&mut self) {
        self.predictor = 1;
        self.columns = 1;
        self.colors = 1;
        self.bits_per_component = 8;
        self.early_change = true;
    }
}

enum DictTok {
    Name(Vec<u8>),
    Int(i64),
    /// 引用后缀 `R`
    Ref,
    DictOpen,
    DictClose,
    ArrOpen,
    ArrClose,
    Other,
}

fn hex_digit_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn next_dict_tok(text: &[u8], i: &mut usize) -> Option<DictTok> {
    let n = text.len();
    *i = skip_pdf_space(text, n, *i);
    if *i >= n {
        return None;
    }
    let b = text[*i];
    let tok = match b {
        b'<' if *i + 1 < n && text[*i + 1] == b'<' => {
            *i += 2;
            DictTok::DictOpen
        }
        b'>' if *i + 1 < n && text[*i + 1] == b'>' => {
            *i += 2;
            DictTok::DictClose
        }
        b'[' => {
            *i += 1;
            DictTok::ArrOpen
        }
        b']' => {
            *i += 1;
            DictTok::ArrClose
        }
        b'(' => {
            *i = skip_pdf_literal_string(text, n, *i);
            DictTok::Other
        }
        b'<' => {
            let mut j = *i + 1;
            while j < n && text[j] != b'>' {
                j += 1;
            }
            *i = (j + 1).min(n);
            DictTok::Other
        }
        b'/' => {
            // 名字：解码 `#xx` 转义，lopdf 比较的是解码后的字节
            let mut j = *i + 1;
            let mut name = Vec::new();
            while j < n && is_pdf_regular(text[j]) {
                if text[j] == b'#' && j + 2 < n {
                    if let (Some(hi), Some(lo)) = (hex_digit_value(text[j + 1]), hex_digit_value(text[j + 2])) {
                        name.push(hi * 16 + lo);
                        j += 3;
                        continue;
                    }
                }
                name.push(text[j]);
                j += 1;
            }
            *i = j;
            DictTok::Name(name)
        }
        b')' | b'>' | b'{' | b'}' => {
            *i += 1;
            DictTok::Other
        }
        _ => {
            let s = *i;
            while *i < n && is_pdf_regular(text[*i]) {
                *i += 1;
            }
            let tok = &text[s..*i];
            match std::str::from_utf8(tok).ok().and_then(|t| t.parse::<i64>().ok()) {
                Some(v) => DictTok::Int(v),
                None if tok == b"R" => DictTok::Ref,
                None => DictTok::Other,
            }
        }
    };
    Some(tok)
}

/// 跳过一个已读入开符号的数组 / 字典直到匹配的闭符号
fn skip_dict_composite(text: &[u8], i: &mut usize) {
    let mut depth = 1usize;
    while depth > 0 {
        match next_dict_tok(text, i) {
            Some(DictTok::ArrOpen) | Some(DictTok::DictOpen) => depth += 1,
            Some(DictTok::ArrClose) | Some(DictTok::DictClose) => depth -= 1,
            Some(_) => {}
            None => return,
        }
    }
}

/// 读取一个值的首 token 之后：若它是整数且后面跟着 `整数 R`，把引用的剩余两个 token 吃掉
fn consume_reference_tail(text: &[u8], i: &mut usize, first: &DictTok) -> bool {
    if !matches!(first, DictTok::Int(_)) {
        return false;
    }
    let save = *i;
    if matches!(next_dict_tok(text, i), Some(DictTok::Int(_)))
        && matches!(next_dict_tok(text, i), Some(DictTok::Ref))
    {
        return true;
    }
    *i = save;
    false
}

fn parse_pdf_stream_dict(text: &[u8]) -> PdfStreamDict {
    let mut info = PdfStreamDict::default();
    let mut i = 0usize;
    if !matches!(next_dict_tok(text, &mut i), Some(DictTok::DictOpen)) {
        return info;
    }
    loop {
        let key = match next_dict_tok(text, &mut i) {
            Some(DictTok::Name(key)) => key,
            Some(DictTok::DictClose) | None => break,
            // 键位置出现非名字 token（如引用值的残余）：跳过
            Some(_) => continue,
        };
        let Some(value) = next_dict_tok(text, &mut i) else {
            break;
        };
        let is_reference = consume_reference_tail(text, &mut i, &value);
        match key.as_slice() {
            b"Type" => info.is_objstm = matches!(&value, DictTok::Name(n) if n == b"ObjStm"),
            b"Subtype" => info.is_image = matches!(&value, DictTok::Name(n) if n == b"Image"),
            b"Filter" => {
                info.filters.clear();
                match value {
                    DictTok::Name(name) => {
                        info.filters.push(name);
                        info.filters_valid = true;
                    }
                    DictTok::ArrOpen => {
                        // 数组里出现任何非名字元素，lopdf 的 filters() 整体报错
                        info.filters_valid = true;
                        loop {
                            match next_dict_tok(text, &mut i) {
                                Some(DictTok::Name(name)) => info.filters.push(name),
                                Some(DictTok::ArrClose) | None => break,
                                Some(DictTok::ArrOpen) | Some(DictTok::DictOpen) => {
                                    info.filters_valid = false;
                                    skip_dict_composite(text, &mut i);
                                }
                                Some(_) => info.filters_valid = false,
                            }
                        }
                    }
                    DictTok::DictOpen => {
                        info.filters_valid = false;
                        skip_dict_composite(text, &mut i);
                    }
                    _ => info.filters_valid = false,
                }
            }
            b"Length" => {
                info.length = match value {
                    DictTok::Int(v) if !is_reference => usize::try_from(v).ok(),
                    DictTok::ArrOpen | DictTok::DictOpen => {
                        skip_dict_composite(text, &mut i);
                        None
                    }
                    _ => None,
                };
            }
            b"DecodeParms" => {
                info.reset_decode_parms();
                match value {
                    DictTok::DictOpen => loop {
                        let parm = match next_dict_tok(text, &mut i) {
                            Some(DictTok::Name(parm)) => parm,
                            Some(DictTok::DictClose) | None => break,
                            Some(DictTok::ArrOpen) | Some(DictTok::DictOpen) => {
                                skip_dict_composite(text, &mut i);
                                continue;
                            }
                            Some(_) => continue,
                        };
                        let Some(parm_value) = next_dict_tok(text, &mut i) else {
                            break;
                        };
                        let parm_is_ref = consume_reference_tail(text, &mut i, &parm_value);
                        match parm_value {
                            // 引用值 lopdf 的 as_i64 取不到，保持默认
                            DictTok::Int(v) if !parm_is_ref => match parm.as_slice() {
                                b"Predictor" => info.predictor = v,
                                b"Columns" => info.columns = v,
                                b"Colors" => info.colors = v,
                                b"BitsPerComponent" => info.bits_per_component = v,
                                b"EarlyChange" => info.early_change = v != 0,
                                _ => {}
                            },
                            DictTok::ArrOpen | DictTok::DictOpen => skip_dict_composite(text, &mut i),
                            _ => {}
                        }
                    },
                    // 数组 / 引用：lopdf 的 as_dict 失败，等同于没有参数
                    DictTok::ArrOpen => skip_dict_composite(text, &mut i),
                    _ => {}
                }
            }
            _ => {
                if matches!(value, DictTok::ArrOpen | DictTok::DictOpen) {
                    skip_dict_composite(text, &mut i);
                }
            }
        }
    }
    info
}

/// 在整个文件里按字节模式找出所有 `N G obj` 对象头（返回 `obj` 的位置），不理会字符串 /
/// 注释 / 流体边界——lopdf 从 xref 偏移解析对象时同样不理会这些边界。
fn find_pdf_object_headers(data: &[u8]) -> Result<Vec<usize>, GBrainError> {
    let mut headers = Vec::new();
    // 回溯扫描的总工作量预算。每个候选的回溯区间都止于上一个候选所在行，区间互不重叠，
    // 正常文件远用不完；这里只作为兜底，防止再出现"每个 obj 回看 4 KiB"一类的线性大常数。
    let budget = data.len().saturating_mul(8).saturating_add(1024 * 1024);
    let mut work = 0usize;
    let mut i = 0usize;
    while let Some(off) = find_subslice(&data[i..], b"obj") {
        let pos = i + off;
        i = pos + 3;
        if pos + 3 < data.len() && is_pdf_regular(data[pos + 3]) {
            continue;
        }
        if has_object_id_before(data, pos, &mut work) {
            headers.push(pos);
        }
        if work > budget {
            return Err(GBrainError::InvalidInput(
                "PDF 结构异常（对象头回溯扫描超出预算），拒绝解析".to_string(),
            ));
        }
    }
    Ok(headers)
}

/// `obj` 之前是否为 `整数 (空白|注释)* 整数 (空白|注释)*`；`work` 累计回溯扫描过的字节数
/// lopdf 从 xref 偏移开始解析、不理会前文，所以这里的"整数"可以是任意位置的数字串；
/// `space` 里的注释从本行任意一个"前方仅空白 + 数字（或行首）"的 `%` 开始都合法，
/// 要逐个回溯尝试（每行最多一个分支能延续到上一行，总工作量仍与扫过的行长线性）。
/// 回溯触及行长 / 注释链上限时按"是对象头"处理：宁可多补扫一段，也不能漏掉。
fn has_object_id_before(data: &[u8], obj_pos: usize, work: &mut usize) -> bool {
    space_then_uint_before(data, obj_pos, 2, MAX_PDF_COMMENT_HOPS, work)
}

/// 向前回溯时一条 `space` 链里最多经过的注释行数
const MAX_PDF_COMMENT_HOPS: usize = 16;
/// 向前回溯时单行最多回看的字节数
const MAX_PDF_COMMENT_LINE_BYTES: usize = 4096;

/// 从 `end` 向前：跳过 `space`（空白 / 注释链），再匹配一个整数，然后继续匹配其余
/// `remaining - 1` 个 `整数 space`；任一回溯分支成立即返回 true。
fn space_then_uint_before(
    data: &[u8],
    end: usize,
    remaining: usize,
    hops: usize,
    work: &mut usize,
) -> bool {
    let mut j = end;
    while j > 0 && is_pdf_whitespace(data[j - 1]) {
        j -= 1;
    }
    *work += end - j;
    if j == 0 {
        return false; // 文件头之前没有数字
    }
    if data[j - 1].is_ascii_digit() && uint_then_rest_before(data, j, remaining, work) {
        return true;
    }
    // 不是整数（或整数解释走不通）：只能是注释结尾——注意注释体本身也可以以数字收尾
    // （`1 0%7\nobj`），所以数字解释失败后仍要按注释继续尝试。
    // lopdf 的注释以 EOL 结束（注释体本身可以以 SP/TAB/NUL/FF 收尾），
    // 所以这段空白里必须出现 CR/LF；没有就不是注释——`obj obj obj …` 这类输入在此直接
    // 返回，否则每个候选都会白白回看 4 KiB（实测约 1 s/MiB），把唯一的 KB worker 挂住几分钟。
    if !data[j..end].iter().any(|&b| b == b'\n' || b == b'\r') {
        return false;
    }
    if hops == 0 {
        return true; // 注释链超过上限，无法判定，按对象头处理
    }
    let line_floor = j.saturating_sub(MAX_PDF_COMMENT_LINE_BYTES);
    let mut line_start = j;
    while line_start > line_floor && data[line_start - 1] != b'\n' && data[line_start - 1] != b'\r' {
        line_start -= 1;
    }
    *work += (j - line_start) * 2;
    if line_start > 0 && data[line_start - 1] != b'\n' && data[line_start - 1] != b'\r' {
        return true; // 本行超过回看上限，无法判定，按对象头处理
    }
    for p in line_start..j {
        if data[p] != b'%' {
            continue;
        }
        let mut k = p;
        while k > line_start && is_pdf_whitespace(data[k - 1]) {
            k -= 1;
        }
        let matched = if k == line_start {
            // 注释是本行第一个 token：space 链延续到上一行
            space_then_uint_before(data, line_start, remaining, hops - 1, work)
        } else if data[k - 1].is_ascii_digit() {
            uint_then_rest_before(data, k, remaining, work)
        } else {
            false
        };
        if matched {
            return true;
        }
    }
    false
}

/// `digits_end` 之前是一个整数：匹配它，然后继续匹配其余 `remaining - 1` 个 `整数 space`
fn uint_then_rest_before(
    data: &[u8],
    digits_end: usize,
    remaining: usize,
    work: &mut usize,
) -> bool {
    let mut d = digits_end;
    while d > 0 && data[d - 1].is_ascii_digit() {
        d -= 1;
    }
    *work += digits_end - d;
    if d == digits_end {
        return false;
    }
    if remaining <= 1 {
        return true;
    }
    space_then_uint_before(data, d, remaining - 1, MAX_PDF_COMMENT_HOPS, work)
}

/// 快速获取 PDF 页数，不解析文本内容
pub fn count_pdf_pages(data: &[u8]) -> Result<usize, GBrainError> {
    check_pdf_nesting_depth(data)?;
    let pdf = lopdf::Document::load_mem(data)
        .map_err(|e| GBrainError::FileError(format!("PDF load failed: {}", e)))?;
    Ok(pdf.get_pages().len())
}

impl Default for PdfParser {
    fn default() -> Self {
        Self::new()
    }
}

impl PdfParser {
    pub fn new() -> Self {
        Self
    }
}

impl DocumentParser for PdfParser {
    fn parse(&self, data: &[u8]) -> Result<ParsedDocument, GBrainError> {
        check_pdf_nesting_depth(data)?;
        let pdf = lopdf::Document::load_mem(data)
            .map_err(|e| GBrainError::FileError(format!("PDF load failed: {}", e)))?;

        let pages = pdf.get_pages();
        let total_pages = pages.len();
        if total_pages > MAX_PDF_PAGES {
            return Err(GBrainError::InvalidInput(format!(
                "PDF 页数 {total_pages} 超过上限 {MAX_PDF_PAGES}，请拆分文件后重试"
            )));
        }
        // get_pages() 返回 BTreeMap<u32, ObjectId>，value 是 PDF 内真实的 page object ID
        let page_ids: Vec<(u32, lopdf::ObjectId)> = pages.into_iter().collect();

        // FIX9-04: 在拼接全文时累计字符偏移，为每页 block 写入 source span
        let mut page_texts: Vec<String> = Vec::new();
        let mut all_text = Vec::new();
        let mut low_density_pages = 0u32;
        // 记录每页 block 在全文中的起止偏移
        let mut page_spans: Vec<(usize, usize)> = Vec::new();
        let mut global_offset: usize = 0;

        // Phase 1: 页级分析结果
        let mut page_analyses: Vec<serde_json::Value> = Vec::new();
        let mut image_rich_pages: Vec<i32> = Vec::new();
        let mut uncertain_pages: Vec<i32> = Vec::new();

        for (page_idx, (page_num, page_obj_id)) in page_ids.iter().enumerate() {
            // Phase 1: 分析页面对象，检测图片、矢量对象、不可见文本、字体编码
            let analysis = analyze_page_objects(&pdf, *page_obj_id);

            let page_num_i32 = (page_idx + 1) as i32;

            if analysis.image_area_ratio >= 0.08 || analysis.image_count >= 1 {
                image_rich_pages.push(page_num_i32);
            }
            if analysis.has_vector_objects || analysis.content_parse_failed {
                uncertain_pages.push(page_num_i32);
            }

            match pdf.extract_text(&[*page_num]) {
                Ok(text) => {
                    let cleaned = clean_text(&text);
                    let deduped = remove_header_footer(&cleaned, &page_texts);

                    let density = deduped.chars().count();
                    if density < 50 {
                        low_density_pages += 1;
                    }

                    // 字体编码异常检测：content stream 有文本操作符但提取文本极少或为乱码
                    // 不限制字符数阈值，长乱码文本（大量私有区/替换字符）也需触发 OCR
                    let font_suspected = analysis.font_encoding_suspected
                        || (!analysis.content_parse_failed
                            && !deduped.trim().is_empty()
                            && is_text_garbled(&deduped));

                    // Phase 1: 记录页级分析
                    page_analyses.push(serde_json::json!({
                        "page_number": page_num_i32,
                        "text": deduped,
                        "char_count": density,
                        "image_area_ratio": analysis.image_area_ratio,
                        "image_count": analysis.image_count,
                        "has_vector_or_unknown_objects": analysis.has_vector_objects,
                        "has_annotations": analysis.has_annotations,
                        "width": analysis.page_width,
                        "height": analysis.page_height,
                        "content_parse_failed": analysis.content_parse_failed,
                        "has_vector_drawing_ops": analysis.has_vector_drawing_ops,
                        "has_invisible_text": analysis.has_invisible_text,
                        "font_encoding_suspected": font_suspected,
                    }));

                    page_texts.push(deduped.clone());
                    if !deduped.trim().is_empty() {
                        let page_block = format!("[PAGE:{}]\n{}", page_num, deduped);
                        let start = global_offset;
                        let block_len = page_block.chars().count();
                        let end = start + block_len;
                        page_spans.push((start, end));
                        global_offset += block_len + 2;
                        all_text.push(page_block);
                    } else {
                        page_spans.push((global_offset, global_offset));
                    }
                }
                Err(_) => {
                    // Phase 1: 解析失败的页也要记录分析
                    page_analyses.push(serde_json::json!({
                        "page_number": page_num_i32,
                        "text": "",
                        "char_count": 0,
                        "image_area_ratio": analysis.image_area_ratio,
                        "image_count": analysis.image_count,
                        "has_vector_or_unknown_objects": analysis.has_vector_objects,
                        "has_annotations": analysis.has_annotations,
                        "width": analysis.page_width,
                        "height": analysis.page_height,
                        "content_parse_failed": true,
                        "has_vector_drawing_ops": analysis.has_vector_drawing_ops,
                        "has_invisible_text": analysis.has_invisible_text,
                        "font_encoding_suspected": analysis.font_encoding_suspected,
                    }));
                    // 空页也要记录 span 占位
                    page_spans.push((global_offset, global_offset));
                    page_texts.push(String::new());
                    continue;
                }
            }
        }

        let content = all_text.join("\n\n");

        // Conservative parser result; the configured detector later persists
        // the authoritative selection used for execution.
        let mut needs_ocr_pages: Vec<i32> = Vec::new();
        let mut ocr_reasons_by_page: std::collections::BTreeMap<String, Vec<String>> =
            std::collections::BTreeMap::new();
        for page in &page_analyses {
            let page_number = page
                .get("page_number")
                .and_then(|v| v.as_i64())
                .unwrap_or(0) as i32;
            let mut reasons = Vec::new();
            let text_empty = page
                .get("text")
                .and_then(|v| v.as_str())
                .map(|v| v.trim().is_empty())
                .unwrap_or(true);
            let char_count = page.get("char_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            if text_empty {
                reasons.push("empty_text_layer".to_string());
            }
            if char_count < 50 {
                reasons.push("low_text_density".to_string());
            }
            if page
                .get("image_count")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
                > 0
            {
                reasons.push("embedded_image".to_string());
            }
            if page
                .get("image_area_ratio")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                >= 0.08
            {
                reasons.push("image_area".to_string());
            }
            for (key, reason) in [
                ("has_vector_or_unknown_objects", "vector_or_unknown_objects"),
                ("has_annotations", "has_annotations"),
                ("content_parse_failed", "parser_error"),
                ("has_vector_drawing_ops", "vector_or_unknown_objects"),
                ("has_invisible_text", "hidden_or_invisible_text_layer"),
                ("font_encoding_suspected", "font_encoding_issue"),
            ] {
                if page.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
                    && !reasons.iter().any(|existing| existing == reason)
                {
                    reasons.push(reason.to_string());
                }
            }
            if !reasons.is_empty() {
                needs_ocr_pages.push(page_number);
                ocr_reasons_by_page.insert(page_number.to_string(), reasons);
            }
        }
        let needs_ocr = !needs_ocr_pages.is_empty();
        let ocr_scope = if needs_ocr_pages.is_empty() {
            "none"
        } else if needs_ocr_pages.len() == total_pages {
            "full"
        } else {
            "partial"
        };

        let mut metadata = HashMap::new();
        metadata.insert("total_pages".to_string(), total_pages.to_string());
        // P2-004: 每页文本以 JSON 数组记录（含 page_number）
        metadata.insert(
            "page_texts".to_string(),
            serde_json::to_string(&page_texts).unwrap_or_default(),
        );
        // P2-006: 文本密度标记
        metadata.insert("needs_ocr".to_string(), needs_ocr.to_string());
        metadata.insert(
            "low_density_pages".to_string(),
            low_density_pages.to_string(),
        );

        // Phase 1: 完整 OCR metadata
        metadata.insert(
            "page_analyses".to_string(),
            serde_json::to_string(&page_analyses).unwrap_or_default(),
        );
        metadata.insert(
            "image_rich_pages".to_string(),
            serde_json::to_string(&image_rich_pages).unwrap_or_default(),
        );
        metadata.insert(
            "uncertain_pages".to_string(),
            serde_json::to_string(&uncertain_pages).unwrap_or_default(),
        );
        metadata.insert(
            "needs_ocr_pages".to_string(),
            serde_json::to_string(&needs_ocr_pages).unwrap_or_default(),
        );
        metadata.insert(
            "ocr_reasons_by_page".to_string(),
            serde_json::to_string(&ocr_reasons_by_page).unwrap_or_default(),
        );
        metadata.insert("ocr_scope".to_string(), ocr_scope.to_string());

        // FIX9-04: 为每页 block 写入真实的 source span（基于 page_spans）
        let blocks: Vec<crate::kb::types::ParsedBlock> = page_texts
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let (start, end) = page_spans.get(i).copied().unwrap_or((0, 0));
                crate::kb::types::ParsedBlock {
                    text: text.clone(),
                    title_path: String::new(),
                    page_number: Some((i + 1) as i32),
                    source_start: Some(start as i32),
                    source_end: Some(end as i32),
                    block_type: "page".to_string(),
                    metadata: String::new(),
                }
            })
            .collect();

        Ok(ParsedDocument {
            content,
            metadata,
            blocks: Some(blocks),
            media_refs: Vec::new(),
        })
    }

    fn extensions(&self) -> &[&str] {
        &["pdf"]
    }
}

/// 将 lopdf Object 转为 f64（支持 Integer 和 Real 两种变体）
fn object_to_f64(obj: &lopdf::Object) -> Option<f64> {
    match obj {
        lopdf::Object::Real(f) => Some(*f as f64),
        lopdf::Object::Integer(i) => Some(*i as f64),
        _ => None,
    }
}

/// 解析 XObject 字典中的间接引用，返回实际对象
fn resolve_xobject<'a>(
    pdf: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Object> {
    match obj {
        lopdf::Object::Reference(obj_id) => pdf.get_object(*obj_id).ok(),
        other => Some(other),
    }
}

/// 将 Object 解析为字典引用，处理间接引用
fn resolve_as_dict<'a>(
    pdf: &'a lopdf::Document,
    obj: &'a lopdf::Object,
) -> Option<&'a lopdf::Dictionary> {
    match obj {
        lopdf::Object::Reference(obj_id) => pdf.get_object(*obj_id).ok()?.as_dict().ok(),
        lopdf::Object::Dictionary(d) => Some(d),
        _ => None,
    }
}

/// 获取页面的 Resources 字典，解析间接引用并沿父 Pages 节点继承资源
fn get_inherited_resources(
    pdf: &lopdf::Document,
    page_obj_id: lopdf::ObjectId,
) -> Option<&lopdf::Dictionary> {
    let mut current_id = page_obj_id;
    let mut visited: std::collections::HashSet<lopdf::ObjectId> = std::collections::HashSet::new();
    let mut depth = 0usize;
    const MAX_PARENT_DEPTH: usize = 64;
    loop {
        if depth > MAX_PARENT_DEPTH {
            // 遍历深度异常，返回 None 由上层保守处理
            return None;
        }
        if !visited.insert(current_id) {
            // 检测到 Parent 环（自引用或成环），返回 None 由上层保守处理
            return None;
        }
        depth += 1;
        let obj = pdf.get_object(current_id).ok()?;
        let dict = obj.as_dict().ok()?;
        if let Ok(resources) = dict.get(b"Resources") {
            return resolve_as_dict(pdf, resources);
        }
        // 当前节点无 Resources，沿 Parent 向上查找
        let parent = dict.get(b"Parent").ok()?;
        match parent {
            lopdf::Object::Reference(id) => current_id = *id,
            _ => return None,
        }
    }
}

/// 沿父 Pages 节点向上解析继承的 MediaBox 或 CropBox。
/// 当页面自身未直接携带 MediaBox 时用于获取页面尺寸。
/// 返回 [x1, y1, x2, y2] 坐标数组。
fn get_inherited_media_box(
    pdf: &lopdf::Document,
    page_obj_id: lopdf::ObjectId,
) -> Option<[f64; 4]> {
    let mut current_id = page_obj_id;
    let mut visited: std::collections::HashSet<lopdf::ObjectId> = std::collections::HashSet::new();
    let mut depth = 0usize;
    const MAX_PARENT_DEPTH: usize = 64;
    loop {
        if depth > MAX_PARENT_DEPTH {
            // 遍历深度异常，返回 None 由 detector 保守处理
            return None;
        }
        if !visited.insert(current_id) {
            // 检测到 Parent 环（自引用或成环），返回 None 由 detector 保守处理
            return None;
        }
        depth += 1;
        let obj = pdf.get_object(current_id).ok()?;
        let dict = obj.as_dict().ok()?;
        // 优先检查 MediaBox
        if let Ok(mediabox) = dict.get(b"MediaBox") {
            if let Ok(arr) = mediabox.as_array() {
                if arr.len() >= 4 {
                    // 四个坐标全部成功解析且为有限数时才返回，任一异常返回 None，
                    // 由 detector 路由到 GeometryUncertain 保守进入 OCR
                    let x1 = object_to_f64(&arr[0]);
                    let y1 = object_to_f64(&arr[1]);
                    let x2 = object_to_f64(&arr[2]);
                    let y2 = object_to_f64(&arr[3]);
                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                        if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite() {
                            return Some([x1, y1, x2, y2]);
                        }
                    }
                    // 坐标存在但无法解析为有效数字，不再用默认值替代，
                    // 返回 None 让 detector 将此页归入 uncertain
                    return None;
                }
            }
        }
        // 回退到 CropBox
        if let Ok(cropbox) = dict.get(b"CropBox") {
            if let Ok(arr) = cropbox.as_array() {
                if arr.len() >= 4 {
                    let x1 = object_to_f64(&arr[0]);
                    let y1 = object_to_f64(&arr[1]);
                    let x2 = object_to_f64(&arr[2]);
                    let y2 = object_to_f64(&arr[3]);
                    if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                        if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite() {
                            return Some([x1, y1, x2, y2]);
                        }
                    }
                    return None;
                }
            }
        }
        // 沿 Parent 向上
        let parent = dict.get(b"Parent").ok()?;
        match parent {
            lopdf::Object::Reference(id) => current_id = *id,
            _ => return None,
        }
    }
}

/// 检测 PDF 页面 content stream 中的 inline image（BI/ID/EI 操作符）
///
/// XObject 只能检测命名的 Image 资源，inline image 通过 BI/ID/EI
/// 操作符直接嵌入在 content stream 中，需要解析 stream 才能发现。
fn detect_inline_images(pdf: &lopdf::Document, page_obj_id: lopdf::ObjectId) -> usize {
    let page_obj = match pdf.get_object(page_obj_id) {
        Ok(obj) => obj,
        // 解析失败时返回最大值，触发不确定页标记（保守策略）
        Err(_) => return usize::MAX / 2,
    };
    let page_dict = match page_obj.as_dict() {
        Ok(d) => d,
        // 解析失败时返回最大值
        Err(_) => return usize::MAX / 2,
    };

    let contents = match page_dict.get(b"Contents") {
        Ok(c) => c,
        // 无 Contents 不一定没有 inline image，但无法检测，保守返回 0
        Err(_) => return 0,
    };

    let collected = collect_content_stream(pdf, contents);
    if collected.had_failure {
        // 任一内容流不可读取时都无法可靠排除 inline image。
        return usize::MAX / 2;
    }

    if collected.data.is_empty() {
        return 0;
    }

    match lopdf::content::Content::decode(&collected.data) {
        Ok(content) => content
            .operations
            .iter()
            .filter(|operation| operation.operator == "BI")
            .count(),
        // An inline image stream whose binary payload cannot be decoded is
        // intentionally classified as uncertain and routed through OCR.
        Err(_) => usize::MAX / 2,
    }
}

/// 汇总页面 content stream 数据（处理单个 stream 或 stream 数组）
/// 保留任一成员解析/解压失败的状态，避免部分成功被误当成完整页面内容。
struct CollectedContentStream {
    data: Vec<u8>,
    had_failure: bool,
}

fn collect_content_stream(
    pdf: &lopdf::Document,
    contents: &lopdf::Object,
) -> CollectedContentStream {
    match contents {
        lopdf::Object::Stream(stream) => match stream.decompressed_content() {
            Ok(data) => CollectedContentStream {
                data,
                had_failure: false,
            },
            Err(_) => CollectedContentStream {
                data: vec![],
                had_failure: true,
            },
        },
        lopdf::Object::Reference(id) => match pdf.get_object(*id) {
            Ok(lopdf::Object::Stream(s)) => match s.decompressed_content() {
                Ok(data) => CollectedContentStream {
                    data,
                    had_failure: false,
                },
                Err(_) => CollectedContentStream {
                    data: vec![],
                    had_failure: true,
                },
            },
            _ => CollectedContentStream {
                data: vec![],
                had_failure: true,
            },
        },
        lopdf::Object::Array(arr) => {
            let mut data = Vec::new();
            let mut had_failure = false;
            for obj in arr {
                let stream_data = match obj {
                    lopdf::Object::Reference(id) => match pdf.get_object(*id) {
                        Ok(lopdf::Object::Stream(s)) => match s.decompressed_content() {
                            Ok(data) => data,
                            Err(_) => {
                                had_failure = true;
                                continue;
                            }
                        },
                        _ => {
                            had_failure = true;
                            continue;
                        }
                    },
                    lopdf::Object::Stream(s) => match s.decompressed_content() {
                        Ok(data) => data,
                        Err(_) => {
                            had_failure = true;
                            continue;
                        }
                    },
                    _ => {
                        had_failure = true;
                        continue;
                    }
                };
                if !stream_data.is_empty() {
                    if !data.is_empty() {
                        data.push(b' ');
                    }
                    data.extend_from_slice(&stream_data);
                }
            }
            CollectedContentStream { data, had_failure }
        }
        _ => CollectedContentStream {
            data: vec![],
            had_failure: true,
        },
    }
}

/// 页面分析扩展结果
struct PageAnalysisResult {
    image_count: usize,
    image_area_ratio: f64,
    has_vector_objects: bool,
    /// 独立于 has_vector_objects 的批注不确定标志，
    /// 不受 Resources 解析流程重置影响，最终合并到 has_vector_objects
    has_annotations: bool,
    page_width: Option<u32>,
    page_height: Option<u32>,
    content_parse_failed: bool,
    has_vector_drawing_ops: bool,
    has_invisible_text: bool,
    font_encoding_suspected: bool,
}

/// 分析 PDF 页面中的对象，检测图片、矢量对象、不可见文本、字体编码异常
///
/// `page_obj_id` 为 get_pages() 返回的真实 ObjectId，不可用页码构造
fn analyze_page_objects(pdf: &lopdf::Document, page_obj_id: lopdf::ObjectId) -> PageAnalysisResult {
    let mut image_count = 0usize;
    let mut total_image_area: f64 = 0.0;
    // 保守策略：默认 has_vector_objects=true，无法确认时加入 uncertain_pages
    let mut has_vector_objects = true;
    // 独立于 has_vector_objects 的批注标志，不受后续 Resources 重置影响
    let mut has_annotations = false;
    let mut page_area: f64 = 1.0; // 默认 1.0 避免除零
    let mut resources_resolved = false;
    let mut page_width: Option<u32> = None;
    let mut page_height: Option<u32> = None;
    let mut content_parse_failed = false;
    let mut has_vector_drawing_ops = false;
    let mut has_invisible_text = false;
    let font_encoding_suspected = false;

    // 获取页面尺寸：先尝试页面自身的 MediaBox，缺失时沿父 Pages 节点继承
    if let Ok(page_obj) = pdf.get_object(page_obj_id) {
        if let Ok(page_dict) = page_obj.as_dict() {
            // 尝试直接 MediaBox
            let mut mediabox_found = false;
            if let Ok(mediabox) = page_dict.get(b"MediaBox") {
                if let Ok(arr) = mediabox.as_array() {
                    if arr.len() >= 4 {
                        // 四个坐标全部成功解析且为有限数时才设置尺寸，
                        // 任一异常保留 None，由 detector 路由到 GeometryUncertain
                        let x1 = object_to_f64(&arr[0]);
                        let y1 = object_to_f64(&arr[1]);
                        let x2 = object_to_f64(&arr[2]);
                        let y2 = object_to_f64(&arr[3]);
                        if let (Some(x1), Some(y1), Some(x2), Some(y2)) = (x1, y1, x2, y2) {
                            if x1.is_finite() && y1.is_finite() && x2.is_finite() && y2.is_finite()
                            {
                                let w = x2 - x1;
                                let h = y2 - y1;
                                page_area = (w * h).max(1.0);
                                page_width = Some(w as u32);
                                page_height = Some(h as u32);
                                mediabox_found = true;
                            }
                        }
                    }
                }
            }
            // 页面自身无 MediaBox 时继承父节点
            if !mediabox_found {
                if let Some([x1, y1, x2, y2]) = get_inherited_media_box(pdf, page_obj_id) {
                    let w = (x2 - x1).max(0.0);
                    let h = (y2 - y1).max(0.0);
                    page_area = (w * h).max(1.0);
                    page_width = Some(w as u32);
                    page_height = Some(h as u32);
                }
            }

            // 检查页面批注（/Annots）：印章、签名图、表单控件等可见内容
            // 可能包含 appearance stream (/AP)，未参与文本层但应在 OCR 判定中考虑
            if let Ok(annots_obj) = page_dict.get(b"Annots") {
                // 先尝试直接数组，再尝试间接引用
                let annots_arr: Option<&Vec<lopdf::Object>> = if let Ok(arr) = annots_obj.as_array()
                {
                    Some(arr)
                } else if let lopdf::Object::Reference(ref_id) = annots_obj {
                    pdf.get_object(*ref_id).ok().and_then(|o| o.as_array().ok())
                } else {
                    None
                };
                if let Some(arr) = annots_arr {
                    if !arr.is_empty() {
                        // 存在未解析的可见批注，使用独立标志避免被 Resources 重置覆盖
                        has_annotations = true;
                    }
                }
            }
        }
    }

    // 遍历页面资源中的 XObject，检测图片
    // 使用 get_inherited_resources 解析间接引用并沿父 Pages 节点继承资源
    if let Some(res_dict) = get_inherited_resources(pdf, page_obj_id) {
        resources_resolved = true;
        // 成功获取 Resources 后重置 has_vector_objects，仅在实际检测到矢量对象时设为 true
        has_vector_objects = false;

        if let Ok(xobjects) = res_dict.get(b"XObject") {
            if let Some(xobj_dict) = resolve_as_dict(pdf, xobjects) {
                for (_name, obj) in xobj_dict.iter() {
                    let Some(xobj) = resolve_xobject(pdf, obj) else {
                        // 无法解析的 XObject 保守标记为不确定
                        has_vector_objects = true;
                        continue;
                    };
                    if let Ok(xobj_dict) = xobj.as_dict() {
                        // 检查 Subtype
                        if let Ok(subtype) = xobj_dict.get(b"Subtype") {
                            match subtype {
                                lopdf::Object::Name(name) if name == b"Image" => {
                                    image_count += 1;
                                    // 估算图片面积
                                    let img_w = xobj_dict
                                        .get(b"Width")
                                        .ok()
                                        .and_then(object_to_f64)
                                        .unwrap_or(100.0);
                                    let img_h = xobj_dict
                                        .get(b"Height")
                                        .ok()
                                        .and_then(object_to_f64)
                                        .unwrap_or(100.0);
                                    total_image_area += img_w * img_h;
                                }
                                lopdf::Object::Name(name) if name == b"Form" => {
                                    // Form XObject 可能包含矢量图形
                                    has_vector_objects = true;
                                }
                                // 未知 Subtype，保守标记为不确定
                                _ => {
                                    has_vector_objects = true;
                                }
                            }
                        } else {
                            // XObject 无 Subtype 字段，保守标记为不确定
                            has_vector_objects = true;
                        }
                    } else {
                        // XObject 对象本身无法解析为字典，保守标记为不确定
                        has_vector_objects = true;
                    }
                }
            } else {
                // /XObject 无法解析为字典，保守标记为不确定
                has_vector_objects = true;
            }
        }

        // 检查是否有 Graphics State（可能指示复杂矢量图形）
        if res_dict.get(b"ExtGState").is_ok() {
            // 有扩展图形状态，可能是矢量对象
            has_vector_objects = true;
        }
    }

    // 检测 content stream 中的 inline image (BI 操作符)
    // XObject 检测无法覆盖 inline image，需要解析 content stream
    let inline_image_count = detect_inline_images(pdf, page_obj_id);
    // 保守值（usize::MAX / 2）表示解析失败，标记为不确定
    if inline_image_count > 0 && inline_image_count < usize::MAX / 4 {
        image_count += inline_image_count;
        // 无法精确估算 inline image 面积，保守按每张占页面 50% 估算
        total_image_area += page_area * 0.5 * inline_image_count as f64;
    } else if inline_image_count >= usize::MAX / 4 {
        // content stream 解析失败，保守标记为不确定
        has_vector_objects = true;
    }

    // 如果无法获取 Resources 字典，保守标记为不确定
    if !resources_resolved {
        has_vector_objects = true;
    }

    // 将独立保存的批注不确定状态合并回 has_vector_objects
    // 必须在 Resources 处理之后合并，避免被第 626 行重置覆盖
    if has_annotations {
        has_vector_objects = true;
    }

    // 计算图片面积占比（图片面积之和 / 页面面积）
    // 由于图片坐标可能未归一化，使用简单估算
    let image_area_ratio = if page_area > 0.0 {
        (total_image_area / page_area).min(1.0)
    } else {
        0.0
    };

    // 分析 content stream：检测绘图操作符、不可见文本、字体编码异常
    if let Ok(page_obj) = pdf.get_object(page_obj_id) {
        if let Ok(page_dict) = page_obj.as_dict() {
            if let Ok(contents) = page_dict.get(b"Contents") {
                let collected = collect_content_stream(pdf, contents);
                if collected.had_failure {
                    content_parse_failed = true;
                }
                if !collected.data.is_empty() {
                    analyze_content_stream(
                        &collected.data,
                        &mut has_vector_drawing_ops,
                        &mut has_invisible_text,
                        &mut content_parse_failed,
                    );
                }
            }
        }
    } else {
        content_parse_failed = true;
    }

    // 简单字体编码异常检测：如果页面对象存在文本操作符但 extract_text 得到的字符极少或全是乱码
    // 在调用方结合 text 长度与 content stream 文本操作符密度对比来判断
    // 此处仅设置 content_parse_failed 供上层使用

    PageAnalysisResult {
        image_count,
        image_area_ratio,
        has_vector_objects,
        has_annotations,
        page_width,
        page_height,
        content_parse_failed,
        has_vector_drawing_ops,
        has_invisible_text,
        font_encoding_suspected,
    }
}

/// 简易乱码检测：如果文本中非 ASCII 字符占比异常高且多为控制字符/私有区字符，
/// 则判定提取的文本可能是乱码（字体编码异常）
fn is_text_garbled(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let total = text.chars().count();
    if total == 0 {
        return false;
    }
    // 统计可疑字符（私有区 U+E000-U+F8FF、控制字符、替换字符）
    let suspicious = text
        .chars()
        .filter(|c| {
            (&'\u{E000}'..=&'\u{F8FF}').contains(&c) // 私有使用区
                || c == &'\u{FFFD}' // 替换字符
                || (c.is_control() && *c != '\n' && *c != '\r' && *c != '\t')
        })
        .count();
    // 可疑字符占比超过 30% 视为乱码
    suspicious as f64 / total as f64 > 0.3
}

/// 分析 content stream 检测绘图操作符和不可见文本
///
/// 检测内容：
/// - 矢量绘图操作符：m, l, c, re, S, f, B（路径构造和填充/描边）
/// - 不可见文本：文本渲染模式 Tr=3（不可见但可搜索）
///
/// 使用 lopdf 内容流解析而非固定子串匹配，
/// 以正确处理合法 PDF token 格式，且不会扫描文字字符串内部。
/// 解析失败时标记为不确定，保守触发 OCR。
/// 分析 content stream 检测不可见文本、路径绘制文字风险和解析错误。
///
/// 保守策略：只要内容流执行了可见路径填充或描边，就无法仅凭语法证明
/// 该绘图不是路径文字，因此将页面纳入不确定范围。
fn analyze_content_stream(
    data: &[u8],
    has_vector_drawing_ops: &mut bool,
    has_invisible_text: &mut bool,
    content_parse_failed: &mut bool,
) {
    let content = match lopdf::content::Content::decode(data) {
        Ok(content) => content,
        Err(_) => {
            *content_parse_failed = true;
            return;
        }
    };

    // 路径绘制操作符：S/s(描边), f/F(填充), B/b(填充+描边)
    const PATH_PAINT_OPS: &[&str] = &["S", "s", "f", "F", "f*", "B", "B*", "b", "b*"];

    for operation in &content.operations {
        let op = operation.operator.as_str();
        if PATH_PAINT_OPS.contains(&op) {
            *has_vector_drawing_ops = true;
        }
        if op == "Tr" {
            match operation.operands.first().and_then(object_to_f64) {
                Some(mode) if (mode - 3.0).abs() < f64::EPSILON => {
                    *has_invisible_text = true;
                }
                Some(_) => {}
                None => {
                    *content_parse_failed = true;
                }
            }
        }
    }
}

fn clean_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut result = Vec::new();
    let mut prev_empty = false;

    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_empty {
                result.push(String::new());
                prev_empty = true;
            }
        } else {
            let normalized: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
            result.push(normalized);
            prev_empty = false;
        }
    }

    result.join("\n")
}

/// P2-005: 启发式去除页眉页脚 — 检测与前页重复的首/尾行
fn remove_header_footer(text: &str, previous_pages: &[String]) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= 2 || previous_pages.is_empty() {
        return text.to_string();
    }

    let first = lines[0].trim();
    let last = lines.last().map(|l| l.trim()).unwrap_or("");

    let mut remove_first = false;
    let mut remove_last = false;

    // 检查与先前页面的重复
    for prev in previous_pages.iter().rev().take(3) {
        let prev_lines: Vec<&str> = prev.lines().collect();
        if !prev_lines.is_empty() && prev_lines[0].trim() == first {
            remove_first = true;
        }
        if let Some(prev_last) = prev_lines.last() {
            if prev_last.trim() == last && last.chars().count() < 50 {
                remove_last = true;
            }
        }
    }

    // 检测纯数字行（页码）
    if first.chars().all(|c| c.is_ascii_digit()) {
        remove_first = true;
    }
    if last.chars().all(|c| c.is_ascii_digit()) {
        remove_last = true;
    }

    let range = if remove_first { 1 } else { 0 }..if remove_last {
        lines.len().saturating_sub(1)
    } else {
        lines.len()
    };

    lines[range].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_header_footer_repeat() {
        let page1 = "Chapter 1\nSome content\nPage 1";
        let page2 = "Chapter 1\nMore content\nPage 2";
        let previous = vec![page1.to_string()];
        let result = remove_header_footer(page2, &previous);
        assert!(!result.contains("Chapter 1"));
        assert!(result.contains("More content"));
        // 页码被移除（footer 变了所以不会被移除，因为不匹配 previous）
    }

    #[test]
    fn test_clean_text() {
        let text = "Hello   world\n\n   \nFoo bar";
        let result = clean_text(text);
        assert!(result.contains("Hello world"));
        assert!(result.contains("Foo bar"));
    }

    // ---- 预扫描（check_pdf_nesting_depth）----

    fn zlib_compress(data: &[u8]) -> Vec<u8> {
        use std::io::Write as _;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn deep_array(depth: usize) -> Vec<u8> {
        let mut v = vec![b'['; depth];
        v.extend(std::iter::repeat(b']').take(depth));
        v
    }

    fn stream_object(id: u32, dict_extra: &str, body: &[u8]) -> Vec<u8> {
        let mut v = format!(
            "{id} 0 obj\n<< /Length {} {dict_extra} >>\nstream\n",
            body.len()
        )
        .into_bytes();
        v.extend_from_slice(body);
        v.extend_from_slice(b"\nendstream\nendobj\n");
        v
    }

    fn err_text(r: Result<(), GBrainError>) -> String {
        match r {
            Ok(()) => String::new(),
            Err(e) => e.to_string(),
        }
    }

    #[test]
    fn prescan_rejects_deep_nesting_at_file_level() {
        let mut pdf = b"%PDF-1.4\n1 0 obj\n".to_vec();
        pdf.extend(deep_array(MAX_PDF_OBJECT_NESTING + 10));
        pdf.extend_from_slice(b"\nendobj\n");
        assert!(err_text(check_pdf_nesting_depth(&pdf)).contains("嵌套深度"));
    }

    #[test]
    fn prescan_accepts_moderate_nesting_and_binary_noise() {
        let mut pdf = b"%PDF-1.4\n1 0 obj\n".to_vec();
        pdf.extend(deep_array(MAX_PDF_OBJECT_NESTING - 1));
        pdf.extend_from_slice(b"\nendobj\n");
        // 未声明 Filter 的流体会按原始字节检查，但随机二进制里散落的 `[` 不应累计
        let noise: Vec<u8> = (0..20_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        pdf.extend(stream_object(2, "", &noise));
        assert!(check_pdf_nesting_depth(&pdf).is_ok());
    }

    #[test]
    fn prescan_rejects_nesting_inside_flate_object_stream() {
        let mut inner = b"2 0 ".to_vec();
        inner.extend(deep_array(MAX_PDF_OBJECT_NESTING + 10));
        let body = zlib_compress(&inner);
        let mut pdf = b"%PDF-1.5\n".to_vec();
        pdf.extend(stream_object(5, "/Type /ObjStm /N 1 /First 4 /Filter /FlateDecode", &body));
        assert!(err_text(check_pdf_nesting_depth(&pdf)).contains("嵌套深度"));
    }

    #[test]
    fn prescan_rejects_zlib_bomb_stream() {
        let body = zlib_compress(&vec![0u8; MAX_PDF_STREAM_INFLATED_BYTES + 1024 * 1024]);
        let mut pdf = b"%PDF-1.5\n".to_vec();
        pdf.extend(stream_object(3, "/Filter /FlateDecode", &body));
        assert!(err_text(check_pdf_nesting_depth(&pdf)).contains("解压"));
    }

    #[test]
    fn prescan_rejects_huge_png_predictor_columns() {
        let body = zlib_compress(&[0u8; 16]);
        let mut pdf = b"%PDF-1.5\n".to_vec();
        pdf.extend(stream_object(
            4,
            "/Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 2000000000 >>",
            &body,
        ));
        assert!(err_text(check_pdf_nesting_depth(&pdf)).contains("预测器"));
    }

    #[test]
    fn prescan_finds_headers_hidden_inside_strings() {
        // lopdf 按 xref 偏移直接解析，字符串里的 `N G obj` 一样会被当对象头
        for header in [
            &b"12 0 obj"[..],
            b"12 0 %c \nobj",
            b"1 0%7\nobj",
            b"(5 %x 12 0 %c\nobj",
            b"12 0 %a 5 %b\nobj",
        ] {
            let mut pdf = b"%PDF-1.4\n1 0 obj\n(".to_vec();
            pdf.extend_from_slice(header);
            pdf.push(b' ');
            pdf.extend(deep_array(MAX_PDF_OBJECT_NESTING + 10));
            pdf.extend_from_slice(b")\nendobj\n");
            assert!(
                err_text(check_pdf_nesting_depth(&pdf)).contains("嵌套深度"),
                "hidden header not caught: {:?}",
                String::from_utf8_lossy(header)
            );
        }
    }

    #[test]
    fn prescan_handles_obj_keyword_flood_quickly() {
        let pdf = b"obj ".repeat(256 * 1024);
        let start = std::time::Instant::now();
        assert!(check_pdf_nesting_depth(&pdf).is_ok());
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
    }

    #[test]
    fn prescan_accepts_lopdf_generated_document() {
        use lopdf::content::{Content, Operation};
        use lopdf::{dictionary, Document, Object, Stream};

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font",
            "Subtype" => "Type1",
            "BaseFont" => "Courier",
        });
        let resources_id = doc.add_object(dictionary! { "Font" => dictionary! { "F1" => font_id } });
        let content = Content {
            operations: vec![
                Operation::new("BT", vec![]),
                Operation::new("Tf", vec!["F1".into(), 48.into()]),
                Operation::new("Td", vec![100.into(), 600.into()]),
                Operation::new("Tj", vec![Object::string_literal("Hello prescan")]),
                Operation::new("ET", vec![]),
            ],
        };
        let content_id = doc.add_object(Stream::new(dictionary! {}, content.encode().unwrap()));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![page_id.into()],
                "Count" => 1,
                "Resources" => resources_id,
                "MediaBox" => vec![0.into(), 0.into(), 595.into(), 842.into()],
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        doc.compress();
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();

        assert!(check_pdf_nesting_depth(&bytes).is_ok());
        assert_eq!(count_pdf_pages(&bytes).unwrap(), 1);
    }
}
