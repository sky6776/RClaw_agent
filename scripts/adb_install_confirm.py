# -*- coding: utf-8 -*-
"""安装 APK 并自动处理全部确认弹窗（原子操作，构建后唯一入口）。

用法：python adb_install_confirm.py <apk 路径> [--adb]

为什么默认走「安装器 UI」而不是 adb install：
    vivo 桌面只给经安装器应用（com.android.packageinstaller）安装的包自动放置
    桌面图标；adb install 直装的包系统侧一切正常（LAUNCHER 意图可解析），
    但桌面两页都不会出现图标。推 APK 到 Download 再 ACTION_VIEW 唤起安装器，
    才与真实用户「文件管理器安装」路径一致，桌面才有图标。

流程：
    push /sdcard/Download → ACTION_VIEW 唤起安装器 → 每 2 秒轮询屏幕
    （uiautomator dump：勾选 CheckBox + 点「继续安装/安装/…」）→ 直到 pm 的
    lastUpdateTime 更新 → 点「完成」收尾 → 截图存档。
    UI 流程 90 秒内毫无动静（安装器没起来）则回退 adb install -r（含弹窗处理）。

教训（教训依旧成立）：adb install 返回 ≠ 安装完成；弹窗分步操作会漏确认
导致安装静默失败。一切以 pm dump 的 lastUpdateTime 变化为准。
"""

import os
import re
import subprocess
import sys
import tempfile
import time
from datetime import datetime

# adb 可执行：默认取 PATH（经 build-android-apk.bat 运行时 toolchain 的
# platform-tools 已注入 PATH）；独立运行且 adb 不在 PATH 时用 RCLAW_ADB 指定。
ADB = os.environ.get("RCLAW_ADB", "adb")
PACKAGE = "ai.rclaw.app"
REMOTE_APK = "/sdcard/Download/rclaw-app.apk"
CONFIRM_LABELS = [
    "继续安装", "仍然安装", "安装", "重新安装", "更新", "确定", "同意",
]
FINISH_LABELS = ["完成", "打开"]
# 已打印过的未知弹窗文本集合（避免刷屏，一种弹窗只报一次）。
seen_dialogs: set[tuple[str, ...]] = set()


def sh(*args, timeout=120):
    return subprocess.run(
        [ADB, "shell", *args], capture_output=True, text=True, timeout=timeout
    ).stdout


def dump_nodes():
    sh("uiautomator", "dump", "/sdcard/window_dump.xml")
    xml = sh("cat", "/sdcard/window_dump.xml")
    nodes = []
    for m in re.finditer(r"<node [^>]*>", xml):
        attrs = dict(re.findall(r'([\w-]+)="([^"]*)"', m.group(0)))
        nums = re.findall(r"\d+", attrs.get("bounds", ""))
        center = None
        if len(nums) == 4:
            x1, y1, x2, y2 = map(int, nums)
            center = ((x1 + x2) // 2, (y1 + y2) // 2)
        nodes.append((attrs, center))
    return nodes


def tap(x, y):
    sh("input", "tap", str(x), str(y))


def tap_by_text(labels):
    for attrs, center in dump_nodes():
        if attrs.get("text") in labels and center:
            tap(*center)
            return attrs.get("text")
    return None


def tap_keypad_digit(digit):
    """vivo 锁屏密码键盘：按钮 resource-id 形如 com.xxx:id/key6。"""
    for attrs, center in dump_nodes():
        rid = attrs.get("resource-id", "")
        if rid.endswith(f"key{digit}") and center:
            tap(*center)
            return True
    return False


def handle_password_gate(pin):
    """vivo「未备案应用」安装要求锁屏密码验证：点「锁屏密码验证」→ 逐位输入。
    pin 经 --pin 传入（不写进仓库）；无 pin 时打印提示交给人工。"""
    for attrs, center in dump_nodes():
        text = attrs.get("text", "")
        if ("锁屏密码" in text or "密码验证" in text) and "Button" in attrs.get("class", ""):
            if center:
                print("[pin] 检测到锁屏密码验证页")
                if not pin:
                    print("[pin] 未提供 --pin，需人工输入密码")
                    screenshot("pin_needed")
                    return False
                tap(*center)
                time.sleep(2.5)
                for digit in pin:
                    if not tap_keypad_digit(digit):
                        print(f"[pin] 找不到按键 key{digit}")
                        screenshot("pin_fail")
                        return False
                    time.sleep(0.5)
                print("[pin] 已输入，等待校验")
                time.sleep(2.5)
                return True
    return False


def last_update_time():
    out = sh("dumpsys", "package", PACKAGE)
    # 完整时间戳含空格（"2026-10-01 10:35:16"）：[^\s]+ 会在空格处截断，
    # 导致安装前后值恒等、完成检测永不触发（10:46 安装事件的实际教训）。
    m = re.search(r"lastUpdateTime=([0-9-]+ [0-9:]+)", out)
    return m.group(1) if m else None


def screenshot(tag):
    png = os.path.join(tempfile.gettempdir(), f"install_{tag}_{int(time.time())}.png")
    with open(png, "wb") as sink:
        subprocess.run([ADB, "exec-out", "screencap", "-p"], stdout=sink, timeout=30)
    print(f"[screenshot] {png}")


def try_confirm_dialog():
    """弹窗出现则勾选+确认，返回是否执行了动作。"""
    acted = False
    # 1) 未勾选的 CheckBox（vivo 风险确认页）
    for attrs, center in dump_nodes():
        if "CheckBox" in attrs.get("class", "") and attrs.get("checked") == "false":
            if center:
                tap(*center)
                acted = True
                break
    time.sleep(0.8)
    # 2) 确认按钮（重新 dump，勾选可能刷新页面）
    if tap_by_text(CONFIRM_LABELS):
        acted = True
        return True
    # 3) 有可点文本但不在标签表 → 主动打印+截图（用户教训：不要闭眼等）。
    texts = sorted(
        {
            a.get("text")
            for a, c in dump_nodes()
            if a.get("text") and c and "Button" in a.get("class", "")
        }
    )
    if texts and tuple(texts) not in seen_dialogs:
        seen_dialogs.add(tuple(texts))
        print(f"[dialog] 未知按钮文本：{texts}（如需自动点请加入 CONFIRM_LABELS）")
        screenshot("dialog")
    return acted


def installer_in_foreground():
    out = sh("dumpsys", "window", "windows") or sh(
        "dumpsys", "activity", "activities"
    )
    return bool(re.search(r"(packageinstaller|appstore|vivo\s*安装)", out, re.I))


def ui_install(before):
    """安装器 UI 流程。返回 True=安装完成。"""
    print(f"[ui-install] push {REMOTE_APK}")
    push = subprocess.run([ADB, "push", sys.argv[1], REMOTE_APK],
                          capture_output=True, text=True, timeout=300)
    if "file pushed" not in push.stdout + push.stderr:
        print("[ui-install] push 失败：", (push.stdout + push.stderr).strip())
        return False
    started = sh(
        "am", "start", "-a", "android.intent.action.VIEW",
        "-d", f"file://{REMOTE_APK}",
        "-t", "application/vnd.android.package-archive",
    )
    print("[ui-install]", started.strip().splitlines()[-1] if started.strip() else "(no output)")
    if "Error" in started and "Starting" not in started:
        return False

    deadline = time.time() + 360
    fallback_at = time.time() + 90
    acted = False
    pin = None
    for arg in sys.argv[2:]:
        if arg.startswith("--pin="):
            pin = arg.split("=", 1)[1]
        elif arg == "--pin" and len(sys.argv) > sys.argv.index(arg) + 1:
            pin = sys.argv[sys.argv.index(arg) + 1]
    while time.time() < deadline:
        now = last_update_time()
        if now and now != before:
            print(f"[ui-install] 完成，lastUpdateTime = {now}")
            tap_by_text(FINISH_LABELS)  # 收尾：完成/打开，失败无妨
            # 装完可能还有「开启超级守护」推广弹窗，取消之。
            tap_by_text(["取消"])
            sh("rm", "-f", REMOTE_APK)
            screenshot("done")
            return True
        if try_confirm_dialog():
            acted = True
        elif handle_password_gate(pin):
            acted = True
        elif not acted and time.time() > fallback_at:
            print("[ui-install] 90 秒无动静（安装器未起来）")
            return False
        time.sleep(2)
    print("[ui-install] 超时：lastUpdateTime 未变化（弹窗已确认={}）".format(acted))
    return False


def adb_install(before):
    """兜底：adb install -r + 弹窗轮询。返回 True=安装完成。"""
    install = subprocess.run([ADB, "install", "-r", sys.argv[1]],
                             capture_output=True, text=True, timeout=900)
    print(f"[adb-install] {install.stdout.strip() or install.stderr.strip()}")
    deadline = time.time() + 360
    while time.time() < deadline:
        now = last_update_time()
        if now and now != before:
            print(f"[adb-install] 完成，lastUpdateTime = {now}")
            screenshot("done")
            return True
        try_confirm_dialog()
        time.sleep(2)
    return False


def main():
    apk = sys.argv[1]
    # 装前新鲜度校验：构建链失败时旧包残留，装了等于白装（本机踩过）。
    age_min = (time.time() - os.path.getmtime(apk)) / 60
    if age_min > 15:
        print(f"[install] 拒绝安装：APK 已是 {age_min:.0f} 分钟前的旧产物（构建可能实际失败）")
        return 1
    print(f"[install] APK 新鲜度 {age_min:.1f} 分钟前 ✓")
    before = last_update_time()
    ok = False if "--adb" in sys.argv else ui_install(before)
    if not ok:
        if "--adb" not in sys.argv:
            print("[install] 回退 adb install -r")
        ok = adb_install(before)
    if not ok:
        print("[install] 失败：请人工检查手机屏幕上的弹窗")
        screenshot("fail")
        return 1
    print("[install] 成功 ✔")
    return 0


if __name__ == "__main__":
    sys.exit(main())
