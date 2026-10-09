---
name: glm-image-gen
description:
  Official skill for generating high-quality images from text prompts using ZhiPu GLM-Image API.
  Excellent at scientific illustrations, high-quality portraits, social media graphics, and commercial posters.
  Supports multiple aspect ratios, HD quality, and watermark control.
  Use this skill when the user wants to generate images, create AI art, text-to-image,
  or convert text descriptions into visual content.
---

# GLM-Image Generation Skill / GLM-Image 图片生成技能

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后必须调用
> Manifest 动态激活的 `skill__glm_image_gen__generate_image`，不得自行选择解释器、脚本路径或参数。
> API Key 和 GLM-Image 请求设置只从
> `skills/glm-master-skill/config.toml` 的 `[zhipu]`、`[glm_image]` 读取。生成文件必须位于
> workspace 的输出目录（如 `output/`），不要添加 `agent/` 前缀。只有发布工具负责把实际
> 成品复制到共享媒体沙箱，避免重复或混淆的预暂存文件。主交付物是生成的图片文件。
> 运行时会注入真实来源 channel，不要手填或猜测。外部 channel 交付时，先读取
> `skills/glm-master-skill/SKILL.md`，再调用 `skill__glm_master_skill__publish_media`，
> 传入 `path=<saved_file>` 和 `media_type=image`。RClaw 会校验并自动保留有效
> `MEDIA_MARKER=`；不得依据请求中的 `save` 路径手写或复制媒体标记。

Generate high-quality images from text prompts using the ZhiPu GLM-Image API.

## When to Use / 使用场景

- Generate images from text descriptions / 从文字描述生成图片
- Create AI art, illustrations, or concept art / 创作 AI 艺术、插画或概念图
- User mentions "生图", "文生图", "AI 绘画", "generate image", "text-to-image", "create image"
- User provides a prompt and wants to see it visualized / 用户提供描述并想看到可视化效果

## Key Features / 核心特性

- **High-quality generation**: HD mode produces more detailed, refined images (~20s)
- **Multiple aspect ratios**: Square, portrait, landscape formats supported
- **GLM-Image model**: Latest model with improved understanding and quality
- **Excellent at**: Scientific illustrations (科普插画), high-quality portraits (高质量人像), social media graphics (社交媒体图文), commercial posters (商业海报)
- **Watermark control**: Enable/disable watermarks (requires signed disclaimer for no-watermark)
- **Content safety**: Built-in content filtering for compliance

## Resource Links / 资源链接

| Resource        | Link                                                                      |
| --------------- | ------------------------------------------------------------------------- |
| **Get API Key** | [智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys) |
| **API Docs**    | [Image Generation / 图像生成](https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E5%9B%BE%E5%83%8F%E7%94%9F%E6%88%90) |
| **Model Docs**  | [GLM-Image 模型文档](https://docs.bigmodel.cn/cn/guide/models/image-generation/glm-image) |

## Prerequisites / 前置条件

### API Key Setup / API Key 配置（Required / 必需）

脚本从 `skills/glm-master-skill/config.toml` 读取全部模型配置，不从环境变量读取模型配置。
This script reads all model settings from the shared TOML and does not read model configuration from environment variables.

**Get Key / 获取 Key：** Visit [智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys) to create or copy your key.

Fill `[zhipu].api_key` in the shared TOML. Do not pass it in command arguments or write
it to logs or generated artifacts.

## Security & Transparency / 安全与透明度

- **Configuration source / 配置来源：** `skills/glm-master-skill/config.toml`
- **Endpoint / 接口：** `[glm_image].api_url`

**⛔ MANDATORY RESTRICTIONS / 强制限制 ⛔**

1. **只能使用 GLM-Image API**：调用 `skill__glm_image_gen__generate_image`
2. **NEVER generate images yourself** — Do NOT try to create images using built-in vision or any other method
3. **NEVER offer alternatives** — Do NOT suggest "I can try to describe it" or similar
4. **IF API fails** — Display the error message and STOP immediately
5. **NO fallback methods** — Do NOT attempt image generation any other way

### 📋 Output Display Rules / 输出展示规则

After `skill__glm_image_gen__generate_image` returns, present the generation result clearly.

- Deliver the local image from `saved_file`; the temporary URL is diagnostic metadata only
- Display the prompt used and generation parameters (size, quality)
- If content_filter indicates issues, show the warning level

**⚠️ Image Display / 图片展示注意：**

The API returns a temporary image URL (valid for 30 days). You should:

1. **Show the image** — Use the URL to display the image in the chat (if platform supports it)
2. **Remind user to save** — "图片链接有效期 30 天，请及时下载保存"
3. **按来源 channel 发布**：读取工具 JSON 结果中的实际 `saved_file`（下载器可能把
   `.png` 规范化为 `.jpg`）。读取 `skills/glm-master-skill/SKILL.md`，再把该文件传给
   `skill__glm_master_skill__publish_media`，设置 `media_type=image`。省略 `name`，
   以保留规范化后的真实文件名和扩展名。
   RClaw 会注入真实来源 channel，不要手填或猜测。它会校验每行 `MEDIA_MARKER=` 并自动保留
   有效标记。不得复用请求中的 `save` 路径替代工具返回路径，不得缩短发布器返回的绝对路径，
   也不得手写任何 channel 的媒体标记。没有有效 `MEDIA_MARKER=` 时，只报告本地产物路径，
   不得声称已发送。

## How to Use / 使用方法

### Generate from Prompt / 从提示词生成

```json
{"prompt":"一只可爱的小猫咪，坐在阳光明媚的窗台上，背景是蓝天白云","save":"output/cat.png"}
```

尺寸、质量、水印和模型在共享 TOML 的 `[glm_image]` 中配置。

### Save Image to Local File / 保存图片到本地

```jsonl
{"prompt":"中国水墨画风格","save":"output/image.png"}
{"prompt":"赛博朋克城市","save":"output/cyberpunk.png"}
```

### Specify User ID (for content moderation) / 指定用户 ID（用于内容审核）

```json
{"prompt":"...","user_id":"user_12345","save":"output/image.png"}
```

## 结构化工具参数

| Parameter        | Required | Description                                                      |
| ---------------- | -------- | ---------------------------------------------------------------- |
| `prompt`         | 是       | 非空图片描述，最多 1000 个字符                                  |
| `user_id`        | 否       | 用于内容审核的终端用户 ID，长度 6–128 个字符                    |
| `save`           | **是**   | workspace 内的本地图片交付路径                                  |

## Supported Sizes / 支持的尺寸

**GLM-Image recommended sizes:**

| Size          | Aspect Ratio | Use Case                  |
| ------------- | ------------ | ------------------------- |
| 1280x1280     | 1:1          | Square (default)          |
| 1568×1056     | 3:2          | Landscape / 横向          |
| 1056×1568     | 2:3          | Portrait / 纵向           |
| 1472×1088     | ~4:3         | Wide landscape            |
| 1088×1472     | ~3:4         | Tall portrait             |
| 1728×960      | 16:9         | Ultra-wide landscape      |
| 960×1728      | 9:16         | Ultra-tall portrait       |

**Custom sizes / 自定义尺寸:**

- Width and height: 512px - 2048px
- Both dimensions must be multiples of 32 / 长宽均需为 32 的整数倍
- Maximum total pixels: 2^22 (4,194,304 px) / 最大像素数不超过 2^22

## Response Format / 响应格式

**Official API Response:**

```json
{
  "created": 123,
  "data": [
    {
      "url": "<string>"
    }
  ],
  "content_filter": [
    {
      "role": "assistant",
      "level": 1
    }
  ]
}
```

**Dynamic Tool Output Format:**

```json
{
  "ok": true,
  "model": "glm-image",
  "image_url": "https://open.bigmodel.cn/.../generated_image.png",
  "prompt": "一只可爱的小猫咪，坐在阳光明媚的窗台上，背景是蓝天白云",
  "size": "1280x1280",
  "quality": "hd",
  "created": 1710835200,
  "content_filter": [
    {
      "role": "assistant",
      "level": 3
    }
  ],
  "saved_file": "/Users/xxx/image.png",
  "error": null
}
```

Key fields:

- `ok` — whether generation succeeded
- `model` — model used for generation
- `image_url` — extracted from `data[0].url`, temporary URL (valid 30 days)
- `prompt` — the text prompt used
- `size` — generated image dimensions
- `quality` — hd or standard
- `created` — Unix timestamp when request was created
- `content_filter` — content safety analysis array (may be empty)
  - `role`: where the issue was detected (user/assistant/history)
  - `level`: severity 0-3 (0 = most severe, 3 = minor)
- `saved_file` — absolute path to the required local image deliverable
- `error` — error details on failure

## Content Safety / 内容安全

The API includes content filtering. If issues are detected, `content_filter` will contain entries with:

- `role`: where the issue was detected (user/assistant/history)
- `level`: severity 0-3 (0 = most severe, 3 = minor)

**If level 0-1 detected:** Generation will fail, show error to user.
**If level 2-3 detected:** Generation may succeed, but warn user about potential issues.

## Error Handling / 错误处理

**API key not configured:**

```json
{
  "ok": false,
  "error": {
    "code": "CONFIG_ERROR",
    "message": "未配置 API Key，请填写 skills/glm-master-skill/config.toml 的 [zhipu].api_key"
  }
}
```

→ Show exact error to user and guide them to the shared TOML

**Authentication failed (401/403):**

```json
{
  "ok": false,
  "error": {
    "code": "authentication_error",
    "message": "令牌已过期或验证不正确",
    "status": 401
  }
}
```

→ API key invalid/expired → reconfigure

**Rate limit (429):**

```json
{
  "ok": false,
  "error": {
    "code": "rate_limit_exceeded",
    "message": "API rate limit exceeded. Please try again later.",
    "status": 429
  }
}
```

→ Quota exhausted → inform user to wait or check quota

**Content filter violation:**

```json
{
  "ok": false,
  "error": {
    "code": "content_filter_violation",
    "message": "Content safety check failed",
    "status": 400
  }
}
```

→ Explain that the prompt may contain inappropriate content

**Invalid size:**

```json
{
  "ok": false,
  "error": {
    "code": "INVALID_SIZE",
    "message": "Invalid size: 500x500 for model glm-image. Must be multiple of 32, 512-2048px, max 2^22 pixels"
  }
}
```

→ Guide user to use valid size from the supported list

**Download failed:**

```json
{
  "ok": false,
  "error": {
    "code": "DOWNLOAD_FAILED",
    "message": "Failed to download image to image.png"
  }
}
```

→ Check file path permissions and disk space

**Network error:**

```json
{
  "ok": false,
  "error": {
    "code": "NETWORK_ERROR",
    "message": "Network error: [Errno 8] nodename nor servname provided, or not known"
  }
}
```

→ Check internet connection

## Prompt Tips / 提示词技巧

**Good prompts:**

- Specific details: "一只橘色的英国短毛猫，绿色眼睛，坐在木质窗台上"
- Style keywords: "赛博朋克风格", "中国水墨画", "油画质感", "3D 渲染"
- Lighting: "阳光明媚", "柔和的逆光", "电影感灯光"
- Composition: "特写镜头", "广角视角", "俯视角度"

**Avoid:**

- Vague descriptions: "好看的图片"
- Contradictory elements: "白天和夜晚同时"
- Too many subjects: Keep focus on 1-2 main elements
