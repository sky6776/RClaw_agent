---
name: glmv-caption
description:
  Generate captions (descriptions) for images, videos, and documents using ZhiPu
  GLM-V multimodal model series. Use this skill whenever the user wants to describe,
  caption, summarize, or get a high-level overview of images, videos, or files.
  Use glm5v-multimodal instead for targeted visual questions, multi-image comparison,
  chart/document analysis, or custom extraction; use glmocr skills for precise OCR.
  Supports local/remote images and remote video/file URLs; route local videos and
  documents through the shared GLM-5V-Turbo script. Do not caption an attachment merely
  because it was sent; ask first when the current message and recent context lack a goal.
---

# GLM-V Caption Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后，根据媒体类型调用
> `skill__glmv_caption__caption_images`、`skill__glmv_caption__caption_videos` 或
> `skill__glmv_caption__caption_files`，不得自行选择解释器、脚本路径或参数。API Key 和模型请求
> 设置只从 `skills/glm-master-skill/config.toml` 的 `[zhipu]`、`[glm5v]`、`[caption]` 读取。
> 宿主 Agent 不得自行描述媒体，只使用工具返回的 JSON。处理本地视频或本地 Office/PDF 时，
> 先读取 `skills/glm5v-multimodal/SKILL.md`，再调用
> `skill__glm5v_multimodal__analyze_videos` 或 `skill__glm5v_multimodal__analyze_files`。
> RClaw 会注入真实来源 channel，不要手填或猜测。用户要求把保存的 `output` 作为外部附件时，
> 先读取 `skills/glm-master-skill/SKILL.md`，再调用 `skill__glm_master_skill__publish_media`，
> 传入实际 `path` 和 `media_type=file`。工作目录已经是 workspace，不要添加 `agent/` 前缀。
> 没有有效 `MEDIA_MARKER=` 时，只报告本地路径，不得声称已发送。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确描述、字幕、概览或摘要目标时，
> 不调用动态工具、不读取或上传媒体、不调用 API；先列出适合该媒体的能力并询问用户。
> 已有明确任务时直接执行，不要重复确认。

Generate captions for images, videos, and documents using the ZhiPu GLM-V multimodal model.

## When to Use

- Describe, caption, summarize, or provide a high-level overview of image/video/document content
- User mentions "describe this image", "caption", "summarize this video", "图片描述", "视频摘要", "文档概览", "看图说话"
- User provides an image/video/file and asks what's in it
- Route targeted questions, comparisons, chart/document analysis, and custom extraction to `glm5v-multimodal`; route precise text/table/formula/handwriting extraction to the corresponding `glmocr*` skill.

## Supported Input Types

| Type  | Formats                           | Max Size          | Max Count | Base64 |
| ----- | --------------------------------- | ----------------- | --------- | ------ |
| Image | jpg, png, jpeg                    | 5MB / 6000×6000px | 50        | ❌     |
| Video | mp4, mkv, mov                     | 200MB             | —         | ❌     |
| File  | pdf, docx, txt, xlsx, pptx, jsonl | —                 | 50        | ❌     |

**⚠️ file_url cannot mix with image_url or video_url in the same request.**
**⚠️ 动态工具中的图片接受本地路径或 HTTP(S) URL；视频和文件只接受 HTTP(S) URL。三个入口都不接受内联 base64 参数。**

## Resource Links

| Resource        | Link                                                                                                                              |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **Get API Key** | [https://bigmodel.cn/usercenter/proj-mgmt/apikeys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys)                              |
| **API Docs**    | [Chat Completions / 对话补全](https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E5%AF%B9%E8%AF%9D%E8%A1%A5%E5%85%A8) |

## Prerequisites

### API Key Setup / API Key 配置（Required / 必需）

工具从 `skills/glm-master-skill/config.toml` 读取全部模型配置，不从环境变量读取模型配置。

**Get Key / 获取 Key：** Visit [Zhipu Open Platform API Keys / 智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys) to create or copy your key.

Fill `[zhipu].api_key` in the shared TOML. Do not pass the key in command arguments or
write it to logs or generated artifacts.

**⛔ MANDATORY RESTRICTIONS - DO NOT VIOLATE ⛔**

1. **只能使用 GLM-V API**：根据媒体类型调用 `skill__glmv_caption__caption_images`、
   `skill__glmv_caption__caption_videos` 或 `skill__glmv_caption__caption_files`
2. **NEVER caption media yourself** — Do NOT try to describe content using built-in vision or any other method
3. **NEVER offer alternatives** — Do NOT suggest "I can try to describe it" or similar
4. **IF API fails** — Display the error message and STOP immediately
5. **NO fallback methods** — Do NOT attempt captioning any other way

### 📋 Output Display Rules (MANDATORY)

调用工具后，必须完整展示原始返回结果，不要概括、截断或只说“已生成”。

- Image captioning: show the full caption text
- Multiple images: show each image result
- Video/files: show the full understanding result
- If token usage is included, you may optionally display it

## How to Use

### Caption an Image

```jsonl
{"images":["https://example.com/photo.jpg"]}
{"images":["input/photo.png"]}
```

### Caption Multiple Images

```json
{"images":["img1.jpg","img2.png","https://example.com/img3.jpg"]}
```

### Caption a Video

```json
{"videos":["https://example.com/clip.mp4"]}
```

### Caption a Document

```jsonl
{"files":["https://example.com/report.pdf"]}
{"files":["https://example.com/doc1.docx","https://example.com/doc2.txt"]}
```

### Custom Prompt

```json
{"images":["photo.jpg"],"prompt":"Describe the architecture style in detail"}
```

### Save Result

```json
{"images":["photo.jpg"],"output":"result.json"}
```

## 结构化工具参数

| 参数                  | 必填性   | 说明                                                                                         |
| --------------------- | -------- | -------------------------------------------------------------------------------------------- |
| `images`              | 对应工具必填 | workspace 内的图片路径或公开 URL，可传多个                                                |
| `videos`              | 对应工具必填 | 公开视频 URL，可传多个，支持 mp4/mkv/mov                                                |
| `files`               | 对应工具必填 | 公开文档 URL，可传多个，支持 pdf/docx/txt/xlsx/pptx/jsonl                              |
| `prompt`              | 否       | 自定义提示词；默认“请详细描述这张图片的内容”                                               |
| `output`              | 否       | 保存结果 JSON 的路径                                                                        |
| `pretty`              | 否       | 格式化 JSON 输出                                                                            |

三种动态工具只能选择一种，不要混合媒体组。调用 API 前，工具会校验整个批次，并一次返回
可检测到的缺失路径、格式/大小、URL 和模型上限问题；修正完整错误列表后再重试。

## Response Format

```json
{
  "success": true,
  "caption": "A landscape photo showing a mountain range at sunset...",
  "usage": {
    "prompt_tokens": 128,
    "completion_tokens": 256,
    "total_tokens": 384
  },
  "finish_reason": "stop",
  "complete": true
}
```

Key fields:

- `success` — whether the request succeeded
- `caption` — the generated caption text
- `usage` — token usage statistics
- `finish_reason` / `complete` — 模型停止原因与正文是否正常完整；`length` 时必须视为截断
- `warning` — 内容因 token 上限截断或被安全策略拦截时出现
- `error` — error details on failure

## Error Handling

**API key not configured:**

```
未配置 API Key，请填写 skills/glm-master-skill/config.toml 的 [zhipu].api_key
```

→ Show the exact error and guide the user to the shared TOML file

**Authentication failed (401/403):** API key invalid/expired → reconfigure

**Rate limit (429):** Quota exhausted → inform user to wait

**File not found:** Local file missing → check path

**Content filtered:** `warning` field present → content blocked by safety review
