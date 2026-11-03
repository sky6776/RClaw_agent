# 平台与构建

[← 返回 README](../README.md)

RClaw 的核心（`rclaw-cli` 以 lib 形式暴露 config / bootstrap / logging）被四种形态复用。本文说明每种形态怎么构建、怎么运行，以及 CI 发布与构建脚本。

| 平台 | 形态 | 说明 |
|---|---|---|
| Windows / Linux / macOS | CLI 二进制 `rclaw` | 全功能，`cargo build --release -p rclaw-cli` 即得 |
| Windows | 桌面 GUI（Tauri 2 + React） | 托盘常驻 + 全部图形面板 |
| Android | 原生 APK（arm64-v8a，包名 `ai.rclaw.app`） | Tauri Android 壳，内置 Python 3.12 技能运行时、whisper、gbrain |
| Android | Termux CLI | 手机上直接编译运行 `rclaw`，专用低内存构建档 |

预构建产物在 [GitHub Releases](https://github.com/sky6776/RClaw_agent/releases)。

## CLI（全平台）

```bash
cargo build --release -p rclaw-cli    # 只编 CLI；无参 cargo build 会连同桌面 crate 编译，需先生成 ui/dist
cp rclaw.toml.example rclaw.toml      # 填入 api_key
./target/release/rclaw                # 或 --daemon 后台常驻
```

配置从当前工作目录读取，数据写到 `./agent/`。控制台通道不显示流式片段与思考过程，不收发媒体——它定位为本机调试与外部通道（QQ / 微信）的宿主进程。详细步骤见 [快速开始与配置](getting-started.md)。

## Windows 桌面版

基于 Tauri 2 的桌面应用（React + Vite + Tailwind 前端，与安卓 APK 共用同一前端）。桌面壳是一个「虚拟通道」：窗口输入写入消息总线，出站消息经 Tauri 事件推给 Webview。

```bat
build-windows.cmd
```

一键完成前端构建（需 Node.js 18+）与桌面二进制编译。手动等价步骤：

```bash
cd crates/rclaw-desktop/ui && npm install && npm run build && cd ../../..
cargo build --release -p rclaw-desktop
```

- 前端产物必须先于 Rust 编译生成：Tauri 在 `generate_context!` 编译期内嵌 `ui/dist`，缺失时编译直接失败。
- 产物 `target/release/rclaw-desktop.exe` 为绿色单文件。release 构建为 GUI 子系统（不弹控制台窗口），debug 构建保留控制台便于看日志。
- 数据目录：`RCLAW_HOME` > `%USERPROFILE%\.rclaw`；旧版 `%APPDATA%\rclaw` 在升级后首次启动自动整体迁入。
- 首运行无 `rclaw.toml` 时进入配置向导（provider 预设 / API Key / 模型 / 上下文窗口 / 自进化开关），生成配置后立即启动 agent。向导原子写入，不覆盖已有文件。
- 点窗口关闭 = 隐藏到托盘；托盘菜单「退出」才结束进程。
- 应用内更新：「关于 → 检查更新」从 GitHub Release 拉取新版。

面板功能见 [图形界面](gui.md)。

## Android APK

原生 APK（arm64-v8a，minSdk 26 / targetSdk 36），在 Windows 上交叉构建：

```bat
build-android-apk.bat              :: debug APK
build-android-apk.bat release      :: release APK（本机签名）
build-android-release.bat          :: 等价于上一条的包装
build-android-apk-d.bat            :: 另一台机器的工具链路径包装示例
```

构建链依次：前端 → Rust 单 `.so`（`librclaw_desktop.so`，`apk-host` feature）→ whisper JNI 库 → Gradle 打包。

- **工具链**：环境变量 `ANDROID_TOOLCHAIN` 指向一个按固定布局摆放的根目录——`jdk\jdk-17.0.19+10`、`sdk`（内含 `ndk\29.0.14206865`、`build-tools\36.0.0`）、`gradle-user-home`、可选 `python312\python.exe`（Gradle 本身走 `gradlew.bat` 包装器）；另需 `cargo-ndk`、Node.js，whisper 步骤还需 CMake + Ninja（`python -m pip install cmake ninja`）与 git。`RCLAW_BUILD_PYTHON` 覆盖 Chaquopy 构建用 Python；`RCLAW_SKIP_FRONTEND_BUILD=1` 复用现有 `ui/dist`；第二个参数 `matrix` 会在构建后自动跑 `scripts/skill_matrix_test.py`。APK 产物在 `gen\android\app\build\outputs\apk\arm64\<debug|release>\`。
- **内置能力**：Chaquopy 内置 Python 3.12 运行时（技能脚本进程内执行，严格串行、超时可中断、脚本哈希审批）；whisper.cpp 本地语音识别（`.so` 与 `ggml-base-q5_1.bin` 模型可内置）；gbrain 知识库进程内集成（不启动 MCP 子进程，数据自动归一化到应用私有目录）；前台服务 + 开机接收器 + AlarmManager 看门狗维持无头运行。
- **技能解包**：Gradle 任务 `syncBundledSkills` 把仓库 `skills/` 打入 APK，首次启动按 APK 版本解包到数据目录并保留用户改动。注意：构建时若存在仓库同级 `../conf/skills` 或仓库内 `conf/skills`，其中的 `ark_key.toml`、`*/config.toml`、`*/.env` 等会**覆盖进 APK 内置技能资产**——这是给自己打包预填 Key 用的，分发给他人的包务必确认该目录不含真实凭据。
- **签名优先级**：`gen\android\app\rclaw-release.keystore`（不进 git）+ 环境变量 `RCLAW_KEYSTORE_PASSWORD` 二者齐备时用 release key；否则存在 `gen\android\app\rclaw-update.keystore` 时用它（口令 / 别名可经 `RCLAW_UPDATE_KEYSTORE_PASSWORD` / `RCLAW_UPDATE_KEY_ALIAS` 覆盖，默认 `android` / `androiddebugkey`）；再否则回退本机 debug keystore。同一套签名配置同时作用于 debug 与 release 构建，全新克隆不配任何 keystore 也能构建安装。
- **签名一致性（应用内更新）**：Android 只允许「同包名 + 同签名」覆盖安装，否则系统安装器报「软件包与现有软件包存在冲突」，只能卸载重装（本地配置全丢）。CI 未配置 release keystore secrets 时，APK 用仓库 secret `ANDROID_DEBUG_KEYSTORE_BASE64` 恢复出的 debug keystore 签名；本机把**同一份** keystore 复制为 `rclaw-update.keystore` 后，本机 / 多台机器 / CI 出的包可以互相覆盖升级。应用内更新在拉起安装器前还会经 `NativeBridge.verifyUpdateApk` 预检包名、签名与 versionCode（禁止降级），不一致时直接给出可读原因。
- **版本号**：`crates/rclaw-desktop/tauri.conf.json` 的 `version` 与 `gen/android/app/tauri.properties` 的 `tauri.androidName` 必须一致，CI 发布前会校验。
- **安装**：`adb install -r <apk>`；vivo 等对 adb 直装不自动放图标的机型，可用 `scripts/adb_install_confirm.py` 走安装器 UI 路径并自动处理确认弹窗（`RCLAW_ADB` 可指定 adb 路径）。`scripts/repack_apk.py` 可修复 APK ZIP 空洞并 zipalign（需 `ANDROID_TOOLCHAIN` 的 build-tools），但固定用本机 debug keystore 重签——release 签名的包重打后会变成 debug 签名。

安卓端界面与原生桥能力见 [图形界面](gui.md#安卓宿主)。

## Termux（手机直接跑 CLI）

在 Android 手机的 Termux 环境里从源码构建运行，无需 APK：

```bash
pkg install rust binutils git python   # python 供技能脚本使用
git clone <本仓库> && cd RClaw_agent

# 专用低内存构建档：关 LTO、放宽 codegen-units、build script 降优化，
# 避免链接期被 OOM 杀掉；-j1 进一步限制并行内存峰值
cargo build --profile release-termux -j1

cp rclaw.toml.example rclaw.toml      # 填入 api_key
./target/release-termux/rclaw
```

保持默认 features（纯 Rust）。技能的 Python 依赖建议走 venv 隔离安装（Manifest 可用 `python_venv` 指定）。`rclaw-desktop` 在 `default-members` 内，Android target 上不开 `apk-host` feature 时它被编译为一个只打印提示的桩二进制，不拉入 tauri 运行时依赖。

## CI 发布

`.github/workflows/release.yml`：推送 `v*` tag 时构建三种产物并发布到 GitHub Release，普通推送 `main` 不触发。手动 `workflow_dispatch` 走同一流水线但不做版本门禁，产物发布为滚动的 `nightly` 预发布（每次删除重建）；应用内更新只看 `/releases/latest`，不会拉到 nightly。

| 产物 | 说明 |
|---|---|
| `RClaw-android-arm64.apk` | 安卓 arm64 安装包（debug / release 签名自适应） |
| `RClaw-desktop-win64.exe` | Windows 桌面单文件（免安装，双击运行）；应用内「关于 → 检查更新」指向此处 |
| `RClaw-termux-aarch64.zip` | Termux 用 aarch64 二进制（`release-termux` profile） |

发布门禁：tag `vX.Y.Z` 必须同时等于 `tauri.conf.json` 的 `version` 与 `tauri.properties` 的 `tauri.androidName`，否则失败并给出修正提示；另有「JNI 桥方法在 R8 后仍存活」校验（`installUpdate` / `verifyUpdateApk` / `runScript` 等被剥离即不发布）。签名 secrets：`RCLAW_KEYSTORE_BASE64`、`RCLAW_KEYSTORE_PASSWORD`，以及更新通道用的 `ANDROID_DEBUG_KEYSTORE_BASE64`。

## 构建脚本与辅助脚本

| 脚本 | 用途 |
|---|---|
| `build-windows.cmd` | 前端构建 + 桌面 release 编译 |
| `build-android-apk.bat [release]` | 前端 → `.so` → whisper → Gradle APK |
| `build-android-release.bat` | 转调 release 构建 |
| `build-android-apk-d.bat` | 固定另一台机器工具链路径的包装示例 |
| `clean-android.bat` / `clear-build.bat` | 清理安卓产物（jniLibs / Gradle 构建目录 / 安卓交叉产物，**含 `ui/dist`**）/ 清理全部构建产物（可选含缓存） |
| `scripts/build-whisper.bat` | 首次自动 `git clone` whisper.cpp 到 `third_party/whisper.cpp`（不入库）并交叉编译 JNI，缺模型时自动下载 |
| `scripts/repack_apk.py` | 修复 APK ZIP 空洞、zipalign 并用本机 debug keystore 重签 |
| `scripts/adb_install_confirm.py` | 驱动安装器确认弹窗并以更新时间验收 |
| `scripts/skill_matrix_test.py` | 真机遍历技能入口做矩阵测试 |
| `scripts/zcode_tokens.py` | 解密 / 探测 / 写入 ZCode 金融 MCP 令牌（见 [MCP 扩展](mcp.md)） |

## Cargo profile 与 feature

- `release`：opt-level 3、thin LTO、单 codegen unit。
- `release-termux`：opt-level 2、无 LTO、16 codegen units、build script 降优化——为手机内存设计。
- feature `apk-host`（`rclaw-desktop`）：开启后编译为 APK 宿主，构建时同时开 `tauri/custom-protocol`；不开且 target 为 Android 时编译为 Termux 提示桩；非 Android target 为桌面 GUI。
- workspace 不定义 OCR 类 feature（不开启上游 PDF 库的 `ocr` feature）；SQLite / QuickJS 以内嵌 C 源随 cargo 编译，不依赖系统库。
