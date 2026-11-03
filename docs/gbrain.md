# gbrain 知识库

[← 返回 README](../README.md)

gbrain 是进程内的知识库子系统（`crates/rclaw-gbrain`，lib-only），为 agent 提供长期「脑页」记忆、文档知识库、混合语义检索与制品管理。目前**仅随 Android APK 进程内集成**；桌面 / CLI 不链接该 crate，但可通过 [MCP 扩展](mcp.md) 外接其它 gbrain server（本仓库不提供可直接启动的 gbrain 可执行文件）。

## 子系统组成

- **存储**：SQLite + sqlite-vec（向量）+ FTS5（全文）。
- **长期脑页**：稳定知识页面，Artifact 可投影到页面并保留来源追溯。
- **Artifact 与路由**：上传的制品按意图与文件类型路由到知识库、稳定脑页、影子页或附件。
- **KB 文档管线**：解析 → 分块 → embedding → 索引，支持页级 OCR 状态。
- **混合检索**：融合标题、FTS5、向量、摘要、表格与元数据，以 RRF（可选 rerank）排序。
- **OCR**：后端为智谱 GLM-OCR；直接 OCR 支持 PDF、PNG / JPG / JPEG；DOCX / XLSX 等内嵌图片在解析管线中自动 OCR。
- **RAPTOR**：可按库启用，以 KMeans 聚类递归生成父摘要节点，默认最多两层；LLM 失败时退化为拼接摘要。
- **后台 worker / autopilot**：异步处理索引与维护作业。

## 支持的文档类型

`pdf` `docx` `xls` `xlsx` `csv` `tsv` `html` `htm` `txt` `md` `markdown` `rst` `json` `xml` `yaml` `yml` `toml` `png` `jpg` `jpeg`。PDF、DOCX、XLS / XLSX、CSV、HTML、Markdown（`md`）有专用解析器，其余文本扩展名（含 `markdown`）走 UTF-8 文本回退。上传受 `GBRAIN_KB_MAX_FILE_SIZE_MB` 与 `GBRAIN_KB_ALLOWED_EXTENSIONS` 白名单约束；源代码扩展名走 code import / sync，不进 KB。

## 工具（18 个）

| 工具 | 说明 |
|---|---|
| `artifact_put` | 写入长期记忆（脑页） |
| `artifact_upload` | 上传制品并按意图 / 类型路由 |
| `artifact_query` | 融合查询（全文 + 向量 + 摘要 + 表格 + 元数据） |
| `artifact_list` / `artifact_get` | 列表 / 详情 |
| `artifact_delete` | 软删除 |
| `artifact_detach` | 解除与页面的关联 |
| `artifact_restore` | 恢复已删除制品 |
| `artifact_reprocess` | 重新解析 / 索引 |
| `artifact_health` | 一致性检查：孤立 / 陈旧投影、来源追溯、缺失原件、卡住超过 24 小时的 KB 作业 |
| `artifact_review_list` / `artifact_review_get` | 查看待审核的提升建议 |
| `artifact_review_apply` | 应用建议（自动接受 pending） |
| `artifact_review_reject` | 拒绝建议 |
| `artifact_review_rollback` | 回滚（只允许目标页最新的 applied 变更） |
| `kb_document_status` | 查文档与页级 OCR 状态 |
| `kb_ocr_run` | 触发 OCR |
| `kb_ocr_retry` | 重试失败、空结果或待处理页 |

审核实体状态：`pending → accepted / applied`、`rejected`、`rolled_back`。

## 配置

在 `rclaw.toml` 的 `[mcp.servers.gbrain]` 段配置；安卓端忽略 `command` / `args` / `cwd`（进程内直连，无 MCP 子进程）。

```toml
[mcp.servers.gbrain]
enabled = true
auto_start = true
startup_timeout_secs = 30
call_timeout_secs = 120

[mcp.servers.gbrain.env]
GBRAIN_EMBEDDING_API_KEY = "..."
GBRAIN_EMBEDDING_BASE_URL = "https://open.bigmodel.cn/api/paas/v4"
GBRAIN_EMBEDDING_MODEL = "embedding-3"
GBRAIN_EMBEDDING_DIMENSIONS = "1536"
# ... 见 rclaw.toml.example 的完整列表
```

`env` 键按用途分组（完整清单与注释见 [`rclaw.toml.example`](../rclaw.toml.example)）：

| 组 | 键 | 必要性 |
|---|---|---|
| 路径 | `GBRAIN_DIR`、`GBRAIN_DB_PATH`、`GBRAIN_KB_STORAGE_DIR`、`GBRAIN_ARTIFACT_STORAGE_DIR`、`GBRAIN_LOG_FILE_PATH` | 严格校验（安卓自动归一化，可不配） |
| embedding | `GBRAIN_EMBEDDING_{API_KEY,BASE_URL,MODEL,DIMENSIONS}` | 必需；OpenAI 兼容 `/embeddings`，Bearer 鉴权 |
| 查询扩展 / 分块 / RAPTOR | `GBRAIN_EXPANSION_*`、`GBRAIN_CHUNKER_*`、`GBRAIN_KB_RAPTOR_*`（各含 `API_KEY` / `BASE_URL` / `MODEL`） | 必需 |
| OCR | `GBRAIN_OCR_ENABLED` 与全部阈值 / 容量 / 模式键（`ALLOW_CUSTOM_BASE_URL`、`PROFILE`、`ENABLE_LAYOUT`、`MODE`、`SUBMIT_MODE`、`*_THRESHOLD`、`MAX_*`、`TEMP_DIR_MAX_BYTES`、`RETURN_CROP_IMAGES`、`NEED_LAYOUT_VISUALIZATION`、`SYNC_INLINE`） | **始终必需**（无论是否启用 OCR） |
| OCR 凭据 | `GBRAIN_OCR_API_KEY` / `GBRAIN_OCR_BASE_URL` / `GBRAIN_OCR_MODEL` | 仅 `GBRAIN_OCR_ENABLED = true` 时必需 |
| 知识库 | `GBRAIN_KB_ENABLED`、`GBRAIN_KB_MAX_FILE_SIZE_MB`、`GBRAIN_KB_ALLOWED_EXTENSIONS`、`GBRAIN_KB_WORKER_*` | 必需 |
| 语音转写 | `GBRAIN_TRANSCRIPTION_PROVIDER` 与所选后端的 `GBRAIN_TRANSCRIPTION_GROQ_*` / `GBRAIN_TRANSCRIPTION_OPENAI_*` | 所选后端的 Key 必需 |
| 日志 / 页面 / worker / autopilot / Artifact 策略 | `GBRAIN_LOG_*`、`GBRAIN_AUTO_*`、`GBRAIN_AUTOPILOT_*`、`GBRAIN_ARTIFACT_*`、`GBRAIN_UPLOAD_PROMOTION_POLICY`、`GBRAIN_POST_WRITE_LINT` | 示例中给出的全部开关与阈值 |
| 可选 | `GBRAIN_EMBEDDING_CONCURRENCY`、`GBRAIN_PAGE_CHUNKER_MODE`、`GBRAIN_RUST_LOG`、`GBRAIN_DEFAULT_KB_LIBRARY_ID`、`GBRAIN_PROGRESS_MODE` / `GBRAIN_PROGRESS_JSON`、`GBRAIN_ASYNC_WORKER_THREADS`、`GBRAIN_SEARCH_DEBUG` / `GBRAIN_SEARCH_EXCLUDE` / `GBRAIN_SOURCE_BOOST` | 可选 |

所有键都必须带 `GBRAIN_` 前缀：安卓端对不带前缀的键**静默忽略**。缺少任一必需键时子系统启动失败并给出键名。

## 数据目录（安卓）

数据自动归一化到应用私有 `files/gbrain`：`brain.db`、`kb/`、`artifacts/`、`logs/gbrain.log`。空值、外平台绝对路径、含 `..` 或逃逸私有根的路径一律回退默认值，并拒绝符号链接祖先。
