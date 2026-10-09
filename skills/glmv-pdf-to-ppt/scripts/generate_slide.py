#!/usr/bin/env python3
"""
Save a slide HTML file and optionally generate the index.html navigation page.

Usage:
    python generate_slide.py \
        --html-file /tmp/slide.html \
        --index 3 --total 12 \
        --title "My Presentation" \
        --out-dir ./ppt/<pdf_name>/

The slide is saved as:  <out-dir>/slide_<index:02d>.html
When index == total, an index.html with slide navigation is auto-generated.
"""

import argparse
import base64
import html
import json
import mimetypes
import os
import re
from html.parser import HTMLParser
from pathlib import Path

IMAGE_SUFFIXES = {".png", ".jpg", ".jpeg", ".gif", ".webp"}
MAX_INLINE_IMAGE_BYTES = 20 * 1024 * 1024
CSS_URL_PATTERN = re.compile(r"url\(\s*([^)]*?)\s*\)", re.IGNORECASE)
CSS_IMPORT_PATTERN = re.compile(r"@import\b", re.IGNORECASE)


class _EmbeddedAssetParser(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.violations: list[str] = []

    def handle_starttag(self, tag, attrs):
        self._check(tag, attrs)

    def handle_startendtag(self, tag, attrs):
        self._check(tag, attrs)

    def _check(self, tag, attrs):
        values = {str(key).lower(): value or "" for key, value in attrs}
        tag = tag.lower()
        if "srcset" in values:
            self.violations.append(f"<{tag}> srcset")
        for key in ("src", "poster"):
            if key in values and not values[key].lstrip().lower().startswith("data:"):
                self.violations.append(f"<{tag}> {key}")
        if tag == "link" and "href" in values:
            if not values["href"].lstrip().lower().startswith("data:"):
                self.violations.append("<link> href")
        if tag == "object" and "data" in values:
            if not values["data"].lstrip().lower().startswith("data:"):
                self.violations.append("<object> data")
        if tag in {"use", "image"}:
            for key in ("href", "xlink:href"):
                if key not in values:
                    continue
                value = values[key].lstrip()
                if not (value.startswith("#") or value.lower().startswith("data:")):
                    self.violations.append(f"<{tag}> {key}")
        if tag == "base" and "href" in values:
            self.violations.append("<base> href")
        if tag == "meta" and values.get("http-equiv", "").lower() == "refresh":
            self.violations.append("<meta> refresh")


def _resolve_local_image(raw: str, base_dirs: list[Path]) -> Path | None:
    path = Path(raw)
    candidates = [path] if path.is_absolute() else [base / path for base in base_dirs]
    roots = [base.resolve() for base in base_dirs]
    for candidate in candidates:
        resolved = candidate.resolve()
        if resolved.suffix.lower() not in IMAGE_SUFFIXES or not resolved.is_file():
            continue
        if resolved.stat().st_size > MAX_INLINE_IMAGE_BYTES:
            continue
        for root in roots:
            try:
                resolved.relative_to(root)
                return resolved
            except ValueError:
                continue
    return None


def _inline_local_images(document: str, base_dirs: list[Path]) -> str:
    pattern = re.compile(
        r'(<img\b[^>]*\bsrc\s*=\s*)(?:(["\'])(.*?)\2|([^\s>]+))',
        re.IGNORECASE,
    )

    def replace(match):
        raw = match.group(3) if match.group(2) else match.group(4)
        if raw.startswith(("data:", "http://", "https://", "#")):
            return match.group(0)
        source = _resolve_local_image(raw, base_dirs)
        if source is None:
            raise ValueError(
                f"Local image must be a supported file inside the task directories: {raw}"
            )
        mime = mimetypes.guess_type(source.name)[0] or "application/octet-stream"
        encoded = base64.b64encode(source.read_bytes()).decode("ascii")
        return f'{match.group(1)}"data:{mime};base64,{encoded}"'

    return pattern.sub(replace, document)


def _reject_remote_assets(document: str) -> None:
    parser = _EmbeddedAssetParser()
    parser.feed(document)
    css_external = False
    for match in CSS_URL_PATTERN.finditer(document):
        value = match.group(1).strip().strip("\"'").lstrip()
        if not (value.startswith("#") or value.lower().startswith("data:")):
            css_external = True
            break
    if parser.violations or css_external or CSS_IMPORT_PATTERN.search(document):
        raise ValueError(
            "External HTML assets are not allowed; inline every asset as a data URL"
        )


def save_slide(html_file: str, index: int, total: int, title: str, out_dir: str) -> str:
    os.makedirs(out_dir, exist_ok=True)
    dst = os.path.join(out_dir, f"slide_{index:02d}.html")
    source = Path(html_file).resolve()
    document = source.read_text(encoding="utf-8")
    document = _inline_local_images(
        document,
        [source.parent, Path(out_dir).resolve()],
    )
    _reject_remote_assets(document)
    Path(dst).write_text(document, encoding="utf-8")
    return os.path.abspath(dst)


def generate_index(total: int, title: str, out_dir: str) -> str:
    """Generate a standalone presentation.html with all slides embedded."""
    slides = []
    for i in range(1, total + 1):
        slide_path = Path(out_dir) / f"slide_{i:02d}.html"
        slides.append(slide_path.read_text(encoding="utf-8"))

    slides_json = json.dumps(slides, ensure_ascii=False).replace("</", "<\\/")
    safe_title = html.escape(title)
    document = f"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{safe_title}</title>
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:system-ui,-apple-system,"Segoe UI",sans-serif;background:#0f172a;color:#e2e8f0;min-height:100vh;display:flex;flex-direction:column}}
header{{height:56px;display:flex;align-items:center;gap:12px;padding:8px 14px;background:#111827;border-bottom:1px solid #334155}}
h1{{font-size:16px;flex:1;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
button{{border:1px solid #475569;background:#1e293b;color:#fff;border-radius:7px;padding:8px 13px;cursor:pointer}}
button:disabled{{opacity:.4;cursor:not-allowed}}
#counter{{min-width:72px;text-align:center;color:#94a3b8}}
iframe{{display:block;width:100%;flex:1;border:0;background:white;min-height:calc(100vh - 56px)}}
</style>
</head>
<body>
<header><h1>{safe_title}</h1><button id="prev">←</button>
<span id="counter"></span><button id="next">→</button></header>
<iframe id="slide" sandbox></iframe>
<script>
const slides = {slides_json};
let index = 0;
const frame = document.getElementById("slide");
const counter = document.getElementById("counter");
const prev = document.getElementById("prev");
const next = document.getElementById("next");
function render() {{
  frame.srcdoc = slides[index];
  counter.textContent = `${{index + 1}} / ${{slides.length}}`;
  prev.disabled = index === 0;
  next.disabled = index === slides.length - 1;
}}
prev.onclick = () => {{ if (index > 0) {{ index--; render(); }} }};
next.onclick = () => {{ if (index + 1 < slides.length) {{ index++; render(); }} }};
addEventListener("keydown", event => {{
  if (event.key === "ArrowLeft") prev.click();
  if (event.key === "ArrowRight" || event.key === " ") next.click();
}});
render();
</script></body>
</html>"""

    index_path = os.path.join(out_dir, "index.html")
    presentation_path = os.path.join(out_dir, "presentation.html")
    Path(index_path).write_text(document, encoding="utf-8")
    Path(presentation_path).write_text(document, encoding="utf-8")
    return os.path.abspath(presentation_path)


def main():
    parser = argparse.ArgumentParser(
        description="Save a slide HTML file and report progress."
    )
    parser.add_argument(
        "--html-file", required=True, help="Path to the rendered HTML file"
    )
    parser.add_argument("--index", type=int, required=True, help="1-based slide index")
    parser.add_argument("--total", type=int, required=True, help="Total slides")
    parser.add_argument("--title", default="Presentation", help="Presentation title")
    parser.add_argument("--out-dir", default="./slides_output", help="Output directory")
    args = parser.parse_args()
    if args.total < 1:
        parser.error("--total must be at least 1")
    if args.index < 1 or args.index > args.total:
        parser.error("--index must be between 1 and --total")

    dst = save_slide(args.html_file, args.index, args.total, args.title, args.out_dir)

    if args.index < args.total:
        print(f"[{args.index}/{args.total}] 已保存第 {args.index} 页 → {dst}")
        print(f"请继续生成第 {args.index + 1} 页。")
    else:
        print(f"[{args.index}/{args.total}] 已保存第 {args.index} 页 → {dst}")
        idx = generate_index(args.total, args.title, args.out_dir)
        print(f"✅ 《{args.title}》全部 {args.total} 页 HTML 预览生成完成！")
        print(f"📂 输出目录：{os.path.abspath(args.out_dir)}")
        print(f"🌐 外部 channel 单文件 HTML：{idx}")


if __name__ == "__main__":
    main()
