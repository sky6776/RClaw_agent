#!/usr/bin/env python3
"""
GLM-OCR CLI

Command-line interface for GLM-OCR vision model.

Usage:
    python skills/glmocr-formula/scripts/glm_ocr_cli.py --file-url "URL"
    python skills/glmocr-formula/scripts/glm_ocr_cli.py --file-url "URL" --pretty
"""

import argparse
import base64
import json
import logging
import mimetypes
import sys
import time
from pathlib import Path
from typing import Any
from urllib.parse import urlparse

try:
    import requests  # noqa: E402
except ImportError:
    print(
        "Error: 'requests' is required. Please install it:\n"
        "  pip install requests\n"
        "Or install all dependencies: pip install -r requirements.txt",
        file=sys.stderr,
    )
    sys.exit(1)

MASTER_SCRIPTS = Path(__file__).resolve().parents[2] / "glm-master-skill" / "scripts"
if str(MASTER_SCRIPTS) not in sys.path:
    sys.path.insert(0, str(MASTER_SCRIPTS))

from glm_config import DEFAULT_CONFIG_PATH, load_config, non_negative_int, positive_int

# Fix Windows console encoding
if sys.platform == "win32" and hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8", errors="replace")
    sys.stderr.reconfigure(encoding="utf-8", errors="replace")

logger = logging.getLogger(__name__)

# =============================================================================
# Constants
# =============================================================================

SUPPORTED_LOCAL_SUFFIXES = {".pdf", ".png", ".jpg", ".jpeg"}
MAX_IMAGE_BYTES = 10 * 1024 * 1024
MAX_PDF_BYTES = 50 * 1024 * 1024

# =============================================================================
# Configuration
# =============================================================================


def get_config(config_path: str | None = None) -> dict[str, Any]:
    """Load GLM-OCR settings from the shared TOML file."""
    return load_config("glm_ocr", config_path=config_path)


# =============================================================================
# File Utilities
# =============================================================================


def _is_url(path: str) -> bool:
    """Check if the given path is a URL."""
    try:
        value = path.strip()
        result = urlparse(value)
        host = result.hostname
        result.port
        # 中文注释：拒绝凭据、空白和反斜杠，避免把歧义地址交给远端抓取器。
        return (
            bool(value)
            and not any(char.isspace() or ord(char) < 0x20 for char in value)
            and "\\" not in value
            and result.scheme in ("http", "https")
            and bool(host)
            and result.username is None
            and result.password is None
        )
    except (AttributeError, ValueError):
        return False


def _file_signature_matches(path: Path, suffix: str) -> bool:
    """按文件头确认本地 OCR 输入确实属于声明的格式。"""
    try:
        # 只读取判型所需的文件头，避免为校验大文件而整体载入内存。
        with path.open("rb") as handle:
            header = handle.read(8)
    except OSError:
        return False
    # 中文注释：扩展名只用于路由，真正上传前必须再核对内容签名。
    if suffix == ".pdf":
        return header.startswith(b"%PDF-")
    if suffix == ".png":
        return header.startswith(b"\x89PNG\r\n\x1a\n")
    return header.startswith(b"\xff\xd8\xff")


def _encode_file(file_path: str) -> tuple[str, str]:
    """
    Encode a local file to base64 data URI.

    Returns:
        tuple of (data_uri, error_message)
        On success, error_message is empty.
    """
    path = Path(file_path)
    if not path.exists():
        return "", f"File not found: {file_path}"
    if not path.is_file():
        return "", f"Not a regular file: {file_path}"
    suffix = path.suffix.lower()
    if suffix not in SUPPORTED_LOCAL_SUFFIXES:
        return "", f"Unsupported local OCR format: {suffix or '(none)'}"
    limit = MAX_PDF_BYTES if suffix == ".pdf" else MAX_IMAGE_BYTES
    if path.stat().st_size > limit:
        return "", f"Local OCR input exceeds {limit // (1024 * 1024)} MB"
    if not _file_signature_matches(path, suffix):
        return "", f"Local OCR input content does not match its extension: {file_path}"

    # Guess MIME type, fallback to application/octet-stream
    mime, _ = mimetypes.guess_type(str(path))
    if not mime:
        mime = "application/octet-stream"

    try:
        data = path.read_bytes()
        b64 = base64.b64encode(data).decode("ascii")
        return f"data:{mime};base64,{b64}", ""
    except Exception as e:
        return "", f"Failed to read file: {e}"


# =============================================================================
# API Request
# =============================================================================


def _make_api_request(config: dict[str, Any], payload: dict) -> dict:
    """
    Make GLM-OCR API request.

    Args:
        config: Shared GLM-OCR request configuration
        payload: Request payload

    Returns:
        API response dict

    Raises:
        RuntimeError: On API errors
    """
    api_url = str(config["api_url"])
    headers = {
        "Authorization": f"Bearer {config['api_key']}",
        "Content-Type": "application/json",
    }
    timeout = positive_int(config, "read_timeout_sec")
    max_retries = non_negative_int(config, "max_retries")

    resp = None
    for attempt in range(max_retries + 1):
        try:
            resp = requests.post(api_url, json=payload, headers=headers, timeout=timeout)
        except requests.RequestException as e:
            if attempt == max_retries:
                raise RuntimeError(
                    f"API request failed after {attempt + 1} attempts: {e}"
                ) from e
            time.sleep(min(2**attempt, 8))
            continue
        if resp.status_code == 429 or resp.status_code >= 500:
            if attempt < max_retries:
                retry_after = resp.headers.get("Retry-After", "")
                try:
                    delay = max(0.0, min(float(retry_after), 30.0))
                except ValueError:
                    delay = min(2**attempt, 8)
                resp.close()
                time.sleep(delay)
                continue
        break

    if resp is None:
        raise RuntimeError("API request returned no response")

    # Handle HTTP errors
    if resp.status_code != 200:
        error_detail = ""
        try:
            error_body = resp.json()
            if isinstance(error_body, dict):
                error_detail = str(
                    error_body.get("error", {}).get("message", "")
                ).strip()
        except Exception:
            pass

        if not error_detail:
            error_detail = (resp.text[:200] or "No response body").strip()

        status_code = resp.status_code
        resp.close()
        if status_code == 401 or status_code == 403:
            raise RuntimeError(
                f"Authentication failed ({status_code}): {error_detail}"
            )
        elif status_code == 429:
            raise RuntimeError(f"API rate limit exceeded (429): {error_detail}")
        elif status_code >= 500:
            raise RuntimeError(
                f"API service error ({status_code}): {error_detail}"
            )
        else:
            raise RuntimeError(f"API error ({status_code}): {error_detail}")

    # Parse response
    try:
        result = resp.json()
    except Exception as exc:
        detail = resp.text[:200]
        resp.close()
        raise RuntimeError(f"Invalid JSON response: {detail}") from exc
    resp.close()

    # Check API-level error
    if "error" in result:
        raise RuntimeError(f"API error: {result['error']}")

    return result


# =============================================================================
# Main API
# =============================================================================


def extract_text(
    image_source: str,
    include_raw_result: bool = False,
    config_path: str | None = None,
    **options,
) -> dict[str, Any]:
    """
    Extract text from image or PDF using GLM-OCR layout parsing API.

    Args:
        image_source: URL or local file path to the image/PDF
        config_path: Shared TOML config path
        **options: Additional API options:
            - return_crop_images: bool - Whether to return cropped images (default: False)
            - need_layout_visualization: bool - Whether to need layout visualization
            - start_page_id: int - Start page ID for PDF
            - end_page_id: int - End page ID for PDF

    Returns:
        {
            "ok": True,
            "text": "OCR extracted text (md_results)...",
            "layout_details": [...],  # Layout analysis results
            "result": { raw API result } or null,
            "error": None,
            "source": "original source",
            "source_type": "url" or "file"
        }
        or on error:
        {
            "ok": False,
            "text": "",
            "layout_details": None,
            "result": None,
            "error": {"code": "...", "message": "..."},
            "source": "original source",
            "source_type": "url" or "file"
        }
    """
    # Get config
    try:
        config = get_config(config_path)
    except ValueError as e:
        return _error("CONFIG_ERROR", str(e), image_source)

    # Determine if URL or local file
    is_url = _is_url(image_source)
    if is_url:
        file_payload = image_source
    else:
        file_payload, encode_err = _encode_file(image_source)
        if encode_err:
            return _error("INPUT_ERROR", encode_err, image_source)

    # Build request payload
    payload = {
        "model": str(config["model"]),
        "file": file_payload,
        "return_crop_images": options.get("return_crop_images", False),
        "need_layout_visualization": options.get("need_layout_visualization", False),
    }
    start_page_id = options.get("start_page_id")
    end_page_id = options.get("end_page_id")
    if start_page_id is not None:
        payload["start_page_id"] = start_page_id
    if end_page_id is not None:
        payload["end_page_id"] = end_page_id

    # Call API
    try:
        result = _make_api_request(config, payload)
    except (KeyError, TypeError, ValueError, RuntimeError) as e:
        # 中文注释：配置字段错误与远端请求错误统一返回稳定的 API_ERROR 信封。
        return _error("API_ERROR", str(e), image_source)

    # Extract text from response
    try:
        extracted_text = _extract_text(result)
    except ValueError as e:
        return _error("API_ERROR", str(e), image_source)

    # Extract layout details
    layout_details = result.get("layout_details")

    return {
        "ok": True,
        "text": extracted_text,
        "layout_details": layout_details,
        "result": result if include_raw_result else None,
        "error": None,
        "source": image_source,
        "source_type": "url" if is_url else "file",
        "raw_result_included": include_raw_result,
    }


def _extract_text(result) -> str:
    """Extract text from GLM-OCR layout parsing API response."""
    if not isinstance(result, dict):
        raise ValueError("Invalid response schema: response must be an object")

    # Format 1: Layout parsing API response (md_results field)
    if "md_results" in result:
        md_results = result["md_results"]
        if isinstance(md_results, str):
            return md_results

    # Format 2: Nested data field
    if "data" in result and isinstance(result["data"], dict):
        data = result["data"]
        if "md_results" in data and isinstance(data["md_results"], str):
            return data["md_results"]

    raise ValueError(
        f"Invalid response schema: unable to extract text from {list(result.keys())}"
    )


def _error(code: str, message: str, source: str) -> dict:
    """Create error response."""
    return {
        "ok": False,
        "text": "",
        "layout_details": None,
        "result": None,
        "error": {"code": code, "message": message},
        "source": source,
        "source_type": "url" if _is_url(source) else "file",
        "raw_result_included": False,
    }


# =============================================================================
# CLI Interface
# =============================================================================


def main():
    parser = argparse.ArgumentParser(
        description="GLM-OCR - Extract text from images and PDFs",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  # Extract from URL
  python skills/glmocr-formula/scripts/glm_ocr_cli.py --file-url "https://example.com/image.jpg"

  # Extract from local file
  python skills/glmocr-formula/scripts/glm_ocr_cli.py --file /path/to/image.jpg

  # Save result to file
  python skills/glmocr-formula/scripts/glm_ocr_cli.py --file photo.png --output result.json --pretty

Configuration:
  Edit skills/glm-master-skill/config.toml.
        """,
    )

    # Input (one of --file-url or --file required)
    input_group = parser.add_mutually_exclusive_group(required=True)
    input_group.add_argument("--file-url", help="URL to image/PDF")
    input_group.add_argument("--file", help="Path to local image/PDF file")

    # Output options
    parser.add_argument(
        "--pretty", action="store_true", help="Pretty-print JSON output"
    )
    parser.add_argument(
        "--output", "-o", metavar="FILE", help="Save result to JSON file"
    )
    parser.add_argument(
        "--include-raw",
        action="store_true",
        help="Include raw upstream API response in output JSON (off by default)",
    )
    parser.add_argument("--start-page", type=int, help="First PDF page, 1-based")
    parser.add_argument("--end-page", type=int, help="Last PDF page, 1-based")
    parser.add_argument(
        "--config",
        default=str(DEFAULT_CONFIG_PATH),
        help="Shared GLM TOML configuration file",
    )

    args = parser.parse_args()
    if args.start_page is not None and args.start_page < 1:
        parser.error("--start-page must be at least 1")
    if args.end_page is not None and args.end_page < 1:
        parser.error("--end-page must be at least 1")
    if (
        args.start_page is not None
        and args.end_page is not None
        and args.end_page < args.start_page
    ):
        parser.error("--end-page must not be less than --start-page")

    # Determine source
    source = args.file_url or args.file

    # Extract text
    result = extract_text(
        image_source=source,
        include_raw_result=args.include_raw,
        config_path=args.config,
        start_page_id=args.start_page,
        end_page_id=args.end_page,
    )

    # Format output
    indent = 2 if args.pretty else None
    json_output = json.dumps(result, indent=indent, ensure_ascii=False)

    # Save to file or print
    if args.output:
        try:
            output_path = Path(args.output).resolve()
            output_path.parent.mkdir(parents=True, exist_ok=True)
            output_path.write_text(json_output, encoding="utf-8")
            print(f"Result saved to: {output_path}", file=sys.stderr)
        except (PermissionError, OSError) as e:
            print(f"Error: Cannot write to {args.output}: {e}", file=sys.stderr)
            sys.exit(5)
    else:
        print(json_output)
        if result["ok"]:
            print("Tip: Use --output result.json to save the result", file=sys.stderr)

    # Exit code based on result
    sys.exit(0 if result["ok"] else 1)


if __name__ == "__main__":
    main()
