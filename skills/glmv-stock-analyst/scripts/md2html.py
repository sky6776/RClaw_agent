#!/usr/bin/env python3
"""
md2html.py — 将 Markdown 报告转换为带专业 CSS 的 HTML 页面
支持：表格、代码块、emoji、图片（本地 file:// 或 base64 内嵌）、文本折线图

外部 channel 下自动内嵌图片为 base64，并同时发布 HTML + 源 MD 两个媒体
标记；CLI/本地默认用 file:// 绝对路径供浏览器查看、不内嵌。

用法:
  python3 md2html.py report.html < report.md
  python3 md2html.py -o report.html report.md
"""

import argparse
import base64
import hashlib
import html
import json
import os
import re
import secrets
import shutil
import sys
import tempfile
from datetime import datetime
from typing import Optional

import bleach
import markdown
from PIL import Image, UnidentifiedImageError

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

# ═══════════════════════════════════════════════
# 专业报告 CSS 模板
# ═══════════════════════════════════════════════
CSS_TEMPLATE = """
<style>
*{margin:0;padding:0;box-sizing:border-box;}
html{font-size:16px;}
body{
  font-family:-apple-system,BlinkMacSystemFont,"Segoe UI","PingFang SC",
             "Hiragino Sans GB","Microsoft YaHei","Noto Sans CJK SC",sans-serif;
  line-height:1.85;color:#2c3e50;background:#f5f7fa;
}
.container{max-width:860px;margin:0 auto;padding:30px 24px;}

/* 标题 */
h1{
  font-size:1.6em;color:#1a1a2e;border-bottom:3px solid #3498db;
  padding-bottom:10px;margin-bottom:6px;font-weight:700;
}
h1 .meta{display:block;font-size:0.45em;color:#888;font-weight:400;margin-top:6px;}
h2{
  font-size:1.3em;color:#2980b9;margin-top:2em;margin-bottom:0.8em;
  padding-left:12px;border-left:4px solid #3498db;font-weight:700;
}
h3{font-size:1.1em;color:#34495e;margin-top:1.5em;margin-bottom:0.6em;font-weight:600;}

/* 段落 */
p{margin-bottom:0.9em;font-size:0.95em;text-align:justify;}

/* 表格 — 报告核心元素 */
table{
  width:100%;border-collapse:collapse;margin:1.2em 0;font-size:0.88em;
  border-radius:6px;overflow:hidden;box-shadow:0 1px 3px rgba(0,0,0,0.08);
}
thead{background:linear-gradient(135deg,#2980b9,#3498db);}
th{
  color:#fff;font-weight:600;padding:10px 14px;text-align:left;
  font-size:0.95em;letter-spacing:0.3px;
}
td{padding:9px 14px;border-bottom:1px solid #ecf0f1;color:#444;}
tbody tr:nth-child(even){background:#fafbfc;}
tbody tr:hover{background:#f0f7ff;}
tbody tr:last-child td{border-bottom:none;}

/* 列表 */
ul,ol{padding-left:1.6em;margin:0.8em 0;font-size:0.9em;line-height:1.75;}
li{margin-bottom:0.35em;}
li::marker{color:#3498db;}

/* 引用块（用于核心判断） */
blockquote{
  margin:1.2em 0;padding:14px 20px;background:linear-gradient(135deg,#fef9e7,#fdf2e9);
  border-left:4px solid #f39c12;border-radius:0 8px 8px 0;
  color:#7d6608;font-size:0.9em;line-height:1.7;
}
blockquote p:last-child{margin-bottom:0;}

/* 代码/折线图 */
code{
  background:#f1f3f5;padding:2px 6px;border-radius:4px;
  font-family:"SF Mono",Monaco,Consolas,monospace;font-size:0.85em;color:#e74c3c;
}
pre{
  background:#1e272e;color:#dcdcdc;border-radius:8px;padding:18px 20px;
  margin:1.2em 0;overflow-x:auto;font-size:0.82em;line-height:1.6;
  box-shadow:inset 0 1px 4px rgba(0,0,0,0.15);
}
pre code{background:none;padding:0;color:inherit;font-size:1em;}

/* 分隔线 */
hr{
  border:none;height:2px;background:linear-gradient(to right,
    transparent,#bdc3c7,transparent);margin:2.5em 0;
}

/* 图片 */
p > img{max-width:100%;height:auto;border-radius:8px;
  box-shadow:0 2px 12px rgba(0,0,0,0.1);margin:1em auto;display:block;}
figure{margin:1.5em 0;text-align:center;}
figure img{max-width:100%;height:auto;border-radius:8px;
  box-shadow:0 2px 12px rgba(0,0,0,0.1);}
figcaption{color:#888;font-size:0.82em;margin-top:8px;font-style:italic;}

/* 强调 */
strong{color:#1a1a2e;font-weight:600;}
em{color:#c0392b;}

/* 链接 */
a{color:#2980b9;text-decoration:none;border-bottom:1px dotted #2980b9;}
a:hover{color:#1abc9c;border-bottom-style:solid;}

/* 免责声明 */
.disclaimer{
  background:#ecf0f1;border-left:4px solid #95a5a6;
  padding:14px 18px;margin-top:2.5em;border-radius:0 8px 8px 0;
  color:#7f8c8d;font-size:0.82em;line-height:1.65;
}

/* KPI 条（由 meta.json 渲染，注入在 bleach 之后，故可用 div/span + class） */
.kpi-strip{display:flex;flex-wrap:wrap;gap:8px;margin:0 0 1.6em;padding:14px 16px;
  background:linear-gradient(135deg,#f8fbff,#eef4fb);border:1px solid #dce7f3;
  border-radius:10px;box-shadow:0 1px 3px rgba(0,0,0,0.04);}
.kpi-tile{display:flex;flex-direction:column;min-width:60px;padding:2px 12px;}
.kpi-tile + .kpi-tile{border-left:1px solid #e2eaf2;}
.kpi-label{font-size:0.7em;color:#8a97a8;letter-spacing:0.3px;}
.kpi-value{font-size:1.02em;font-weight:700;color:#1a1a2e;margin-top:3px;}
.kpi-tile.kpi-up .kpi-value{color:#16a34a;}
.kpi-tile.kpi-wait .kpi-value{color:#d97706;}
.kpi-tile.kpi-down .kpi-value{color:#dc2626;}
@media(max-width:640px){.kpi-strip{padding:10px;gap:4px;}.kpi-tile{min-width:52px;padding:2px 8px;}}

/* 响应式 */
@media(max-width:640px){
  .container{padding:16px 12px;}
  h1{font-size:1.35em;}
  table{font-size:0.78em;}
  th,td{padding:7px 8px;}
}

/* 打印优化 */
@media print{
  body{background:#fff;color:#000;}
  .container{max-width:100%;padding:0;}
  table{page-break-inside:avoid;}
  h2{page-break-after:avoid;}
}
</style>
"""


_IMG_MIME = {
    "png": "image/png",
    "jpg": "image/jpeg",
    "jpeg": "image/jpeg",
    "gif": "image/gif",
    "webp": "image/webp",
    "bmp": "image/bmp",
}
_MAX_EMBED_IMAGE_BYTES = 20 * 1024 * 1024
_MAX_EMBED_IMAGE_PIXELS = 50_000_000

_HTML_TAGS = {
    "a",
    "blockquote",
    "br",
    "code",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "img",
    "li",
    "ol",
    "p",
    "pre",
    "strong",
    "table",
    "tbody",
    "td",
    "th",
    "thead",
    "tr",
    "ul",
}


def _safe_html_attribute(
    tag: str, name: str, value: str, *, embedded_images_only: bool
) -> bool:
    if tag == "a":
        return name in {"href", "title"} and not value.lstrip().lower().startswith(
            ("data:", "file:", "javascript:", "vbscript:")
        )
    if tag == "img":
        if name in {"alt", "title"}:
            return True
        if name == "src":
            lowered = value.lstrip().lower()
            embedded = re.match(
                r"^data:image/(?:png|jpeg|gif|webp|bmp);base64,", lowered
            )
            if embedded_images_only:
                return bool(embedded)
            return bool(embedded) or lowered.startswith(
                ("file://", "http://", "https://")
            ) or not re.match(r"^[a-z][a-z0-9+.-]*:", lowered)
    return tag == "code" and name == "class" and value.startswith("language-")


def _sanitize_html(html_text: str, *, embedded_images_only: bool) -> str:
    """仅保留报告展示所需标签和属性，移除脚本、事件处理器及危险 URL。"""
    return bleach.clean(
        html_text,
        tags=_HTML_TAGS,
        attributes=lambda tag, name, value: _safe_html_attribute(
            tag, name, value, embedded_images_only=embedded_images_only
        ),
        protocols={"data", "file", "http", "https", "mailto"},
        strip=True,
        strip_comments=True,
    )


def _local_image_path(src: str, base_dir: str) -> str:
    """解析报告目录内的本地栅格图片；目录逃逸、符号链接逃逸和非图片均拒绝。"""
    if src.lstrip().lower().startswith(
        ("http://", "https://", "data:", "//", "#", "file:")
    ):
        return ""

    root = os.path.realpath(base_dir)
    candidate = os.path.realpath(src if os.path.isabs(src) else os.path.join(root, src))
    try:
        if os.path.commonpath((root, candidate)) != root:
            return ""
    except ValueError:
        # Windows 不同盘符无法比较，必然不在报告目录内。
        return ""

    ext = os.path.splitext(candidate)[1].lower().lstrip(".")
    if ext not in _IMG_MIME or not os.path.isfile(candidate):
        return ""
    try:
        if os.path.getsize(candidate) > _MAX_EMBED_IMAGE_BYTES:
            return ""
        with Image.open(candidate) as image:
            if image.width * image.height > _MAX_EMBED_IMAGE_PIXELS:
                return ""
            if Image.MIME.get(image.format) != _IMG_MIME[ext]:
                return ""
            image.verify()
    except (OSError, UnidentifiedImageError, ValueError, Image.DecompressionBombError):
        return ""
    return candidate


def _data_uri(path: str) -> str:
    """读取本地图片并以 base64 data URI 返回，便于 HTML 自包含携带。"""
    ext = os.path.splitext(path)[1].lower().lstrip(".")
    mime = _IMG_MIME[ext]
    with open(path, "rb") as f:
        b64 = base64.b64encode(f.read()).decode("ascii")
    return f"data:{mime};base64,{b64}"


def _kpi_num(value, fmt="{}"):
    if value is None or value == "":
        return ""
    try:
        return fmt.format(value)
    except (TypeError, ValueError):
        return str(value)


def _kpi_pct(value, fmt):
    text = _kpi_num(value, fmt)
    return text if text.rstrip().endswith("%") else text + "%"


def _kpi_levels(value):
    if isinstance(value, list):
        return "/".join(_kpi_num(x) for x in value if x not in (None, ""))
    return _kpi_num(value)


def build_kpi_html(meta):
    """由 META 字段生成顶部 KPI 条 HTML。代码生成、所有动态值经 html.escape；
    在 _sanitize_html 之后注入，故不受 bleach 白名单限制。meta 为空返回空串。"""
    if not isinstance(meta, dict) or not meta:
        return ""
    esc = html.escape

    def tile(label, value, extra_cls=""):
        if value in (None, ""):
            return ""
        return (
            f'<div class="kpi-tile{extra_cls}">'
            f'<span class="kpi-label">{esc(str(label))}</span>'
            f'<span class="kpi-value">{esc(str(value))}</span></div>'
        )

    tiles = []
    view = meta.get("投资观点")
    if view not in (None, ""):
        cls = {
            "看多": " kpi-up",
            "中性": " kpi-wait",
            "看空": " kpi-down",
        }.get(str(view), "")
        conf = meta.get("观点置信度")
        value = str(view) + (
            f"（置信 {_kpi_num(conf, '{:.2f}')}）"
            if conf not in (None, "")
            else ""
        )
        tiles.append(tile("投资观点", value, cls))

    action = meta.get("行动建议")
    if action not in (None, ""):
        cls = {
            "买入": " kpi-up",
            "观望": " kpi-wait",
            "回避": " kpi-down",
        }.get(str(action), "")
        tiles.append(tile("行动建议", action, cls))

    risk = meta.get("风险等级")
    if risk not in (None, ""):
        cls = {"低": " kpi-up", "中": " kpi-wait", "高": " kpi-down"}.get(
            str(risk), ""
        )
        tiles.append(tile("风险等级", risk, cls))

    tiles.append(tile("数据截至", meta.get("数据截至")))
    currency = str(meta.get("币种") or "").strip()

    def price_text(value):
        prefix = f"{currency} " if currency else ""
        return prefix + _kpi_num(value)

    current = meta.get("现价")
    tiles.append(tile("现价", price_text(current) if current not in (None, "") else ""))

    target = meta.get("目标价")
    if target not in (None, ""):
        upside = meta.get("上行空间_pct")
        tv = price_text(target)
        if upside not in (None, ""):
            sign = "+" if (isinstance(upside, (int, float)) and upside > 0) else ""
            tv += f"（{sign}{_kpi_pct(upside, '{:.1f}')}）"
        tiles.append(tile("目标价", tv))

    horizon = meta.get("目标期限月")
    if horizon not in (None, ""):
        tiles.append(tile("目标期限", _kpi_num(horizon) + "个月"))
    tiles.append(tile("估值方法", meta.get("估值方法")))
    tiles.append(tile("PE", _kpi_num(meta.get("PE_ttm"))))
    tiles.append(tile("PB", _kpi_num(meta.get("PB"))))
    divy = meta.get("股息率_pct")
    if divy not in (None, ""):
        tiles.append(tile("股息率", _kpi_pct(divy, "{:.2f}")))
    tiles.append(tile("支撑", _kpi_levels(meta.get("支撑位"))))
    tiles.append(tile("压力", _kpi_levels(meta.get("压力位"))))
    tiles.append(tile("止损", _kpi_num(meta.get("止损位"))))

    tiles = [t for t in tiles if t]
    if not tiles:
        return ""
    return '<div class="kpi-strip">' + "".join(tiles) + "</div>"


def convert(
    md_text: str,
    title: str = "分析报告",
    base_dir: str = "",
    embed_images: bool = False,
    meta: dict = None,
) -> str:
    """将 Markdown 文本转为完整 HTML 页面。

    embed_images=True 时把本地图片内嵌为 base64 data URI，生成的 HTML 自包含、
    可随消息发到外部通道（weixin/napcat）后离线看图；默认（False）仍用 file:// 绝对
    路径，适合本地浏览器查看、文件更小。
    """

    # 处理图片相对路径：外部通道内嵌 base64，本地用 file:// 绝对路径
    if base_dir:
        if embed_images:

            def _fix_img(m):
                alt, src = m.group(1), m.group(2)
                path = _local_image_path(src, base_dir)
                return f"![{alt}]({_data_uri(path)})" if path else m.group(0)

        else:

            def _fix_img(m):
                alt, src = m.group(1), m.group(2)
                if src.startswith(("http", "data:", "//", "#", "/")):
                    return m.group(0)
                abs_path = os.path.abspath(
                    src if os.path.isabs(src) else os.path.join(base_dir, src)
                )
                return f"![{alt}](file://{abs_path})"

        md_text = re.sub(r"!\[([^\]]*)\]\(([^)]+)\)", _fix_img, md_text)

    # Markdown 扩展
    md_ext = [
        "tables",  # 支持表格
        "fenced_code",  # 代码块 ```
        "nl2br",  # 换行
        "sane_lists",  # 规范列表
        # 'smart_strong',   # 非标准扩展，移除
    ]

    html_body = markdown.markdown(md_text, extensions=md_ext)

    # 提取 <h1> 作为页面标题（如果没有自定义标题）
    if not title or title == "分析报告":
        m = re.search(r"<h1[^>]*>(.*?)</h1>", html_body)
        if m:
            title = html.unescape(re.sub(r"<[^>]+>", "", m.group(1)))
        elif not title:
            title = "分析报告"

    html_body = _sanitize_html(html_body, embedded_images_only=embed_images)
    safe_title = html.escape(title, quote=True)
    kpi_html = build_kpi_html(meta) if meta else ""

    full_html = f"""<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width, initial-scale=1.0">
<title>{safe_title}</title>
{CSS_TEMPLATE}
</head>
<body>
<div class="container">
{kpi_html}
{html_body}
</div>
</body>
</html>"""
    return full_html


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


def report_attachment_names(report_path: str, generated_at=None):
    """生成外部 channel 收件人可见的 HTML/MD 报告文件名。"""
    report_path = os.path.abspath(report_path)
    report_dir = os.path.dirname(report_path)
    summary_path = os.path.join(report_dir, "summary.json")
    summary = {}
    try:
        with open(summary_path, "r", encoding="utf-8") as f:
            loaded = json.load(f)
        if isinstance(loaded, dict):
            summary = loaded
    except (OSError, UnicodeError, json.JSONDecodeError):
        pass

    stock_name = str(summary.get("stock_name") or "").strip()
    stock_code = str(summary.get("stock_code") or "").strip()
    if not stock_name or not stock_code:
        try:
            with open(report_path, "r", encoding="utf-8") as f:
                heading_text = f.read(4096)
            heading = re.search(
                r"(?m)^#\s+(.+?)\s*[（(]([^()（）]+)[）)]", heading_text
            )
            if heading:
                stock_name = stock_name or _EMOJI_RE.sub("", heading.group(1)).strip()
                stock_code = stock_code or heading.group(2).strip()
        except (OSError, UnicodeError):
            pass

    stock_name = stock_name or "股票"
    if not stock_code:
        parent = os.path.basename(report_dir)
        match = re.match(r"(.+?)_\d{8}_\d{4}(?:\d{2})?$", parent)
        stock_code = match.group(1) if match else "未知代码"

    if generated_at is None:
        try:
            generated_at = datetime.fromtimestamp(os.path.getmtime(report_path))
        except OSError:
            generated_at = datetime.now()
    timestamp = generated_at.strftime("%Y%m%d_%H%M%S")
    stem = safe_filename(f"{stock_name}_{stock_code}_{timestamp}")
    return f"{stem}.html", f"{stem}.md"


def _channel_enabled(source_channel: str) -> str:
    """返回已使能的 RClaw 外部 channel id。"""
    ch = (source_channel or "").strip().lower()
    if ch == "napcatqq":
        ch = "napcat"
    # channel 会进入 marker 前缀和能力变量名；限制为有界 ASCII token，
    # 避免来源文本改变 marker 结构或构造意外的环境变量名。
    if len(ch) > 64 or not re.fullmatch(r"[a-z][a-z0-9_-]*", ch):
        return ""
    if ch in {"cli", "system"}:
        return ""

    # 来源决定 marker 前缀；动态 enabled 变量表示该 channel 确实支持媒体。
    # 未来 channel 仍复用同一沙箱，只需声明能力，不需在这里新增硬编码分支。
    flag = f"RCLAW_{ch.upper().replace('-', '_')}_ENABLED"
    return ch if os.environ.get(flag) == "1" else ""


def _file_sha256(path: str) -> str:
    digest = hashlib.sha256()
    with open(path, "rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _publish_media_file(
    out_path: str,
    source_channel: str,
    *,
    caption: str,
    recv_name: str,
    expect_ext: str,
) -> Optional[str]:
    """把任意产物文件复制到媒体沙箱并返回通道媒体标记。

    caption：媒体标记里展示给用户的说明；recv_name：收件人看到的文件名；
    expect_ext：强制发布文件名后缀（.html/.md）。投递 channel 优先使用 RClaw 注入的
    真实逐轮来源，并要求对应 `RCLAW_<CHANNEL>_ENABLED=1` 表示媒体能力可用。
    cli/system/未使能 channel 不发布、不打印标记（前台由技能打开
    浏览器，daemon 只在回复里说明产物已生成）。发布路径
    media_dir/out/<内容SHA-256>/<收件文件名> 落在 RClaw channel 共用的 media_dir
    沙箱内；内容寻址目录避免并发报告覆盖尚未被 channel 消费的旧 marker。
    """
    ch = _channel_enabled(source_channel)
    if not ch:
        return None

    media_dir = (os.environ.get("RCLAW_MEDIA_DIR") or "").strip()
    if not media_dir:
        print(f"⚠ RCLAW_MEDIA_DIR 为空，跳过 {expect_ext} 附件发布", file=sys.stderr)
        return None

    abs_out = os.path.abspath(out_path)
    recipient_name = safe_filename(recv_name)
    if not recipient_name.lower().endswith(expect_ext):
        recipient_name += expect_ext
    publish_root = os.path.join(media_dir, "out")
    temporary = None

    try:
        # 先复制再计算临时副本哈希，确保目录身份对应实际待发字节；最后原子改名，
        # channel 不会观察到半写入文件。
        os.makedirs(publish_root, exist_ok=True)
        fd, temporary = tempfile.mkstemp(prefix=".publish-", dir=publish_root)
        os.close(fd)
        shutil.copy2(abs_out, temporary)
        expected_digest = _file_sha256(temporary)
        publish_dir = os.path.join(publish_root, expected_digest)
        os.makedirs(publish_dir, exist_ok=True)
        publish_path = os.path.join(publish_dir, recipient_name)
        try:
            os.replace(temporary, publish_path)
        except OSError:
            if (
                not os.path.isfile(publish_path)
                or _file_sha256(publish_path) != expected_digest
            ):
                raise
            os.remove(temporary)
        temporary = None
    except OSError as exc:
        print(f"⚠ {expect_ext} 附件发布失败: {exc}", file=sys.stderr)
        return None
    finally:
        if temporary is not None:
            try:
                os.remove(temporary)
            except FileNotFoundError:
                pass

    marker_path = publish_path.replace("\\", "/")
    # 每次真实发布使用新 delivery，避免完成状态查询把用户已撤销的旧 marker 复活。
    delivery = secrets.token_hex(16)
    if ch == "weixin":
        marker = (
            f'[:weixin-media type=file path="{marker_path}" caption="{caption}" '
            f'delivery="{delivery}"]'
        )
    elif ch == "napcat":
        marker = (
            f'[:napcat-media type=file path="{marker_path}" '
            f'name="{recipient_name}" caption="{caption}" delivery="{delivery}"]'
        )
    else:
        marker = (
            f'[:{ch}-media type=file path="{marker_path}" '
            f'name="{recipient_name}" caption="{caption}" delivery="{delivery}"]'
        )
    return marker


def main():
    parser = argparse.ArgumentParser(description="Markdown → HTML (专业报告样式)")
    parser.add_argument("output", nargs="?", help="输出HTML路径")
    parser.add_argument("-o", "--out", help="输出HTML路径（等价于位置参数）")
    parser.add_argument("-i", "--input", help="输入MD文件（默认stdin）")
    parser.add_argument("--title", default="分析报告", help="页面标题")
    parser.add_argument(
        "--embed-images",
        action="store_true",
        help="把本地图片内嵌为 base64 data URI，生成自包含 HTML（外部通道自动启用）",
    )
    parser.add_argument(
        "--source-channel",
        default="",
        help="直接运行时的 source_channel；RClaw 注入的 RCLAW_SOURCE_CHANNEL 优先",
    )
    parser.add_argument(
        "--no-md",
        action="store_true",
        help="外部通道下只发布 HTML，不再额外发布源 Markdown 文件",
    )
    parser.add_argument(
        "--meta",
        default="",
        help="可选 meta.json 路径，用于在 HTML 顶部渲染 KPI 条",
    )
    args = parser.parse_args()
    # 外部 channel 共用媒体沙箱，不能从输出目录推断 marker 前缀；RClaw 注入的
    # 本轮来源必须覆盖模型可能手填的参数，参数仅保留给直接运行脚本的场景。
    runtime_channel = os.environ.get("RCLAW_SOURCE_CHANNEL", "").strip()
    if runtime_channel:
        args.source_channel = runtime_channel

    out_path = args.out or args.output
    if not out_path:
        print("用法: python3 md2html.py output.html < input.md", file=sys.stderr)
        print("      python3 md2html.py -o output.html input.md", file=sys.stderr)
        sys.exit(1)

    # 读取输入
    if args.input and os.path.exists(args.input):
        with open(args.input, "r", encoding="utf-8") as f:
            md_text = f.read()
    else:
        md_text = sys.stdin.read()

    if not md_text.strip():
        print("❌ 输入为空", file=sys.stderr)
        sys.exit(1)

    # 外部 channel 下 HTML 会随消息发到其它设备，必须内嵌图片为 base64，
    # 否则 file:// 路径在收件端无法显示；CLI/本地保持 file://，文件更小。
    external = bool(_channel_enabled(args.source_channel))
    embed_images = bool(args.embed_images) or external

    # 转换
    base_dir = (
        os.path.dirname(os.path.abspath(args.input)) if args.input else os.getcwd()
    )

    meta = None
    if args.meta:
        if not os.path.exists(args.meta):
            print(f"⚠ meta.json 不存在，跳过 KPI 条: {args.meta}", file=sys.stderr)
        else:
            try:
                with open(args.meta, "r", encoding="utf-8") as f:
                    loaded_meta = json.load(f)
                if isinstance(loaded_meta, dict):
                    meta = loaded_meta
                else:
                    print("⚠ meta.json 顶层不是对象，跳过 KPI 条", file=sys.stderr)
            except (OSError, UnicodeError, json.JSONDecodeError) as exc:
                print(f"⚠ meta.json 读取失败，跳过 KPI 条: {exc}", file=sys.stderr)

    result = convert(
        md_text, title=args.title, base_dir=base_dir, embed_images=embed_images, meta=meta
    )

    # 写入
    os.makedirs(os.path.dirname(os.path.abspath(out_path)), exist_ok=True)
    with open(out_path, "w", encoding="utf-8") as f:
        f.write(result)

    size_kb = len(result.encode("utf-8")) / 1024
    embed_note = "（图片已内嵌）" if embed_images else ""
    print(f"✅ {out_path} ({size_kb:.0f}KB){embed_note}")

    if not external:
        # cli/system：本地浏览器打开即可，不发媒体标记。
        return

    # 外部通道：先暂存 HTML（自包含、含图），再暂存源 Markdown 文件。只有整组
    # 产物全部成功才统一打印回执，避免第二份失败时第一条 marker 已经被运行时接纳，
    # 最终出现“声称 HTML + MD 两份均已发送，实际只发出一份”。
    html_name, md_name = report_attachment_names(args.input or out_path)
    markers = []
    html_marker = _publish_media_file(
        out_path,
        args.source_channel,
        caption="HTML详细报告",
        recv_name=html_name,
        expect_ext=".html",
    )
    if html_marker is None:
        return
    markers.append(html_marker)
    if args.input and not args.no_md:
        md_marker = _publish_media_file(
            args.input,
            args.source_channel,
            caption="Markdown源文件",
            recv_name=md_name,
            expect_ext=".md",
        )
        if md_marker is None:
            return
        markers.append(md_marker)
    for marker in markers:
        print(f"MEDIA_MARKER={marker}")


if __name__ == "__main__":
    main()
