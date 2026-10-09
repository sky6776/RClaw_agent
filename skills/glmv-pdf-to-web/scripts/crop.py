#!/usr/bin/env python3
"""
Crop a rectangular region from an image and save it locally.

Coordinates are in thousandths (0-1000) of the image dimensions,
matching the same convention as image_process_local.py.

Usage:
    python crop.py --path <image_path> --box x1 y1 x2 y2 --out-dir DIR [--name STEM]

Output (stdout):
    JSON: {"path": "/abs/path/to/<name>_crop.png", "width": 800,
           "height": 600, "aspect": 1.33}
"""

import os
import json
import argparse
import re
from PIL import Image as PILImage


def _safe_stem(path: str) -> str:
    stem = os.path.splitext(os.path.basename(path))[0]
    stem = re.sub(r"[^\w\-]", "_", stem)
    return stem[:40] or "image"


def crop_image(
    img_path: str, box: list[int], out_dir: str, name: str | None = None
) -> str:
    """
    Open image at `img_path`, crop the `box` region (thousandths 0-1000),
    save as PNG to `out_dir`, return absolute path.
    """
    for v in box:
        if not (0 <= v <= 1000):
            raise ValueError(f"Box coordinate {v} out of range 0-1000")
    x1, y1, x2, y2 = box

    with PILImage.open(img_path) as src:
        img = src.convert("RGB")
        w, h = img.size

        left = int(round(w * x1 / 1000))
        top = int(round(h * y1 / 1000))
        right = int(round(w * x2 / 1000))
        bottom = int(round(h * y2 / 1000))

        if right <= left or bottom <= top:
            raise ValueError(
                f"Invalid crop box -> pixel rect ({left},{top},{right},{bottom}) "
                f"on {w}x{h} image"
            )

        cropped = img.crop((left, top, right, bottom))

    stem = _safe_stem(name) if name else _safe_stem(img_path)
    filename = f"{stem}_crop.png"
    os.makedirs(out_dir, exist_ok=True)
    out_path = os.path.join(out_dir, filename)

    cropped.save(out_path, format="PNG")
    return os.path.abspath(out_path)


def main():
    parser = argparse.ArgumentParser(
        description="Crop an image region and save locally."
    )
    parser.add_argument("--path", required=True, help="Local path to the source image")
    parser.add_argument(
        "--box",
        nargs=4,
        type=int,
        metavar=("X1", "Y1", "X2", "Y2"),
        required=True,
        help="Crop box in thousandths (0-1000): x1 y1 x2 y2",
    )
    parser.add_argument(
        "--out-dir", required=True, help="Directory to save the cropped PNG"
    )
    parser.add_argument(
        "--name", default=None, help="Output filename stem (default: derived from path)"
    )
    args = parser.parse_args()

    out_path = crop_image(args.path, args.box, args.out_dir, args.name)
    with PILImage.open(out_path) as output:
        width, height = output.size
    print(
        json.dumps(
            {
                "path": out_path,
                "width": width,
                "height": height,
                "aspect": round(width / height, 2),
            },
            ensure_ascii=False,
        )
    )


if __name__ == "__main__":
    main()
