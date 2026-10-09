# GLM-Doc-Parser Output Schema

This document describes the output format returned by the GLM-OCR API wrapper.

## API Response Envelope

The `skill__glmocr__extract_text` dynamic tool returns a standardized response envelope:

```json
{
  "ok": true,
  "text": "Extracted text content in Markdown format...",
  "layout_details": [...],
  "result": null,
  "error": null,
  "source": "https://example.com/image.jpg",
  "source_type": "url",
  "raw_result_included": false
}
```

## Fields

### `ok` (boolean)
- Indicates whether the OCR operation was successful
- `true`: Extraction completed successfully
- `false`: An error occurred

### `text` (string)
- The extracted text content from the image in Markdown format
- Only present when `ok` is `true`
- Contains the full text extracted from the image/PDF
- Tables are formatted as Markdown tables
- Formulas are formatted as LaTeX

### `layout_details` (array | null)
- Layout analysis results from the GLM-OCR API
- Only present when `ok` is `true`
- Contains detailed information about detected elements:
  - Tables with cell positions
  - Formulas with positions
  - Text blocks with layout information
- Only present when `need_layout_visualization` is enabled in the API request

### `result` (object | null)
- 默认是 `null`，避免重复输出可能很大的上游响应
- 动态工具调用传入 `include_raw=true` 时，包含完整响应
- Includes fields like `md_results`, `layout_details`, `usage`, etc.

### `raw_result_included` (boolean)
- 明确表示本次结果是否附带了完整上游响应

### `error` (object | null)
- Error details when operation fails
- Only present when `ok` is `false`
- Structure:
  ```json
  {
    "code": "ERROR_CODE",
    "message": "Human readable error message"
  }
  ```

### `source` (string)
- The original input source (URL)
- Always present

### `source_type` (string)
- Type of the input source
- Value: `"url"` or `"file"`

## Error Codes

| Code | Description |
|------|-------------|
| `CONFIG_ERROR` | API key or URL not configured |
| `INPUT_ERROR` | Invalid input (invalid URL, unsupported format, etc.) |
| `API_ERROR` | API request failed (network error, authentication, rate limit, etc.) |

## Raw API Response

The `result` field contains the raw GLM-OCR API response from the layout parsing endpoint:

```json
{
  "md_results": "Extracted text content in Markdown format...",
  "layout_details": [
    {
      "type": "table",
      "bbox": [x1, y1, x2, y2],
      "cells": [...]
    },
    {
      "type": "formula",
      "bbox": [x1, y1, x2, y2],
      "text": "LaTeX formula..."
    }
  ],
  "usage": {
    "prompt_tokens": 1000,
    "completion_tokens": 500,
    "total_tokens": 1500
  }
}
```

The raw response structure follows the GLM-OCR Layout Parsing API format and may include:
- `md_results`: Markdown formatted text extraction results
- `layout_details`: Detailed layout analysis with bounding boxes and element classifications
- `usage`: Token usage statistics

## Usage Examples

### Basic Text Extraction

```json
{"file_url":"https://example.com/image.jpg","include_raw":true}
```

Call `skill__glmocr__extract_text` with this input. Read extracted content from `text` when
`ok=true`; otherwise report `error.message` without retrying through a hand-written Python or shell command.

### Accessing Raw API Response

```json
{"file_url":"https://example.com/image.jpg","include_raw":true}
```

When `ok=true`, inspect `result.usage.total_tokens` in the structured tool output.

### URL Processing

```json
{"file_url":"https://example.com/document.jpg"}
```

When `ok=true`, use the returned `source_type` and `text` fields.
