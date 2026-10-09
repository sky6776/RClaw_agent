---
name: glmv-resume-screen
description:
  Screen and evaluate resumes against criteria using ZhiPu GLM-V multimodal model.
  Reads multiple resume files (PDF/DOCX/TXT), compares against user-defined screening
  criteria, and outputs a Markdown table with pass/fail analysis. Use when the user
  wants to filter resumes, compare candidates, or batch-evaluate job applications.
  Resume attachments alone are not authorization to screen; ask first if no goal or criteria exist in recent context.
---

# GLM-V Resume Screening Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后必须调用
> `skill__glmv_resume_screen__screen_resumes`，不得自行选择解释器、脚本路径或参数。
> API Key 和模型请求设置只从 `skills/glm-master-skill/config.toml` 的
> `[zhipu]`、`[glm5v]`、`[resume_screen]` 读取。
> 宿主 Agent 不得自行读取简历内容。本工具直接处理远程 PDF/DOC/DOCX/TXT URL 和
> workspace 内的本地 PDF；本地 DOC/DOCX/TXT 应先读取 `skills/glm5v-multimodal/SKILL.md`，
> 再调用 `skill__glm5v_multimodal__analyze_files`，并使用其 Markdown/JSON 文本结果。
> RClaw 会注入真实来源 channel，不要手填或猜测。用户要求把保存的 `output` 作为外部附件时，
> 先读取 `skills/glm-master-skill/SKILL.md`，再调用 `skill__glm_master_skill__publish_media`，
> 传入实际 `path` 和 `media_type=file`。
> 工作目录已经是 workspace，不要添加 `agent/` 前缀。RClaw 会校验并自动保留有效
> `MEDIA_MARKER=`；不得手写或复制标记。没有有效标记时，只报告本地文件路径，不得声称已发送。

> **附件意图门禁（强制）：** 仅收到简历且当前消息与紧邻上下文没有筛选目标或标准时，
> 不调用动态工具、不解析或上传简历、不调用 API；先询问用户。已有明确岗位/标准时直接执行。

Batch-read resumes and screen candidates against your criteria using the ZhiPu GLM-V multimodal model.

## When to Use

- Filter/screen multiple resumes against specific criteria
- User mentions "筛选简历", "评估候选人", "简历筛选", "resume screening", "filter candidates"
- Compare candidates for a job position
- Batch-evaluate job applications

## Supported Input Types

| Type           | Formats                  | Max Count                      | Source                         |
| -------------- | ------------------------ | ------------------------------ | ------------------------------ |
| Resume (URL)   | pdf, docx, txt           | 50                             | 动态工具字段 `files`           |
| Resume (Local) | pdf                         | 50 pages maximum                | `skill__glmv_resume_screen__screen_resumes` |

> 本工具处理远程简历 URL 与本地 PDF；不要在同一次调用中混用两类输入。本地 Office/文本
> 简历改用 `skill__glm5v_multimodal__analyze_files`。

### 📋 Output Display Rules (MANDATORY)

After the dynamic tool returns, **you must display the complete screening result (Markdown table) exactly as returned**. Do not summarize, truncate, or only say "screening completed". Users need each candidate's detailed analysis to decide.

- Show the full Markdown table (index, name, pass/fail, match level, reasoning)
- If the `output` field was provided, give the saved file path and show its content
- If screening output is empty, explain why

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

### Basic Screening

```json
{"files":["https://example.com/resume1.pdf","https://example.com/resume2.docx"],"criteria":"3年以上工作经验，有Python开发经验，有大型项目管理经验"}
```

### Save as Markdown

```json
{"files":["https://example.com/resume1.pdf","https://example.com/resume2.docx"],"criteria":"本科以上学历，5年后端开发经验","output":"result.md"}
```

### Save as JSON

```json
{"files":["https://example.com/resume1.pdf","https://example.com/resume2.docx"],"criteria":"有机器学习经验","output":"result.json","pretty":true}
```

### Custom System Prompt

```json
{"files":["https://example.com/resume1.pdf"],"criteria":"前端开发岗位，3年经验","system_prompt":"你是一位资深技术面试官，特别关注候选人的项目深度和技术选型能力"}
```

## Output Example

The model outputs a Markdown table like this:

```markdown
| 序号 | 候选人姓名 | 是否符合    | 符合程度 | 原因分析                                                    |
| ---- | ---------- | ----------- | -------- | ----------------------------------------------------------- |
| 1    | 张三       | ✅ 符合     | 高       | 5年后端经验，熟练使用Python和Django，主导过3个大型项目      |
| 2    | 李四       | ❌ 不符合   | 低       | 仅1年开发经验，主要使用Java，无Python经验                   |
| 3    | 王五       | ⚠️ 部分符合 | 中       | 3年Python经验但无项目管理经验，技术栈匹配但缺乏大型项目经历 |
```

## 结构化工具参数

| Parameter               | Required | Description                                                |
| ----------------------- | -------- | ---------------------------------------------------------- |
| `files`                 | 是       | 远程简历 URL（pdf/docx/txt）或本地 PDF，最多 50 个文件/页     |
| `criteria`              | 是       | 完整筛选标准                                                   |
| `system_prompt`         | 否       | 自定义系统提示词；默认使用专业招聘助理                         |
| `output`                | 否       | 结果文件；Markdown 用 `.md`，JSON 用 `.json`                   |
| `pretty`                | 否       | 格式化 JSON 输出                                               |

## Error Handling

The script preflights the complete batch before rendering or calling the API and returns all
detectable path, format, URL, PDF-open, combined-page-count, and mixed-input errors together.
Fix the whole reported set before rerunning.

Python/JSON 返回值包含 `finish_reason` 与 `complete`；Markdown 或 stdout 模式在结果不完整时会把警告写到 stderr。输出目录不存在时脚本会创建，`.md` 扩展名按大小写无关处理。
本地 PDF 渲染后还会逐页复核 5 MB 图片上限，超限页面会在请求前明确报错。

**API key not configured:** → Guide user to `[zhipu].api_key` in the shared TOML

**Authentication failed (401/403):** → API key invalid/expired → reconfigure

**Rate limit (429):** → Quota exhausted → wait and retry

**不支持的本地文件：** 本工具只直接接受本地 PDF；其它本地简历格式先读取
`skills/glm5v-multimodal/SKILL.md`，再调用 `skill__glm5v_multimodal__analyze_files`。

**Output incomplete:** → `warning` 字段或 stderr 警告表明 token 截断或安全拦截

**Timeout:** → Resumes too large or too many → reduce file count
