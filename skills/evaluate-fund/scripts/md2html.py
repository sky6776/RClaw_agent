#!/usr/bin/env python3
"""Render a safe, self-contained fund report and publish it for RClaw media channels."""

from __future__ import annotations

import argparse
import base64
import hashlib
import html
import os
import re
import secrets
import shutil
import sys
import tempfile
from pathlib import Path
from typing import Iterable
from urllib.parse import unquote, urlparse


CSS = """
:root { color-scheme: light; --ink:#1e293b; --muted:#64748b; --line:#dbe4ee;
  --brand:#0f766e; --paper:#ffffff; --wash:#f1f5f9; --warn:#fff7ed; }
* { box-sizing:border-box; }
body { margin:0; background:var(--wash); color:var(--ink); font:16px/1.7 -apple-system,
  BlinkMacSystemFont,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif; }
main { max-width:1040px; margin:32px auto; padding:48px 56px; background:var(--paper);
  border:1px solid var(--line); border-radius:14px; box-shadow:0 10px 30px #0f172a12; }
h1,h2,h3,h4 { line-height:1.3; color:#0f172a; margin:1.5em 0 .6em; }
h1 { margin-top:0; color:var(--brand); font-size:2rem; }
h2 { border-bottom:2px solid #ccfbf1; padding-bottom:.3em; font-size:1.45rem; }
h3 { font-size:1.18rem; }
p { margin:.65em 0; } a { color:#0369a1; overflow-wrap:anywhere; }
table { width:100%; border-collapse:collapse; margin:1em 0 1.4em; font-size:.94rem; }
th,td { padding:.6em .7em; border:1px solid var(--line); vertical-align:top; }
th { background:#ecfdf5; text-align:left; } tr:nth-child(even) td { background:#f8fafc; }
blockquote { margin:1em 0; padding:.7em 1em; background:var(--warn); border-left:4px solid #f59e0b; }
code { background:#eef2f7; border-radius:4px; padding:.12em .35em; font-family:Consolas,monospace; }
pre { overflow:auto; padding:1em; background:#0f172a; color:#e2e8f0; border-radius:8px; }
pre code { padding:0; background:none; color:inherit; }
img { max-width:100%; height:auto; border-radius:8px; }
ul,ol { padding-left:1.5em; } hr { border:0; border-top:1px solid var(--line); margin:2em 0; }
.report-title { margin-bottom:.2em; } .subtitle { color:var(--muted); margin-top:0; }
@media (max-width:700px) { main { margin:0; padding:24px 18px; border:0; border-radius:0; }
  table { display:block; overflow-x:auto; white-space:nowrap; } }
@media print { body { background:#fff; } main { margin:0; max-width:none; border:0; box-shadow:none; } }
"""
SKILL_DIR = Path(__file__).resolve().parent.parent
IMAGE_MIME_BY_SUFFIX = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".gif": "image/gif",
    ".webp": "image/webp",
    ".bmp": "image/bmp",
}


def image_signature_matches(path: Path, mime: str) -> bool:
    with path.open("rb") as handle:
        header = handle.read(12)
    if mime == "image/png":
        return header.startswith(b"\x89PNG\r\n\x1a\n")
    if mime == "image/jpeg":
        return header.startswith(b"\xff\xd8\xff")
    if mime == "image/gif":
        return header.startswith((b"GIF87a", b"GIF89a"))
    if mime == "image/webp":
        return len(header) >= 12 and header[:4] == b"RIFF" and header[8:12] == b"WEBP"
    if mime == "image/bmp":
        return header.startswith(b"BM")
    return False


def safe_url(raw_url: str, base_dir: Path, embed_images: bool, image: bool) -> str | None:
    value = html.unescape(raw_url).strip().strip("<>")
    normalized = re.sub(r"[\x00-\x20]+", "", unquote(value)).lower()
    if normalized.startswith(("javascript:", "vbscript:", "data:")):
        return None
    parsed = urlparse(value)
    if parsed.scheme in {"http", "https"}:
        if image and embed_images:
            raise ValueError("external-channel HTML requires local images inside the report directory")
        return value
    if not image and (parsed.scheme == "mailto" or value.startswith("#")):
        return value
    if parsed.scheme or value.startswith("//"):
        return None
    if not image:
        return value

    candidate = (base_dir / unquote(value)).resolve()
    try:
        candidate.relative_to(base_dir.resolve())
    except ValueError:
        return None
    if not candidate.is_file():
        return None
    if embed_images:
        if candidate.stat().st_size > 20 * 1024 * 1024:
            raise ValueError(f"image is larger than 20 MiB: {candidate}")
        mime = IMAGE_MIME_BY_SUFFIX.get(candidate.suffix.lower(), "")
        if not mime:
            raise ValueError(f"unsupported local image type: {candidate}")
        if not image_signature_matches(candidate, mime):
            raise ValueError(f"image content does not match its extension: {candidate}")
        encoded = base64.b64encode(candidate.read_bytes()).decode("ascii")
        return f"data:{mime};base64,{encoded}"
    return candidate.as_uri()


def render_inline(text: str, base_dir: Path, embed_images: bool) -> str:
    code_values: list[str] = []

    def stash_code(match: re.Match[str]) -> str:
        code_values.append(f"<code>{html.escape(match.group(1))}</code>")
        return f"\x00CODE{len(code_values) - 1}\x00"

    escaped = html.escape(re.sub(r"`([^`]+)`", stash_code, text), quote=False)

    def image_replace(match: re.Match[str]) -> str:
        alt = html.unescape(match.group(1))
        source = safe_url(match.group(2), base_dir, embed_images, image=True)
        return html.escape(alt) if source is None else f'<img src="{html.escape(source, quote=True)}" alt="{html.escape(alt, quote=True)}">'

    def link_replace(match: re.Match[str]) -> str:
        label = match.group(1)
        target = safe_url(match.group(2), base_dir, embed_images, image=False)
        return label if target is None else f'<a href="{html.escape(target, quote=True)}">{label}</a>'

    target_pattern = r"(<[^>]+>|(?:[^()]|\([^()]*\)){1,})"
    escaped = re.sub(rf"!\[([^]]*)\]\({target_pattern}\)", image_replace, escaped)
    escaped = re.sub(rf"\[([^]]+)\]\({target_pattern}\)", link_replace, escaped)
    escaped = re.sub(r"\*\*([^*]+)\*\*", r"<strong>\1</strong>", escaped)
    escaped = re.sub(r"(?<!\*)\*([^*]+)\*(?!\*)", r"<em>\1</em>", escaped)
    for index, value in enumerate(code_values):
        escaped = escaped.replace(f"\x00CODE{index}\x00", value)
    return escaped


def split_table_row(line: str) -> list[str]:
    stripped = line.strip().strip("|")
    return [cell.strip() for cell in stripped.split("|")]


def is_table_separator(line: str) -> bool:
    cells = split_table_row(line)
    return bool(cells) and all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells)


def markdown_to_body(markdown: str, base_dir: Path, embed_images: bool) -> str:
    lines = markdown.replace("\r\n", "\n").replace("\r", "\n").split("\n")
    output: list[str] = []
    index = 0

    while index < len(lines):
        line = lines[index]
        stripped = line.strip()
        if not stripped:
            index += 1
            continue

        fence = re.match(r"^\s*```([^`]*)$", line)
        if fence:
            language = fence.group(1).strip()
            code_lines: list[str] = []
            index += 1
            while index < len(lines) and not re.match(r"^\s*```\s*$", lines[index]):
                code_lines.append(lines[index])
                index += 1
            index += 1 if index < len(lines) else 0
            class_name = f' class="language-{html.escape(language, quote=True)}"' if language else ""
            output.append(f"<pre><code{class_name}>{html.escape(chr(10).join(code_lines))}</code></pre>")
            continue

        heading = re.match(r"^(#{1,6})\s+(.+?)\s*#*$", line)
        if heading:
            level = len(heading.group(1))
            output.append(f"<h{level}>{render_inline(heading.group(2), base_dir, embed_images)}</h{level}>")
            index += 1
            continue

        if re.fullmatch(r"\s*(?:-{3,}|\*{3,}|_{3,})\s*", line):
            output.append("<hr>")
            index += 1
            continue

        if "|" in line and index + 1 < len(lines) and is_table_separator(lines[index + 1]):
            headers = split_table_row(line)
            index += 2
            rows: list[list[str]] = []
            while index < len(lines) and "|" in lines[index] and lines[index].strip():
                rows.append(split_table_row(lines[index]))
                index += 1
            output.append("<table><thead><tr>" + "".join(
                f"<th>{render_inline(cell, base_dir, embed_images)}</th>" for cell in headers
            ) + "</tr></thead><tbody>")
            for row in rows:
                padded = (row + [""] * len(headers))[: len(headers)]
                output.append("<tr>" + "".join(
                    f"<td>{render_inline(cell, base_dir, embed_images)}</td>" for cell in padded
                ) + "</tr>")
            output.append("</tbody></table>")
            continue

        if re.match(r"^\s*>\s?", line):
            parts: list[str] = []
            while index < len(lines) and re.match(r"^\s*>\s?", lines[index]):
                parts.append(re.sub(r"^\s*>\s?", "", lines[index]))
                index += 1
            output.append(f"<blockquote>{render_inline(' '.join(parts), base_dir, embed_images)}</blockquote>")
            continue

        list_match = re.match(r"^\s*([-+*]|\d+[.)])\s+(.+)$", line)
        if list_match:
            ordered = bool(re.match(r"\d", list_match.group(1)))
            tag = "ol" if ordered else "ul"
            items: list[str] = []
            while index < len(lines):
                current = re.match(r"^\s*([-+*]|\d+[.)])\s+(.+)$", lines[index])
                if not current or bool(re.match(r"\d", current.group(1))) != ordered:
                    break
                items.append(current.group(2))
                index += 1
            output.append(f"<{tag}>" + "".join(
                f"<li>{render_inline(item, base_dir, embed_images)}</li>" for item in items
            ) + f"</{tag}>")
            continue

        paragraph = [stripped]
        index += 1
        while index < len(lines) and lines[index].strip():
            candidate = lines[index]
            if (
                re.match(r"^(#{1,6})\s+", candidate)
                or re.match(r"^\s*```", candidate)
                or re.match(r"^\s*([-+*]|\d+[.)])\s+", candidate)
                or re.match(r"^\s*>\s?", candidate)
                or ("|" in candidate and index + 1 < len(lines) and is_table_separator(lines[index + 1]))
            ):
                break
            paragraph.append(candidate.strip())
            index += 1
        output.append(f"<p>{render_inline(' '.join(paragraph), base_dir, embed_images)}</p>")

    return "\n".join(output)


def render_document(markdown: str, title: str, base_dir: Path, embed_images: bool) -> str:
    body = markdown_to_body(markdown, base_dir, embed_images)
    return "\n".join(
        [
            "<!doctype html>",
            '<html lang="zh-CN"><head>',
            '<meta charset="utf-8">',
            '<meta name="viewport" content="width=device-width,initial-scale=1">',
            '<meta http-equiv="Content-Security-Policy" content="default-src \'none\'; img-src data: file: https: http:; style-src \'unsafe-inline\'">',
            f"<title>{html.escape(title)}</title>",
            f"<style>{CSS}</style>",
            "</head><body><main>",
            body,
            "</main></body></html>",
        ]
    )


def canonical_channel(channel: str) -> str:
    value = channel.strip().lower()
    value = "napcat" if value == "napcatqq" else value
    # channel 会进入 marker 前缀和能力变量名；限制为有界 ASCII token，
    # 避免来源文本改变 marker 结构或构造意外的环境变量名。
    return (
        value
        if len(value) <= 64 and re.fullmatch(r"[a-z][a-z0-9_-]*", value)
        else ""
    )


def channel_enabled(channel: str) -> bool:
    canonical = canonical_channel(channel)
    if canonical in {"", "cli", "daemon", "system"}:
        return False
    names = {canonical, channel.strip().lower()}
    # 该变量是“通道已实现媒体解析”的显式能力契约，不是宽松用户布尔配置；
    # 与运行时和通用发布器一样只接受精确的 `1`，避免误为文本 channel 生成 marker。
    return any(
        os.getenv(f"RCLAW_{name.upper().replace('-', '_')}_ENABLED") == "1"
        for name in names
    )


def publish_file(source: Path, channel: str, caption: str) -> tuple[Path, str] | None:
    media_root = (os.getenv("RCLAW_MEDIA_DIR") or "").strip()
    if not media_root:
        raise ValueError("RCLAW_MEDIA_DIR is required for an enabled external channel")
    safe_name = re.sub(r"[^A-Za-z0-9._\u4e00-\u9fff-]+", "_", source.name)
    # marker 中的路径保留 RCLAW_MEDIA_DIR 的词法前缀：Android 上 /data/user/0 是
    # /data/data 的符号链接，resolve() 后前缀改变，运行时的词法沙箱校验会拒绝
    # 整条 marker（物理位置由运行时 canonicalize 兜底）。
    output_dir = Path(media_root) / "out"
    destination_root = output_dir.expanduser().resolve()
    temporary = None
    try:
        # marker 可能在通道真正读取前短暂排队。先复制到临时文件，再以完整内容哈希
        # 选择目录并原子发布，避免同名新报告覆盖旧 marker 将要发送的内容。
        destination_root.mkdir(parents=True, exist_ok=True)
        fd, temporary_name = tempfile.mkstemp(prefix=".publish-", dir=destination_root)
        os.close(fd)
        temporary = Path(temporary_name)
        shutil.copy2(source, temporary)
        content_digest = hashlib.sha256()
        with temporary.open("rb") as handle:
            for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                content_digest.update(chunk)
        digest = content_digest.hexdigest()
        destination_dir = destination_root / digest
        destination_dir.mkdir(parents=True, exist_ok=True)
        destination = destination_dir / safe_name
        try:
            os.replace(temporary, destination)
        except OSError:
            existing_digest = hashlib.sha256()
            if destination.is_file():
                with destination.open("rb") as handle:
                    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
                        existing_digest.update(chunk)
            if not destination.is_file() or existing_digest.hexdigest() != digest:
                raise
            temporary.unlink()
        temporary = None
    except OSError as exc:
        print(f"warning: media publish failed for {source}: {exc}", file=sys.stderr)
        return None
    finally:
        if temporary is not None:
            try:
                temporary.unlink()
            except FileNotFoundError:
                pass
    clean_path = (output_dir / digest / safe_name).as_posix().replace('"', "")
    clean_caption = caption.replace('"', "")
    canonical = canonical_channel(channel)
    # 同一内容可以落到同一哈希路径，但每次真实发布需要新 delivery id；否则
    # AgentStatus/AgentWait 重放旧结果时无法与用户撤销后的重新发布区分。
    delivery = secrets.token_hex(16)
    if canonical == "napcat":
        marker = (
            f'[:napcat-media type=file path="{clean_path}" name="{safe_name}" '
            f'caption="{clean_caption}" delivery="{delivery}"]'
        )
    elif canonical == "weixin":
        marker = (
            f'[:weixin-media type=file path="{clean_path}" caption="{clean_caption}" '
            f'delivery="{delivery}"]'
        )
    else:
        marker = (
            f'[:{canonical}-media type=file path="{clean_path}" name="{safe_name}" '
            f'caption="{clean_caption}" delivery="{delivery}"]'
        )
    return destination, marker


def run_self_test() -> None:
    source = """# 基金报告

| 指标 | 数值 |
|---|---:|
| 回报 | **12%** |

- 风险一
- 风险二

<script>alert(1)</script> [危险](javascript:alert(1))
"""
    rendered = render_document(source, "测试", Path.cwd(), False)
    assert "<h1>基金报告</h1>" in rendered
    assert "<table>" in rendered
    assert "<strong>12%</strong>" in rendered
    assert "&lt;script&gt;" in rendered
    assert "javascript:" not in rendered
    assert safe_url("java\tscript:alert(1)", Path.cwd(), False, False) is None
    parenthesized = render_inline("[链接](https://example.com/a_(b))", Path.cwd(), False)
    assert 'href="https://example.com/a_(b)"' in parenthesized
    print("self-test passed")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Convert a Markdown report to safe standalone HTML.")
    parser.add_argument("output_html", nargs="?", type=Path, help="HTML output path")
    parser.add_argument("-i", "--input", type=Path, help="Markdown input path")
    parser.add_argument("--title", default="基金评估报告", help="HTML document title")
    parser.add_argument(
        "--source-channel",
        default="",
        help="直接运行时的 source channel；RClaw 注入的 RCLAW_SOURCE_CHANNEL 优先",
    )
    parser.add_argument("--embed-images", action="store_true", help="embed local report images as data URLs")
    parser.add_argument("--no-md", action="store_true", help="do not publish the Markdown companion")
    parser.add_argument("--self-test", action="store_true", help="run deterministic built-in tests")
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.self_test:
        run_self_test()
        return 0
    if args.input is None or args.output_html is None:
        raise ValueError("output_html and --input are required unless --self-test is used")
    input_path = args.input.expanduser().resolve()
    output_path = args.output_html.expanduser().resolve()
    if input_path == output_path:
        raise ValueError("output_html must be different from the Markdown input path")
    try:
        output_path.relative_to(SKILL_DIR)
    except ValueError:
        pass
    else:
        raise ValueError("output_html must be outside the skill directory")
    # 所有外部 channel 共用媒体沙箱，目录无法说明 marker 应归属哪个 channel。
    # 因此逐轮运行时来源必须压过模型可能手填的参数；参数只供脱离 RClaw 直跑。
    runtime_channel = (os.getenv("RCLAW_SOURCE_CHANNEL") or "").strip()
    channel = runtime_channel or args.source_channel
    external = channel_enabled(channel)
    markdown = input_path.read_text(encoding="utf-8-sig")
    rendered = render_document(markdown, args.title, input_path.parent, args.embed_images or external)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(rendered + "\n", encoding="utf-8")
    print(f"OUTPUT_HTML={output_path}")

    if external:
        if not (os.getenv("RCLAW_MEDIA_DIR") or "").strip():
            print("warning: RCLAW_MEDIA_DIR is empty; report files were not published", file=sys.stderr)
        else:
            html_publish = publish_file(output_path, channel, "HTML详细报告")
            md_publish = None
            if not args.no_md:
                md_publish = publish_file(input_path, channel, "Markdown原始报告")
            if html_publish and (args.no_md or md_publish):
                print(f"MEDIA_MARKER={html_publish[1]}")
                if md_publish:
                    print(f"MEDIA_MARKER={md_publish[1]}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
