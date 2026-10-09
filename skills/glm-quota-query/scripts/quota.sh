#!/bin/sh
# glm-quota-query: 查询智谱 GLM Coding Plan 编码套餐额度
# 端点: /api/monitor/usage/quota/limit (内部监控端点, 未正式写入公开 API 文档)
# 依赖: curl + sed/grep/awk + date, 无需 jq/python
# Key 只从 skills/glmv-stock-analyst/config.toml 读取, 绝不打印
set -u

SCRIPT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")" 2>/dev/null && pwd) || SCRIPT_DIR=""
CONFIG="${SCRIPT_DIR:-.}/../../glmv-stock-analyst/config.toml"

SHOW_MODEL_USAGE=0
for arg in "$@"; do
  case "$arg" in
    --model-usage) SHOW_MODEL_USAGE=1 ;;
    *) echo "未知参数: $arg（仅支持 --model-usage）" >&2; exit 1 ;;
  esac
done

[ -f "$CONFIG" ] || { echo "错误：找不到配置文件 $CONFIG" >&2; exit 1; }
API_KEY=$(sed -n 's/^api_key = "\(.*\)"$/\1/p' "$CONFIG" | head -n 1)
[ -n "$API_KEY" ] || { echo "错误：config.toml 中未读到 api_key" >&2; exit 1; }

http_get() { curl -s -m 20 -H "Authorization: Bearer $API_KEY" "$1"; }

BODY=$(http_get "https://open.bigmodel.cn/api/monitor/usage/quota/limit")
case "$BODY" in
  *'"success":true'*|*'"code":200'*) : ;;
  *) BODY=$(http_get "https://bigmodel.cn/api/monitor/usage/quota/limit") ;;
esac
case "$BODY" in
  *'"success":true'*|*'"code":200'*) : ;;
  *)
    echo "错误：额度接口请求失败（两个域名均未返回成功响应）" >&2
    [ -n "$BODY" ] && printf '%s\n' "$BODY" | head -c 500 >&2
    exit 2
    ;;
esac

NOW=$(date +%s)
LEVEL=$(printf '%s' "$BODY" | sed -n 's/.*"level":"\([^"]*\)".*/\1/p')

reset_info() {
  _ms=${1:-0}
  case "$_ms" in ''|*[!0-9]*) echo "未知"; return ;; esac
  _sec=$(( _ms / 1000 ))
  _diff=$(( _sec - NOW ))
  [ "$_diff" -lt 0 ] && _diff=0
  _h=$(( _diff / 3600 ))
  _m=$(( (_diff % 3600) / 60 ))
  _when=$(date -d "@$_sec" '+%Y-%m-%d %H:%M' 2>/dev/null || date -r "$_sec" '+%Y-%m-%d %H:%M' 2>/dev/null || echo "@$_sec")
  echo "$_when（约 ${_h}小时${_m}分后重置）"
}

echo "══════════════════════════════════════════════"
echo " GLM Coding Plan 额度查询"
echo " 查询时间: $(date '+%Y-%m-%d %H:%M:%S')"
case "$LEVEL" in
  lite|Lite|LITE) echo " 套餐档位: Lite" ;;
  pro|Pro|PRO)    echo " 套餐档位: Pro" ;;
  max|Max|MAX)    echo " 套餐档位: Max" ;;
  *)              echo " 套餐档位: ${LEVEL:-未知}" ;;
esac
echo "══════════════════════════════════════════════"

TOKEN_RECS=$(printf '%s' "$BODY" | grep -o '{"type":"TOKENS_LIMIT"[^}]*}')
for REC in $TOKEN_RECS; do
  UNIT=$(printf '%s' "$REC"  | sed -n 's/.*"unit":\([0-9]*\).*/\1/p')
  PCT=$(printf '%s' "$REC"   | sed -n 's/.*"percentage":\([0-9.]*\).*/\1/p')
  RESET=$(printf '%s' "$REC" | sed -n 's/.*"nextResetTime":\([0-9]*\).*/\1/p')
  case "$UNIT" in
    3) TITLE="5 小时 Token 窗口" ;;
    6) TITLE="每周 Token 窗口" ;;
    *) TITLE="Token 窗口 (unit=$UNIT)" ;;
  esac
  REM=$(awk -v p="${PCT:-0}" 'BEGIN{printf "%.1f", 100-p}')
  echo ""
  echo "【$TITLE】"
  echo "  已用: ${PCT:-?}%    剩余: ${REM}%"
  echo "  重置: $(reset_info "$RESET")"
done

MCP_REC=$(printf '%s' "$BODY" | grep -o '{"type":"TIME_LIMIT"[^}]*}')
if [ -n "$MCP_REC" ]; then
  M_NUMBER=$(printf '%s' "$MCP_REC" | sed -n 's/.*"number":\([0-9]*\).*/\1/p')
  M_USAGE=$(printf '%s' "$MCP_REC" | grep -o '"usage":[0-9]*' | head -n1 | sed 's/"usage"://')
  M_CUR=$(printf '%s'   "$MCP_REC" | sed -n 's/.*"currentValue":\([0-9]*\).*/\1/p')
  M_REM=$(printf '%s'   "$MCP_REC" | sed -n 's/.*"remaining":\([0-9]*\).*/\1/p')
  M_PCT=$(printf '%s'   "$MCP_REC" | sed -n 's/.*"percentage":\([0-9.]*\).*/\1/p')
  M_RESET=$(printf '%s' "$MCP_REC" | sed -n 's/.*"nextResetTime":\([0-9]*\).*/\1/p')
  echo ""
  echo "【MCP 工具月度配额】(联网搜索/网页读取/开源仓库)"
  echo "  已用: ${M_PCT:-?}%"
  echo "  进度: 已用 ${M_CUR:-?} / 总额度 ${M_USAGE:-?}，剩余 ${M_REM:-?} 次"
  echo "  重置: $(reset_info "$M_RESET")"
  DETAILS=$(printf '%s' "$BODY" | grep -o '"modelCode":"[^"]*","usage":[0-9]*')
  if [ -n "$DETAILS" ]; then
    echo "  明细:"
    printf '%s\n' "$DETAILS" | sed 's/"modelCode":"\([^"]*\)","usage":\([0-9]*\)/    - \1: 已用 \2/'
  fi
  echo "  （注：usage=总额度、currentValue=已用、remaining=剩余，字段名以平台返回为准）"
fi

if [ "$SHOW_MODEL_USAGE" = "1" ]; then
  echo ""
  echo "──────────────────────────────────────────────"
  echo "【24 小时模型用量】(model-usage)"
  MU=$(http_get "https://open.bigmodel.cn/api/monitor/usage/model-usage")
  if [ -z "$MU" ]; then
    MU=$(http_get "https://bigmodel.cn/api/monitor/usage/model-usage")
  fi
  if [ -z "$MU" ] || [ "$MU" = "{}" ]; then
    echo "  暂无数据（该端点在无用量时可能返回空体）"
  else
    printf '%s\n' "$MU" | head -c 2000
    echo ""
  fi
fi

echo ""
echo "══════════════════════════════════════════════"
echo " 说明: percentage=已用百分比; 剩余=100-已用。"
echo " 端点为平台内部监控接口, 结构可能随平台调整。"
