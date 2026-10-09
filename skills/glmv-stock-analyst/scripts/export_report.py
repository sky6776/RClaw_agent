#!/usr/bin/env python3
"""
export_report.py — 将 Markdown 报告导出为 PDF 或 DOCX（图片嵌入，格式保留）

使用方式：
  python3 export_report.py report.md --format pdf
  python3 export_report.py report.md --format docx
  python3 export_report.py report.md --format both
  python3 export_report.py report.md --format pdf --output ~/Downloads/腾讯分析.pdf

依赖：
  pip3 install fpdf2 python-docx Pillow
"""

import argparse
import hashlib
import os
import re
import secrets
import shutil
import sys
import tempfile
from typing import Optional

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")


def file_sha256(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()

# ═══════════════════════════════════════════════
# Markdown 解析
# ═══════════════════════════════════════════════


def parse_markdown(md_text):
    """将 Markdown 文本解析为结构化元素列表"""
    elements = []
    lines = md_text.split("\n")
    i = 0
    while i < len(lines):
        line = lines[i]

        # 空行
        if not line.strip():
            i += 1
            continue

        # 标题
        m = re.match(r"^(#{1,4})\s+(.+)", line)
        if m:
            level = len(m.group(1))
            elements.append(
                {"type": "heading", "level": level, "text": m.group(2).strip()}
            )
            i += 1
            continue

        # 水平分割线
        if re.match(r"^[-=─═]{3,}", line.strip()) or line.strip() == "---":
            elements.append({"type": "hr"})
            i += 1
            continue

        # 图片
        m = re.match(r"^!\[([^\]]*)\]\(([^)]+)\)", line)
        if m:
            elements.append({"type": "image", "alt": m.group(1), "path": m.group(2)})
            i += 1
            continue

        # 表格（连续的 | 行）
        if "|" in line and line.strip().startswith("|"):
            table_lines = []
            while i < len(lines) and "|" in lines[i] and lines[i].strip():
                table_lines.append(lines[i])
                i += 1
            # 解析表格
            rows = []
            for tl in table_lines:
                cells = [c.strip() for c in tl.strip().strip("|").split("|")]
                # 跳过分隔行 (---|---|---)
                if all(re.match(r"^[-:]+$", c) for c in cells if c):
                    continue
                rows.append(cells)
            if rows:
                elements.append({"type": "table", "rows": rows})
            continue

        # 引用块
        if line.startswith(">"):
            text = line.lstrip("> ").strip()
            elements.append({"type": "blockquote", "text": text})
            i += 1
            continue

        # 列表项（- xxx / * xxx / 1. xxx）
        if re.match(r"^[\-\*]\s+|^\d+\.\s+", line):
            items = []
            while i < len(lines) and re.match(r"^[\-\*]\s+|^\d+\.\s+", lines[i]):
                item_text = re.sub(r"^[\-\*]\s+|^\d+\.\s+", "", lines[i]).strip()
                items.append(item_text)
                i += 1
            elements.append({"type": "list", "items": items})
            continue

        # 普通段落（合并连续非空行）
        para_lines = []
        while (
            i < len(lines)
            and lines[i].strip()
            and not re.match(r"^[#|>!\-\*=─═]", lines[i])
            and not re.match(r"^\d+\.\s+", lines[i])
        ):
            # 检查下一行是否是图片
            if re.match(r"^!\[", lines[i]):
                break
            para_lines.append(lines[i])
            i += 1
        if para_lines:
            elements.append({"type": "paragraph", "text": "\n".join(para_lines)})
            continue

        i += 1

    return elements


# ═══════════════════════════════════════════════
# 中文字体检测
# ═══════════════════════════════════════════════


def find_cn_font():
    """查找系统中可用的中文字体文件"""
    candidates = [
        # macOS
        "/System/Library/Fonts/PingFang.ttc",
        "/System/Library/Fonts/Supplemental/PingFang.ttc",
        "/System/Library/Fonts/STHeiti Light.ttc",
        "/System/Library/Fonts/STHeiti Medium.ttc",
        "/Library/Fonts/Arial Unicode MS.ttf",
        # Linux
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        # Windows
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\simhei.ttf",
    ]
    for p in candidates:
        if os.path.exists(p):
            return p
    return None


# Emoji / 装饰符号的 Unicode 块。只剥离这些，保留中文等正常文字——
# 旧实现用 `[^0-9A-Za-z._-]+` 这种 ASCII 白名单会把中文也一并清掉。
_EMOJI_RE = re.compile(
    "["
    "\U0001F1E6-\U0001F1FF"  # 区域指示符（国旗）
    "\U0001F300-\U0001FAFF"  # 各类 Emoji 与象形/补充符号
    "\u2600-\u27BF"          # 杂项符号 & 装饰符
    "\uFE00-\uFE0F"          # 变体选择符（Emoji 修饰）
    "\u200D"                 # 零宽连接符 ZWJ
    "]+"
)

# 文件名中不安全的字符：控制符、路径分隔符、Windows 保留符、
# 媒体标记终止符 `]`、空白。
_FILENAME_UNSAFE_RE = re.compile(r"[\x00-\x1f\x7f<>:\"/\\|?*\]\s]")


def safe_filename(name: str) -> str:
    """清理成安全文件名：剥离 Emoji，路径不安全字符替成 `_`，但保留中文等文字。"""
    name = _EMOJI_RE.sub("", name)
    name = "".join(ch if ch.isprintable() else "_" for ch in name)
    safe = _FILENAME_UNSAFE_RE.sub("_", name)
    safe = re.sub(r"_+", "_", safe).strip("._ ")
    return safe or "report"


def media_channel(source_channel: str) -> str:
    ch = (source_channel or "").strip().lower()
    if ch == "napcatqq":
        ch = "napcat"
    # channel 会进入 marker 前缀和能力变量名；限制为有界 ASCII token，
    # 避免来源文本改变 marker 结构或构造意外的环境变量名。
    if (
        len(ch) > 64
        or not re.fullmatch(r"[a-z][a-z0-9_-]*", ch)
        or ch in {"cli", "system"}
    ):
        return ""
    return ch


def maybe_publish_export_file(
    out_path: str, source_channel: str, fmt: str
) -> Optional[str]:
    """把 PDF/DOCX 复制到媒体沙箱并返回通道媒体标记。"""
    ch = media_channel(source_channel)
    if not ch:
        return None
    flag = f"RCLAW_{ch.upper().replace('-', '_')}_ENABLED"
    if os.environ.get(flag) != "1":
        # 来源决定 marker 前缀；enabled 表示 channel 确有媒体能力。未来 channel
        # 仍复用同一沙箱，只需声明能力，不需在本脚本新增硬编码分支。
        return None

    media_dir = (os.environ.get("RCLAW_MEDIA_DIR") or "").strip()
    if not media_dir:
        print(f"⚠ RCLAW_MEDIA_DIR 为空，跳过 {fmt.upper()} 附件发布", file=sys.stderr)
        return None

    ext = f".{fmt.lower()}"
    abs_out = os.path.abspath(out_path)
    parent = safe_filename(os.path.basename(os.path.dirname(abs_out)))
    base = safe_filename(os.path.basename(abs_out))
    if not base.lower().endswith(ext):
        base += ext
    publish_root = os.path.join(media_dir, "out")
    recipient_name = f"{parent}_{base}"
    temporary = None

    try:
        # marker 入队后才由 channel 读取文件；内容寻址 + 原子改名避免同名导出在这段
        # 窗口内被后续任务覆盖，导致“验证的是 A、实际发出 B”。
        os.makedirs(publish_root, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".publish-", dir=publish_root)
        os.close(fd)
        shutil.copy2(abs_out, temporary)
        expected_digest = file_sha256(temporary)
        publish_dir = os.path.join(publish_root, expected_digest)
        os.makedirs(publish_dir, exist_ok=True)
        publish_path = os.path.join(publish_dir, recipient_name)
        try:
            os.replace(temporary, publish_path)
        except OSError:
            if (
                not os.path.isfile(publish_path)
                or file_sha256(publish_path) != expected_digest
            ):
                raise
            os.remove(temporary)
        temporary = None
    except OSError as exc:
        print(f"⚠ {fmt.upper()} 附件发布失败: {exc}", file=sys.stderr)
        return None
    finally:
        if temporary is not None:
            try:
                os.remove(temporary)
            except FileNotFoundError:
                pass

    marker_path = publish_path.replace("\\", "/")
    caption = f"{fmt.upper()}详细报告"
    # 内容路径可复用，但一次新的发布动作必须有新身份，防止旧 Agent 终态回放
    # 在用户取消后重新获得发送能力。
    delivery = secrets.token_hex(16)
    if ch == "weixin":
        marker = (
            f'[:weixin-media type=file path="{marker_path}" caption="{caption}" '
            f'delivery="{delivery}"]'
        )
    elif ch == "napcat":
        marker = (
            f'[:napcat-media type=file path="{marker_path}" '
            f'name="分析报告{ext}" caption="{caption}" delivery="{delivery}"]'
        )
    else:
        marker = (
            f'[:{ch}-media type=file path="{marker_path}" '
            f'name="分析报告{ext}" caption="{caption}" delivery="{delivery}"]'
        )
    return marker


# ═══════════════════════════════════════════════
# 导出 PDF（fpdf2）
# ═══════════════════════════════════════════════


def export_pdf(elements, output_path, md_dir):
    """将解析后的元素导出为 PDF"""
    try:
        from fpdf import FPDF
    except ImportError:
        print("❌ fpdf2 未安装。请运行: pip3 install fpdf2", file=sys.stderr)
        return False

    pdf = FPDF()
    pdf.set_auto_page_break(auto=True, margin=20)
    pdf.add_page()

    # 注册中文字体
    font_path = find_cn_font()
    if font_path:
        try:
            pdf.add_font("CN", "", font_path, uni=True)
            pdf.add_font("CN", "B", font_path, uni=True)
            font_name = "CN"
        except Exception:
            font_name = "Helvetica"
    else:
        font_name = "Helvetica"

    def set_font(size=10, bold=False):
        style = "B" if bold else ""
        pdf.set_font(font_name, style, size)

    def clean_md(text):
        """去除 Markdown 格式标记，保留纯文本"""
        text = re.sub(r"\*\*(.+?)\*\*", r"\1", text)
        text = re.sub(r"\*(.+?)\*", r"\1", text)
        text = re.sub(r"`(.+?)`", r"\1", text)
        text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
        return text

    def resolve_image(path):
        """解析图片路径（支持绝对路径和相对路径）"""
        if os.path.isabs(path) and os.path.exists(path):
            return path
        rel = os.path.join(md_dir, path)
        if os.path.exists(rel):
            return rel
        return None

    for elem in elements:
        t = elem["type"]

        if t == "heading":
            sizes = {1: 18, 2: 15, 3: 13, 4: 11}
            set_font(sizes.get(elem["level"], 11), bold=True)
            # 标题不能孤立在页底（至少留 30pt 给后续内容）
            if pdf.get_y() + 30 > pdf.h - pdf.b_margin:
                pdf.add_page()
            pdf.ln(4)
            pdf.multi_cell(0, 8, clean_md(elem["text"]))
            pdf.ln(2)

        elif t == "paragraph":
            set_font(10)
            pdf.multi_cell(0, 6, clean_md(elem["text"]))
            pdf.ln(3)

        elif t == "blockquote":
            set_font(9)
            pdf.set_text_color(100, 100, 100)
            pdf.multi_cell(0, 5, clean_md(elem["text"]))
            pdf.set_text_color(0, 0, 0)
            pdf.ln(2)

        elif t == "list":
            set_font(10)
            for item in elem["items"]:
                if pdf.get_y() + 8 > pdf.h - pdf.b_margin:
                    pdf.add_page()
                    set_font(10)
                pdf.set_x(pdf.l_margin)  # 重置X位置
                pdf.multi_cell(0, 6, f"    • {clean_md(item)}")
            pdf.ln(2)

        elif t == "hr":
            y = pdf.get_y()
            pdf.line(pdf.l_margin, y, pdf.w - pdf.r_margin, y)
            pdf.ln(5)

        elif t == "image":
            img_path = resolve_image(elem["path"])
            if img_path:
                try:
                    # 计算图片宽度（不超过页面宽度）
                    max_w = pdf.w - pdf.l_margin - pdf.r_margin
                    from PIL import Image as PILImage

                    with PILImage.open(img_path) as im:
                        w, h = im.size
                    # 按比例缩放
                    img_w = min(max_w, 170)
                    img_h = img_w * h / w
                    # 检查是否需要换页
                    if pdf.get_y() + img_h > pdf.h - pdf.b_margin:
                        pdf.add_page()
                    pdf.image(img_path, x=pdf.l_margin, w=img_w)
                    pdf.ln(3)
                except Exception as e:
                    set_font(8)
                    pdf.set_text_color(150, 150, 150)
                    pdf.multi_cell(0, 5, f"[图片: {elem['alt']}]")
                    pdf.set_text_color(0, 0, 0)
            else:
                set_font(8)
                pdf.set_text_color(150, 150, 150)
                pdf.multi_cell(0, 5, f"[图片未找到: {elem['alt']}]")
                pdf.set_text_color(0, 0, 0)

        elif t == "table":
            rows = elem["rows"]
            if not rows:
                continue
            n_cols = max(len(r) for r in rows)
            usable_w = pdf.w - pdf.l_margin - pdf.r_margin
            row_h = 7

            # 计算列宽：按内容长度分配
            col_max_len = [0] * n_cols
            for row in rows:
                for j, cell in enumerate(row):
                    if j < n_cols:
                        col_max_len[j] = max(col_max_len[j], len(clean_md(cell)))
            total_len = max(sum(col_max_len), 1)
            col_w = [max(usable_w * (l / total_len), 20) for l in col_max_len]
            # 归一化到 usable_w
            scale = usable_w / sum(col_w)
            col_w = [w * scale for w in col_w]

            # 预估表格总高度，不够就换页（表格 < 整页高度时才整体移）
            table_h = len(rows) * row_h + 5
            remaining = pdf.h - pdf.b_margin - pdf.get_y()
            if table_h < (pdf.h - pdf.t_margin - pdf.b_margin) and table_h > remaining:
                pdf.add_page()

            for row_idx, row in enumerate(rows):
                # 检查单行是否需要换页
                if pdf.get_y() + row_h > pdf.h - pdf.b_margin:
                    pdf.add_page()

                if row_idx == 0:
                    set_font(9, bold=True)
                    pdf.set_fill_color(240, 240, 240)
                    fill = True
                else:
                    set_font(9)
                    fill = False

                x_start = pdf.l_margin
                y_start = pdf.get_y()
                for j, cell in enumerate(row):
                    if j >= n_cols:
                        break
                    pdf.set_xy(x_start + sum(col_w[:j]), y_start)
                    text = clean_md(cell)
                    # 按列宽截断（每个字约占 2.5pt 宽度）
                    max_chars = max(int(col_w[j] / 2.5), 5)
                    if len(text) > max_chars:
                        text = text[: max_chars - 1] + "…"
                    pdf.cell(col_w[j], row_h, text, border=1, align="L", fill=fill)
                pdf.ln(row_h)

            pdf.ln(3)

    try:
        pdf.output(output_path)
        return True
    except Exception as e:
        print(f"❌ PDF 生成失败: {e}", file=sys.stderr)
        return False


# ═══════════════════════════════════════════════
# 导出 DOCX（python-docx）
# ═══════════════════════════════════════════════


def export_docx(elements, output_path, md_dir):
    """将解析后的元素导出为 DOCX"""
    try:
        from docx import Document
        from docx.enum.text import WD_ALIGN_PARAGRAPH
        from docx.shared import Inches, Pt, RGBColor
    except ImportError:
        print(
            "❌ python-docx 未安装。请运行: pip3 install python-docx", file=sys.stderr
        )
        return False

    doc = Document()

    # 设置默认字体
    style = doc.styles["Normal"]
    style.font.size = Pt(10)

    def clean_md(text):
        """去除 Markdown 格式标记"""
        text = re.sub(r"\*\*(.+?)\*\*", r"\1", text)
        text = re.sub(r"\*(.+?)\*", r"\1", text)
        text = re.sub(r"`(.+?)`", r"\1", text)
        text = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", text)
        return text

    def add_formatted_paragraph(text, style_name="Normal"):
        """添加段落，处理 **加粗** 格式和换行"""
        p = doc.add_paragraph(style=style_name)
        text_lines = text.split("\n")
        for li, line_text in enumerate(text_lines):
            parts = re.split(r"(\*\*[^*]+\*\*)", line_text)
            for part in parts:
                if part.startswith("**") and part.endswith("**"):
                    run = p.add_run(part[2:-2])
                    run.bold = True
                else:
                    cleaned = re.sub(r"`(.+?)`", r"\1", part)
                    cleaned = re.sub(r"\[([^\]]+)\]\([^)]+\)", r"\1", cleaned)
                    p.add_run(cleaned)
            # 行间加换行（最后一行不加）
            if li < len(text_lines) - 1:
                p.add_run().add_break()
        return p

    def resolve_image(path):
        if os.path.isabs(path) and os.path.exists(path):
            return path
        rel = os.path.join(md_dir, path)
        if os.path.exists(rel):
            return rel
        return None

    for elem in elements:
        t = elem["type"]

        if t == "heading":
            level = min(elem["level"], 4)
            doc.add_heading(clean_md(elem["text"]), level=level)

        elif t == "paragraph":
            add_formatted_paragraph(elem["text"])

        elif t == "blockquote":
            p = doc.add_paragraph(clean_md(elem["text"]))
            p.paragraph_format.left_indent = Inches(0.3)
            for run in p.runs:
                run.font.color.rgb = RGBColor(100, 100, 100)
                run.font.size = Pt(9)

        elif t == "list":
            for item in elem["items"]:
                p = doc.add_paragraph(clean_md(item), style="List Bullet")

        elif t == "hr":
            p = doc.add_paragraph()
            p.paragraph_format.space_before = Pt(6)
            p.paragraph_format.space_after = Pt(6)
            run = p.add_run("─" * 60)
            run.font.color.rgb = RGBColor(180, 180, 180)
            run.font.size = Pt(8)

        elif t == "image":
            img_path = resolve_image(elem["path"])
            if img_path:
                try:
                    doc.add_picture(img_path, width=Inches(5.5))
                    # 图片居中
                    last_paragraph = doc.paragraphs[-1]
                    last_paragraph.alignment = WD_ALIGN_PARAGRAPH.CENTER
                except Exception:
                    p = doc.add_paragraph(f"[图片: {elem['alt']}]")
                    p.runs[0].font.color.rgb = RGBColor(150, 150, 150)
            else:
                p = doc.add_paragraph(f"[图片未找到: {elem['alt']}]")
                p.runs[0].font.color.rgb = RGBColor(150, 150, 150)

        elif t == "table":
            rows = elem["rows"]
            if not rows:
                continue
            n_cols = max(len(r) for r in rows)
            table = doc.add_table(rows=len(rows), cols=n_cols)
            table.style = "Light Grid Accent 1"
            for i, row in enumerate(rows):
                for j, cell in enumerate(row):
                    if j < n_cols:
                        table.cell(i, j).text = clean_md(cell)
                        # 表头加粗
                        if i == 0:
                            for p in table.cell(i, j).paragraphs:
                                for run in p.runs:
                                    run.bold = True

    try:
        doc.save(output_path)
        return True
    except Exception as e:
        print(f"❌ DOCX 生成失败: {e}", file=sys.stderr)
        return False


# ═══════════════════════════════════════════════
# 主函数
# ═══════════════════════════════════════════════


def main():
    parser = argparse.ArgumentParser(
        description="将 Markdown 股票分析报告导出为 PDF 或 DOCX",
        epilog="示例：\n"
        "  python3 export_report.py report.md --format pdf\n"
        "  python3 export_report.py report.md --format docx\n"
        "  python3 export_report.py report.md --format both\n"
        "  python3 export_report.py report.md --format pdf --output ~/Downloads/报告.pdf\n",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("input", help="输入的 Markdown 文件路径")
    parser.add_argument(
        "--format",
        choices=["pdf", "docx", "both"],
        default="pdf",
        help="输出格式 (默认: pdf)",
    )
    parser.add_argument(
        "--output", default=None, help="输出文件路径（不指定则自动命名）"
    )
    parser.add_argument(
        "--source-channel",
        default="",
        help="直接运行时的 source_channel；RClaw 注入的 RCLAW_SOURCE_CHANNEL 优先",
    )
    args = parser.parse_args()
    # 外部 channel 共用媒体沙箱，不能从输出目录推断 marker 前缀；RClaw 注入的
    # 本轮来源必须覆盖模型可能手填的参数，参数仅保留给直接运行脚本的场景。
    runtime_channel = os.environ.get("RCLAW_SOURCE_CHANNEL", "").strip()
    if runtime_channel:
        args.source_channel = runtime_channel

    # 读取 Markdown
    if not os.path.exists(args.input):
        print(f"❌ 文件不存在: {args.input}", file=sys.stderr)
        sys.exit(1)

    with open(args.input, "r", encoding="utf-8") as f:
        md_text = f.read()

    md_dir = os.path.dirname(os.path.abspath(args.input))
    base_name = os.path.splitext(os.path.basename(args.input))[0]

    # 解析
    print(f"📄 解析 Markdown: {args.input}", file=sys.stderr)
    elements = parse_markdown(md_text)
    n_img = sum(1 for e in elements if e["type"] == "image")
    n_tbl = sum(1 for e in elements if e["type"] == "table")
    print(f"   {len(elements)} 个元素，{n_img} 张图片，{n_tbl} 个表格", file=sys.stderr)

    # 导出
    formats = [args.format] if args.format != "both" else ["pdf", "docx"]
    publish_channel = media_channel(args.source_channel)
    publish_enabled = bool(
        publish_channel
        and os.environ.get(
            f"RCLAW_{publish_channel.upper().replace('-', '_')}_ENABLED"
        )
        == "1"
    )
    publish_markers = []
    complete = True

    for fmt in formats:
        if args.output and len(formats) == 1:
            out_path = args.output
        else:
            out_path = os.path.join(md_dir, f"{base_name}.{fmt}")

        print(f"📝 导出 {fmt.upper()}: {out_path}", file=sys.stderr)

        if fmt == "pdf":
            ok = export_pdf(elements, out_path, md_dir)
        else:
            ok = export_docx(elements, out_path, md_dir)

        if ok:
            size_kb = os.path.getsize(out_path) // 1024
            print(f"   ✅ 成功! {size_kb}KB", file=sys.stderr)
            marker = maybe_publish_export_file(out_path, args.source_channel, fmt)
            if marker is not None:
                publish_markers.append(marker)
            elif publish_enabled:
                complete = False
        else:
            print(f"   ❌ 失败", file=sys.stderr)
            complete = False

    # `--format both` 是一个双附件交付。等待两种格式均生成且均暂存成功后再统一
    # 输出回执，避免第一条 marker 先被运行时接纳、第二种格式随后失败造成半交付。
    if complete:
        for marker in publish_markers:
            print(f"MEDIA_MARKER={marker}")


if __name__ == "__main__":
    main()
