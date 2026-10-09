# glmocr-sdk — Prompt 模板

SDK 风格批量 OCR 包装器：逐个复用共享 GLM-OCR 客户端批量解析多份**本地**图片/PDF，
返回 Markdown 与结构化 JSON。唯一支持多文件批量的 OCR 入口；Termux 上自动改用仓库内 API 客户端。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号为全集连续编号（P98 起），与其他分册不重号。
- 只接受本地路径；远程 URL 请改用 glmocr（单文件）。

整理日期：2026-09-05

---

### P98 多文件批量 OCR
覆盖：extract_batch（本地文件数组按序批处理、output 存 UTF-8 JSON、pretty；用户明确点名 SDK/批量时触发）

```text
用 GLM-OCR SDK 批量识别这批本地文件：
<文件1>、<文件2>、<文件3>，
文字、表格、公式都要，结果汇总存成一个 JSON。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| 多文件批量（本地路径数组） | P98 |
| 输出 UTF-8 JSON（output/pretty） | P98 |
| Termux 降级到 API 客户端 | 自动行为，P98 在 Termux 上验收 |
| 远程 URL | 明确不支持——URL 走 glmocr 分册 P93 |
