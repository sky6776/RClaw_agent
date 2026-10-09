---
name: glmv-prompt-gen
description:
  Analyze images/videos and generate professional prompts for text-to-image and
  text-to-video AI tools (Midjourney, Stable Diffusion, DALL-E, Sora, Runway, Kling, Pika).
  Use when the user wants to generate prompts from reference images/videos, create
  AI art prompts, or get prompt engineering suggestions from visual content. Do not
  process an attachment alone; ask first when the current message and recent context lack a goal.
---

# GLM-V Prompt Generation Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后，根据媒体类型调用
> `skill__glmv_prompt_gen__prompt_from_images` 或 `skill__glmv_prompt_gen__prompt_from_videos`，
> 不得自行选择解释器、脚本路径或参数。API Key 和模型请求设置只从
> `skills/glm-master-skill/config.toml` 的 `[zhipu]`、`[glm5v]`、`[prompt_gen]` 读取。
> 宿主 Agent 不得自行推导视觉提示词，只使用匹配动态工具返回的文本。本地视频先读取
> `skills/glm5v-multimodal/SKILL.md`，再调用 `skill__glm5v_multimodal__analyze_videos`，
> 并给出明确的文生图/文生视频提示词生成要求。RClaw 会注入真实来源 channel，不要手填或猜测。
> 用户要求把保存的 `output` 作为外部附件时，先读取 `skills/glm-master-skill/SKILL.md`，
> 再调用 `skill__glm_master_skill__publish_media`，传入实际 `path` 和 `media_type=file`。
> 工作目录已经是 workspace，不要添加 `agent/` 前缀。RClaw 会校验并自动保留有效
> `MEDIA_MARKER=`；不得手写或复制标记。没有有效标记时，只报告本地文件路径，不得声称已发送。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确要生成图片提示词、视频提示词
> 或指定平台格式时，不调用动态工具、不读取或上传媒体、不调用 API；先询问用户。目标明确时直接执行。

Analyze reference images or videos and generate professional prompts for AI image/video generation tools.

## When to Use

- Generate prompts for text-to-image tools (Midjourney, Stable Diffusion, DALL-E, etc.)
- Generate prompts for text-to-video tools (Sora, Runway, Kling, Pika, etc.)
- User mentions "生成prompt", "文生图prompt", "文生视频prompt", "prompt工程", "参考图生成prompt", "generate prompt"
- User provides an image/video and wants to recreate or remix it
- Extract prompt ideas from reference visual content

## Supported Input Types

| Type  | Formats        | Max Size          | Max Count | Base64        |
| ----- | -------------- | ----------------- | --------- | ------------- |
| Image | jpg, png, jpeg | 5MB / 6000×6000px | 50        | ❌            |
| Video | mp4, mkv, mov  | 200MB             | —         | ❌ (URL only) |

> ⚠️ Images and videos cannot be used in the same request.
> ⚠️ 动态图片工具接受本地路径或 HTTP(S) URL；视频工具只接受 HTTP(S) URL。两个入口都不接受内联 base64 参数。

### 📋 Output Display Rules (MANDATORY)

After the selected dynamic tool returns, **you must display the full prompt output exactly as returned**. Do not summarize, truncate, or only say "prompt generated". Users need the complete prompt (especially the English prompt) for direct copy/paste.

- Show the full output: content analysis + prompt + prompt breakdown
- In `auto` mode, show both text-to-image and text-to-video prompts
- English prompts are core output and must be shown completely
- If the `output` field was provided, give the saved file path and show its content

## Output Modes

| Mode    | Description                                        |
| ------- | -------------------------------------------------- |
| `image` | Generate prompts for text-to-image tools (default) |
| `video` | Generate prompts for text-to-video tools           |
| `auto`  | Generate prompts for both image and video          |

## Resource Links

| Resource        | Link                                                                                                                              |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **Get API Key** | [https://bigmodel.cn/usercenter/proj-mgmt/apikeys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys)                              |
| **API Docs**    | [Chat Completions / 对话补全](https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E5%AF%B9%E8%AF%9D%E8%A1%A5%E5%85%A8) |

## Prerequisites

### API Key Setup / API Key 配置（Required / 必需）

This script reads all model settings from `skills/glm-master-skill/config.toml` and does not read model configuration from environment variables.
脚本从 `skills/glm-master-skill/config.toml` 读取全部模型配置，不从环境变量读取模型配置。

**Get Key / 获取 Key：** Visit [Zhipu Open Platform API Keys / 智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys) to create or copy your key.

Fill `[zhipu].api_key` in the shared TOML. Do not pass the key in command arguments or
write it to logs or generated artifacts.

## How to Use

### Image → Text-to-Image Prompt

```jsonl
{"images":["https://example.com/photo.jpg"]}
{"images":["input/photo.png"]}
```

### Image → Text-to-Video Prompt

```json
{"images":["https://example.com/scene.jpg"],"mode":"video"}
```

### Image → Both (Image + Video Prompts)

```json
{"images":["https://example.com/photo.jpg"],"mode":"auto"}
```

### Video → Text-to-Video Prompt

```json
{"videos":["https://example.com/clip.mp4"],"mode":"video"}
```

### Save Result to File

```json
{"images":["photo.jpg"],"mode":"image","output":"prompt.md"}
```

## Output Example (image mode)

```
### Content Analysis
A cyberpunk cityscape at night, with dense skyscrapers, glowing neon signs, and rain-wet streets reflecting colorful light.

### Prompt
Cyberpunk cityscape at night, towering skyscrapers with glowing neon signs,
rain-wet streets reflecting colorful lights, flying cars in the distance,
volumetric fog, dramatic lighting, ultra detailed, 8K, cinematic composition

### Prompt Breakdown
- **Subject**: Futuristic skyline with skyscrapers and neon lights
- **Style**: Cyberpunk, sci-fi
- **Color**: Cool/warm contrast with blue-purple dominance and neon accents
- **Lighting**: Neon glow, wet-surface reflections, volumetric fog
- **Composition**: Wide-angle perspective with layered depth
- **Mood**: Mysterious, futuristic, high-tech
```

## 结构化工具参数

| Parameter             | Required | Description                                        |
| --------------------- | -------- | -------------------------------------------------- |
| `images`              | 对应工具必填 | workspace 内的图片路径或公开 URL，支持 jpg/png/jpeg |
| `videos`              | 对应工具必填 | 公开视频 URL，支持 mp4/mkv/mov                    |
| `mode`                | 否       | 输出模式：`image`（默认）、`video` 或 `auto`       |
| `output`              | 否       | 保存结果的文件路径                                  |
| `pretty`              | 否       | 格式化 JSON 错误输出                                |

Before encoding or calling the API, the script validates the complete selected batch and
returns every detectable missing path, bad format/size, invalid URL, and model-limit issue
in one error response. Fix the full list before rerunning.

动态工具返回值包含 `finish_reason` 与 `complete`；底层入口在 `complete=false` 时把警告写到 stderr。不得把 `finish_reason=length` 的提示词描述为完整结果。

## Error Handling

**API key not configured:** → Guide user to `[zhipu].api_key` in the shared TOML

**Authentication failed (401/403):** → API key invalid/expired → check at [Zhipu API Keys / 智谱官网](https://bigmodel.cn/usercenter/proj-mgmt/apikeys)

**Rate limit (429):** → Quota exhausted → wait and retry

**Output incomplete:** → stderr 出现 `Warning`，按 `finish_reason` 说明 token 截断或安全拦截

**Timeout:** → Video processing may take time → increase timeout or use smaller files
