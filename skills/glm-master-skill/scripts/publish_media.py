#!/usr/bin/env python3
"""Stage one RClaw artifact and print a source-channel media marker."""

import argparse
import hashlib
import os
import re
import secrets
import shutil
import sys
import tempfile
from pathlib import Path


UNSAFE = re.compile(r'[\x00-\x1f\x7f<>:"/\\|?*\]\s]+')


def configure_stdout():
    """Emit markers as UTF-8 so RClaw can decode non-ASCII Windows paths."""
    if hasattr(sys.stdout, "reconfigure"):
        sys.stdout.reconfigure(encoding="utf-8")


def safe_name(name: str) -> str:
    cleaned = UNSAFE.sub("_", Path(name).name).strip("._ ")
    return cleaned or "artifact"


def file_sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def channel_enabled(source_channel: str) -> str:
    channel = (source_channel or "").strip().lower()
    if channel == "napcatqq":
        channel = "napcat"
    # channel 同时用于 marker 前缀和能力变量名；限制为有界 ASCII token，
    # 避免来源文本改变 marker 结构或构造意外的环境变量名。
    if len(channel) > 64 or not re.fullmatch(r"[a-z][a-z0-9_-]*", channel):
        return ""
    if channel in {"cli", "system"}:
        return ""

    # 真实逐轮来源只决定 marker 前缀；动态 enabled 变量表示该 channel 确实实现了
    # 媒体发送。未来 channel 仍复用同一沙箱，只需声明能力，不需在本脚本硬编码分支。
    flag = f"RCLAW_{channel.upper().replace('-', '_')}_ENABLED"
    return channel if os.environ.get(flag) == "1" else ""


def infer_type(path: Path) -> str:
    suffix = path.suffix.lower()
    if suffix in {".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"}:
        return "image"
    if suffix in {".mp4", ".mov", ".mkv", ".webm", ".avi"}:
        return "video"
    if suffix in {".mp3", ".wav", ".ogg", ".m4a", ".flac", ".aac", ".amr", ".opus", ".silk"}:
        return "voice"
    return "file"


def stage_file(source: Path, media_root: str, recipient_name: str) -> Path:
    """原子复制到内容寻址目录，避免并发发布覆盖旧 marker 指向的文件。

    返回给 marker 的路径保留 RCLAW_MEDIA_DIR 的词法前缀：Android 上
    /data/user/0 是 /data/data 的符号链接，resolve() 后前缀改变，运行时的
    词法沙箱校验会拒绝整条 marker（真实物理位置由运行时 canonicalize 兜底）。
    """
    output_dir = Path(media_root) / "out"
    resolved_dir = output_dir.resolve()
    resolved_dir.mkdir(parents=True, exist_ok=True)
    fd, temporary_name = tempfile.mkstemp(prefix=".publish-", dir=resolved_dir)
    os.close(fd)
    temporary = Path(temporary_name)
    try:
        shutil.copy2(source, temporary)
        expected_digest = file_sha256(temporary)
        staged_dir = resolved_dir / expected_digest
        staged_dir.mkdir(parents=True, exist_ok=True)
        staged = staged_dir / recipient_name
        try:
            os.replace(temporary, staged)
        except OSError:
            # Windows 可能因 channel 正在读同一完整文件而不允许替换；只有现有字节
            # 与内容目录一致时才可安全复用，否则绝不能让 marker 指向损坏内容。
            if not staged.is_file() or file_sha256(staged) != expected_digest:
                raise
            temporary.unlink()
        return output_dir / expected_digest / recipient_name
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


def main():
    configure_stdout()
    parser = argparse.ArgumentParser(
        description="Copy an artifact into RClaw media sandbox and emit MEDIA_MARKER"
    )
    parser.add_argument("path", help="Artifact file")
    parser.add_argument(
        "--source-channel",
        default="",
        help="Source channel for direct CLI use; RCLAW_SOURCE_CHANNEL takes precedence",
    )
    parser.add_argument(
        "--type",
        choices=["auto", "file", "image", "video", "voice"],
        default="auto",
    )
    parser.add_argument("--name", default="", help="Recipient-visible filename")
    parser.add_argument("--caption", default="RClaw 生成结果")
    args = parser.parse_args()

    source = Path(args.path).resolve()
    if not source.is_file():
        parser.error(f"Artifact not found: {source}")
    # 所有外部 channel 共用 RCLAW_MEDIA_DIR，目录本身不能说明回执归属。
    # RClaw 的逐轮环境值比模型可能手填的参数可靠；参数仅供脱离 RClaw 直跑时回退。
    source_channel = os.environ.get("RCLAW_SOURCE_CHANNEL", "").strip()
    channel = channel_enabled(source_channel or args.source_channel)
    if not channel:
        print(f"ARTIFACT={source}")
        return

    media_root = os.environ.get("RCLAW_MEDIA_DIR", "").strip()
    if not media_root:
        print(f"ARTIFACT={source}")
        return

    recipient_name = safe_name(args.name or source.name)
    staged = stage_file(source, media_root, recipient_name)

    marker_type = infer_type(source) if args.type == "auto" else args.type
    marker_path = str(staged).replace("\\", "/")
    caption = safe_name(args.caption)
    # delivery 区分“重新执行发布器”与 AgentStatus/AgentWait 对旧终态的文本重放。
    # channel 会忽略未知属性，运行时则可在用户撤销旧意图后只接纳新的真实发布。
    delivery = secrets.token_hex(16)
    if channel == "napcat":
        marker = (
            f'[:napcat-media type={marker_type} path="{marker_path}" '
            f'name="{recipient_name}" caption="{caption}" delivery="{delivery}"]'
        )
    elif channel == "weixin":
        marker = (
            f'[:weixin-media type={marker_type} path="{marker_path}" '
            f'caption="{caption}" delivery="{delivery}"]'
        )
    else:
        marker = (
            f'[:{channel}-media type={marker_type} path="{marker_path}" '
            f'name="{recipient_name}" caption="{caption}" delivery="{delivery}"]'
        )
    print(f"MEDIA_MARKER={marker}")


if __name__ == "__main__":
    main()
