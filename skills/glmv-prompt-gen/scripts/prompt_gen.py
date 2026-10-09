"""
GLM-V Prompt Generation Tool
Use Zhipu GLM-V vision models to analyze images/videos and generate prompts for text-to-image and text-to-video workflows.

Official API docs (https://docs.bigmodel.cn/cn/guide/models/vlm/glm-5v-turbo):

- Images: URL or Base64, ≤5MB each, ≤6000×6000px, only jpg/png/jpeg
- Videos: URL only (no local path, no base64), mp4/mkv/mov, ≤200MB
- GLM-4V-Flash: ≤1 image, NO Base64
- image_url and video_url cannot mix in the same request

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


# Model max_tokens limits (per official docs)
MODEL_MAX_TOKENS = {
    "glm-5v-turbo": {"default": 16384, "max": 128000},
    "glm-4.6v": {"default": 16384, "max": 32768},
    "glm-4.6v-flash": {"default": 16384, "max": 32768},
    "glm-4.6v-flashx": {"default": 16384, "max": 32768},
}

# Max images per model
MODEL_IMAGE_LIMITS = {
    "glm-5v-turbo": 50,
    "glm-4.6v": 50,
    "glm-4.6v-flash": 50,
    "glm-4.6v-flashx": 50,
}

# Models that do NOT support Base64 (URL only)
NO_BASE64_MODELS = {"glm-4v-flash"}

SUPPORTED_IMAGE_EXTS = {".jpg", ".jpeg", ".png"}
IMAGE_MIME_TYPES = {".jpg": "image/jpeg", ".jpeg": "image/jpeg", ".png": "image/png"}
MAX_IMAGE_SIZE_MB = 5

OUTPUT_IMAGE = "image"
OUTPUT_VIDEO = "video"

OUTPUT_FORMAT_RULE = (
    "## 输出格式\n"
    "只输出两部分，不要任何多余内容：\n"
    "### 内容分析\n"
    "（用中文写一段完整的文字，详细描述素材的内容和视觉特征。\n"
    "不要分点、不要列表，写成一段连贯流畅的分析，\n"
    "涵盖主体、风格、色调、光影、构图、氛围、细节等所有你观察到的要素。）\n\n"
    "### Prompt\n"
    "（根据语言判断规则，用中文或英文生成一段可直接复制使用的 prompt。\n"
    "写成一段完整、流畅、描述性强的文字，不要用分点或列表。\n"
    "必须涵盖以下六个要素，各要素之间用自然的语言衔接，不要生硬罗列：\n"
    "1. 主体：画面核心对象及其状态、动作、表情等\n"
    "2. 风格：如写实摄影、油画、水彩、赛博朋克、扁平插画等\n"
    "3. 色调：整体色彩倾向、主色调、色彩对比关系\n"
    "4. 光影：光源方向、明暗对比、特殊光效（逆光、侧光、柔光等）\n"
    "5. 构图：视角（俯视/平视/仰视）、景别（特写/中景/远景）、画面布局\n"
    "6. 氛围：整体情绪和感受（温馨/冷峻/神秘/欢快等）\n"
    "不要遗漏任何要素，这段 prompt 就是用户最终复制粘贴到生图/生视频工具里的东西。）\n"
)

LANG_RULE = (
    "## 语言判断规则\n"
    "根据参考素材的内容自动判断 prompt 应使用中文还是英文，只输出一种语言：\n"
    "- 如果素材包含中文文字、中国元素（如中文品牌、中国场景、中国文化符号等），"
    "用**中文**生成 prompt\n"
    "- 如果素材为纯西式内容（英文文字、西方场景、国际品牌等），"
    "用**英文**生成 prompt\n"
    "- 纯图片无文字时，根据视觉风格判断：西式时尚/摄影/建筑等用英文，中式场景用中文\n"
    "- 判断模糊时，优先输出中文 prompt\n"
    "- 绝不要同时输出中英两个版本\n"
)

SYSTEM_PROMPT_IMAGE = (
    "你是一位专业的 AI 绘画 prompt 工程师。分析用户提供的参考图片，生成可直接用于文生图工具的 prompt。\n\n"
    "## 工作流程\n"
    "1. 仔细观察参考图片的：构图、风格、色调、光影、主体、氛围、细节\n"
    "2. 提炼关键视觉元素\n"
    "3. 根据素材内容判断语言，生成一段完整的 prompt\n\n"
    + LANG_RULE
    + "\n"
    + OUTPUT_FORMAT_RULE
)

SYSTEM_PROMPT_VIDEO = (
    "你是一位专业的 AI 视频生成 prompt 工程师。分析用户提供的参考视频，生成可直接用于文生视频工具的 prompt。\n\n"
    "## 工作流程\n"
    "1. 仔细观察参考视频的：场景、动作、镜头运动、节奏、风格、色调、主体、氛围\n"
    "2. 提炼关键视觉和动态元素\n"
    "3. 根据素材内容判断语言，生成一段完整的 prompt\n\n"
    + LANG_RULE
    + "\n"
    + OUTPUT_FORMAT_RULE
)

SYSTEM_PROMPT_AUTO = (
    "你是一位专业的 AI 创意 prompt 工程师。分析用户提供的参考素材，生成可直接用于文生图和文生视频工具的 prompt。\n\n"
    "## 工作流程\n"
    "1. 仔细观察参考素材的内容、风格、色调、光影、构图/镜头、主体、氛围\n"
    "2. 提炼关键视觉元素\n"
    "3. 根据素材内容判断语言，分别生成文生图和文生视频两段 prompt\n\n"
    + LANG_RULE
    + "\n"
    "## 输出格式\n"
    "只输出三部分，不要任何多余内容：\n"
    "### 内容分析\n"
    "（用中文写一段完整的文字，详细描述素材的内容和视觉特征，不要分点列表。）\n\n"
    "### 文生图 Prompt\n"
    "（根据语言判断规则，用中文或英文生成一段完整描述性段落，可直接复制使用。\n"
    "必须涵盖主体、风格、色调、光影、构图、氛围六个要素，用自然语言衔接。）\n\n"
    "### 文生视频 Prompt\n"
    "（根据语言判断规则，用中文或英文生成一段完整描述性段落，可直接复制使用。\n"
    "必须涵盖场景、主体动作、镜头运动、风格、色调光影、节奏氛围六个要素，用自然语言衔接。）\n"
)


def image_signature_matches(data: bytes, image_type: str) -> bool:
    if image_type in {".jpg", ".jpeg", "jpeg"}:
        return data.startswith(b"\xff\xd8\xff")
    if image_type in {".png", "png"}:
        return data.startswith(b"\x89PNG\r\n\x1a\n")
    return False


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
    images: Optional[List[str]], videos: Optional[List[str]], model: str
) -> List[str]:
    """Return all deterministic media input problems before encoding any file."""
    errors: List[str] = []
    if bool(images) == bool(videos):
        errors.append("Provide exactly one of images or videos.")
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
                path = Path(value)
                if not path.is_file():
                    errors.append(f"images[{index}]: image not found: {image}")
                    continue
                if path.suffix.lower() not in SUPPORTED_IMAGE_EXTS:
                    errors.append(
                        f"images[{index}]: unsupported image format '{path.suffix.lower()}'."
                    )
                elif path.stat().st_size > MAX_IMAGE_SIZE_MB * 1024 * 1024:
                    errors.append(
                        f"images[{index}]: image exceeds {MAX_IMAGE_SIZE_MB}MB: {image}"
                    )
                else:
                    try:
                        with path.open("rb") as image_file:
                            signature = image_file.read(8)
                    except OSError as exc:
                        errors.append(
                            f"images[{index}]: image cannot be read: {image}: {exc}"
                        )
                    else:
                        if not image_signature_matches(signature, path.suffix.lower()):
                            errors.append(
                                f"images[{index}]: image content does not match its "
                                f"'{path.suffix.lower()}' extension: {image}"
                            )
                        else:
                            # 中文注释：文件头正确后继续校验官方 6000 像素边长上限。
                            try:
                                dimensions = image_dimensions(path.read_bytes())
                            except OSError as exc:
                                errors.append(
                                    f"images[{index}]: image cannot be read: {image}: {exc}"
                                )
                            else:
                                if dimensions is None or any(
                                    side <= 0 or side > 6000 for side in dimensions
                                ):
                                    errors.append(
                                        f"images[{index}]: image dimensions must be "
                                        "readable and at most 6000x6000."
                                    )
    for index, video in enumerate(videos or []):
        if not _is_url(video):
            errors.append(f"videos[{index}] must be a valid HTTP(S) URL: {video}")
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


def load_image_as_data_url(image_path: str) -> str:
    path = Path(image_path)
    if not path.is_file():
        raise FileNotFoundError(f"Image not found: {image_path}")
    ext = path.suffix.lower()
    if ext not in SUPPORTED_IMAGE_EXTS:
        raise ValueError(
            f"Unsupported image format '{ext}'. Only {', '.join(sorted(SUPPORTED_IMAGE_EXTS))} are supported."
        )
    size_bytes = path.stat().st_size
    max_bytes = MAX_IMAGE_SIZE_MB * 1024 * 1024
    if size_bytes > max_bytes:
        raise ValueError(
            f"Image too large: {size_bytes / 1024 / 1024:.1f}MB (max {MAX_IMAGE_SIZE_MB}MB)."
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
    image_input = image_input.strip()
    if image_input.startswith("data:"):
        return image_input
    if image_input.startswith("base64:"):
        return f"data:image/jpeg;base64,{image_input[7:]}"
    if _is_url(image_input):
        return image_input
    return load_image_as_data_url(image_input)


def generate(
    images: Optional[List[str]] = None,
    videos: Optional[List[str]] = None,
    output_mode: str = OUTPUT_IMAGE,
    config_path: Optional[str] = None,
) -> Dict[str, Any]:
    basic_input_errors = validate_inputs(images, videos, "")
    try:
        config = load_config("glm5v", "prompt_gen", config_path=config_path)
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
                *validate_inputs(images, videos, model),
                *config_errors,
            ]
        )
    )

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
            max_tokens,
            connect_timeout_sec,
            read_timeout_sec,
            max_retries,
            endpoint,
        )
    )

    if output_mode == OUTPUT_IMAGE:
        system_prompt = SYSTEM_PROMPT_IMAGE
    elif output_mode == OUTPUT_VIDEO:
        system_prompt = SYSTEM_PROMPT_VIDEO
    else:
        system_prompt = SYSTEM_PROMPT_AUTO

    content = []
    if images:
        for img in images:
            content.append(
                {"type": "image_url", "image_url": {"url": process_image(img)}}
            )
    elif videos:
        for vid in videos:
            content.append({"type": "video_url", "video_url": {"url": vid.strip()}})

    content.append(
        {
            "type": "text",
            "text": "请分析以上参考素材，生成可以直接用于 AI 生图/生视频工具的 prompt。",
        }
    )

    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }

    payload: Dict[str, Any] = {
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": content},
        ],
        "temperature": temperature,
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
        "result": caption,
        "output_mode": output_mode,
        "usage": result.get("usage", {}),
        "finish_reason": finish_reason,
        # 只有正常停止才代表生成的提示词没有被 token 上限截断。
        "complete": finish_reason == "stop",
    }
    if finish_reason == "length":
        out["warning"] = "Output was truncated because the token limit was reached."
    elif finish_reason == "sensitive":
        out["warning"] = "Content blocked by safety review (finish_reason: sensitive)"
    return out


def main():
    parser = argparse.ArgumentParser(
        description="Generate prompts for text-to-image/text-to-video from reference images or videos using GLM-V"
    )

    input_group = parser.add_argument_group("Input (pick one)")
    input_group.add_argument(
        "--images",
        "-i",
        nargs="+",
        help="Image paths or URLs (jpg/png/jpeg, base64 OK)",
    )
    input_group.add_argument(
        "--videos", "-v", nargs="+", help="Video URLs (mp4/mkv/mov, URL only)"
    )

    parser.add_argument(
        "--mode",
        "-m",
        default=OUTPUT_IMAGE,
        choices=[OUTPUT_IMAGE, OUTPUT_VIDEO, "auto"],
        help="Output prompt type: image (default), video, or auto (both)",
    )
    parser.add_argument(
        "--config",
        default=str(DEFAULT_CONFIG_PATH),
        help="Shared GLM TOML configuration file",
    )
    parser.add_argument("--output", "-o", help="Save result to file")
    parser.add_argument(
        "--pretty", action="store_true", help="Pretty-print JSON error output"
    )

    args = parser.parse_args()

    try:
        result = generate(
            images=args.images,
            videos=args.videos,
            output_mode=args.mode,
            config_path=args.config,
        )
    except Exception as e:
        result = {"success": False, "error": str(e)}

    if not result.get("success"):
        indent = 2 if args.pretty else None
        print(json.dumps(result, ensure_ascii=False, indent=indent))
        sys.exit(1)

    if args.output:
        # 允许调用方直接指定尚不存在的嵌套输出目录。
        output_path = Path(args.output).resolve()
        output_path.parent.mkdir(parents=True, exist_ok=True)
        with output_path.open("w", encoding="utf-8") as f:
            f.write(result.get("result", ""))
        print(f"Result saved to: {output_path}", file=sys.stderr)
    else:
        print(result.get("result", ""))
    if result.get("warning"):
        print(f"Warning: {result['warning']}", file=sys.stderr)


if __name__ == "__main__":
    main()
