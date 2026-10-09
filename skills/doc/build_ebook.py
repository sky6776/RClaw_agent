#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
build_ebook.py — 把本目录（skills/doc/）下全部 .md 技能分册汇编成一本带详细书签（页签）
和可点击目录页的 PDF 电子书。文档变更/新增/删除后重新运行本脚本即可全量重建。

用法:
    python build_ebook.py            # 输出到本目录 RClaw技能Prompt模板电子书.pdf
    python build_ebook.py -o 路径    # 指定输出文件

依赖:
    pip install fpdf2
    中文字体使用 Windows 系统字体（微软雅黑 msyh.ttc / msyhbd.ttc，缺失时按候选链回退）。

汇编规则:
    - 扫描本目录全部 *.md（脚本自身与输出 PDF 除外）。
    - 排序：文件内出现 "### P<n>" 模板编号的按最小编号排序（保持 P1→P127 阅读顺序），
      无编号的文件按文件名排在其后 —— 新增文档无需改脚本。
    - 书签层级：文档一级标题 → 顶层页签；其后的 # / ## → 二级；### (各条 Prompt) → 三级。
    - 目录页带页码，正文书签在 PDF 阅读器侧边栏可直接跳转。
"""

import argparse
import datetime as dt
import math
import re
import sys
from pathlib import Path

from fpdf import FPDF
from fpdf.fonts import FontFace

DIR = Path(__file__).resolve().parent
OUT_DEFAULT = DIR / "RClaw技能Prompt模板电子书.pdf"
TITLE = "RClaw 技能 Prompt 模板电子书"

INK = (32, 38, 48)
MUTED = (120, 128, 140)
ACCENT = (31, 78, 121)
CODE_BG = (244, 246, 248)
HEAD_BG = (222, 231, 240)
LINK = (17, 88, 166)
CODE_TX = (146, 44, 44)

# CJK 字体候选（常规, 粗体；粗体缺省时复用常规文件）
FONT_CANDIDATES = [
    (r"C:\Windows\Fonts\msyh.ttc", r"C:\Windows\Fonts\msyhbd.ttc"),
    (r"C:\Windows\Fonts\Deng.ttf", r"C:\Windows\Fonts\Dengb.ttf"),
    (r"C:\Windows\Fonts\simhei.ttf", r"C:\Windows\Fonts\simhei.ttf"),
    ("/system/fonts/NotoSansCJK-Regular.ttc", "/system/fonts/NotoSansCJK-Bold.ttc"),
    ("/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
     "/usr/share/fonts/opentype/noto/NotoSansCJK-Bold.ttc"),
]

# 嵌入字体没有 emoji 字形，替换为常见 CJK 字体中存在的近义符号
EMOJI_MAP = {
    "🔴": "●", "🟡": "◐", "⚪": "○", "✅": "√", "❌": "×", "⚠️": "(!)", "⚠": "(!)",
    "🌐": "", "📄": "", "💻": "", "🗄": "", "📦": "", "🚀": "", "🤖": "", "💡": "",
    "\ufe0f": "",  # 变体选择符
}
EMOJI_RANGE = re.compile(
    "[" "\U0001F000-\U0001FAFF" "\U00002600-\U000027BF" "\U0001F900-\U0001F9FF" "]"
)


def sanitize(text: str) -> str:
    for k, v in EMOJI_MAP.items():
        text = text.replace(k, v)
    return EMOJI_RANGE.sub("", text)


def strip_md(text: str) -> str:
    """表格单元格用：去掉行内标记，链接只留文字。"""
    text = re.sub(r"\[([^\]]*)\]\([^)]*\)", r"\1", text)
    return text.replace("**", "").replace("`", "")


def display_len(text: str) -> float:
    return sum(2.0 if ord(ch) > 0x2E80 else 1.0 for ch in text)


# ---------------------------------------------------------------- Markdown 解析

INLINE_RE = re.compile(r"(\*\*.+?\*\*|`[^`]+`|\[[^\]]+\]\([^)\s]+\))")


def parse_blocks(lines):
    """把一个 md 文件的行解析为块序列。"""
    blocks, i, n = [], 0, len(lines)
    para = []

    def flush():
        if para:
            blocks.append(("para", list(para)))
            para.clear()

    while i < n:
        raw = lines[i].rstrip("\n")
        line = raw.strip()
        if not line:
            flush()
            i += 1
            continue
        m = re.match(r"^(#{1,6})\s+(.*)$", line)
        if m:
            flush()
            blocks.append(("h", min(len(m.group(1)), 3), m.group(2).strip()))
            i += 1
            continue
        if line in {"---", "***", "___"}:
            flush()
            blocks.append(("hr",))
            i += 1
            continue
        if line.startswith("```"):
            flush()
            i += 1
            code = []
            while i < n and not lines[i].strip().startswith("```"):
                code.append(lines[i].rstrip("\n"))
                i += 1
            i += 1  # 跳过收尾 ```
            blocks.append(("code", code))
            continue
        if line.startswith(">"):
            flush()
            quote = []
            while i < n and lines[i].strip().startswith(">"):
                quote.append(lines[i].strip().lstrip(">").strip())
                i += 1
            blocks.append(("quote", quote))
            continue
        if line.startswith("|") and i + 1 < n and re.match(r"^\|[\s:|-]+\|?\s*$", lines[i + 1].strip()):
            flush()
            rows = [split_row(line)]
            i += 2
            while i < n and lines[i].strip().startswith("|"):
                rows.append(split_row(lines[i].strip()))
                i += 1
            blocks.append(("table", rows))
            continue
        m = re.match(r"^(\s*)([-*])\s+(.*)$", raw) or re.match(r"^(\s*)(\d+[.、)])\s+(.*)$", raw)
        if m:
            flush()
            indent = len(m.group(1).expandtabs(2))
            blocks.append(("li", indent // 2, m.group(2), m.group(3).strip()))
            i += 1
            continue
        para.append(line)
        i += 1
    flush()
    return blocks


def split_row(line: str):
    line = line.strip().strip("|")
    return [c.strip() for c in line.split("|")]


# ---------------------------------------------------------------- 渲染

class EbookPDF(FPDF):
    def footer(self):
        if self.page_no() == 1:  # 封面不放页脚
            return
        self.set_y(-13)
        self.set_font("cjk", "", 8)
        self.set_text_color(*MUTED)
        self.cell(0, 5, f"{TITLE} · 第 {self.page_no()} 页", align="C")
        self.set_text_color(*INK)


def register_fonts(pdf):
    last_err = None
    for reg, bold in FONT_CANDIDATES:
        reg_p, bold_p = Path(reg), Path(bold)
        if not reg_p.exists():
            continue
        try:
            pdf.add_font("cjk", "", str(reg_p))
            pdf.add_font("cjk", "B", str(bold_p if bold_p.exists() else reg_p))
            return True
        except Exception as e:  # 字体文件存在但解析失败（如 TTC 不兼容）→ 换下一个
            last_err = e
    raise RuntimeError(f"找不到可用的 CJK 字体，请安装微软雅黑或修改 FONT_CANDIDATES：{last_err}")


def write_inline(pdf, text, size=10.5, color=INK):
    """按 **粗体** / `代码` / [链接](url) 逐段渲染一行行内文本，结束时换行。"""
    for tok in INLINE_RE.split(text):
        if not tok:
            continue
        if tok.startswith("**") and tok.endswith("**") and len(tok) > 4:
            pdf.set_font("cjk", "B", size)
            pdf.set_text_color(*INK)
            pdf.write(5.6, sanitize(tok[2:-2]))
        elif tok.startswith("`") and tok.endswith("`") and len(tok) > 2:
            pdf.set_font("cjk", "", size - 0.7)
            pdf.set_text_color(*CODE_TX)
            pdf.write(5.6, sanitize(tok[1:-1]))
        elif tok.startswith("["):
            m = re.match(r"\[([^\]]+)\]\(([^)\s]+)\)", tok)
            if m:
                pdf.set_font("cjk", "", size)
                pdf.set_text_color(*LINK)
                pdf.write(5.6, sanitize(m.group(1)), link=m.group(2))
                continue
            pdf.set_font("cjk", "", size)
            pdf.set_text_color(*color)
            pdf.write(5.6, sanitize(tok))
        else:
            pdf.set_font("cjk", "", size)
            pdf.set_text_color(*color)
            pdf.write(5.6, sanitize(tok))
    pdf.ln(5.6)
    pdf.set_text_color(*INK)


def render_heading(pdf, text, md_level, doc_started, state, records=None):
    if md_level == 1 and not doc_started[0]:
        desired, doc_started[0] = 0, True
    elif md_level == 1:
        desired = 1
    else:
        desired = md_level - 1  # h2→1, h3→2
    bm_level = min(desired, state["level"] + 1)  # 书签不允许跳级
    state["level"] = bm_level
    if records is not None:
        records.append((bm_level, sanitize(text), pdf.page_no()))
    size = {1: 19, 2: 14, 3: 12}[md_level]
    need = size * 0.55 + 8
    if pdf.get_y() + need > pdf.page_break_trigger:
        pdf.add_page()
    else:
        pdf.ln(2.5 if md_level == 1 else 1.6)
    if md_level == 1:
        pdf.set_fill_color(*HEAD_BG)
        pdf.set_text_color(*ACCENT)
        pdf.set_font("cjk", "B", size)
        pdf.start_section(sanitize(text), bm_level)
        pdf.cell(0, size * 0.62, "  " + sanitize(text), fill=True, new_x="LMARGIN", new_y="NEXT")
        pdf.ln(1.4)
    else:
        pdf.set_text_color(*(ACCENT if md_level == 3 else INK))
        pdf.set_font("cjk", "B", size)
        pdf.start_section(sanitize(text), bm_level)
        pdf.cell(0, size * 0.6, sanitize(text), new_x="LMARGIN", new_y="NEXT")
        if md_level == 3:
            pdf.set_draw_color(*ACCENT)
            pdf.set_line_width(0.35)
            pdf.line(pdf.l_margin, pdf.get_y() + 0.6, pdf.l_margin + 26, pdf.get_y() + 0.6)
        pdf.ln(1.8)
    pdf.set_text_color(*INK)


def render_para(pdf, text):
    write_inline(pdf, text)


def render_quote(pdf, quote_lines):
    pdf.set_font("cjk", "", 9.6)
    pdf.set_text_color(96, 104, 118)
    y0 = pdf.get_y() + 0.8
    for ln in quote_lines:
        pdf.set_x(pdf.l_margin + 5)
        pdf.multi_cell(pdf.epw - 8, 4.9, sanitize(ln), new_x="LMARGIN", new_y="NEXT")
    pdf.set_draw_color(150, 160, 175)
    pdf.set_line_width(0.5)
    pdf.line(pdf.l_margin + 1.5, y0, pdf.l_margin + 1.5, pdf.get_y() - 0.5)
    pdf.ln(1.6)
    pdf.set_text_color(*INK)


def render_code(pdf, code_lines):
    pdf.set_font("cjk", "", 8.2)
    lh, avail = 4.3, pdf.epw - 8
    total = 3
    for ln in code_lines:
        w = pdf.get_string_width(sanitize(ln))
        total += max(1, math.ceil(w / avail)) * lh if w else lh
    if pdf.get_y() + total > pdf.page_break_trigger:
        pdf.add_page()
    y0 = pdf.get_y()
    pdf.set_fill_color(*CODE_BG)
    pdf.rect(pdf.l_margin, y0, pdf.epw, total + 1, "F")
    pdf.set_text_color(58, 66, 82)
    pdf.set_xy(pdf.l_margin + 4, y0 + 1.5)
    for ln in code_lines:
        pdf.set_x(pdf.l_margin + 4)
        pdf.multi_cell(avail, lh, sanitize(ln) if ln else " ", new_x="LMARGIN", new_y="NEXT")
    pdf.set_y(max(pdf.get_y(), y0 + total) + 2)
    pdf.set_text_color(*INK)


def render_table(pdf, rows):
    ncol = max(len(r) for r in rows)
    rows = [r + [""] * (ncol - len(r)) for r in rows]
    weights = [max(min(max(display_len(strip_md(r[i])) for r in rows) + 2, 64), 4) for i in range(ncol)]
    total = sum(weights)
    pdf.set_font("cjk", "", 8.4)
    try:
        with pdf.table(
            col_widths=tuple(w / total for w in weights),
            text_align="LEFT",
            line_height=4.5,
            padding=1.1,
            borders_layout="ALL",
            headings_style=FontFace(emphasis="BOLD", fill_color=HEAD_BG, color=INK),
        ) as table:
            for r in rows:
                row = table.row()
                for cell in r:
                    row.cell(sanitize(strip_md(cell)))
    except Exception as e:
        pdf.ln(1)
        write_inline(pdf, f"[表格渲染失败：{e}] {strip_md(' | '.join(rows[0]))}", 8.5, MUTED)
    pdf.ln(2)


def render_list_item(pdf, indent, marker, text):
    pdf.set_font("cjk", "", 10.2)
    bullet = "•" if marker in "-*" else marker
    x = pdf.l_margin + 4 + indent * 6
    pdf.set_x(x)
    pdf.cell(5, 5.4, bullet + " ")
    pdf.set_x(x + 5)
    y0 = pdf.get_y()
    write_inline(pdf, text, 10.2)
    # write_inline 用 pdf.write 续排，起点从当前 x 起；列表首行需整体缩进
    if pdf.get_y() == y0:
        pass
    pdf.ln(0.4)


def render_hr(pdf):
    pdf.ln(1.2)
    pdf.set_draw_color(200, 206, 214)
    pdf.set_line_width(0.3)
    y = pdf.get_y()
    pdf.line(pdf.l_margin, y, pdf.l_margin + pdf.epw, y)
    pdf.ln(3)


def render_cover(pdf, n_docs, n_prompts):
    pdf.add_page()
    pdf.set_fill_color(*ACCENT)
    pdf.rect(0, 0, pdf.w, 8, "F")
    pdf.set_y(60)
    pdf.set_font("cjk", "B", 30)
    pdf.set_text_color(*ACCENT)
    pdf.cell(0, 16, TITLE, align="C", new_x="LMARGIN", new_y="NEXT")
    pdf.set_y(86)
    pdf.set_font("cjk", "", 13)
    pdf.set_text_color(*INK)
    pdf.cell(0, 8, "skills/doc 技能分册全量汇编 · 书签目录可跳转", align="C", new_x="LMARGIN", new_y="NEXT")
    pdf.ln(10)
    pdf.set_draw_color(*ACCENT)
    pdf.set_line_width(0.5)
    pdf.line(pdf.l_margin + 55, pdf.get_y(), pdf.l_margin + pdf.epw - 55, pdf.get_y())
    pdf.ln(10)
    pdf.set_font("cjk", "", 12)
    pdf.set_text_color(*MUTED)
    pdf.cell(0, 8, f"收录 {n_docs} 份技能分册 · {n_prompts} 条 Prompt 模板", align="C", new_x="LMARGIN", new_y="NEXT")
    pdf.cell(0, 8, f"生成日期：{dt.date.today().isoformat()}", align="C", new_x="LMARGIN", new_y="NEXT")
    pdf.set_y(-30)
    pdf.set_font("cjk", "", 9)
    pdf.cell(0, 6, "由 build_ebook.py 自动生成 · 重新运行即可随文档变更重建", align="C")
    pdf.set_text_color(*INK)


# 目录版式常量：绘制与页数模拟共用，保证两遍分页一致
TOP_MARGIN = 16        # 与 build() 的 set_margins 保持一致
BOTTOM_MARGIN = 17
TOC_LH = 4.5           # 目录行高 mm
TOC_TITLE_H = 16       # 首页"目录"标题占用 mm
PAGE_H = 297           # A4


def toc_pages_needed(records):
    """按 draw_toc 的换页规则模拟目录需要几页。"""
    trigger = PAGE_H - BOTTOM_MARGIN
    pages, y = 1, TOP_MARGIN + TOC_TITLE_H
    for _ in records:
        if y + TOC_LH > trigger:
            pages += 1
            y = TOP_MARGIN
        y += TOC_LH
    return pages


def draw_toc(pdf, records, offset):
    """绘制目录页；页码 = 第一遍记录的真实页码 + 目录自身页数偏移。"""
    pdf.set_y(TOP_MARGIN)
    pdf.set_font("cjk", "B", 16)
    pdf.set_text_color(*ACCENT)
    pdf.cell(0, 10, "目录", new_x="LMARGIN", new_y="NEXT")
    pdf.ln(2)
    num_w = 12
    for level, name, page in records:
        if pdf.get_y() + TOC_LH > pdf.page_break_trigger:
            pdf.add_page()
            pdf.set_y(TOP_MARGIN)
        indent = [0, 5, 10][min(level, 2)]
        if len(name) > 66:
            name = name[:64] + "…"
        pdf.set_font("cjk", "B" if level == 0 else "", 9.5 if level == 0 else 8.8)
        pdf.set_text_color(*(INK if level == 0 else MUTED))
        x0 = pdf.l_margin + indent
        pdf.set_x(x0)
        pdf.cell(pdf.epw - indent - num_w, TOC_LH, name)
        label = str(page + offset)
        pdf.set_x(pdf.l_margin + pdf.epw - num_w)
        pdf.cell(num_w, TOC_LH, label, align="R")
        name_end = x0 + pdf.get_string_width(name) + 1.5
        dots_end = pdf.l_margin + pdf.epw - num_w - pdf.get_string_width(label) - 1.5
        if dots_end > name_end:
            pdf.set_draw_color(190, 196, 205)
            pdf.set_line_width(0.15)
            y = pdf.get_y() + TOC_LH * 0.72
            pdf.line(name_end, y, dots_end, y)
        pdf.ln(TOC_LH)
    pdf.set_text_color(*INK)


# ---------------------------------------------------------------- 主流程

def collect_docs():
    """[(排序键, 路径, 最小P编号)]；有 P 编号的按编号在前，无编号的按文件名在后。"""
    entries = []
    for path in sorted(DIR.glob("*.md")):
        text = path.read_text(encoding="utf-8-sig")
        nums = [int(m.group(1)) for m in re.finditer(r"^### P(\d+)", text, re.M)]
        entries.append((min(nums) if nums else 10_000, path.name, text))
    entries.sort(key=lambda e: (e[0], e[1]))
    return entries


def build(entries, out_path=None, toc_records=None, toc_offset=0):
    """构建电子书。

    - collect 用法（第一遍）：build(entries) → 返回 [(书签级别, 标题, 页码)]，不写文件。
      目录不计入分页，因此此时正文从第 2 页开始。
    - 最终用法（第二遍）：build(entries, out_path, toc_records, toc_offset)。
      封面 + 目录共占 1+toc_offset 页，正文整体后移 toc_offset 页，
      故目录里显示的页码 = 第一遍记录页码 + toc_offset。
    两遍的正文渲染逻辑完全一致，分页只差恒定偏移。
    """
    collecting = out_path is None
    n_prompts = sum(len(re.findall(r"^### P\d+", e[2], re.M)) for e in entries)
    pdf = EbookPDF(orientation="P", unit="mm", format="A4")
    pdf.set_margins(18, TOP_MARGIN, 18)
    pdf.set_auto_page_break(auto=True, margin=BOTTOM_MARGIN)
    pdf.set_title(TITLE)
    pdf.set_author("RClaw skills/doc")
    pdf.set_creator("build_ebook.py (fpdf2)")
    register_fonts(pdf)

    render_cover(pdf, len(entries), n_prompts)
    records = []

    if not collecting:
        pdf.add_page()  # 目录从封面后的新页开始，不与封面叠印
        draw_toc(pdf, toc_records, toc_offset)
    pdf.add_page()  # 正文固定从新页开始（两遍一致）

    for idx, (_, name, text) in enumerate(entries):
        if idx > 0:
            pdf.add_page()  # 每份分册新起一页
        blocks = parse_blocks(text.splitlines())
        doc_started = [False]
        state = {"level": -1}
        for b in blocks:
            kind = b[0]
            if kind == "h":
                render_heading(pdf, b[2], b[1], doc_started, state,
                               records if collecting else None)
            elif kind == "para":
                render_para(pdf, b[1][0])
            elif kind == "code":
                render_code(pdf, b[1])
            elif kind == "quote":
                render_quote(pdf, b[1])
            elif kind == "table":
                render_table(pdf, b[1])
            elif kind == "li":
                render_list_item(pdf, b[1], b[2], b[3])
            elif kind == "hr":
                render_hr(pdf)
        doc_started[0] = True

    if collecting:
        return records
    pdf.output(str(out_path))
    return None


def main():
    ap = argparse.ArgumentParser(description="把 skills/doc 下的 md 分册汇编成带书签的 PDF 电子书")
    ap.add_argument("-o", "--out", default=str(OUT_DEFAULT), help="输出 PDF 路径")
    args = ap.parse_args()
    out_path = Path(args.out)

    entries = collect_docs()
    if not entries:
        print(f"[ERROR] {DIR} 下没有找到 .md 文档")
        return 1
    print(f"[1/3] 已扫描 {len(entries)} 份分册：")
    for num, name, _ in entries:
        print(f"      {'P%-4d' % num if num < 10000 else '---- '} {name}")

    print("[2/3] 第一遍构建：收集标题页码 ……")
    records = build(entries)
    offset = toc_pages_needed(records)
    print(f"      {len(records)} 个标题，目录占 {offset} 页")

    print("[3/3] 第二遍构建：封面 + 目录 + 正文 ……")
    build(entries, out_path=out_path, toc_records=records, toc_offset=offset)

    size_kb = out_path.stat().st_size / 1024
    print(f"完成：{out_path}（{size_kb:.0f} KB）")
    return 0


if __name__ == "__main__":
    sys.exit(main())
