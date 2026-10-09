# glmocr — Prompt 模板

GLM-OCR 通用文字提取：从图片/PDF（本地或 HTTPS URL）提取正文、表格、公式、手写内容和
版面信息，输出 Markdown（表格转 Markdown 表格、公式转 LaTeX）。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号为全集连续编号（P93 起），与其他分册不重号。
- 无兜底：只能走 GLM-OCR API；附件无明确目标时先询问。

整理日期：2026-09-05

---

### P93 通用文字识别
覆盖：extract_text（本地图片/PDF 或远程 URL 二选一 → Markdown 正文，含表格/公式/手写/版面）

```text
识别这张图片里的文字：<图片路径或URL>。
```

### P94 PDF 指定页范围 + 保存 JSON
覆盖：start_page/end_page 页范围（1 计数）+ output 存 JSON + pretty

```text
提取 <PDF路径> 第 <3> 到 <10> 页的文字，结果存成 JSON。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| 图片/PDF 文字提取（本地/URL） | P93 |
| 表格转 Markdown 表格 | P93（换表格图即可验收） |
| 公式转 LaTeX | P93（换公式图即可验收） |
| PDF 页范围（start_page/end_page） | P94 |
| 结果保存 JSON（output/pretty） | P94 |
| include_raw 调试模式 | 调试用途，非用户话术功能 |
