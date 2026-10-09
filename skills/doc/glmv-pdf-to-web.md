# glmv-pdf-to-web — Prompt 模板

把 PDF（论文/技术报告/项目文档）转成单页学术/项目网站（NeurIPS/CVPR 风格项目主页）：
hero + 摘要 + 贡献 + 方法 + 结果 + 引用等标准章节，自包含 index.html。

- 占位符用 `<>` 标出，替换后即可使用。
- 编号为全集连续编号（P85 起），与其他分册不重号。

整理日期：2026-09-05

---

### P85 论文项目主页
覆盖：完整流程（渲染 → 逐页分析 → 章节 outline.json → grounding 裁剪图表并 PASS/FAIL 复核 → 按宽高比定布局 → 自包含 index.html）

```text
把这篇论文 <PDF路径或URL> 做成一个项目主页/论文网站，
风格 <干净现代的学术风>，中英文跟论文保持一致。
```

---

# 功能点覆盖对照表

| 功能点 | 覆盖的 prompt |
|---|---|
| 完整转换流程（渲染→分析→章节→裁剪→HTML） | P85 |
| 标准七章节（hero/abstract/contributions/method/results/conclusion/citation） | P85 |
| 风格指定或按领域推断 | P85 |
| 视觉风格用户定制 | P85 中替换风格词即可 |
| 语言模式（Chinese/English 不混用） | P85 中说明语言即可 |
| BibTeX 引用块 + Copy 按钮 | 内置行为，P85 产出后验收 |
