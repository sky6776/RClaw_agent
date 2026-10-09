---
name: glm5v-multimodal
description: >
  使用 Manifest 动态工具调用 GLM-5V-Turbo，对图片、本地/远程视频、Office/PDF/文本文件执行不属于
  专用工作流的通用多模态分析，例如视觉问答、多图对比、图表解读、文档问答或自定义结构化提取。
  仅当任务不匹配任何更具体的 GLM skill，或用户明确点名 `glm5v-multimodal` 通用入口时
  使用；仅提到 GLM-5V-Turbo 模型名称不会覆盖专用路由。描述或摘要、专门 OCR、目标定位、
  提示词生成、基于文档写作、简历筛选、PDF
  转换、应用或网站生成、股票分析和图像生成应优先使用对应专用 skill。仅收到附件、媒体 URL
  或文件路径且处理目标不明时先询问，不调用动态工具或 API。GLM-5V-Turbo 不支持独立音频输入。
---

# GLM-5V-Turbo 多模态分析

调用 Skill 激活的结构化动态工具，把一种媒体类型和文本提示发送给 GLM-5V-Turbo，并将模型文本结果返回给用户。

## 定位与路由边界

- 把本 skill 作为通用分析入口和专用工作流的共享底层客户端，不把它当作所有媒体任务的默认入口。
- 用户要求描述或摘要时使用 `glmv-caption`；要求精确提取文字、表格、公式、手写内容或版面时使用相应的 `glmocr*` skill。
- 用户要求定位/框选、反推生成提示词、基于文档创作、筛选简历、转换 PDF、生成应用/网站、分析股票或生成图片时，使用对应专用 skill。
- 只有专用 skill 不覆盖任务，或用户明确点名 `glm5v-multimodal` 通用入口时，才直接执行本 skill；仅指定 GLM-5V-Turbo 模型仍按任务类型选择专用 skill。
- 专用 Skill 通过其动态入口复用通用实现，不会把任务重新路由到本 Skill。

> **强制架构边界：** RClaw 宿主 Agent 的 LLM 不是 GLM-5V-Turbo。Agent 不自行解释图片、
> 视频或二进制文档；读取本 Skill 后必须调用 Manifest 动态激活的专用工具，再依据工具结果
> 或 `output` 产生的纯文本继续工作。不得仅凭宿主模型的原生多模态能力跳过专用工具。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确处理目标时，不调用动态工具、
> 不抽帧、不读取或上传媒体、不调用 API；只根据文件类型列出可用操作并询问用户。
> 当前消息或前文已明确任务时直接执行，不要重复确认。

## 路径约定

- 本地输入路径和 `output` 路径相对于 agent workspace；专用工具会校验并规范化路径。
- 入口脚本、Python 解释器和参数映射由 `skill.toml` 与运行时管理；不得通过 shell 自行拼接。
- RClaw 已注入当前真实来源 channel，不要手填或猜测。用户要求把 `output`
  文件作为外部 channel 附件发送时，先读取 `skills/glm-master-skill/SKILL.md`，再调用
  `skill__glm_master_skill__publish_media`，传入实际 `path` 和 `media_type=file`。
  RClaw 会校验 stdout 中的 `MEDIA_MARKER=` 并自动把有效标记保留到最终出站消息；
  不要手抄或手写媒体标记，没有有效 marker 时只报告实际文件路径，不得声称已发送。

## 首次配置

在 `skills/glm-master-skill/config.toml` 的 `[zhipu]` 中填写 `api_key`，并在 `[glm5v]`
维护模型、端点、超时、重试、thinking/stream 和输入限制。动态工具不读取模型配置环境变量；
不要把密钥写进命令参数、提示词、输出文件、日志或聊天回复。

核心脚本依赖 `requests`；本地视频抽帧还需要 Termux `ffmpeg`：

```bash
pkg install python ffmpeg
python -m pip install -r skills/glm5v-multimodal/scripts/requirements.txt
```

## 选择输入方式

一次请求只能选择下列一组，不能混合图片、视频和文件：

- 图片：调用 `skill__glm5v_multimodal__analyze_images`，本地路径放入 `images`，HTTPS URL 放入 `image_urls`。本地图片按 data URL 发送，仅支持 JPG/JPEG/PNG，单图不超过 5 MB，宽高均不超过 6000 像素，并在上传前核对文件头与尺寸。
- 远程视频：调用 `skill__glm5v_multimodal__analyze_videos`，把 URL 放入 `video_urls`，使用原生 `video_url`。
- 外部 channel 下载到本地的视频：调用同一工具并把路径放入 `videos`。底层实现通过 Termux `ffmpeg` 按 `[glm5v].image_limit` 均匀抽帧，再作为同一组图片交给 GLM-5V-Turbo；结果只覆盖视觉画面，不包含音频。不要把本地视频伪装成远程 URL。
- 文件：调用 `skill__glm5v_multimodal__analyze_files`，本地路径放入 `files`，HTTPS URL 放入 `file_urls`。本地文件先由智谱同步文件解析接口提取内容，再交给 GLM-5V-Turbo；远程文件 URL 直接用 `file_url` 发送给模型。
- 独立音频：不支持。不要发送未公开的 `audio_url`；如用户需要音频转写，应改用专门的 ASR 能力，并明确那不是 GLM-5V-Turbo 原生输入。

本地文件支持 PDF、DOC、DOCX、PPT、PPTX、XLS、XLSX、CSV、TXT、MD 和 HTML。

## 工作流程

1. 明确用户希望从媒体中获得什么结果，取得所有实际路径或 HTTPS URL。
2. 组织一个自包含提示词，写明任务、输出格式、逐项关注点和不确定信息的处理方式。若核心目标变成精确 OCR，停止本流程并改用相应的 `glmocr*` skill；仅在更广泛的分析中顺带读取少量可见文字。
3. 按用户指定顺序重复传入同组参数。图片会映射为“图1、图2……”，文件会保留文件名或 URL 标签。
4. 按媒体类型调用对应的动态专用工具。工具会在联网或编码前扫描整批本地路径、格式、大小和 URL，一次列出全部可确定的输入问题；修正整批后再重跑。预计回答较长时始终传入 `output`，以免工具输出截断。
5. 读取 stdout 中 `__GLM5V_RESULT_BEGIN__` 与 `__GLM5V_RESULT_END__` 之间的 JSON。优先使用 `text`；若 stdout 被截断，则读取 `output_path` 指向的 UTF-8 文件。
6. 将结果整理给用户，不要补写输入中无法核实的事实。若 `finish_reason` 为 `length` 或 `complete` 为 `false`，明确说明内容被截断，并缩小任务后重跑。

## 动态工具

读取本 Skill 后，运行时根据 `skill.toml` 激活三个互斥媒体入口：

- `skill__glm5v_multimodal__analyze_images`：传入 `images` 和/或 `image_urls`。
- `skill__glm5v_multimodal__analyze_videos`：传入 `videos` 和/或 `video_urls`。
- `skill__glm5v_multimodal__analyze_files`：传入 `files` 和/或 `file_urls`。

每个入口都至少传入 `prompt` 或 `prompt_file`，可选传入 `output` 与 `dry_run`。`prompt` 与 `prompt_file` 可以同时使用，工具会依次拼接两者。不得把不同媒体组混入同一次调用，也不得退回 shell 手写脚本路径或参数。

## 调用约束

- 图片默认最多 8 张，文件默认最多 8 个；超过上限时筛选最相关输入或拆成多次调用。
- 请求固定使用 `stream=true` 和 `thinking={"type":"enabled"}`。不要通过改配置或临时请求关闭思考模式。
- 工具的底层实现只聚合 `delta.content`，忽略并且不保存 `delta.reasoning_content`。
- 连接或流读取失败、HTTP 408/429/500/502/503/504、流未完整结束时，脚本会丢弃
  该次未完成内容并有限退避；额外重试次数读取 `[zhipu].max_retries`（默认 3）。
- 执行真实请求的进程总超时由 Manifest 固定为 1500 秒；`[zhipu].read_timeout_sec`
  是连续未收到 SSE 分片的空闲超时，不是总运行时长。
- 模型输出不是独立核验的事实。涉及医疗、法律、财务或安全决策时，说明局限并要求使用权威来源复核。

## 本地验证

配置 API key 前可在相应动态工具中传入 `dry_run=true` 检查参数和请求摘要；该模式不会调用 API，也不会调用文件解析服务。
