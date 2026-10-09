#!/usr/bin/env python3
"""
Save the academic website HTML to disk.

Usage:
    python generate_web.py \
        --html-file /tmp/website.html \
        --title "Paper Title" \
        --out-dir ./web/<pdf_name>/

The file is saved as:  <out-dir>/index.html
"""

import argparse
import base64
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


def save_web(html_file: str, title: str, out_dir: str) -> str:
    os.makedirs(out_dir, exist_ok=True)
    dst = os.path.join(out_dir, "index.html")
    source = Path(html_file).resolve()
    document = source.read_text(encoding="utf-8")
    document = _inline_local_images(
        document,
        [source.parent, Path(out_dir).resolve()],
    )
    _reject_remote_assets(document)
    Path(dst).write_text(document, encoding="utf-8")
    return os.path.abspath(dst)


def main():
    parser = argparse.ArgumentParser(description="Save the academic website HTML.")
    parser.add_argument(
        "--html-file", required=True, help="Path to the generated HTML file"
    )
    parser.add_argument("--title", default="Paper", help="Title of the paper")
    parser.add_argument("--out-dir", default="./web_output", help="Output directory")
    args = parser.parse_args()

    dst = save_web(args.html_file, args.title, args.out_dir)
    print(f"✅ 已生成《{args.title}》外部 channel 单文件 HTML → {dst}")


if __name__ == "__main__":
    main()
