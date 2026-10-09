"""
GLM-V Resume Screening Tool
Batch-read and screen resumes with Zhipu GLM-V vision models.

Official API docs (https://docs.bigmodel.cn/api-reference/模型-api/对话补全):
- file_url supports URL only (no local path, no base64)
- Supported remote formats: pdf/doc/docx/txt, up to 50 files
- Local files support PDF only (automatically converted page-by-page into images)
- file_url cannot be mixed with image_url or video_url in the same request

RClaw adaptation: model metadata and help text updated for GLM-5V-Turbo.
"""

import json
import base64
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
from glm_config import (
    DEFAULT_CONFIG_PATH,
    api_url,
    load_config,
    non_negative_int,
    positive_int,
)

try:
    import fitz  # PyMuPDF

    HAS_PYMUPDF = True
except ImportError:
    HAS_PYMUPDF = False

MAX_LOCAL_PDF_BYTES = 50 * 1024 * 1024
MAX_RENDERED_PAGE_BYTES = 5 * 1024 * 1024

# Model max_tokens limits (per official docs)
MODEL_MAX_TOKENS = {
    "glm-5v-turbo": {"default": 16384, "max": 128000},
    "glm-4.6v": {"default": 16384, "max": 32768},
    "glm-4.6v-flash": {"default": 16384, "max": 32768},
    "glm-4.6v-flashx": {"default": 16384, "max": 32768},
}

DEFAULT_SYSTEM_PROMPT = (
    "你是一位专业的人力资源专家和简历筛选助手。"
    "你需要仔细阅读每份简历，根据用户给出的筛选标准，判断每位候选人是否符合要求，并给出详细的分析原因。"
    "输出必须是严格的 Markdown 表格格式，表头为：序号、候选人姓名、是否符合、符合程度（高/中/低）、原因分析。"
    "原因分析应简明扼要，突出与筛选标准的匹配点和不足之处。"
)

DEFAULT_USER_PROMPT_TEMPLATE = (
    "以下是 {count} 份候选人的简历，请根据以下筛选标准进行评估：\n\n"
    "## 筛选标准\n{criteria}\n\n"
    "请逐一分析每份简历，并输出 Markdown 表格。"
)


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


def validate_resume_inputs(files: List[str], criteria: str) -> List[str]:
    """Collect all deterministic input errors before rendering any resume."""
    errors: List[str] = []
    if not files:
        errors.append("No resume files provided.")
    if not criteria.strip():
        errors.append("Screening criteria cannot be empty.")
    if len(files) > 50:
        errors.append(f"Too many files: {len(files)} (max 50).")

    local_count = 0
    remote_count = 0
    total_pages = 0
    dependency_reported = False
    for index, raw in enumerate(files):
        value = raw.strip()
        if not value:
            errors.append(f"files[{index}] must not be blank.")
            continue
        if _is_url(value):
            remote_count += 1
            continue
        if value.lower().startswith(("http://", "https://")):
            errors.append(f"files[{index}] is not a valid HTTP(S) URL: {raw}")
            continue

        local_count += 1
        path = Path(value)
        if not path.is_file():
            errors.append(f"files[{index}] not found: {raw}")
            continue
        if path.suffix.lower() != ".pdf":
            errors.append(f"files[{index}] local files only support PDF: {raw}")
            continue
        if path.stat().st_size > MAX_LOCAL_PDF_BYTES:
            errors.append(f"files[{index}] local PDF exceeds 50 MB: {raw}")
            continue
        if not HAS_PYMUPDF:
            import os

            if os.environ.get("RCLAW_ANDROID") == "1":
                # APK：无 fitz，页数经 PdfRenderer 桥校验（加密文档由渲染期自然报错）。
                from rclaw_android_runtime import pdf_page_count

                try:
                    count = pdf_page_count(str(path))
                    if count < 1:
                        # 与桌面分支同语义：0 页文档在此拦截，不发空跑 API 请求。
                        errors.append(f"files[{index}] local PDF contains no pages: {raw}")
                    else:
                        total_pages += count
                except Exception as exc:
                    errors.append(f"files[{index}] cannot be opened as PDF: {exc}")
                continue
            if not dependency_reported:
                errors.append("PyMuPDF is required to process local PDF files.")
                dependency_reported = True
            continue
        try:
            doc = fitz.open(str(path))
            try:
                if doc.needs_pass:
                    errors.append(f"files[{index}] local PDF requires a password: {raw}")
                elif doc.page_count < 1:
                    errors.append(f"files[{index}] local PDF contains no pages: {raw}")
                else:
                    total_pages += doc.page_count
            finally:
                doc.close()
        except Exception as exc:
            errors.append(f"files[{index}] cannot be opened as PDF: {exc}")

    if local_count and remote_count:
        errors.append(
            "Cannot mix local files and URLs in the same request (API limitation)."
        )
    if total_pages > 50:
        errors.append(f"Too many local PDF pages: {total_pages} (max 50).")
    return errors


def format_validation_errors(errors: List[str]) -> str:
    return (
        f"Input preflight found {len(errors)} problem(s):\n"
        + "\n".join(f"- {error}" for error in errors)
    )


def _pdf_to_images(pdf_path: str, max_pages: int = 50) -> List[str]:
    """Convert local PDF pages to base64 data URLs."""
    if not HAS_PYMUPDF:
        import os

        if os.environ.get("RCLAW_ANDROID") == "1":
            # APK：无 fitz，经 PdfRenderer Java 桥渲染页图（含页数上限校验）。
            from rclaw_android_runtime import pdf_page_count, pdf_to_data_urls

            page_count = pdf_page_count(pdf_path)
            if page_count > max_pages:
                raise ValueError(
                    f"Too many pages: {page_count} pages exceed remaining limit "
                    f"of {max_pages}"
                )
            if page_count < 1:
                raise ValueError(f"PDF contains no pages: {pdf_path}")
            return pdf_to_data_urls(
                pdf_path,
                dpi=150,
                max_pages=max_pages,
                max_page_bytes=MAX_RENDERED_PAGE_BYTES,
            )
        raise RuntimeError(
            "PyMuPDF is required to process local PDF files. "
            "Install it with: pip install PyMuPDF"
        )
    path = Path(pdf_path)
    if not path.is_file():
        raise FileNotFoundError(f"File not found: {pdf_path}")
    if path.suffix.lower() != ".pdf":
        raise ValueError(f"Not a PDF file: {pdf_path}")
    if path.stat().st_size > MAX_LOCAL_PDF_BYTES:
        raise ValueError("Local PDF exceeds 50 MB")
    if max_pages < 1:
        raise ValueError("Too many pages: maximum combined page count is 50")

    doc = fitz.open(str(path))
    try:
        if doc.page_count > max_pages:
            raise ValueError(
                f"Too many pages: {doc.page_count} pages exceed remaining limit "
                f"of {max_pages}"
            )
        images = []
        for page_number, page in enumerate(doc, start=1):
            pix = page.get_pixmap(dpi=150)
            img_bytes = pix.tobytes("png")
            # 中文注释：本地 PDF 页最终按 image_url 发送，必须再次满足单图 5 MB 上限。
            if len(img_bytes) > MAX_RENDERED_PAGE_BYTES:
                raise ValueError(
                    f"Rendered PDF page {page_number} exceeds 5 MB; reduce page complexity or DPI"
                )
            b64 = base64.b64encode(img_bytes).decode()
            images.append(f"data:image/png;base64,{b64}")
        return images
    finally:
        doc.close()


def screen(
    files: List[str],
    criteria: str,
    system_prompt: Optional[str] = None,
    config_path: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Screen resumes using GLM-V model.

    Args:
        files: Resume URLs (pdf/docx/txt) or local PDFs (max 50 files/pages)
        criteria: Screening criteria text
        system_prompt: Custom system prompt
        config_path: Shared TOML config path; defaults to glm-master-skill/config.toml

    Returns:
        Dict with success, result, and usage fields
    """
    input_errors = validate_resume_inputs(files, criteria)
    if input_errors:
        return {"success": False, "error": format_validation_errors(input_errors)}

    try:
        config = load_config("glm5v", "resume_screen", config_path=config_path)
        api_key = config["api_key"]
        model = str(config["model"])
        temperature = float(config["temperature"])
        # 中文注释：采样温度必须是有限的 0..1 数值。
        if not math.isfinite(temperature) or not 0 <= temperature <= 1:
            raise ValueError("temperature must be finite and between 0 and 1")
        max_tokens = positive_int(config, "max_tokens")
        connect_timeout_sec = positive_int(config, "connect_timeout_sec")
        read_timeout_sec = positive_int(config, "read_timeout_sec")
        max_retries = non_negative_int(config, "max_retries")
        endpoint = api_url(config, "chat/completions")
    except (KeyError, TypeError, ValueError) as exc:
        # 中文注释：在渲染 PDF 前一次性校验配置，避免昂贵预处理后才因配置失败。
        return {"success": False, "error": f"Configuration: {exc}"}

    # Validate max_tokens against model limits
    model_key = model.lower()
    model_limits = MODEL_MAX_TOKENS.get(model_key)
    if model_limits and max_tokens > model_limits["max"]:
        return {
            "success": False,
            "error": f"max_tokens {max_tokens} exceeds model '{model}' limit of {model_limits['max']}.",
        }

    # Local PDFs and remote URLs cannot mix (API limitation: file_url and image_url cannot coexist).
    has_local = any(not _is_url(f) for f in files)
    if has_local:
        # All local PDFs → image_url (base64)
        local_page_count = 0
        pdf_page_images: List[List[str]] = []
        for f in files:
            path = Path(f.strip())
            try:
                pages = _pdf_to_images(str(path), 50 - local_page_count)
            except (ValueError, FileNotFoundError, RuntimeError) as e:
                return {"success": False, "error": str(e)}
            pdf_page_images.append(pages)
            local_page_count += len(pages)

        if local_page_count > 50:
            return {
                "success": False,
                "error": f"Too many pages: {local_page_count} (max 50).",
            }

        content = []
        for pages in pdf_page_images:
            for page_img in pages:
                content.append(
                    {
                        "type": "image_url",
                        "image_url": {"url": page_img},
                    }
                )
    else:
        # All remote URLs → file_url
        if len(files) > 50:
            return {
                "success": False,
                "error": f"Too many files: {len(files)} (max 50).",
            }

        content = []
        for f in files:
            content.append(
                {
                    "type": "file_url",
                    "file_url": {"url": f.strip()},
                }
            )

    user_prompt = DEFAULT_USER_PROMPT_TEMPLATE.format(
        count=len(files),
        criteria=criteria.strip(),
    )
    content.append({"type": "text", "text": user_prompt})

    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }

    payload: Dict[str, Any] = {
        "model": model,
        "messages": [
            {"role": "system", "content": system_prompt or DEFAULT_SYSTEM_PROMPT},
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
        "usage": result.get("usage", {}),
        "criteria": criteria,
        "file_count": len(files),
        "finish_reason": finish_reason,
        # 只有正常停止才代表简历筛选正文完整。
        "complete": finish_reason == "stop",
    }
    if finish_reason == "length":
        out["warning"] = "Output was truncated because the token limit was reached."
    elif finish_reason == "sensitive":
        out["warning"] = "Content blocked by safety review (finish_reason: sensitive)"
    return out


def main():
    parser = argparse.ArgumentParser(
        description="Screen resumes using GLM-V model — read resumes and evaluate against criteria"
    )

    parser.add_argument(
        "--files",
        "-f",
        nargs="+",
        required=True,
        help="Resume URLs (pdf/docx/txt) or local PDFs (max 50 files/pages)",
    )
    parser.add_argument(
        "--criteria",
        "-c",
        required=True,
        help="Screening criteria (e.g., '3+ years experience, Python skills')",
    )
    parser.add_argument(
        "--system-prompt",
        "-s",
        default=None,
        help="Custom system prompt (default: professional HR assistant)",
    )
    parser.add_argument(
        "--config",
        default=str(DEFAULT_CONFIG_PATH),
        help="Shared GLM TOML configuration file",
    )
    parser.add_argument(
        "--output", "-o", default=None, help="Save result to file (.md or .json)"
    )
    parser.add_argument(
        "--pretty", action="store_true", help="Pretty-print JSON output"
    )

    args = parser.parse_args()

    try:
        result = screen(
            files=args.files,
            criteria=args.criteria,
            system_prompt=args.system_prompt,
            config_path=args.config,
        )
    except Exception as e:
        result = {"success": False, "error": str(e)}

    if not result.get("success"):
        indent = 2 if args.pretty else None
        print(json.dumps(result, ensure_ascii=False, indent=indent))
        sys.exit(1)

    # 中文注释：输出扩展名按大小写无关处理，并提前创建父目录。
    output_path = Path(args.output).resolve() if args.output else None
    if output_path:
        output_path.parent.mkdir(parents=True, exist_ok=True)
    if output_path and output_path.suffix.lower() == ".md":
        # Save as markdown (extract just the table/content)
        with output_path.open("w", encoding="utf-8") as f:
            f.write("# 简历筛选结果\n\n")
            f.write(f"**筛选标准：** {args.criteria}\n\n")
            f.write(f"**简历数量：** {result.get('file_count', 0)} 份\n\n")
            f.write("---\n\n")
            f.write(result.get("result", ""))
        print(f"Result saved to: {output_path}", file=sys.stderr)
    elif output_path:
        # Save as JSON
        indent = 2 if args.pretty else None
        with output_path.open("w", encoding="utf-8") as f:
            json.dump(result, f, ensure_ascii=False, indent=indent)
        print(f"Result saved to: {output_path}", file=sys.stderr)
    else:
        # Print markdown result to stdout
        print(result.get("result", ""))
    if result.get("warning"):
        print(f"Warning: {result['warning']}", file=sys.stderr)


if __name__ == "__main__":
    main()
