---
name: glmv-doc-based-writing
description:
  Write a textual content based on given document(s) and requirements, using ZhiPu GLM-V multimodal model.
  Read and comprehend one or multiple documents (PDF/DOCX), write a content in Markdown format according to the specified requirements. Use when the user wants to draft a paper/article/essay/report/review/post/brief/proposal/plan, etc. Do not process an attachment alone; ask first when the current message and recent context do not specify the desired writing task.
---

# GLM-V Document-Based Writing Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后必须调用
> `skill__glmv_doc_based_writing__write_from_documents`，不得自行选择解释器、脚本路径或参数。
> API Key 和模型请求设置只从 `skills/glm-master-skill/config.toml` 的
> `[zhipu]`、`[glm5v]`、`[doc_based_writing]` 读取。
> 宿主 Agent 只编排和编辑工具返回的文本，不得自行理解源文档。本工具直接处理远程
> PDF/DOC/DOCX URL 和 workspace 内的本地 PDF；其它本地 Office/文本文件应先读取
> `skills/glm5v-multimodal/SKILL.md`，再调用 `skill__glm5v_multimodal__analyze_files`。
> RClaw 会注入真实来源 channel，不要手填或猜测。用户要求把保存的 `output` 作为外部附件时，
> 先读取 `skills/glm-master-skill/SKILL.md`，再调用 `skill__glm_master_skill__publish_media`，
> 传入实际 `path` 和 `media_type=file`。
> 工作目录已经是 workspace，不要添加 `agent/` 前缀。RClaw 会校验并自动保留有效
> `MEDIA_MARKER=`；不得手写或复制标记。没有有效标记时，只报告本地文件路径，不得声称已发送。

> **附件意图门禁（强制）：** 当前消息与紧邻上下文均未明确文稿类型、目的或写作要求时，
> 不调用动态工具、不解析或上传文档、不调用 API；先询问用户希望生成什么。已有明确任务时直接执行。

Comprehend the given document(s) and write a textual content (paper/article/essay/report/review/post/brief/proposal/plan) according to your requirements using the ZhiPu GLM-V multimodal model.

## When to Use

- Write a textual content according to specified requirements, AFTER reading provided document(s)
- User mentions "基于文档的写作", "文章撰写", "文档解读", "新闻稿撰写", "简报撰写", "影评/书评撰写", "内容总结", "内容创作", "评论写作", "文档续写", "文档翻译", "方案策划", "发言稿撰写", "document-based writing", "article writing", "document reading", "press release writing", "brief writing", "film/book review writing", "content summarization", "content creation", "commentary writing", "document continuation", "document translation", "proposal planning ", "speech writing"

## Supported Input Types

| Type             | Formats                                         | Max Count                         | Source                     |
| ---------------- | ----------------------------------------------- | --------------------------------- | -------------------------- |
| Document (URL)   | pdf, doc, docx                                  | 50                                | 动态工具字段 `files`       |
| Document (Local) | pdf                                              | 50 pages maximum                  | `skill__glmv_doc_based_writing__write_from_documents` |

> 本工具处理远程文档 URL 与本地 PDF；不要在同一次调用中混用两类输入。其它本地
> Office/文本文件改用 `skill__glm5v_multimodal__analyze_files`。

### 📋 Output Display Rules (MANDATORY)

After the dynamic tool returns, **you must display the complete content (Markdown format) exactly as returned**. Do not summarize, truncate, translate, comment, or only say "Writing Completed!".

## Resource Links

| Resource        | Link                                                                                                                              |
| --------------- | --------------------------------------------------------------------------------------------------------------------------------- |
| **Get API Key** | [https://bigmodel.cn/usercenter/proj-mgmt/apikeys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys)                              |
| **API Docs**    | [Chat Completions / 对话补全](https://docs.bigmodel.cn/api-reference/%E6%A8%A1%E5%9E%8B-api/%E5%AF%B9%E8%AF%9D%E8%A1%A5%E5%85%A8) |

## Prerequisites

### API Key Setup / API Key 配置（Required / 必需）

This script reads all model settings from `skills/glm-master-skill/config.toml` and does not read model configuration from environment variables.
脚本从 `skills/glm-master-skill/config.toml` 读取全部模型配置，不从环境变量读取模型配置。

**Get Key / 获取 Key：** Visit [Zhipu Open Platform API Keys / 智谱开放平台 API Keys](https://bigmodel.cn/usercenter/proj-mgmt/apikeys) to create or copy your key.

Fill `[zhipu].api_key` in the shared TOML. Do not pass the key in command arguments or
write it to logs or generated artifacts.

## How to Use

### Basic Writing

```json
{"files":["https://example.com/document1.pdf","https://example.com/document2.docx"],"requirements":"基于这篇论文撰写公众号文章，要求偏技术风格"}
```

### Save as Markdown

```json
{"files":["https://example.com/document1.pdf","https://example.com/document2.docx"],"requirements":"总结文档主要内容和核心观点","output":"result.md"}
```

### Save as JSON

```json
{"files":["https://example.com/document1.pdf","https://example.com/document2.docx"],"requirements":"撰写新闻稿","output":"result.json","pretty":true}
```

### Custom System Prompt

```json
{"files":["https://example.com/document1.pdf"],"requirements":"为这本书撰写书评","system_prompt":"你是一位拥有20年跨领域写作经验的资深写作专家，擅长撰写书评"}
```

## 结构化工具参数

| Parameter               | Required | Description                                                |
| ----------------------- | -------- | ---------------------------------------------------------- |
| `files`                  | 是       | 远程文档 URL（pdf/doc/docx）或本地 PDF，最多 50 个文件/页      |
| `requirements`           | 是       | 完整写作要求                                                    |
| `system_prompt`          | 否       | 自定义系统提示词；默认使用通用写作专家                          |
| `output`                 | 否       | 结果文件；Markdown 用 `.md`，JSON 用 `.json`                    |
| `pretty`                 | 否       | 格式化 JSON 输出                                                |

## Error Handling

The script preflights the complete batch before rendering or calling the API and returns all
detectable path, format, URL, PDF-open, combined-page-count, and mixed-input errors together.
Fix the whole reported set before rerunning.

Python/JSON 返回值包含 `finish_reason` 与 `complete`；Markdown 或 stdout 模式在结果不完整时会把警告写到 stderr。输出目录不存在时脚本会创建，`.md` 扩展名按大小写无关处理。
本地 PDF 渲染后还会逐页复核 5 MB 图片上限，超限页面会在请求前明确报错。

**API key not configured:** → Guide user to `[zhipu].api_key` in the shared TOML

**Authentication failed (401/403):** → API key invalid/expired → reconfigure

**Rate limit (429):** → Quota exhausted → wait and retry

**不支持的本地文件：** 本工具只直接接受本地 PDF；其它本地文档格式先读取
`skills/glm5v-multimodal/SKILL.md`，再调用 `skill__glm5v_multimodal__analyze_files`。

**Output incomplete:** → `warning` 字段或 stderr 警告表明 token 截断或安全拦截

**Timeout:** → Documents too large or too many → reduce file count
