#!/usr/bin/env bash
# 从 agent workspace 调用 skill 脚本，并保留调用方 cwd 作为数据输出目录。
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"

locate_venv_python() {
    local candidate
    for candidate in "$SCRIPT_DIR/venv/bin/python" "$SCRIPT_DIR/venv/Scripts/python.exe"; do
        if [ -x "$candidate" ]; then
            printf '%s\n' "$candidate"
            return 0
        fi
    done
    return 1
}

SCRIPT_NAME="${1:-}"
case "$SCRIPT_NAME" in
    ""|*/*|*\\*)
        echo "用法: bash skills/glmv-stock-analyst/scripts/run.sh <script.py> [参数...]" >&2
        exit 2
        ;;
esac
shift

TARGET="$SCRIPT_DIR/$SCRIPT_NAME"
if [ ! -f "$TARGET" ]; then
    echo "❌ skill 脚本不存在: $TARGET" >&2
    exit 2
fi

PYTHON="$(locate_venv_python || true)"
if [ -z "$PYTHON" ]; then
    echo "❌ 未找到 skill venv Python；请先运行 setup.sh。" >&2
    exit 1
fi

exec "$PYTHON" "$TARGET" "$@"
