---
name: glmocr-sdk
description: >
  使用脚本批量解析本地图片、PDF 和扫描文档，返回 GLM-OCR 的 Markdown 与结构化 JSON。
  用户明确要求通过 GLM-OCR SDK 批量提取文字、表格、公式、版面区域或坐标时使用。
  仅收到附件且当前消息与紧邻上下文没有处理目标时先询问，不调用动态工具或 API。在 Termux
  上自动改用同仓库的官方 GLM-OCR API 客户端，不安装不兼容的 SDK；不用于音频转写或普通照片描述。
---

# GLM-OCR SDK for RClaw

通过 Manifest 动态激活的 `skill__glmocr_sdk__extract_batch` 调用 GLM-OCR。宿主 Agent 不直接读取图片或 PDF，只消费
工具返回的 JSON；批处理实现逐个复用共享 GLM-OCR 客户端，避免依赖
Termux 上不可用的 SDK 原生组件。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确批量 OCR 目标时，不调用动态工具、
> 不读取或上传附件、不调用 API；先说明可提取的内容类型并询问用户。已有明确任务时直接执行。

## 安全与运行约束

- API Key、模型、端点、超时和重试只从 `skills/glm-master-skill/config.toml` 读取。
- 解释器、脚本路径和参数映射由 `skill.toml` 固定；不要在 Termux 安装可选的 `glmocr` 包。
- 只接受本地图片或 PDF 路径。远程 URL 先读取 `skills/glmocr/SKILL.md`，再调用其 `skill__glmocr__extract_text`。
- 不要让宿主 Agent 自行做 OCR，也不要绕过 Manifest 动态工具调用模型。

## 调用

单文件：

```json
{"files":["input/document.pdf"],"pretty":true}
```

多文件并保存 JSON：

```json
{"files":["input/page1.png","input/page2.png"],"output":"output/ocr-results.json","pretty":true}
```

结构化工具参数：

- `files`：一个或多个本地图片/PDF，必需。
- `output`：可选的 UTF-8 JSON 文件。
- `pretty`：格式化 stdout 与输出文件。

## 输出

```json
{
  "success": true,
  "backend": "termux-direct-api",
  "results": []
}
```

`backend=termux-direct-api` 表示使用仓库内 GLM-OCR API 脚本；所有模型交互仍由脚本完成。

将 `results` 中的 Markdown/结构化区域交给宿主 Agent 继续整理。文字/JSON 结果直接回复；
若用户要求保存结果，逐字复用结构化字段 `output` 对应的实际文件路径；工具已从 workspace 启动，
不得再添加 `agent/` 前缀。RClaw 会注入真实来源 channel，不要手填或猜测。先读取
`skills/glm-master-skill/SKILL.md`，再调用 `skill__glm_master_skill__publish_media`，传入
`path=<json>` 和 `media_type=file` 发送生成的 JSON 文件。RClaw 会校验 stdout 中每个 `MEDIA_MARKER=` 并自动把有效标记保留到
最终出站消息；不得手抄或手写媒体标记。没有有效 `MEDIA_MARKER=` 时只报告文件路径，不得声称已发送。

不要在 Termux 安装 `glmocr` 包，也不要改用宿主模型 OCR。
