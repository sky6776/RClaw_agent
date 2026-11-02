//! 会话消息导出：选中的消息打包成 Markdown 文本文档、内嵌图片的 DOCX 或
//! 自包含单文件 HTML（折叠块 + 轮次导航 + 图片原位内嵌）。
//!
//! 设计取舍：PDF 的中文字形需要嵌入字体/子集化（重依赖），DOCX 由查看器
//! 解析字体（CJK 天然安全），因此图片文档走 DOCX；纯文本走 Markdown；
//! 需要折叠块/轮次导航的可交互回放走 HTML（零 JS，`<details>` + 锚点）。

use base64::Engine as _;

use std::io::Write;
use std::path::{Path, PathBuf};

/// 单条待导出消息（role: user | assistant | thinking | status；content 为消息原文）。
/// ts 为落库时间（unix 秒，0=未知）：HTML 导出按它算「已工作 X」段时长；md/docx 不消费。
/// seg_complete 为「该消息所在的会话段是否整段被导出」：完整导出恒 true；多选导出由
/// 前端按段判定——不完整段平铺渲染，不显示「已工作 X」（选中子集的时间跨度会虚报）。
/// msg_id 为 state.sqlite 行 id（0=未落库/未知）；tools/tool_detail_error 仅 HTML 消费
/// （md/docx 渲染器忽略），由 commands 层 attach（仅 fmt=html 时查询详情）。
#[derive(Debug, Clone)]
pub struct ExportMessage {
    pub role: String,
    pub content: String,
    pub ts: i64,
    pub seg_complete: bool,
    pub msg_id: i64,
    pub tools: Vec<ExportToolCall>,
    pub tool_detail_error: Option<String>,
}

/// 单条工具调用的导出全量内容（HTML 消费；字段已在装载层按 cap 截断）。
#[derive(Debug, Clone)]
pub struct ExportToolCall {
    pub name: String,
    pub skill: Option<String>,
    /// 完整入参（pretty JSON 文本，可能带截断注记）。
    pub input: String,
    pub duration_ms: Option<u64>,
    pub result: Option<ExportToolResult>,
    pub checkpoint: Option<ExportToolCheckpoint>,
    /// 工具详情总预算超限后，本调用只保留元信息、正文未装载。
    pub content_omitted: bool,
}

#[derive(Debug, Clone)]
pub struct ExportToolResult {
    pub content: String,
    pub is_error: bool,
}

#[derive(Debug, Clone)]
pub struct ExportToolCheckpoint {
    /// 写入前文件的显示路径（装载层已相对化到工作区，防泄露本机目录）。
    pub path: String,
    /// 写入前文件全文；None = 新建（无写入前内容）。
    pub before: Option<String>,
    /// 该次操作删除了文件。
    pub deleted: bool,
}

/// 工具详情安全阀（异常大字段/总量；正常内容几 KB 内不触发）。
pub(crate) const TOOL_INPUT_MAX_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const TOOL_RESULT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const TOOL_CHECKPOINT_MAX_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const TOOL_TOTAL_MAX_BYTES: usize = 64 * 1024 * 1024;
/// 工具详情总量超过该值时在文档头提示「工具详情较大，打开可能较慢」。
const TOOL_BIG_HINT_BYTES: usize = 20 * 1024 * 1024;

/// 按字符边界截到 max 字节以内（不切坏 UTF-8）。
fn truncate_chars_to_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// 从尾部取至多 max 字节（字符边界对齐）。
fn tail_chars_to_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// 字段级截断：超限保留头部 + 截断注记。
/// 返回 (截断后文本, 原始字节数 Option——None 未截断)。
pub(crate) fn truncate_tool_field(text: &str, max: usize) -> (String, Option<usize>) {
    if text.len() <= max {
        return (text.to_string(), None);
    }
    let original = text.len();
    (
        format!(
            "{}\n…（已截断，原文 {}）",
            truncate_chars_to_bytes(text, max),
            format_bytes(original)
        ),
        Some(original),
    )
}

/// result 专用：头尾各半（shell 错误常在尾部），中间省略。
pub(crate) fn truncate_tool_result(text: &str, max: usize) -> (String, Option<usize>) {
    if text.len() <= max {
        return (text.to_string(), None);
    }
    let original = text.len();
    let half = max / 2;
    let head = truncate_chars_to_bytes(text, half);
    let tail = tail_chars_to_bytes(text, half);
    (
        format!(
            "{head}\n…（中间省略 {}）\n{tail}",
            format_bytes(original - head.len() - tail.len())
        ),
        Some(original),
    )
}

fn format_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    }
}

/// 导出文件名安全化：保留中英数字与连字符，其余折叠为下划线。
fn safe_file_name(title: &str) -> String {
    let cleaned: String = title
        .trim()
        .chars()
        .take(40)
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '·') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = trimmed_underscores(&cleaned);
    if trimmed.is_empty() {
        "export".to_string()
    } else {
        trimmed
    }
}

fn trimmed_underscores(value: &str) -> String {
    value.trim_matches('_').to_string()
}

/// 剥离运行时媒体协议行（`[:x-media …]` / `MEDIA_MARKER=` 家族）：导出文档里
/// 它们是内部协议噪声，图片经文件引用或内嵌呈现。
fn strip_media_control_lines(content: &str) -> String {
    content
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            !(t.starts_with("MEDIA_MARKER=")
                || t.starts_with("RCLAW_VALIDATED_MEDIA_MARKER=")
                || t.starts_with("RCLAW_MEDIA_PUBLISH_ATTEMPTED=")
                || (t.starts_with("[:") && t.contains("-media ")))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 从单条消息原文提取图片工作区路径（marker path= 属性 + 正文内联路径）。
/// 与前端 MediaPreviews 同源的解析规则，条内去重保序。
fn extract_image_paths_from_content(content: &str) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    let mut push = |raw: &str| {
        let ws = raw.replace('\\', "/");
        let rel = match ws.rfind("/agent/") {
            Some(i) => ws[i + "/agent/".len()..].to_string(),
            None => ws.trim_start_matches('/').to_string(),
        };
        let rel = rel
            .trim()
            .trim_end_matches(|c: char| c == '.' || c == ',' || c.is_whitespace());
        if rel.is_empty() {
            return;
        }
        let lower = rel.to_lowercase();
        let is_image = [".png", ".jpg", ".jpeg", ".gif", ".bmp"]
            .iter()
            .any(|e| lower.ends_with(e));
        if is_image && !paths.iter().any(|p| p == rel) {
            paths.push(rel.to_string());
        }
    };
    // 1) 媒体回执行里的 path（绝对或相对）。
    let mut rest = content;
    while let Some(start) = rest.find("[:") {
        let Some(end) = rest[start..].find(']') else {
            break;
        };
        let marker = &rest[start..start + end];
        if let Some(p) = marker
            .split("path=\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
        {
            push(p);
        }
        rest = &rest[start + end..];
    }
    // 2) 正文内联的工作区相对路径。
    // 用 ASCII 小写化做大小写不敏感查找：to_lowercase() 对部分 Unicode
    // （如 'İ'）会改变字节长度，索引就不再对齐原文的字符边界（切片 panic）；
    // ASCII 小写化逐字符保长度，索引天然一致。
    let lower: String = content.chars().map(|c| c.to_ascii_lowercase()).collect();
    for dir in ["media/", "uploads/", "output/"] {
        let mut from = 0usize;
        while let Some(found) = lower[from..].find(dir) {
            let abs = from + found;
            // 目录前缀必须是路径边界（不在更长单词中间）。
            let boundary_ok = abs == 0
                || !content[..abs]
                    .chars()
                    .next_back()
                    .is_some_and(|ch| ch.is_alphanumeric() || ch == '/' || ch == '_');
            if boundary_ok {
                let tail: String = content[abs..]
                    .chars()
                    .take_while(|ch| {
                        ch.is_alphanumeric()
                            || matches!(ch, '/' | '_' | '-' | '.' | '(' | ')' | ' ')
                            || ('\u{4e00}'..='\u{9fff}').contains(ch)
                            || ('\u{ff00}'..='\u{ffef}').contains(ch)
                    })
                    .collect();
                push(&tail);
            }
            from = abs + dir.len();
        }
    }
    paths
}

/// 从多条消息提取图片工作区路径：条内 + 跨消息去重保序，截到
/// `EXPORT_IMAGE_MAX_COUNT`（docx 的内嵌数量上限）。
pub fn extract_image_paths(contents: &[String]) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for content in contents {
        for rel in extract_image_paths_from_content(content) {
            if !paths.iter().any(|p| p == &rel) {
                paths.push(rel);
            }
        }
    }
    paths.truncate(EXPORT_IMAGE_MAX_COUNT);
    paths
}

/// 解析 PNG/JPEG/GIF/BMP 头部的像素宽高（避免引入解码库；仅用于排版比例）。
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if bytes.len() >= 10 && (bytes[..3] == *b"GIF") {
        let w = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let h = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return Some((w, h));
    }
    // JPEG：扫描 SOF 段取高宽。
    if bytes.len() > 4 && bytes[0] == 0xff && bytes[1] == 0xd8 {
        let mut i = 2usize;
        while i + 9 < bytes.len() {
            if bytes[i] != 0xff {
                i += 1;
                continue;
            }
            let marker = bytes[i + 1];
            if marker == 0xd8 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
                i += 2;
                continue;
            }
            let len = u16::from_be_bytes(bytes[i + 2..i + 4].try_into().ok()?) as usize;
            if matches!(marker, 0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc9 | 0xca | 0xcb) {
                let h = u16::from_be_bytes(bytes[i + 5..i + 7].try_into().ok()?) as u32;
                let w = u16::from_be_bytes(bytes[i + 7..i + 9].try_into().ok()?) as u32;
                return Some((w, h));
            }
            if len < 2 {
                return None;
            }
            i += 2 + len;
        }
    }
    None
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 渲染 Markdown 文本文档。
pub fn render_markdown(title: &str, stamp: &str, messages: &[ExportMessage]) -> String {
    let mut out = format!("# {title}\n\n> RClaw 会话导出 · {stamp}\n\n---\n\n");
    for msg in messages {
        let who = match msg.role.as_str() {
            "user" => "用户",
            "assistant" => "助手",
            "thinking" => "思考过程",
            _ => "工具",
        };
        let body = strip_media_control_lines(&msg.content).trim().to_string();
        out.push_str(&format!(
            "## {who}\n\n{}\n\n",
            if body.is_empty() {
                "（媒体消息）"
            } else {
                &body
            }
        ));
    }
    out
}

/// 内嵌图片限制（docx/html 共用口径）：单张 10MB、去重后总数 200 张
/// （导出文档不是媒体库；200 是天花板不是目标，docx 满配极端场景可能
/// 内存吃紧——验收时按「极端场景可能失败」接受）。
const DOCX_IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;
const EXPORT_IMAGE_MAX_COUNT: usize = 200;

/// HTML 图片内嵌总预算（原始字节）：单文件要能在浏览器里打开，base64
/// 膨胀 4/3 后 ~107MB 已是单文件体积上限（打开/转发可能较慢，验收按此
/// 措辞）；超预算占位不中断。
const HTML_IMAGE_TOTAL_MAX_BYTES: u64 = 80 * 1024 * 1024;
/// 内嵌原始字节超过该值时在文档头提示「文件较大，打开可能较慢」。
const HTML_BIG_FILE_HINT_BYTES: u64 = 20 * 1024 * 1024;

struct DocxImage {
    rel_id: String,
    file_name: String,
    bytes: Vec<u8>,
    /// 像素宽高（未知时用默认比例）。
    dims: Option<(u32, u32)>,
}

struct ExportImageFile {
    /// canonicalize 后的真实路径（docx 据此取扩展名——rel 可能是 symlink）。
    canonical: PathBuf,
    bytes: Vec<u8>,
}

/// 路径形状 + 扩展名白名单校验（不碰文件系统）：loader 与 html 预算判断
/// 共用的前置闸——安全拒绝必须先于任何 fs 访问。绝对路径（含 Windows
/// 盘符 `C:/…` 与根目录 `/…`）一并拒绝，防止 metadata 阶段 stat 工作区外
/// 文件泄露存在性/大小。盘符前缀跨平台拒绝（首字符 ASCII 字母 + 第二
/// 字符 `:`）：本产品工作区只在 Windows/安卓存在，盘符路径必是越界引用。
fn validate_image_rel(rel: &str) -> Result<(), String> {
    use std::path::Component;
    let bytes = rel.as_bytes();
    if rel.contains("..")
        || rel.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || Path::new(rel).is_absolute()
        || matches!(
            Path::new(rel).components().next(),
            Some(Component::Prefix(_)) | Some(Component::RootDir)
        )
    {
        return Err("路径越界".into());
    }
    let ext = rel
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "bmp") {
        return Err("扩展名不在图片白名单".into());
    }
    Ok(())
}

/// 路径闸门 + 真实路径解析：validate → canonicalize → 必须仍在 agent 根内
/// （工作区内指向外部的 symlink 在这里被拦下）。预扫 / 预算判断 / loader
/// 共用——此后才允许对该路径做任何 fs 访问。
fn resolve_export_image(root_canon: &Path, rel: &str) -> Result<PathBuf, String> {
    validate_image_rel(rel)?;
    let canonical = root_canon
        .join(rel)
        .canonicalize()
        .map_err(|e| format!("文件不存在: {e}"))?;
    if !canonical.starts_with(root_canon) {
        return Err("路径越出工作区".into());
    }
    Ok(canonical)
}

/// 按工作区相对路径读取一张待内嵌图片（docx/html 共用）：resolve（形状/
/// 白名单/canonicalize/根内）→ 先查元数据（单张大小上限在读取前拒绝）→
/// 读 bytes。任何一步失败都返回 Err，由调用方决定占位或跳过。
fn load_export_image_file(
    root_canon: &Path,
    rel: &str,
    max_bytes: u64,
) -> Result<ExportImageFile, String> {
    let canonical = resolve_export_image(root_canon, rel)?;
    read_export_image(canonical, max_bytes)
}

/// 读取已解析（canonical、根内）的图片：元数据校验 + 读 bytes。
fn read_export_image(canonical: PathBuf, max_bytes: u64) -> Result<ExportImageFile, String> {
    let meta = canonical
        .metadata()
        .map_err(|e| format!("读取元数据失败: {e}"))?;
    if !meta.is_file() {
        return Err("不是常规文件".into());
    }
    if meta.len() > max_bytes {
        return Err("超过单张大小上限".into());
    }
    let bytes = std::fs::read(&canonical).map_err(|e| format!("读取失败: {e}"))?;
    Ok(ExportImageFile { canonical, bytes })
}

/// 渲染 DOCX（zip 包）。图片从 agent_root 按工作区相对路径读取并内嵌。
pub fn render_docx(
    title: &str,
    stamp: &str,
    messages: &[ExportMessage],
    agent_root: &Path,
) -> Result<Vec<u8>, String> {
    let contents: Vec<String> = messages.iter().map(|m| m.content.clone()).collect();
    let image_paths = extract_image_paths(&contents);
    let root_canon = agent_root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    let mut images: Vec<DocxImage> = Vec::new();
    for (idx, rel) in image_paths.iter().enumerate() {
        let Ok(file) = load_export_image_file(&root_canon, rel, DOCX_IMAGE_MAX_BYTES) else {
            continue;
        };
        // 扩展名取 canonical 真实路径（rel 可能是 symlink，字节属于目标文件）。
        let ext = file
            .canonical
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .unwrap_or_else(|| "png".into());
        let ext = if ext == "jpeg" {
            "jpg".to_string()
        } else {
            ext
        };
        images.push(DocxImage {
            rel_id: format!("rIdImg{}", idx + 1),
            file_name: format!("image{}.{}", idx + 1, ext),
            dims: image_dimensions(&file.bytes),
            bytes: file.bytes,
        });
    }

    let mut body = String::new();
    // 标题 + 导出说明。
    body.push_str(&text_paragraph(&xml_escape(title), true, 28));
    body.push_str(&text_paragraph(
        &xml_escape(&format!("RClaw 会话导出 · {stamp}")),
        false,
        18,
    ));
    for msg in messages {
        let who = match msg.role.as_str() {
            "user" => "用户",
            "assistant" => "助手",
            "thinking" => "思考过程",
            _ => "工具",
        };
        body.push_str(&text_paragraph(&xml_escape(who), true, 22));
        let text = strip_media_control_lines(&msg.content);
        for line in text.lines() {
            let t = line.trim_end();
            if t.is_empty() {
                body.push_str("<w:p/>");
            } else {
                body.push_str(&text_paragraph(&xml_escape(t), false, 21));
            }
        }
    }
    // 图片统一追加到文末（正文位置与图片的对应关系由阅读顺序呈现）。
    for (idx, image) in images.iter().enumerate() {
        body.push_str(&image_paragraph(image, idx as i32 + 1));
    }
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<w:body>{body}\
<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/>\
<w:pgMar w:top=\"1440\" w:right=\"1440\" w:bottom=\"1440\" w:left=\"1440\" w:header=\"720\" w:footer=\"720\" w:gutter=\"0\"/></w:sectPr>\
</w:body></w:document>"
    );

    let content_types = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Default Extension=\"png\" ContentType=\"image/png\"/>\
<Default Extension=\"jpg\" ContentType=\"image/jpeg\"/>\
<Default Extension=\"gif\" ContentType=\"image/gif\"/>\
<Default Extension=\"bmp\" ContentType=\"image/bmp\"/>\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/>\
</Types>";

    let root_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rIdDoc\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/>\
</Relationships>";

    let image_rels: String = images
        .iter()
        .map(|img| {
            format!(
                "<Relationship Id=\"{id}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/{name}\"/>",
                id = img.rel_id,
                name = img.file_name
            )
        })
        .collect();
    let doc_rels = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{image_rels}</Relationships>"
    );

    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options: zip::write::SimpleFileOptions = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        zip.start_file("[Content_Types].xml", options)
            .map_err(|e| format!("打包 docx 失败: {e}"))?;
        zip.write_all(content_types.as_bytes())
            .map_err(|e| format!("打包 docx 失败: {e}"))?;
        zip.start_file("_rels/.rels", options)
            .map_err(|e| format!("打包 docx 失败: {e}"))?;
        zip.write_all(root_rels.as_bytes())
            .map_err(|e| format!("打包 docx 失败: {e}"))?;
        zip.start_file("word/document.xml", options)
            .map_err(|e| format!("打包 docx 失败: {e}"))?;
        zip.write_all(document.as_bytes())
            .map_err(|e| format!("打包 docx 失败: {e}"))?;
        if !images.is_empty() {
            zip.start_file("word/_rels/document.xml.rels", options)
                .map_err(|e| format!("打包 docx 失败: {e}"))?;
            zip.write_all(doc_rels.as_bytes())
                .map_err(|e| format!("打包 docx 失败: {e}"))?;
        }
        for image in &images {
            zip.start_file(format!("word/media/{}", image.file_name), options)
                .map_err(|e| format!("打包 docx 失败: {e}"))?;
            zip.write_all(&image.bytes)
                .map_err(|e| format!("打包 docx 失败: {e}"))?;
        }
        zip.finish().map_err(|e| format!("打包 docx 失败: {e}"))?;
    }
    Ok(buf.into_inner())
}

/// 文本段落（bold = 角色名/标题；size 半点）。
fn text_paragraph(text: &str, bold: bool, size_half_points: u32) -> String {
    let rpr = format!(
        "{}<w:sz w:val=\"{size}\"/>",
        if bold { "<w:b/>" } else { "" },
        size = size_half_points
    );
    format!(
        "<w:p><w:pPr><w:spacing w:after=\"100\"/></w:pPr><w:r><w:rPr>{rpr}</w:rPr><w:t xml:space=\"preserve\">{text}</w:t></w:r></w:p>"
    )
}

/// 图片段落：inline drawing，宽度上限 6.0 英寸（A4 内容宽），等比缩放。
fn image_paragraph(image: &DocxImage, doc_id: i32) -> String {
    // 96 DPI 下 1px = 9525 EMU；页宽 6 英寸 = 5,486,400 EMU。
    const EMU_PER_PX: i64 = 9525;
    const MAX_CX: i64 = 5_486_400;
    let (cx, cy) = match image.dims {
        Some((w, h)) if w > 0 && h > 0 => {
            let cx = (w as i64 * EMU_PER_PX).min(MAX_CX);
            let cy = (cx as f64 * h as f64 / w as f64) as i64;
            (cx, cy)
        }
        _ => (4_800_000, 3_200_000),
    };
    format!(
        "<w:p><w:pPr><w:spacing w:after=\"160\"/></w:pPr><w:r><w:drawing>\
<wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\">\
<wp:extent cx=\"{cx}\" cy=\"{cy}\"/>\
<wp:docPr id=\"{doc_id}\" name=\"Picture {doc_id}\"/>\
<a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic><pic:nvPicPr><pic:cNvPr id=\"{doc_id}\" name=\"{name}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"{rid}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm>\
<a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr></pic:pic>\
</a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>",
        name = xml_escape(&image.file_name),
        rid = image.rel_id
    )
}

/// HTML 文本/属性转义（比 xml_escape 多转单引号；先转 `&` 防双转义）。
/// 导出 HTML 会被转发给第三方查看，漏转义即存储型 XSS，所有插值必须经它。
/// 入口统一先剥控制字符（保留换行/制表）：summary/标题/路径里的 ANSI ESC、
/// NUL 等不可见字符不进产物。
fn html_escape(text: &str) -> String {
    let text = strip_control_chars(text);
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// 去掉 C0/C1 控制字符（保留换行/制表），避免不可见控制字符进 HTML 文本。
fn strip_control_chars(text: &str) -> String {
    text.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .collect()
}

/// 折叠摘要行：strip 协议行 → 剥控制字符 → 首个非空行 → 空白折叠 → 按
/// chars 截断加省略号（中文多字节安全；控制字符不占截断额度）。
fn html_summary_line(content: &str, max_chars: usize) -> String {
    let stripped = strip_control_chars(&strip_media_control_lines(content));
    let line = stripped
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let collapsed = line.split_whitespace().collect::<Vec<_>>().join(" ");
    let count = collapsed.chars().count();
    if count <= max_chars {
        collapsed
    } else {
        let mut out: String = collapsed.chars().take(max_chars).collect();
        out.push('…');
        out
    }
}

/// 按 magic bytes 识别图片 MIME：只认 PNG/JPEG/GIF/BMP 四种魔数，无法
/// 识别即拒绝（SVG 有脚本面，改名也不放行）。
fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        Some("image/png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF8") {
        Some("image/gif")
    } else if bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

/// 读文件头部（至多 4 字节）做魔数识别——预扫的可内增性判断与渲染的全量
/// sniff 走同一识别函数；`read`（非 read_exact）保证 2/3 字节魔数的短文件
/// （BMP/JPEG 最小头）两边口径一致。
fn file_magic_ok(path: &Path) -> bool {
    use std::io::Read as _;
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 4];
    let mut filled = 0;
    while filled < head.len() {
        match f.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return false,
        }
    }
    sniff_image_mime(&head[..filled]).is_some()
}

/// 一轮 = 一条 user 条目及其后的 assistant/thinking/status；首条 user 之前
/// 的归 preamble（不进导航）。与聊天页 RoundNav 同口径（连续 user 各算一轮）。
/// 段内分组与会话页 listBlocks 同启发式：最终回复 = 段内最后一个 assistant，
/// 其之前的行是工作过程（折叠容器内），之后的为随附行（容器外平铺）；段内
/// 无 thinking/status（无过程可折）时整体平铺不拆。
struct HtmlRound<'a> {
    preview: String,
    user: &'a ExportMessage,
    /// user 之后、最终回复之前的全部行（工作过程，折叠容器内；可能为空）。
    process: Vec<&'a ExportMessage>,
    /// 段内最后一个 assistant 条目（最终回复，容器外）。None = 整段平铺。
    final_reply: Option<&'a ExportMessage>,
    /// 最终回复之后的行（随附行，容器外平铺）。
    following: Vec<&'a ExportMessage>,
    /// 「已工作 X」秒数：首条过程行 → 最终回复的落库时间差（与会话页同口径
    /// 的展示跨度）；None = 无过程/时间戳缺失/跨度非正，显示「已处理」。
    work_secs: Option<i64>,
}

fn split_html_rounds(messages: &[ExportMessage]) -> (Vec<&ExportMessage>, Vec<HtmlRound<'_>>) {
    let mut preamble: Vec<&ExportMessage> = Vec::new();
    let mut rounds: Vec<HtmlRound> = Vec::new();
    for msg in messages {
        if msg.role == "user" {
            let preview = {
                let p = html_summary_line(&msg.content, 80);
                if p.is_empty() {
                    "（媒体消息）".to_string()
                } else {
                    p
                }
            };
            rounds.push(HtmlRound {
                preview,
                user: msg,
                process: Vec::new(),
                final_reply: None,
                following: Vec::new(),
                work_secs: None,
            });
        } else if let Some(round) = rounds.last_mut() {
            if round.final_reply.is_none() {
                round.process.push(msg);
            } else {
                round.following.push(msg);
            }
        } else {
            preamble.push(msg);
        }
    }
    // 倒序找段内最后一个 assistant 为最终回复；仅当段完整（多选导出未截断，
    // 由前端逐条 seg_complete 判定）且存在可折叠的过程行（thinking/status，
    // 与会话页 hasProcess+processRows 非空同条件）才拆段，否则整段平铺——
    // 选中子集的时间跨度会虚报「已工作 X」，还可能把中间 assistant 误判为
    // 最终回复。随后算工作时长跨度。
    for round in &mut rounds {
        let complete = std::iter::once(round.user)
            .chain(round.process.iter().copied())
            .all(|m| m.seg_complete);
        if !complete {
            continue;
        }
        let Some(fi) = round.process.iter().rposition(|m| m.role == "assistant") else {
            continue;
        };
        let has_process = round
            .process
            .iter()
            .any(|m| m.role == "status" || m.role == "thinking");
        if !has_process || fi == 0 {
            continue;
        }
        let final_reply = round.process.remove(fi);
        round.following.splice(0..0, round.process[fi..].to_vec());
        round.process.truncate(fi);
        round.final_reply = Some(final_reply);
        if let Some(first) = round.process.first() {
            let last_ts = round.final_reply.map(|m| m.ts).unwrap_or(0);
            if first.ts > 0 && last_ts > first.ts {
                round.work_secs = Some(last_ts - first.ts);
            }
        }
    }
    (preamble, rounds)
}

/// 「已工作 X」时长文案，与会话页 formatDuration 同口径：X 秒 / X 分 Y 秒。
fn format_work_duration(secs: i64) -> String {
    if secs < 60 {
        format!("{secs} 秒")
    } else {
        format!("{} 分 {} 秒", secs / 60, secs % 60)
    }
}

/// HTML 内联样式：浅/深色跟随系统；折叠、气泡、响应式导航（宽屏右侧固定
/// 栏 / 窄屏顶部 sticky chips）。const 而非 format! 拼接，避免大括号转义。
const HTML_CSS: &str = r#"
:root{--bg:#f6f7f9;--card:#fff;--ink:#1d2230;--muted:#5c6675;--line:#dfe3ea;--accent:#4f5bd5;--user:#e9ebfb;--code:#eef0f4;--danger:#c0392b}
@media (prefers-color-scheme:dark){:root{--bg:#0f1218;--card:#171b24;--ink:#e3e7ef;--muted:#9aa3b2;--line:#2a2f3a;--accent:#8b93f8;--user:#1f2536;--code:#1d222c;--danger:#e07060}}
*{box-sizing:border-box}
html{scroll-behavior:smooth}
@media (prefers-reduced-motion:reduce){html{scroll-behavior:auto}}
body{margin:0 auto;max-width:56rem;padding:1rem;background:var(--bg);color:var(--ink);font:15px/1.65 -apple-system,"Segoe UI","Microsoft YaHei",Roboto,sans-serif;overflow-wrap:anywhere;word-break:break-word}
.doc-head h1{margin:.2rem 0;font-size:1.35rem}
.doc-meta{margin:.2rem 0 .6rem;color:var(--muted);font-size:.8rem}
.rounds{position:sticky;top:0;z-index:2;display:flex;gap:.4rem;align-items:center;background:var(--bg);border-bottom:1px solid var(--line);padding:.45rem 0;overflow-x:auto}
.rounds-label{color:var(--muted);font-size:.75rem;flex:none}
.rounds a{flex:none;display:flex;gap:.35rem;align-items:center;max-width:11rem;text-decoration:none;color:var(--ink);background:var(--card);border:1px solid var(--line);border-radius:999px;padding:.15rem .6rem .15rem .45rem;font-size:.78rem}
.rounds a b{color:var(--accent)}
.rounds a span{overflow:hidden;white-space:nowrap;text-overflow:ellipsis}
.rounds a:focus-visible{outline:2px solid var(--accent);outline-offset:1px}
main{margin-top:.8rem}
.round{scroll-margin-top:3.6rem;border-left:3px solid transparent;padding-left:.6rem;margin-bottom:1.2rem}
.round:target{border-left-color:var(--accent)}
.round h2{font-size:.85rem;color:var(--muted);font-weight:600;margin:.2rem 0 .5rem}
.msg{margin:.6rem 0}
.msg .who{font-size:.72rem;color:var(--muted);margin-bottom:.15rem}
.msg .body{background:var(--card);border:1px solid var(--line);border-radius:.8rem;padding:.6rem .8rem;white-space:pre-wrap}
details{margin:.6rem 0;border:1px solid var(--line);border-radius:.8rem;background:var(--card)}
details>summary{cursor:pointer;padding:.45rem .8rem;font-size:.82rem;color:var(--muted);list-style:none;min-height:1.4rem}
details>summary::-webkit-details-marker{display:none}
details>summary::before{content:"\25B8\A0";color:var(--muted)}
details[open]>summary::before{content:"\25BE\A0"}
details>summary:focus-visible{outline:2px solid var(--accent);outline-offset:-2px;border-radius:.8rem}
details.thinking{border-style:dashed}
details.work>summary{font-weight:600}
.tool-call{margin:.45rem 0;padding:.45rem .6rem;border:1px solid var(--line);border-radius:.5rem}
.tool-call-head{margin:0 0 .25rem;font-size:.8rem;font-weight:600;color:var(--ink)}
.tool-skill,.tool-duration{margin-left:.4rem;font-size:.72rem;font-weight:400;color:var(--muted)}
.tool-error{margin-left:.4rem;font-size:.72rem;font-weight:400;color:var(--danger)}
.tool-field h5{margin:.4rem 0 .15rem;font-size:.72rem;font-weight:600;color:var(--muted)}
.tool-pre{margin:0;max-height:28rem;overflow:auto;white-space:pre-wrap;word-break:break-word;background:var(--code);border:1px solid var(--line);border-radius:.4rem;padding:.4rem .6rem;font-family:ui-monospace,Consolas,Menlo,monospace;font-size:.75rem}
.tool-empty{font-size:.75rem;color:var(--muted)}
.tool-note{color:var(--muted)}
details .body{margin:0 .8rem .6rem;white-space:pre-wrap}
details.tool .body{color:var(--muted)}
.media{margin:.5rem 0 0;scroll-margin-top:3.6rem}
.media img{max-width:100%;height:auto;border-radius:.5rem;border:1px solid var(--line);display:block}
.media figcaption{font-size:.72rem;color:var(--muted);margin-top:.15rem;overflow-wrap:anywhere}
.media-ref{display:inline-block;margin:.25rem .5rem 0 0}
a.media-back{font-size:.78rem;color:var(--accent)}
.media-note{font-size:.78rem;color:var(--muted)}
.media-skip{font-size:.78rem;color:var(--muted);border:1px dashed var(--line);border-radius:.5rem;padding:.35rem .6rem;margin-top:.4rem}
.doc-foot{margin:2rem 0 1rem;color:var(--muted);font-size:.75rem;border-top:1px solid var(--line);padding-top:.6rem}
@media (min-width:1100px){
body{padding-right:15.5rem}
.rounds{position:fixed;top:1rem;right:1rem;bottom:1rem;left:auto;width:13.5rem;flex-direction:column;align-items:stretch;border:1px solid var(--line);border-radius:.8rem;background:var(--card);padding:.5rem;overflow-y:auto;overflow-x:hidden}
.rounds a{max-width:none}
.round{scroll-margin-top:1rem}
.media{scroll-margin-top:1rem}
}
@media print{.rounds{display:none}}
/* 触屏：折叠标题与导航 chip 提到 44px 触控高度（桌面鼠标不受影响）。 */
@media (pointer:coarse){
details>summary{min-height:2.75rem;display:flex;align-items:center}
.rounds a{min-height:2.75rem}
}
"#;

/// HTML 单条消息的图片内嵌预算：跨消息去重 + 数量/总量上限 + 占位统计。
/// 同一未内嵌路径只在首次出现时占位一次（后续出现不再重复提示）。
struct HtmlImageBudget {
    root_canon: std::path::PathBuf,
    /// 内嵌总预算（原始字节）与单张上限（测试注入小值）。
    total_budget: u64,
    max_image_bytes: u64,
    embedded: usize,
    /// 已尝试且失败/超限/跳过的候选数（不含未尝试）。
    skipped: usize,
    /// 尝试上限触发后只计数未尝试的候选数（头部单独统计）。
    unattempted: usize,
    /// 已尝试（resolve+读）的候选数——与 embedded/skipped 独立计数，
    /// MAX_IMAGE_ATTEMPTS 封顶 fs 访问量。
    attempts: usize,
    total_bytes: u64,
    /// 已内嵌路径 → (锚点序号, 首次出现是否在非折叠 article 中)。
    embedded_rel: std::collections::HashMap<String, (usize, bool)>,
    /// 已给过占位的路径（含加载失败/超限），后续出现静默跳过。
    noted_rel: std::collections::HashSet<String>,
    /// 尝试上限的聚合提示只发一次。
    attempts_note_emitted: bool,
    /// 已渲染的工具详情文本量（头部「工具详情较大」提示用）。
    tool_text_bytes: usize,
}

impl HtmlImageBudget {
    fn new(root_canon: &Path, total_budget: u64, max_image_bytes: u64) -> Self {
        Self {
            root_canon: root_canon.to_path_buf(),
            total_budget,
            max_image_bytes,
            embedded: 0,
            skipped: 0,
            unattempted: 0,
            attempts: 0,
            total_bytes: 0,
            embedded_rel: std::collections::HashMap::new(),
            noted_rel: std::collections::HashSet::new(),
            attempts_note_emitted: false,
            tool_text_bytes: 0,
        }
    }

    /// 渲染一条消息的图片（在正文之后调用）。first_in_open_article：本条
    /// 消息是否渲染为非折叠的 article（user/assistant）——折叠块里的图
    /// 不能作为回链目标（锚点滚动不会展开 details）。
    fn render_images(&mut self, out: &mut String, content: &str, open_article: bool) {
        use std::fmt::Write as _;
        for rel in extract_image_paths_from_content(content) {
            if let Some(&(id, first_open)) = self.embedded_rel.get(&rel) {
                // 包一层 .media-ref：连续多个回链/注记不连排（CSS 有间距）。
                if first_open {
                    let _ = write!(
                        out,
                        "<span class=\"media-ref\"><a class=\"media-back\" href=\"#img-{id}\">📎 同图已在前文内嵌</a></span>"
                    );
                } else {
                    out.push_str(
                        "<span class=\"media-ref\"><span class=\"media-note\">（同图已在前文内嵌，位于折叠块中）</span></span>",
                    );
                }
                continue;
            }
            if self.noted_rel.contains(&rel) {
                continue;
            }
            // 尝试数上限（与内嵌数量上限分开）：fs 尝试达到
            // MAX_IMAGE_ATTEMPTS 后，新路径只计数不再做任何 fs 访问——
            // 防一条消息几千个坏路径打满 CPU/IO，又不会让前 200 个坏路径
            // 挡掉后续有效图（内嵌上限只由 embedded 把守）。首次触发输出
            // 一条聚合提示，头部统计始终准确。语法非法（validate 失败，无
            // fs）不耗尝试预算。
            if self.attempts >= MAX_IMAGE_ATTEMPTS {
                if !self.attempts_note_emitted {
                    self.attempts_note_emitted = true;
                    let _ = write!(
                        out,
                        "<div class=\"media-skip\">其余图片未尝试（已达尝试上限 {MAX_IMAGE_ATTEMPTS}）</div>"
                    );
                }
                self.unattempted += 1;
                self.noted_rel.insert(rel.clone());
                continue;
            }
            // 占位原因判断全部在读取之前：路径形状/白名单 → 内嵌数量上限 →
            // resolve（canonicalize + 根内，此后才允许 fs 访问）→ 元数据
            //（存在性 + 总量预算）。
            let canonical = match validate_image_rel(&rel) {
                Err(e) => {
                    self.skip(out, &rel, &e);
                    continue;
                }
                Ok(()) if self.embedded >= EXPORT_IMAGE_MAX_COUNT => {
                    let reason = format!("超出内嵌数量上限 {EXPORT_IMAGE_MAX_COUNT} 张");
                    self.skip(out, &rel, &reason);
                    continue;
                }
                Ok(()) => {
                    self.attempts += 1;
                    match resolve_export_image(&self.root_canon, &rel) {
                        Err(e) => {
                            self.skip(out, &rel, &e);
                            continue;
                        }
                        Ok(canonical) => canonical,
                    }
                }
            };
            match std::fs::metadata(&canonical) {
                Ok(meta) if meta.len() == 0 => {
                    self.skip(out, &rel, "空文件");
                    continue;
                }
                Ok(meta) if self.total_bytes.saturating_add(meta.len()) > self.total_budget => {
                    self.skip(out, &rel, "超出内嵌总量预算");
                    continue;
                }
                // metadata Err 不在此定因，交给读取步骤报真实错误。
                Ok(_) | Err(_) => {}
            }
            let file = match read_export_image(canonical, self.max_image_bytes) {
                Ok(f) => f,
                Err(e) => {
                    self.skip(out, &rel, &e);
                    continue;
                }
            };
            // MIME 只认 magic bytes：白名单扩展名但内容不对（改名 SVG/损坏
            // 文件）一律不放行——`<img>` 虽不执行脚本，坏图也无展示价值。
            let Some(mime) = sniff_image_mime(&file.bytes) else {
                self.skip(out, &rel, "图片格式未识别");
                continue;
            };
            self.embedded += 1;
            self.total_bytes += file.bytes.len() as u64;
            let id = self.embedded;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&file.bytes);
            let _ = write!(
                out,
                "<figure class=\"media\" id=\"img-{id}\"><img src=\"data:{mime};base64,{b64}\" alt=\"{}\" loading=\"lazy\" decoding=\"async\"><figcaption>{}</figcaption></figure>",
                html_escape(&rel),
                html_escape(&rel)
            );
            self.embedded_rel.insert(rel.clone(), (id, open_article));
        }
    }

    /// 记一次未内嵌：计数 + 去重集合 + 占位 div（超过 `SKIP_NOTE_MAX` 条
    /// 后只累计不再输出，头部统计仍准确）。
    fn skip(&mut self, out: &mut String, rel: &str, reason: &str) {
        use std::fmt::Write as _;
        self.skipped += 1;
        self.noted_rel.insert(rel.to_string());
        if self.noted_rel.len() <= SKIP_NOTE_MAX {
            let _ = write!(
                out,
                "<div class=\"media-skip\">图片未内嵌：{}（{}）</div>",
                html_escape(rel),
                html_escape(reason)
            );
        }
    }
}

/// 未内嵌占位 div 的渲染上限（超出后仅计数，头部统计兜底）。
const SKIP_NOTE_MAX: usize = 20;
/// 图片候选的 fs 尝试上限（成功+失败累计）：防坏路径刷屏打满 IO，又与
/// 内嵌数量上限（200，只由 embedded 把守）互不影响。
const MAX_IMAGE_ATTEMPTS: usize = 2000;

/// 「耗时」文案：毫秒 → 「X 毫秒 / X.X 秒 / X 分 Y 秒」。
fn format_tool_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms} 毫秒")
    } else if ms < 60_000 {
        // 59.5s 以上按整数秒显示，避免四舍五入出「60.0 秒」与下一档矛盾。
        if ms >= 59_500 {
            format!("{} 秒", ms / 1000)
        } else {
            format!("{:.1} 秒", ms as f64 / 1000.0)
        }
    } else {
        format_work_duration((ms / 1000) as i64)
    }
}

/// 渲染一个 status 条目的全量工具调用（section 平铺，不嵌套 details——
/// 外层 status 已折叠，三层交互太深）。所有动态值经 html_escape。
fn render_tool_details(out: &mut String, tools: &[ExportToolCall], budget: &mut HtmlImageBudget) {
    use std::fmt::Write as _;
    for call in tools {
        let _ = write!(
            out,
            "<section class=\"tool-call\"><h4 class=\"tool-call-head\"><span class=\"tool-name\">{}</span>",
            html_escape(&call.name)
        );
        if let Some(skill) = &call.skill {
            let _ = write!(out, "<span class=\"tool-skill\">🧩 {}</span>", html_escape(skill));
        }
        if let Some(ms) = call.duration_ms {
            let _ = write!(
                out,
                "<span class=\"tool-duration\">耗时 {}</span>",
                format_tool_duration(ms)
            );
        }
        if call.result.as_ref().is_some_and(|r| r.is_error) {
            out.push_str("<span class=\"tool-error\">❌ 出错</span>");
        }
        out.push_str("</h4>");
        if call.content_omitted {
            out.push_str(
                "<div class=\"tool-empty\">工具详情总量已达上限，本调用的入参/结果正文未展开</div></section>",
            );
            continue;
        }
        out.push_str("<div class=\"tool-field\"><h5>入参</h5><pre class=\"tool-pre\">");
        out.push_str(&html_escape(&call.input));
        out.push_str("</pre></div>");
        match &call.result {
            Some(result) => {
                out.push_str("<div class=\"tool-field\"><h5>结果</h5><pre class=\"tool-pre\">");
                out.push_str(&html_escape(&result.content));
                out.push_str("</pre></div>");
            }
            None => out.push_str(
                "<div class=\"tool-field\"><h5>结果</h5><div class=\"tool-empty\">（无结果记录）</div></div>",
            ),
        }
        if let Some(cp) = &call.checkpoint {
            let _ = write!(
                out,
                "<div class=\"tool-field\"><h5>文件写入前快照 · {}{}</h5>",
                html_escape(&cp.path),
                if cp.deleted { "（该次操作删除了文件）" } else { "" }
            );
            match &cp.before {
                Some(before) => {
                    out.push_str("<pre class=\"tool-pre\">");
                    out.push_str(&html_escape(before));
                    out.push_str("</pre>");
                }
                // 四象限：新建 / 已删除 / 普通快照 / 删除前快照。
                None if cp.deleted => {
                    out.push_str("<div class=\"tool-empty\">文件已删除（无写入前内容）</div>")
                }
                None => out.push_str("<div class=\"tool-empty\">新建文件（无写入前内容）</div>"),
            }
            out.push_str("</div>");
        }
        out.push_str("</section>");
        // 头部提示口径与预估一致：按转义后的近似渲染量累计（高转义内容
        // 原始字节远小于渲染量，只算原始会漏报）。
        budget.tool_text_bytes += html_escaped_len(&call.input)
            + html_escaped_len(&call.name)
            + call.skill.as_deref().map(html_escaped_len).unwrap_or(0)
            + call
                .result
                .as_ref()
                .map(|r| html_escaped_len(&r.content))
                .unwrap_or(0)
            + call
                .checkpoint
                .as_ref()
                .map(|c| {
                    html_escaped_len(&c.path)
                        + c.before.as_deref().map(html_escaped_len).unwrap_or(0)
                })
                .unwrap_or(0);
    }
}

/// 渲染一条消息的折叠/正文结构（图片在正文之后原位内嵌）。allow_backlink_target：
/// 该消息是否渲染在默认可见（非折叠容器内）的位置——折叠容器里的图不能作为
/// 重复图回链目标（锚点滚动不会展开 details）。
fn render_html_message(
    out: &mut String,
    msg: &ExportMessage,
    budget: &mut HtmlImageBudget,
    allow_backlink_target: bool,
) {
    use std::fmt::Write as _;
    let text = strip_control_chars(&strip_media_control_lines(&msg.content));
    let trimmed = text.trim();
    match msg.role.as_str() {
        "thinking" => {
            out.push_str("<details class=\"thinking\"><summary>💭 思考过程</summary>");
            let body = if trimmed.is_empty() { "（媒体消息）" } else { trimmed };
            let _ = write!(out, "<div class=\"body\">{}</div>", html_escape(body));
            budget.render_images(out, &msg.content, false);
            out.push_str("</details>");
        }
        "user" | "assistant" => {
            let (who, cls) = if msg.role == "user" {
                ("用户", "user")
            } else {
                ("助手", "assistant")
            };
            let _ = write!(out, "<article class=\"msg {cls}\"><div class=\"who\">{who}</div>");
            let body = if trimmed.is_empty() { "（媒体消息）" } else { trimmed };
            let _ = write!(out, "<div class=\"body\">{}</div>", html_escape(body));
            budget.render_images(out, &msg.content, allow_backlink_target);
            out.push_str("</article>");
        }
        // status（工具行）与其余未知角色：独立折叠行，summary 露出动词/目标。
        // 有全量工具详情时逐调用渲染；详情读取失败显示说明；都没有才回落到
        // 现状注记/原文。
        _ => {
            let summary = html_summary_line(&msg.content, 120);
            let summary = if summary.is_empty() { "🔧 工具调用".to_string() } else { summary };
            let _ = write!(
                out,
                "<details class=\"tool\"><summary>{}</summary>",
                html_escape(&summary)
            );
            if !msg.tools.is_empty() {
                render_tool_details(out, &msg.tools, budget);
            } else if let Some(err) = &msg.tool_detail_error {
                let _ = write!(
                    out,
                    "<div class=\"body tool-note\">{}</div>",
                    html_escape(err)
                );
            } else {
                let single_visible = !trimmed.is_empty()
                    && !trimmed.contains('\n')
                    && trimmed.chars().count() <= 120;
                if single_visible {
                    out.push_str(
                        "<div class=\"body\">导出数据仅含工具状态文案，完整入参/结果未包含</div>",
                    );
                } else {
                    let _ = write!(out, "<div class=\"body\">{}</div>", html_escape(trimmed));
                }
            }
            budget.render_images(out, &msg.content, false);
            out.push_str("</details>");
        }
    }
}

/// 渲染自包含单文件 HTML：`<details>` 折叠（thinking/status 均默认折叠，
/// 由用户点击展开）+ 锚点轮次导航 + 图片 base64 原位内嵌。零 JS、零
/// 外部资源；所有动态值经 html_escape。
///
/// 与 docx 不同：工作区不可用时**不整体报错**——图片逐张占位，正文照常
/// 导出（md 本就不依赖 fs；html 至少保住文本）。
pub fn render_html(
    title: &str,
    stamp: &str,
    messages: &[ExportMessage],
    agent_root: &Path,
) -> Result<String, String> {
    render_html_with_budget(
        title,
        stamp,
        messages,
        agent_root,
        HtmlBudgetPolicy {
            total_budget: HTML_IMAGE_TOTAL_MAX_BYTES,
            max_image_bytes: DOCX_IMAGE_MAX_BYTES,
            big_file_hint_bytes: HTML_BIG_FILE_HINT_BYTES,
            tool_big_hint_bytes: TOOL_BIG_HINT_BYTES,
        },
    )
}

/// 与 html_escape 一致的逐字符映射，直接算转义后的精确字节长度（容量
/// 预估用——`&`→5 字节、`"`→6 字节，×2 粗估会漏掉转义密集的内容）。
fn html_escaped_len(text: &str) -> usize {
    text.chars()
        .filter(|&c| c == '\n' || c == '\t' || !c.is_control())
        .map(|c| match c {
            '&' => 5,
            '<' | '>' => 4,
            '"' => 6,
            '\'' => 5,
            c => c.len_utf8(),
        })
        .sum()
}

/// 容量预扫：文字按转义后精确长度（status 另计 summary 的转义长度）+
/// 唯一候选图片按 base64 后 4/3。图片候选必须 resolve 成功（真实存在且
/// 根内）且可内嵌（非空/不超限/魔数合法）才计数——无效/缺失候选不占
/// 200 名额、不污染预估、不 stat 根外文件（symlink 越界在 resolve 被
/// 拦）。fs 尝试与 render_images 共用 MAX_IMAGE_ATTEMPTS 预算。返回
/// (总预估, 计入的图片原始字节)。
fn estimate_html_capacity(
    messages: &[ExportMessage],
    root: &Path,
    total_budget: u64,
    max_image_bytes: u64,
) -> (usize, u64) {
    // 调用方可能传未 canonicalize 的路径（测试直传临时目录）——根内校验
    // 要求两侧同形，内部统一 canonicalize（与 render_html_with_budget 一致）。
    let root_canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let rounds = messages
        .iter()
        .filter(|m| m.role == "user")
        .count();
    let text_bytes: usize = messages
        .iter()
        .map(|m| {
            let base = html_escaped_len(&m.content) + 256;
            let base = if matches!(m.role.as_str(), "user" | "assistant" | "thinking") {
                base
            } else {
                // status/未知角色：summary 另占最多 120 字（高转义内容下
                // 不可忽略）。
                base + html_escaped_len(&html_summary_line(&m.content, 120)) + 128
            };
            // 工具详情文本（装载层已按 cap 截断，这里按实际长度计）+ 每调用
            // 标记开销——不纳入会让单缓冲「一次预留」失效，退回反复扩容。
            let tools: usize = m
                .tools
                .iter()
                .map(|t| {
                    if t.content_omitted {
                        128
                    } else {
                        html_escaped_len(&t.input)
                            + html_escaped_len(&t.name)
                            + t.skill.as_deref().map(html_escaped_len).unwrap_or(0)
                            + t.result
                                .as_ref()
                                .map(|r| html_escaped_len(&r.content))
                                .unwrap_or(0)
                            + t.checkpoint
                                .as_ref()
                                .map(|c| {
                                    html_escaped_len(&c.path)
                                        + c.before.as_deref().map(html_escaped_len).unwrap_or(0)
                                })
                                .unwrap_or(0)
                            + 512
                    }
                })
                .sum();
            base + tools
        })
        .sum();
    let mut image_bytes: u64 = 0;
    let mut seen = std::collections::HashSet::new();
    let mut counted = 0usize;
    // 尝试预算与 render_images 同口径（MAX_IMAGE_ATTEMPTS 封顶 fs 尝试）：
    // 预扫超限即停——既防海量坏路径把预扫卡住，也保证「尝试后才出现的
    // 候选两边都不计/不嵌」，容量不虚高。
    let mut attempts = 0usize;
    'outer: for msg in messages {
        for rel in extract_image_paths_from_content(&msg.content) {
            if counted >= EXPORT_IMAGE_MAX_COUNT || attempts >= MAX_IMAGE_ATTEMPTS {
                break 'outer;
            }
            if !seen.insert(rel.clone()) {
                continue;
            }
            // 语法非法（不触发 fs）不耗尝试预算，与渲染一致。
            if validate_image_rel(&rel).is_err() {
                continue;
            }
            attempts += 1;
            // 只把真正可内嵌的候选计入名额与容量：resolve 成功、常规文件、
            // 非空、不超单张上限、头部魔数合法。is_file/大小判断必须在
            // file_magic_ok（会 File::open）之前——FIFO/设备节点 open 会
            // 阻塞等待 writer，绝不能对它们执行。
            if let Ok(canonical) = resolve_export_image(&root_canon, &rel) {
                if let Ok(meta) = std::fs::metadata(&canonical) {
                    if meta.is_file() && meta.len() > 0 && meta.len() <= max_image_bytes {
                        if file_magic_ok(&canonical) {
                            counted += 1;
                            image_bytes += meta.len();
                        }
                    }
                }
            }
        }
        if counted >= EXPORT_IMAGE_MAX_COUNT || attempts >= MAX_IMAGE_ATTEMPTS {
            break 'outer;
        }
    }
    let image_est = image_bytes
        .min(total_budget)
        .saturating_mul(4)
        .saturating_div(3);
    let estimate = text_bytes
        .saturating_add(rounds.saturating_mul(1024))
        .saturating_add(image_est.min(usize::MAX as u64) as usize)
        .saturating_add(256 * 1024);
    (estimate, image_bytes)
}

/// 头部统计占位符：单缓冲流式渲染下，统计数字（内嵌张数）要等消息主体
/// 渲染完才知道，先占位最后回填。含控制字符 \u{1}，而正文经
/// strip_control_chars 不可能含它，find 必中自家 token。
const HTML_STATS_TOKEN: &str = "RCLAW\u{1}EXPORT_STATS\u{1}";

/// 测试可注入预算的渲染主体（生产入口 render_html 用常量）。
///
/// 全程只用一个 `out` 缓冲：先写头部（统计行放占位符）与导航，消息主体
/// 直接流入，最后回填统计——避免「body + out」双份完整 HTML 的内存峰值
/// （80MB 预算的产物 base64 后 ~107MB，双份就是 214MB）。容量按文字与
/// 候选图片体积预估一次到位，避免中途翻倍扩容的瞬时拷贝。
/// 渲染预算与提示阈值（生产走常量，测试注入小值）。
#[derive(Debug, Clone, Copy)]
struct HtmlBudgetPolicy {
    /// 图片内嵌总预算（原始字节）。
    total_budget: u64,
    /// 单张图片上限。
    max_image_bytes: u64,
    /// 图片总量超过该值 → 头部提示。
    big_file_hint_bytes: u64,
    /// 工具详情文本超过该值 → 头部提示。
    tool_big_hint_bytes: usize,
}

fn render_html_with_budget(
    title: &str,
    stamp: &str,
    messages: &[ExportMessage],
    agent_root: &Path,
    policy: HtmlBudgetPolicy,
) -> Result<String, String> {
    use std::fmt::Write as _;
    // 根不可用（canonicalize 失败）时用原路径兜底：逐图在 fs 访问处失败
    // 并占位，正文不丢。
    let root_canon = agent_root
        .canonicalize()
        .unwrap_or_else(|_| agent_root.to_path_buf());
    let (preamble, rounds) = split_html_rounds(messages);

    // 容量预估（预扫与实际内嵌同口径：只计 resolve 成功的候选）。
    let (estimate, _) =
        estimate_html_capacity(messages, &root_canon, policy.total_budget, policy.max_image_bytes);
    let mut out = String::with_capacity(estimate);

    out.push_str("<!DOCTYPE html>\n<html lang=\"zh-CN\">\n<head>\n<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<meta http-equiv=\"Content-Security-Policy\" content=\"default-src 'none'; img-src data:; style-src 'unsafe-inline'; base-uri 'none'; form-action 'none'\">\n");
    out.push_str("<meta name=\"color-scheme\" content=\"light dark\">\n");
    out.push_str("<link rel=\"icon\" href=\"data:,\">\n");
    let _ = write!(out, "<title>{}</title>\n", html_escape(title));
    let _ = write!(out, "<style>{}</style>\n</head>\n<body>\n", HTML_CSS);
    let _ = write!(out, "<header class=\"doc-head\"><h1>{}</h1>", html_escape(title));
    // 头部统计先占位，消息主体渲染完后回填（轮次/条目数已知，张数未知）。
    // 0 轮（没有 user 条目）时「N 轮」省略，与导航 ≥2 才渲染的口径一致。
    let rounds_label = if rounds.is_empty() {
        String::new()
    } else {
        format!(" · {} 轮", rounds.len())
    };
    let _ = write!(
        out,
        "<p class=\"doc-meta\">RClaw 会话导出 · {}{} · {} 条 · {HTML_STATS_TOKEN}</p></header>\n",
        html_escape(stamp),
        rounds_label,
        messages.len()
    );
    // 轮次导航：≥2 轮才渲染（与聊天页 RoundNav 一致）。
    if rounds.len() >= 2 {
        out.push_str("<nav class=\"rounds\" aria-label=\"轮次导航\"><span class=\"rounds-label\">轮次</span>");
        for (idx, round) in rounds.iter().enumerate() {
            let _ = write!(
                out,
                "<a href=\"#round-{}\"><b>{}</b><span>{}</span></a>",
                idx + 1,
                idx + 1,
                html_escape(&round.preview)
            );
        }
        out.push_str("</nav>\n");
    }
    out.push_str("<main>\n");

    // 消息主体直接流入 out（单缓冲，无第二份全文）。
    let mut budget = HtmlImageBudget::new(&root_canon, policy.total_budget, policy.max_image_bytes);
    if !preamble.is_empty() {
        out.push_str(
            "<section class=\"round\" id=\"preamble\" aria-label=\"会话开始\"><h2>会话开始</h2>",
        );
        for msg in &preamble {
            render_html_message(&mut out, msg, &mut budget, true);
        }
        out.push_str("</section>");
    }
    for (idx, round) in rounds.iter().enumerate() {
        let _ = write!(
            out,
            "<section class=\"round\" id=\"round-{}\" aria-label=\"第 {} 轮\"><h2>第 {} 轮</h2>",
            idx + 1,
            idx + 1,
            idx + 1
        );
        render_html_message(&mut out, round.user, &mut budget, true);
        if let Some(final_reply) = round.final_reply {
            // 工作过程折叠容器（会话页「已工作 X」段同款：默认收起，点开看
            // 思考/工具行/中间说明——内部各条仍是独立可折叠的 details）。
            // 容器内消息不做回链目标（折叠着，锚点滚不过来）。
            let label = match round.work_secs {
                Some(secs) => format!("已工作 {}", format_work_duration(secs)),
                None => "已处理".to_string(),
            };
            let _ = write!(
                out,
                "<details class=\"work\"><summary>{}</summary>",
                html_escape(&label)
            );
            for msg in &round.process {
                render_html_message(&mut out, msg, &mut budget, false);
            }
            out.push_str("</details>");
            // 最终回复容器外；其后的随附行平铺（与会话页同构）。
            render_html_message(&mut out, final_reply, &mut budget, true);
            for msg in &round.following {
                render_html_message(&mut out, msg, &mut budget, true);
            }
        } else {
            // 整段平铺：无 thinking/status 的纯问答轮、无最终回复的轮、或多选
            // 导出的不完整段（会话页 flat 口径——没有可折叠的过程就不渲染空
            // 容器，不完整段的时长会虚报故同样平铺）。
            for msg in &round.process {
                render_html_message(&mut out, msg, &mut budget, true);
            }
        }
        out.push_str("</section>");
    }
    out.push_str("</main>\n");

    // 回填头部统计 + 条件提示（大文件按验收措辞：体积上限，打开可能较慢）。
    // 未内嵌（尝试失败）与未尝试（触发尝试上限）分开统计。
    let mut meta = format!("内嵌 {} 张图片", budget.embedded);
    if budget.skipped > 0 {
        meta.push_str(&format!(" · {} 张未内嵌", budget.skipped));
    }
    if budget.unattempted > 0 {
        meta.push_str(&format!(" · {} 张未尝试", budget.unattempted));
    }
    if budget.total_bytes > policy.big_file_hint_bytes {
        meta.push_str(" · 文件较大，打开可能较慢");
    }
    if budget.tool_text_bytes > policy.tool_big_hint_bytes {
        meta.push_str(" · 工具详情较大，打开可能较慢");
    }
    let stats = html_escape(&meta);
    match out.find(HTML_STATS_TOKEN) {
        // replace_range 会对占位符之后的 nav+body 做一次整体搬移（~100MB 级
        // 产物 ≈ 数十 ms 一次性成本，远小于图片读取+base64 编码耗时）——
        // 比双缓冲持两份全文便宜一个量级，取舍接受。
        Some(pos) => out.replace_range(pos..pos + HTML_STATS_TOKEN.len(), &stats),
        // 占位符不变量被未来改动破坏时降级为错误，不 panic 在用户导出里。
        None => return Err("内部错误：统计占位符丢失".into()),
    };

    // 脚注按内嵌结果条件化（0 图/全内嵌/部分未内嵌各有文案）。全未内嵌
    // 不承诺「保留路径引用」——占位 20 条后不再列出路径，头部统计兜底。
    let failed_total = budget.skipped + budget.unattempted;
    let foot = match (budget.embedded, failed_total) {
        (_, 0) if budget.embedded > 0 => "由 RClaw 导出 · 图片已内嵌，可离线打开",
        (0, _) if failed_total > 0 => "由 RClaw 导出 · 图片未能内嵌",
        _ => "由 RClaw 导出",
    };
    let _ = write!(
        out,
        "<footer class=\"doc-foot\">{foot} · 折叠块在打印/PDF 中不会自动展开</footer>\n</body>\n</html>\n"
    );
    Ok(out)
}

/// 导出格式白名单：`chat_export_full` / `export_messages` 两个命令与
/// `write_export_file` 的 match 共用同一事实源，避免三处漂移。
pub fn is_supported_format(fmt: &str) -> bool {
    matches!(fmt, "md" | "docx" | "html")
}

/// 导出目标文件名（output/exports/ 下的相对路径）。
pub fn export_rel_path(title: &str, stamp_compact: &str, ext: &str) -> String {
    format!(
        "output/exports/{}-{}.{}",
        stamp_compact,
        safe_file_name(title),
        ext
    )
}

/// 解析导出相对路径为绝对路径（供命令层落盘；调用方保证 agent 根）。
pub fn export_abs_path(agent_root: &Path, rel: &str) -> PathBuf {
    agent_root.join(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn safe_file_name_keeps_cjk_and_drops_punctuation() {
        assert_eq!(safe_file_name("质疑 Rust，学习！"), "质疑_Rust_学习");
        assert_eq!(safe_file_name("///"), "export");
    }

    #[test]
    fn strip_media_control_lines_removes_markers_only() {
        let content = "正文\nMEDIA_MARKER=[:desktop-media type=image path=\"x\"]\n下一行";
        assert_eq!(strip_media_control_lines(content), "正文\n下一行");
    }

    #[test]
    fn extract_image_paths_dedupes_marker_and_inline() {
        let contents = vec![
            "看这张 media/out/abc/图.jpg 不错".to_string(),
            "MEDIA_MARKER=[:desktop-media type=image path=\"/data/user/0/a/files/agent/media/out/abc/图.jpg\" name=\"图.jpg\"]".to_string(),
            "视频 output/v.mp4 不算图片".to_string(),
        ];
        let paths = extract_image_paths(&contents);
        assert_eq!(paths, vec!["media/out/abc/图.jpg".to_string()]);
    }

    #[test]
    fn image_dimensions_reads_png_header() {
        // 8B 签名 + 长度/类型占位 + IHDR 宽高。
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        bytes.extend_from_slice(&[0, 0, 0, 13]); // IHDR 长度
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&100u32.to_be_bytes());
        bytes.extend_from_slice(&50u32.to_be_bytes());
        assert_eq!(image_dimensions(&bytes), Some((100, 50)));
    }

    #[test]
    fn markdown_renders_roles() {
        let md = render_markdown(
            "标题",
            "2026-10-03 22:00",
            &[
                ExportMessage {
                    role: "user".into(),
                    content: "你好".into(),
                    ts: 0,
                    seg_complete: true,
                    msg_id: 0,
                    tools: Vec::new(),
                    tool_detail_error: None,
                },
                ExportMessage {
                    role: "assistant".into(),
                    content: "MEDIA_MARKER=[:x-media type=image path=\"p\"]\n图如下".into(),
                    ts: 0,
                    seg_complete: true,
                    msg_id: 0,
                    tools: Vec::new(),
                    tool_detail_error: None,
                },
                ExportMessage {
                    role: "thinking".into(),
                    content: "先列出目录".into(),
                    ts: 0,
                    seg_complete: true,
                    msg_id: 0,
                    tools: Vec::new(),
                    tool_detail_error: None,
                },
                ExportMessage {
                    role: "status".into(),
                    content: "🔧 调用工具: list output".into(),
                    ts: 0,
                    seg_complete: true,
                    msg_id: 0,
                    tools: Vec::new(),
                    tool_detail_error: None,
                },
            ],
        );
        assert!(md.contains("## 用户"));
        assert!(md.contains("## 助手"));
        assert!(md.contains("## 思考过程"));
        assert!(md.contains("## 工具"));
        assert!(md.contains("图如下"));
        assert!(!md.contains("MEDIA_MARKER"));
    }

    #[test]
    fn docx_zip_is_wellformed_and_carries_images() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media")).unwrap();
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&10u32.to_be_bytes());
        png.extend_from_slice(&20u32.to_be_bytes());
        std::fs::write(dir.path().join("media/a.png"), &png).unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "看 media/a.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let bytes = render_docx("标题", "2026-10-03", &messages, dir.path()).unwrap();
        let cursor = std::io::Cursor::new(&bytes);
        let mut zip = zip::ZipArchive::new(cursor).unwrap();
        let names: Vec<String> = zip.file_names().map(str::to_string).collect();
        assert!(names.contains(&"[Content_Types].xml".to_string()));
        assert!(names.contains(&"word/document.xml".to_string()));
        assert!(names.contains(&"word/media/image1.png".to_string()));
        let mut doc = String::new();
        zip.by_name("word/document.xml")
            .unwrap()
            .read_to_string(&mut doc)
            .unwrap();
        assert!(doc.contains("用户"));
        assert!(doc.contains("rIdImg1"));
        assert!(doc.contains("cx=\"95250\"")); // 10px * 9525
    }

    #[test]
    fn extract_image_paths_respects_export_count_cap() {
        // 250 条内联路径（路径置于行尾：内联提取会吃进尾部空格与文字）→
        // 去重后截到 EXPORT_IMAGE_MAX_COUNT（200）。
        let contents: Vec<String> = (0..250)
            .map(|i| format!("图 media/out/{i}.png"))
            .collect();
        let paths = extract_image_paths(&contents);
        assert_eq!(paths.len(), EXPORT_IMAGE_MAX_COUNT);
    }

    // ── render_html ──

    /// 写一张最小 PNG（IHDR 头含 10×20 像素）到 agent 根下相对路径。
    fn write_png(root: &Path, rel: &str) {
        let full = root.join(rel);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        png.extend_from_slice(&[0, 0, 0, 13]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&10u32.to_be_bytes());
        png.extend_from_slice(&20u32.to_be_bytes());
        std::fs::write(full, &png).unwrap();
    }

    #[test]
    fn html_escapes_dangerous_content() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "<script>alert(1)</script>".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "\"><img src=x onerror=alert(2)>".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("标\"题", "2026-11-01", &messages, Path::new("/nonexistent"))
            .unwrap();
        // 原始标签/属性不出现；onerror 文本作为已转义正文存活是安全预期。
        assert!(!html.contains("<script>"));
        assert!(!html.contains("<img"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&lt;img"));
        assert!(html.contains("&quot;"));
    }

    #[test]
    fn html_thinking_and_status_default_collapsed_with_summary() {
        let messages = vec![
            ExportMessage {
                role: "thinking".into(),
                content: "先列目录".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "🔧 调用工具: read".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "多行状态\n第二行详情".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // thinking 与 status 一致默认折叠（用户指令），无 open 属性。
        assert!(html.contains("<details class=\"thinking\">"));
        assert!(!html.contains("details class=\"thinking\" open"));
        assert!(html.contains("<details class=\"tool\"><summary>🔧 调用工具: read</summary>"));
        // 单行 status 与 summary 全同 → body 是弱化注记而非重复原文。
        assert!(html.contains("导出数据仅含工具状态文案，完整入参/结果未包含"));
        // 多行 status → body 展开完整原文。
        assert!(html.contains("第二行详情"));
        // 不伪造思考时长。
        assert!(!html.contains("持续了"));
    }

    #[test]
    fn html_round_nav_links_match_sections() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "第一轮的问题".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "回答一".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "user".into(),
                content: "第二轮的问题".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(html.contains("<nav class=\"rounds\""));
        assert!(html.contains("href=\"#round-1\""));
        assert!(html.contains("href=\"#round-2\""));
        assert!(html.contains("id=\"round-1\""));
        assert!(html.contains("id=\"round-2\""));
        assert_eq!(html.matches("href=\"#round-").count(), 2);
        assert!(html.contains("第一轮的问题"));
    }

    #[test]
    fn html_single_round_has_no_nav() {
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "只有一个问题".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(!html.contains("<nav class=\"rounds\""));
        assert!(html.contains("id=\"round-1\""));
    }

    #[test]
    fn html_preamble_and_consecutive_users() {
        let messages = vec![
            ExportMessage {
                role: "assistant".into(),
                content: "开场白（首条 user 之前）".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "user".into(),
                content: "问一".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "user".into(),
                content: "问二".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 首条 user 之前的 assistant 归 preamble，不进导航锚点。
        assert!(html.contains("id=\"preamble\""));
        assert!(html.contains("开场白（首条 user 之前）"));
        assert!(html.contains("id=\"round-1\""));
        assert!(html.contains("id=\"round-2\""));
        assert!(html.contains("问一"));
        assert!(html.contains("问二"));
    }

    #[test]
    fn html_embeds_image_in_place_from_marker_only_message() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/图.png");
        let messages = vec![ExportMessage {
            role: "assistant".into(),
            content: "MEDIA_MARKER=[:desktop-media type=image path=\"/data/user/0/a/files/agent/media/out/图.png\" name=\"图.png\"]".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        // strip 后正文为空 → （媒体消息）占位；图片仍在原位内嵌。
        assert!(html.contains("（媒体消息）"));
        assert!(html.contains("data:image/png;base64,"));
        assert!(html.contains("id=\"img-1\""));
        // 头部统计。
        assert!(html.contains("内嵌 1 张图片"));
    }

    #[test]
    fn html_dedupes_repeat_with_backlink() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/a.png");
        let marker = "MEDIA_MARKER=[:desktop-media type=image path=\"/agent/media/out/a.png\"]";
        let messages = vec![
            ExportMessage {
                role: "assistant".into(),
                content: marker.to_string(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: format!("{marker}\n再次引用"),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert_eq!(html.matches("data:image/png;base64,").count(), 1);
        assert!(html.contains("href=\"#img-1\""));
        assert!(html.contains("同图已在前文内嵌"));
    }

    #[test]
    fn html_skips_path_traversal_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media")).unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "看 media/../../secret.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(!html.contains("data:image"));
        assert!(html.contains("media-skip"));
        assert!(html.contains("路径越界"));
        assert!(html.contains("1 张未内嵌"));
    }

    #[test]
    fn html_total_budget_stops_and_notes() {
        // 总预算走内部函数注入小值（80MB 常量在测试里不可行）。
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/a.png");
        write_png(dir.path(), "media/out/b.png");
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "图一 media/out/a.png".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "user".into(),
                content: "图二 media/out/b.png".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html_with_budget(
            "t",
            "2026-11-01",
            &messages,
            dir.path(),
            HtmlBudgetPolicy {
                total_budget: 24,
                max_image_bytes: u64::MAX,
                big_file_hint_bytes: u64::MAX,
                tool_big_hint_bytes: usize::MAX,
            },
        )
        .unwrap();
        // 第一张内嵌；第二张超总量预算 → 占位 + 未内嵌计数。
        assert_eq!(html.matches("data:image/png;base64,").count(), 1);
        assert!(html.contains("超出内嵌总量预算"));
        assert!(html.contains("1 张未内嵌"));
    }

    #[test]
    fn html_summary_truncates_multibyte() {
        let long = "汉".repeat(300);
        let s = html_summary_line(&long, 80);
        assert_eq!(s.chars().count(), 81); // 80 + 省略号
        assert!(s.ends_with('…'));
        // 取首个非空行 + 行内空白折叠。
        assert_eq!(html_summary_line("  a  \n第二行", 80), "a");
        assert_eq!(html_summary_line("a\nb", 80), "a");
    }

    #[test]
    fn html_head_charset_csp_doctype_and_footer_note() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "q1".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "user".into(),
                content: "q2".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        assert!(html.contains("<meta charset=\"utf-8\">"));
        assert!(html.contains("Content-Security-Policy"));
        assert!(html.contains("lang=\"zh-CN\""));
        assert!(html.contains("折叠块在打印/PDF 中不会自动展开"));
    }

    #[test]
    fn html_oversize_image_skipped() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/big.png");
        // 把单张上限压到 1 字节：loader 在读取前按元数据拒绝。
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "看 media/out/big.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html_with_budget(
            "t",
            "2026-11-01",
            &messages,
            dir.path(),
            HtmlBudgetPolicy {
                total_budget: u64::MAX,
                max_image_bytes: 1,
                big_file_hint_bytes: u64::MAX,
                tool_big_hint_bytes: usize::MAX,
            },
        )
        .unwrap();
        assert!(html.contains("超过单张大小上限"));
        assert!(!html.contains("data:image"));
    }

    #[test]
    fn sniff_image_mime_recognizes_magic_and_rejects_svg() {
        assert_eq!(sniff_image_mime(&[0x89, b'P', b'N', b'G']), Some("image/png"));
        assert_eq!(sniff_image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image_mime(b"GIF89a"), Some("image/gif"));
        assert_eq!(sniff_image_mime(b"BM\x00\x00"), Some("image/bmp"));
        // SVG 有脚本面，即使扩展名叫 .png 也拒绝。
        assert_eq!(sniff_image_mime(b"<svg xmlns="), None);
    }

    #[test]
    fn html_big_file_hint_when_over_threshold() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/a.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "图 media/out/a.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 提示阈值注入 1 字节：24 字节的图必触发。
        let html = render_html_with_budget(
            "t",
            "2026-11-01",
            &messages,
            dir.path(),
            HtmlBudgetPolicy {
                total_budget: u64::MAX,
                max_image_bytes: u64::MAX,
                big_file_hint_bytes: 1,
                tool_big_hint_bytes: usize::MAX,
            },
        )
        .unwrap();
        assert!(html.contains("文件较大，打开可能较慢"));
        // 阈值放宽后不出现。
        let html2 = render_html_with_budget(
            "t",
            "2026-11-01",
            &messages,
            dir.path(),
            HtmlBudgetPolicy {
                total_budget: u64::MAX,
                max_image_bytes: u64::MAX,
                big_file_hint_bytes: u64::MAX,
                tool_big_hint_bytes: usize::MAX,
            },
        )
        .unwrap();
        assert!(!html2.contains("文件较大"));
    }

    #[test]
    fn html_rejects_windows_absolute_path_before_fs_access() {
        let dir = tempfile::tempdir().unwrap();
        // 内联提取只认 media/uploads/output 前缀；绝对路径经 marker path=
        // 进入管线（回归的是 validate_image_rel 闸门本身）。
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "MEDIA_MARKER=[:x-media type=image path=\"C:/windows/evil.png\"]".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(!html.contains("data:image"));
        assert!(html.contains("路径越界"));
        // metadata 阶段不会 stat 工作区外文件：产物里只有占位，无大小信息。
        assert!(html.contains("1 张未内嵌"));
    }

    #[test]
    fn html_multiple_backlinks_are_individually_wrapped() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/a.png");
        let marker = "MEDIA_MARKER=[:x-media type=image path=\"/agent/media/out/a.png\"]";
        // 首条 assistant 内嵌；后续两条各出一个回链（跨消息重复，单条消息内
        // 提取器本身去重）。
        let messages = vec![
            ExportMessage {
                role: "assistant".into(),
                content: marker.to_string(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: format!("再看一次\n{marker}"),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: format!("最后一次\n{marker}"),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert_eq!(html.matches("data:image/png;base64,").count(), 1);
        assert_eq!(html.matches("<span class=\"media-ref\">").count(), 2);
        assert_eq!(html.matches("href=\"#img-1\"").count(), 2);
    }

    #[test]
    fn html_control_chars_never_reach_output() {
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "正文\u{1}带 ANSI\u{7}控制符".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(!html.contains('\u{1}'));
        assert!(!html.contains('\u{7}'));
        assert!(html.contains("正文带 ANSI控制符"));
    }

    #[test]
    fn html_footer_varies_with_embed_result() {
        // 无图：脚注不提图片内嵌。
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "纯文本".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(!html.contains("图片已内嵌"));
        assert!(!html.contains("图片未能内嵌"));
        // 全内嵌：含「图片已内嵌」。
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/a.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "图 media/out/a.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(html.contains("图片已内嵌，可离线打开"));
        // 全未内嵌：措辞不承诺「保留路径引用」（占位 20 条后路径不再列出）。
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "缺图 media/out/nope.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(html.contains("图片未能内嵌"));
        assert!(!html.contains("路径引用"));
        // 部分内嵌：头部同时有内嵌与未内嵌计数，脚注回落中性文案。
        let dir2 = tempfile::tempdir().unwrap();
        write_png(dir2.path(), "media/out/ok.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "好图 media/out/ok.png\n缺图 media/out/nope.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir2.path()).unwrap();
        assert!(html.contains("内嵌 1 张图片 · 1 张未内嵌"));
        assert!(!html.contains("图片已内嵌，可离线打开"));
        assert!(!html.contains("图片未能内嵌"));
    }

    #[test]
    fn html_skip_notes_capped_at_20_but_count_accurate() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media/out")).unwrap();
        let mut content = String::new();
        for i in 0..22 {
            content.push_str(&format!("缺图 media/out/missing{i}.png\n"));
        }
        let messages = vec![ExportMessage {
            role: "user".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        // 前 20 条占位，之后只累计：头部统计仍准确（22 张未内嵌）。
        // 注：匹配 div 开标签而非类名——<style> 里的 .media-skip 规则也会命中。
        assert_eq!(
            html.matches("<div class=\"media-skip\">").count(),
            20
        );
        assert!(html.contains("22 张未内嵌"));
    }

    #[test]
    fn html_empty_and_missing_files_report_distinct_reasons() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media/out")).unwrap();
        std::fs::write(dir.path().join("media/out/empty.png"), b"").unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "空图 media/out/empty.png\n缺图 media/out/nope.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(html.contains("（空文件）"));
        // metadata Err 不在此定因，交给 loader 报真实错误。
        assert!(html.contains("文件不存在"));
    }

    #[test]
    fn html_rejects_svg_content_in_png_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media/out")).unwrap();
        std::fs::write(dir.path().join("media/out/evil.png"), b"<svg xmlns='x'></svg>").unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "看 media/out/evil.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(!html.contains("data:image"));
        assert!(html.contains("图片格式未识别"));
        // 未内嵌恰好计一次（回归：曾因手动+skip() 双计数而偏大）。
        assert!(html.contains("1 张未内嵌"));
    }

    #[test]
    fn html_skipped_cap_does_not_block_later_valid_image() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/late.png");
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!("缺图 media/out/missing{i}.png\n"));
        }
        content.push_str("好图 media/out/late.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 200 个坏路径不占内嵌名额：后续有效图仍内嵌（尝试数上限 2000 与
        // 内嵌上限 200 是两回事）。
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert_eq!(html.matches("data:image/png;base64,").count(), 1);
        assert!(html.contains("内嵌 1 张图片"));
        assert!(html.contains("200 张未内嵌"));
    }

    #[test]
    fn estimate_ignores_empty_oversize_and_bad_magic_candidates() {
        use std::io::Write as _;
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/ok.png"); // 24 字节，有效
        write_png(dir.path(), "media/out/big.png"); // PNG 魔数 + 100 字节尾部 → 超 50 上限
        let mut big = std::fs::OpenOptions::new()
            .append(true)
            .open(dir.path().join("media/out/big.png"))
            .unwrap();
        big.write_all(&[0u8; 100]).unwrap();
        drop(big);
        std::fs::write(dir.path().join("media/out/empty.png"), b"").unwrap();
        std::fs::write(dir.path().join("media/out/bad.png"), b"<svg xmlns='x'/>").unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "ok media/out/ok.png\nbig media/out/big.png\nempty media/out/empty.png\nbad media/out/bad.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 单张上限 50：big 超限 → 只有 ok 可内嵌。
        let (_, image_bytes) =
            estimate_html_capacity(&messages, dir.path(), u64::MAX, 50);
        assert_eq!(image_bytes, 24);
        // 放宽上限：big（合法 PNG 魔数）计入；empty/bad 仍被排除。
        let (_, image_bytes) =
            estimate_html_capacity(&messages, dir.path(), u64::MAX, u64::MAX);
        assert_eq!(image_bytes, 24 + 124);
    }

    #[test]
    fn estimate_sniffs_short_magic_heads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media/out")).unwrap();
        // 2/3 字节最小魔数头（BMP/JPEG）与 1 字节非图片。
        std::fs::write(dir.path().join("media/out/b.bmp"), b"BM").unwrap();
        std::fs::write(dir.path().join("media/out/j.jpg"), [0xFF, 0xD8, 0xFF]).unwrap();
        std::fs::write(dir.path().join("media/out/x.png"), b"P").unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "b media/out/b.bmp\nj media/out/j.jpg\nx media/out/x.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 预扫用「读至多 4 字节」口径，与渲染全量 sniff 一致：2/3 字节魔数
        // 可内嵌（计 2+3 字节），1 字节 x.png 不计。
        let (_, image_bytes) =
            estimate_html_capacity(&messages, dir.path(), u64::MAX, u64::MAX);
        assert_eq!(image_bytes, 5);
    }

    #[test]
    fn estimate_html_capacity_respects_attempts_cap() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/late.png");
        let mut content = String::new();
        for i in 0..2000 {
            content.push_str(&format!("缺图 media/out/missing{i}.png\n"));
        }
        content.push_str("好图 media/out/late.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 尝试预算被 2000 个缺失路径耗尽：预扫与渲染同口径，late.png 两边
        // 都不计/不嵌（文档化的产品取舍——超长坏路径列表截断处理）。
        let (_, image_bytes) =
            estimate_html_capacity(&messages, dir.path(), u64::MAX, u64::MAX);
        assert_eq!(image_bytes, 0);
    }

    #[test]
    fn html_attempts_cap_behavior_is_documented_by_test() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/late.png");
        let mut content = String::new();
        for i in 0..2000 {
            content.push_str(&format!("缺图 media/out/missing{i}.png\n"));
        }
        content.push_str("好图 media/out/late.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        // 尝试上限触发：有效图不再内嵌，聚合提示出现一次，头部把「未内嵌
        // （尝试失败 2000）」与「未尝试（1）」分开统计。
        assert!(!html.contains("data:image/png;base64,"));
        assert_eq!(
            html.matches("其余图片未尝试（已达尝试上限 2000）").count(),
            1
        );
        assert!(html.contains("内嵌 0 张图片 · 2000 张未内嵌 · 1 张未尝试"));
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_never_opened_or_counted() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("media/out")).unwrap();
        // FIFO：open 会阻塞等待 writer——预扫/渲染都不得对它 open。std
        // 没有 mkfifo（属于 libc/nix），走系统命令，失败（无 coreutils）就
        // 跳过 FIFO 部分只测目录——显式标记，避免覆盖静默降级。
        let fifo_path = dir.path().join("media/out/fifo.png");
        let have_fifo = std::process::Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !have_fifo {
            eprintln!("（special_files 测试：mkfifo 命令不可用，FIFO 分支跳过，仅覆盖目录）");
        }
        // 目录伪装 .png。
        std::fs::create_dir(dir.path().join("media/out/dir.png")).unwrap();
        let mut content = String::new();
        if have_fifo {
            content.push_str("f media/out/fifo.png\n");
        }
        content.push_str("d media/out/dir.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 预扫 + 渲染放进子线程并限时：若未来实现回归成先 open FIFO，
        // 这里会超时 panic（明确失败）而非挂死整个测试进程。
        let (tx, rx) = std::sync::mpsc::channel();
        let dirp = dir.path().to_path_buf();
        let handle = std::thread::spawn(move || {
            let est = estimate_html_capacity(&messages, &dirp, u64::MAX, u64::MAX);
            let html = render_html("t", "2026-11-01", &messages, &dirp).unwrap();
            tx.send((est, html)).expect("接收端未提前关闭");
        });
        let ((_, image_bytes), html) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| panic!("特殊文件处理挂起（可能对 FIFO 执行了 open）"));
        handle.join().unwrap();
        // 两者都不计入容量、不内嵌。
        assert_eq!(image_bytes, 0);
        assert!(!html.contains("data:image"));
        assert_eq!(
            html.matches("<div class=\"media-skip\">").count(),
            if have_fifo { 2 } else { 1 }
        );
        // 各自的占位原因：FIFO 大小为 0 → 「空文件」；目录 → 「不是常规
        // 文件」（read_export_image 的 is_file 闸）。
        if have_fifo {
            assert!(html.contains("media/out/fifo.png（空文件）"));
        }
        assert!(html.contains("media/out/dir.png（不是常规文件）"));
    }

    #[test]
    fn html_render_caps_at_200_unique_images() {
        let dir = tempfile::tempdir().unwrap();
        let mut content = String::new();
        for i in 0..201 {
            let rel = format!("media/out/{i}.png");
            write_png(dir.path(), &rel);
            content.push_str(&format!("图 {rel}。"));
        }
        let messages = vec![ExportMessage {
            role: "assistant".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert_eq!(html.matches("data:image/png;base64,").count(), 200);
        assert!(html.contains("超出内嵌数量上限 200 张"));
    }

    #[test]
    fn estimate_ignores_invalid_candidates_so_valid_images_still_count() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/ok.png"); // 24 字节
        let mut content = String::new();
        for i in 0..200 {
            content.push_str(&format!("缺图 media/out/missing{i}.png\n"));
        }
        content.push_str("好图 media/out/ok.png");
        let messages = vec![ExportMessage {
            role: "user".into(),
            content,
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 前 200 个缺失候选不占名额：有效图仍被计数并进入容量预估。
        let (_, image_bytes) =
            estimate_html_capacity(&messages, dir.path(), u64::MAX, u64::MAX);
        assert_eq!(image_bytes, 24);
        // 全无效时图片贡献为 0。
        let bad_only = vec![ExportMessage {
            role: "user".into(),
            content: "缺图 media/out/missing0.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let (_, image_bytes) =
            estimate_html_capacity(&bad_only, dir.path(), u64::MAX, u64::MAX);
        assert_eq!(image_bytes, 0);
    }

    #[test]
    fn html_work_process_folds_with_duration() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问题".into(),
                ts: 100,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "thinking".into(),
                content: "先列目录看看".into(),
                ts: 110,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "🔧 调用工具: read".into(),
                ts: 120,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "最终回答".into(),
                ts: 165,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 段容器默认收起；时长 = 首条过程行(110) → 最终回复(165) 的跨度。
        assert!(html.contains("<details class=\"work\"><summary>已工作 55 秒</summary>"));
        // 顺序：user → 容器内思考/工具 → 容器外最终回复；容器必须在最终
        // 回复前闭合（段内开合标签配对——若容器意外包住 final，开数会多于
        // 闭数，find 首个 </details> 的弱断言抓不住）。
        let work_open = html.find("<details class=\"work\"").unwrap();
        let think = html.find("先列目录看看").unwrap();
        let final_pos = html.find("最终回答").unwrap();
        assert!(work_open < think && think < final_pos);
        let seg = &html[work_open..final_pos];
        assert_eq!(
            seg.matches("<details").count(),
            seg.matches("</details>").count()
        );
    }

    #[test]
    fn html_work_flat_when_no_process_rows() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "答".into(),
                ts: 130,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 无 thinking/status 的纯问答轮：不渲染空容器（会话页 flat 口径）。
        assert!(!html.contains("<details class=\"work\""));
        assert!(html.contains("id=\"round-1\""));
    }

    #[test]
    fn html_work_duration_missing_ts_shows_processed() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "thinking".into(),
                content: "想".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "答".into(),
                ts: 0,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 时间戳缺失/跨度非正 → 会话页同 fallback「已处理」。
        assert!(html.contains("<details class=\"work\"><summary>已处理</summary>"));
    }

    #[test]
    fn html_intermediate_assistant_inside_process_and_following_outside() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "中间说明".into(),
                ts: 110,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "🔧 调用工具: shell".into(),
                ts: 120,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "最终回答".into(),
                ts: 150,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "🔧 调用工具: 事后".into(),
                ts: 160,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 中间说明在容器内；容器在最终回复前闭合（开合配对）；随附行在最后。
        let work_open = html.find("<details class=\"work\"").unwrap();
        let final_pos = html.find("最终回答").unwrap();
        assert!(html.find("中间说明").unwrap() > work_open);
        let seg = &html[work_open..final_pos];
        assert_eq!(
            seg.matches("<details").count(),
            seg.matches("</details>").count()
        );
        assert!(final_pos < html.find("事后").unwrap());
        // 时长从首条过程行（中间说明 110）到最终回复（150）。
        assert!(html.contains("已工作 40 秒"));
    }

    #[test]
    fn html_no_final_segment_flat() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "thinking".into(),
                content: "想的".into(),
                ts: 110,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "🔧 调用工具: read".into(),
                ts: 120,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        // 取消轮（无最终回复）：整段平铺，无容器，内容完整可见。
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(!html.contains("<details class=\"work\""));
        assert!(html.contains("想的") && html.contains("🔧 调用工具: read"));
    }

    #[test]
    fn html_preamble_with_process_flat() {
        let messages = vec![
            ExportMessage {
                role: "assistant".into(),
                content: "开场".into(),
                ts: 50,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "thinking".into(),
                content: "开场思考".into(),
                ts: 60,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "答".into(),
                ts: 130,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        // preamble（首条 user 之前）含 thinking 也不套 work 容器；round-1
        // 纯问答段同样平铺。
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(!html.contains("<details class=\"work\""));
        assert!(html.contains("id=\"preamble\"") && html.contains("开场思考"));
        assert!(html.contains("id=\"round-1\""));
    }

    #[test]
    fn html_mixed_seg_complete_flat() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: false,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "thinking".into(),
                content: "思考正文".into(),
                ts: 110,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "最终回答".into(),
                ts: 150,
                seg_complete: false,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        // 同段混用 true/false：.all 门槛 fail-closed → 平铺。
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(!html.contains("<details class=\"work\""));
        assert!(html.contains("思考正文") && html.contains("最终回答"));
    }

    #[test]
    fn html_partial_selection_renders_flat_without_duration() {
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: false,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "thinking".into(),
                content: "思考内容".into(),
                ts: 110,
                seg_complete: false,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: "最终回答".into(),
                ts: 150,
                seg_complete: false,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 多选不完整段：平铺、无「已工作」——选中子集的跨度会虚报。
        assert!(!html.contains("<details class=\"work\""));
        assert!(!html.contains("已工作"));
        // 内容一个不丢。
        assert!(html.contains("思考内容") && html.contains("最终回答"));
    }

    #[test]
    fn html_backlink_never_targets_folded_container() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "media/out/a.png");
        let marker = "MEDIA_MARKER=[:x-media type=image path=\"/agent/media/out/a.png\"]";
        let messages = vec![
            ExportMessage {
                role: "user".into(),
                content: "问".into(),
                ts: 100,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            // 中间说明带图（进折叠容器内）；最终回复引用同图。
            ExportMessage {
                role: "assistant".into(),
                content: format!("中间说明 {marker}"),
                ts: 110,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "status".into(),
                content: "🔧 调用工具: read".into(),
                ts: 120,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
            ExportMessage {
                role: "assistant".into(),
                content: format!("最终回答 {marker}"),
                ts: 150,
                seg_complete: true,
                msg_id: 0,
                tools: Vec::new(),
                tool_detail_error: None,
            },
        ];
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        // 首图在折叠容器内：同图回链必须降级为文本注记（锚点滚动不会展开
        // details，指向折叠 figure 的链接点了也看不到）。
        assert_eq!(html.matches("data:image/png;base64,").count(), 1);
        assert!(!html.contains("href=\"#img-"));
        assert!(html.contains("位于折叠块中"));
    }

    #[test]
    fn work_duration_format_matches_chat() {
        assert_eq!(format_work_duration(55), "55 秒");
        assert_eq!(format_work_duration(60), "1 分 0 秒");
        assert_eq!(format_work_duration(90), "1 分 30 秒");
    }

    #[test]
    fn html_tool_msg_id_zero_note_not_old_placeholder() {
        // 直播未落库（msg_id=0）：拍板的降级说明，不是旧「仅状态文案」注记。
        let messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 调用工具: read".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: Some("该工具行尚未落库，无法读取完整详情".into()),
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(html.contains("该工具行尚未落库，无法读取完整详情"));
        assert!(!html.contains("导出数据仅含工具状态文案"));
    }

    #[test]
    fn format_tool_duration_boundaries() {
        assert_eq!(format_tool_duration(999), "999 毫秒");
        assert_eq!(format_tool_duration(1500), "1.5 秒");
        // 59_999ms 四舍五入会出「60.0 秒」，与 1 分档矛盾 → 整数秒显示。
        assert_eq!(format_tool_duration(59_999), "59 秒");
        assert_eq!(format_tool_duration(59_500), "59 秒");
        assert_eq!(format_tool_duration(60_000), "1 分 0 秒");
    }

    #[test]
    fn format_bytes_small_values() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn html_tool_details_render_and_escape() {
        let messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 调用工具: read".into(),
            ts: 100,
            seg_complete: true,
            msg_id: 7,
            tools: vec![ExportToolCall {
                name: "read".into(),
                skill: Some("pdf-to-ppt".into()),
                input: "{\"path\":\"a</pre><script>alert(1)</script>\"}".into(),
                duration_ms: Some(1500),
                result: Some(ExportToolResult {
                    content: "输出 \"quoted\" & <b>html</b>".into(),
                    is_error: true,
                }),
                checkpoint: None,
                content_omitted: false,
            }],
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 全部动态值转义：script/引号/尖括号不穿透 pre。
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
        assert!(html.contains("&quot;quoted&quot;"));
        // 结构与元信息；checkpoint None → 快照块整块省略。
        assert!(html.contains("<section class=\"tool-call\">"));
        assert!(html.contains("🧩 pdf-to-ppt"));
        assert!(html.contains("耗时 1.5 秒"));
        assert!(html.contains("❌ 出错"));
        assert!(html.contains("<h5>入参</h5>") && html.contains("<h5>结果</h5>"));
        assert!(!html.contains("文件写入前快照"));
    }

    #[test]
    fn html_tool_checkpoint_three_states() {
        let mk = |before: Option<&str>, deleted: bool, path: &str| ExportToolCall {
            name: "write".into(),
            skill: None,
            input: "{}".into(),
            duration_ms: None,
            result: None,
            checkpoint: Some(ExportToolCheckpoint {
                path: path.to_string(),
                before: before.map(str::to_string),
                deleted,
            }),
            content_omitted: false,
        };
        let messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 调用工具: write".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: vec![
                mk(Some("旧内容"), false, "media/out/a.txt"),
                mk(None, false, "media/out/new.txt"),
                mk(None, true, "media/out/gone<b>.txt"),
            ],
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(html.contains("旧内容"));
        // 四象限各归各位：「新建」只出现一次（deleted+无 before 不能落进
        // 新建分支——回归曾经的矛盾文案）。
        assert_eq!(html.matches("新建文件（无写入前内容）").count(), 1);
        assert!(html.contains("文件已删除（无写入前内容）"));
        assert!(html.contains("（该次操作删除了文件）"));
        // checkpoint.path 转义。
        assert!(!html.contains("gone<b>.txt"));
        assert!(html.contains("gone&lt;b&gt;.txt"));
    }

    #[test]
    fn html_tool_detail_error_and_no_tools_notes() {
        let messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 调用工具: read".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: Some("读取工具详情失败: <script>alert(9)</script>".into()),
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        // 失败说明可见且转义。
        assert!(!html.contains("<script>alert(9)"));
        assert!(html.contains("读取工具详情失败: &lt;script&gt;"));
        assert!(!html.contains("<section class=\"tool-call\">"));
    }

    #[test]
    fn html_tool_content_omitted_note() {
        let messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 调用工具: shell".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: vec![ExportToolCall {
                name: "shell".into(),
                skill: None,
                input: String::new(),
                duration_ms: Some(300),
                result: None,
                checkpoint: None,
                content_omitted: true,
            }],
            tool_detail_error: None,
        }];
        let html = render_html("t", "2026-11-01", &messages, Path::new("/nonexistent")).unwrap();
        assert!(html.contains("工具详情总量已达上限，本调用的入参/结果正文未展开"));
    }

    #[test]
    fn truncate_tool_result_keeps_head_tail_and_char_boundary() {
        let text = "0".repeat(100);
        let (out, original) = truncate_tool_result(&text, 10);
        assert_eq!(original, Some(100));
        assert!(out.starts_with("00000"));
        assert!(out.ends_with("00000"));
        assert!(out.contains("中间省略"));
        // 中文多字节：max 落在字符中间时回退到边界，输出仍是合法 UTF-8。
        let cjk = "汉".repeat(50); // 150 字节
        let (out, _) = truncate_tool_result(&cjk, 10);
        assert!(out.starts_with("汉") && out.ends_with("汉"));
        let (out, _) = truncate_tool_field(&cjk, 10);
        assert!(out.starts_with("汉") && out.contains("已截断"));
    }

    #[test]
    fn estimate_html_capacity_includes_tool_text() {
        let mut messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 x".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        let (est0, _) =
            estimate_html_capacity(&messages, Path::new("/nonexistent"), u64::MAX, u64::MAX);
        messages[0].tools.push(ExportToolCall {
            name: "shell".into(),
            skill: None,
            input: "y".repeat(1000),
            duration_ms: None,
            result: Some(ExportToolResult {
                content: "z".repeat(2000),
                is_error: false,
            }),
            checkpoint: None,
            content_omitted: false,
        });
        let (est1, _) =
            estimate_html_capacity(&messages, Path::new("/nonexistent"), u64::MAX, u64::MAX);
        // 工具文本进预估（转义后长度 + 标记余量），单缓冲一次预留不失效。
        assert!(est1 - est0 >= 3000);
    }

    #[test]
    fn html_tool_big_hint_in_header() {
        let big = "x".repeat(64);
        let messages = vec![ExportMessage {
            role: "status".into(),
            content: "🔧 调用工具: shell".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: vec![ExportToolCall {
                name: "shell".into(),
                skill: None,
                input: big,
                duration_ms: None,
                result: None,
                checkpoint: None,
                content_omitted: false,
            }],
            tool_detail_error: None,
        }];
        // 工具提示阈值注入 1 字节：64 字节工具文本必触发。
        let html = render_html_with_budget(
            "t",
            "2026-11-01",
            &messages,
            Path::new("/nonexistent"),
            HtmlBudgetPolicy {
                total_budget: u64::MAX,
                max_image_bytes: u64::MAX,
                big_file_hint_bytes: u64::MAX,
                tool_big_hint_bytes: 1,
            },
        )
        .unwrap();
        assert!(html.contains("工具详情较大，打开可能较慢"));
        // 阈值放宽后不出现。
        let html2 = render_html_with_budget(
            "t",
            "2026-11-01",
            &messages,
            Path::new("/nonexistent"),
            HtmlBudgetPolicy {
                total_budget: u64::MAX,
                max_image_bytes: u64::MAX,
                big_file_hint_bytes: u64::MAX,
                tool_big_hint_bytes: usize::MAX,
            },
        )
        .unwrap();
        assert!(!html2.contains("工具详情较大"));
    }

    #[cfg(unix)]
    #[test]
    fn estimate_and_render_do_not_follow_symlink_outside_root() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.png"), b"secret-bytes").unwrap();
        std::fs::create_dir_all(dir.path().join("media/out")).unwrap();
        symlink(outside.path().join("secret.png"), dir.path().join("media/out/link.png"))
            .unwrap();
        let messages = vec![ExportMessage {
            role: "user".into(),
            content: "图 media/out/link.png".into(),
            ts: 0,
            seg_complete: true,
            msg_id: 0,
            tools: Vec::new(),
            tool_detail_error: None,
        }];
        // 预扫：symlink 指向根外 → resolve 拒绝 → 不计数不 stat。
        let (_, image_bytes) =
            estimate_html_capacity(&messages, dir.path(), u64::MAX, u64::MAX);
        assert_eq!(image_bytes, 0);
        // 渲染：占位「路径越出工作区」，内容不进产物。
        let html = render_html("t", "2026-11-01", &messages, dir.path()).unwrap();
        assert!(!html.contains("secret-bytes"));
        assert!(!html.contains("data:image"));
        assert!(html.contains("路径越出工作区"));
    }
}
