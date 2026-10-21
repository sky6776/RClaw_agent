//! 会话消息导出：选中的消息打包成 Markdown 文本文档或内嵌图片的 DOCX。
//!
//! 设计取舍：PDF 的中文字形需要嵌入字体/子集化（重依赖），DOCX 由查看器
//! 解析字体（CJK 天然安全），因此图片文档走 DOCX；纯文本走 Markdown。

use std::io::Write;
use std::path::{Path, PathBuf};

/// 单条待导出消息（role: user | assistant | thinking | status；content 为消息原文）。
#[derive(Debug)]
pub struct ExportMessage {
    pub role: String,
    pub content: String,
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

/// 从消息原文提取图片工作区路径（marker path= 属性 + 正文内联路径）。
/// 与前端 MediaPreviews 同源的解析规则，去重保序。
pub fn extract_image_paths(contents: &[String]) -> Vec<String> {
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
    for content in contents {
        // 1) 媒体回执行里的 path（绝对或相对）。
        let mut rest = content.as_str();
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
    }
    paths.truncate(24);
    paths
}

/// 解析 PNG/JPEG/GIF/BMP 头部的像素宽高（避免引入解码库；仅用于排版比例）。
fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.len() >= 24 && bytes[..8] == [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] {
        let w = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let h = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return Some((w, h));
    }
    if bytes.len() >= 10 && (bytes[..3] == [b'G', b'I', b'F']) {
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

/// 内嵌图片限制：单张 10MB、总数 24 张（导出文档不是媒体库）。
const DOCX_IMAGE_MAX_BYTES: u64 = 10 * 1024 * 1024;

struct DocxImage {
    rel_id: String,
    file_name: String,
    bytes: Vec<u8>,
    /// 像素宽高（未知时用默认比例）。
    dims: Option<(u32, u32)>,
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
        if rel.contains("..") || rel.starts_with('/') {
            continue;
        }
        let full = root_canon.join(rel);
        let Ok(canonical) = full.canonicalize() else {
            continue;
        };
        if !canonical.starts_with(&root_canon) {
            continue;
        }
        let Ok(meta) = canonical.metadata() else {
            continue;
        };
        if !meta.is_file() || meta.len() > DOCX_IMAGE_MAX_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&canonical) else {
            continue;
        };
        let ext = canonical
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
            dims: image_dimensions(&bytes),
            bytes,
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
                },
                ExportMessage {
                    role: "assistant".into(),
                    content: "MEDIA_MARKER=[:x-media type=image path=\"p\"]\n图如下".into(),
                },
                ExportMessage {
                    role: "thinking".into(),
                    content: "先列出目录".into(),
                },
                ExportMessage {
                    role: "status".into(),
                    content: "🔧 调用工具: list output".into(),
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
}
