# seedance-video — Prompt 模板

火山方舟 Doubao Seedance 2.5 视频生成/编辑:文生视频(最长 30 秒,含同步音频/台词/音效/BGM)、
首帧/首尾帧生视频、全模态参考生视频(主体/风格/动作/声音,图+视频+音频上限 50 个素材)、
视频编辑(替换/增删/局部重绘/音频增删改,支持时间戳)、视频延长(向前/向后,可多轮)、
样片模式两步出片、尾帧接力多段叙事、联网搜索时效内容。

- 占位符用 `<>` 标出,替换后即可使用。
- 编号为全集连续编号(P140 起),与其他分册不重号。
- API Key 在 `skills/ark_key.toml`([ark].api_key,火山方舟系列共用),只由脚本内部读取;
  请求参数走 `skills/seedance-video/config.toml` 的 `[seedance]` 表(默认 720p / adaptive / 智能时长)。
- 开通条件:账户余额 > 200 元或已购相应资源包;按分辨率/时长计费,生成前 agent 先与用户
  确认分辨率与时长。单次生成通常 1-10 分钟(30 秒 1080p 更久),期间耐心等待轮询。
- 任务类型由素材与提示词意图判定,配置不符会异步报错且照样计费:编辑提示词须含
  「编辑视频/增加/删除/去掉/替换/改成」等关键词,延长须含「向前/向后延长/延续/续写」。
- 参考视频仅接受 http(s) URL 或 `asset://` 素材 ID(本地视频先传公网存储);
  不支持含真人人脸的参考素材。轮询超时用 check_task 补查,排队中可 cancel_task。
- video_url 24 小时有效且限下载 100 次,只作诊断;主交付物是本地视频文件。

整理日期:2026-09-23

---

### P140 基础文生视频(30 秒分段叙事)
覆盖:generate_video(prompt + save 必填;时间戳 [N-M秒] 分段;ratio/duration 覆盖默认值)

```text
帮我生成一段视频:写实自然纪录片风格,[0-10秒]<清晨湿地一只白鹭单脚伫立,镜头缓慢推近>;
[10-20秒]<它猛地俯冲入水叼起一条鱼>;[20-30秒]<飞向朝阳,慢镜头剪影收尾>。
环境音有水声和鸟鸣,<16:9> 画幅、<30> 秒,保存到 output/egret.mp4。
```

### P141 首帧 / 首尾帧生视频
覆盖:first_frame / last_frame(输出画幅自动对齐首帧;与其他参考素材互斥)

```text
用 <output/frame.jpg> 作首帧生成视频:<女孩转身微笑挥手,镜头缓慢拉远,街景人流虚化>,
保存到 output/greet.mp4。
```

```text
用 <media/bud.jpg> 作首帧、<media/bloom.jpg> 作尾帧,生成花朵从含苞到完全绽放的
延时视频,光线渐暖,保存到 output/bloom.mp4。
```

### P142 全模态参考生视频(素材职责映射)
覆盖:image/video/audio 参考素材组合(≤30 图 + 10 视频 + 10 音频);@图片N/@视频N/@音频N 逐条绑定职责

```text
生成一支广告片,果味饼干为主角:@图片1 是主体形象,@视频1 参考开场构图,@视频2 参考特写动态,
@音频1 作为节奏参考。素材依次为 <media/cookie.png>、<https://example.com/comp.mp4>、
<https://example.com/motion.mp4>、<media/bgm.mp3>,画面干净高级,保存到 output/ad.mp4。
```

### P143 视频编辑(替换 / 增删 / 音频,支持时间戳)
覆盖:task_type=edit(参考视频 4-30 秒;ratio=adaptive、duration=-1 自动锁定;输出对齐母版)

```text
编辑视频:把 @视频1 中 <4-6秒> <男人喝咖啡> 的动作改为 <拖地>,其余内容保持不变。
参考视频 <https://example.com/clip.mov>,保存为 output/edited.mov。
```

```text
编辑视频:删除 @视频1 的背景音乐,加上 <轻快的爵士乐>;把画面右侧的 <亚洲女生> 替换为
@图片1 中的女生。参考视频 <https://example.com/clip.mp4>,参考图 <media/girl.png>,
保存为 output/remix.mp4。
```

### P144 视频延长(向前 / 向后)
覆盖:task_type=extend(提示词须含延长关键词;可指定 duration 或 -1 智能选择)

```text
向后延长 @视频1(<https://example.com/prev.mp4>):<窗户打开后镜头进入美术馆室内,
最后进入画作世界>,与原片段无缝衔接,保存到 output/extended.mp4。
```

### P145 样片模式两步出片
覆盖:draft=true 先出 480p 预览 → 确认后凭 draft_task_id 出 1080p 正式片(低成本试错)

```text
先用样片模式出一段 480p 预览:<30 秒复杂叙事描述>,保存为 output/draft.mp4;
我确认效果后,再用这个样片出 1080p 正式片。
```

### P146 尾帧接力多段连续叙事
覆盖:return_last_frame=true 返回尾帧 JPEG,作为下一段的 first_frame 传入

```text
生成多段连续叙事视频:第一段 <描述>,保存 output/part1.mp4 并返回尾帧;
之后每段以上一段尾帧作首帧继续 <描述>,分别保存 output/part2.mp4、output/part3.mp4。
```

### P147 联网搜索生成时效内容
覆盖:web_search=true(增加时延;搜索次数见 usage.tool_usage;不适用于样片转正式)

```text
生成一段 <8> 秒的科技新闻短视频:展示本周最值得关注的三条 AI 行业动态,
新闻字幕风格包装,联网搜索最新内容,保存到 output/news.mp4。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| prompt + save 基础文生视频、时间戳分段 | P140 |
| ratio / duration 覆盖与计费确认 | P140 |
| first_frame / last_frame 首尾帧模式 | P141 |
| 全模态参考(@图片N/@视频N/@音频N 职责绑定) | P142 |
| task_type=edit 编辑(替换/增删/音频/时间戳) | P143 |
| task_type=extend 延长 | P144 |
| draft 样片模式两步出片 | P145 |
| return_last_frame 尾帧接力 | P146 |
| web_search 联网搜索 | P147 |
| check_task / cancel_task 任务管理 | 文档头部说明 |
