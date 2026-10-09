---
name: glmv-pdf-to-ppt
description:
  Convert a PDF (research paper, report, or any document) into a polished PowerPoint
  presentation (.pptx) with a structured outline JSON and summary markdown. Trigger this
  skill when the user mentions making slides or a PPT from a PDF — in Chinese or English.
  A PDF attachment by itself is not a conversion request; ask first if recent context has no goal.
---

# PDF → PowerPoint Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后只调用
> Manifest 动态激活的 `skill__glmv_pdf_to_ppt__render_pdf`、
> `skill__glmv_pdf_to_ppt__crop_image` 和 `skill__glmv_pdf_to_ppt__generate_presentation`。
> 不得自行选择 Python、Node、脚本路径或参数。中间文件和成品都放在 workspace 任务目录。
> 宿主 Agent 不得直接查看页面或裁剪图片；每次视觉解释都应读取相应 GLM Skill，调用其动态工具，
> 并只使用返回的文本/JSON。外部 channel 的主交付物是 `presentation.pptx`，HTML 仅为可选预览。
> RClaw 会注入真实来源 channel，不要手填或猜测。先读取 `skills/glm-master-skill/SKILL.md`，
> 再调用 `skill__glm_master_skill__publish_media`，传入 PPTX 的实际 `path` 和
> `media_type=file`。没有有效 `MEDIA_MARKER=` 时，只报告本地产物路径，不得声称已发送。

> **附件意图门禁（强制）：** 仅收到 PDF、且当前消息与紧邻上下文没有“转成 PPT/幻灯片”
> 等明确目标时，不解析 PDF、不渲染页面、不调用动态工具或 API；先询问用户。目标明确时直接生成 `.pptx`。

Convert any PDF into a real `.pptx` presentation. Pages are converted to images at DPI 120,
analyzed through the GLM-5V-Turbo dynamic tool, summarized into `outline.json`/`slides.json`, and
then packaged by `skill__glmv_pdf_to_ppt__generate_presentation`. Crops remain local and `summary.md` accompanies the deck.

脚本入口与参数以同目录 `skill.toml` 为唯一事实源，只通过本 Skill 激活的动态工具调用。

## Dependencies

Python packages (install once):
```bash
pip install pymupdf pillow
```

Termux-compatible setup:
```bash
pkg install python poppler python-pillow
python -m pip install requests
pkg install nodejs
npm install --prefix skills/glmv-pdf-to-ppt/scripts
```

`skill__glmv_pdf_to_ppt__render_pdf` uses PyMuPDF when available and automatically falls back to Termux
`pdftoppm` from the `poppler` package.

## When to Use

Trigger when the user asks to make slides or a presentation from a PDF — phrases like:
"make a PPT from a PDF", "convert PDF to slides", "create a presentation from this paper", "根据pdf做ppt", "根据论文做幻灯片", "做PPT", "做幻灯片", "生成演示文稿", "把这个pdf转成ppt", or any similar intent in Chinese or English.

## Output Directory Convention

All output goes under `{WORKSPACE}/ppt/<pdf_stem>_<timestamp>/`:

```
ppt/
└── <pdf_stem>_<timestamp>/
    ├── outline.json        ← structured slide plan (SlidesPlan schema)
    ├── slides.json         ← input for the PPTX generator
    ├── crops/              ← locally-saved cropped images
    │   ├── slide3_method_crop.png
    │   └── slide5_results_crop.png
    ├── presentation.pptx   ← primary deliverable
    ├── presentation.html   ← optional browser preview only
    └── summary.md          ← final summary document
```

- `<pdf_stem>` = PDF filename without extension
- `<timestamp>` = format `YYYYMMDD_HHMMSS` (e.g. `20240119_143022`)
- Cropped images go in `crops/` subfolder
- Each slide HTML references images via relative path `crops/<name>.png`

## Input

`<pdf_input>` means the PDF path or HTTP/HTTPS URL from the current user request. RClaw
does not define a `$ARGUMENTS` shell variable; substitute the actual value in every command.

- If user provides a **URL**: download with curl first, then convert
- If user provides a **local PDF path**: convert directly

---

## Workflow

### Phase 0 — Create Output Directory

Compute the output path:
```python
import os, datetime
pdf_stem = os.path.splitext(os.path.basename(pdf_path))[0]
timestamp = datetime.datetime.now().strftime("%Y%m%d_%H%M%S")
out_dir = os.path.join(workspace, "ppt", f"{pdf_stem}_{timestamp}")
```

Create it immediately:
```bash
mkdir -p "<out_dir>/crops"
```

Record `out_dir` — use it for all subsequent phases.

---

### Phase 1 — Convert PDF Pages to Images (DPI 120)

本地转换器会核对 `%PDF-` 文件头，拒绝超过 100 MB 或 200 页的输入；动态工具字段 `dpi` 仅允许 36–600，避免异常参数耗尽内存。

If the input is a URL, download it first:
```bash
curl --fail --location --proto '=https,http' --max-filesize 52428800 \
  --output "<out_dir>/source.pdf" -- "<pdf_url>"
```
然后调用 `skill__glmv_pdf_to_ppt__render_pdf`：
```json
{"pdf_source":"<pdf_path>","dpi":120,"out_dir":"<out_dir>/pages"}
```

Outputs JSON to stdout:
```json
[{"page": 1, "path": "/abs/path/page_001.png"}, ...]
```

解析并保存完整的 `page → path` 映射。这些本地路径只传给对应 GLM 动态工具和
`skill__glmv_pdf_to_ppt__crop_image`；宿主 Agent 不直接查看图片。

---

### Phase 2 — Read All Pages in Order

读取 `skills/glm5v-multimodal/SKILL.md`，规划前先通过
`skill__glm5v_multimodal__analyze_images` 按页序分析全部页面图片，每批最多 8 张。把每批结果写到
`<out_dir>/page_analysis/`:

```json
{"prompt":"按页码顺序分析这些 PDF 页面，提取结构、论点、图表、表格、关键数字和适合演示的视觉素材。","images":["<page_001.png>","<page_002.png>"],"output":"<out_dir>/page_analysis/batch_001.md"}
```

宿主 Agent 只读取这些文本分析文件，不直接查看页面图片。

While reading, note:
- What figures, charts, or tables appear on which pages
- The overall arc (intro → method → results → conclusion for papers; or logical structure for other doc types)
- Candidate visuals worth cropping for slides (page number + rough region)

Do NOT plan or write slides yet — just read and understand all pages first.

---

### Phase 3 — Plan Outline & Save outline.json

After reading all pages, plan 8–15 slides (adapt freely for non-academic documents).

| Slide | Typical purpose |
|-------|----------------|
| 1 | Title, authors, affiliation, venue/year |
| 2 | Motivation / Problem statement |
| 3 | Related Work (brief) |
| 4–N-2 | Method / Core contributions (one concept per slide) |
| N-1 | Results & Experiments |
| N | Conclusion & Future Work |

For each slide that needs a visual, identify:
- Which page it comes from (the local page path from Phase 1)
- A description of what the visual shows and why it belongs on this slide

**Save the outline as `<out_dir>/outline.json`** using exactly this schema:

```json
{
  "presentation_title": "Paper Title Here",
  "lang": "Chinese",
  "total_slides": 10,
  "slides_plan": [
    {
      "slide_index": 1,
      "title": "Slide Title",
      "main_content": "Key points and text content for this slide",
      "template_id": null,
      "required_crops": [
        {
          "url": "<page_image_url_from_phase1>",
          "visual_description": "Figure 3: architecture diagram showing encoder-decoder",
          "usage_reason": "Illustrates the core model structure for slide 4"
        }
      ]
    }
  ]
}
```

Field notes:
- `lang`: `"Chinese"` or `"English"` — match the PDF language
- `template_id`: always `null`
- `required_crops`: empty array `[]` if this slide needs no images
- `url` in each crop: the **local file path** of the source page image (from Phase 1 `path` field) — pass this path to `skill__glmv_pdf_to_ppt__crop_image`
- `visual_description`: what the visual shows, including figure/table number if available
- `usage_reason`: why this visual belongs on this particular slide

- For images that need cropping, note the approximate region — exact crop boxes are determined in Phase 4

Save the resulting UTF-8 JSON as `<out_dir>/outline.json`.

---

### Phase 4 — Crop Required Images with GLM Grounding

以文本方式读取 `outline.json`，按源页面对裁剪目标分组。宿主 Agent 不得自行查看页面像素，
也不得把视觉解释委派给另一个 Agent。目标定位技能（`glmv-grounding`）已随跨端清理删除；
裁剪阶段降级为按 `outline.json` 的页码直接使用整页渲染图，不再做 bbox 目标裁剪（用户需要
局部图时可提示在查看器中手动裁剪）：

```json
{"image":"<page_image_path>","prompt":"Locate these named elements: <name and description list>. Return an objects-detection list with label and bbox_2d. Coordinates must be normalized 0-1000 and tightly include each complete element."}
```

只使用工具返回的 JSON。确认 `ok` 为 true，并把每个请求名称匹配到一个
四元素 `bbox_2d`；目标缺失时不得编造坐标。需要时用文件工具保存返回的 JSON。通过
`skill__glmv_pdf_to_ppt__crop_image` 裁剪每个已接受的框：

```json
{"path":"<page_image_path>","box":[0,0,1000,1000],"name":"slide<N>_<descriptive_name>","out_dir":"<out_dir>/crops"}
```

裁剪工具会返回 `path`、`width`、`height` 和 `aspect`；例如
`{"path":"<workspace>/slide3_method_crop.png","width":800,"height":600,"aspect":1.33}`。
不要直接查看裁剪结果，
应通过 `skill__glm5v_multimodal__analyze_images` 验证：

```json
{"images":["<out_dir>/crops/<crop_name>_crop.png"],"prompt":"Does this crop contain the complete target '<target description>' with little unrelated content? Answer PASS or FAIL and explain any missing edge or excess region.","output":"<out_dir>/crops/<crop_name>_review.md"}
```

On `FAIL`, rerun grounding with the review text in the prompt, crop again, and recheck.
Build `slide_index → [crop filename, ...]` only from verified crops.

---

### Phase 5 — Measure Cropped Image Dimensions

Reuse the `width`, `height`, and `aspect` fields returned by each successful
`skill__glmv_pdf_to_ppt__crop_image` call. Do not run a separate inline script to
measure the generated files.

Use aspect ratios to pick each slide's layout:

| Aspect ratio | Layout recommendation |
|---|---|
| **< 0.7** (tall/narrow) | `text + image` side-by-side — `max-height: 600px` on image |
| **0.7 – 1.3** (square-ish) | `text + image` — image takes ~50% width |
| **> 1.3** (wide) | Image on top or bottom, text above/below |
| **> 2.0** (very wide, e.g. tables) | `full-image` — spans full 1280px width, caption below |

---

### Phase 6 — Generate the PowerPoint

Create `<out_dir>/slides.json` from `outline.json` and the verified crops:

```json
{
  "title": "Presentation title",
  "subtitle": "Optional subtitle",
  "slides": [
    {
      "title": "Slide title",
      "bullets": ["Point one", "Point two"],
      "images": [
        {"path": "crops/slide3_method_crop.png", "caption": "Optional caption"}
      ]
    }
  ]
}
```

正文保持简洁，每页最多 5–6 个要点。然后调用
`skill__glmv_pdf_to_ppt__generate_presentation`：

```json
{"slides_json":"<out_dir>/slides.json","output":"<out_dir>/presentation.pptx"}
```

The command must exit successfully and report the expected slide count. The HTML slide
generator is optional preview tooling only and never replaces `presentation.pptx`.

---

### Phase 7 — Generate summary.md

Write `<out_dir>/summary.md` in the same language as the slides (`lang` from `outline.json`).

Include:
- Document title and basic info (authors, venue, year if applicable)
- Brief abstract/overview (2–3 sentences)
- Per-slide breakdown table: slide number, title, 1–2 sentence summary
- Main contributions or takeaways (bullet list)
- Link to `presentation.pptx`

Example structure:
```markdown
# [Presentation Title]

> **来源 / Source:** [PDF filename] | **语言 / Language:** Chinese | **幻灯片数 / Slides:** 10

## 摘要
[2-3 sentence overview]

## 幻灯片概览
| # | 标题 | 主要内容 |
|---|------|---------|
| 1 | 标题页 | ... |
...

## 主要贡献
- ...

## 📂 演示文稿
[下载 PowerPoint](presentation.pptx)
```

---

## Optional HTML Preview Spec

Each slide is a **standalone HTML file** — full `<html>…</html>` with embedded CSS only.

**Canvas:** fixed `1280 × 720 px`, `overflow: hidden` — nothing scrolls.

**Consistent design across all slides:**
- Choose a visual style that fits the document's domain and tone — no fixed palette or font required
- If the user specifies a style, follow it exactly; otherwise infer from the content (e.g. a ML paper → clean modern; a historical report → editorial serif; a product pitch → bold and branded)
- Same fonts, colors, and spacing system applied uniformly to every slide
- Every slide shows: slide title, page counter (bottom-right corner), presentation title (subtle footer)

**Navigation on each slide:**
- Two transparent click areas cover the full slide height: left 50% → previous slide, right 50% → next slide
- On slide 1 the left area is inert; on the last slide the right area is inert
- Keyboard `←` / `→` arrows also navigate
- No visible buttons needed — optionally show a subtle `‹` / `›` hint at the edges that fades in on hover

**Layout patterns:**
- `title-card` — centered hero, large title, authors/venue below
- `text-only` — structured bullet points, max 5–6 items, generous whitespace
- `text + image` — image right or left, text opposite
- `full-image` — image fills canvas, minimal text overlay
- `grid` — 2×2 or 3-column figures with captions

**Images:**
- Use relative paths: `crops/<name>_crop.png`
- Add `style="object-fit: contain; max-width: 100%; max-height: 100%;"`
- Add captions below in small italic text

**Do NOT:**
- Use external JS frameworks or icon CDNs
- Use placeholder/stock images — only the cropped PDFs
- Generate generic purple-gradient-on-white slides
- Let content overflow the 720px height

---

## Quality Checklist

- [ ] Output directory named `<pdf_stem>_<timestamp>/`
- [ ] `outline.json` saved with valid SlidesPlan schema
- [ ] `slides.json` saved and every referenced image exists
- [ ] All crops saved to `crops/` (local only, no cloud upload)
- [ ] `presentation.pptx` opens and has the expected slide count
- [ ] No slide content overflows the 16:9 canvas
- [ ] Consistent theme across all slides
- [ ] Slide number and presentation title visible on every slide
- [ ] `summary.md` written in the correct language, links to `presentation.pptx`

---

## Language

Match the PDF language. Chinese PDF → Chinese slides and summary. English → English. No mixing.
