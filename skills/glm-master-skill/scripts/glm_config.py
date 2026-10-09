#!/usr/bin/env python3
"""Load shared GLM API settings from TOML without environment fallbacks."""

from __future__ import annotations

import tomllib
from pathlib import Path
from typing import Any
from urllib.parse import urlparse


DEFAULT_CONFIG_PATH = Path(__file__).resolve().parent.parent / "config.toml"


class ConfigError(ValueError):
    """Raised when the shared GLM configuration is missing or invalid."""


def _validate_http_url(value: Any, key: str, path: Path) -> str:
    """校验配置中的接口地址，避免把格式错误或含凭据的地址留到请求阶段。"""
    text = str(value or "").strip()
    try:
        parsed = urlparse(text)
        parsed.port
    except ValueError as exc:
        raise ConfigError(f"{key} 不是有效 HTTP(S) URL: {path}") from exc
    if (
        not text
        or parsed.scheme not in ("http", "https")
        or not parsed.hostname
        or parsed.username is not None
        or parsed.password is not None
        or "\\" in text
        or any(char.isspace() or ord(char) < 0x20 for char in text)
    ):
        raise ConfigError(f"{key} 不是有效 HTTP(S) URL: {path}")
    return text


def _read_toml(path: Path) -> dict[str, Any]:
    if not path.is_file():
        raise ConfigError(f"GLM 配置文件不存在: {path}")
    try:
        data = tomllib.loads(path.read_text(encoding="utf-8-sig"))
    except (OSError, UnicodeError, tomllib.TOMLDecodeError) as exc:
        raise ConfigError(f"无法读取 GLM 配置文件 {path}: {exc}") from exc
    if not isinstance(data, dict):
        raise ConfigError(f"GLM 配置文件必须包含 TOML 表: {path}")
    return data


def load_config(
    *sections: str,
    config_path: str | Path | None = None,
    require_api_key: bool = True,
) -> dict[str, Any]:
    """Merge ``[zhipu]`` with task sections from the shared TOML file.

    Later task sections override earlier ones. No environment variable is read.
    """

    path = Path(config_path or DEFAULT_CONFIG_PATH).expanduser().resolve()
    data = _read_toml(path)
    common = data.get("zhipu")
    if not isinstance(common, dict):
        raise ConfigError(f"配置文件缺少 [zhipu] 表: {path}")

    merged: dict[str, Any] = dict(common)
    for section in sections:
        values = data.get(section)
        if not isinstance(values, dict):
            raise ConfigError(f"配置文件缺少 [{section}] 表: {path}")
        merged.update(values)

    api_key = str(merged.get("api_key") or "").strip()
    if require_api_key and not api_key:
        raise ConfigError(f"未配置 API Key，请填写 {path} 的 [zhipu].api_key")
    merged["api_key"] = api_key
    merged["_api_key"] = api_key
    merged["_config_path"] = str(path)

    if str(merged.get("auth") or "").strip().lower() != "bearer":
        raise ConfigError(f"[zhipu].auth 必须为 bearer: {path}")

    if "glm5v" in sections:
        _validate_http_url(merged.get("base_url"), "[zhipu].base_url", path)
        if not str(merged.get("model") or "").strip():
            raise ConfigError(f"[glm5v].model 不能为空: {path}")
        if str(merged.get("thinking") or "").strip().lower() != "enabled":
            raise ConfigError(f"[glm5v].thinking 必须为 enabled: {path}")
        if merged.get("stream") is not True:
            raise ConfigError(f"[glm5v].stream 必须为 true: {path}")
        if not str(merged.get("max_tokens_key") or "").strip():
            raise ConfigError(f"[glm5v].max_tokens_key 不能为空: {path}")
        # 中文注释：所有视觉封装共享的数值字段在统一入口先完成类型与范围校验。
        for key in ("connect_timeout_sec", "read_timeout_sec", "max_tokens", "image_limit"):
            positive_int(merged, key)
        non_negative_int(merged, "max_retries")
    if "glm_ocr" in sections:
        _validate_http_url(merged.get("api_url"), "[glm_ocr].api_url", path)
        if not str(merged.get("model") or "").strip():
            raise ConfigError(f"[glm_ocr].model 不能为空: {path}")
        positive_int(merged, "read_timeout_sec")
        non_negative_int(merged, "max_retries")
    if "glm_image" in sections:
        _validate_http_url(merged.get("api_url"), "[glm_image].api_url", path)
        for key in ("model", "size", "quality"):
            if not str(merged.get(key) or "").strip():
                raise ConfigError(f"[glm_image].{key} 不能为空: {path}")
        for key in ("request_timeout_sec", "download_timeout_sec", "max_download_bytes"):
            positive_int(merged, key)
        non_negative_int(merged, "max_retries")
        if not isinstance(merged.get("watermark_enabled"), bool):
            raise ConfigError(f"[glm_image].watermark_enabled 必须为布尔值: {path}")
    return merged


def positive_int(config: dict[str, Any], key: str) -> int:
    value = config.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ConfigError(f"{key} 必须是正整数（配置文件: {config.get('_config_path')}）")
    return value


def non_negative_int(config: dict[str, Any], key: str) -> int:
    value = config.get(key)
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ConfigError(f"{key} 必须是非负整数（配置文件: {config.get('_config_path')}）")
    return value


def api_url(config: dict[str, Any], suffix: str) -> str:
    base = str(config.get("base_url") or "").strip().rstrip("/")
    if not base:
        raise ConfigError(f"base_url 不能为空（配置文件: {config.get('_config_path')}）")
    suffix = suffix.strip("/")
    return base if base.endswith(f"/{suffix}") else f"{base}/{suffix}"
