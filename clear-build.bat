@echo off
setlocal
chcp 65001 >nul
REM ============================================================================
REM RClaw 构建产物清理（Windows）
REM
REM 用法：
REM   clear-build.bat          清理全部编译产物（debug + release 一起清）
REM   clear-build.bat all      追加清理依赖缓存（node_modules / whisper.cpp 克隆）
REM
REM 说明：debug 与 release 的产物都在同一批目录里（cargo target\{debug,release}、
REM gradle app\build\outputs\apk\arm64\{debug,release}），本脚本一次性全删——
REM 不区分构建类型。
REM 保留（非产物，删了要重新下载/克隆）：assets 里的 whisper 模型、
REM third_party\whisper.cpp（传 all 才删）、ui\node_modules（传 all 才删）。
REM 若 target 提示「另一个程序正在使用」（IDE/rust-analyzer 占用），关掉后重跑即可。
REM ============================================================================
set "REPO=%~dp0"
if "%REPO:~-1%"=="\" set "REPO=%REPO:~0,-1%"

echo [1/6] cargo 产物（target\，含 debug+release、aarch64 交叉编译）
if exist "%REPO%\target" rmdir /s /q "%REPO%\target"

echo [2/6] gradle 产物（gen\android\app\build，含 debug+release APK 与中间产物）
if exist "%REPO%\crates\rclaw-desktop\gen\android\app\build" rmdir /s /q "%REPO%\crates\rclaw-desktop\gen\android\app\build"

echo [3/6] gradle 项目缓存（.gradle）
if exist "%REPO%\crates\rclaw-desktop\gen\android\.gradle" rmdir /s /q "%REPO%\crates\rclaw-desktop\gen\android\.gradle"
if exist "%REPO%\.gradle" rmdir /s /q "%REPO%\.gradle"

echo [4/6] whisper 编译产物（build-whisper）
if exist "%REPO%\build-whisper" rmdir /s /q "%REPO%\build-whisper"

echo [5/6] 前端构建产物（ui\dist）
if exist "%REPO%\crates\rclaw-desktop\ui\dist" rmdir /s /q "%REPO%\crates\rclaw-desktop\ui\dist"

echo [6/6] Rust 桌面构建产物（VS 生成目录，如存在）
if exist "%REPO%\crates\rclaw-desktop\gen\schemas" rmdir /s /q "%REPO%\crates\rclaw-desktop\gen\schemas"

if /i "%~1"=="all" (
    echo [all] 追加清理：ui\node_modules（下次构建 npm install 重装）
    if exist "%REPO%\crates\rclaw-desktop\ui\node_modules" rmdir /s /q "%REPO%\crates\rclaw-desktop\ui\node_modules"
    echo [all] 追加清理：third_party\whisper.cpp（下次构建重新 clone）
    if exist "%REPO%\third_party\whisper.cpp" rmdir /s /q "%REPO%\third_party\whisper.cpp"
)

echo.
echo 清理完成。
exit /b 0
