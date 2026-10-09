# -*- coding: utf-8 -*-
"""真机 skill 脚本矩阵测试：每个 skill 的主入口以 --help 走一遍
SkillScriptInstrumentation（真实 Chaquopy 桥 + 技能安装 + 隔离配置）。"""
import base64
import re
import subprocess
import sys
import threading
import time

ADB = r"E:\my-android\android-toolchain\sdk\platform-tools\adb.exe"
INSTR = "ai.rclaw.app.test/ai.rclaw.app.SkillScriptInstrumentation"
ARGS_B64 = base64.b64encode('["--help"]'.encode()).decode()

MATRIX = [
    ("evaluate-fund", "fund_metrics.py"),
    ("glm-image-gen", "glm_image_cli.py"),
    ("glm-master-skill", "publish_media.py"),
    ("glm-master-skill", "glm_config.py"),
    ("glm5v-multimodal", "glm5v.py"),
    ("glm5v-multimodal", "glm5v_stream.py"),
    ("glmocr", "glm_ocr_cli.py"),
    ("glmocr-formula", "glm_ocr_cli.py"),
    ("glmocr-handwriting", "glm_ocr_cli.py"),
    ("glmocr-table", "glm_ocr_cli.py"),
    ("glmocr-sdk", "glmocr_sdk_cli.py"),
    ("glmv-caption", "glmv_caption.py"),
    ("glmv-doc-based-writing", "doc_based_writing.py"),
    ("glmv-pdf-to-ppt", "generate_slide.py"),
    ("glmv-pdf-to-web", "generate_web.py"),
    ("glmv-prompt-gen", "prompt_gen.py"),
    ("glmv-resume-screen", "resume_screen.py"),
    ("glmv-stock-analyst", "fetch_all.py"),
    ("glmv-stock-analyst", "fetch_ir.py"),
    ("glmv-stock-analyst", "text_chart.py"),
    ("happyhorse-i2v", "happyhorse_i2v_cli.py"),
    ("happyhorse-r2v", "happyhorse_r2v_cli.py"),
    ("happyhorse-t2v", "happyhorse_t2v_cli.py"),
    ("pick-funds", "fund_fallback.py"),
    ("read-macro", "macro_fallback.py"),
    ("screen-funds", "screen_funds.py"),
    ("seedance-video", "seedance_video_cli.py"),
    ("seedream-image", "seedream_image_cli.py"),
    ("watch-positions", "ak_fallback.py"),
]

PRINT_LOCK = threading.Lock()


def sh(args, timeout):
    return subprocess.run(args, capture_output=True, text=True, timeout=timeout)


def run_one(idx, skill, script):
    rel = f"skills/{skill}/scripts/{script}"
    rid = f"r{idx}"
    out = {
        "skill": skill,
        "script": script,
        "exit": None,
        "verdict": "",
        "note": "",
        "ms": 0,
    }
    try:
        proc = sh(
            [
                ADB, "shell", "am", "instrument", "-w",
                "-e", "script", rel,
                "-e", "args_b64", ARGS_B64,
                "-e", "result_id", rid,
                INSTR,
            ],
            timeout=240,
        )
        text = proc.stdout + proc.stderr
        m = re.search(r"exit_code=(-?\d+)", text)
        out["exit"] = int(m.group(1)) if m else None
        mh = re.search(r"harness_error=([^\r\n]*)", text)
        if mh:
            out["verdict"] = "HARNESS_FAIL"
            out["note"] = mh.group(1)[:120]
            return out
        # 读结果 JSON 摘要
        res = sh(
            [ADB, "shell", "run-as", "ai.rclaw.app", "cat",
             f"files/agent/tmp/skill-test-results/{rid}.json"],
            timeout=30,
        )
        body = res.stdout
        if not body.strip():
            out["verdict"] = "NO_RESULT"
            out["note"] = "result json missing"
            return out
        md = re.search(r'"duration_ms":(\d+)', body)
        out["ms"] = int(md.group(1)) if md else 0
        ms = re.search(r'"stdout":"(.*?)","stdout_truncated', body, re.S)
        me = re.search(r'"stderr":"(.*?)","stderr_truncated', body, re.S)
        stdout = (ms.group(1) if ms else "").replace("\\n", " ")[:90]
        stderr = (me.group(1) if me else "").replace("\\n", " ")[:90]
        if out["exit"] == 0:
            out["verdict"] = "PASS"
            out["note"] = stdout or stderr
        elif "usage" in (stdout + stderr).lower() or "Traceback" not in (stdout + stderr):
            out["verdict"] = "OK-ERR"
            out["note"] = (stderr or stdout)[:90]
        else:
            out["verdict"] = "FAIL"
            out["note"] = (stderr or stdout)[:120]
    except subprocess.TimeoutExpired:
        out["verdict"] = "TIMEOUT"
        out["note"] = "instrument > 240s"
    except Exception as e:  # noqa: BLE001
        out["verdict"] = "DRIVER_ERR"
        out["note"] = str(e)[:120]
    return out


def main():
    results = []
    with PRINT_LOCK:
        print(f"begin {len(MATRIX)} runs", flush=True)
    for idx, (skill, script) in enumerate(MATRIX, 1):
        row = run_one(idx, skill, script)
        results.append(row)
        with PRINT_LOCK:
            print(
                f"[{idx:>2}/{len(MATRIX)}] {row['verdict']:<12} exit={row['exit']} "
                f"{row['skill']}/{row['script']} {row['ms']}ms | {row['note'][:80]}",
                flush=True,
            )
    fails = [r for r in results if r["verdict"] not in ("PASS", "OK-ERR")]
    print(f"\n== SUMMARY: {len(results)} runs, PASS={sum(1 for r in results if r['verdict']=='PASS')}, "
          f"OK-ERR={sum(1 for r in results if r['verdict']=='OK-ERR')}, "
          f"FAIL={len(fails)}", flush=True)
    for r in fails:
        print(f"  FAIL: {r['skill']}/{r['script']} exit={r['exit']} {r['note'][:100]}", flush=True)


if __name__ == "__main__":
    sys.exit(main())
