#!/usr/bin/env python3
"""
GLM-Image Generation CLI - ZhiPu GLM-Image API client

Usage:
    python glm_image_cli.py --prompt "A cute cat" --save image.png

API Docs: https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E5%9B%BE%E5%83%8F%E7%94%9F%E6%88%90
"""

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from urllib.parse import urlparse

MASTER_SCRIPTS = Path(__file__).resolve().parents[2] / "glm-master-skill" / "scripts"
if str(MASTER_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(MASTER_SCRIPTS))

from glm_config import (
    ConfigError,
    DEFAULT_CONFIG_PATH,
    load_config,
    non_negative_int,
    positive_int,
)


def parse_args():
    parser = argparse.ArgumentParser(
        description="Generate images from text prompts using GLM-Image API",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  python glm_image_cli.py --prompt "A cute cat sitting on windowsill" --save cat.png
  python glm_image_cli.py --prompt "Cyberpunk city" --save city.png
  python glm_image_cli.py --prompt "Chinese landscape painting" --save image.png
        """,
    )

    parser.add_argument(
        "-p",
        "--prompt",
        type=str,
        required=True,
        help="Text description of the desired image (required)",
    )
    parser.add_argument(
        "--user-id",
        type=str,
        default=None,
        help="End-user ID for content moderation (6-128 chars)",
    )
    parser.add_argument(
        "--save",
        type=str,
        required=True,
        help="Required local output path (the extension is normalized from image bytes)",
    )
    parser.add_argument(
        "--config",
        default=str(DEFAULT_CONFIG_PATH),
        help="Shared GLM TOML configuration file",
    )
    return parser.parse_args()


def validate_size(size: str, model: str = "glm-image") -> bool:
    """Validate size format and dimensions.

    glm-image requirements:
    - Width and height: 512px - 2048px
    - Both dimensions must be multiples of 32
    - Maximum total pixels: 2^22 (4,194,304)

    Other models (cogview-4, cogview-3-flash, etc.):
    - Width and height: 512px - 2048px
    - Both dimensions must be multiples of 16
    - Maximum total pixels: 2^21 (2,097,152)
    """
    try:
        parts = size.split("x")
        if len(parts) != 2:
            return False
        width = int(parts[0])
        height = int(parts[1])

        if model.lower() == "glm-image":
            # 中文注释：glm-image 的宽高必须为 32 的倍数。
            if width % 32 != 0 or height % 32 != 0:
                return False
            # 中文注释：官方当前允许 512 至 2048 像素，不再沿用旧版 1024 下限。
            if not (512 <= width <= 2048 and 512 <= height <= 2048):
                return False
            # Max total pixels: 2^22 = 4194304
            if width * height > 4194304:
                return False
        else:
            # Other models: cogview-4, cogview-3-flash, etc.
            # Must be multiple of 16
            if width % 16 != 0 or height % 16 != 0:
                return False
            # Must be between 512-2048
            if not (512 <= width <= 2048 and 512 <= height <= 2048):
                return False
            # Max total pixels: 2^21 = 2097152
            if width * height > 2097152:
                return False

        return True
    except (ValueError, IndexError):
        return False


def validate_user_id(user_id: str) -> bool:
    """Validate user_id length (6-128 chars)."""
    if not user_id:
        return True
    # 中文注释：同时拒绝控制字符，避免把多行或不可见标识传入审核字段。
    return 6 <= len(user_id) <= 128 and not any(ord(char) < 0x20 for char in user_id)


def validate_prompt(prompt: str) -> bool:
    """校验提示词非空且不超过接口规定的 1000 个字符。"""
    return isinstance(prompt, str) and bool(prompt.strip()) and len(prompt) <= 1000


def validate_runtime_config(config: dict) -> tuple[str, str]:
    """在生成前校验请求与下载阶段都会使用的完整配置。"""
    model = str(config["model"])
    size = str(config["size"])
    positive_int(config, "request_timeout_sec")
    positive_int(config, "download_timeout_sec")
    positive_int(config, "max_download_bytes")
    non_negative_int(config, "max_retries")
    if not isinstance(config.get("watermark_enabled"), bool):
        raise ConfigError("watermark_enabled 必须为 true 或 false")
    return model, size


def _image_suffix(data: bytes) -> str:
    if data.startswith(b"\x89PNG\r\n\x1a\n"):
        return ".png"
    if data.startswith(b"\xff\xd8\xff"):
        return ".jpg"
    if data.startswith((b"GIF87a", b"GIF89a")):
        return ".gif"
    if len(data) >= 12 and data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        return ".webp"
    return ""


def download_image(
    url: str, save_path: str, timeout_sec: int, max_bytes: int
) -> str | None:
    """Download an image and normalize its extension from magic bytes."""
    try:
        parsed = urlparse(url)
        if parsed.scheme not in ("http", "https") or not parsed.netloc:
            return None
        with urllib.request.urlopen(url, timeout=timeout_sec) as response:
            data = response.read(max_bytes + 1)
        if len(data) > max_bytes:
            return None
        path = Path(save_path).resolve()
        actual_suffix = _image_suffix(data)
        if not actual_suffix:
            return None
        equivalent = {".jpg", ".jpeg"}
        if actual_suffix and (
            path.suffix.lower() != actual_suffix
            and not {path.suffix.lower(), actual_suffix}.issubset(equivalent)
        ):
            path = path.with_suffix(actual_suffix)
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
        return str(path)
    except Exception:
        return None


def generate_image(
    config: dict,
    prompt: str,
    user_id: str = None,
) -> dict:
    """Call GLM-Image API to generate image.

    Returns response in official format:
    {
        "created": 123,
        "data": [{"url": "..."}],
        "content_filter": [{"role": "assistant", "level": 1}]
    }
    """

    url = str(config["api_url"])
    api_key = str(config["api_key"])
    model = str(config["model"])
    size = str(config["size"])
    quality = str(config["quality"])
    watermark = config.get("watermark_enabled")
    if not isinstance(watermark, bool):
        raise ConfigError("watermark_enabled 必须为 true 或 false")
    request_timeout = positive_int(config, "request_timeout_sec")
    max_retries = non_negative_int(config, "max_retries")

    payload = {
        "model": model,
        "prompt": prompt,
        "size": size,
        "quality": quality,
        "watermark_enabled": watermark,
    }

    if user_id:
        payload["user_id"] = user_id

    data = json.dumps(payload).encode("utf-8")

    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
    )

    for attempt in range(max_retries + 1):
        try:
            with urllib.request.urlopen(req, timeout=request_timeout) as response:
                result = json.loads(response.read().decode("utf-8"))
                return {
                    "ok": True,
                    "result": result,
                    "image_url": (
                        result.get("data", [{}])[0].get("url")
                        if result.get("data")
                        else None
                    ),
                    "prompt": prompt,
                    "size": size,
                    "quality": quality,
                    "created": result.get("created"),
                    "content_filter": result.get("content_filter", []),
                }
        except urllib.error.HTTPError as e:
            error_body = e.read().decode("utf-8") if e.fp else ""
            if (e.code == 429 or e.code >= 500) and attempt < max_retries:
                retry_after = e.headers.get("Retry-After", "") if e.headers else ""
                try:
                    delay = max(0.0, min(float(retry_after), 30.0))
                except ValueError:
                    delay = min(2**attempt, 8)
                time.sleep(delay)
                continue

            error_detail = ""
            error_code = str(e.code)
            try:
                error_json = json.loads(error_body)
                if isinstance(error_json, dict):
                    error_detail = str(
                        error_json.get("error", {}).get("message", "")
                    ).strip()
                    error_code = str(
                        error_json.get("error", {}).get("code", str(e.code))
                    )
            except (TypeError, ValueError):
                pass
            if not error_detail:
                error_detail = (error_body[:200] or "No response body").strip()
            return {
                "ok": False,
                "error": {
                    "code": error_code,
                    "message": error_detail,
                    "status": e.code,
                },
            }
        except urllib.error.URLError as e:
            if attempt < max_retries:
                time.sleep(min(2**attempt, 8))
                continue
            return {
                "ok": False,
                "error": {
                    "code": "NETWORK_ERROR",
                    "message": f"Network error: {str(e.reason)}",
                },
            }
        except Exception as e:
            return {
                "ok": False,
                "error": {"code": "UNKNOWN_ERROR", "message": str(e)},
            }

    return {
        "ok": False,
        "error": {"code": "UNKNOWN_ERROR", "message": "No response returned"},
    }


def main():
    args = parse_args()

    # 在读取配置和发起付费请求前先拒绝确定无效的提示词。
    if not validate_prompt(args.prompt):
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": {
                        "code": "INVALID_PROMPT",
                        "message": "Prompt must be non-blank and no longer than 1000 characters.",
                    },
                },
                ensure_ascii=False,
            )
        )
        sys.exit(1)

    try:
        config = load_config("glm_image", config_path=args.config)
    except ConfigError as exc:
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": {
                        "code": "CONFIG_ERROR",
                        "message": str(exc),
                    },
                },
                ensure_ascii=False,
            )
        )
        sys.exit(1)

    try:
        # 中文注释：付费生成前一次性校验完整配置，避免生成成功后才在下载阶段失败。
        model, size = validate_runtime_config(config)
    except (KeyError, TypeError, ValueError) as exc:
        print(
            json.dumps(
                {"ok": False, "error": {"code": "CONFIG_ERROR", "message": str(exc)}},
                ensure_ascii=False,
            )
        )
        sys.exit(1)

    # 中文注释：按所选模型的官方尺寸约束做请求前校验。
    if not validate_size(size, model):
        if model.lower() == "glm-image":
            size_msg = "Must be multiple of 32, 512-2048px, max 2^22 pixels"
        else:
            size_msg = "Must be multiple of 16, 512-2048px, max 2^21 pixels"
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": {
                        "code": "INVALID_SIZE",
                        "message": f"Invalid size: {size} for model {model}. {size_msg}",
                    },
                },
                ensure_ascii=False,
            )
        )
        sys.exit(1)

    # Validate user_id
    if args.user_id and not validate_user_id(args.user_id):
        print(
            json.dumps(
                {
                    "ok": False,
                    "error": {
                        "code": "INVALID_USER_ID",
                        "message": f"Invalid user_id: {args.user_id}. Must be 6-128 characters.",
                    },
                },
                ensure_ascii=False,
            )
        )
        sys.exit(1)

    # Generate image
    result = generate_image(
        config=config,
        prompt=args.prompt,
        user_id=args.user_id,
    )

    # Download the generated image to the required local deliverable.
    saved_file = None
    if result["ok"] and result.get("image_url"):
        downloaded_path = download_image(
            result["image_url"],
            args.save,
            positive_int(config, "download_timeout_sec"),
            positive_int(config, "max_download_bytes"),
        )
        if downloaded_path:
            saved_file = downloaded_path
        else:
            result["ok"] = False
            result["error"] = {
                "code": "DOWNLOAD_FAILED",
                "message": f"Failed to download image to {args.save}",
            }
    elif result["ok"]:
        result["ok"] = False
        result["error"] = {
            "code": "MISSING_IMAGE_URL",
            "message": "GLM-Image returned no downloadable image URL",
        }

    # Output result (matching official API format where applicable)
    output_data = {
        "ok": result["ok"],
        "model": model,
        "image_url": result.get("image_url"),
        "prompt": result.get("prompt"),
        "size": result.get("size"),
        "quality": result.get("quality"),
        "created": result.get("created"),
        "content_filter": result.get("content_filter", []),
        "saved_file": saved_file,
        "error": result.get("error"),
    }

    # Print to stdout (compact JSON)
    print(json.dumps(output_data, ensure_ascii=False))

    # Exit with error code if failed
    if not result["ok"]:
        sys.exit(1)


if __name__ == "__main__":
    main()
