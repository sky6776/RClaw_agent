#!/usr/bin/env python3
"""Termux-friendly batch wrapper for the shared GLM-OCR API script."""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

def main():
    parser = argparse.ArgumentParser(
        description="Batch-parse images or PDFs through the shared GLM-OCR script"
    )
    parser.add_argument("files", nargs="+", help="Input image/PDF paths")
    parser.add_argument("--output", "-o", help="Optional UTF-8 JSON output file")
    parser.add_argument("--pretty", action="store_true", help="Pretty-print JSON")
    default_config = (
        Path(__file__).resolve().parents[2] / "glm-master-skill" / "config.toml"
    )
    parser.add_argument(
        "--config",
        default=str(default_config),
        help="Shared GLM TOML configuration file",
    )
    args = parser.parse_args()

    paths = [str(Path(raw).resolve()) for raw in args.files]
    missing = [path for path in paths if not Path(path).is_file()]
    if missing:
        parser.error(f"Input file not found: {missing[0]}")

    cli = (
        Path(__file__).resolve().parents[2]
        / "glmocr"
        / "scripts"
        / "glm_ocr_cli.py"
    )
    android = os.environ.get("RCLAW_ANDROID") == "1"
    results = []
    for path in paths:
        if android:
            # APK：无独立解释器可 spawn（sys.executable 指向宿主进程），
            # 进程内 runpy 运行目标 CLI，捕获 stdout 供 JSON 解析。
            import io as _io
            import runpy
            from contextlib import redirect_stdout

            argv_backup = sys.argv[:]
            buffer = _io.StringIO()
            sys.argv = [str(cli), "--file", path, "--config", args.config]
            exit_code = 0
            try:
                with redirect_stdout(buffer):
                    try:
                        runpy.run_path(str(cli), run_name="__main__")
                    except SystemExit as exit_error:
                        code = exit_error.code
                        if code in (None, 0):
                            exit_code = 0
                        elif isinstance(code, int):
                            exit_code = code
                        else:
                            # sys.exit("msg") 的字符串退出码：当作失败归一为 1。
                            exit_code = 1
                    except Exception as exc:
                        # 桌面分支经进程隔离，异常只表现为非零退出码；进程内运行
                        # 必须就地转成干净的失败退出，否则异常穿出中断后续文件批次。
                        raise SystemExit(f"glmocr CLI 异常: {path}: {exc}") from exc
            finally:
                sys.argv = argv_backup
            output = buffer.getvalue()
            if exit_code != 0:
                raise SystemExit(output or f"glmocr CLI 失败: {path}")
            try:
                results.append(json.loads(output))
            except json.JSONDecodeError as exc:
                raise SystemExit(f"glmocr CLI 输出不是 JSON: {exc}\n{output[:500]}") from exc
            continue
        completed = subprocess.run(
            [
                sys.executable,
                str(cli),
                "--file",
                path,
                "--config",
                args.config,
            ],
            text=True,
            encoding="utf-8",
            capture_output=True,
        )
        if completed.returncode != 0:
            raise SystemExit(completed.stdout or completed.stderr)
        results.append(json.loads(completed.stdout))

    payload = {
        "success": True,
        "backend": "termux-direct-api",
        "results": results,
    }
    text = json.dumps(payload, ensure_ascii=False, indent=2 if args.pretty else None)
    print(text)

    if args.output:
        output = Path(args.output).resolve()
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(text + "\n", encoding="utf-8")
        print(f"Result saved to: {output}", file=sys.stderr)


if __name__ == "__main__":
    main()
