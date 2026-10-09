---
name: glmocr
description:
  Extract text from images using GLM-OCR API. Supports images and PDFs with
  high accuracy OCR, table recognition, formula extraction, and handwriting recognition.
  Use this skill whenever the user wants to extract text from images, perform OCR on
  pictures, scan documents, convert images to text, or process any image files to get
  their textual content. Do not run OCR merely because a file was attached; when the
  current message and recent context do not specify a task, ask what the user wants first.
---

# GLM-OCR Text Extraction Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后必须调用
> Manifest 动态激活的 `skill__glmocr__extract_text`，不得自行选择 Python 或拼接脚本路径。
> API Key 和 GLM-OCR 请求设置只从 `skills/glm-master-skill/config.toml` 的
> `[zhipu]`、`[glm_ocr]` 读取。RClaw 会注入真实来源 channel，不要手填或猜测。
> 用户要求把保存的 `output` 作为外部附件时，先读取 `skills/glm-master-skill/SKILL.md`，
> 再调用 `skill__glm_master_skill__publish_media`，传入实际 `path` 和 `media_type=file`。
> 工作目录已经是 workspace，不要添加 `agent/` 前缀。RClaw 会校验并自动保留有效
> `MEDIA_MARKER=`；不得手写或复制标记。没有有效标记时，只报告本地文件路径，不得声称已发送。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确 OCR 或其它处理目标时，
> 不调用动态工具、不读取或上传附件、不调用 API；先说明可提取正文、表格、公式或手写内容并询问用户。
> 已有明确任务时直接执行，不要重复确认。

Extract text from images and PDFs using the GLM-OCR layout parsing API.

## When to Use

- Extract text from images (PNG, JPG, PDF)
- Convert screenshots to text
- Process scanned documents
- OCR photos containing text (including handwritten text)
- Recognize tables and formulas in documents
- User mentions "OCR", "文字识别", "文档解析"

## Key Features

- **Table recognition**: Detects and converts tables to Markdown format
- **Formula extraction**: LaTeX format output
- **Handwriting support**: Strong recognition for handwritten text
- **Local file & URL**: Supports both local files and remote URLs

## Resource Links

| Resource | Link |
|----------|------|
| **Get API Key** | [https://www.bigmodel.cn/usercenter/proj-mgmt/apikeys](https://www.bigmodel.cn/usercenter/proj-mgmt/apikeys) |
| **GitHub** | [https://github.com/zai-org/GLM-OCR](https://github.com/zai-org/GLM-OCR) |

## Prerequisites

- `skills/glm-master-skill/config.toml` configured (see Setup below)

## Security Notes

- No runtime package installation is performed by the scripts.
- OCR requests use the endpoint configured in the shared TOML.
- Model configuration environment variables are not read.

**⛔ MANDATORY RESTRICTIONS - DO NOT VIOLATE ⛔**

1. **只能使用 GLM-OCR API**：调用 `skill__glmocr__extract_text`
2. **NEVER parse documents directly** - Do NOT try to extract text yourself
3. **NEVER offer alternatives** - Do NOT suggest "I can try to analyze it" or similar
4. **IF API fails** - Display the error message and STOP immediately
5. **NO fallback methods** - Do NOT attempt text extraction any other way

## Setup

Get the API key from the official console, then fill `[zhipu].api_key` in
`skills/glm-master-skill/config.toml`. Do not pass it in command arguments or write it
to logs or generated artifacts.

## How to Use

调用 `skill__glmocr__extract_text`，并且必须且只能提供 `file` 或 `file_url` 之一。建议传入 `output` 保存 JSON；完整上游响应可能很大，默认不返回，仅在调试时设置 `include_raw=true`。

## 结构化工具参数

| 参数 | 必填性 | 说明 |
|-----------|----------|-------------|
| `file_url` | 二选一 | 图片或 PDF 的公开 URL |
| `file` | 二选一 | workspace 内的本地图片或 PDF 路径 |
| `output` | 否 | 保存结果 JSON 的路径 |
| `pretty` | 否 | 格式化 JSON 输出 |
| `include_raw` | 否 | 仅调试时附带完整上游 API 响应，默认关闭 |
| `start_page` | 否 | PDF 起始页，按 1 计数；省略时使用 API 默认值 |
| `end_page` | 否 | PDF 结束页，按 1 计数；省略时解析到 API 默认终点 |

## Response Format

```json
{
  "ok": true,
  "text": "# Extracted text in Markdown...",
  "layout_details": [[...]],
  "result": null,
  "error": null,
  "source": "/path/to/file.jpg",
  "source_type": "file",
  "raw_result_included": false
}
```

Key fields:
- `ok` — whether extraction succeeded
- `text` — extracted text in Markdown (use this for display)
- `layout_details` — layout analysis details
- `result` — 设置 `include_raw=true` 时才包含完整上游响应，否则为 `null`
- `raw_result_included` — 是否实际附带了完整上游响应
- `error` — error details on failure

## Error Handling

**API key not configured:**
```
未配置 API Key，请填写 skills/glm-master-skill/config.toml 的 [zhipu].api_key
```
→ Show exact error to user, guide them to configure

**Authentication failed (401/403):** API key invalid/expired → reconfigure

**Rate limit (429):** Quota exhausted → inform user to wait

**File not found:** Local file missing → check path

## Reference

- `references/output_schema.md` — detailed output format specification
