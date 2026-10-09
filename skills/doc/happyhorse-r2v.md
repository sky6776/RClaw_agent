# happyhorse-r2v — Prompt 模板

阿里云百炼 HappyHorse-1.1-R2V 参考生视频：以 1-9 张参考图（人物/服装/道具/场景）加
提示词合成视频，prompt 中用 [Image 1]、[Image 2] 按 --image 顺序引用各图并描述动作
运镜。ratio 由参数显式指定（与 i2v 不同，不跟随参考图）。结果链接 24 小时有效，
主交付物是本地 MP4 文件。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号为全集连续编号（P136 起），与其他分册不重号。
- API Key 在 `skills/happyhorse_key.toml`（t2v/i2v/r2v 系列共享），只由脚本内部读取，
  不打印不外传；各 skill 的 config.toml 只存模型与请求参数。
- 按秒计费（480P 0.45 / 720P 0.9 / 1080P 1.2 元/秒，与参考图数量无关），生成前 agent
  先与用户确认分辨率与时长；单次生成通常 1-5 分钟，期间耐心等待轮询结果。
- 参考图要求：jpg/jpeg/png/webp，短边 ≥400 像素（推荐 720P 以上素材），≤20MB；
  prompt 必填且引用编号不能超过传入图数。

整理日期：2026-09-22

---

### P136 基础参考生视频（双图引用）
覆盖：generate_video（prompt 含 [Image N] 引用 + 2 张图 URL + save；其余走 config 默认值 480P / 16:9 / 5 秒 / 不加水印）

```text
用两张参考图合成一段视频：<图片1URL> 里的 <主体描述> 穿上 <图片2URL> 里的
<服装/道具描述>，<动作与运镜描述>，保存到 output/<文件名>.mp4。
注意 prompt 里要写成 [Image 1]、[Image 2] 来指代这两张图。
```

### P137 本地图与 URL 混用
覆盖：--image 可重复传参、本地路径自动转 base64、顺序对应 [Image N]

```text
把 <input/人物.png> 里的角色放进 <input/场景.png> 的场景里，
让她 <动作描述>，镜头 <运镜描述>；本地图片和
<https://example.com/prop.jpg> 的道具一起用，保存到 output/<文件名>.mp4。
```

### P138 显式画幅与高清长片
覆盖：ratio（显式指定，不跟随参考图）+ resolution + duration；生成前确认费用

```text
用 <input/角色.png> 和 <input/背景.png> 合成一段 <21:9> 宽幅、<1080P>、
<10> 秒的视频：[Image 1] 中的角色在 [Image 2] 的场景中 <动作描述>，
镜头 <运镜描述>，保存到 output/<文件名>.mp4。生成前先按
<1080P> 1.2 元/秒算一下这段的费用并跟我确认。
```

### P139 固定种子与水印
覆盖：seed（可复现生成）+ watermark 水印开关；分辨率/比例/时长沿用上次参数

```text
用固定种子 <42> 重新生成刚才那条视频：<同样的图和 prompt>，
分辨率、画幅、时长都不变，这次加上 <右下角水印>，保存为 output/<文件名>-b.mp4。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| prompt 必填且用 [Image N] 引用参考图 | P136、P138 |
| images 1-9 张（URL / 本地 / 混用，顺序即编号） | P136、P137 |
| ratio 显式指定画幅（9 种，不跟随参考图） | P138 |
| resolution 分辨率档位与计费确认 | P138 |
| duration 时长（3-15 秒） | P138 |
| seed 固定种子可复现 | P139 |
| watermark 水印开关 | P139 |
| 共享 happyhorse_key.toml，Key 不打印不外传 | 内置约束，验收时检查输出 |
| video_url 24 小时有效，交付以本地 MP4 为准 | 内置约束 |
