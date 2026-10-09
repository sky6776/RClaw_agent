"""APK 内置 Java 桥的 Python 侧薄封装（阶段 3，供 skill 脚本条件调用）。

仅在 `RCLAW_ANDROID=1`（APK 宿主）下被 import；桌面/Termux 路径完全不感知
本模块。提供 PDF 栅格化（PdfRenderer）与视频抽帧（MediaMetadataRetriever）
两类平台能力；PDF 文本提取统一走纯 Python 的 pypdf，不经本模块。
"""

import base64
import json
import os
import tempfile
from pathlib import Path

from java import jclass


_PDF_RUNTIME = jclass("ai.rclaw.app.PdfRuntime")
_MEDIA_RUNTIME = jclass("ai.rclaw.app.MediaRuntime")


def is_android():
    return os.environ.get("RCLAW_ANDROID") == "1"


def render_pdf(pdf_path, output_dir=None, dpi=150, max_pages=300):
    pdf_path = os.path.realpath(pdf_path)
    if output_dir is None:
        output_dir = tempfile.mkdtemp(prefix="rclaw-pdf-")
    output_dir = os.path.realpath(output_dir)
    return json.loads(
        str(_PDF_RUNTIME.renderPdf(pdf_path, output_dir, int(dpi), int(max_pages)))
    )


def pdf_to_data_urls(pdf_path, dpi=150, max_pages=50, max_page_bytes=None):
    # max_page_bytes：单页 PNG 字节上限，超限抛 ValueError——与桌面 fitz 路径的
    # 单页上限校验同语义（调用方传各自的 MAX_RENDERED_PAGE_BYTES）。
    import shutil

    output_dir = tempfile.mkdtemp(prefix="rclaw-pdf-")
    try:
        pages = render_pdf(pdf_path, output_dir=output_dir, dpi=dpi, max_pages=max_pages)
        result = []
        for page in pages:
            raw = Path(page["path"]).read_bytes()
            if max_page_bytes is not None and len(raw) > max_page_bytes:
                raise ValueError(
                    "Rendered page exceeds size limit: %s (%d > %d bytes)"
                    % (page["path"], len(raw), max_page_bytes)
                )
            encoded = base64.b64encode(raw).decode("ascii")
            result.append("data:image/png;base64," + encoded)
        return result
    finally:
        # 桌面 fitz 路径全程内存、无中间文件；桥路径的自建渲染目录用完即清，
        # 否则每次处理 PDF 都在私有目录漏一批 PNG。
        shutil.rmtree(output_dir, ignore_errors=True)


def pdf_page_count(pdf_path):
    return int(_PDF_RUNTIME.pageCount(os.path.realpath(pdf_path)))


def render_pdf_page(pdf_path, output_path, dpi=150, page_index=0):
    """单页栅格化（pageIndex 0-based），返回输出 PNG 路径。"""
    return str(
        _PDF_RUNTIME.renderPdfPage(
            os.path.realpath(pdf_path),
            os.path.realpath(output_path),
            int(dpi),
            int(page_index),
        )
    )


def extract_video_frames(video_path, output_dir, frame_limit):
    return json.loads(
        str(
            _MEDIA_RUNTIME.extractVideoFrames(
                os.path.realpath(video_path),
                os.path.realpath(output_dir),
                int(frame_limit),
            )
        )
    )
