---
name: happyhorse-t2v
description:
  Generate videos from text prompts using Alibaba Cloud Bailian HappyHorse-1.1-T2V (text-to-video).
  Produces physically realistic, motion-smooth MP4 clips (3-15 seconds) with selectable resolution
  (480P/720P/1080P), aspect ratio, duration, watermark and seed. Use this skill when the user wants
  to generate a video, create a video from text, 文生视频, AI 生成视频, or turn a text description
  into video content.
---

# HappyHorse-1.1-T2V 视频生成技能

> **RClaw 使用说明：** 读取本 Skill 后必须调用 Manifest 动态激活的
> `skill__happyhorse_t2v__generate_video`，不得自行选择解释器、脚本路径或参数。
> API Key 从系列共享的 `skills/happyhorse_key.toml` 读取（t2v/i2v/r2v 共用一把 key）；
> 请求参数只从 `skills/happyhorse-t2v/config.toml` 的 `[happyhorse]` 表读取
> （独立配置，不读 glm-master-skill 共享配置，也不读环境变量；config 内残留
> api_key 会被脚本拒绝）。
> 生成文件必须位于 workspace 的输出目录（如 `output/`）。主交付物是生成的本地
> MP4 文件；结果 `video_url` 仅 24 小时有效，只作诊断信息。
> 外部 channel 交付时，先读取 `skills/glm-master-skill/SKILL.md`，再调用
> `skill__glm_master_skill__publish_media`，传入 `path=<saved_file>` 和
> `media_type=video`。运行时会注入真实来源 channel，不要手填或猜测。

调用阿里云百炼 HappyHorse-1.1-T2V，从文本提示词生成 MP4 视频。
模型主打物理真实、运动流畅：文本语义理解、镜头调度与动态生成表现强，
在人物动作、场景氛围、视觉美感和物理运动上细节丰富、一致性高。

## When to Use / 使用场景

- 从文字描述生成视频 / 从一段场景描写生成短片
- User mentions "生成视频", "文生视频", "AI 视频", "generate video", "text-to-video"
- 用户提供场景描述并想看到动态视频效果

**不要用于**：图生视频（本模型只接受文本输入；图生视频请用 happyhorse-1.1-i2v 等，本 skill 未接入）、静态图片（用 glm-image-gen）。

## 模型与计费

| 项目 | 说明 |
| ---- | ---- |
| 模型 | `happyhorse-1.1-t2v` |
| 输入 / 输出 | 文本 → 视频（MP4, H.264） |
| 480P | 0.45 元/秒 |
| 720P | 0.9 元/秒 |
| 1080P | 1.2 元/秒 |
| 免费额度 | 不支持开启 |
| 限流 | RPM 300 |

**费用 = 分辨率单价 × 生成秒数**（如 480P、5 秒 ≈ 2.25 元）。生成前先向用户确认
分辨率与时长，避免高额意外消费。

## How to Use / 使用方法

```json
{"prompt":"一座由硬纸板和瓶盖搭建的微型城市，在夜晚焕发出生机。一列硬纸板火车缓缓驶过，小灯点缀其间。","save":"output/cardboard-city.mp4"}
```

省略的参数使用 `config.toml` 默认值（当前：480P / 16:9 / 5 秒 / 不加水印）。

### 指定分辨率、比例与时长

```json
{"prompt":"雪后的故宫角楼，无人机镜头缓缓拉升","save":"output/gugong.mp4","resolution":"1080P","ratio":"21:9","duration":10}
```

### 加水印与固定种子

```json
{"prompt":"...","save":"output/demo.mp4","watermark":true,"seed":42}
```

## 结构化工具参数

| Parameter    | Required | Description |
| ------------ | -------- | ----------- |
| `prompt`     | 是       | 非空视频描述，任意语言，中文上限 2500 字符（超出服务端自动截断） |
| `save`       | 是       | workspace 内的视频保存路径；扩展名自动规范化为 `.mp4` |
| `resolution` | 否       | `480P` / `720P` / `1080P` |
| `ratio`      | 否       | `16:9` / `9:16` / `1:1` / `4:3` / `3:4` / `4:5` / `5:4` / `9:21` / `21:9` |
| `duration`   | 否       | 3-15 秒整数 |
| `watermark`  | 否       | `true` 时添加右下角 "Happy Horse" 水印 |
| `seed`       | 否       | 0-2147483647；固定种子可提升可复现性（不保证完全一致） |

## 响应格式

```json
{
  "ok": true,
  "model": "happyhorse-1.1-t2v",
  "task_id": "1549b10c-...",
  "video_url": "https://...output.mp4?Expires=...",
  "prompt": "...",
  "parameters": {"resolution": "480P", "ratio": "16:9", "duration": 5, "watermark": false},
  "usage": {"duration": 5, "video_count": 1, "SR": 480, "ratio": "16:9"},
  "saved_file": "output/cardboard-city.mp4",
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

**鉴权失败（401/403）：** `code` 为 `InvalidApiKey` / `AccessDenied` → Key 无效或无该模型权限，请用户检查百炼控制台

**参数越界：** `duration` 超出 3-15、`ratio`/`resolution` 取值非法会在请求前被拒绝（`INVALID_PROMPT` 或 Manifest 校验），无需发起付费请求

**任务失败：**

```json
{"ok": false, "error": {"code": "TASK_FAILED", "message": "...", "task_id": "...", "task_status": "FAILED"}}
```

→ 把 `message` 原样展示给用户；常见原因是提示词触发内容风控，调整措辞后重试

**轮询超时（`POLL_TIMEOUT`）：** 任务仍在服务端运行，错误里带 `task_id`；告知用户结果链接 24 小时内有效，可稍后在体验中心查看，或减少时长/分辨率后重试

**内容风控（`InvalidParameter` / 风控类 message）：** 提示词含敏感内容被拒，改写提示词

## Prompt Tips / 提示词技巧

视频提示词比图片更需要写清"运动"：

- **主体 + 动作**：「一列纸板小火车缓缓驶过」（动作要具体：驶过、拉升、奔跑、绽放）
- **场景与氛围**：「夜晚的微型城市，小灯点缀其间」
- **镜头语言**：「无人机镜头缓缓拉升」「特写」「跟随镜头」「缓慢推近」
- **物理与质感**：「水花飞溅」「布料飘动」「硬纸板质感」
- 避免：多主体频繁切换、要求文字字幕（模型不擅长渲染文字）、单句堆砌过多场景

## Resource Links / 资源链接

- API 文档：[HappyHorse 文生视频 API 参考](https://help.aliyun.com/zh/model-studio/happyhorse-text-to-video-api-reference)
- 模型页：百炼控制台 → 模型广场 → HappyHorse-1.1-T2V
- Key 管理：[百炼控制台 API-KEY 页面](https://bailian.console.aliyun.com/?apiKey=1)
