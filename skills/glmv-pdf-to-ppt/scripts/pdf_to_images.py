#!/usr/bin/env python3
"""
Convert a local PDF file to PNG images.
Use curl to download remote PDFs before calling this script.

Usage:
    python pdf_to_images.py <pdf_path> [--dpi 120] [--out-dir DIR]

Output (stdout):
    JSON list of {"page": int, "path": "/abs/path/page_001.png"}
"""

import os
import sys
import json
import argparse
import shutil
import subprocess
import tempfile
import re


GENERATED_PAGE = re.compile(r"^(?:page_\d{3}|_pdftoppm-\d+)\.png$")
MAX_PDF_BYTES = 100 * 1024 * 1024
MAX_PDF_PAGES = 200


def validate_pdf(pdf_path: str, dpi: int) -> str:
    """校验本地 PDF 的类型、大小与渲染分辨率。"""
    if dpi < 36 or dpi > 600:
        return "DPI must be between 36 and 600"
    if not os.path.isfile(pdf_path):
        return f"File not found: {pdf_path}"
    if os.path.getsize(pdf_path) > MAX_PDF_BYTES:
        return "PDF exceeds 100 MB"
    try:
        with open(pdf_path, "rb") as handle:
            if not handle.read(5).startswith(b"%PDF-"):
                return "File content is not a PDF"
    except OSError as exc:
        return f"Cannot read PDF: {exc}"
    return ""


def convert_pdf(pdf_path: str, out_dir: str, dpi: int) -> list[dict]:
    validation_error = validate_pdf(pdf_path, dpi)
    if validation_error:
        raise ValueError(validation_error)
    try:
        import fitz
    except ImportError:
        fitz = None

    out_dir = os.path.abspath(out_dir)
    os.makedirs(os.path.dirname(out_dir), exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=".rclaw-pdf-render-", dir=os.path.dirname(out_dir)
    ) as staging:
        rendered = []
        if os.environ.get("RCLAW_ANDROID") == "1":
            # APK：fitz/pdftoppm 均不可用，走 Android PdfRenderer Java 桥。
            from rclaw_android_runtime import pdf_page_count, render_pdf

            if pdf_page_count(pdf_path) > MAX_PDF_PAGES:
                raise RuntimeError(f"PDF exceeds {MAX_PDF_PAGES} pages")
            pages = render_pdf(
                pdf_path, staging, dpi=dpi, max_pages=MAX_PDF_PAGES
            )
            rendered = [str(page["path"]) for page in pages]
        elif fitz is None:
            pdftoppm = shutil.which("pdftoppm")
            if not pdftoppm:
                raise RuntimeError(
                    "Install PyMuPDF, or on Termux run: pkg install poppler"
                )
            # 中文注释：fallback 渲染前先查页数，避免无上限地生成中间图片。
            pdfinfo = shutil.which("pdfinfo")
            if not pdfinfo:
                raise RuntimeError("pdfinfo is required to enforce the PDF page limit")
            info = subprocess.run(
                [pdfinfo, pdf_path], capture_output=True, text=True, check=True
            )
            page_match = re.search(r"^Pages:\s+(\d+)\s*$", info.stdout, re.MULTILINE)
            if not page_match:
                raise RuntimeError("Unable to determine PDF page count")
            if int(page_match.group(1)) > MAX_PDF_PAGES:
                raise RuntimeError(f"PDF exceeds {MAX_PDF_PAGES} pages")
            prefix = os.path.join(staging, "_pdftoppm")
            subprocess.run(
                [pdftoppm, "-png", "-r", str(dpi), pdf_path, prefix],
                check=True,
            )
            names = sorted(
                (
                    name
                    for name in os.listdir(staging)
                    if name.startswith("_pdftoppm-") and name.endswith(".png")
                ),
                key=lambda name: int(os.path.splitext(name)[0].rsplit("-", 1)[-1]),
            )
            rendered = [os.path.join(staging, name) for name in names]
        else:
            with fitz.open(pdf_path) as doc:
                if len(doc) == 0:
                    raise RuntimeError("PDF contains no pages")
                if len(doc) > MAX_PDF_PAGES:
                    raise RuntimeError(f"PDF exceeds {MAX_PDF_PAGES} pages")
                for page_num, page in enumerate(doc, 1):
                    out_path = os.path.join(staging, f"page_{page_num:03d}.png")
                    pix = page.get_pixmap(
                        matrix=fitz.Matrix(dpi / 72, dpi / 72), alpha=False
                    )
                    pix.save(out_path)
                    rendered.append(out_path)

        if not rendered:
            raise RuntimeError("PDF renderer produced no pages")

        os.makedirs(out_dir, exist_ok=True)
        for name in os.listdir(out_dir):
            existing = os.path.join(out_dir, name)
            if GENERATED_PAGE.fullmatch(name) and (
                os.path.isfile(existing) or os.path.islink(existing)
            ):
                os.unlink(existing)

        results = []
        for page_num, source in enumerate(rendered, 1):
            out_path = os.path.join(out_dir, f"page_{page_num:03d}.png")
            os.replace(source, out_path)
            results.append({"page": page_num, "path": out_path})
            print(f"  page {page_num}/{len(rendered)} → {out_path}", file=sys.stderr)
        return results


def main():
    parser = argparse.ArgumentParser(
        description="Convert PDF pages to local PNG images."
    )
    parser.add_argument("pdf_source", help="Local path to the PDF file.")
    parser.add_argument(
        "--dpi", type=int, default=120, help="Rendering DPI (default 120)."
    )
    parser.add_argument(
        "--out-dir",
        default=None,
        help="Directory to save images (default: temp dir named after PDF stem).",
    )
    args = parser.parse_args()
    if args.dpi < 36 or args.dpi > 600:
        parser.error("--dpi must be between 36 and 600")

    source = args.pdf_source
    validation_error = validate_pdf(source, args.dpi)
    if validation_error:
        print(f"[ERROR] {validation_error}", file=sys.stderr)
        sys.exit(1)
    pdf_path = source
    pdf_stem = os.path.splitext(os.path.basename(source))[0]
    out_dir = args.out_dir or os.path.join(tempfile.gettempdir(), f"{pdf_stem}_pages")
    results = convert_pdf(pdf_path, out_dir, args.dpi)
    print(json.dumps(results, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main()
