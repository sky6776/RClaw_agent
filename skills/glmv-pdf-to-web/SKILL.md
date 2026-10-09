---
name: glmv-pdf-to-web
description:
  Convert a PDF (research paper, technical report, or project document) into a beautiful
  single-page academic/project website with a structured outline JSON. Trigger this
  skill when the user wants to make a paper page, project homepage, or academic website
  from a PDF — in Chinese or English. A PDF attachment by itself is not a website request;
  ask first if the current message and recent context have no processing goal.
---

# PDF → Academic Project Website Skill

> **RClaw 移植说明（基于上游提交 `2ecd31c`）：** 读取本 Skill 后只调用
> Manifest 动态激活的 `skill__glmv_pdf_to_web__render_pdf`、
> `skill__glmv_pdf_to_web__crop_image` 和 `skill__glmv_pdf_to_web__generate_website`。
> 不得自行选择解释器、脚本路径或参数。中间文件和成品都放在 workspace 任务目录。
> 宿主 Agent 不得直接查看页面或裁剪图片；每次视觉解释都应读取相应 GLM Skill，调用其动态工具，
> 并只使用返回的文本/JSON。外部 channel 的主交付物是独立 `index.html`。
> RClaw 会注入真实来源 channel，不要手填或猜测。先读取 `skills/glm-master-skill/SKILL.md`，
> 再调用 `skill__glm_master_skill__publish_media`，传入 HTML 的实际 `path` 和
> `media_type=file`。没有有效 `MEDIA_MARKER=` 时，只报告本地产物路径，不得声称已发送。

> **附件意图门禁（强制）：** 仅收到 PDF、且当前消息与紧邻上下文没有“转成网页/项目主页”
> 等明确目标时，不解析 PDF、不渲染页面、不调用动态工具或 API；先询问用户。目标明确时直接生成 HTML。

把论文或技术文档 PDF 转为适合 NeurIPS/CVPR/ICLR 项目发布的单页网站。页面在本地按
DPI 120 渲染，保存结构化 `outline.json` 并在本地裁剪图片，最后通过
`skill__glmv_pdf_to_web__generate_website` 保存网页。

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
```

`skill__glmv_pdf_to_web__render_pdf` uses PyMuPDF when available and automatically falls back to Termux
`pdftoppm` from the `poppler` package.

## When to Use

Trigger when the user asks to create a webpage or project page from a PDF — phrases like:
"make a project page from a PDF", "create a paper website", "build an academic website for this paper", "论文主页", "做项目主页", "根据pdf做网页", "把论文做成主页", or any similar intent in Chinese or English.

## Output Directory Convention

All output goes under `{WORKSPACE}/web/<pdf_stem>_<timestamp>/`:

```
web/
└── <pdf_stem>_<timestamp>/
    ├── outline.json        ← structured web plan (WebPlan schema)
    ├── crops/              ← locally-saved cropped images
    │   ├── fig_arch_crop.png
    │   ├── table_results_crop.png
    │   └── ...
    └── index.html          ← the website
```

- `<pdf_stem>` = PDF filename without extension
- `<timestamp>` = format `YYYYMMDD_HHMMSS`
- HTML references images via relative path `crops/<name>_crop.png`

## Input

`<pdf_input>` means the PDF path or HTTP/HTTPS URL from the current user request. RClaw
does not define a `$ARGUMENTS` shell variable; substitute the actual value in every command.

- If user provides a **URL**: download with curl first, then convert
- If user provides a **local PDF path**: convert directly

---

## Workflow

### Phase 0 — Create Output Directory

```python
import os, datetime
pdf_stem = os.path.splitext(os.path.basename(pdf_path))[0]
timestamp = datetime.datetime.now().strftime("%Y%m%d_%H%M%S")
out_dir = os.path.join(workspace, "web", f"{pdf_stem}_{timestamp}")
```

```bash
mkdir -p "<out_dir>/crops"
```

---

### Phase 1 — Convert PDF Pages to Images (DPI 120)

本地转换器会核对 `%PDF-` 文件头，拒绝超过 100 MB 或 200 页的输入；动态工具字段 `dpi` 仅允许 36–600，避免异常参数耗尽内存。

If the input is a URL, download it first:
```bash
curl --fail --location --proto '=https,http' --max-filesize 52428800 \
  --output "<out_dir>/source.pdf" -- "<pdf_url>"
```
然后调用 `skill__glmv_pdf_to_web__render_pdf`：
```json
{"pdf_source":"<pdf_path>","dpi":120,"out_dir":"<out_dir>/pages"}
```

Outputs JSON to stdout:
```json
[{"page": 1, "path": "/abs/path/page_001.png"}, ...]
```

Parse and store the full `page → path` map.

---

### Phase 2 — Read All Pages in Order

读取 `skills/glm5v-multimodal/SKILL.md`，规划前先通过
`skill__glm5v_multimodal__analyze_images` 按页序分析全部页面图片，每批最多 8 张。把每批结果写到
`<out_dir>/page_analysis/`:

```json
{"prompt":"按页码顺序分析这些 PDF 页面，提取标题、作者、摘要、贡献、链接、图表、表格和适合网页展示的素材。","images":["<page_001.png>","<page_002.png>"],"output":"<out_dir>/page_analysis/batch_001.md"}
```

宿主 Agent 只读取这些文本分析文件，不直接查看页面图片。

While reading, note:
- Title, authors, affiliations, venue, year
- Abstract text (verbatim)
- Key contributions
- Paper/Code/Dataset links (arXiv, GitHub, etc.)
- Figures, tables, diagrams — which pages, rough regions
- Teaser/hero figure if present

Do NOT plan sections yet — read everything first.

---

### Phase 3 — Plan Sections & Save outline.json

Plan the website sections. Standard structure for academic papers (adapt as needed):

| `section_id` | Purpose |
|---|---|
| `hero` | Title, authors, venue badge, link buttons |
| `abstract` | Full abstract text |
| `contributions` | 3–5 key contribution cards |
| `method` | Architecture figure + method explanation |
| `results` | Quantitative table + qualitative figures |
| `conclusion` | Brief conclusion |
| `citation` | BibTeX block |

For each section that needs an image, identify:
- Which page it comes from (the local page path from Phase 1)
- A description of what the visual shows and why it belongs in this section

**Save as `<out_dir>/outline.json`** using exactly this schema:

```json
{
  "project_title": "Paper Title",
  "lang": "English",
  "authors": ["Author One", "Author Two"],
  "sections_plan": [
    {
      "section_index": 1,
      "section_id": "hero",
      "title": "Hero",
      "content": "Title, authors, venue, teaser figure description",
      "required_images": [
        {
          "url": "<local_page_path_from_phase1>",
          "visual_description": "Figure 1: teaser showing input-output examples",
          "usage_reason": "Hero section visual to immediately show the paper's output"
        }
      ]
    }
  ]
}
```

Field notes:
- `lang`: `"Chinese"` or `"English"` — match the PDF language
- `required_images`: empty array `[]` if section needs no images
- `url`: the **local file path** of the source page (from Phase 1 `path` field)
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
`skill__glmv_pdf_to_web__crop_image` 裁剪每个已接受的框：

```json
{"path":"<page_image_path>","box":[0,0,1000,1000],"name":"<descriptive_name>","out_dir":"<out_dir>/crops"}
```

裁剪工具会返回 `path`、`width`、`height` 和 `aspect`；例如
`{"path":"<workspace>/<name>_crop.png","width":800,"height":600,"aspect":1.33}`。
不要直接查看裁剪结果，
应通过 `skill__glm5v_multimodal__analyze_images` 验证：

```json
{"images":["<out_dir>/crops/<name>_crop.png"],"prompt":"Does this crop contain the complete target '<target description>' with little unrelated content? Answer PASS or FAIL and explain any missing edge or excess region.","output":"<out_dir>/crops/<name>_review.md"}
```

On `FAIL`, rerun grounding with the review text in the prompt, crop again, and recheck.
Build `section_id → [crop filename, ...]` only from verified crops.

---

### Phase 5 — Measure Cropped Image Dimensions

Reuse the `width`, `height`, and `aspect` fields returned by each successful
`skill__glmv_pdf_to_web__crop_image` call. Do not run a separate inline script to
measure the generated files.

| Aspect ratio | Layout recommendation |
|---|---|
| **< 0.7** (tall/narrow) | `max-width: 400–500px`, centered |
| **0.7 – 1.3** (square-ish) | `max-width: 600–700px` |
| **> 1.3** (wide) | Full-width, `max-width: 100%` |
| **> 2.0** (very wide, e.g. tables) | Full-width with horizontal scroll fallback |

---

### Phase 6 — Generate the Single-Page HTML

**Step A — Write HTML** to `<out_dir>/website-draft.html`

- All `<img src="...">` must use **relative paths**: `crops/<name>_crop.png`
- Do NOT use absolute paths

**步骤 B：调用 `skill__glmv_pdf_to_web__generate_website`：**
```json
{"html_file":"<out_dir>/website-draft.html","title":"<paper title>","out_dir":"<out_dir>/"}
```

---

## HTML Spec

A **single self-contained HTML file** — embedded CSS, minimal vanilla JS only, and no
external frameworks, fonts, scripts, stylesheets, or image URLs.

**Page layout:**
- Max content width: `900px`, centered, comfortable side padding
- Sticky top nav with section anchor links + smooth scroll
- Looks good at 1200px wide; readable at 768px

**Typography:**
- Use a resilient system font stack for headings and body/UI
- Body: 17–18px, line-height 1.7
- Strong heading hierarchy (h1 >> h2 >> h3)

**Visual style:**
- If the user specifies a style, follow it exactly
- Otherwise, infer an appropriate aesthetic from the paper's domain and tone (e.g. CV/ML paper → clean modern academic; systems paper → dark technical; humanities → warm editorial serif)
- Define colors and fonts as CSS variables; no fixed palette or font choices are required

**Section guidelines:**

`hero`:
- Large title (2–3rem), authors list with affiliation superscripts, venue badge pill
- Link buttons: `[📄 Paper] [💻 Code] [🗄️ Dataset]` — grey out if no URL
- Teaser figure below (if found)

`abstract`:
- Verbatim text with subtle left border accent

`contributions`:
- Cards in a 2–3 column CSS grid, each with Unicode symbol + heading + description

`method`:
- Full-width architecture figure (`<figure><img><figcaption>`) + prose explanation

`results`:
- **Quantitative table** as real `<table>` — use actual numbers from the PDF, best numbers **bolded**
- **Qualitative figures** in a grid (2–4 images with captions)

`conclusion`:
- 2–3 paragraphs

`citation`:
- `<pre><code>` BibTeX block reconstructed from PDF metadata
- "Copy" button using `navigator.clipboard` vanilla JS

**Images:**
- All `<img>` use relative paths: `crops/<name>_crop.png`
- Add `loading="lazy"` and descriptive `alt`
- Wrap in `<figure>` with `<figcaption>`

**Animations (subtle only):**
- Fade-in on scroll via `IntersectionObserver` + CSS transitions
- Hover states on buttons/cards

---

---

## Quality Checklist

- [ ] Output directory named `<pdf_stem>_<timestamp>/`
- [ ] `outline.json` saved with valid WebPlan schema
- [ ] All crops saved to `crops/` (local only)
- [ ] All metadata (title, authors, venue, year) from the PDF
- [ ] Abstract is verbatim
- [ ] Quantitative table has real numbers from the paper
- [ ] All crop images referenced via `crops/<name>_crop.png`
- [ ] BibTeX block accurate and copyable
- [ ] Nav anchors scroll to correct sections
- [ ] `skill__glmv_pdf_to_web__generate_website` 调用成功

---

## Language

Match the PDF language. English paper → English website. Chinese paper → Chinese. No mixing.
