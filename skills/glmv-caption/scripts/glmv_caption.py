"""
GLM-5V-Turbo Caption Tool
Generate captions for images, videos, and files with Zhipu GLM-5V-Turbo.

Official API docs (https://docs.bigmodel.cn/api-reference/模型-api/对话补全):
- file_url cannot mix with image_url or video_url in the same request
- Images: URL or Base64, ≤5MB each, ≤6000×6000px, only jpg/png/jpeg
- GLM-4V-Flash: ≤1 image, NO Base64 (URL only)
- Videos: URL only (no local path, no base64), mp4/mkv/mov
- Files: URL only (no local path, no base64), pdf/txt/docx/jsonl/xlsx/pptx, up to 50 files

RClaw adaptation: model metadata and help text updated for GLM-5V-Turbo.
"""

import json
import base64
import binascii
import argparse
import math
import sys
from pathlib import Path
from typing import List, Optional, Dict, Any
from urllib.parse import urlparse

COMMON_SCRIPTS = Path(__file__).resolve().parents[2] / "glm5v-multimodal" / "scripts"
if str(COMMON_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(COMMON_SCRIPTS))
MASTER_SCRIPTS = Path(__file__).resolve().parents[2] / "glm-master-skill" / "scripts"
if str(MASTER_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(MASTER_SCRIPTS))

from glm5v_stream import GLM5VStreamError, post_stream_json
from glm5v import image_dimensions
from glm_config import (
    ConfigError,
    DEFAULT_CONFIG_PATH,
    api_url,
    load_config,
    non_negative_int,
    positive_int,
)

DEFAULT_PROMPT = "请详细描述这张图片的内容"

# Model max_tokens limits (per official docs)
MODEL_MAX_TOKENS = {
    "glm-5v-turbo": {"default": 16384, "max": 128000},
    "glm-4.6v": {"default": 16384, "max": 32768},
    "glm-4.6v-flash": {"default": 16384, "max": 32768},
    "glm-4.6v-flashx": {"default": 16384, "max": 32768},
}

# Officially supported image formats only (per Zhipu API docs)
SUPPORTED_IMAGE_EXTS = {".jpg", ".jpeg", ".png"}
IMAGE_MIME_TYPES = {
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
    ".png": "image/png",
}

# Image constraints per official docs
MAX_IMAGE_SIZE_MB = 5
MAX_IMAGE_PIXELS = 6000 * 6000  # 36 megapixels

# Max images per model (per official docs)
# Official vision model enum: glm-4.6v, autoglm-phone, glm-4.6v-flash, glm-4.6v-flashx, glm-4v-flash, glm-4.1v-thinking-flashx, glm-4.1v-thinking-flash
MODEL_IMAGE_LIMITS = {
    "glm-5v-turbo": 50,
    "glm-4.6v": 50,
    "glm-4.6v-flash": 50,
    "glm-4.6v-flashx": 50,
}

# Models that do NOT support Base64 (URL only)
NO_BASE64_MODELS = {"glm-4v-flash"}


def image_signature_matches(data: bytes, image_type: str) -> bool:
    if image_type in {".jpg", ".jpeg", "jpeg"}:
        return data.startswith(b"\xff\xd8\xff")
    if image_type in {".png", "png"}:
        return data.startswith(b"\x89PNG\r\n\x1a\n")
    return False


def validate_local_image(image_path: str) -> str:
    """Validate local image against official API constraints.

    Returns error message if validation fails, empty string if OK.
    """
    path = Path(image_path)
    if not path.is_file():
        return f"Image not found: {image_path}"

    # Check format
    ext = path.suffix.lower()
    if ext not in SUPPORTED_IMAGE_EXTS:
        return (
            f"Unsupported image format '{ext}'. "
            f"Only {', '.join(sorted(SUPPORTED_IMAGE_EXTS))} are supported by the Zhipu API."
        )

    # Check file size (≤5MB)
    size_bytes = path.stat().st_size
    max_bytes = MAX_IMAGE_SIZE_MB * 1024 * 1024
    if size_bytes > max_bytes:
        return (
            f"Image too large: {size_bytes / 1024 / 1024:.1f}MB "
            f"(max {MAX_IMAGE_SIZE_MB}MB). "
            f"Consider resizing or compressing the image."
        )
    try:
        with path.open("rb") as image_file:
            signature = image_file.read(8)
    except OSError as exc:
        return f"Image cannot be read: {image_path}: {exc}"
    if not image_signature_matches(signature, ext):
        return f"Image content does not match its '{ext}' extension: {image_path}"
    # 中文注释：文件头正确后继续校验官方 6000 像素边长上限。
    try:
        dimensions = image_dimensions(path.read_bytes())
    except OSError as exc:
        return f"Image cannot be read: {image_path}: {exc}"
    if dimensions is None or any(side <= 0 or side > 6000 for side in dimensions):
        return f"Image dimensions must be readable and no larger than 6000x6000: {image_path}"

    return ""


def load_image_as_data_url(image_path: str) -> str:
    """Convert a local image file to data URL format.

    Validates format and size before encoding.
    """
    path = Path(image_path)
    if not path.is_file():
        raise FileNotFoundError(f"Image not found: {image_path}")

    ext = path.suffix.lower()
    if ext not in SUPPORTED_IMAGE_EXTS:
        raise ValueError(
            f"Unsupported image format '{ext}'. "
            f"Only {', '.join(sorted(SUPPORTED_IMAGE_EXTS))} are supported by the Zhipu API."
        )

    # Check file size
    size_bytes = path.stat().st_size
    max_bytes = MAX_IMAGE_SIZE_MB * 1024 * 1024
    if size_bytes > max_bytes:
        raise ValueError(
            f"Image too large: {size_bytes / 1024 / 1024:.1f}MB "
            f"(max {MAX_IMAGE_SIZE_MB}MB). "
            f"Consider resizing or compressing the image."
        )
    with path.open("rb") as image_file:
        signature = image_file.read(8)
    if not image_signature_matches(signature, ext):
        raise ValueError(
            f"Image content does not match its '{ext}' extension: {image_path}"
        )
    # 中文注释：直接调用编码函数时也不能绕过图片尺寸约束。
    dimensions = image_dimensions(path.read_bytes())
    if dimensions is None or any(side <= 0 or side > 6000 for side in dimensions):
        raise ValueError(
            f"Image dimensions must be readable and no larger than 6000x6000: {image_path}"
        )

    mime_type = IMAGE_MIME_TYPES[ext]
    with open(path, "rb") as f:
        img_data = base64.b64encode(f.read()).decode()

    return f"data:{mime_type};base64,{img_data}"


def process_image(image_input: str) -> str:
    """
    Process an image input into a usable image_url.

    Supports: URL, local path (→ data URL), data URL (pass-through), base64:xxx
    """
    image_input = image_input.strip()

    if image_input.startswith("data:"):
        return image_input

    if image_input.startswith("base64:"):
        return f"data:image/jpeg;base64,{image_input[7:]}"

    if _is_url(image_input):
        return image_input

    return load_image_as_data_url(image_input)


def build_content(
    images: Optional[List[str]] = None,
    videos: Optional[List[str]] = None,
    files: Optional[List[str]] = None,
    prompt: str = DEFAULT_PROMPT,
) -> List[Dict[str, Any]]:
    """Build the multimodal content array for the API request."""
    content = []

    if images:
        for img in images:
            content.append(
                {
                    "type": "image_url",
                    "image_url": {"url": process_image(img)},
                }
            )
    elif videos:
        for vid in videos:
            content.append(
                {
                    "type": "video_url",
                    "video_url": {"url": vid.strip()},
                }
            )
    elif files:
        for f in files:
            content.append(
                {
                    "type": "file_url",
                    "file_url": {"url": f.strip()},
                }
            )

    content.append({"type": "text", "text": prompt})
    return content


def _is_url(s: str) -> bool:
    value = s.strip()
    try:
        parsed = urlparse(value)
        host = parsed.hostname
        parsed.port
    except (AttributeError, ValueError):
        return False
    return (
        bool(value)
        and not any(char.isspace() or ord(char) < 0x20 for char in value)
        and "\\" not in value
        and parsed.scheme in {"http", "https"}
        and bool(host)
        and parsed.username is None
        and parsed.password is None
    )


def validate_inline_image(value: str, index: int) -> List[str]:
    """Validate an inline image before decoding it into the request payload."""
    errors: List[str] = []
    expected_type = "jpeg"
    if value.startswith("data:"):
        if "," not in value:
            return [f"images[{index}]: data URL is missing its payload separator."]
        header, payload = value.split(",", 1)
        normalized_header = header.lower()
        if normalized_header not in {
            "data:image/jpeg;base64",
            "data:image/jpg;base64",
            "data:image/png;base64",
        }:
            errors.append(
                f"images[{index}]: data URL must contain a base64 JPEG or PNG image."
            )
            expected_type = ""
        elif normalized_header == "data:image/png;base64":
            expected_type = "png"
    else:
        payload = value[7:]
    if not payload.strip():
        errors.append(f"images[{index}]: base64 payload is empty.")
        return errors
    try:
        decoded = base64.b64decode(payload, validate=True)
    except (binascii.Error, ValueError):
        errors.append(f"images[{index}]: base64 payload is invalid.")
        return errors
    if len(decoded) > MAX_IMAGE_SIZE_MB * 1024 * 1024:
        errors.append(
            f"images[{index}]: decoded image exceeds {MAX_IMAGE_SIZE_MB}MB."
        )
    if expected_type and not image_signature_matches(decoded, expected_type):
        errors.append(
            f"images[{index}]: decoded content does not match its {expected_type.upper()} type."
        )
    # 中文注释：内联 base64 同样执行尺寸校验，避免只校验格式与字节数。
    dimensions = image_dimensions(decoded)
    if dimensions is None or any(side <= 0 or side > 6000 for side in dimensions):
        errors.append(
            f"images[{index}]: image dimensions must be readable and at most 6000x6000."
        )
    return errors


def validate_inputs(
    images: Optional[List[str]],
    videos: Optional[List[str]],
    files: Optional[List[str]],
    model: str,
) -> List[str]:
    """Return every deterministic input problem before building an API request."""
    errors: List[str] = []
    groups = [bool(images), bool(videos), bool(files)]
    if sum(groups) != 1:
        errors.append("Provide exactly one of images, videos, or files.")

    model_key = model.lower()
    if images:
        max_images = MODEL_IMAGE_LIMITS.get(model_key, 50)
        if len(images) > max_images:
            errors.append(
                f"Too many images: {len(images)} (max {max_images} for model '{model}')."
            )
        for index, image in enumerate(images):
            value = image.strip()
            lowered = value.lower()
            if model_key in NO_BASE64_MODELS and not _is_url(value):
                errors.append(
                    f"images[{index}]: model '{model}' requires an HTTP(S) image URL."
                )
                continue
            if value.startswith(("data:", "base64:")):
                errors.extend(validate_inline_image(value, index))
            elif _is_url(value):
                pass
            elif lowered.startswith(("http://", "https://")):
                errors.append(f"images[{index}]: invalid HTTP(S) URL: {image}")
            else:
                error = validate_local_image(value)
                if error:
                    errors.append(f"images[{index}]: {error}")

    if files and len(files) > 50:
        errors.append(f"Too many files: {len(files)} (max 50).")
    for label, values in (("videos", videos), ("files", files)):
        for index, value in enumerate(values or []):
            if not _is_url(value):
                errors.append(f"{label}[{index}] must be a valid HTTP(S) URL: {value}")
    return errors


def format_validation_errors(errors: List[str]) -> str:
    return (
        f"Input preflight found {len(errors)} problem(s):\n"
        + "\n".join(f"- {error}" for error in errors)
    )


def collect_config_value(
    errors: List[str], field: str, callback
) -> Any:
    try:
        return callback()
    except (ConfigError, KeyError, TypeError, ValueError) as exc:
        errors.append(f"Configuration {field}: {exc}")
        return None


def finite_config_float(config: Dict[str, Any], field: str) -> float:
    value = float(config[field])
    if not math.isfinite(value):
        raise ConfigError(f"{field} must be finite")
    # 中文注释：GLM 采样参数使用 0..1 区间，越界值应在请求前拒绝。
    if not 0 <= value <= 1:
        raise ConfigError(f"{field} must be between 0 and 1")
    return value


def caption(
    images: Optional[List[str]] = None,
    videos: Optional[List[str]] = None,
    files: Optional[List[str]] = None,
    prompt: str = DEFAULT_PROMPT,
    config_path: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Generate captions for images, videos, or documents using GLM-5V-Turbo.

    Args:
        images: Image URLs, local paths, or base64 strings
        videos: Public video URLs (mp4/mkv/mov)
        files: Public file URLs (pdf/docx/txt/xlsx/pptx/jsonl)
        prompt: Prompt for the model
        config_path: Shared TOML config path; defaults to glm-master-skill/config.toml

    Returns:
        Dict with success, caption, and usage fields
    """
    basic_input_errors = validate_inputs(images, videos, files, "")
    if not isinstance(prompt, str) or not prompt.strip():
        basic_input_errors.append("Prompt cannot be empty.")
    try:
        config = load_config("glm5v", "caption", config_path=config_path)
    except (ConfigError, OSError, ValueError) as exc:
        errors = [*basic_input_errors, f"Configuration: {exc}"]
        return {"success": False, "error": format_validation_errors(errors)}
    api_key = config["api_key"]
    model = str(config["model"])
    config_errors: List[str] = []
    temperature = collect_config_value(
        config_errors,
        "temperature",
        lambda: finite_config_float(config, "temperature"),
    )
    top_p = collect_config_value(
        config_errors, "top_p", lambda: finite_config_float(config, "top_p")
    )
    max_tokens = collect_config_value(
        config_errors, "max_tokens", lambda: positive_int(config, "max_tokens")
    )
    connect_timeout_sec = collect_config_value(
        config_errors,
        "connect_timeout_sec",
        lambda: positive_int(config, "connect_timeout_sec"),
    )
    read_timeout_sec = collect_config_value(
        config_errors,
        "read_timeout_sec",
        lambda: positive_int(config, "read_timeout_sec"),
    )
    max_retries = collect_config_value(
        config_errors,
        "max_retries",
        lambda: non_negative_int(config, "max_retries"),
    )
    endpoint = collect_config_value(
        config_errors,
        "base_url",
        lambda: api_url(config, "chat/completions"),
    )

    input_errors = list(
        dict.fromkeys(
            [
                *basic_input_errors,
                *validate_inputs(images, videos, files, model),
                *config_errors,
            ]
        )
    )

    # Validate max_tokens against model limits
    model_key = model.lower()
    model_limits = MODEL_MAX_TOKENS.get(model_key)
    if (
        model_limits
        and max_tokens is not None
        and max_tokens > model_limits["max"]
    ):
        input_errors.append(
            f"max_tokens {max_tokens} exceeds model '{model}' limit of "
            f"{model_limits['max']}."
        )
    if input_errors:
        return {"success": False, "error": format_validation_errors(input_errors)}
    assert all(
        value is not None
        for value in (
            temperature,
            top_p,
            max_tokens,
            connect_timeout_sec,
            read_timeout_sec,
            max_retries,
            endpoint,
        )
    )

    content = build_content(images=images, videos=videos, files=files, prompt=prompt)

    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }

    payload: Dict[str, Any] = {
        "model": model,
        "messages": [{"role": "user", "content": content}],
        "temperature": temperature,
        "top_p": top_p,
        str(config["max_tokens_key"]): max_tokens,
    }

    try:
        result = post_stream_json(
            endpoint,
            headers,
            payload,
            connect_timeout_sec=connect_timeout_sec,
            read_timeout_sec=read_timeout_sec,
            max_retries=max_retries,
            thinking_type=config["thinking"],
            stream=config["stream"],
        )
    except GLM5VStreamError as exc:
        return {
            "success": False,
            "error": str(exc),
            "status_code": exc.status_code,
        }

    caption = ""
    finish_reason = ""
    if result.get("choices"):
        caption = result["choices"][0].get("message", {}).get("content", "")
        finish_reason = result["choices"][0].get("finish_reason", "")
    if not caption.strip():
        return {
            "success": False,
            "error": f"GLM-5V-Turbo returned empty content (finish_reason={finish_reason or 'unknown'})",
            "usage": result.get("usage", {}),
        }

    out = {
        "success": True,
        "caption": caption,
        "usage": result.get("usage", {}),
        "finish_reason": finish_reason,
        # 只有正常停止才代表正文完整，避免把 token 截断误报为完整结果。
        "complete": finish_reason == "stop",
    }
    if finish_reason == "length":
        out["warning"] = "Output was truncated because the token limit was reached."
    elif finish_reason == "sensitive":
        out["warning"] = "Content blocked by safety review (finish_reason: sensitive)"
    return out


def main():
    parser = argparse.ArgumentParser(
        description="Generate captions for images, videos, and documents using GLM-5V-Turbo"
    )

    # Input groups (mutually exclusive — API limitation)
    input_group = parser.add_argument_group("Input (pick one)")
    input_group.add_argument(
        "--images",
        "-i",
        nargs="+",
        help="Image paths or URLs (supports multiple, base64 OK)",
    )
    input_group.add_argument(
        "--videos", "-v", nargs="+", help="Public video URLs (mp4/mkv/mov)"
    )
    input_group.add_argument(
        "--files",
        "-f",
        nargs="+",
        help="Public document URLs (pdf/docx/txt/xlsx/pptx/jsonl)",
    )

    parser.add_argument("--prompt", "-p", default=DEFAULT_PROMPT, help="Custom prompt")
    parser.add_argument(
        "--config",
        default=str(DEFAULT_CONFIG_PATH),
        help="Shared GLM TOML configuration file",
    )

    # Output options
    parser.add_argument("--output", "-o", help="Save result JSON to file")
    parser.add_argument(
        "--pretty", action="store_true", help="Pretty-print JSON output"
    )

    args = parser.parse_args()

    try:
        result = caption(
            images=args.images,
            videos=args.videos,
            files=args.files,
            prompt=args.prompt,
            config_path=args.config,
        )
    except Exception as e:
        result = {"success": False, "error": str(e)}

    indent = 2 if args.pretty else None
    print(json.dumps(result, ensure_ascii=False, indent=indent))

    if not result.get("success"):
        sys.exit(1)

    if args.output:
        # 允许调用方直接指定尚不存在的嵌套输出目录。
        output_path = Path(args.output).resolve()
        output_path.parent.mkdir(parents=True, exist_ok=True)
        with output_path.open("w", encoding="utf-8") as f:
            json.dump(result, f, ensure_ascii=False, indent=2)
        print(f"\nResult saved to: {output_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
