---
name: extract-archive
description: 解压常见格式的压缩包：zip、tar、tar.gz/tgz、tar.bz2/tbz2、tar.xz/txz，以及单文件 .gz/.bz2/.xz。自动防护路径穿越（zip slip）、修正 Windows 中文 zip 文件名乱码，输出解压清单。当用户发来压缩包要"解压 / extract / 解开 / 压缩包里有什么"时触发。7z/rar 不支持。
---

# 解压压缩包（extract-archive）

把压缩包解到 agent 工作区目录，支持 zip 与全系列 tar、单文件 gz/bz2/xz。

## 何时使用

- 用户发来压缩包（常落在 `uploads/`）并要求解压、查看内容或取出其中文件
- 需要把某个 tar.gz / zip 展开到指定目录继续处理

## 如何使用

先 read 本 SKILL.md 激活 Manifest，然后调用动态工具：

```
skill__extract_archive__extract(archive="uploads/pkg.tar.gz")
skill__extract_archive__extract(archive="uploads/pkg.zip", dest="work/pkg")
```

- `archive`（必填）：压缩包路径（相对工作区，如 `uploads/xxx.zip`）
- `--dest`（可选）：目标目录；缺省解到压缩包同目录下以主干命名的子目录
  （`uploads/pkg.tar.gz` → `uploads/pkg/`）

## 支持格式

- zip
- tar / tar.gz / tgz / tar.bz2 / tbz2 / tar.xz / txz
- 单文件 .gz / .bz2 / .xz（解包到目标目录）
- **不支持**：7z / rar / cab / iso / dmg（会明确报错，请让用户在电脑解压后再传）

## 安全与乱码

- 解压前做路径穿越防护（zip slip / tar 成员含 `..`、绝对路径、盘符的条目会被跳过并警告）
- tar 用 Python data 过滤器：拒绝符号链接外逃、设备文件
- Windows 产出的中文 zip（GBK 文件名）自动修正名字，不会出现乱码文件

## 注意事项

- 安卓上也可以直接用 shell 的 `tar -xzf`，但部分 ROM 的 toybox tar 解"外来
  uid"的包会报 chown 错误（文件其实已解出）——优先用本 skill，稳定无此怪癖
- 目标目录已存在的同名文件会被覆盖
- 输出包含目标目录路径与顶层条目清单，据此继续 read / list 处理里面的文件
