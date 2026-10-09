@echo off
setlocal
REM ============================================================================
REM Machine-specific wrapper: THIS PC keeps the Android toolchain on
REM D:\my-android\android-toolchain, while build-android-apk.bat defaults to
REM E:\my-android\android-toolchain (the other PC). Set the override then
REM delegate with all arguments, e.g.:
REM   build-android-apk-d.bat            build debug APK
REM   build-android-apk-d.bat release    build release APK
REM   build-android-apk-d.bat debug matrix   + on-device skill matrix
REM The original script stays untouched so both machines share one repo.
REM This file is ASCII-only on purpose (GBK codepage .bat lesson, 2026-10-02).
REM ============================================================================
set "ANDROID_TOOLCHAIN=D:\my-android\android-toolchain"
call "%~dp0build-android-apk.bat" %*
exit /b %ERRORLEVEL%
