---
name: seedream-image
description:
  Seedream 5.0 lite 图片生成/编辑（文生图、图生图、参考图生图、多图融合、
  组图、联网搜索）, Volcano Ark image generation and editing. Generate, edit,
  restyle, or combine images: text-to-image, image editing (add/remove/replace/
  modify, doodle/box visual signals), multi-image fusion, reference-based
  generation, sequential image groups (storyboards, brand sets), web search.
  Use when the user says 生成图片, 改图, P图, 换背景, 换装, 分镜, generate image,
  edit image, image to image.
---

# Seedream 5.0 lite 图片生成技能

> **RClaw 使用说明：** 读取本 Skill 后必须调用 Manifest 动态激活的
> `skill__seedream_image__generate_image`，不得自行选择解释器、脚本路径或参数。
> API Key 从系列共享的 `skills/ark_key.toml` 读取（[ark].api_key）；
> 请求参数只从 `skills/seedream-image/config.toml` 的 `[seedream]` 表读取
> （不读环境变量；config 内残留 api_key 会被脚本拒绝）。
> 生成文件必须位于 workspace 的输出目录（如 `output/`）。主交付物是生成的本地
> 图片文件；结果中的 `url` 仅 24 小时有效，只作诊断信息。
> 外部 channel 交付时，先读取 `skills/glm-master-skill/SKILL.md`，再调用
> `skill__glm_master_skill__publish_media`，传入 `path=<saved_file>` 和
> `media_type=image`。运行时会注入真实来源 channel，不要手填或猜测。

调用火山方舟 Doubao Seedream 5.0 lite（Model ID: `doubao-seedream-5-0-lite-260128`）
同步图片生成接口，覆盖该模型支持的全部图片能力：文生图、图生图编辑、
多图融合、参考图生图、组图生成与联网搜索。

**本 skill 不覆盖** Seedream 5.0 pro / flash 专属能力：交互编辑（坐标/框选/箭头
标注编辑）、图层拆分、透明背景输出、fast 提示词优化模式。用户需要这些能力时
如实告知需要升级模型，不要伪造参数。

## When to Use / 使用场景

- 从文字生成图片 / 海报 / logo / 插画（文生图）
- 编辑现有图片：增加、删除、替换、修改元素，风格转换，材质替换，改背景改视角
- 多张参考图融合：换装、人物替换、风格迁移等复合编辑
- 参考图生图：保持人物形象 / 风格 / 产品款式做衍生设计（手办、图标、成套设计）
- 组图输出：漫画分镜、影视分镜、成套品牌视觉、表情包、系列壁纸
- 需要时效性信息的图（天气预报、时事海报）→ 开联网搜索
- User mentions "生成图片", "文生图", "图生图", "改图", "P图", "换装", "组图", "分镜",
  "generate image", "edit image", "image to image"

**不要用于**：视频生成（用 happyhorse 系列）；图层拆分/交互编辑（Seedream 5.0
pro/flash 专属，本 skill 未接入）；图片文字识别（用 glmocr 系列）。

## 模型与能力

| 项目 | 说明 |
| ---- | ---- |
| 模型 | `doubao-seedream-5-0-260128`（官方主 ID；同时支持等效 ID `doubao-seedream-5-0-lite-260128`，config 默认用后者，两者同一模型） |
| 接口 | `POST https://ark.cn-beijing.volces.com/api/v3/images/generations`（同步） |
| 输入 | 文本提示词 + 0-14 张参考图（URL 或本地文件，单张 ≤ 30MB；支持 jpeg/png/webp/bmp/tiff/gif/heic/heif） |
| 输出 | 单图或组图（最多 15 张；参考图数 + 生成数 ≤ 15） |
| 分辨率 | 2K / 3K / 4K 档位，或显式宽x高（总像素 [2560x1440, 4096x4096]，宽高比 [1/16, 16]） |
| 格式 | png / jpeg |
| 限流 | 500 张/分钟（IPM） |

## How to Use / 使用方法

### 文生图（纯文本 → 单图）

```json
{"prompt":"充满活力的特写编辑肖像，模特眼神犀利，头戴雕塑感帽子，色彩拼接丰富，Vogue杂志封面美学风格","save":"output/portrait.png"}
```

### 图生图编辑（单图 + 编辑指令）

```json
{"prompt":"去掉女生的帽子。","image":["media/in.png"],"save":"output/edited.png"}
```

编辑四类操作的提示词写法：**增加**「给图中女生增加相同款式的银色耳线和项链」、
**删除**「去掉女生的帽子」、**替换**「把最大的面包人换成牛角包形象，保持动作和表情不变」、
**修改**「让图中三个机器人变成透明水晶材质」。

### 视觉信号控制（涂鸦/线框）

画面复杂难以用文字指明对象时，让用户在输入图上用涂鸦、线框、箭头标记位置，
提示词引用标记即可：

```json
{"prompt":"将房间内红色涂抹位置放入电视，蓝色涂抹位置放入沙发，不改变其他布局，确保放入物体和整张图的原木风格一致。","image":["media/marked.png"],"save":"output/room.png"}
```

### 多图融合 / 复合编辑（2-14 张参考图）

```json
{"prompt":"将图1的服装换为图2的服装","image":["media/person.png","media/outfit.png"],"save":"output/fusion.png"}
```

多图复合编辑模板：**替换**「将图一的主体替换为图二的主体」、**组合**「让图一人物
穿上图二的服装」、**迁移**「参考图二的风格，对图一进行风格转换」。

### 参考图生图（保持人物形象/风格/款式）

```json
{"prompt":"参考图中的人物形象，做一个动漫人物手办，放在桌上，后面放置一个印有角色图像的生日礼物包装盒，场景设置在室内，尽可能真实。","image":["media/character.png"],"save":"output/figure.png"}
```

提示词须包含两部分：先**指明参考对象**（参考图中的人物形象/风格/款式），
再**描述生成画面**（场景、构图、风格）。

### 组图输出（文/单图/多图 → 多图）

```json
{"prompt":"生成一组电影级科幻写实风的4张影视分镜：场景1为宇航员在空间站维修飞船，中全景构图，冷色调科幻光影；场景2为陨石带袭击，广角史诗镜头；场景3为宇航员紧急躲避，近景动态抓拍；场景4为受伤后惊险逃回飞船，舱内暖光与太空冷光对比。","save":"output/storyboard.png","max_images":4}
```

传入 `max_images` 即开启组图模式；结果自动保存为 `storyboard_001.png` .. 
`storyboard_004.png`。参考图 + 生成总数 ≤ 15。提示词用「一套」「一组」「N张」
或逐场景描述来控制组图内容。

### 联网搜索（时效性内容）

```json
{"prompt":"制作一张上海未来5日的天气预报图，现代扁平化插画风格，横向排版，包含5个等宽垂直卡片。","save":"output/weather.png","web_search":true}
```

## 结构化工具参数

| Parameter       | Required | Description |
| --------------- | -------- | ----------- |
| `prompt`        | 是       | 非空提示词；建议 ≤300 汉字（过多细节会被模型忽略）；要渲染的文字放双引号中 |
| `save`          | 是       | workspace 内保存路径；扩展名规范化为 .png/.jpg；组图加 `_001.._NNN` 序号 |
| `image`         | 否       | 参考图数组（≤14 张），URL 或本地路径；本地图片自动转 Base64 |
| `size`          | 否       | `2K`/`3K`/`4K` 档位（宽高比写进提示词，如「竖版」「16:9」），或 `宽x高`（如 `2848x1600`） |
| `max_images`    | 否       | 组图模式最大张数（1-15）；传入即开启组图 |
| `web_search`    | 否       | `true` 开启联网搜索 |
| `output_format` | 否       | `png` / `jpeg`（省略时默认 png，可在 config 修改） |
| `watermark`     | 否       | `true` 加「AI 生成」水印（默认不加） |

提示词写法与各场景详细范例见 `skills/seedream-prompt-gen/SKILL.md`。

## 响应格式

```json
{
  "ok": true,
  "model": "doubao-seedream-5-0-260128",
  "created": 1760000000,
  "prompt": "...",
  "parameters": {"size": "2K", "output_format": "png", "watermark": false,
                  "sequential_image_generation": "auto", "max_images": 4,
                  "web_search": false, "reference_images": 0},
  "usage": {"generated_images": 4, "output_tokens": 52019, "total_tokens": 52019},
  "images": [
    {"index": 1, "saved_file": "output/storyboard_001.png", "url": "https://...",
     "size": "2048x2048", "output_format": "png"}
  ],
  "failed_images": [],
  "note": "url 24 小时内有效，请以 saved_file 为交付物",
  "error": null
}
```

- `images[].saved_file` 是主交付物；`url` 24 小时过期
- `model` 为响应归一化后的 ID（请求 lite 别名时返回 `doubao-seedream-5-0-260128`，同一模型）
- `usage.generated_images` 为成功生成并计费的张数
- `failed_images`：组图模式下单张生成/下载失败不影响其他图片，失败明细在此列出；
  全部失败才返回 `ok:false`
- 单张 2K 图通常 10-30 秒；4K 组图可达数分钟

## Error Handling / 错误处理

**未配置 Key：**

```json
{"ok": false, "error": {"code": "CONFIG_ERROR", "message": "未配置 API Key，请填写 skills/ark_key.toml 的 [ark].api_key"}}
```

→ 引导用户在 [火山方舟 API Key 管理](https://console.volcengine.com/ark) 创建 Key 并填写

**鉴权/权限失败（401/403）：** Key 无效或未开通该模型 → 检查方舟控制台开通状态

**参数越界：** `INVALID_SIZE`（档位/像素区间/宽高比不符）、`INVALID_IMAGE`
（超 14 张、格式不支持、单张超 30MB）、`INVALID_PARAMS`（max_images 越界、
参考图+生成数超 15）都会在请求前被本地拒绝，不产生付费

**内容风控 / 生成失败：** 上游错误码原样透出（`error.message` 展示给用户），
常见原因是提示词触发内容安全策略，改写后重试

**部分失败（组图）：** 看 `failed_images`，成功图片照常交付，可只对失败部分重试

**下载失败（`DOWNLOAD_FAILED`）：** 单图场景直接报错重试；组图场景看
`failed_images`，可整组重试

## Prompt Tips / 提示词技巧（速查）

- 自然语言写清 **主体 + 行为 + 环境**，再补风格、色彩、光影、构图
- 要生成的**文字内容放双引号**：标题为 "Seedream"
- 编辑指令**指名对象**、避免代词：✅「图中最高的那只熊猫」❌「它」
- 说明**保持不变**的部分：「保持动作和表情不变」
- 完整指南见 `skills/seedream-prompt-gen/SKILL.md`

## 关于流式输出的说明

Seedream 5.0 lite 支持 `stream=true` 流式返回，但本 skill 固定使用非流式：
工具调用需要等全部图片生成并落盘后才返回一次结果，流式对最终交付没有增益。
如后续需要边生成边交付再做接入。

## Resource Links / 资源链接

- API 文档：[图片生成 API](https://ark.volcengine.com/docs/ark/image-generation-api)
- 教程：[图片生成教程](https://docs.volcengine.com/docs/ark/seedream-4-0-5-0)
- 提示词指南：[Seedream 4.0-5.0 提示词指南](https://ark.volcengine.com/docs/ark/seedream-4-0-5-0-prompt-guide)
- Key 管理：[火山方舟 API Key 管理](https://console.volcengine.com/ark)
