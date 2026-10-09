#!/usr/bin/env python3
"""
Seedance 2.5 视频生成 CLI - 火山方舟异步视频生成任务客户端

覆盖 Seedance 2.5 全部能力：文生视频（单段最长 30 秒）、首帧/首尾帧生视频、
全模态参考生视频（图片/视频/音频参考，上限 30+10+10 共 50 个素材）、
视频编辑（替换主体、增删对象、局部重绘、音频编辑，支持时间戳）、
视频延长（向前/向后，可多轮）、样片模式（draft 480p 预览，再出 1080p 成片）。

流程：创建任务 -> 轮询状态 -> 下载结果视频到本地。
请求配置从本 skill 目录的 config.toml 读取；API Key 从系列共享的
skills/ark_key.toml 读取。均不使用环境变量。

API 文档: https://ark.volcengine.com/docs/ark/create-video-generation-task-api
"""

from __future__ import annotations

import argparse
import base64
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import tomllib

DEFAULT_CONFIG_PATH = Path(__file__).resolve().parents[1] / "config.toml"
DEFAULT_KEY_CONFIG_PATH = Path(__file__).resolve().parents[2] / "ark_key.toml"

VALID_RESOLUTIONS = ("480p", "720p", "1080p")
VALID_RATIOS = ("16:9", "4:3", "1:1", "3:4", "9:16", "21:9", "adaptive")
VALID_TASK_TYPES = ("auto", "reference", "edit", "extend")
MIN_DURATION = 4
MAX_DURATION = 30
SMART_DURATION = -1
MIN_EXPIRES_AFTER = 3600
MAX_EXPIRES_AFTER = 259200
MAX_PRIORITY = 9
MAX_SAFETY_ID_LEN = 64
MAX_PROMPT_CHARS = 5000
TERMINAL_OK = "succeeded"
TERMINAL_FAIL = ("failed", "cancelled", "expired")
DOWNLOAD_CHUNK_BYTES = 1024 * 1024
# 本地图片转 Base64 的格式映射；视频不支持 Base64（仅 URL / 素材 ID）。
IMAGE_MIMES = {
    "jpeg": "jpeg", "jpg": "jpeg", "png": "png", "webp": "webp",
    "bmp": "bmp", "tiff": "tiff", "tif": "tiff", "gif": "gif",
    "heic": "heic", "heif": "heif",
}
AUDIO_MIMES = {"wav": "wav", "mp3": "mp3"}
EDIT_KEYWORDS = ("编辑", "增加", "加上", "删除", "删掉", "去掉", "修改", "替换", "改成")
EXTEND_KEYWORDS = ("延长", "延续", "续写")


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
    """读取并校验 [seedance] 配置表，任何缺失或非法字段都在付费前拒绝。"""
    if not path.is_file():
        fail("CONFIG_ERROR", f"配置文件不存在: {path}")
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        fail("CONFIG_ERROR", f"无法读取配置文件 {path}: {exc}")
    config = data.get("seedance")
    if not isinstance(config, dict):
        fail("CONFIG_ERROR", f"配置文件缺少 [seedance] 表: {path}")

    def text(key: str) -> str:
        value = str(config.get(key) or "").strip()
        if not value:
            fail("CONFIG_ERROR", f"[seedance].{key} 不能为空（配置文件: {path}）")
        return value

    if config.get("api_key"):
        fail(
            "CONFIG_ERROR",
            f"API Key 已集中到 {DEFAULT_KEY_CONFIG_PATH.name}，"
            f"请从本文件删除 [seedance].api_key（配置文件: {path}）",
        )
    text("model")
    for key in ("submit_url", "task_url_template"):
        value = text(key)
        if not value.startswith(("https://", "http://")) or any(char.isspace() for char in value):
            fail("CONFIG_ERROR", f"[seedance].{key} 不是有效 HTTP(S) URL（配置文件: {path}）")
    if "{task_id}" not in str(config["task_url_template"]):
        fail("CONFIG_ERROR", f"[seedance].task_url_template 必须包含 {{task_id}}（配置文件: {path}）")
    if str(config.get("resolution")) not in VALID_RESOLUTIONS:
        fail("CONFIG_ERROR", f"[seedance].resolution 必须是 {'/'.join(VALID_RESOLUTIONS)}（配置文件: {path}）")
    if str(config.get("ratio")) not in VALID_RATIOS:
        fail("CONFIG_ERROR", f"[seedance].ratio 必须是 {'/'.join(VALID_RATIOS)}（配置文件: {path}）")
    duration = config.get("duration")
    if (
        isinstance(duration, bool)
        or not isinstance(duration, int)
        or not (duration == SMART_DURATION or MIN_DURATION <= duration <= MAX_DURATION)
    ):
        fail(
            "CONFIG_ERROR",
            f"[seedance].duration 必须是 {SMART_DURATION} 或 {MIN_DURATION}-{MAX_DURATION} 的整数（配置文件: {path}）",
        )
    for key in ("generate_audio", "watermark"):
        if not isinstance(config.get(key), bool):
            fail("CONFIG_ERROR", f"[seedance].{key} 必须为 true 或 false（配置文件: {path}）")
    if str(config.get("output_format")) not in ("mp4", "mov"):
        fail("CONFIG_ERROR", f"[seedance].output_format 必须是 mp4/mov（配置文件: {path}）")
    for key in (
        "request_timeout_sec", "download_timeout_sec", "max_download_bytes",
        "max_input_image_bytes", "max_input_audio_bytes", "max_request_body_bytes",
        "max_images", "max_videos", "max_audios", "poll_timeout_sec",
    ):
        value = config.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            fail("CONFIG_ERROR", f"[seedance].{key} 必须是正整数（配置文件: {path}）")
    if (
        isinstance(config.get("poll_interval_sec"), bool)
        or not isinstance(config.get("poll_interval_sec"), int)
        or config["poll_interval_sec"] < 1
    ):
        fail("CONFIG_ERROR", f"[seedance].poll_interval_sec 必须是至少 1 的整数（配置文件: {path}）")
    retries = config.get("max_retries")
    if isinstance(retries, bool) or not isinstance(retries, int) or retries < 0:
        fail("CONFIG_ERROR", f"[seedance].max_retries 必须是非负整数（配置文件: {path}）")
    return config


def load_api_key(path: Path) -> str:
    """从系列共享的 ark_key.toml 读取 api_key（火山方舟系列共用）。"""
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


def parse_args():
    parser = argparse.ArgumentParser(
        description="Seedance 2.5 video generation client (Volcano Ark async tasks)",
    )
    sub = parser.add_subparsers(dest="command", required=True)

    gen = sub.add_parser("generate", help="Create task, poll until done, download video")
    gen.add_argument("-p", "--prompt", type=str, default="",
                     help="Text prompt (optional when reference assets are given; "
                          "required for text-to-video; forbidden with --draft-task-id)")
    gen.add_argument("--save", type=str, required=True,
                     help="Local output path (extension normalized to .mp4/.mov)")
    gen.add_argument("--image", action="append", default=None, metavar="URL_OR_PATH",
                     help="Reference image (repeatable, up to 30): http(s) URL, asset://ID or local file")
    gen.add_argument("--first-frame", type=str, default=None, metavar="URL_OR_PATH",
                     help="First frame image (role=first_frame)")
    gen.add_argument("--last-frame", type=str, default=None, metavar="URL_OR_PATH",
                     help="Last frame image (role=last_frame); requires --first-frame")
    gen.add_argument("--video", action="append", default=None, metavar="URL",
                     help="Reference video (repeatable, up to 10): http(s) URL or asset://ID only")
    gen.add_argument("--audio", action="append", default=None, metavar="URL_OR_PATH",
                     help="Reference audio (repeatable, up to 10): http(s) URL, asset://ID, or local wav/mp3")
    gen.add_argument("--resolution", type=str, default=None, choices=VALID_RESOLUTIONS)
    gen.add_argument("--ratio", type=str, default=None, choices=VALID_RATIOS)
    gen.add_argument("--duration", type=int, default=None,
                     help=f"{SMART_DURATION} (smart) or {MIN_DURATION}-{MAX_DURATION} seconds")
    gen.add_argument("--task-type", type=str, default=None, choices=VALID_TASK_TYPES,
                     help="omni_reference_task_type guide: auto/reference/edit/extend")
    gen.add_argument("--mute", action="store_true", help="Generate silent video (default has audio)")
    gen.add_argument("--watermark", action="store_true", help='Add "AI 生成" watermark')
    gen.add_argument("--output-format", type=str, default=None, choices=("mp4", "mov"))
    gen.add_argument("--return-last-frame", action="store_true",
                     help="Also return the last frame as a jpeg for chained generation")
    gen.add_argument("--draft", action="store_true",
                     help="Draft mode: cheap 480p preview (resolution forced to 480p)")
    gen.add_argument("--draft-task-id", type=str, default=None, metavar="ID",
                     help="Generate final 1080p video from a draft task id")
    gen.add_argument("--web-search", action="store_true",
                     help="Enable web_search tool so the model can fetch fresh internet info (adds latency)")
    gen.add_argument("--priority", type=int, default=None, metavar="N",
                     help=f"Queue priority 0-{MAX_PRIORITY} (higher runs first within the endpoint)")
    gen.add_argument("--execution-expires-after", type=int, default=None, metavar="SEC",
                     help=f"Task expiry threshold in seconds [{MIN_EXPIRES_AFTER}, {MAX_EXPIRES_AFTER}] (default 48h)")
    gen.add_argument("--safety-identifier", type=str, default=None, metavar="ID",
                     help="End-user identifier (<=64 chars) for platform policy detection")

    chk = sub.add_parser("check", help="Query task status by id")
    chk.add_argument("--task-id", type=str, required=True)
    chk.add_argument("--save", type=str, default=None,
                     help="Optional: download the video if the task already succeeded")

    cnl = sub.add_parser("cancel", help="Cancel (queued) or delete a task")
    cnl.add_argument("--task-id", type=str, required=True)

    for p in (gen, chk, cnl):
        p.add_argument("--config", type=str, default=str(DEFAULT_CONFIG_PATH))
        p.add_argument("--key-config", type=str, default=str(DEFAULT_KEY_CONFIG_PATH))
    return parser.parse_args()


def read_local_asset(raw: str, mimes: dict, max_bytes: int, kind: str) -> str:
    """本地素材转 Base64 data URI；URL 与 asset:// 原样透传。"""
    if raw.startswith(("http://", "https://", "asset://")):
        return raw
    path = Path(raw).expanduser()
    if not path.is_file():
        fail("INVALID_INPUT", f"{kind}不是 URL/素材ID 也不是存在的本地文件: {raw}")
    ext = path.suffix.lower().lstrip(".")
    if ext not in mimes:
        fail("INVALID_INPUT", f"不支持的{kind}格式 .{ext}（允许: {', '.join(sorted(set(mimes)))}）: {raw}")
    size = path.stat().st_size
    if size > max_bytes:
        fail("INVALID_INPUT", f"{kind}超过 {max_bytes} 字节上限（实际 {size} 字节）: {raw}")
    encoded = base64.b64encode(path.read_bytes()).decode("ascii")
    return f"data:{'image' if kind == '图片' else 'audio'}/{mimes[ext]};base64,{encoded}"


def normalize_video_input(raw: str) -> str:
    """视频仅支持公网 URL 与素材 ID，不接受本地文件（API 无视频 Base64 通道）。"""
    if raw.startswith(("http://", "https://", "asset://")):
        return raw
    fail(
        "INVALID_VIDEO",
        f"参考视频只接受 http(s) URL 或 asset:// 素材 ID，不支持本地文件: {raw}。"
        f"请先把视频上传到可公网访问的存储（如火山引擎 TOS）再提供 URL",
    )


def validate_task_options(args):
    """校验与任务管理相关的通用参数；样片转正式与普通任务共用。"""
    if args.priority is not None and not 0 <= args.priority <= MAX_PRIORITY:
        fail("INVALID_PARAMS", f"--priority 必须在 0-{MAX_PRIORITY} 之间，实际 {args.priority}")
    if args.execution_expires_after is not None and not (
        MIN_EXPIRES_AFTER <= args.execution_expires_after <= MAX_EXPIRES_AFTER
    ):
        fail(
            "INVALID_PARAMS",
            f"--execution-expires-after 必须在 [{MIN_EXPIRES_AFTER}, {MAX_EXPIRES_AFTER}] 秒之间，"
            f"实际 {args.execution_expires_after}",
        )
    if args.safety_identifier is not None and len(args.safety_identifier) > MAX_SAFETY_ID_LEN:
        fail("INVALID_PARAMS", f"--safety-identifier 长度须不超过 {MAX_SAFETY_ID_LEN} 字符")


def apply_task_options(payload: dict, args):
    """把联网搜索 / 优先级 / 超时阈值 / 用户标识写入请求体。

    样片转正式任务仅 priority / execution_expires_after / safety_identifier
    可指定；web_search 已在 build_request 的样片分支单独拒绝。
    """
    if args.web_search:
        payload["tools"] = [{"type": "web_search"}]
    if args.priority is not None:
        payload["priority"] = args.priority
    if args.execution_expires_after is not None:
        payload["execution_expires_after"] = args.execution_expires_after
    if args.safety_identifier is not None:
        payload["safety_identifier"] = args.safety_identifier


def build_request(args, config: dict) -> dict:
    """构建创建任务请求体，并完成全部本地前置校验与参数自动锁定。"""
    notes = []
    validate_task_options(args)

    if args.duration is not None and not (args.duration == SMART_DURATION or MIN_DURATION <= args.duration <= MAX_DURATION):
        fail("INVALID_PARAMS", f"--duration 必须是 {SMART_DURATION}（智能）或 {MIN_DURATION}-{MAX_DURATION}，实际 {args.duration}")

    draft_id = (args.draft_task_id or "").strip()
    if draft_id:
        # 样片转正式：content 只允许 draft_task；提示词与素材由模型自动复用，禁止传入。
        if args.draft:
            fail("INVALID_PARAMS", "--draft 是生成样片，--draft-task-id 是基于已有样片出正式片，两者不可同传")
        for name, value in (
            ("--prompt", args.prompt), ("--image", args.image), ("--first-frame", args.first_frame),
            ("--last-frame", args.last_frame), ("--video", args.video), ("--audio", args.audio),
            ("--task-type", args.task_type), ("--ratio", args.ratio), ("--duration", args.duration),
        ):
            if value:
                fail("INVALID_PARAMS", f"基于样片生成正式视频时 {name} 由模型自动复用，禁止传入")
        if args.mute:
            fail("INVALID_PARAMS", "基于样片生成正式视频时是否生成声音沿用样片任务，不支持 --mute")
        if args.web_search:
            fail("INVALID_PARAMS", "基于样片生成正式视频不支持 --web-search（tools 不在官方可重新指定参数之列）")
        resolution = args.resolution or "1080p"
        if resolution != "1080p":
            fail("INVALID_PARAMS", "基于样片生成正式视频仅支持 1080p 分辨率")
        content = [{"type": "draft_task", "draft_task": {"id": draft_id}}]
        payload = {"model": config["model"], "content": content, "resolution": resolution}
        if args.output_format:
            payload["output_format"] = args.output_format
        if args.watermark:
            payload["watermark"] = True
        if args.return_last_frame:
            payload["return_last_frame"] = True
        apply_task_options(payload, args)
        return {"payload": payload, "notes": ["样片转正式：仅分辨率/输出格式/水印/尾帧及任务管理参数可指定"]}

    prompt = (args.prompt or "").strip()
    if len(prompt) > MAX_PROMPT_CHARS:
        fail("INVALID_PROMPT", f"Prompt 须不超过 {MAX_PROMPT_CHARS} 字符（建议中文 500 字内）")

    images = [read_local_asset(u, IMAGE_MIMES, config["max_input_image_bytes"], "图片") for u in (args.image or [])]
    videos = [normalize_video_input(u) for u in (args.video or [])]
    audios = [read_local_asset(u, AUDIO_MIMES, config["max_input_audio_bytes"], "音频") for u in (args.audio or [])]
    first_frame = read_local_asset(args.first_frame, IMAGE_MIMES, config["max_input_image_bytes"], "图片") if args.first_frame else None
    last_frame = read_local_asset(args.last_frame, IMAGE_MIMES, config["max_input_image_bytes"], "图片") if args.last_frame else None

    frame_mode = bool(first_frame or last_frame)
    if last_frame and not first_frame:
        fail("INVALID_PARAMS", "首尾帧生视频需要同时提供 --first-frame 与 --last-frame（首帧单图可只传 --first-frame）")
    if frame_mode and (images or videos or audios):
        fail(
            "INVALID_PARAMS",
            "首帧/首尾帧生视频与全模态参考（reference_image/reference_video/reference_audio）互斥，不可混用",
        )

    total_images = len(images) + (2 if frame_mode else 0)
    if total_images > config["max_images"]:
        fail("INVALID_INPUT", f"图片数量超过上限 {config['max_images']}（含首尾帧），实际 {total_images} 张")
    if len(videos) > config["max_videos"]:
        fail("INVALID_INPUT", f"参考视频超过上限 {config['max_videos']}，实际 {len(videos)} 个")
    if len(audios) > config["max_audios"]:
        fail("INVALID_INPUT", f"参考音频超过上限 {config['max_audios']}，实际 {len(audios)} 段")
    if total_images + len(videos) + len(audios) > 50:
        fail("INVALID_INPUT", "参考素材总数超过 50 个上限（30 图 + 10 视频 + 10 音频）")

    if not prompt and not (images or videos or audios or frame_mode):
        fail("INVALID_PARAMS", "纯文生视频必须提供 --prompt；或提供至少一个参考素材")

    task_type = args.task_type or "auto"
    if task_type == "edit" and not videos:
        fail("INVALID_PARAMS", "视频编辑任务（--task-type edit）必须至少提供 1 个 --video 参考视频")
    if task_type == "extend" and not videos:
        fail("INVALID_PARAMS", "视频延长任务（--task-type extend）必须至少提供 1 个 --video 参考视频")
    if prompt and task_type == "edit" and not any(k in prompt for k in EDIT_KEYWORDS):
        notes.append("编辑任务提示词建议包含关键词：编辑视频/增加/加上/删除/去掉/修改/替换/改成")
    if prompt and task_type == "extend" and not any(k in prompt for k in EXTEND_KEYWORDS):
        notes.append("延长任务提示词建议包含关键词：向前/向后延长、延续、续写")

    # 参数自动锁定：编辑/延长/首尾帧任务的 ratio、duration 特殊限制。
    resolution = args.resolution or str(config["resolution"])
    ratio = args.ratio or str(config["ratio"])
    duration = args.duration if args.duration is not None else int(config["duration"])
    if args.draft:
        if resolution != "480p":
            notes.append(f"样片模式固定 480p（已从 {resolution} 调整）")
            resolution = "480p"
    if task_type == "edit":
        if ratio != "adaptive":
            notes.append(f"视频编辑任务 ratio 固定 adaptive（已从 {ratio} 调整）")
            ratio = "adaptive"
        if duration != SMART_DURATION:
            notes.append(f"视频编辑任务 duration 固定 -1（已从 {duration} 调整）")
            duration = SMART_DURATION
    if task_type == "extend" and ratio != "adaptive":
        notes.append(f"视频延长任务 ratio 固定 adaptive（已从 {ratio} 调整）")
        ratio = "adaptive"
    if frame_mode and ratio != "adaptive":
        notes.append(f"首帧/首尾帧任务 ratio 固定 adaptive（已从 {ratio} 调整）")
        ratio = "adaptive"

    content = []
    if prompt:
        content.append({"type": "text", "text": prompt})
    if first_frame:
        content.append({"type": "image_url", "image_url": {"url": first_frame}, "role": "first_frame"})
    if last_frame:
        content.append({"type": "image_url", "image_url": {"url": last_frame}, "role": "last_frame"})
    for url in images:
        content.append({"type": "image_url", "image_url": {"url": url}, "role": "reference_image"})
    for url in videos:
        content.append({"type": "video_url", "video_url": {"url": url}, "role": "reference_video"})
    for url in audios:
        content.append({"type": "audio_url", "audio_url": {"url": url}, "role": "reference_audio"})

    payload = {
        "model": config["model"],
        "content": content,
        "resolution": resolution,
        "ratio": ratio,
        "duration": duration,
        "generate_audio": not args.mute,
        "watermark": args.watermark or bool(config["watermark"]),
        "output_format": args.output_format or str(config["output_format"]),
    }
    if task_type != "auto":
        payload["omni_reference_task_type"] = task_type
    if args.return_last_frame:
        payload["return_last_frame"] = True
    if args.draft:
        payload["draft"] = True
    apply_task_options(payload, args)

    # Base64 请求体总大小校验（官方上限 64MB）。
    body_bytes = len(json.dumps(payload).encode("utf-8"))
    if body_bytes > config["max_request_body_bytes"]:
        fail(
            "INVALID_INPUT",
            f"请求体 {body_bytes} 字节超过上限 {config['max_request_body_bytes']}（Base64 素材过大），"
            f"请改用公网 URL 传入大文件",
        )
    return {"payload": payload, "notes": notes}


def request_json(config: dict, url: str, method: str = "GET", payload: dict | None = None):
    """发起一次方舟请求；返回解析后的 JSON。

    POST 创建任务只在 429 与连接级网络错误时重试；5xx 立即报错，
    避免任务已创建却被重复提交。GET/DELETE 无副作用，可放宽重试。
    """
    headers = {"Authorization": f"Bearer {config['api_key']}"}
    data = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        data = json.dumps(payload).encode("utf-8")
    max_retries = config["max_retries"]

    for attempt in range(max_retries + 1):
        req = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=config["request_timeout_sec"]) as response:
                body = response.read().decode("utf-8")
            if not body.strip():
                return {}
            result = json.loads(body)
            if not isinstance(result, dict):
                fail("PROTOCOL_ERROR", f"响应不是 JSON 对象: {str(result)[:200]}")
            return result
        except urllib.error.HTTPError as exc:
            raw = exc.read().decode("utf-8", errors="replace") if exc.fp else ""
            retryable = exc.code == 429 or (method != "POST" and exc.code >= 500)
            if retryable and attempt < max_retries:
                retry_after = exc.headers.get("Retry-After", "") if exc.headers else ""
                try:
                    time.sleep(max(0.0, min(float(retry_after), 30.0)))
                except ValueError:
                    time.sleep(min(2**attempt, 8))
                continue
            detail = ""
            code = str(exc.code)
            try:
                error_json = json.loads(raw)
                if isinstance(error_json, dict):
                    inner = error_json.get("error") if "error" in error_json else error_json
                    if isinstance(inner, dict):
                        detail = str(inner.get("message", "")).strip()
                        code = str(inner.get("code", exc.code))
            except (TypeError, ValueError):
                pass
            fail(
                code if code else str(exc.code),
                detail or (raw[:300] or f"HTTP {exc.code}"),
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


def task_url(config: dict, task_id: str) -> str:
    return str(config["task_url_template"]).format(task_id=task_id)


def poll_task(config: dict, task_id: str) -> dict:
    """按配置间隔轮询，直到任务成功、失败或超出等待预算。"""
    deadline = time.monotonic() + config["poll_timeout_sec"]
    while True:
        result = request_json(config, task_url(config, task_id))
        status = str(result.get("status") or "")
        if not status:
            fail("POLL_FAILED", f"任务查询响应异常: {json.dumps(result, ensure_ascii=False)[:300]}")
        print(f"task status: {status}", file=sys.stderr)
        if status == TERMINAL_OK:
            return result
        if status in TERMINAL_FAIL:
            error = result.get("error") or {}
            fail(
                "TASK_" + status.upper(),
                str(error.get("message") or f"任务结束于 {status}"),
                task_id=task_id,
                task_status=status,
                error_code=error.get("code"),
            )
        if time.monotonic() >= deadline:
            fail(
                "POLL_TIMEOUT",
                f"等待 {config['poll_timeout_sec']} 秒后任务仍未完成（当前 {status}）。"
                f"任务仍在服务端运行，task_id={task_id}，可稍后用 check 子命令查询并下载。",
                task_id=task_id,
                task_status=status,
            )
        time.sleep(config["poll_interval_sec"])


def download_media(url: str, target: Path, timeout_sec: int, max_bytes: int, kind: str) -> Path:
    """流式下载视频/图片并按魔数确认内容。视频校验 ftyp 盒，图片校验 JPEG/PNG。"""
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
                        if kind == "video" and chunk[4:8] != b"ftyp":
                            output.close()
                            target.unlink(missing_ok=True)
                            fail("DOWNLOAD_FAILED", "响应不是有效的 MP4/MOV 视频文件")
                        if kind == "image" and not (
                            chunk.startswith(b"\x89PNG\r\n\x1a\n") or chunk.startswith(b"\xff\xd8\xff")
                        ):
                            output.close()
                            target.unlink(missing_ok=True)
                            fail("DOWNLOAD_FAILED", "响应不是有效的 JPEG/PNG 图片")
                        first_chunk = False
                    received += len(chunk)
                    if received > max_bytes:
                        output.close()
                        target.unlink(missing_ok=True)
                        fail("DOWNLOAD_FAILED", f"文件超过下载上限（>{max_bytes} 字节），已中止")
                    output.write(chunk)
            if received == 0:
                target.unlink(missing_ok=True)
                fail("DOWNLOAD_FAILED", "下载内容为空")
            return target
    except urllib.error.URLError as exc:
        fail("DOWNLOAD_FAILED", f"下载失败: {exc}")
    except OSError as exc:
        # URLError 是 OSError 子类，须在其后捕获；覆盖磁盘满、权限不足等写盘失败。
        fail("DOWNLOAD_FAILED", f"写入本地文件失败: {exc}")


def summarize_task(result: dict) -> dict:
    """抽取任务结果的关键字段。"""
    content = result.get("content") or {}
    return {
        "task_id": result.get("id"),
        "status": result.get("status"),
        "video_url": content.get("video_url"),
        "last_frame_url": content.get("last_frame_url"),
        "model": result.get("model"),
        "created_at": result.get("created_at"),
        "duration": result.get("duration"),
        "framespersecond": result.get("framespersecond"),
        "resolution": result.get("resolution"),
        "ratio": result.get("ratio"),
        "generate_audio": result.get("generate_audio"),
        "output_format": result.get("output_format"),
        "tools": result.get("tools"),
        "usage": result.get("usage"),
        "error": result.get("error"),
    }


def run_generate(args, config: dict):
    built = build_request(args, config)
    payload = built["payload"]
    for note in built["notes"]:
        print(f"note: {note}", file=sys.stderr)

    assets = sum(
        1 for item in payload.get("content", [])
        if item.get("type") not in ("text", "draft_task")
    )
    hint = f"resolution={payload.get('resolution')} ratio={payload.get('ratio')} duration={payload.get('duration')}"
    if assets:
        hint += f" assets={assets}"
    if payload.get("omni_reference_task_type"):
        hint += f" task_type={payload['omni_reference_task_type']}"
    print(f"creating video task ({hint})...", file=sys.stderr)

    created = request_json(config, config["submit_url"], method="POST", payload=payload)
    task_id = str(created.get("id") or "")
    if not task_id:
        fail("SUBMIT_FAILED", f"创建任务响应缺少任务 ID: {json.dumps(created, ensure_ascii=False)[:300]}")
    print(f"task submitted: {task_id}", file=sys.stderr)

    result = poll_task(config, task_id)
    summary = summarize_task(result)
    video_url = summary["video_url"]
    if not video_url:
        fail("MISSING_VIDEO_URL", f"任务成功但响应中没有 video_url（task_id={task_id}）", task_id=task_id)

    save_path = Path(args.save)
    ext = "." + str(payload.get("output_format") or "mp4")
    target = save_path if save_path.suffix.lower() == ext else save_path.with_suffix(ext)
    saved = download_media(video_url, target, config["download_timeout_sec"], config["max_download_bytes"], "video")
    summary["saved_file"] = str(saved)
    if summary["last_frame_url"]:
        frame_target = saved.parent / f"{saved.stem}_last_frame.jpg"
        summary["saved_last_frame"] = str(
            download_media(summary["last_frame_url"], frame_target, config["download_timeout_sec"], config["max_download_bytes"], "image")
        )

    output = {
        "ok": True,
        "note": "video_url 24 小时内有效且下载次数上限 100，请以 saved_file 为交付物；任务记录保留 7 天",
        "prompt": (args.prompt or "").strip() or None,
        "parameters_auto_locked": built["notes"],
        **summary,
        "error": None,
    }
    print(json.dumps(output, ensure_ascii=False))


def run_check(args, config: dict):
    result = request_json(config, task_url(config, args.task_id.strip()))
    summary = summarize_task(result)
    output = {"ok": True, **summary}
    if args.save:
        if summary["status"] != TERMINAL_OK or not summary["video_url"]:
            fail("NOT_READY", f"任务状态为 {summary['status']}，尚无可下载视频（task_id={args.task_id}）")
        save_path = Path(args.save)
        ext = "." + str(summary.get("output_format") or "mp4")
        target = save_path if save_path.suffix.lower() == ext else save_path.with_suffix(ext)
        output["saved_file"] = str(
            download_media(summary["video_url"], target, config["download_timeout_sec"], config["max_download_bytes"], "video")
        )
    print(json.dumps(output, ensure_ascii=False))


def run_cancel(args, config: dict):
    task_id = args.task_id.strip()
    request_json(config, task_url(config, task_id), method="DELETE")
    print(
        json.dumps(
            {
                "ok": True,
                "task_id": task_id,
                "note": "已请求取消（仅排队中任务可取消）或删除任务记录",
                "error": None,
            },
            ensure_ascii=False,
        )
    )


def main():
    args = parse_args()
    config = load_config(Path(args.config).expanduser())
    config["api_key"] = load_api_key(Path(args.key_config).expanduser())
    if args.command == "generate":
        run_generate(args, config)
    elif args.command == "check":
        run_check(args, config)
    else:
        run_cancel(args, config)


if __name__ == "__main__":
    main()
