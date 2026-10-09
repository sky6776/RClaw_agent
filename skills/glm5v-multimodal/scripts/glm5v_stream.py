"""Shared GLM-5V-Turbo streaming client for RClaw GLM-V skills.

Adapted from the retry and SSE handling in
skills/glmv-stock-analyst/scripts/glm5v_report.py.
"""

import json
import time

import requests


RETRYABLE_STATUS_CODES = {408, 429, 500, 502, 503, 504}
RETRYABLE_EXCEPTIONS = (
    requests.exceptions.Timeout,
    requests.exceptions.ConnectionError,
    requests.exceptions.ChunkedEncodingError,
    requests.exceptions.ContentDecodingError,
)


class GLM5VStreamError(RuntimeError):
    """GLM-5V request or response error with an optional HTTP status."""

    def __init__(self, message, status_code=None):
        super().__init__(message)
        self.status_code = status_code


class StreamInterruptedError(GLM5VStreamError):
    """The SSE response ended without a complete assistant choice."""


def _response_detail(response):
    response.encoding = "utf-8"
    try:
        return json.dumps(response.json(), ensure_ascii=False)
    except ValueError:
        return response.text


def _retry_delay(response, attempt):
    raw = response.headers.get("Retry-After", "")
    try:
        return max(0.0, min(float(raw), 30.0))
    except (TypeError, ValueError):
        return min(2**attempt, 8)


def _parse_stream_response(response):
    response.encoding = "utf-8"
    content_parts = []
    finish_reason = ""
    usage = {}
    saw_choice = False
    saw_done = False

    for raw_line in response.iter_lines(decode_unicode=True):
        if isinstance(raw_line, bytes):
            try:
                raw_line = raw_line.decode("utf-8")
            except UnicodeDecodeError as exc:
                raise StreamInterruptedError("SSE stream contains invalid UTF-8") from exc

        line = (raw_line or "").strip()
        if not line or line.startswith(":"):
            continue
        if line == "data: [DONE]":
            saw_done = True
            break
        if not line.startswith("data:"):
            continue

        raw_data = line[5:].strip()
        if not raw_data:
            continue
        try:
            event = json.loads(raw_data)
        except json.JSONDecodeError as exc:
            raise StreamInterruptedError("SSE stream contains invalid JSON") from exc

        if event.get("error"):
            raise GLM5VStreamError(
                f"GLM-5V-Turbo stream error: "
                f"{json.dumps(event['error'], ensure_ascii=False)}"
            )
        if isinstance(event.get("usage"), dict):
            usage = event["usage"]

        choices = event.get("choices") or []
        if not choices:
            continue
        saw_choice = True
        choice = choices[0] or {}
        delta = choice.get("delta") or {}
        piece = delta.get("content")
        if isinstance(piece, str):
            content_parts.append(piece)
        # Deliberately ignore delta.reasoning_content.
        if choice.get("finish_reason"):
            finish_reason = str(choice["finish_reason"])

    if not saw_choice:
        raise StreamInterruptedError("GLM-5V-Turbo stream returned no assistant choice")
    if not finish_reason:
        ending = (
            "received [DONE] without finish_reason"
            if saw_done
            else "ended before [DONE] or finish_reason"
        )
        raise StreamInterruptedError(f"GLM-5V-Turbo stream {ending}")

    return {
        "choices": [
            {
                "message": {
                    "role": "assistant",
                    "content": "".join(content_parts),
                },
                "finish_reason": finish_reason,
            }
        ],
        "usage": usage,
    }


def post_stream_json(
    api_url,
    headers,
    payload,
    connect_timeout_sec,
    read_timeout_sec,
    max_retries,
    thinking_type,
    stream,
):
    """POST a mandatory thinking-enabled streaming request and aggregate its text."""

    body = dict(payload)
    thinking_type = str(thinking_type).strip().lower()
    if thinking_type != "enabled" or stream is not True:
        raise GLM5VStreamError("GLM-5V-Turbo requires thinking=enabled and stream=true")
    body["thinking"] = {"type": thinking_type}
    body["stream"] = stream
    connect_timeout_sec = int(connect_timeout_sec)
    read_timeout_sec = int(read_timeout_sec)
    max_retries = int(max_retries)

    for attempt in range(max_retries + 1):
        try:
            with requests.post(
                api_url,
                headers=headers,
                json=body,
                timeout=(connect_timeout_sec, read_timeout_sec),
                stream=True,
            ) as response:
                if response.status_code in RETRYABLE_STATUS_CODES:
                    if attempt == max_retries:
                        raise GLM5VStreamError(
                            f"GLM-5V-Turbo failed after {attempt + 1} attempts: "
                            f"HTTP {response.status_code} {_response_detail(response)}",
                            response.status_code,
                        )
                    time.sleep(_retry_delay(response, attempt))
                    continue
                if not response.ok:
                    raise GLM5VStreamError(
                        f"GLM-5V-Turbo request failed: HTTP {response.status_code} "
                        f"{_response_detail(response)}",
                        response.status_code,
                    )
                try:
                    return _parse_stream_response(response)
                except StreamInterruptedError:
                    if attempt == max_retries:
                        raise
                    continue
        except RETRYABLE_EXCEPTIONS as exc:
            if attempt == max_retries:
                raise GLM5VStreamError(
                    f"GLM-5V-Turbo connection/read failed after {attempt + 1} attempts"
                ) from exc
            time.sleep(min(2**attempt, 8))

    raise GLM5VStreamError("GLM-5V-Turbo request did not return a result")
