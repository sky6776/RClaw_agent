---
name: glm-master-skill
description: |
  RClaw 中的 GLM 官方技能导航与运行规范。用于在 GLM-OCR、GLM-Image、GLM-5V-Turbo
  图片/视频/文档理解、定位、简历筛选、基于文档写作、PDF 转演示/网页、PRD 开发、股票分析
  和网页视觉复刻等能力之间选择正确的本地 skill。该 skill 只负责路由与说明，不直接调用 API。
  仅收到附件、媒体 URL 或文件路径且用户意图不明时也使用本路由：先询问处理目标，不调用动态工具或 API。
---

# GLM Skills for RClaw

> **RClaw 移植说明：** 本目录基于智谱官方 `zai-org/GLM-skills` 移植，并针对 RClaw 的
> workspace 路径、Termux/POSIX Python 命令、共享 TOML 配置和按需 HTML 交付做了修改。

本 skill 只做能力路由。确定目标后，继续读取并执行对应目录下的 `SKILL.md`。

## 外部 channel 附件意图门禁（强制）

本规则适用于 NapCatQQ、Weixin 和未来所有 RClaw channel，不按 channel 名称分支：

1. 结合当前消息和紧邻的有效对话上下文判断任务。只收到附件、媒体 URL、文件路径，或只有“看看这个”“处理一下”等未说明目标的话时，视为意图不明确。
2. 意图不明确时，不运行 skill 脚本，不读取正文/画面，不抽帧、不做 OCR、不上传媒体、不调用任何模型 API，也不创建产物。可以使用 channel 已提供的文件名、扩展名、MIME 和大小来组织询问。
3. 根据已知媒体类型给出 2–5 个本系统确实支持的选项并询问用户，例如视频可选摘要、时间线、画面文字识别、目标定位/跟踪或提示词生成。独立音频只能建议先用已配置的 ASR 转写，不得声称 GLM-5V-Turbo 可直接处理音频。
4. 用户在当前消息中已明确目标，或前文已明确说明“下一条附件要做什么”而本条是在补交附件时，直接选择最具体的 skill 执行，不要重复确认。
5. 用户回答选项或补充明确要求后，才把该答复作为本次处理授权，并只执行所选范围。

仅收到一个视频时，可回复：“已收到视频。我可以帮你概括内容、整理时间线、识别画面文字、定位/跟踪指定对象，或根据画面生成提示词。你希望怎么处理？”

## RClaw 统一运行规则

- RClaw `shell` 的当前目录是 agent workspace。不要添加 `agent/` 前缀，也不要切换到 skill 目录。
- 所有 skill 脚本必须通过读取相应 `SKILL.md` 后由 Manifest 激活的 `skill__...` 专用工具调用；不要翻译、拼接或猜测上游脚本命令。
- 新增的智谱 skill 统一读取 `skills/glm-master-skill/config.toml`；API Key、模型、端点、超时、重试和模型生成参数都不读取环境变量。不要把 Key 写入 `SKILL.md`、命令参数、输出、日志或提交记录。
- `glmv-stock-analyst` 保留自己的 `skills/glmv-stock-analyst/config.toml`；其它 GLM skill 共用上述配置。动态工具不开放配置路径参数，需要调整时直接维护对应 TOML。
- 本地输入和输出必须位于 workspace；动态工具会解析相对路径并拒绝越界绝对路径和父目录跳转。
- 宿主 Agent 使用的 LLM 与 GLM-5V-Turbo 相互独立。所有 `glmv-*` 视觉或文档感知都必须调用对应动态工具；宿主 Agent 只读取工具返回的纯文本/JSON，不得假定自己能直接看懂二进制媒体。
- 没有专用 API 客户端的工作流统一读取 `skills/glm5v-multimodal/SKILL.md`，并调用对应的
  `skill__glm5v_multimodal__...` 动态工具。这不表示把专用任务重新路由为
  `glm5v-multimodal`。PDF 页面、原型图和浏览器截图按每批最多 8 张图片送入，长文档分批处理。
- 调用 GLM-5V-Turbo 时，一次请求只使用图片、视频或文件中的一种媒体类型，不混合三类输入。
- GLM-5V-Turbo 不接受独立音频输入。音频先通过专用语音识别能力转成文本，再交给视觉或文档 skill。
- `glmocr*` 明确调用 GLM-OCR，`glm-image-gen` 明确调用 GLM-Image；这两类专用 skill 不伪装成 GLM-5V-Turbo 调用。
- 外部交付不绑定具体 channel；RClaw 在每轮运行时注入真实 `source_channel`，NapCatQQ、Weixin 和未来 channel 都复用同一媒体沙箱与发布工具。Skill 不手填或猜测 channel；`RCLAW_<CHANNEL>_ENABLED=1` 仅声明该 channel 已实现媒体能力，不代表独立沙箱。只有需要浏览器交互或坐标叠加的结果才生成 HTML（如 Grounding、PDF→Web、应用预览）；PDF→PPT 发送 `.pptx`，生图发送图片，应用/网站工程发送 ZIP，文字类直接发送文本/JSON。Termux 不自动打开浏览器。
- 文件型产物生成后，使用生成工具实际返回或最终确认存在的文件路径调用
  `skill__glm_master_skill__publish_media`。只传 `path`、`media_type`、可选 `name` 和 `caption`；
  不提供 `source_channel`，始终使用 RClaw 注入的真实来源；
  运行时和直跑两种方式都只会为已声明 `RCLAW_<CHANNEL>_ENABLED=1` 的媒体 channel 生成 marker。
  RClaw 会校验 stdout 中每个 `MEDIA_MARKER=` 的真实来源和沙箱文件，并自动把有效标记保留
  到最终出站消息；不得手抄或根据请求路径手写媒体标记，没有有效 marker 时只报告实际文件路径，不得声称已发送。

## 配置与脚本调用关系

共享配置文件是 `skills/glm-master-skill/config.toml`：

- `[zhipu]`：公用 API Key、基础地址、连接/读取超时和重试次数。
- `[glm5v]`：GLM-5V-Turbo 模型、thinking/stream、token、输入限制和本地文件解析接口；各 GLM-5V 任务默认继承这里的 `max_tokens = 128000`。
- `[caption]`、`[doc_based_writing]`、`[prompt_gen]`、`[resume_screen]`、`[grounding]`：仅配置对应任务特有的生成参数；如确有需要，可在任务 section 中显式覆盖通用值。
- `[glm_ocr]`：GLM-OCR 专用模型、端点、超时和重试。
- `[glm_image]`：GLM-Image 专用模型、端点、尺寸、质量、水印、超时和重试。

`glm5v-multimodal`、Caption、文档写作、Prompt、简历、Grounding、OCR 和 GLM-Image
动态工具直接发起模型 API 请求；PDF→PPT/Web、PRD→App、网页复刻和 OCR 批处理等复合 Skill
由其固定实现复用底层能力。两种方式都不会让宿主 Agent 直接调用或模拟 GLM。

## 能力路由

| 需求 | 使用的 skill |
|---|---|
| 没有专用流程覆盖的视觉问答、多图对比、图表/文档问答，或用户明确点名通用入口 `glm5v-multimodal` | `glm5v-multimodal` |
| 图片、视频或文档的描述与摘要 | `glmv-caption` |
| 从图片反推文生图或文生视频提示词 | `glmv-prompt-gen` |
| 根据文档要求撰写新内容 | `glmv-doc-based-writing` |
| 简历批量筛选、评分、排序 | `glmv-resume-screen` |
| PDF 转 PowerPoint（`.pptx`） | `glmv-pdf-to-ppt` |
| PDF 转网页 | `glmv-pdf-to-web` |
| 股票研究与 GLM-5V 图表分析 | `glmv-stock-analyst` |
| 通用 OCR 与版面解析 | `glmocr` |
| 表格 OCR | `glmocr-table` |
| 公式 OCR | `glmocr-formula` |
| 手写文字 OCR | `glmocr-handwriting` |
| Termux 兼容的 GLM-OCR 批处理入口 | `glmocr-sdk` |
| 文生图 | `glm-image-gen` |

专用 skill 与 `glm5v-multimodal` 同时可能匹配时，始终选择专用 skill；通过动态工具复用
`glm5v-multimodal` 不改变路由结果。`glm5v-multimodal` 是本次为 RClaw
补充的通用兜底入口；`glmv-stock-analyst` 保留仓库中已有的 RClaw 增强版本，其余目录来自
上述官方提交并加入 RClaw 适配说明。

## 推荐执行顺序

1. 根据上表选一个最具体的 skill；仅指定 GLM-5V-Turbo 模型不会覆盖专用路由，只有没有专用项或用户明确点名 `glm5v-multimodal` 通用入口时才选择它。
2. 完整读取该 skill 的 `SKILL.md`，遵守其中的输入、输出和安全限制。
3. 检查 Skill 依赖；仅安装该 Skill 明确需要的依赖。
4. 确认共享 TOML 中所需 section 已配置；不得用环境变量或命令参数覆盖模型配置。
5. 将结果写到 workspace 下的任务目录，并向用户说明实际使用的模型、输入类型和限制。

## 官方参考

- GLM Skills: <https://github.com/zai-org/GLM-skills>
- GLM-5V-Turbo: <https://docs.bigmodel.cn/cn/guide/models/vlm/glm-5v-turbo>
- GLM-OCR: <https://github.com/zai-org/GLM-OCR>
- GLM-Image: <https://github.com/zai-org/GLM-Image>
