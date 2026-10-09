#!/usr/bin/env python3
"""Send one media class and a text prompt to GLM-5V-Turbo."""

import argparse
import base64
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from urllib.parse import urlparse

try:
    import requests
except ModuleNotFoundError as exc:
    raise SystemExit(
        "缺少 requests，请安装 scripts/requirements.txt 中的依赖。"
    ) from exc


SCRIPT_DIR = Path(__file__).resolve().parent
SKILL_DIR = SCRIPT_DIR.parent
MASTER_SCRIPTS_DIR = SKILL_DIR.parent / "glm-master-skill" / "scripts"
if str(MASTER_SCRIPTS_DIR) not in sys.path:
    sys.path.insert(0, str(MASTER_SCRIPTS_DIR))

from glm_config import (  # noqa: E402
    DEFAULT_CONFIG_PATH,
    ConfigError,
    load_config as load_shared_config,
    non_negative_int,
    positive_int,
)

DEFAULT_CONFIG = DEFAULT_CONFIG_PATH
RETRYABLE_STATUS_CODES = {408, 429, 500, 502, 503, 504}
IMAGE_TYPES = {
    ".png": "image/png",
    ".jpg": "image/jpeg",
    ".jpeg": "image/jpeg",
}
MAX_IMAGE_BYTES = 5 * 1024 * 1024
MAX_IMAGE_DIMENSION = 6000
VIDEO_TYPES = {".mp4", ".mov", ".mkv", ".webm", ".avi"}
FILE_TYPES = {
    ".pdf",
    ".doc",
    ".docx",
    ".ppt",
    ".pptx",
    ".xls",
    ".xlsx",
    ".csv",
    ".txt",
    ".md",
    ".html",
}


class StreamInterruptedError(RuntimeError):
    pass


def load_config(path, require_key=True):
    config = load_shared_config(
        "glm5v", config_path=path, require_api_key=require_key
    )
    if not str(config.get("base_url") or "").strip():
        raise ConfigError("base_url 不能为空")
    if not str(config.get("model") or "").strip():
        raise ConfigError("model 不能为空")
    for key in (
        "connect_timeout_sec",
        "read_timeout_sec",
        "max_tokens",
        "image_limit",
        "file_limit",
        "max_file_chars",
    ):
        positive_int(config, key)
    non_negative_int(config, "max_retries")
    for key in (
        "file_parser_url",
        "file_parser_tool_type",
        "file_parser_format",
    ):
        if not str(config.get(key) or "").strip():
            raise ConfigError(f"{key} 不能为空")
    if str(config.get("thinking") or "").strip().lower() != "enabled":
        raise ConfigError('thinking 必须保持为 "enabled"')
    if config.get("stream") is not True:
        raise ConfigError("stream 必须保持为 true")
    if str(config.get("auth") or "bearer").strip().lower() != "bearer":
        raise ConfigError('仅支持 auth = "bearer"')
    return config


def api_endpoint(base_url, suffix):
    base = str(base_url).strip().rstrip("/")
    if base.endswith(suffix):
        return base
    for known_suffix in ("/chat/completions", "/files/parser/sync"):
        if base.endswith(known_suffix):
            base = base[: -len(known_suffix)]
            break
    return f"{base}{suffix}"


def load_prompt(prompt, prompt_file):
    parts = []
    if prompt and prompt.strip():
        parts.append(prompt.strip())
    if prompt_file:
        path = Path(prompt_file)
        if not path.is_file():
            raise FileNotFoundError(f"找不到提示词文件: {path}")
        text = path.read_text(encoding="utf-8-sig").strip()
        if text:
            parts.append(text)
    if not parts:
        raise ValueError("必须提供 --prompt 或 --prompt-file")
    return "\n\n".join(parts)


def validate_url(raw_url, label):
    value = raw_url.strip() if isinstance(raw_url, str) else ""
    parsed = None
    host = None
    try:
        parsed = urlparse(value)
        host = parsed.hostname
        parsed.port
    except ValueError:
        pass
    if (
        not value
        or any(char.isspace() or ord(char) < 0x20 for char in value)
        or "\\" in value
        or parsed is None
        or parsed.scheme not in ("http", "https")
        or not host
        or parsed.username is not None
        or parsed.password is not None
    ):
        raise ValueError(f"{label} 必须是 HTTP(S) URL: {raw_url}")
    return value


def _validation_error(errors):
    return ValueError(
        f"输入预检发现 {len(errors)} 个问题:\n"
        + "\n".join(f"- {error}" for error in errors)
    )


def _image_signature_matches(path):
    try:
        with path.open("rb") as image_file:
            signature = image_file.read(12)
    except OSError:
        return False
    ext = path.suffix.lower()
    if ext == ".png":
        return signature.startswith(b"\x89PNG\r\n\x1a\n")
    if ext in {".jpg", ".jpeg"}:
        return signature.startswith(b"\xff\xd8\xff")
    return False


def image_dimensions(data):
    """从 PNG/JPEG 二进制解析宽高，不引入额外图片库依赖。"""
    if data.startswith(b"\x89PNG\r\n\x1a\n") and len(data) >= 24:
        return int.from_bytes(data[16:20], "big"), int.from_bytes(data[20:24], "big")
    if not data.startswith(b"\xff\xd8\xff"):
        return None
    offset = 2
    sof_markers = {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}
    while offset + 4 <= len(data):
        if data[offset] != 0xFF:
            offset += 1
            continue
        while offset < len(data) and data[offset] == 0xFF:
            offset += 1
        if offset >= len(data):
            break
        marker = data[offset]
        offset += 1
        if marker in {0x01, 0xD8, 0xD9} or 0xD0 <= marker <= 0xD7:
            continue
        if offset + 2 > len(data):
            break
        segment_length = int.from_bytes(data[offset : offset + 2], "big")
        if segment_length < 2 or offset + segment_length > len(data):
            break
        if marker in sof_markers and segment_length >= 7:
            height = int.from_bytes(data[offset + 3 : offset + 5], "big")
            width = int.from_bytes(data[offset + 5 : offset + 7], "big")
            return width, height
        offset += segment_length
    return None


def _collect_local_paths(
    raw_paths,
    allowed_suffixes,
    label,
    max_bytes=None,
    *,
    validate_image_content=False,
):
    paths = []
    seen = set()
    errors = []
    for index, raw_path in enumerate(raw_paths):
        path = Path(raw_path).resolve()
        if not path.is_file():
            errors.append(f"{label}[{index}] 找不到文件: {path}")
            continue
        if path.suffix.lower() not in allowed_suffixes:
            errors.append(f"{label}[{index}] 格式不支持: {path.name}")
            continue
        if max_bytes is not None and path.stat().st_size > max_bytes:
            errors.append(
                f"{label}[{index}] 文件过大: {path.name}，"
                f"上限 {max_bytes // (1024 * 1024)} MB"
            )
            continue
        if validate_image_content and not _image_signature_matches(path):
            errors.append(
                f"{label}[{index}] 内容与扩展名不匹配或文件不可读: {path.name}"
            )
            continue
        if validate_image_content:
            try:
                dimensions = image_dimensions(path.read_bytes())
            except OSError:
                dimensions = None
            if dimensions is None:
                errors.append(f"{label}[{index}] 无法解析图片尺寸: {path.name}")
                continue
            if any(side <= 0 or side > MAX_IMAGE_DIMENSION for side in dimensions):
                errors.append(
                    f"{label}[{index}] 图片宽高必须在 1-{MAX_IMAGE_DIMENSION} 像素内: "
                    f"{dimensions[0]}x{dimensions[1]}"
                )
                continue
        if path not in seen:
            seen.add(path)
            paths.append(path)
    return paths, errors


def validate_local_paths(raw_paths, allowed_suffixes, label, max_bytes=None):
    paths, errors = _collect_local_paths(
        raw_paths, allowed_suffixes, label, max_bytes=max_bytes
    )
    if errors:
        raise _validation_error(errors)
    return paths


def _collect_urls(raw_urls, label):
    urls = []
    seen = set()
    errors = []
    for index, raw_url in enumerate(raw_urls):
        try:
            value = validate_url(raw_url, f"{label}[{index}]")
            # 中文注释：重复 URL 只上传一次，输入上限按实际请求项计算。
            if value not in seen:
                seen.add(value)
                urls.append(value)
        except ValueError as exc:
            errors.append(str(exc))
    return urls, errors


def select_media(args, config):
    local_images, image_path_errors = _collect_local_paths(
        args.image,
        IMAGE_TYPES,
        "图片",
        max_bytes=MAX_IMAGE_BYTES,
        validate_image_content=True,
    )
    image_urls, image_url_errors = _collect_urls(args.image_url, "图片 URL")
    local_videos, video_path_errors = _collect_local_paths(
        args.video, VIDEO_TYPES, "视频"
    )
    video_urls, video_url_errors = _collect_urls(args.video_url, "视频 URL")
    local_files, file_path_errors = _collect_local_paths(
        args.file, FILE_TYPES, "文件"
    )
    file_urls, file_url_errors = _collect_urls(args.file_url, "文件 URL")
    errors = [
        *image_path_errors,
        *image_url_errors,
        *video_path_errors,
        *video_url_errors,
        *file_path_errors,
        *file_url_errors,
    ]

    groups = {
        "image": bool(args.image or args.image_url),
        "video": bool(args.video or args.video_url),
        "file": bool(args.file or args.file_url),
    }
    selected = [name for name, present in groups.items() if present]
    if len(selected) != 1:
        errors.append("一次请求必须且只能选择图片、视频或文件中的一组输入")

    mode = selected[0] if len(selected) == 1 else ""
    # 中文注释：上限检查使用预检后的去重结果，避免重复参数被误判为超限。
    if mode == "image" and len(local_images) + len(image_urls) > config["image_limit"]:
        errors.append(f"图片数量超过 image_limit={config['image_limit']}")
    if mode == "file" and len(local_files) + len(file_urls) > config["file_limit"]:
        errors.append(f"文件数量超过 file_limit={config['file_limit']}")
    if args.video and args.video_url:
        errors.append("本地视频抽帧和远程 video_url 不能在同一请求中混用")
    if len(args.video) > 1:
        errors.append("本地视频抽帧每次只支持一个视频")
    if mode == "video" and len(local_videos) + len(video_urls) > config["file_limit"]:
        errors.append(f"视频数量超过 file_limit={config['file_limit']}")
    if errors:
        raise _validation_error(errors)
    return {
        "mode": mode,
        "local_images": local_images,
        "image_urls": image_urls,
        "local_videos": local_videos,
        "video_urls": video_urls,
        "local_files": local_files,
        "file_urls": file_urls,
    }


def image_block(path):
    encoded = base64.b64encode(path.read_bytes()).decode("ascii")
    mime = IMAGE_TYPES[path.suffix.lower()]
    # 中文注释：使用官方兼容的 data URL，避免裸 base64 在不同 API 网关上解释不一致。
    return {
        "type": "image_url",
        "image_url": {"url": f"data:{mime};base64,{encoded}"},
    }


def extract_video_frames(path, output_dir, frame_limit):
    if os.environ.get("RCLAW_ANDROID") == "1":
        # APK：无 ffmpeg，经 MediaMetadataRetriever Java 桥抽帧。
        from rclaw_android_runtime import extract_video_frames as android_extract

        # 桥侧校验 1..8（与抽帧设计上限一致）：用户配置 image_limit > 8 时
        # 安卓按 8 抽，不因超范围被桥拒绝。
        frames_raw = android_extract(path, output_dir, min(int(frame_limit), 8))
        frames = []
        for frame in frames_raw:
            frame_path = Path(frame.get("path") or "")
            timestamp = float(frame.get("timestamp") or 0.0)
            if frame_path.is_file():
                frames.append((frame_path, timestamp))
        if frames:
            return frames
        raise RuntimeError(f"APK 视频抽帧未能产生任何画面: {path.name}")
    ffmpeg = shutil.which("ffmpeg")
    ffprobe = shutil.which("ffprobe")
    if not ffmpeg or not ffprobe:
        raise RuntimeError(
            "本地视频需要 ffmpeg/ffprobe；Termux 请运行: pkg install ffmpeg"
        )
    probe = subprocess.run(
        [
            ffprobe,
            "-v",
            "error",
            "-show_entries",
            "format=duration",
            "-of",
            "default=noprint_wrappers=1:nokey=1",
            str(path),
        ],
        capture_output=True,
        text=True,
        encoding="utf-8",
        check=False,
    )
    try:
        duration = float(probe.stdout.strip())
    except ValueError as exc:
        raise RuntimeError(f"无法读取视频时长: {path.name}") from exc
    if duration <= 0:
        raise RuntimeError(f"视频时长无效: {path.name}")

    count = min(int(frame_limit), max(1, min(8, int(duration + 0.999))))
    timestamps = [duration * (index + 0.5) / count for index in range(count)]
    frames = []
    for index, timestamp in enumerate(timestamps, 1):
        frame_path = Path(output_dir) / f"frame_{index:03d}.jpg"
        completed = subprocess.run(
            [
                ffmpeg,
                "-v",
                "error",
                "-ss",
                f"{timestamp:.3f}",
                "-i",
                str(path),
                "-frames:v",
                "1",
                "-vf",
                "scale=1600:-2:force_original_aspect_ratio=decrease",
                "-q:v",
                "3",
                "-y",
                str(frame_path),
            ],
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
        if completed.returncode == 0 and frame_path.is_file():
            frames.append((frame_path, timestamp))
    if not frames:
        raise RuntimeError(f"ffmpeg 未能从视频抽取画面: {path.name}")
    return frames


def response_error_detail(response):
    response.encoding = "utf-8"
    try:
        return json.dumps(response.json(), ensure_ascii=False)
    except ValueError:
        return response.text


def retry_delay(response, attempt):
    raw = response.headers.get("Retry-After", "")
    try:
        return max(0.0, min(float(raw), 30.0))
    except (TypeError, ValueError):
        return min(2**attempt, 8)


def parse_local_file(config, path):
    endpoint = str(config["file_parser_url"])
    headers = {"Authorization": f"Bearer {config['_api_key']}"}
    data = {
        "tool_type": str(config["file_parser_tool_type"]),
        "file_type": path.suffix.lower().lstrip("."),
        "format_type": str(config["file_parser_format"]),
    }
    retryable_exceptions = (
        requests.exceptions.Timeout,
        requests.exceptions.ConnectionError,
    )
    max_retries = int(config["max_retries"])
    for attempt in range(max_retries + 1):
        try:
            with path.open("rb") as handle:
                response = requests.post(
                    endpoint,
                    headers=headers,
                    data=data,
                    files={"file": (path.name, handle)},
                    timeout=(
                        int(config["connect_timeout_sec"]),
                        int(config["read_timeout_sec"]),
                    ),
                )
            with response:
                if response.status_code in RETRYABLE_STATUS_CODES:
                    if attempt == max_retries:
                        raise RuntimeError(
                            f"文件解析在 {attempt + 1} 次尝试后仍失败: {path.name}"
                        )
                    time.sleep(retry_delay(response, attempt))
                    continue
                if not response.ok:
                    raise RuntimeError(
                        f"文件解析失败: {path.name}, HTTP {response.status_code}\n"
                        f"{response_error_detail(response)}"
                    )
                result = response.json()
                if result.get("status") != "succeeded" or not isinstance(
                    result.get("content"), str
                ):
                    raise RuntimeError(
                        f"文件解析未成功: {path.name}, status={result.get('status')}"
                    )
                return result["content"]
        except retryable_exceptions as exc:
            if attempt == max_retries:
                raise RuntimeError(f"文件解析连接失败: {path.name}") from exc
            time.sleep(min(2**attempt, 8))
    raise RuntimeError(f"文件解析未返回结果: {path.name}")


def parse_local_files(config, paths):
    parsed = []
    if not paths:
        return parsed
    per_file_limit = max(1, int(config["max_file_chars"]) // len(paths))
    for path in paths:
        content = parse_local_file(config, path)
        clipped = len(content) > per_file_limit
        content = content[:per_file_limit]
        parsed.append((path.name, content, clipped))
    return parsed


def build_content(user_prompt, media, parsed_files, video_frames=None):
    blocks = []
    manifest = []
    mode = media["mode"]
    if mode == "image":
        for index, path in enumerate(media["local_images"], 1):
            blocks.append(image_block(path))
            manifest.append(f"图{index}: {path.name}")
        offset = len(manifest)
        for index, url in enumerate(media["image_urls"], offset + 1):
            blocks.append({"type": "image_url", "image_url": {"url": url}})
            manifest.append(f"图{index}: {url}")
    elif mode == "video":
        if video_frames:
            source_name = media["local_videos"][0].name
            for index, (path, timestamp) in enumerate(video_frames, 1):
                blocks.append(image_block(path))
                manifest.append(
                    f"本地视频 {source_name} 抽帧{index}: {timestamp:.2f} 秒"
                )
        else:
            for index, url in enumerate(media["video_urls"], 1):
                blocks.append({"type": "video_url", "video_url": {"url": url}})
                manifest.append(f"视频{index}: {url}")
    else:
        for name, _, _ in parsed_files:
            manifest.append(f"本地文件: {name}")
        for url in media["file_urls"]:
            blocks.append({"type": "file_url", "file_url": {"url": url}})
            manifest.append(f"远程文件: {url}")

    prompt_parts = ["本次输入清单：", "\n".join(f"- {item}" for item in manifest)]
    if parsed_files:
        prompt_parts.append("本地文件解析内容：")
        for name, content, clipped in parsed_files:
            suffix = "\n[内容因长度上限被截断]" if clipped else ""
            prompt_parts.append(
                f"<<<FILE {name} BEGIN>>>\n{content}{suffix}\n<<<FILE {name} END>>>"
            )
    if video_frames:
        prompt_parts.append(
            "说明：输入来自本地视频的均匀时间抽帧，只包含视觉画面，不包含音频。"
        )
    prompt_parts.extend(["用户任务：", user_prompt])
    blocks.append({"type": "text", "text": "\n\n".join(prompt_parts)})
    return blocks


def parse_stream_response(response):
    content_parts = []
    finish_reason = ""
    saw_done = False
    aggregate = {}
    response.encoding = "utf-8"
    for line in response.iter_lines(decode_unicode=True):
        if isinstance(line, bytes):
            try:
                line = line.decode("utf-8")
            except UnicodeDecodeError as exc:
                raise StreamInterruptedError("SSE 流包含不完整的 UTF-8 数据") from exc
        if not line or not line.startswith("data:"):
            continue
        payload = line[5:].lstrip()
        if payload.strip() == "[DONE]":
            saw_done = True
            break
        try:
            event = json.loads(payload)
        except json.JSONDecodeError as exc:
            raise StreamInterruptedError("SSE 流包含不完整的 JSON 事件") from exc
        if not isinstance(event, dict):
            raise RuntimeError("SSE 事件必须是 JSON 对象")
        if event.get("error") is not None:
            error = event["error"]
            detail = error.get("message") if isinstance(error, dict) else str(error)
            raise RuntimeError(f"GLM-5V-Turbo 流式响应返回错误: {detail}")
        for key in ("id", "request_id", "created", "model", "usage"):
            if key in event:
                aggregate[key] = event[key]
        choices = event.get("choices")
        if not isinstance(choices, list) or not choices:
            continue
        choice = choices[0]
        if not isinstance(choice, dict):
            raise RuntimeError("SSE choices[0] 必须是对象")
        delta = choice.get("delta") or {}
        if not isinstance(delta, dict):
            raise RuntimeError("SSE delta 必须是对象")
        content = delta.get("content")
        if content is not None:
            if not isinstance(content, str):
                raise RuntimeError("SSE delta.content 必须是字符串")
            content_parts.append(content)
        # Intentionally ignore delta.reasoning_content.
        current_finish = choice.get("finish_reason")
        if current_finish:
            if not isinstance(current_finish, str):
                raise RuntimeError("SSE finish_reason 必须是字符串")
            finish_reason = current_finish
    if not finish_reason:
        ending = "收到 [DONE] 但缺少 finish_reason" if saw_done else "EOF 前未收到终止事件"
        raise StreamInterruptedError(f"GLM-5V-Turbo 流式响应不完整：{ending}")
    aggregate["choices"] = [{
        "index": 0,
        "message": {"role": "assistant", "content": "".join(content_parts)},
        "finish_reason": finish_reason,
    }]
    return aggregate


def call_glm5v(config, content):
    body = {
        "model": str(config["model"]),
        "messages": [{"role": "user", "content": content}],
        "thinking": {"type": str(config["thinking"]).strip().lower()},
        "stream": config["stream"],
    }
    max_tokens = int(config.get("max_tokens") or 0)
    if max_tokens > 0:
        body[str(config.get("max_tokens_key") or "max_tokens")] = max_tokens

    retryable_exceptions = (
        requests.exceptions.Timeout,
        requests.exceptions.ConnectionError,
        requests.exceptions.ChunkedEncodingError,
        requests.exceptions.ContentDecodingError,
    )
    max_retries = int(config["max_retries"])
    for attempt in range(max_retries + 1):
        try:
            response = requests.post(
                api_endpoint(config["base_url"], "/chat/completions"),
                headers={
                    "Authorization": f"Bearer {config['_api_key']}",
                    "Content-Type": "application/json",
                },
                json=body,
                timeout=(
                    int(config["connect_timeout_sec"]),
                    int(config["read_timeout_sec"]),
                ),
                stream=True,
            )
            with response:
                if response.status_code in RETRYABLE_STATUS_CODES:
                    if attempt == max_retries:
                        raise RuntimeError(
                            f"GLM-5V-Turbo 请求在 {attempt + 1} 次尝试后仍失败"
                        )
                    print(
                        f"WARNING: HTTP {response.status_code}，保持 thinking=enabled 重试。",
                        file=sys.stderr,
                    )
                    time.sleep(retry_delay(response, attempt))
                    continue
                if not response.ok:
                    raise RuntimeError(
                        f"GLM-5V-Turbo 请求失败: HTTP {response.status_code}\n"
                        f"{response_error_detail(response)}"
                    )
                try:
                    return parse_stream_response(response)
                except StreamInterruptedError:
                    if attempt == max_retries:
                        raise
                    print(
                        "WARNING: 流式响应未完整结束，已丢弃本次内容并保持 thinking=enabled 重试。",
                        file=sys.stderr,
                    )
        except retryable_exceptions as exc:
            if attempt == max_retries:
                raise RuntimeError(
                    f"GLM-5V-Turbo 在 {attempt + 1} 次尝试后仍连接或读取失败"
                ) from exc
            print(
                "WARNING: 连接或流读取失败，已丢弃本次内容并保持 thinking=enabled 重试。",
                file=sys.stderr,
            )
            time.sleep(min(2**attempt, 8))
    raise RuntimeError("GLM-5V-Turbo 流式请求未返回结果")


def extract_result(raw_response):
    try:
        choice = raw_response["choices"][0]
        text = choice["message"]["content"]
        finish_reason = choice["finish_reason"]
    except (KeyError, IndexError, TypeError) as exc:
        raise RuntimeError("GLM-5V-Turbo 响应缺少文本结果") from exc
    if not isinstance(text, str):
        raise RuntimeError("GLM-5V-Turbo 文本结果格式无效")
    return text, str(finish_reason or "")


def input_summary(media):
    return {
        "mode": media["mode"],
        "local_images": [str(path) for path in media["local_images"]],
        "image_urls": media["image_urls"],
        "local_videos": [str(path) for path in media["local_videos"]],
        "video_urls": media["video_urls"],
        "local_files": [str(path) for path in media["local_files"]],
        "file_urls": media["file_urls"],
    }


def main():
    parser = argparse.ArgumentParser(description="使用 GLM-5V-Turbo 分析图片、视频或文件")
    parser.add_argument("--prompt", default="", help="多模态分析提示词")
    parser.add_argument("--prompt-file", default="", help="UTF-8 提示词文件")
    parser.add_argument("--image", action="append", default=[], help="本地图片路径")
    parser.add_argument("--image-url", action="append", default=[], help="图片 URL")
    parser.add_argument("--video", action="append", default=[], help="本地视频路径（ffmpeg 抽帧）")
    parser.add_argument("--video-url", action="append", default=[], help="视频 URL")
    parser.add_argument("--file", action="append", default=[], help="本地文件路径")
    parser.add_argument("--file-url", action="append", default=[], help="文件 URL")
    parser.add_argument("--output", default="", help="可选的 UTF-8 文本输出路径")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG), help="GLM 配置文件")
    parser.add_argument("--dry-run", action="store_true", help="校验输入但不调用 API")
    args = parser.parse_args()

    preflight_errors = []
    user_prompt = None
    media = None
    config = None
    try:
        user_prompt = load_prompt(args.prompt, args.prompt_file)
    except (OSError, ValueError) as exc:
        preflight_errors.append(str(exc))
    try:
        config = load_config(args.config, require_key=not args.dry_run)
    except (ConfigError, OSError, ValueError) as exc:
        preflight_errors.append(str(exc))
    try:
        media = select_media(
            args,
            config
            if config is not None
            else {"image_limit": sys.maxsize, "file_limit": sys.maxsize},
        )
    except (OSError, ValueError) as exc:
        lines = str(exc).splitlines()
        preflight_errors.extend(
            line[2:] if line.startswith("- ") else line
            for line in (lines[1:] if len(lines) > 1 else lines)
        )
    if preflight_errors:
        raise _validation_error(preflight_errors)
    assert user_prompt is not None and media is not None and config is not None

    output_path = Path(args.output).resolve() if args.output else None
    if args.dry_run:
        result = {
            "endpoint": api_endpoint(config["base_url"], "/chat/completions"),
            "model": config["model"],
            "thinking": config["thinking"],
            "stream": config["stream"],
            "connect_timeout_sec": config["connect_timeout_sec"],
            "read_timeout_sec": config["read_timeout_sec"],
            "max_retries": config["max_retries"],
            "prompt_chars": len(user_prompt),
            "inputs": input_summary(media),
            "output_path": str(output_path) if output_path else None,
        }
        print(json.dumps(result, ensure_ascii=False, indent=2))
        return 0

    parsed_files = parse_local_files(config, media["local_files"])
    if media["local_videos"]:
        with tempfile.TemporaryDirectory(prefix="rclaw-glm5v-video-") as frame_dir:
            video_frames = extract_video_frames(
                media["local_videos"][0],
                frame_dir,
                config["image_limit"],
            )
            content = build_content(
                user_prompt, media, parsed_files, video_frames=video_frames
            )
            raw_response = call_glm5v(config, content)
    else:
        content = build_content(user_prompt, media, parsed_files)
        raw_response = call_glm5v(config, content)
    text, finish_reason = extract_result(raw_response)
    if finish_reason not in ("stop", "length"):
        raise RuntimeError(f"GLM-5V-Turbo 未正常结束（finish_reason={finish_reason}）")
    if finish_reason == "length":
        print("WARNING: 模型输出达到长度上限，结果可能不完整。", file=sys.stderr)
    if output_path:
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(text.rstrip() + "\n", encoding="utf-8")

    result = {
        "text": text,
        "finish_reason": finish_reason,
        "complete": finish_reason == "stop",
        "output_path": str(output_path) if output_path else None,
        "inputs": input_summary(media),
        "parsed_files": [name for name, _, _ in parsed_files],
        "usage": raw_response.get("usage") or {},
    }
    print("__GLM5V_RESULT_BEGIN__")
    print(json.dumps(result, ensure_ascii=False, indent=2))
    print("__GLM5V_RESULT_END__")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        raise SystemExit(1)
