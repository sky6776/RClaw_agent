#!/usr/bin/env python3
"""解压常见格式压缩包（zip / tar 系列 / 单文件 gzip、bz2、xz）。

只用标准库，桌面 CPython 与 APK（Chaquopy）通用：
- zip：zipfile（Windows GBK 文件名修正 + zip-slip 防护）
- tar / tar.gz / tgz / tar.bz2 / tbz2 / tar.xz / txz：tarfile（data 过滤器防
  路径穿越；旧解释器无 filter 参数时回退手动校验后解压）
- .gz / .bz2 / .xz（单文件）：gzip / bz2 / lzma 解包
7z / rar 等无标准库支持，明确报错，不做半吊子解压。
"""
from __future__ import annotations

import argparse
import bz2
import gzip
import lzma
import os
import shutil
import sys
import tarfile
import zipfile

TAR_SUFFIXES = (
    ".tar.gz",
    ".tgz",
    ".tar.bz2",
    ".tbz2",
    ".tbz",
    ".tar.xz",
    ".txz",
    ".tar",
)
SINGLE_SUFFIXES = {".gz": gzip, ".bz2": bz2, ".xz": lzma}
# 有专用工具生态、标准库解不了的格式：明确拒绝并告知，而不是误报"格式无法识别"。
KNOWN_UNSUPPORTED = (".7z", ".rar", ".cab", ".iso", ".dmg", ".pkg")


def archive_stem(name: str) -> str:
    """foo.tar.gz → foo：按已知压缩后缀逐层剥（长的优先）。"""
    lower = name.lower()
    suffixes = sorted(
        list(TAR_SUFFIXES) + list(SINGLE_SUFFIXES) + [".zip"],
        key=len,
        reverse=True,
    )
    for suf in suffixes:
        if lower.endswith(suf):
            return name[: -len(suf)]
    return os.path.splitext(name)[0]


def default_dest(archive: str) -> str:
    return os.path.join(
        os.path.dirname(os.path.abspath(archive)),
        archive_stem(os.path.basename(archive)),
    )


def unsafe_parts(name: str) -> list[str] | None:
    """拆路径部件；绝对路径 / .. / Windows 盘符返回 None（zip-slip 防护）。"""
    parts = [p for p in name.replace("\\", "/").split("/") if p not in ("", ".")]
    if not parts or any(p == ".." for p in parts):
        return None
    if any(len(p) == 2 and p[1] == ":" and p[0].isalpha() for p in parts):
        return None
    return parts


def zip_member_name(info: zipfile.ZipInfo) -> str:
    """未打 UTF-8 旗标的条目名按 cp437 反解后尝试常见中文编码（Windows 压缩包）。"""
    if info.flag_bits & 0x800:
        return info.filename
    try:
        raw = info.filename.encode("cp437")
    except UnicodeEncodeError:
        return info.filename
    for enc in ("gbk", "big5", "shift_jis"):
        try:
            return raw.decode(enc)
        except UnicodeDecodeError:
            continue
    return info.filename


def extract_zip(archive: str, dest: str):
    files = 0
    total = 0
    skipped = []
    tops = set()
    with zipfile.ZipFile(archive) as zf:
        for info in zf.infolist():
            name = zip_member_name(info)
            parts = unsafe_parts(name)
            if parts is None:
                skipped.append(name)
                continue
            tops.add(parts[0])
            target = os.path.join(dest, *parts)
            if info.is_dir():
                os.makedirs(target, exist_ok=True)
                continue
            os.makedirs(os.path.dirname(target) or dest, exist_ok=True)
            with zf.open(info) as src, open(target, "wb") as out:
                shutil.copyfileobj(src, out)
            files += 1
            total += info.file_size
    return files, total, sorted(tops), skipped


def extract_tar(archive: str, dest: str):
    files = 0
    total = 0
    tops = set()
    skipped = []
    with tarfile.open(archive, "r:*") as tf:
        members = tf.getmembers()
        try:
            tf.extractall(dest, filter="data")  # 3.12+：拒路径穿越/外部链接/设备文件
        except TypeError:
            # 旧解释器无 data 过滤器：手动校验每个成员的路径与类型后解压
            # （符号/硬链接可借道把后续条目写出目标目录，设备文件无意义且危险）。
            for member in members:
                if unsafe_parts(member.name) is None:
                    raise ValueError(f"tar 内含不安全路径: {member.name}") from None
                if member.issym() or member.islnk() or member.isdev():
                    raise ValueError(f"tar 内含链接/设备条目: {member.name}") from None
            tf.extractall(dest)
        for member in members:
            parts = unsafe_parts(member.name)
            if parts is None:
                skipped.append(member.name)
                continue
            tops.add(parts[0])
            if member.isfile():
                files += 1
                total += member.size
    return files, total, sorted(tops), skipped


def extract_single(archive: str, dest: str, kind: str) -> str:
    module = SINGLE_SUFFIXES[kind]
    target = os.path.join(dest, archive_stem(os.path.basename(archive)))
    with module.open(archive, "rb") as src, open(target, "wb") as out:
        shutil.copyfileobj(src, out)
    return target


def format_size(num: int) -> str:
    if num >= 1024 * 1024:
        return f"{num / 1024 / 1024:.1f} MB"
    if num >= 1024:
        return f"{num / 1024:.1f} KB"
    return f"{num} B"


def main() -> int:
    parser = argparse.ArgumentParser(description="解压常见格式压缩包")
    parser.add_argument("archive", help="压缩包路径")
    parser.add_argument("--dest", default=None, help="目标目录（缺省：压缩包旁同名子目录）")
    args = parser.parse_args()

    archive = args.archive
    if not os.path.isfile(archive):
        print(f"error: 压缩包不存在: {archive}")
        return 1

    lower = archive.lower()
    if lower.endswith(KNOWN_UNSUPPORTED):
        ext = os.path.splitext(archive)[1]
        print(
            f"error: 暂不支持 {ext} 格式（7z/rar 等需要专用工具，"
            "请在电脑上解压后再发送）"
        )
        return 1

    dest = args.dest or default_dest(archive)
    os.makedirs(dest, exist_ok=True)

    try:
        if lower.endswith(".zip"):
            files, total, tops, skipped = extract_zip(archive, dest)
            kind = "zip"
        elif lower.endswith(TAR_SUFFIXES):
            files, total, tops, skipped = extract_tar(archive, dest)
            kind = "tar"
        elif lower.endswith(tuple(SINGLE_SUFFIXES)):
            target = extract_single(archive, dest, os.path.splitext(lower)[1])
            kind = "单文件解包"
            files, total, tops, skipped = 1, os.path.getsize(target), [target], []
        else:
            print(
                "error: 无法识别的压缩格式。支持：zip / tar / tar.gz / tgz / "
                "tar.bz2 / tbz2 / tar.xz / txz / .gz / .bz2 / .xz"
            )
            return 1
    except ModuleNotFoundError as e:
        print(f"error: 当前环境缺少 {e.name} 模块，无法解压该压缩格式")
        return 1
    except (zipfile.BadZipFile, tarfile.TarError, ValueError, OSError, EOFError) as e:
        print(f"error: 解压失败: {e}")
        return 1

    print(f"解压完成（{kind}）")
    print(f"压缩包: {archive}")
    print(f"目标目录: {dest}")
    if kind == "单文件解包":
        print(f"解出文件: {tops[0]}（{format_size(total)}）")
    else:
        print(f"解出文件 {files} 个，共 {format_size(total)}")
        shown = tops[:30]
        print("顶层条目:")
        for name in shown:
            print(f"  - {name}")
        if len(tops) > len(shown):
            print(f"  …另有 {len(tops) - len(shown)} 项")
    for name in skipped:
        print(f"警告: 跳过不安全路径条目: {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
