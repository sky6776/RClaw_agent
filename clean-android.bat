@echo off
setlocal
REM ============================================================================
REM RClaw Android 构建产物一键清理
REM
REM 清理对象：
REM   1. jniLibs 手工复制的 .so（gen/android/app/src/main/jniLibs/）
REM   2. gen/android 的 gradle 构建目录（app/build、build、buildSrc/build、.kotlin）
REM   3. cargo 的 android target 产物（target/aarch64-linux-android 等）
REM   4. 前端产物 ui/dist（下次构建会重建）
REM 注意：不清 gradle 依赖缓存（gradle-user-home 在工具链目录，跨项目共享）
REM      与 cargo 全局缓存；只删本项目可再生产物。
REM ============================================================================

set "REPO=%~dp0"
if "%REPO:~-1%"=="\" set "REPO=%REPO:~0,-1%"

echo [1/4] jniLibs .so
if exist "%REPO%\crates\rclaw-desktop\gen\android\app\src\main\jniLibs" (
    rmdir /s /q "%REPO%\crates\rclaw-desktop\gen\android\app\src\main\jniLibs"
    echo   已删除 jniLibs
) else (
    echo   无
)

echo [2/4] gradle 构建目录
for %%d in (
    "%REPO%\crates\rclaw-desktop\gen\android\app\build"
    "%REPO%\crates\rclaw-desktop\gen\android\build"
    "%REPO%\crates\rclaw-desktop\gen\android\buildSrc\build"
    "%REPO%\crates\rclaw-desktop\gen\android\.kotlin"
) do (
    if exist %%d (
        rmdir /s /q %%d
        echo   已删除 %%~d
    )
)

echo [3/4] cargo android target 产物
for %%t in (aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android) do (
    if exist "%REPO%\target\%%t" (
        rmdir /s /q "%REPO%\target\%%t"
        echo   已删除 target\%%t
    )
)

echo [4/4] 前端产物 ui/dist
if exist "%REPO%\crates\rclaw-desktop\ui\dist" (
    rmdir /s /q "%REPO%\crates\rclaw-desktop\ui\dist"
    echo   已删除 ui\dist
) else (
    echo   无
)

echo.
echo 清理完成。
exit /b 0
