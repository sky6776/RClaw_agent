@echo off
rem ============================================================
rem  One-click rebuild of the skills-doc ebook (PDF).
rem  Scans all *.md next to this script via build_ebook.py.
rem  Docs changed / added / removed -> just double-click again.
rem ============================================================
setlocal
cd /d "%~dp0"
chcp 65001 >nul
set "PYTHONUTF8=1"

set "PY="
python --version >nul 2>nul && set "PY=python"
if not defined PY (
    py -3 --version >nul 2>nul && set "PY=py -3"
)
if not defined PY (
    echo [ERROR] Python 3 not found. Install it from https://www.python.org/downloads/
    pause
    exit /b 1
)

%PY% -c "import fpdf" >nul 2>nul
if errorlevel 1 (
    echo [setup] Installing fpdf2 ...
    %PY% -m pip install --quiet fpdf2
    if errorlevel 1 (
        echo [ERROR] Failed to install fpdf2. Check network / pip config.
        pause
        exit /b 1
    )
)

echo [build] Running build_ebook.py ...
%PY% build_ebook.py
if errorlevel 1 (
    echo [ERROR] Build failed.
    pause
    exit /b 1
)

echo.
echo Done. The PDF is in this folder.
pause
