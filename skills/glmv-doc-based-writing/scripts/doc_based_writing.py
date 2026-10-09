"""
GLM-V Document-Based Writing Skill
Write a textual content based on given document(s) and requirements, with ZhiPu GLM-V multimodal model.

Official API docs (https://docs.bigmodel.cn/api-reference/模型-api/对话补全):
- file_url supports URL only (no local path, no base64)
- Supported remote formats: pdf/doc/docx, up to 50 files
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

DEFAULT_SYSTEM_PROMPT = """\
# Role: 全能写作专家
## Profile
你是一位拥有20年经验的顶尖写作专家，精通学术研究、商业文案、创意写作及职场公文等各类文体。你具备深厚的语言学养、严密的逻辑思维以及对不同受众心理的精准洞察。你的目标是通过文字精准传达信息、说服读者并展现卓越的专业素养。
## Core Capabilities
1.  **语言驾驭**：精通多语种写作（包括中文、英文等），能根据语境灵活调整语气、风格与措辞。
2.  **逻辑构建**：擅长构建清晰的论证框架，确保文章结构严谨、层次分明、过渡自然。
3.  **受众洞察**：能根据目标读者调整内容的深度与表达方式。
4.  **格式规范**：熟悉APA、MLA、Chicago、GB/T 7713等主流格式规范及各类商务文档标准。
## Writing Protocols (文体处理协议)
请根据用户指定的文体，严格执行以下标准：
### 1. 学术与研究类
*   **Paper (学术论文)**：必须具备严谨的学术态度。结构需包含摘要、引言、方法、结果、讨论、结论。引用需规范，论证需有数据或文献支撑，避免主观臆断，语言正式客观。
*   **Essay (短文/随笔)**：侧重于观点表达与思想深度。需有明确的中心思想，结构灵活但有逻辑脉络。语言可适当具个性化，但需保持理性和思辨性。
*   **Review (综述/评论)**：需对现有文献或作品进行批判性分析。不仅要总结内容，更要指出优缺点、研究空白或发展趋势，展现宏观视野。
*   **Report (报告)**：注重事实陈述与数据呈现。结构通常包括执行摘要、背景、正文（数据/事实）、建议。语言需简练、直接、无歧义。
### 2. 商务与职场类
*   **Proposal (提案/方案)**：以说服为核心。明确痛点，提出解决方案，展示实施路径与预期收益。结构需具有说服力（问题-方案-优势-预算/计划）。
*   **Plan (计划书)**：强调可执行性。需包含明确的SMART目标（具体、可衡量、可达成、相关、有时限）、时间轴、资源分配及风险评估。
*   **Brief (简报/摘要)**：极简主义的艺术。需在有限篇幅内提炼核心信息，去除所有冗余修饰，确保读者能在最短时间内获取关键点。
### 3. 媒体与传播类
*   **Article (文章/稿件)**：注重可读性与吸引力。需有引人入胜的标题、强有力的开篇和流畅的叙述节奏。根据发布平台调整专业度与趣味性的平衡。
*   **Post (推文/帖子)**：侧重互动性与传播性。语言需接地气、有感染力，善用排版、表情符号（如适用）和标签，开头需在3秒内抓住读者注意力。
## Workflow (工作流程)
在开始写作任务前，请遵循以下步骤：
1.  **大纲构建**：
    *   对于长篇或结构复杂的文档，先撰写大纲。
2.  **内容撰写**：
    *   输出内容时，请确保语法正确、用词精准。
    *   根据文体自动调整语气（例如：Paper需严肃客观，Post需生动活泼）。
3.  **优化迭代**：
    *   写作完成后，简要说明该文稿的亮点（如逻辑结构、修辞手法）。
    *   询问用户是否需要修改、润色或扩展。
## Constraints (约束条件)
*   拒绝生成虚假信息或学术不端内容。
*   保持专业，不输出带有偏见、歧视或攻击性的语言。
*   始终以”帮助用户写出最好的作品”为最高优先级。
"""

DEFAULT_USER_PROMPT_TEMPLATE = (
    "这里有{count}份文档，请根据以下要求完成写作任务，并以Markdown格式输出：\n\n"
    "## \n{requirements}"
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


def validate_document_inputs(files: List[str], requirements: str) -> List[str]:
    """Collect all deterministic input errors without rendering any PDF pages."""
    errors: List[str] = []
    if not files:
        errors.append("No document files provided.")
    if not requirements.strip():
        errors.append("Requirements cannot be empty.")
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


def write_from_documents(
    files: List[str],
    requirements: str,
    system_prompt: Optional[str] = None,
    config_path: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Write content from source documents using the configured GLM-V model.

    Args:
        files: Document URLs (PDF/DOC/DOCX, URL only, max 50), or local PDFs
        requirements: Writing requirements
        system_prompt: Custom system prompt
        config_path: Shared TOML config path; defaults to glm-master-skill/config.toml

    Returns:
        Dict with success, result, and usage fields
    """
    input_errors = validate_document_inputs(files, requirements)
    if input_errors:
        return {"success": False, "error": format_validation_errors(input_errors)}

    try:
        config = load_config("glm5v", "doc_based_writing", config_path=config_path)
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
        requirements=requirements.strip(),
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
        "requirements": requirements,
        "file_count": len(files),
        "finish_reason": finish_reason,
        # 只有正常停止才代表文档写作正文完整。
        "complete": finish_reason == "stop",
    }
    if finish_reason == "length":
        out["warning"] = "Output was truncated because the token limit was reached."
    elif finish_reason == "sensitive":
        out["warning"] = "Content blocked by safety review (finish_reason: sensitive)"
    return out


def main():
    parser = argparse.ArgumentParser(
        description="Write a textual content based on given document(s) and requirements, using GLM-V model"
    )

    parser.add_argument(
        "--files",
        "-f",
        nargs="+",
        required=True,
        help="Document URLs (pdf/doc/docx) or local PDFs (max 50 files/pages)",
    )
    parser.add_argument(
        "--requirements",
        "-c",
        required=True,
        help="Writing requirements (e.g., 'Draft a professional article')",
    )
    parser.add_argument(
        "--system-prompt",
        "-s",
        default=None,
        help="Custom system prompt (default: general-purpose writing expert)",
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
        result = write_from_documents(
            files=args.files,
            requirements=args.requirements,
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
            f.write("# 写作内容\n\n")
            f.write(f"**写作要求：** {args.requirements}\n\n")
            f.write(f"**文档数量：** {result.get('file_count', 0)} 份\n\n")
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
