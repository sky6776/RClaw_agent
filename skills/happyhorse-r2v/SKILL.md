---
name: happyhorse-r2v
description:
  Generate videos from 1-9 reference images plus a text prompt using Alibaba Cloud Bailian
  HappyHorse-1.1-R2V (reference-to-video). The prompt references each image as [Image 1],
  [Image 2], ... in submission order to control subjects, scenes and styles; aspect ratio is
  an explicit parameter (not derived from images). Accepts public image URLs or local image
  files (auto base64 upload); produces MP4 clips (3-15 seconds).
  Use when the user wants reference-to-video, 参考生视频, 多图合成视频, 用这几张图生成视频,
  keep the same character/outfit across shots, or combine several images into one video.
  Do NOT use for a single first-frame image animation (use the happyhorse-i2v skill),
  pure text-to-video (use the happyhorse-t2v skill), or static image generation
  (use the glm-image-gen skill).
  Requires network, an Alibaba Cloud API key, paid per-second billing, and a long runtime
  (usually 1-5 minutes per clip); confirm resolution, ratio and duration with the user first.
---

# HappyHorse-1.1-R2V 参考生视频技能

> **RClaw 使用说明：** 读取本 Skill 后必须调用 Manifest 动态激活的
> `skill__happyhorse_r2v__generate_video`，不得自行选择解释器、脚本路径或参数。
> API Key 从系列共享的 `skills/happyhorse_key.toml` 读取（t2v/i2v/r2v 共用一把 key）；
> 请求参数只从 `skills/happyhorse-r2v/config.toml` 的 `[happyhorse]` 表读取
> （独立配置，不读 glm-master-skill 共享配置，也不读环境变量；config 内残留
> api_key 会被脚本拒绝）。
> 生成文件必须位于 workspace 的输出目录（如 `output/`）。主交付物是生成的本地
> MP4 文件；结果 `video_url` 仅 24 小时有效，只作诊断信息。
> 外部 channel 交付时，先读取 `skills/glm-master-skill/SKILL.md`，再调用
> `skill__glm_master_skill__publish_media`，传入 `path=<saved_file>` 和
> `media_type=video`。运行时会注入真实来源 channel，不要手填或猜测。

以 1-9 张参考图加提示词合成视频（参考生视频）。模型稳定保持主体、场景风格与画面
一致性：把人物、服装、道具、场景分别用参考图提供，再用 prompt 描述镜头与动作。
与 i2v 不同，**ratio 由参数显式指定**，不跟随参考图。

## When to Use / 使用场景

- 多张参考图合成一段视频 / 参考生视频 / 多图合成视频
- 保持同一人物、服装或道具的跨镜头一致性（如"让 [Image 1] 的人用 [Image 2] 的扇子"）
- User mentions "参考生视频", "多图合成视频", "用这几张图生成视频", "reference-to-video"

**不要用于**：单张首帧图动画（用 happyhorse-i2v skill）、纯文字生成视频（用 happyhorse-t2v skill）、静态图片（用 glm-image-gen）。

## 模型与计费

| 项目 | 说明 |
| ---- | ---- |
| 模型 | `happyhorse-1.1-r2v` |
| 输入 / 输出 | 文本（必填，含 `[Image N]` 引用）+ 1-9 张参考图 → 视频（MP4, H.264） |
| 480P | 0.45 元/秒 |
| 720P | 0.9 元/秒 |
| 1080P | 1.2 元/秒 |
| 免费额度 | 不支持开启 |
| 限流 | RPM 300 |

**费用 = 分辨率单价 × 生成秒数**（如 480P、5 秒 ≈ 2.25 元），与参考图数量无关。
生成前先向用户确认分辨率与时长，避免高额意外消费。

## 参考图要求

- **数量**：1-9 张；`--image` 顺序即 prompt 中 `[Image 1]`、`[Image 2]` 的编号
- **格式**：JPG / JPEG / PNG / WEBP
- **分辨率**：短边 ≥ 400 像素（推荐 720P 以上素材）
- **大小**：单张不超过 20MB
- **来源**：公网 HTTP(S) URL，或 workspace 内本地图片（脚本自动转 base64 上传）

## How to Use / 使用方法

### 用图片 URL 生成

```json
{"prompt":"[Image 1]中身着红色旗袍的女性缓缓展开[Image 2]中的折扇，镜头推近至面部特写","image":["https://example.com/qipao.jpg","https://example.com/fan.jpg"],"save":"output/r2v-demo.mp4"}
```

### 用本地图片生成（可与 URL 混用）

```json
{"prompt":"[Image 1]中的人物穿上[Image 2]中的盔甲，转身面对镜头","image":["input/portrait.png","input/armor.png"],"save":"output/knight.mp4"}
```

省略的参数使用 `config.toml` 默认值（当前：480P / 16:9 / 5 秒 / 不加水印）。

### 指定比例、分辨率、时长、水印与种子

```json
{"prompt":"[Image 1]中的角色在[Image 2]的场景中奔跑，镜头跟随","image":["input/role.png","input/scene.png"],"save":"output/run.mp4","ratio":"21:9","resolution":"1080P","duration":10,"watermark":true,"seed":42}
```

`prompt` 必填且必须用 `[Image N]` 指代参考图并写明动作与运镜，这是 R2V 的核心用法；
只传图不写引用关系会导致模型无法理解各图的作用。

## 结构化工具参数

| Parameter    | Required | Description |
| ------------ | -------- | ----------- |
| `prompt`     | 是       | 用 `[Image N]` 引用参考图并描述动作运镜，中文上限 2500 字符 |
| `image`      | 是       | 参考图列表，重复传 1-9 个；顺序即 `[Image N]` 编号；URL 或本地路径 |
| `save`       | 是       | workspace 内的视频保存路径；扩展名自动规范化为 `.mp4` |
| `resolution` | 否       | `480P` / `720P` / `1080P` |
| `ratio`      | 否       | `16:9` / `9:16` / `1:1` / `4:3` / `3:4` / `4:5` / `5:4` / `9:21` / `21:9`（显式指定） |
| `duration`   | 否       | 3-15 秒整数 |
| `watermark`  | 否       | `true` 时添加右下角 "Happy Horse" 水印 |
| `seed`       | 否       | 0-2147483647；固定种子可提升可复现性（不保证完全一致） |

## 响应格式

```json
{
  "ok": true,
  "model": "happyhorse-1.1-r2v",
  "task_id": "…",
  "video_url": "https://...output.mp4?Expires=...",
  "prompt": "...",
  "parameters": {"resolution": "480P", "ratio": "16:9", "duration": 5, "watermark": false},
  "usage": {"duration": 5, "video_count": 1, "SR": 480, "ratio": "16:9"},
  "saved_file": "output/r2v-demo.mp4",
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

**图片问题（`INVALID_IMAGE`）：** 数量超过 9 张、本地文件不存在、格式不支持（需 jpg/jpeg/png/webp）或超过 20MB → 请求前被拦截；服务端还会校验短边 ≥400 像素

**prompt 缺失或无 [Image N]（`INVALID_PROMPT`）：** r2v 的 prompt 必填；只传图不写引用会让模型无法理解各图作用 → 帮用户补写引用关系

**鉴权失败（401/403）：** `InvalidApiKey` / `AccessDenied` → Key 无效或无该模型权限

**参数越界：** `duration` 超出 3-15、`resolution`/`ratio` 取值非法会在请求前被拒绝（Manifest 校验），不产生费用

**任务失败（`TASK_FAILED`）：** 把 `message` 原样展示；常见原因是图片或提示词触发内容风控

**轮询超时（`POLL_TIMEOUT`）：** 任务仍在服务端运行，错误里带 `task_id`；结果链接 24 小时内有效

## Prompt Tips / 提示词技巧

R2V 提示词的核心是"谁/什么来自哪张图 + 做什么动作 + 什么镜头"：

- **引用清晰**："[Image 1]中的女性""[Image 2]中的折扇"——每张图都要有明确身份
- **动作与运镜**："镜头先以侧面中景…随即切换至低角度仰拍…最后推近至面部特写"
- **一致性**：人物、服装、道具、场景分别用不同参考图提供，模型会保持 ID 与风格
- 避免：引用了不存在的 [Image N]、所有图塞进一句无引用关系的话、要求生成大段文字

## Resource Links / 资源链接

- API 文档：[HappyHorse 参考生视频 API 参考](https://help.aliyun.com/zh/model-studio/happyhorse-reference-to-video-api-reference)
- 模型页：百炼控制台 → 模型广场 → HappyHorse-1.1-R2V
- Key 管理：[百炼控制台 API-KEY 页面](https://bailian.console.aliyun.com/?apiKey=1)
