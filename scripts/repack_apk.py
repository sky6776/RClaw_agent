"""APK zip 空洞修复：重写 zip 条目 → zipalign → debug 重签。

背景：gradle 偶发产出带空洞的 APK（体积异常翻倍，实测 1.41GB），流式安装极慢；
本会话又发现两个重打包坑——① python `item.compress_type or ZIP_DEFLATED` 会把
STORED(0) 静默压成 DEFLATE，违反 targetSdk 30+ 对 resources.arsc 不压缩的强制
（vivo 安装器报「软件包似乎无效」）；② 输出名不能是 .apk 后缀之外（adb install
拒绝非 .apk/.apex 文件名）。

用法：python scripts/repack_apk.py <apk 路径>
产物：<同目录>/<同名>.apk（原文件保留为 .bak）。zipalign/apksigner 取
ANDROID_TOOLCHAIN 环境变量（必填，不内置机器相关路径）。
"""
import os
import subprocess
import sys
import zipfile

TOOLCHAIN = os.environ.get("ANDROID_TOOLCHAIN")
if not TOOLCHAIN:
    sys.exit("请设置 ANDROID_TOOLCHAIN 环境变量（Android toolchain 根目录）")
BUILD_TOOLS = os.path.join(TOOLCHAIN, "sdk", "build-tools", "36.0.0")
KEYSTORE = os.path.join(os.environ["USERPROFILE"], ".android", "debug.keystore")


def main():
    src = sys.argv[1]
    dst = src + ".repack"
    stored_src = stored_dst = 0
    with zipfile.ZipFile(src) as zin, zipfile.ZipFile(
        dst, "w", zipfile.ZIP_DEFLATED, compresslevel=1
    ) as zout:
        for item in zin.infolist():
            data = zin.read(item.filename)
            zi = zipfile.ZipInfo(item.filename, date_time=item.date_time)
            # 严格保留原压缩方式：STORED(0) 是假值，`or` 兜底会把不压缩条目压瘪。
            zi.compress_type = item.compress_type
            zi.external_attr = item.external_attr
            stored_src += item.compress_type == zipfile.ZIP_STORED
            zout.writestr(zi, data, compress_type=zi.compress_type, compresslevel=1)
            stored_dst += zi.compress_type == zipfile.ZIP_STORED
    if stored_src != stored_dst:
        sys.exit(f"STORED 条目数不一致 src={stored_src} dst={stored_dst}，中止")

    # 输出必须是 .apk 结尾（adb install 硬性要求）且不与输入同名（避免自我覆盖）。
    base = src[:-4] if src.endswith(".apk") else src
    final = base + ".fixed.apk"
    aligned = src + ".aligned"
    subprocess.run(
        [os.path.join(BUILD_TOOLS, "zipalign.exe"), "-f", "4", dst, aligned], check=True
    )
    env = dict(os.environ, JAVA_HOME=os.path.join(TOOLCHAIN, "jdk", "jdk-17.0.19+10"))
    subprocess.run(
        [
            os.path.join(BUILD_TOOLS, "apksigner.bat"),
            "sign",
            "--ks",
            KEYSTORE,
            "--ks-pass",
            "pass:android",
            "--out",
            final,
            aligned,
        ],
        check=True,
        env=env,
    )
    os.remove(dst)
    os.remove(aligned)
    print(f"repacked: {final} ({os.path.getsize(final)} 字节, STORED {stored_dst} 条)")
    print(f"原文件（含空洞）保留: {src}")


if __name__ == "__main__":
    main()
