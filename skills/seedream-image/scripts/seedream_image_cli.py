#!/usr/bin/env python3
"""
Seedream 5.0 lite 图片生成 CLI - 火山方舟同步图片生成客户端

覆盖 Seedream 5.0 lite 的全部图片能力：文生图、图生图（增/删/替换/修改等
编辑）、多图融合、参考图生图、组图（文/单图/多图生组图）与联网搜索。
不覆盖 Seedream 5.0 pro / flash 专属的交互编辑、图层拆分与透明背景。

流程：构建请求 -> 同步调用 images/generations -> 下载结果图片到本地。
请求配置从本 skill 目录的 config.toml 读取；API Key 从系列共享的
skills/ark_key.toml 读取。均不使用环境变量。

API 文档: https://ark.volcengine.com/docs/ark/image-generation-api
"""

from __future__ import annotations

import argparse
import base64
import json
import re
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import tomllib

DEFAULT_CONFIG_PATH = Path(__file__).resolve().parents[1] / "config.toml"
DEFAULT_KEY_CONFIG_PATH = Path(__file__).resolve().parents[2] / "ark_key.toml"

VALID_SIZE_TIERS = ("2K", "3K", "4K")
MIN_TOTAL_PIXELS = 2560 * 1440
MAX_TOTAL_PIXELS = 4096 * 4096
MIN_ASPECT_RATIO = 1 / 16
MAX_ASPECT_RATIO = 16
MAX_GENERATED_IMAGES = 15
MAX_PROMPT_CHARS = 4000
# API 允许的输入图片格式；本地文件按扩展名映射 MIME。
ALLOWED_IMAGE_EXTS = {
    "jpeg": "jpeg",
    "jpg": "jpeg",
    "png": "png",
    "webp": "webp",
    "bmp": "bmp",
    "tiff": "tiff",
    "tif": "tiff",
    "gif": "gif",
    "heic": "heic",
    "heif": "heif",
}
DOWNLOAD_CHUNK_BYTES = 1024 * 1024
PIXEL_SIZE_RE = re.compile(r"^(\d{3,5})x(\d{3,5})$")


def fail(code: str, message: str, **extra):
    """输出统一的失败 JSON 并以非零退出码结束。"""
    print(
        json.dumps(
            {"ok": False, "error": {"code": code, "message": message}, **extra},
            ensure_ascii=False,
        )
    )
    sys.exit(1)


def load_config(path: Path) -> dict:
    """读取并校验 [seedream] 配置表，任何缺失或非法字段都在付费前拒绝。"""
    if not path.is_file():
        fail("CONFIG_ERROR", f"配置文件不存在: {path}")
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        fail("CONFIG_ERROR", f"无法读取配置文件 {path}: {exc}")
    config = data.get("seedream")
    if not isinstance(config, dict):
        fail("CONFIG_ERROR", f"配置文件缺少 [seedream] 表: {path}")

    def text(key: str) -> str:
        value = str(config.get(key) or "").strip()
        if not value:
            fail("CONFIG_ERROR", f"[seedream].{key} 不能为空（配置文件: {path}）")
        return value

    if config.get("api_key"):
        # API Key 已集中管理：火山方舟系列 skill 共用 skills/ark_key.toml。
        fail(
            "CONFIG_ERROR",
            f"API Key 已集中到 {DEFAULT_KEY_CONFIG_PATH.name}，"
            f"请从本文件删除 [seedream].api_key（配置文件: {path}）",
        )
    text("model")
    api_url = text("api_url")
    if not api_url.startswith(("https://", "http://")) or any(char.isspace() for char in api_url):
        fail("CONFIG_ERROR", f"[seedream].api_url 不是有效 HTTP(S) URL（配置文件: {path}）")
    validate_size_value(text("size"))
    if str(config.get("output_format")) not in ("png", "jpeg"):
        fail("CONFIG_ERROR", f"[seedream].output_format 必须是 png/jpeg（配置文件: {path}）")
    if not isinstance(config.get("watermark"), bool):
        fail("CONFIG_ERROR", f"[seedream].watermark 必须为 true 或 false（配置文件: {path}）")
    for key in (
        "request_timeout_sec",
        "download_timeout_sec",
        "max_download_bytes",
        "max_input_image_bytes",
        "max_input_images",
    ):
        value = config.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            fail("CONFIG_ERROR", f"[seedream].{key} 必须是正整数（配置文件: {path}）")
    retries = config.get("max_retries")
    if isinstance(retries, bool) or not isinstance(retries, int) or retries < 0:
        fail("CONFIG_ERROR", f"[seedream].max_retries 必须是非负整数（配置文件: {path}）")
    return config


def load_api_key(path: Path) -> str:
    """从系列共享的 ark_key.toml 读取 api_key。"""
    if not path.is_file():
        fail("CONFIG_ERROR", f"Key 配置文件不存在: {path}")
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        fail("CONFIG_ERROR", f"无法读取 Key 配置文件 {path}: {exc}")
    table = data.get("ark")
    if not isinstance(table, dict):
        fail("CONFIG_ERROR", f"Key 配置文件缺少 [ark] 表: {path}")
    api_key = str(table.get("api_key") or "").strip()
    if not api_key:
        fail("CONFIG_ERROR", f"未配置 API Key，请填写 {path} 的 [ark].api_key")
    return api_key


def validate_size_value(size: str) -> str:
    """校验尺寸取值：档位 2K/3K/4K，或满足总像素与宽高比约束的 宽x高。"""
    if size in VALID_SIZE_TIERS:
        return size
    match = PIXEL_SIZE_RE.match(size)
    if match:
        width, height = int(match.group(1)), int(match.group(2))
        pixels = width * height
        ratio = width / height
        if not MIN_TOTAL_PIXELS <= pixels <= MAX_TOTAL_PIXELS:
            fail(
                "INVALID_SIZE",
                f"总像素 {pixels} 超出 [{MIN_TOTAL_PIXELS}, {MAX_TOTAL_PIXELS}] "
                f"（Seedream 5.0 lite 像素制约束）: {size}",
            )
        if not MIN_ASPECT_RATIO <= ratio <= MAX_ASPECT_RATIO:
            fail(
                "INVALID_SIZE",
                f"宽高比 {ratio:.3f} 超出 [1/16, 16]: {size}",
            )
        return size
    fail(
        "INVALID_SIZE",
        f"无效 size: {size}。须为档位 {'/'.join(VALID_SIZE_TIERS)} 或 宽x高 像素值"
        f"（总像素 [{MIN_TOTAL_PIXELS}, {MAX_TOTAL_PIXELS}]、宽高比 [1/16, 16]）",
    )


def parse_args():
    parser = argparse.ArgumentParser(
        description="Generate images using Doubao Seedream 5.0 lite (Volcano Ark)",
    )
    parser.add_argument(
        "-p", "--prompt", type=str, required=True,
        help="Text prompt for image generation or editing",
    )
    parser.add_argument(
        "--save", type=str, required=True,
        help="Local output path (extension normalized to .png/.jpg; "
             "multi-image results get _001.._NNN suffixes)",
    )
    parser.add_argument(
        "--image", action="append", default=None, metavar="URL_OR_PATH",
        help="Reference image (repeatable, up to 14): http(s) URL or local file path",
    )
    parser.add_argument(
        "--size", type=str, default=None,
        help="Image size tier (2K/3K/4K) or explicit WxH pixels; omit for config default",
    )
    parser.add_argument(
        "--max-images", type=int, default=None, metavar="N",
        help=f"Enable sequential (group) mode and cap generated images at N (1-{MAX_GENERATED_IMAGES})",
    )
    parser.add_argument(
        "--web-search", action="store_true",
        help="Enable web_search tool so the model can fetch fresh internet info",
    )
    parser.add_argument(
        "--output-format", type=str, default=None, choices=("png", "jpeg"),
        help="Output image format; omit for config default",
    )
    parser.add_argument(
        "--watermark", action="store_true",
        help='Add the "AI 生成" watermark at bottom-right; omit for config default',
    )
    parser.add_argument(
        "--config", type=str, default=str(DEFAULT_CONFIG_PATH),
        help="Path to this skill's config.toml",
    )
    parser.add_argument(
        "--key-config", type=str, default=str(DEFAULT_KEY_CONFIG_PATH),
        help="Shared ark_key.toml holding the API key",
    )
    return parser.parse_args()


def normalize_image_input(raw: str, max_bytes: int) -> str:
    """把单个参考图归一化为 API 取值：URL 原样，本地路径转 Base64 data URI。"""
    if raw.startswith(("http://", "https://")):
        return raw
    path = Path(raw).expanduser()
    if not path.is_file():
        fail("INVALID_IMAGE", f"参考图不是 URL 也不是存在的本地文件: {raw}")
    ext = path.suffix.lower().lstrip(".")
    if ext not in ALLOWED_IMAGE_EXTS:
        fail(
            "INVALID_IMAGE",
            f"不支持的图片格式 .{ext}（允许: {', '.join(sorted(set(ALLOWED_IMAGE_EXTS)))}）: {raw}",
        )
    size = path.stat().st_size
    if size > max_bytes:
        fail(
            "INVALID_IMAGE",
            f"参考图超过 {max_bytes} 字节上限（实际 {size} 字节）: {raw}",
        )
    encoded = base64.b64encode(path.read_bytes()).decode("ascii")
    return f"data:image/{ALLOWED_IMAGE_EXTS[ext]};base64,{encoded}"


def generate(config: dict, payload: dict) -> dict:
    """调用同步生成接口并返回解析后的 JSON。

    仅对 HTTP 429 与连接级网络错误重试；5xx 一律立即报错——同步接口
    下 5xx 通常意味着生成已失败，但响应链路中断时任务可能已计费，
    重试会重复计费，宁可直接把错误交给上层。
    """
    headers = {
        "Authorization": f"Bearer {config['api_key']}",
        "Content-Type": "application/json",
    }
    data = json.dumps(payload).encode("utf-8")
    max_retries = config["max_retries"]

    for attempt in range(max_retries + 1):
        req = urllib.request.Request(config["api_url"], data=data, headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=config["request_timeout_sec"]) as response:
                result = json.loads(response.read().decode("utf-8"))
            if not isinstance(result, dict):
                fail("PROTOCOL_ERROR", f"响应不是 JSON 对象: {str(result)[:200]}")
            return result
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", errors="replace") if exc.fp else ""
            if exc.code == 429 and attempt < max_retries:
                retry_after = exc.headers.get("Retry-After", "") if exc.headers else ""
                try:
                    time.sleep(max(0.0, min(float(retry_after), 30.0)))
                except ValueError:
                    time.sleep(min(2**attempt, 8))
                continue
            detail = ""
            code = str(exc.code)
            try:
                error_json = json.loads(body)
                if isinstance(error_json, dict):
                    inner = error_json.get("error") if "error" in error_json else error_json
                    if isinstance(inner, dict):
                        detail = str(inner.get("message", "")).strip()
                        code = str(inner.get("code", exc.code))
            except (TypeError, ValueError):
                pass
            fail(
                code if code else str(exc.code),
                detail or (body[:300] or f"HTTP {exc.code}"),
                status=exc.code,
            )
        except urllib.error.URLError as exc:
            if attempt < max_retries:
                time.sleep(min(2**attempt, 8))
                continue
            fail("NETWORK_ERROR", f"Network error: {exc.reason}")
        except Exception as exc:  # noqa: BLE001 - 统一转为结构化错误输出
            fail("UNKNOWN_ERROR", str(exc))
    fail("UNKNOWN_ERROR", "No response returned")


class DownloadError(Exception):
    """单张图片下载失败；组图场景下不阻断其余图片。"""


def download_image(url: str, target: Path, timeout_sec: int, max_bytes: int) -> Path:
    """流式下载图片并按文件魔数确认内容，扩展名按请求格式规范化。"""
    try:
        with urllib.request.urlopen(url, timeout=timeout_sec) as response:
            target.parent.mkdir(parents=True, exist_ok=True)
            received = 0
            first_chunk = True
            with target.open("wb") as output:
                while True:
                    chunk = response.read(DOWNLOAD_CHUNK_BYTES)
                    if not chunk:
                        break
                    if first_chunk:
                        expected = target.suffix.lower()
                        is_png = chunk.startswith(b"\x89PNG\r\n\x1a\n")
                        is_jpeg = chunk.startswith(b"\xff\xd8\xff")
                        if (expected == ".png" and not is_png) or (expected in (".jpg", ".jpeg") and not is_jpeg):
                            output.close()
                            target.unlink(missing_ok=True)
                            raise DownloadError("响应内容与请求的图片格式不符")
                        if not is_png and not is_jpeg:
                            output.close()
                            target.unlink(missing_ok=True)
                            raise DownloadError("响应不是有效的 PNG/JPEG 图片")
                        first_chunk = False
                    received += len(chunk)
                    if received > max_bytes:
                        output.close()
                        target.unlink(missing_ok=True)
                        raise DownloadError(f"图片超过下载上限（>{max_bytes} 字节），已中止")
                    output.write(chunk)
            if received == 0:
                target.unlink(missing_ok=True)
                raise DownloadError("下载内容为空")
            return target
    except urllib.error.URLError as exc:
        raise DownloadError(f"下载图片失败: {exc}") from exc
    except OSError as exc:
        # URLError 是 OSError 子类，须在其后捕获；覆盖磁盘满、权限不足等写盘失败。
        raise DownloadError(f"写入本地文件失败: {exc}") from exc


def main():
    args = parse_args()

    prompt = (args.prompt or "").strip()
    if not prompt:
        fail("INVALID_PROMPT", "Prompt must be non-blank.")
    if len(prompt) > MAX_PROMPT_CHARS:
        fail("INVALID_PROMPT", f"Prompt must be no longer than {MAX_PROMPT_CHARS} characters.")

    config = load_config(Path(args.config).expanduser())
    config["api_key"] = load_api_key(Path(args.key_config).expanduser())

    size = validate_size_value(args.size or str(config["size"]))
    output_format = args.output_format or str(config["output_format"])
    watermark = args.watermark or bool(config["watermark"])

    # 参考图：1-14 张，URL 或本地文件（本地转 Base64）。
    images = [
        normalize_image_input(raw, config["max_input_image_bytes"])
        for raw in (args.image or [])
    ]
    if len(images) > config["max_input_images"]:
        fail(
            "INVALID_IMAGE",
            f"参考图最多 {config['max_input_images']} 张（Seedream 5.0 lite 上限），实际 {len(images)} 张",
        )

    # 组图：--max-images 开启 sequential auto 模式；参考图数 + 生成数 ≤ 15。
    sequential = "disabled"
    if args.max_images is not None:
        if not 1 <= args.max_images <= MAX_GENERATED_IMAGES:
            fail(
                "INVALID_PARAMS",
                f"--max-images 必须在 1-{MAX_GENERATED_IMAGES} 之间，实际 {args.max_images}",
            )
        if len(images) + args.max_images > MAX_GENERATED_IMAGES:
            fail(
                "INVALID_PARAMS",
                f"参考图数量 + 生成数量 ≤ {MAX_GENERATED_IMAGES}：当前参考图 {len(images)} 张 + "
                f"最多生成 {args.max_images} 张",
            )
        sequential = "auto"

    payload = {
        "model": config["model"],
        "prompt": prompt,
        "size": size,
        "output_format": output_format,
        "watermark": watermark,
        "response_format": "url",
        "stream": False,
    }
    if images:
        payload["image"] = images[0] if len(images) == 1 else images
    if sequential == "auto":
        payload["sequential_image_generation"] = "auto"
        payload["sequential_image_generation_options"] = {"max_images": args.max_images}
    if args.web_search:
        payload["tools"] = [{"type": "web_search"}]

    usage_hint = f"size={size} format={output_format} sequential={sequential}"
    if images:
        usage_hint += f" refs={len(images)}"
    print(f"generating image ({usage_hint})...", file=sys.stderr)
    result = generate(config, payload)

    data = result.get("data")
    if not isinstance(data, list) or not data:
        fail(
            "MISSING_IMAGES",
            f"响应中没有图像数据: {json.dumps(result, ensure_ascii=False)[:300]}",
        )

    # 保存路径：单图用原始路径（扩展名规范化）；多图在文件名后加 _001.._NNN。
    save_path = Path(args.save)
    ext = ".png" if output_format == "png" else ".jpg"
    single = len(data) == 1 and isinstance(data[0], dict) and data[0].get("url")

    images_out = []
    failed_images = []
    saved_count = 0
    for position, entry in enumerate(data, start=1):
        if not isinstance(entry, dict):
            continue
        url = entry.get("url")
        if not url:
            # 组图场景下单张生成失败不影响其他图片，记录后继续。
            failed_images.append({"index": position, "error": entry.get("error") or {}})
            continue
        if single:
            target = save_path if save_path.suffix.lower() == ext else save_path.with_suffix(ext)
        else:
            saved_count += 1
            target = save_path.parent / f"{save_path.stem}_{saved_count:03d}{ext}"
        try:
            saved = download_image(
                str(url), target, config["download_timeout_sec"], config["max_download_bytes"]
            )
        except DownloadError as exc:
            failed_images.append(
                {"index": position, "error": {"code": "DOWNLOAD_FAILED", "message": f"第 {position} 张图片下载失败: {exc}"}}
            )
            continue
        images_out.append(
            {
                "index": position,
                "saved_file": str(saved),
                "url": str(url),
                "size": entry.get("size"),
                "output_format": entry.get("output_format", output_format),
            }
        )

    if not images_out:
        detail = ""
        if len(failed_images) == 1:
            detail = f": {json.dumps(failed_images[0]['error'], ensure_ascii=False)[:200]}"
        fail("TASK_FAILED", f"请求完成但没有成功保存任何图片{detail}")

    print(
        json.dumps(
            {
                "ok": True,
                "model": result.get("model", config["model"]),
                "created": result.get("created"),
                "prompt": prompt,
                "parameters": {
                    "size": size,
                    "output_format": output_format,
                    "watermark": watermark,
                    "sequential_image_generation": sequential,
                    "max_images": args.max_images if sequential == "auto" else None,
                    "web_search": bool(args.web_search),
                    "reference_images": len(images),
                },
                "usage": result.get("usage"),
                "images": images_out,
                "failed_images": failed_images,
                "note": "url 24 小时内有效，请以 saved_file 为交付物",
                "error": None,
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
