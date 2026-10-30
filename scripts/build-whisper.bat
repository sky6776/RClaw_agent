@echo off
REM ============================================================================
REM whisper speech lib build (arm64-v8a -> jniLibs/librclaw_whisper.so)
REM
REM First run: shallow-clones whisper.cpp into third_party\whisper.cpp, then
REM cross-compiles with CMake+NDK (whisper statically linked into one .so,
REM JNI wrapper included).
REM Requires: python -m pip install cmake ninja; ANDROID_NDK_HOME points to NDK.
REM Skipped when the artifact already exists - delete the .so under jniLibs
REM or pass force to rebuild.
REM This file is ASCII-only on purpose: Chinese comments in a .bat get
REM mis-parsed under the GBK console codepage - see build-android-release.bat.
REM ============================================================================
setlocal enabledelayedexpansion

if "%ANDROID_NDK_HOME%"=="" (
    echo [ERROR] ANDROID_NDK_HOME is not set. Point it at the Android NDK root.
    exit /b 1
)
set "REPO=%~dp0.."
set "JNI_DIR=%REPO%\crates\rclaw-desktop\gen\android\app\src\main\jniLibs\arm64-v8a"
set "SO=%JNI_DIR%\librclaw_whisper.so"
set "BUILD_DIR=%REPO%\build-whisper\arm64"

if /i not "%~1"=="force" if exist "%SO%" (
    echo [whisper] exists already - %SO% - skipping, pass force to rebuild
    exit /b 0
)

for /f "usebackq delims=" %%i in (`python -c "import sysconfig; print(sysconfig.get_path('scripts'))"`) do set "PY_SCRIPTS=%%i"
set "PATH=%PY_SCRIPTS%;%PATH%"

if not exist "%REPO%\third_party\whisper.cpp\.git" (
    echo [whisper] clone whisper.cpp ...
    git clone --depth 1 https://github.com/ggml-org/whisper.cpp "%REPO%\third_party\whisper.cpp" || exit /b 1
)

echo [whisper] configure ...
cmake -S "%REPO%\third_party\whisper-jni" -B "%BUILD_DIR%" -G Ninja ^
    -DCMAKE_TOOLCHAIN_FILE="%ANDROID_NDK_HOME%\build\cmake\android.toolchain.cmake" ^
    -DANDROID_ABI=arm64-v8a -DANDROID_PLATFORM=android-26 -DCMAKE_BUILD_TYPE=Release || exit /b 1

echo [whisper] build ...
cmake --build "%BUILD_DIR%" --parallel || exit /b 1

if not exist "%JNI_DIR%" mkdir "%JNI_DIR%"
copy /y "%BUILD_DIR%\librclaw_whisper.so" "%JNI_DIR%\" || exit /b 1

REM ---- bundled model into APK assets for offline use ----
set "MODEL_DIR=%REPO%\crates\rclaw-desktop\gen\android\app\src\main\assets\models"
set "MODEL_FILE=%MODEL_DIR%\ggml-base-q5_1.bin"
if not exist "%MODEL_FILE%" (
    echo [whisper] download bundled model ggml-base-q5_1.bin - 57MB, CN mirror first ...
    mkdir "%MODEL_DIR%" 2>nul
    curl -L -o "%MODEL_FILE%" --retry 2 "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin" || curl -L -o "%MODEL_FILE%" --retry 2 "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin" || exit /b 1
)

echo [whisper] done: %SO%
exit /b 0
