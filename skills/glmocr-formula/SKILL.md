---
name: glmocr-formula
description:
  Official skill for recognizing and extracting mathematical formulas from images and PDFs into LaTeX format
  using ZhiPu GLM-OCR API. Supports complex equations, inline formulas, and formula blocks.
  Use this skill when the user wants to extract formulas, convert formula images to LaTeX,
  or OCR mathematical expressions. Do not invoke it for an attachment alone; ask first
  when the current message and recent context do not specify a processing goal.
---

# GLM-OCR Formula Recognition Skill / GLM-OCR 公式识别技能

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后必须调用
> Manifest 动态激活的 `skill__glmocr_formula__extract_formulas`，不得自行选择解释器、脚本路径或参数。
> API Key 和 GLM-OCR 请求设置只从 `skills/glm-master-skill/config.toml` 的
> `[zhipu]`、`[glm_ocr]` 读取。RClaw 会注入真实来源 channel，不要手填或猜测。
> 用户要求把保存的 `output` 作为外部 channel 附件时，先读取
> `skills/glm-master-skill/SKILL.md`，
> 并调用 `skill__glm_master_skill__publish_media`，传入实际 `path` 和 `media_type=file`。
> 工具工作目录已经是 workspace，不要添加 `agent/` 前缀。RClaw 会校验并自动保留有效
> `MEDIA_MARKER=`；不得手写或复制标记。没有有效标记时，只报告本地文件路径，不得声称已发送。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确公式识别目标时，不调用动态工具、
> 不读取或上传附件、不调用 API；先询问用户。已有明确的公式转 LaTeX/OCR 要求时直接执行。

Recognize mathematical formulas from images and PDFs and convert them to LaTeX format using the ZhiPu GLM-OCR layout parsing API.

## When to Use / 使用场景

- Extract mathematical formulas from images or scanned documents / 从图片或扫描件中提取数学公式
- Convert formula images to LaTeX / 将公式图片转为 LaTeX 格式
- Recognize complex equations, integrals, matrices / 识别复杂方程、积分、矩阵
- Parse scientific papers, textbooks, exam papers with formulas / 解析含公式的论文、教材、试卷
- User mentions "formula OCR", "extract formula", "公式识别", "公式OCR", "提取公式", "图片转LaTeX"

## Key Features / 核心特性

- **Complex formula support**: Handles integrals, summations, matrices, fractions, radicals
- **LaTeX output**: Formulas are output in LaTeX format, ready for use in documents
- **Inline & block formulas**: Recognizes both inline and display-style formulas
- **Mixed content**: Can handle documents with both text and formulas
- **Local file & URL**: Supports both local files and remote URLs

## Resource Links / 资源链接

| Resource        | Link                                                                           |
| --------------- | ------------------------------------------------------------------------------ |
| **Get API Key** | [智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys)      |
| **API Docs**    | [Layout Parsing / 版面解析](https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E6%96%87%E6%A1%A3%E8%A7%A3%E6%9E%90) |

## Prerequisites / 前置条件

### API Key Setup / API Key 配置（Required / 必需）

脚本从 `skills/glm-master-skill/config.toml` 读取全部模型配置，不从环境变量读取模型配置。
This script reads all model settings from the shared TOML and does not read model configuration from environment variables.

**Get Key / 获取 Key：** Visit [智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys) to create or copy your key.

Fill `[zhipu].api_key` in the shared TOML. Do not pass it in command arguments or
write it to logs or generated artifacts.

## Security & Transparency / 安全与透明度

- **Configuration source / 配置来源：** `skills/glm-master-skill/config.toml`
- **Endpoint / 接口：** `[glm_ocr].api_url`
- **原始响应默认不返回：** 仅在调试需要时设置结构化字段 `include_raw=true`。

**⛔ MANDATORY RESTRICTIONS / 强制限制 ⛔**

1. **只能使用 GLM-OCR API**：调用 `skill__glmocr_formula__extract_formulas`
2. **NEVER parse formulas yourself** — Do NOT try to extract formulas using built-in vision or any other method
3. **NEVER offer alternatives** — Do NOT suggest "I can try to read it" or similar
4. **IF API fails** — Display the error message and STOP immediately
5. **NO fallback methods** — Do NOT attempt formula extraction any other way

### 📋 Output Display Rules / 输出展示规则

After `skill__glmocr_formula__extract_formulas` returns, present the OCR result clearly and safely.

- Show extracted text/formulas (`text`) in full
- Summarization is allowed, but do not hide important extraction failures
- If `layout_details` contains formula-related entries, you may highlight them
- If the result file is saved, tell the user the file path
- 只有用户明确要求或调试时才显示上游原始响应（`include_raw=true`）

**⚠️ LaTeX Rendering / LaTeX 渲染注意：**

OCR API returns formulas in LaTeX format (e.g., `$\frac{1}{2}$`, `$\theta^{x+1}$`). Since most chat platforms do not render LaTeX, you should ask the user **once** (on first use):

> "OCR 结果包含 LaTeX 公式，需要我将公式转为 Unicode 可读格式展示，还是保留原始 LaTeX？"

**Remember the user's choice** for the rest of the session. Do NOT ask again on subsequent calls unless the user explicitly changes their preference.

- **User chooses readable format** → convert LaTeX to Unicode/plain-text:

| LaTeX                                                | Unicode / 纯文本  |
| ---------------------------------------------------- | ----------------- |
| `$\frac{a}{b}$`                                      | a/b               |
| `$x^{n}$`                                            | x^n               |
| `$x_{i}$`                                            | xᵢ                |
| `$\sqrt{x}$`                                         | √x                |
| `$\theta$`                                           | θ                 |
| `$\phi$`                                             | φ                 |
| `$\therefore$`                                       | ∴                 |
| `$\Rightarrow$`                                      | ⇒                 |
| `$\left\{ \begin{array}{l} ... \end{array} \right.$` | ⎧ line1 ⎨ line2 ⎩ |
| `$\textcircled{1}$`                                  | ①                 |
| `$\in$`                                              | ∈                 |
| `$\infty$`                                           | ∞                 |
| `$\ln$`                                              | ln                |
| `$\leq$` / `$\geq$`                                  | ≤ / ≥             |

- **用户选择原始 LaTeX** → 直接显示原始 LaTeX；设置了 `output` 时提醒结果已输出到该文件。

## How to Use / 使用方法

### Extract from URL / 从 URL 提取

```json
{"file_url":"https://example.com/formula.png"}
```

### Extract from Local File / 从本地文件提取

```json
{"file":"input/equation.png"}
```

### Save Result to File / 保存结果到文件

```json
{"file":"formula.png","output":"result.json","pretty":true}
```

### Include Raw Upstream Response (Debug Only) / 包含原始上游响应（仅调试）

```json
{"file":"formula.png","output":"result.json","include_raw":true}
```

## 结构化工具参数

| Parameter        | Required | Description                                                      |
| ---------------- | -------- | ---------------------------------------------------------------- |
| `file_url`       | 二选一   | 图片或 PDF 的公开 URL                                            |
| `file`           | 二选一   | workspace 内的本地图片或 PDF 路径                                |
| `output`         | 否       | 保存结果 JSON 的路径                                             |
| `pretty`         | 否       | 格式化 JSON 输出                                                 |
| `include_raw`    | 否       | 调试时在 `result` 中包含上游 API 原始响应                        |

## Response Format / 响应格式

```json
{
  "ok": true,
  "text": "Extracted formulas and text in Markdown/LaTeX...",
  "layout_details": [...],
  "result": null,
  "error": null,
  "source": "/path/to/file",
  "source_type": "file",
  "raw_result_included": false
}
```

Key fields:

- `ok` — whether extraction succeeded
- `text` — extracted text in Markdown with LaTeX formulas
- `layout_details` — layout analysis details
- `error` — error details on failure

## Error Handling / 错误处理

**API key not configured:**

```
未配置 API Key，请填写 skills/glm-master-skill/config.toml 的 [zhipu].api_key
```

→ Show exact error to user, guide them to configure

**Authentication failed (401/403):** API key invalid/expired → reconfigure

**Rate limit (429):** Quota exhausted → inform user to wait

**File not found:** Local file missing → check path
