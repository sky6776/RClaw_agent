# glmv-caption — Prompt 模板

用 GLM-V 多模态模型为图片（本地/URL，≤50 张）、视频（公开 URL，≤200MB）、文档（公开 URL，≤50 个）
生成描述/摘要/概览。附件意图门禁：只发附件不说明目标时，agent 会先询问再执行。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号为全集连续编号（P74 起），与其他分册不重号。
- 精确 OCR、目标定位、反推提示词有各自的专用技能（见对应分册），本包只管"描述"。

整理日期：2026-09-05

---

### P74 图片描述
覆盖：caption-images（本地路径或 URL 批量 ≤50 张、自定义 prompt、结果 JSON 保存、逐图完整展示）

```text
描述一下这张图片：<图片路径或URL>。
```
```text
看图说话：这批图 <图片1> <图片2> <图片3> 每张都描述一遍，
重点说出图里的文字内容，结果存成 JSON。
```

### P75 视频描述
覆盖：caption-videos（公开 HTTPS 视频概述，mp4/mkv/mov ≤200MB）

```text
给这个视频生成内容概述：<视频URL>。
```

### P76 文档概览
覆盖：caption-files（公开 HTTPS 文档 URL ≤50 个：pdf/docx/txt/xlsx/pptx/jsonl）

```text
概述一下这份文档讲了什么：<文档URL>。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| caption-images 图片描述（批量/自定义prompt/存JSON） | P74 |
| caption-videos 视频描述 | P75 |
| caption-files 文档概览 | P76 |
| 附件意图门禁（只发附件先询问） | 另见 glm-master-skill.md P106 |
