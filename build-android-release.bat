@echo off
setlocal
REM ============================================================================
REM RClaw Android RELEASE build (arm64-v8a, optimized, smaller APK)
REM
REM Usage:
REM   build-android-release.bat            build release APK
REM   build-android-release.bat matrix     also run skill matrix (device needed)
REM
REM Notes:
REM - Release compiles Rust/frontend with optimizations: first run 15-30 min.
REM - Debug build: build-android-apk.bat (no args).
REM - Signing: uses gen\android\app\rclaw-release.keystore when present
REM   (alias rclaw; keystore file and RCLAW_KEYSTORE_PASSWORD env var stay
REM   local, not in git); otherwise falls back to debug signing so the APK
REM   installs out of the box.
REM - Clean: clear-build.bat removes debug AND release artifacts together.
REM
REM This file is ASCII-only on purpose: Chinese comments in a .bat get
REM mis-parsed under the GBK console codepage (byte misalignment executed
REM a comment tail as a command and recursed - 2026-10-02 lesson).
REM ============================================================================
"%~dp0build-android-apk.bat" release %~1
