#!/usr/bin/env node
/**
 * Build a real PowerPoint deck from GLM-5V-produced slide JSON.
 *
 * Termux:
 *   pkg install nodejs
 *   npm install --prefix skills/glmv-pdf-to-ppt/scripts
 *   node skills/glmv-pdf-to-ppt/scripts/generate_pptx.mjs \
 *     --slides-json slides.json --output presentation.pptx
 */

import fs from "node:fs";
import path from "node:path";
import PptxGenJS from "pptxgenjs";

function parseArgs(argv) {
  const args = {};
  for (let index = 0; index < argv.length; index += 1) {
    const key = argv[index];
    if (!key.startsWith("--")) continue;
    args[key.slice(2)] = argv[index + 1];
    index += 1;
  }
  if (!args["slides-json"] || !args.output) {
    throw new Error("Usage: --slides-json FILE --output FILE.pptx");
  }
  return args;
}

function resolveSlideImage(raw, baseDir) {
  if (typeof raw !== "string" || !raw.trim()) {
    throw new Error("Slide image path must be a non-empty string");
  }
  const resolved = path.resolve(baseDir, raw);
  if (!fs.existsSync(resolved) || !fs.statSync(resolved).isFile()) {
    throw new Error(`Slide image not found: ${raw}`);
  }
  const realBase = fs.realpathSync(baseDir);
  const realImage = fs.realpathSync(resolved);
  const relative = path.relative(realBase, realImage);
  if (relative.startsWith(`..${path.sep}`) || relative === ".." || path.isAbsolute(relative)) {
    throw new Error(`Slide image must stay inside the slides JSON directory: ${raw}`);
  }
  if (![".png", ".jpg", ".jpeg"].includes(path.extname(realImage).toLowerCase())) {
    throw new Error(`Unsupported slide image format: ${raw}`);
  }
  if (fs.statSync(realImage).size > 25 * 1024 * 1024) {
    throw new Error(`Slide image exceeds 25 MB: ${raw}`);
  }
  return realImage;
}

function imageDimensions(filePath) {
  const data = fs.readFileSync(filePath);
  if (
    data.length >= 24 &&
    data.subarray(0, 8).equals(Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]))
  ) {
    return { width: data.readUInt32BE(16), height: data.readUInt32BE(20) };
  }
  if (data.length >= 4 && data[0] === 0xff && data[1] === 0xd8) {
    let offset = 2;
    const sofMarkers = new Set([
      0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd,
      0xce, 0xcf,
    ]);
    while (offset + 8 < data.length) {
      if (data[offset] !== 0xff) {
        offset += 1;
        continue;
      }
      const marker = data[offset + 1];
      if (sofMarkers.has(marker)) {
        return {
          width: data.readUInt16BE(offset + 7),
          height: data.readUInt16BE(offset + 5),
        };
      }
      if (marker === 0xd8 || marker === 0xd9) {
        offset += 2;
      } else {
        const length = data.readUInt16BE(offset + 2);
        if (length < 2) break;
        offset += length + 2;
      }
    }
  }
  return { width: 16, height: 9 };
}

function contain(filePath, box) {
  const dimensions = imageDimensions(filePath);
  const imageRatio = dimensions.width / dimensions.height;
  const boxRatio = box.w / box.h;
  if (imageRatio >= boxRatio) {
    const height = box.w / imageRatio;
    return { x: box.x, y: box.y + (box.h - height) / 2, w: box.w, h: height };
  }
  const width = box.h * imageRatio;
  return { x: box.x + (box.w - width) / 2, y: box.y, w: width, h: box.h };
}

function addFooter(slide, pageNumber) {
  slide.addText(String(pageNumber), {
    x: 12.2,
    y: 7.08,
    w: 0.55,
    h: 0.22,
    fontFace: "Aptos",
    fontSize: 10,
    color: "64748B",
    align: "right",
    margin: 0,
  });
}

function addTitleSlide(pptx, deck) {
  const slide = pptx.addSlide();
  slide.background = { color: "0F172A" };
  slide.addText(deck.title || "Presentation", {
    x: 0.8,
    y: 2.25,
    w: 11.7,
    h: 1.2,
    fontFace: "Aptos Display",
    fontSize: 52,
    bold: true,
    color: "F8FAFC",
    margin: 0,
    breakLine: false,
    fit: "shrink",
  });
  if (deck.subtitle) {
    slide.addText(String(deck.subtitle), {
      x: 0.82,
      y: 3.62,
      w: 10.8,
      h: 0.72,
      fontFace: "Aptos",
      fontSize: 24,
      color: "94A3B8",
      margin: 0,
      fit: "shrink",
    });
  }
  slide.addShape(pptx.ShapeType.rect, {
    x: 0.8,
    y: 1.92,
    w: 1.4,
    h: 0.08,
    line: { color: "22D3EE", transparency: 100 },
    fill: { color: "22D3EE" },
  });
}

function addContentSlide(pptx, item, pageNumber, baseDir) {
  const slide = pptx.addSlide();
  slide.background = { color: "F8FAFC" };
  slide.addText(String(item.title || `Slide ${pageNumber}`), {
    x: 0.65,
    y: 0.34,
    w: 12.0,
    h: 0.58,
    fontFace: "Aptos Display",
    fontSize: 36,
    bold: true,
    color: "0F172A",
    margin: 0,
    fit: "shrink",
  });
  slide.addShape(pptx.ShapeType.line, {
    x: 0.65,
    y: 1.02,
    w: 12.0,
    h: 0,
    line: { color: "CBD5E1", width: 1 },
  });

  const bullets = Array.isArray(item.bullets)
    ? item.bullets.map((value) => String(value).trim()).filter(Boolean)
    : [];
  const images = Array.isArray(item.images) ? item.images.slice(0, 2) : [];
  const resolvedImages = images
    .map((entry) => {
      const raw = typeof entry === "string" ? entry : entry?.path;
      if (!raw) return null;
      const resolved = resolveSlideImage(raw, baseDir);
      return {
        path: resolved,
        caption: typeof entry === "string" ? "" : entry.caption || "",
      };
    })
    .filter(Boolean);

  const textWidth = resolvedImages.length ? 5.25 : 11.8;
  if (bullets.length) {
    slide.addText(bullets.map((value) => `• ${value}`).join("\n"), {
      x: 0.72,
      y: 1.3,
      w: textWidth,
      h: 5.45,
      fontFace: "Aptos",
      fontSize: 20,
      color: "1E293B",
      breakLine: false,
      valign: "top",
      margin: 0.08,
      paraSpaceAfterPt: 14,
      fit: "shrink",
    });
  }

  if (resolvedImages.length === 1) {
    const image = resolvedImages[0];
    const placement = contain(image.path, { x: 6.25, y: 1.32, w: 6.35, h: 5.35 });
    slide.addImage({ path: image.path, ...placement });
    if (image.caption) {
      slide.addText(String(image.caption), {
        x: 6.25,
        y: 6.72,
        w: 6.35,
        h: 0.28,
        fontFace: "Aptos",
        fontSize: 11,
        color: "64748B",
        align: "center",
        margin: 0,
        fit: "shrink",
      });
    }
  } else if (resolvedImages.length === 2) {
    resolvedImages.forEach((image, index) => {
      const placement = contain(image.path, {
        x: 6.25,
        y: 1.3 + index * 2.75,
        w: 6.35,
        h: 2.45,
      });
      slide.addImage({ path: image.path, ...placement });
    });
  }

  if (!bullets.length && !resolvedImages.length && item.body) {
    slide.addText(String(item.body), {
      x: 0.72,
      y: 1.35,
      w: 11.8,
      h: 5.5,
      fontFace: "Aptos",
      fontSize: 22,
      color: "1E293B",
      margin: 0.08,
      valign: "top",
      fit: "shrink",
    });
  }
  addFooter(slide, pageNumber);
}

async function main() {
  if (process.argv.includes("--help") || process.argv.includes("-h")) {
    process.stdout.write(
      "Usage: generate_pptx.mjs --slides-json FILE --output FILE.pptx\n"
    );
    return;
  }
  const args = parseArgs(process.argv.slice(2));
  const jsonPath = path.resolve(args["slides-json"]);
  const outputPath = path.resolve(args.output);
  if (path.extname(jsonPath).toLowerCase() !== ".json") {
    throw new Error("--slides-json must point to a .json file");
  }
  if (path.extname(outputPath).toLowerCase() !== ".pptx") {
    throw new Error("--output must end with .pptx");
  }
  const rawJson = fs.readFileSync(jsonPath, "utf8").replace(/^\uFEFF/, "");
  const deck = JSON.parse(rawJson);
  const slides = Array.isArray(deck) ? deck : deck.slides;
  if (!Array.isArray(slides) || slides.length === 0) {
    throw new Error("slides-json must contain a non-empty slides array");
  }

  const pptx = new PptxGenJS();
  pptx.layout = "LAYOUT_WIDE";
  pptx.author = "RClaw GLM Skills";
  pptx.subject = deck.title || "GLM-5V generated presentation";
  pptx.title = deck.title || "Presentation";
  pptx.company = "RClaw";
  pptx.lang = "zh-CN";
  pptx.theme = {
    headFontFace: "Aptos Display",
    bodyFontFace: "Aptos",
    lang: "zh-CN",
  };

  if (!Array.isArray(deck)) addTitleSlide(pptx, deck);
  const baseDir = path.dirname(jsonPath);
  slides.forEach((item, index) =>
    addContentSlide(pptx, item || {}, index + 1, baseDir)
  );

  fs.mkdirSync(path.dirname(outputPath), { recursive: true });
  await pptx.writeFile({ fileName: outputPath });
  process.stdout.write(
    JSON.stringify({
      ok: true,
      output: outputPath,
      slides: slides.length + (Array.isArray(deck) ? 0 : 1),
    })
  );
}

main().catch((error) => {
  process.stderr.write(`${error.stack || error.message}\n`);
  process.exit(1);
});
