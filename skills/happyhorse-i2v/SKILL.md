---
name: happyhorse-i2v
description:
  Generate videos from a single first-frame image using Alibaba Cloud Bailian HappyHorse-1.1-I2V
  (image-to-video). Accepts a public image URL or a local image file (auto base64 upload) and
  produces physically realistic, motion-smooth MP4 clips (3-15 seconds) with selectable resolution
  and duration; output aspect ratio follows the input image.
  Use when the user wants image-to-video, 让图片动起来, 图生视频, 照片变视频, animate an image,
  or turn a picture into video content.
  Do NOT use for pure text-to-video (use the happyhorse-t2v skill) or static image generation
  (use the glm-image-gen skill).
  Requires network, an Alibaba Cloud API key, paid per-second billing, and a long runtime
  (usually 1-5 minutes per clip); confirm resolution and duration with the user first.
---

# HappyHorse-1.1-I2V 图生视频技能

> **RClaw 使用说明：** 读取本 Skill 后必须调用 Manifest 动态激活的
> `skill__happyhorse_i2v__generate_video`，不得自行选择解释器、脚本路径或参数。
> API Key 从系列共享的 `skills/happyhorse_key.toml` 读取（t2v/i2v/r2v 共用一把 key）；
> 请求参数只从 `skills/happyhorse-i2v/config.toml` 的 `[happyhorse]` 表读取
> （独立配置，不读 glm-master-skill 共享配置，也不读环境变量；config 内残留
> api_key 会被脚本拒绝）。
> 生成文件必须位于 workspace 的输出目录（如 `output/`）。主交付物是生成的本地
> MP4 文件；结果 `video_url` 仅 24 小时有效，只作诊断信息。
> 外部 channel 交付时，先读取 `skills/glm-master-skill/SKILL.md`，再调用
> `skill__glm_master_skill__publish_media`，传入 `path=<saved_file>` 和
> `media_type=video`。运行时会注入真实来源 channel，不要手填或猜测。

以一张首帧图片为起点生成视频（图生视频）。模型精准理解输入图像并延续创作意图，
在人物皮肤质感、ID 跨片段保持、动作流畅度、文字渲染稳定性与音画同步上表现出色。
**输出画幅自动跟随首帧图片**，不支持 ratio 参数。

## When to Use / 使用场景

- 让一张图片动起来 / 图生视频 / 照片变视频
- User mentions "图生视频", "让图片动起来", "照片变视频", "image-to-video", "animate this image"
- 用户给出一张图（本地文件或图片链接）并想看到它动起来的效果

**不要用于**：纯文字生成视频（用 happyhorse-t2v skill）；静态图片生成（用 glm-image-gen）。

## 模型与计费

| 项目 | 说明 |
| ---- | ---- |
| 模型 | `happyhorse-1.1-i2v` |
| 输入 / 输出 | 文本（可选）+ 首帧图片（必选）→ 视频（MP4, H.264, 24fps） |
| 480P | 0.45 元/秒 |
| 720P | 0.9 元/秒 |
| 1080P | 1.2 元/秒 |
| 免费额度 | 不支持开启 |
| 限流 | RPM 300 |

**费用 = 分辨率单价 × 生成秒数**（如 480P、5 秒 ≈ 2.25 元）。生成前先向用户确认
分辨率与时长，避免高额意外消费。

## 首帧图片要求

- **格式**：JPG / JPEG / PNG / WEBP
- **分辨率**：宽和高均 ≥ 300 像素
- **宽高比**：1:2.5 至 2.5:1（输出视频跟随该比例）
- **大小**：不超过 20MB
- **来源**：公网 HTTP(S) URL，或 workspace 内本地图片（脚本自动转 base64 上传）

本地图片注意：base64 会让请求体变大（约为文件体积的 1.33 倍），超大图片建议先压缩。

## How to Use / 使用方法

### 用图片 URL 生成

```json
{"image":"https://example.com/photo.jpg","prompt":"镜头缓缓推近，人物微笑着转过头","save":"output/photo-video.mp4"}
```

### 用本地图片生成

```json
{"image":"input/cat.png","prompt":"猫的耳朵轻轻抖动，尾巴慢慢摇摆","save":"output/cat.mp4"}
```

省略的参数使用 `config.toml` 默认值（当前：480P / 5 秒 / 不加水印）。

### 指定分辨率、时长、水印与种子

```json
{"image":"input/poster.png","prompt":"海报上的文字逐行亮起，光效扫过","save":"output/poster.mp4","resolution":"1080P","duration":10,"watermark":true,"seed":42}
```

`prompt` 是可选的：描述主体动作与镜头运动即可；省略时由模型自行发挥。

## 结构化工具参数

| Parameter    | Required | Description |
| ------------ | -------- | ----------- |
| `image`      | 是       | 首帧图片：公网 URL 或本地路径（jpg/jpeg/png/webp，≥300px，≤20MB） |
| `prompt`     | 否       | 动作/镜头描述，中文上限 2500 字符（超出服务端自动截断） |
| `save`       | 是       | workspace 内的视频保存路径；扩展名自动规范化为 `.mp4` |
| `resolution` | 否       | `480P` / `720P` / `1080P`（画幅自动跟随首帧） |
| `duration`   | 否       | 3-15 秒整数 |
| `watermark`  | 否       | `true` 时添加右下角 "Happy Horse" 水印 |
| `seed`       | 否       | 0-2147483647；固定种子可提升可复现性（不保证完全一致） |

## 响应格式

```json
{
  "ok": true,
  "model": "happyhorse-1.1-i2v",
  "task_id": "…",
  "video_url": "https://...output.mp4?Expires=...",
  "prompt": "...",
  "parameters": {"resolution": "480P", "duration": 5, "watermark": false},
  "usage": {"duration": 5, "video_count": 1, "SR": 480},
  "saved_file": "output/photo-video.mp4",
  "note": "video_url 24 小时内有效，请以 saved_file 为交付物",
  "error": null
}
```

- `usage.duration` 为计费秒数（服务端偶发省略该字段，为 null 时以请求参数为准）
- `video_url` 由 OSS 签名，**24 小时过期**；交付一律使用 `saved_file`
- 提交后任务在服务端异步执行，通常 **1-5 分钟** 完成；脚本会一直轮询直到结束

## Error Handling / 错误处理

**未配置 Key：**

```json
{"ok": false, "error": {"code": "CONFIG_ERROR", "message": "未配置 API Key，请填写 skills/happyhorse_key.toml 的 [happyhorse].api_key"}}
```

→ 引导用户填写 `skills/happyhorse_key.toml` 的 `[happyhorse].api_key`（t2v/i2v/r2v 系列共享）

**图片问题（`INVALID_IMAGE`）：** 本地文件不存在、格式不支持（需 jpg/jpeg/png/webp）或超过 20MB → 换图或先压缩；服务端还会校验 ≥300 像素与宽高比范围

**鉴权失败（401/403）：** `InvalidApiKey` / `AccessDenied` → Key 无效或无该模型权限

**参数越界：** `duration` 超出 3-15、`resolution` 取值非法会在请求前被拒绝（Manifest 校验），不产生费用

**任务失败（`TASK_FAILED`）：** 把 `message` 原样展示；常见原因是图片或提示词触发内容风控

**轮询超时（`POLL_TIMEOUT`）：** 任务仍在服务端运行，错误里带 `task_id`；结果链接 24 小时内有效

## Prompt Tips / 提示词技巧

图生视频的提示词只需描述"图里没有的动态"：

- **动作**：「头发随风轻扬」「猫的耳朵抖动」「人物转头微笑」
- **镜头**：「镜头缓缓推近」「环绕镜头」「轻微手持晃动」
- **氛围**：「光影流动」「花瓣飘落」「霓虹闪烁」
- 避免：重新描述图片已有的静态内容、要求画面主体大幅位移（首帧约束下易变形）、要求生成文字

## Resource Links / 资源链接

- API 文档：[HappyHorse 图生视频 API 参考](https://help.aliyun.com/zh/model-studio/happyhorse-image-to-video-api-reference)
- 模型页：百炼控制台 → 模型广场 → HappyHorse-1.1-I2V
- Key 管理：[百炼控制台 API-KEY 页面](https://bailian.console.aliyun.com/?apiKey=1)
