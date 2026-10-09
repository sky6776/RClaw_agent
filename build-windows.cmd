@echo off
setlocal
rem =============================================================================
rem RClaw one-shot build for Windows: frontend (ui/dist) -> desktop binary.
rem Frontend must build first: tauri embeds ui/dist at compile time.
rem Usage: run from repo root, or double-click in Explorer.
rem NOTE: kept ASCII-only on purpose - batch files with UTF-8 Chinese text plus
rem "chcp 65001" get mis-parsed when cmd re-seeks the file mid-run.
rem =============================================================================
set ATTEMPT=0

:retry
set /a ATTEMPT+=1
cd /d "%~dp0"
echo.
echo ===== Build attempt %ATTEMPT% =====

where npm >nul 2>nul || (echo [ERROR] npm not found. Install Node.js 18+ first. & goto :fail)
where cargo >nul 2>nul || (echo [ERROR] cargo not found. Install Rust MSVC toolchain first. & goto :fail)

rem 1) Frontend: React + Vite, outputs to ui/dist
pushd crates\rclaw-desktop\ui
if not exist node_modules (
    echo [1/2] npm install ...
    call npm install || goto :fail
)
echo [1/2] npm run build ...
call npm run build || goto :fail
popd

rem 2) Desktop binary: plain --release on Windows; release-termux is for Termux only
echo [2/2] cargo build --release -p rclaw-desktop ...
cargo build --release -p rclaw-desktop || goto :fail

echo.
echo Build OK: target\release\rclaw-desktop.exe
pause
exit /b 0

:fail
rem unwind the pushd above if we failed inside the frontend step
popd >nul 2>nul
echo.
echo Build FAILED (attempt %ATTEMPT%). Retrying in 10 seconds... close this window to stop.
timeout /t 10 /nobreak >nul
goto :retry
