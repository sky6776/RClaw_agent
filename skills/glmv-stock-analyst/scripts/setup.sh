#!/usr/bin/env bash
# setup.sh — 股票分析师 Skill 环境初始化（可重复执行）
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
VENV_DIR="$SCRIPT_DIR/venv"
VENV_PY=""
REQUIREMENTS="$SCRIPT_DIR/requirements.txt"
REQUIRED_MODULES=(pandas numpy matplotlib akshare markdown bleach requests fpdf docx PIL)

cd "$SCRIPT_DIR"

locate_venv_python() {
    local candidate
    for candidate in "$VENV_DIR/bin/python" "$VENV_DIR/Scripts/python.exe"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

print_usage() {
    echo ""
    echo "━━━ 后续使用方式 ━━━"
    echo "  获取股票数据：skill__glmv_stock_analyst__fetch_data"
    echo "  获取 IR 文档：skill__glmv_stock_analyst__fetch_ir"
    echo "  生成报告：skill__glmv_stock_analyst__generate_report"
    echo "  不要手写或运行 Skill 脚本命令。"
}

check_required_modules() {
    local output
    if ! output="$("$VENV_PY" - "${REQUIRED_MODULES[@]}" 2>&1 <<'PY'
import importlib
import sys

for module in sys.argv[1:]:
    try:
        importlib.import_module(module)
    except Exception:
        print(f"必需模块导入失败: {module}", file=sys.stderr)
        raise
PY
    )"; then
        echo "⚠️ venv 依赖检查失败" >&2
        if [ -n "$output" ]; then
            printf '%s\n' "$output" >&2
        fi
        return 1
    fi
}

TERMUX_MODE=false
case "$SCRIPT_DIR" in
    */data/data/com.termux*) TERMUX_MODE=true ;;
esac
case "${PREFIX:-}" in
    /data/data/com.termux*) TERMUX_MODE=true ;;
esac
if [ -n "${TERMUX_VERSION:-}" ]; then
    TERMUX_MODE=true
elif command -v pkg >/dev/null 2>&1 && [ "$(uname -o 2>/dev/null || true)" = "Android" ]; then
    TERMUX_MODE=true
fi
if [ "$TERMUX_MODE" = true ]; then
    echo "📱 当前运行环境: Termux"
    # akshare 依赖 mini_racer（V8 引擎，Termux aarch64 无法编译），pip 安装必然失败回滚，
    # 连带 bleach/markdown 等纯 Python 依赖也装不上。fetch_all.py 对缺失的 akshare 会
    # 优雅跳过并回落东财原始接口，因此 Termux 下不安装、也不把它列为必需模块。
    REQUIRED_MODULES=(pandas numpy matplotlib markdown bleach requests fpdf docx PIL)
else
    echo "💻 当前运行环境: 普通 POSIX"
fi

echo "🔍 检测 Python 环境..."

VENV_PY="$(locate_venv_python || true)"
if [ -n "$VENV_PY" ] && check_required_modules; then
    echo "✅ venv 已就绪: $VENV_PY"
    print_usage
    exit 0
fi

if [ -d "$VENV_DIR" ] && [ -z "$VENV_PY" ]; then
    echo "❌ venv 目录存在，但未找到可执行的 bin/python 或 Scripts/python.exe。" >&2
    echo "   该 venv 可能已损坏或与当前平台不兼容；请备份需要的文件并移走后重试。" >&2
    exit 1
fi

# Windows：Git Bash/MSYS 的 python3 会创建 bin/ 布局 venv（bin/python.exe），其平台
# 标签没有 pandas/numpy 预编译 wheel，依赖安装必然退化为源码编译并失败。
# 因此非 Termux 平台优先选择原生 Windows Python（py 启动器 → python → python3）。
select_native_windows_python() {
    local candidate
    for candidate in "$(command -v py 2>/dev/null || true)" \
                     "$(command -v python 2>/dev/null || true)" \
                     "$(command -v python3 2>/dev/null || true)"; do
        [ -n "$candidate" ] || continue
        if "$candidate" -c 'import sys; sys.exit(0 if sys.platform == "win32" else 1)' >/dev/null 2>&1; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

if [ "$TERMUX_MODE" != true ] && [ -f "$VENV_DIR/bin/python.exe" ]; then
    echo "🧹 检测到 MSYS/Cygwin Python 创建的 venv（bin/python.exe 布局），移除后用原生 Python 重建..."
    rm -rf "$VENV_DIR"
fi

if [ "$TERMUX_MODE" = true ]; then
    if command -v python3 >/dev/null 2>&1; then
        SYS_PY="$(command -v python3)"
    elif command -v python >/dev/null 2>&1; then
        SYS_PY="$(command -v python)"
    else
        echo "❌ 未找到可用的 Python 环境" >&2
        exit 1
    fi
else
    if SYS_PY="$(select_native_windows_python)"; then
        echo "🐍 使用原生 Python: $SYS_PY"
    else
        echo "❌ 未找到原生 Windows Python（py/python/python3 均缺失或为 MSYS 版）" >&2
        echo "   Windows: winget install Python.Python.3.12" >&2
        exit 1
    fi
fi

if [ "$TERMUX_MODE" = true ]; then
    if ! command -v pkg >/dev/null 2>&1; then
        echo "❌ 已识别为 Termux，但找不到 pkg 命令；请修复 Termux 包管理器后重试。" >&2
        exit 1
    fi
    echo "📦 检测到 Termux，安装带原生扩展的预编译包..."
    if ! pkg install -y python-numpy matplotlib python-pillow python-lxml tur-repo; then
        echo "❌ Termux 原生依赖安装失败。请先执行 pkg upgrade，确认软件源可用后重试。" >&2
        exit 1
    fi
    if ! pkg install -y python-pandas; then
        echo "❌ python-pandas 安装失败。该包通常由 tur-repo 提供，请检查 TUR 软件源后重试。" >&2
        exit 1
    fi
fi

if [ ! -d "$VENV_DIR" ]; then
    echo "📦 创建虚拟环境 (venv)..."
    if [ "$TERMUX_MODE" = true ]; then
        "$SYS_PY" -m venv --system-site-packages "$VENV_DIR"
    else
        "$SYS_PY" -m venv "$VENV_DIR"
    fi
elif [ "$TERMUX_MODE" = true ]; then
    echo "🔧 venv 已存在但依赖不完整，将原地修复，不重新创建"
    VENV_CFG="$VENV_DIR/pyvenv.cfg"
    if [ ! -f "$VENV_CFG" ]; then
        echo "❌ venv 缺少 pyvenv.cfg，无法安全修复: $VENV_CFG" >&2
        exit 1
    fi
    if ! grep -Eq '^include-system-site-packages[[:space:]]*=[[:space:]]*true$' "$VENV_CFG"; then
        if grep -Eq '^include-system-site-packages[[:space:]]*=' "$VENV_CFG"; then
            sed -i 's/^include-system-site-packages[[:space:]]*=.*/include-system-site-packages = true/' "$VENV_CFG"
        else
            printf '\ninclude-system-site-packages = true\n' >> "$VENV_CFG"
        fi
        echo "✅ 已允许现有 venv 复用 Termux 系统 Python 包"
    fi
else
    echo "🔧 venv 已存在但依赖不完整，将原地补装依赖，不重新创建"
fi

VENV_PY="$(locate_venv_python || true)"
if [ -z "$VENV_PY" ]; then
    echo "❌ 初始化后仍未找到 venv Python: $VENV_DIR" >&2
    exit 1
fi

PIP_INDEX_ARGS=()
if command -v curl >/dev/null 2>&1 && command -v awk >/dev/null 2>&1; then
    echo "🌐 测速 pip 镜像源..."
    BEST_MIRROR=""
    BEST_TIME="9999"
    BEST_NAME=""
    MIRRORS=(
        "default|https://pypi.org/simple/"
        "清华 TUNA|https://pypi.tuna.tsinghua.edu.cn/simple"
        "阿里云|https://mirrors.aliyun.com/pypi/simple/"
    )
    for ENTRY in "${MIRRORS[@]}"; do
        NAME="${ENTRY%%|*}"
        URL="${ENTRY##*|}"
        if ! SEC="$(curl -o /dev/null -s -w '%{time_total}' --connect-timeout 5 --max-time 8 "$URL")"; then
            SEC="99.99"
        fi
        printf "  %-12s %ss\n" "$NAME" "$SEC"
        if awk -v current="$SEC" -v best="$BEST_TIME" 'BEGIN { exit !(current < best) }'; then
            BEST_TIME="$SEC"
            BEST_MIRROR="$URL"
            BEST_NAME="$NAME"
        fi
    done
    if [ -n "$BEST_NAME" ] && [ "$BEST_NAME" != "default" ]; then
        BEST_HOST="$(printf '%s' "$BEST_MIRROR" | sed 's|https\?://||;s|/.*||')"
        PIP_INDEX_ARGS=(--index-url "$BEST_MIRROR" --trusted-host "$BEST_HOST")
        echo "✅ 最快镜像: $BEST_NAME (${BEST_TIME}s)"
    else
        echo "✅ 使用默认源 PyPI"
    fi
else
    echo "ℹ️ 未找到 curl/awk，使用默认 PyPI"
fi

REQ_FILE="$REQUIREMENTS"
TERMUX_REQ_FILE=""
cleanup() {
    if [ -n "$TERMUX_REQ_FILE" ]; then
        rm -f "$TERMUX_REQ_FILE"
    fi
}
trap cleanup EXIT

if [ "$TERMUX_MODE" = true ]; then
    TERMUX_REQ_FILE="$(mktemp)"
    # 原生包由 pkg/TUR 提供；akshare 拖带的 mini_racer（V8 引擎）在 Termux aarch64 上
    # 无法安装；PyMuPDF 是可选的 IR 提取能力，避免在 Android 上强制源码编译。
    grep -Eiv '^[[:space:]]*(numpy|pandas|matplotlib|Pillow|pymupdf|akshare)([<>=!~[:space:]]|$)' \
        "$REQUIREMENTS" > "$TERMUX_REQ_FILE"
    REQ_FILE="$TERMUX_REQ_FILE"
fi

echo ""
echo "📥 安装 Python 依赖..."
"$VENV_PY" -m pip install "${PIP_INDEX_ARGS[@]}" -r "$REQ_FILE"

if ! check_required_modules; then
    echo "❌ 依赖安装完成，但必需模块仍不可用；请根据上面的 import 错误处理后重试。" >&2
    exit 1
fi

echo ""
echo "✅ 安装完成！"
if [ "$TERMUX_MODE" = true ] && ! "$VENV_PY" -c 'import fitz' >/dev/null 2>&1; then
    echo "ℹ️ Termux 下未安装可选的 PyMuPDF，仅跳过外部研报/IR PDF 图表提取；report.pdf 仍由 fpdf2 + Pillow 生成。"
fi
print_usage
