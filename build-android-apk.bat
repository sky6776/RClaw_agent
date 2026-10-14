@echo off
setlocal enabledelayedexpansion
REM ============================================================================
REM RClaw Android APK one-shot build (debug/release, arm64-v8a, single .so + Chaquopy)
REM
REM Usage:
REM   build-android-apk.bat              build debug APK
REM   build-android-apk.bat release      build release APK (local signing configured)
REM   build-android-apk.bat debug matrix run skill script matrix after build - needs device
REM
REM Toolchain root defaults to D:\my-android\android-toolchain (this PC, local_compile branch);
REM RCLAW_BUILD_PYTHON overrides the Chaquopy buildPython.
REM Output: crates\rclaw-desktop\gen\android\app\build\outputs\apk\arm64\{debug|release}\
REM Release signing: gen\android\app\rclaw-release.keystore (not in git) plus
REM RCLAW_KEYSTORE_PASSWORD env var / alias rclaw (self-signed, local install only).
REM This file is ASCII-only on purpose: Chinese comments in a .bat get
REM mis-parsed under the GBK console codepage - see build-android-release.bat.
REM ============================================================================
setlocal

if "%ANDROID_TOOLCHAIN%"=="" set "ANDROID_TOOLCHAIN=D:\my-android\android-toolchain"

set "PROFILE=%~1"
if "%PROFILE%"=="" set "PROFILE=debug"
if /i "%PROFILE%"=="debug" (
    set "GRADLE_TASK=assembleArm64Debug"
    set "RUST_TASK=rustBuildArm64Debug"
    set "CARGO_FLAGS="
) else (
    set "PROFILE=release"
    set "GRADLE_TASK=assembleArm64Release"
    set "RUST_TASK=rustBuildArm64Release"
    set "CARGO_FLAGS=--release"
)

REM Local keystore without its password would silently debug-sign the release
REM APK (signature mismatch vs an installed release build). Warn loudly instead.
if /i "%PROFILE%"=="release" if not defined RCLAW_KEYSTORE_PASSWORD (
    echo [WARN] RCLAW_KEYSTORE_PASSWORD not set - release falls back to debug signing
)

set "JAVA_HOME=%ANDROID_TOOLCHAIN%\jdk\jdk-17.0.19+10"
set "ANDROID_HOME=%ANDROID_TOOLCHAIN%\sdk"
set "ANDROID_SDK_ROOT=%ANDROID_HOME%"
set "ANDROID_NDK_HOME=%ANDROID_HOME%\ndk\29.0.14206865"
set "NDK_HOME=%ANDROID_NDK_HOME%"
set "NDK_ROOT=%ANDROID_NDK_HOME%"
set "GRADLE_USER_HOME=%ANDROID_TOOLCHAIN%\gradle-user-home"
REM rquickjs/bindgen (aarch64 target) runs on the host and needs libclang.
if not defined LIBCLANG_PATH set "LIBCLANG_PATH=%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\bin"
REM The NDK's libclang auto resource-dir detection fails under bindgen on
REM Windows (stdbool.h not found), so pass the clang resource dir explicitly.
REM Use the target-suffixed var: bindgen prefers it over the plain one, which
REM cargo-ndk overwrites with a sysroot-only value. Values are shell-lexed by
REM bindgen, so use forward slashes (backslashes get eaten as escapes).
if defined BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android goto :bindgen_env_done
for /d %%v in ("%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\lib\clang\*") do set "CLANG_RESOURCE_DIR=%%v"
set "BINDGEN_EXTRA_CLANG_ARGS_aarch64_linux_android=--sysroot=%ANDROID_NDK_HOME:\=/%/toolchains/llvm/prebuilt/windows-x86_64/sysroot -resource-dir=%CLANG_RESOURCE_DIR:\=/%"
:bindgen_env_done
REM NDK toolchain bin on PATH: bindgen's in-process clang driver fails to
REM locate its own installation without it (empty InstalledDir, builtin
REM headers like stdbool.h unresolvable), and cc needs the cross wrappers.
set "PATH=%ANDROID_NDK_HOME%\toolchains\llvm\prebuilt\windows-x86_64\bin;%PATH%"
if "%RCLAW_BUILD_PYTHON%"=="" (
    set "RCLAW_BUILD_PYTHON=%ANDROID_TOOLCHAIN%\python312\python.exe"
    if not exist "!RCLAW_BUILD_PYTHON!" set "RCLAW_BUILD_PYTHON="
)
set "PATH=%JAVA_HOME%\bin;%ANDROID_HOME%\platform-tools;%PATH%"

set "REPO=%~dp0"
if "%REPO:~-1%"=="\" set "REPO=%REPO:~0,-1%"

REM tauri-build only generates gen/android/tauri.settings.gradle and
REM app/tauri.build.gradle.kts when TAURI_ANDROID_PROJECT_PATH is set;
REM the tauri/wry build scripts generate TauriActivity.kt (+proguard-tauri.pro)
REM only when the WRY_ANDROID_* vars below are set. Without these the gradle
REM step fails on a machine without leftovers of a prior tauri-CLI-driven
REM build (Unresolved reference 'TauriActivity').
set "TAURI_ANDROID_PROJECT_PATH=%REPO%\crates\rclaw-desktop\gen\android"
set "WRY_ANDROID_PACKAGE=ai.rclaw.app"
set "TAURI_ANDROID_PACKAGE_UNESCAPED=ai.rclaw.app"
set "WRY_ANDROID_LIBRARY=rclaw_desktop"
REM wry kotlin templates go into the generated\ subdir: the same files were
REM placed there by the tauri CLI era - writing to the old path makes duplicate
REM class declarations (redeclaration compile failure).
set "WRY_ANDROID_KOTLIN_FILES_OUT_DIR=%REPO%\crates\rclaw-desktop\gen\android\app\src\main\java\ai\rclaw\app\generated"
REM wry canonicalizes the dir before writing into it; fresh checkouts lack it
REM (the dir is gitignored), so create it up front.
if not exist "%WRY_ANDROID_KOTLIN_FILES_OUT_DIR%" mkdir "%WRY_ANDROID_KOTLIN_FILES_OUT_DIR%"

echo [1/5] frontend build - ui/dist
pushd "%REPO%\crates\rclaw-desktop\ui"
call npm install --no-audit --no-fund || goto :fail
call npm run build || goto :fail
popd

echo [2/5] Rust single .so - librclaw_desktop.so, aarch64, apk-host
cargo ndk -t arm64-v8a build -p rclaw-desktop --features apk-host,tauri/custom-protocol %CARGO_FLAGS% || goto :fail

echo [3/5] copy .so to jniLibs - no symlinks on Windows without dev mode
set "JNI_DIR=%REPO%\crates\rclaw-desktop\gen\android\app\src\main\jniLibs\arm64-v8a"
if not exist "%JNI_DIR%" mkdir "%JNI_DIR%"
copy /y "%REPO%\target\aarch64-linux-android\%PROFILE%\librclaw_desktop.so" "%JNI_DIR%\" || goto :fail

echo [3.5/5] whisper lib - librclaw_whisper.so, auto clone+build when missing
call scripts\build-whisper.bat || goto :fail

echo [4/5] Gradle APK - skip rustBuild, .so already in place
pushd "%REPO%\crates\rclaw-desktop\gen\android"
call gradlew.bat %GRADLE_TASK% --no-daemon --no-configuration-cache ^
    -x %RUST_TASK% -Dkotlin.compiler.execution.strategy=in-process ^
    -PabiList=arm64-v8a -ParchList=arm64 -PtargetList=aarch64 || (popd & goto :fail)
popd

set "APK_DIR=%REPO%\crates\rclaw-desktop\gen\android\app\build\outputs\apk\arm64\%PROFILE%"
set "APK_FILE=%APK_DIR%\app-arm64-%PROFILE%.apk"

echo [5/5] artifact check and location
REM stale-APK guard: freshness check runs in adb_install_confirm.py before install.
for %%f in ("%APK_FILE%") do echo APK: %%f (%%~zf bytes)

if /i "%~2"=="matrix" (
    echo [matrix] running skill script matrix tests...
    python scripts\skill_matrix_test.py
)
echo.
echo Done. Install: adb install -r "%APK_FILE%"
exit /b 0

:fail
echo Build FAILED - see output above.
exit /b 1
