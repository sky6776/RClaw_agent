# glmv-prompt-gen — Prompt 模板

分析参考图片/视频，为文生图与文生视频 AI 工具（Midjourney、Stable Diffusion、DALL-E、
Sora、Runway、Kling、Pika）反推专业提示词。输出含内容分析 + prompt + prompt breakdown。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号为全集连续编号（P88 起），与其他分册不重号。
- 图片 ≤50 张（本地或 URL）；视频仅公开 HTTPS URL ≤200MB；图片与视频不能同请求。

整理日期：2026-09-05

---

### P88 参考图 → 文生图 prompt
覆盖：prompt_from_images 默认 mode=image（内容分析 + 英文 prompt + 按 Subject/Style/Color/Lighting/Composition/Mood 拆解）

```text
参考这张图 <图片路径或URL>，给我生成一套 Midjourney / Stable Diffusion 的文生图 prompt。
```

### P89 参考图/视频 → 文生视频 prompt
覆盖：mode=video（图片或视频反推文生视频 prompt；本地视频先转交 analyze_videos）

```text
参考这个视频 <视频URL>，帮我写一段 Sora/Kling 的文生视频 prompt。
```

### P90 两套都要 + 保存
覆盖：mode=auto（同时输出文生图 + 文生视频两套）+ output 参数保存结果

```text
参考 <图片>，文生图和文生视频的 prompt 都给我来一套，存到文件里。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| 图 → 文生图 prompt（mode=image） | P88 |
| 图/视频 → 文生视频 prompt（mode=video） | P89 |
| 两套同时输出（mode=auto） | P90 |
| 结果保存（output） | P90 |
| Prompt Breakdown 六维拆解 | P88–P90 均输出，验收即可 |
