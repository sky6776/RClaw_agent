---
name: seedance-video
description:
  Generate and edit videos using Volcano Ark Doubao Seedance 2.5. Supports
  text-to-video up to 30s with synced audio, first-frame / first-and-last-frame
  to video, omni-reference generation (subject/style/motion/audio from up to 50
  image/video/audio assets), video editing (replace subject, add/remove objects,
  inpainting, audio edits with timestamps), video extension (forward/backward,
  multi-round), draft mode two-step delivery, and chained clips via last-frame
  reuse. Use this skill when the user wants to generate, edit, extend, or remix
  videos, 生成视频, 文生视频, 图生视频, 首帧, 首尾帧, 视频编辑, 视频延长,
  全模态参考, generate video, video to video, edit video.
---

# Seedance 2.5 视频生成技能

> **RClaw 使用说明：** 读取本 Skill 后必须调用 Manifest 动态激活的
> `skill__seedance_video__generate_video`（生成）、`skill__seedance_video__check_task`
> （查询）、`skill__seedance_video__cancel_task`（取消/删除），不得自行选择解释器、
> 脚本路径或参数。API Key 从系列共享的 `skills/ark_key.toml` 读取
> （[ark].api_key，火山方舟系列共用）；请求参数只从
> `skills/seedance-video/config.toml` 的 `[seedance]` 表读取（不读环境变量；
> config 内残留 api_key 会被脚本拒绝）。生成文件必须位于 workspace 的输出目录
> （如 `output/`）。主交付物是本地视频文件；`video_url` 24 小时有效且下载上限
> 100 次，只作诊断信息。外部 channel 交付时，先读取
> `skills/glm-master-skill/SKILL.md`，再调用 `skill__glm_master_skill__publish_media`，
> 传入 `path=<saved_file>` 和 `media_type=video`。运行时会注入真实来源 channel，
> 不要手填或猜测。

调用火山方舟 Doubao Seedance 2.5（Model ID: `doubao-seedance-2-5-260628`）
异步视频生成接口，覆盖该模型全部能力。

**开通条件**：账户余额 > 200 元，或已购 200 元档以上节省计划/Seedance 2.5 资源包。

## When to Use / 使用场景

- 文字生成视频（单段最长 30 秒，含同步音频/台词/音效/BGM）
- 首帧或首尾帧图片生成视频（以首帧为起点/尾帧为目标演化，输出宽高比对齐首帧）
- 全模态参考生视频：主体/风格/动作/运镜/音色参考（图/视频/音频自由组合，上限 30 图 + 10 视频 + 10 音频 = 50 个）；纯音频也能生视频
- 视频编辑：替换视频主体、增删对象、局部画面重绘/修复、字幕水印删除、音频增删改（支持时间戳指定生效时段）
- 视频延长：向前/向后延续，多轮延长成长片
- 样片模式：先 480p 低成本预览，确认后一步出 1080p 正式片
- 多段连续叙事：返回尾帧作为下一段首帧
- User mentions "生成视频", "文生视频", "图生视频", "视频编辑", "改视频", "延长视频",
  "续写视频", "首帧", "首尾帧", "generate video", "edit video", "extend video"

## 任务类型与硬性规则（必读）

Seedance 2.5 按输入素材与提示词意图判定任务类型；配置不一致会**异步报错**（任务
创建成功后才失败，照样计费）。脚本会对显式 `--task-type` 做前置校验和参数自动
锁定（结果里 `parameters_auto_locked` 会列出调整项）：

| 任务类型 | 触发条件 | 特殊限制 |
| ---- | ---- | ---- |
| 文生视频 | 仅文本 | ratio/duration 自由 |
| 首帧/首尾帧 | `--first-frame`（可加 `--last-frame`） | ratio 自动锁定 adaptive，输出对齐首帧；不可与其他参考素材混用 |
| 参考生视频 | ≥1 个 reference 素材（图/视频/音频） | ratio/duration 自由（推荐 adaptive + duration -1） |
| 视频编辑 | ≥1 参考视频 + 编辑意图提示词 | ratio=adaptive、duration=-1（自动锁定）；参考视频须 4-30 秒；输出对齐母版宽高比与时长 |
| 视频延长 | ≥1 参考视频 + 延长意图提示词 | ratio=adaptive（自动锁定）；duration 可指定或 -1 |

编辑提示词须含关键词：**编辑视频 / 增加 / 加上 / 删除 / 删掉 / 去掉 / 修改 / 替换 / 改成**；
延长提示词须含：**向前/向后延长 / 延续 / 续写**。缺关键词时脚本会提示。

**素材引用**：提示词用 `@图片N` / `@视频N` / `@音频N` 指代素材，编号按上传顺序
（图、视频、音频各自独立编号）。多主体时逐条列清映射关系。

**真人人脸限制**：不支持直接上传含真人人脸的参考图/视频；可用模型自家生成的含脸
产物（近 30 天）、预置虚拟人像（`asset://` 素材 ID）或已授权素材。

## How to Use / 使用方法

### 文生视频（30 秒长叙事 + 分时间段）

```json
{"prompt":"写实自然纪录片风格，电影级光影。[0-10秒]：清晨的湿地，一只白鹭单脚伫立在水草间，镜头缓慢推近。[10-20秒]：它猛地俯冲入水，激起水花，叼起一条鱼。[20-30秒]：飞向朝阳，慢镜头剪影收尾。环境音：水声、鸟鸣。","save":"output/egret.mp4","ratio":"16:9","duration":30}
```

### 首帧生视频 / 首尾帧生视频

```json
{"prompt":"女孩转身微笑挥手，镜头缓慢拉远，街景人流虚化","first_frame":"output/frame.jpg","save":"output/greet.mp4"}
```

```json
{"prompt":"花朵从含苞到完全绽放的延时过程，光线渐暖","first_frame":"media/bud.jpg","last_frame":"media/bloom.jpg","save":"output/bloom.mp4"}
```

### 全模态参考生视频（多素材映射）

```json
{"prompt":"广告片风格，果味饼干为主角。主体参考@图片1；开场构图参考@视频1；特写动态参考@视频2；节奏参考@音频1。整体画面干净、高级。","image":["media/cookie.png"],"video":["https://example.com/comp.mp4","https://example.com/motion.mp4"],"audio":["media/bgm.mp3"],"save":"output/ad.mp4"}
```

纯音频参考也可以：只传 `audio` 不传图/视频（人物口播、音乐可视化等）。

### 视频编辑（时间戳 + 替换/增删/修复）

```json
{"prompt":"视频编辑：把 @视频1 中 4-6 秒男人喝咖啡的动作改为拖地，其余内容保持不变。","video":["https://example.com/clip.mov"],"save":"output/edited.mov","task_type":"edit","output_format":"mov"}
```

```json
{"prompt":"编辑任务：删除 @视频1 的背景音乐，加上轻快的爵士乐；把画面右侧的亚洲女生替换为 @图片1 中的女生。","video":["https://example.com/clip.mp4"],"image":["media/girl.png"],"save":"output/remix.mp4","task_type":"edit"}
```

### 视频延长（向前/向后，可多轮）

```json
{"prompt":"向后延长 @视频1：窗户打开后镜头进入美术馆室内，最后进入画作世界，与原片段无缝衔接。","video":["https://example.com/prev.mp4"],"save":"output/extended.mp4","task_type":"extend"}
```

注意：`--video` 只接受 http(s) URL 或 `asset://` 素材 ID（API 无视频 Base64 通道）。
本地视频请先上传到公网存储（如火山引擎 TOS）再提供 URL。

### 样片模式两步出片

```json
{"prompt":"……30 秒复杂叙事……","save":"output/draft.mp4","draft":true}
```

确认样片符合预期后，用返回的 `task_id` 出正式片（仅 1080p；提示词/素材自动复用
不可重传；可改 output_format/watermark/return_last_frame 及任务管理参数
priority/execution_expires_after/safety_identifier；不支持 web_search/mute）：

```json
{"draft_task_id":"cgt-2026****-draft","save":"output/final.mov","output_format":"mov"}
```

### 多段连续生成（尾帧接力）

```json
{"prompt":"……","save":"output/part1.mp4","return_last_frame":true}
```

返回的 `saved_last_frame`（JPEG）作为下一次的 `--first-frame` 传入即可。

### 查询 / 取消任务

```json
{"task_id":"cgt-2026****","save":"output/resumed.mp4"}
```

轮询超时（`POLL_TIMEOUT`）后任务仍在服务端运行，稍后用 `check_task` 补查并下载；
排队中的任务可用 `cancel_task` 取消。

### 联网搜索（时效性内容）

```json
{"prompt":"制作一段 8 秒的科技新闻短视频，展示本周最值得关注的三条 AI 行业动态，新闻字幕风格包装。","save":"output/news.mp4","web_search":true}
```

## 结构化工具参数（generate_video）

| Parameter | Required | Description |
| --------- | -------- | ----------- |
| `prompt` | 文生视频必填 | 提示词；素材职责用 `@图片N/@视频N/@音频N`；编辑/延长须含意图关键词；与 `draft_task_id` 互斥 |
| `save` | 是 | 视频保存路径；扩展名规范化 .mp4/.mov |
| `image` | 否 | 参考图（≤30，URL/asset/本地） |
| `first_frame` / `last_frame` | 否 | 首帧/尾帧图；首尾帧模式与其他参考互斥 |
| `video` | 否 | 参考视频（≤10，单个 2-30s、视频编辑任务母版须 4-30s、总 ≤30s，仅 URL/asset） |
| `audio` | 否 | 参考音频（≤10，单个 2-30s、总 ≤30s，URL/asset/本地 wav、mp3） |
| `resolution` | 否 | `480p`/`720p`/`1080p`（默认 720p；1080p 为 10bit H.265） |
| `ratio` | 否 | `16:9`/`4:3`/`1:1`/`3:4`/`9:16`/`21:9`/`adaptive`（默认 adaptive） |
| `duration` | 否 | `-1` 智能选择（默认）或 4-30 秒 |
| `task_type` | 否 | `auto`/`reference`/`edit`/`extend`，显式指定可前置校验 |
| `mute` | 否 | `true` 无声视频（默认有声） |
| `watermark` | 否 | `true` 加「AI 生成」水印（默认不加） |
| `output_format` | 否 | `mp4`/`mov`（默认 mp4） |
| `return_last_frame` | 否 | `true` 附带尾帧 JPEG |
| `draft` | 否 | `true` 样片模式（固定 480p） |
| `draft_task_id` | 否 | 基于样片出 1080p 正式片 |
| `web_search` | 否 | `true` 开启联网搜索（时效性内容，增加时延；搜索次数见 usage.tool_usage；不适用于 `draft_task_id` 样片转正式） |
| `priority` | 否 | 队列优先级 0-9（越大越先执行，同优先级 FIFO） |
| `execution_expires_after` | 否 | 任务超时阈值秒 [3600, 259200]，默认 48 小时 |
| `safety_identifier` | 否 | 终端用户标识（≤64 字符，建议传哈希），平台政策检测用 |

**不适用的参数**（API 中为 Seedance 1.0 pro 专属，2.5 不支持）：`seed`、`camera_fixed`、
`frames`、`service_tier=flex`。`callback_url` 回调面向 webhook 服务，RClaw 采用轮询模式，
未暴露。

提示词写法与各任务模板见 `skills/sd25-pe/SKILL.md`（官方提示词优化技能）。

## 响应格式

```json
{
  "ok": true,
  "task_id": "cgt-2026****",
  "status": "succeeded",
  "saved_file": "output/egret.mp4",
  "video_url": "https://...（24 小时有效，下载上限 100 次）",
  "duration": 30,
  "framespersecond": 24,
  "resolution": "720p",
  "ratio": "16:9",
  "generate_audio": true,
  "tools": null,
  "usage": {"completion_tokens": ..., "total_tokens": ...},
  "parameters_auto_locked": ["视频编辑任务 ratio 固定 adaptive（已从 16:9 调整）"],
  "error": null
}
```

- `duration` 为约数（总帧数 ÷ 24 向下取整）；编辑任务输出时长与母版基本一致（误差约 0.4 秒内）
- 开启联网搜索时 `tools` 返回实际使用的工具，搜索次数见 `usage.tool_usage.web_search`
- 任务记录保留 7 天；生成通常 1-10 分钟（30 秒 1080p 更久）

## Error Handling / 错误处理

**未配置 Key：** `CONFIG_ERROR` → 填写 `skills/ark_key.toml` 的 `[ark].api_key`

**参数问题（请求前本地拒绝，不计费）：** `INVALID_PARAMS`（首尾帧缺一、模式互斥、
编辑/延长缺参考视频、duration 越界、draft 模式传了禁用参数）、`INVALID_INPUT`
（素材数量/格式/大小超限、本地视频文件、Base64 请求体超 64MB）

**异步任务失败（`TASK_FAILED` 等）：** 任务创建后模型侧校验失败，常见原因是提示词
意图与任务类型不符（如编辑意图但没写编辑关键词）或素材不合规——按 message 调整后重新提交

**轮询超时（`POLL_TIMEOUT`）：** 任务仍在运行，带 `task_id`；稍后调用
`skill__seedance_video__check_task` 补查，或用 `cancel_task` 取消排队任务

**限流（429）：** 个人版 RPM 180 / 并发 3（企业 600/10）；稍后重试

## Prompt Tips / 提示词技巧（速查）

- 一句话概述（主体+地点+事件+风格/运镜），再用时间戳 `[N-M秒]` 或「镜头 N」逐段展开
- 素材职责逐条绑定：`@图片1 是主角形象，@视频1 参考运镜，@音频1 是 BGM`
- 台词放双引号内优化口型与音频：`男人说："你要去哪？"`
- 编辑写清 `A→B` 的变化与保持不变的部分；时间戳指定生效时段
- 反向控制支持字幕/音频：「不要字幕」「不要 bgm」
- 完整方法论见 `skills/sd25-pe/SKILL.md`

## Resource Links / 资源链接

- 教程：[Doubao Seedance 2.5 教程](https://docs.volcengine.com/docs/ark/seedance-2-5)
- 提示词指南：[Seedance 2.5 提示词指南](https://docs.volcengine.com/docs/ark/seedance-2-5-prompt-guide)
- API：[创建任务](https://ark.volcengine.com/docs/ark/create-video-generation-task-api) ·
  [查询任务](https://ark.volcengine.com/docs/ark/get-video-generation-task-api) ·
  [任务列表](https://ark.volcengine.com/docs/ark/list-video-generation-tasks-api) ·
  [取消/删除](https://ark.volcengine.com/docs/ark/cancel-or-delete-video-generation-tasks-api)
- Key 管理：[火山方舟 API Key 管理](https://console.volcengine.com/ark)
