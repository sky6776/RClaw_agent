#!/usr/bin/env python3
"""
HappyHorse-1.1-T2V 文生视频 CLI - 阿里云百炼 DashScope 异步视频生成客户端

流程：提交任务 -> 轮询任务状态 -> 下载结果视频到本地。
请求配置从本 skill 目录的 config.toml 读取；API Key 从系列共享的
skills/happyhorse_key.toml 读取。均不使用环境变量。

API 文档: https://help.aliyun.com/zh/model-studio/happyhorse-text-to-video-api-reference
"""

from __future__ import annotations

import argparse
import json
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

import tomllib

DEFAULT_CONFIG_PATH = Path(__file__).resolve().parents[1] / "config.toml"
DEFAULT_KEY_CONFIG_PATH = Path(__file__).resolve().parents[2] / "happyhorse_key.toml"

VALID_RESOLUTIONS = ("480P", "720P", "1080P")
VALID_RATIOS = ("16:9", "9:16", "1:1", "4:3", "3:4", "4:5", "5:4", "9:21", "21:9")
MIN_DURATION = 3
MAX_DURATION = 15
MAX_PROMPT_CHARS = 5000
DOWNLOAD_CHUNK_BYTES = 1024 * 1024


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
    """读取并校验 [happyhorse] 配置表，任何缺失或非法字段都在付费前拒绝。"""
    if not path.is_file():
        fail("CONFIG_ERROR", f"配置文件不存在: {path}")
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        fail("CONFIG_ERROR", f"无法读取配置文件 {path}: {exc}")
    config = data.get("happyhorse")
    if not isinstance(config, dict):
        fail("CONFIG_ERROR", f"配置文件缺少 [happyhorse] 表: {path}")

    def text(key: str) -> str:
        value = str(config.get(key) or "").strip()
        if not value:
            fail("CONFIG_ERROR", f"[happyhorse].{key} 不能为空（配置文件: {path}）")
        return value

    if config.get("api_key"):
        # API Key 已集中管理：t2v/i2v/r2v 共用 skills/happyhorse_key.toml。
        fail(
            "CONFIG_ERROR",
            f"API Key 已集中到 {DEFAULT_KEY_CONFIG_PATH.name}，"
            f"请从本文件删除 [happyhorse].api_key（配置文件: {path}）",
        )
    for key in ("model", "submit_url"):
        text(key)
    template = text("task_url_template")
    if "{task_id}" not in template:
        fail("CONFIG_ERROR", f"[happyhorse].task_url_template 必须包含 {{task_id}}（配置文件: {path}）")
    for key in ("submit_url", "task_url_template"):
        value = str(config[key])
        if not value.startswith(("https://", "http://")) or any(
            char.isspace() for char in value
        ):
            fail("CONFIG_ERROR", f"[happyhorse].{key} 不是有效 HTTP(S) URL（配置文件: {path}）")

    if str(config.get("resolution")) not in VALID_RESOLUTIONS:
        fail(
            "CONFIG_ERROR",
            f"[happyhorse].resolution 必须是 {'/'.join(VALID_RESOLUTIONS)}（配置文件: {path}）",
        )
    if str(config.get("ratio")) not in VALID_RATIOS:
        fail(
            "CONFIG_ERROR",
            f"[happyhorse].ratio 必须是 {'/'.join(VALID_RATIOS)}（配置文件: {path}）",
        )
    if (
        isinstance(config.get("duration"), bool)
        or not isinstance(config.get("duration"), int)
        or not MIN_DURATION <= config["duration"] <= MAX_DURATION
    ):
        fail(
            "CONFIG_ERROR",
            f"[happyhorse].duration 必须是 {MIN_DURATION}-{MAX_DURATION} 的整数（配置文件: {path}）",
        )
    if not isinstance(config.get("watermark"), bool):
        fail("CONFIG_ERROR", f"[happyhorse].watermark 必须为 true 或 false（配置文件: {path}）")
    for key in (
        "request_timeout_sec",
        "download_timeout_sec",
        "max_download_bytes",
        "poll_timeout_sec",
    ):
        value = config.get(key)
        if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
            fail("CONFIG_ERROR", f"[happyhorse].{key} 必须是正整数（配置文件: {path}）")
    if (
        isinstance(config.get("poll_interval_sec"), bool)
        or not isinstance(config.get("poll_interval_sec"), int)
        or config["poll_interval_sec"] < 1
    ):
        fail("CONFIG_ERROR", f"[happyhorse].poll_interval_sec 必须是至少 1 的整数（配置文件: {path}）")
    retries = config.get("max_retries")
    if isinstance(retries, bool) or not isinstance(retries, int) or retries < 0:
        fail("CONFIG_ERROR", f"[happyhorse].max_retries 必须是非负整数（配置文件: {path}）")
    return config


def load_api_key(path: Path) -> str:
    """从系列共享的 happyhorse_key.toml 读取 api_key（t2v/i2v/r2v 共用）。"""
    if not path.is_file():
        fail("CONFIG_ERROR", f"Key 配置文件不存在: {path}")
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        fail("CONFIG_ERROR", f"无法读取 Key 配置文件 {path}: {exc}")
    table = data.get("happyhorse")
    if not isinstance(table, dict):
        fail("CONFIG_ERROR", f"Key 配置文件缺少 [happyhorse] 表: {path}")
    api_key = str(table.get("api_key") or "").strip()
    if not api_key:
        fail("CONFIG_ERROR", f"未配置 API Key，请填写 {path} 的 [happyhorse].api_key")
    return api_key


def parse_args():
    parser = argparse.ArgumentParser(
        description="Generate videos from text prompts using HappyHorse-1.1-T2V",
    )
    parser.add_argument(
        "-p", "--prompt", type=str, required=True, help="Text description of the desired video",
    )
    parser.add_argument(
        "--save", type=str, required=True,
        help="Local output path (extension is normalized to .mp4)",
    )
    parser.add_argument(
        "--resolution", type=str, default=None, choices=VALID_RESOLUTIONS,
        help="Resolution tier; omit to use config default",
    )
    parser.add_argument(
        "--ratio", type=str, default=None, choices=VALID_RATIOS,
        help="Aspect ratio; omit to use config default",
    )
    parser.add_argument(
        "--duration", type=int, default=None,
        help=f"Video duration in seconds ({MIN_DURATION}-{MAX_DURATION}); omit for config default",
    )
    parser.add_argument(
        "--watermark", action="store_true",
        help='Add the "Happy Horse" watermark; omit for config default',
    )
    parser.add_argument(
        "--seed", type=int, default=None,
        help="Random seed (0-2147483647); omit for server-side randomness",
    )
    parser.add_argument(
        "--config", type=str, default=str(DEFAULT_CONFIG_PATH),
        help="Path to this skill's config.toml",
    )
    parser.add_argument(
        "--key-config", type=str, default=str(DEFAULT_KEY_CONFIG_PATH),
        help="Shared happyhorse_key.toml holding the API key (t2v/i2v/r2v)",
    )
    return parser.parse_args()


def build_parameters(config: dict, args) -> dict:
    """命令行参数优先，未提供时回退到配置默认值。"""
    parameters = {
        "resolution": args.resolution or config["resolution"],
        "ratio": args.ratio or config["ratio"],
        "duration": args.duration if args.duration is not None else config["duration"],
        "watermark": args.watermark or config["watermark"],
    }
    if args.seed is not None:
        parameters["seed"] = args.seed
    return parameters


def request_json(
    url: str,
    api_key: str,
    timeout_sec: int,
    max_retries: int,
    payload: dict | None = None,
) -> dict:
    """发起一次 DashScope 请求；返回解析后的 JSON。

    payload 非 None 表示付费任务的 POST 提交：只在 429 与连接级网络错误
    （请求确定未达服务端）时重试；5xx 一律立即报错——网关 5xx 时任务可能
    已创建，重试会重复建任务、重复计费。GET 轮询无副作用，429/5xx 都可重试。
    """
    headers = {"Authorization": f"Bearer {api_key}", "Content-Type": "application/json"}
    data = None
    if payload is not None:
        # ensure_ascii=True 让请求体保持纯 ASCII，规避终端编码差异。
        data = json.dumps(payload).encode("utf-8")
        headers["X-DashScope-Async"] = "enable"
    req = urllib.request.Request(url, data=data, headers=headers)

    for attempt in range(max_retries + 1):
        try:
            with urllib.request.urlopen(req, timeout=timeout_sec) as response:
                result = json.loads(response.read().decode("utf-8"))
            if not isinstance(result, dict):
                fail("PROTOCOL_ERROR", f"响应不是 JSON 对象: {str(result)[:200]}")
            return result
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", errors="replace") if exc.fp else ""
            retryable = exc.code == 429 or (payload is None and exc.code >= 500)
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


def submit_task(config: dict, prompt: str, parameters: dict) -> str:
    result = request_json(
        config["submit_url"],
        config["api_key"],
        config["request_timeout_sec"],
        config["max_retries"],
        payload={"model": config["model"], "input": {"prompt": prompt}, "parameters": parameters},
    )
    output = result.get("output")
    if not isinstance(output, dict) or not output.get("task_id"):
        fail(
            "SUBMIT_FAILED",
            f"任务提交响应缺少 task_id: {json.dumps(result, ensure_ascii=False)[:300]}",
        )
    print(f"task submitted: {output['task_id']} ({output.get('task_status')})", file=sys.stderr)
    return str(output["task_id"])


def poll_task(config: dict, task_id: str) -> dict:
    """按配置间隔轮询，直到任务成功、失败或超出等待预算。"""
    url = config["task_url_template"].format(task_id=task_id)
    deadline = time.monotonic() + config["poll_timeout_sec"]
    while True:
        result = request_json(
            url,
            config["api_key"],
            config["request_timeout_sec"],
            config["max_retries"],
        )
        output = result.get("output")
        if not isinstance(output, dict) or not output.get("task_status"):
            fail(
                "POLL_FAILED",
                f"任务查询响应异常: {json.dumps(result, ensure_ascii=False)[:300]}",
            )
        status = str(output["task_status"])
        print(f"task status: {status}", file=sys.stderr)
        if status == "SUCCEEDED":
            return output
        if status in ("FAILED", "CANCELED", "UNKNOWN"):
            fail(
                "TASK_" + status,
                str(output.get("message") or output.get("code") or f"任务结束于 {status}"),
                task_id=task_id,
                task_status=status,
            )
        if time.monotonic() >= deadline:
            fail(
                "POLL_TIMEOUT",
                f"等待 {config['poll_timeout_sec']} 秒后任务仍未完成（当前 {status}）。"
                f"任务仍在服务端运行，task_id={task_id}，结果链接 24 小时内有效。",
                task_id=task_id,
                task_status=status,
            )
        time.sleep(config["poll_interval_sec"])


def download_video(url: str, save_path: Path, timeout_sec: int, max_bytes: int) -> Path:
    """流式下载 MP4 并按文件魔数确认内容；扩展名统一规范化为 .mp4。"""
    try:
        with urllib.request.urlopen(url, timeout=timeout_sec) as response:
            target = save_path if save_path.suffix.lower() == ".mp4" else save_path.with_suffix(".mp4")
            target.parent.mkdir(parents=True, exist_ok=True)
            received = 0
            first_chunk = True
            with target.open("wb") as output:
                while True:
                    chunk = response.read(DOWNLOAD_CHUNK_BYTES)
                    if not chunk:
                        break
                    if first_chunk:
                        # MP4 头部：偏移 4-8 字节为 "ftyp" 盒标识，防止把错误页写成视频。
                        if len(chunk) < 12 or chunk[4:8] != b"ftyp":
                            output.close()
                            target.unlink(missing_ok=True)
                            fail("DOWNLOAD_FAILED", "响应不是有效的 MP4 视频文件")
                        first_chunk = False
                    received += len(chunk)
                    if received > max_bytes:
                        output.close()
                        target.unlink(missing_ok=True)
                        fail(
                            "DOWNLOAD_FAILED",
                            f"视频超过下载上限（>{max_bytes} 字节），已中止",
                        )
                    output.write(chunk)
            if received == 0:
                target.unlink(missing_ok=True)
                fail("DOWNLOAD_FAILED", "下载内容为空")
            return target
    except urllib.error.URLError as exc:
        fail("DOWNLOAD_FAILED", f"下载视频失败: {exc}")
    except OSError as exc:
        # URLError 是 OSError 子类，须在其后捕获；这里覆盖磁盘满、权限不足等写盘失败。
        fail("DOWNLOAD_FAILED", f"写入本地文件失败: {exc}")


def main():
    args = parse_args()

    prompt = (args.prompt or "").strip()
    if not prompt:
        fail("INVALID_PROMPT", "Prompt must be non-blank.")
    if len(prompt) > MAX_PROMPT_CHARS:
        fail("INVALID_PROMPT", f"Prompt must be no longer than {MAX_PROMPT_CHARS} characters.")

    config = load_config(Path(args.config).expanduser())
    config["api_key"] = load_api_key(Path(args.key_config).expanduser())
    parameters = build_parameters(config, args)
    usage_hint = (
        f"resolution={parameters['resolution']} ratio={parameters['ratio']} "
        f"duration={parameters['duration']}s"
    )
    print(f"generating video ({usage_hint})...", file=sys.stderr)

    task_id = submit_task(config, prompt, parameters)
    output = poll_task(config, task_id)
    video_url = str(output.get("video_url") or "")
    if not video_url:
        fail("MISSING_VIDEO_URL", "任务成功但响应中没有 video_url", task_id=task_id)

    saved = download_video(
        video_url,
        Path(args.save),
        config["download_timeout_sec"],
        config["max_download_bytes"],
    )
    print(
        json.dumps(
            {
                "ok": True,
                "model": config["model"],
                "task_id": task_id,
                "video_url": video_url,
                "prompt": prompt,
                "parameters": parameters,
                "usage": output.get("usage"),
                "saved_file": str(saved),
                "note": "video_url 24 小时内有效，请以 saved_file 为交付物",
                "error": None,
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
