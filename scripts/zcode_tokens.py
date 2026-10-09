#!/usr/bin/env python3
"""从 ZCode 客户端本机凭据库提取金融 MCP 网关的两个鉴权令牌。

背景:rclaw.toml 的 [mcp.servers.*.headers] 需要
  Authorization            = Bearer <ZCode JWT>
  X-Bigmodel-Authorization = Bearer <bigmodel OAuth access token>
两个令牌由服务器签发、随 ZCode 客户端登录态轮换;本脚本从客户端的加密凭据文件
(~/.zcode/v2/credentials.json)解密出**当前有效值**,解决"过期需重新提取"的问题
——客户端刷新登录态后重跑本脚本即可。

解密格式(自 ZCode 桌面端 app 包确认):enc:v1:base64url(nonce)."base64url(tag)."base64url(ct),
AES-256-GCM,密钥 = sha256(秘密);秘密 = 环境变量 ZCODE_CREDENTIAL_SECRET(若设置),
否则回退字符串 zcode-credential-fallback:{platform}:{homedir}:{username}。

用法:
  python scripts/zcode_tokens.py                 # 提取并脱敏显示(默认)
  python scripts/zcode_tokens.py --print         # 明文打印(自行重定向,勿贴聊天)
  python scripts/zcode_tokens.py --probe         # 提取后实测网关 initialize + 真实工具调用(余额/设备限制在此暴露)
  python scripts/zcode_tokens.py --write PATH    # 就地更新 rclaw.toml 的两组 headers(注释保真)

安全:令牌是登录凭据。默认只显示前 6 位;--write 只改 header 值,不动其他内容。
"""
from __future__ import annotations

import argparse
import base64
import datetime as _dt
import hashlib
import json
import os
import re
import sys
import urllib.parse  # noqa: F401 (保持与探针请求同构时可用)

CREDENTIALS = os.path.expanduser("~/.zcode/v2/credentials.json")
GATEWAY = "https://zcode.z.ai/api/v1/mcp/server/finance_hexin_stock"


def _secret() -> str:
    env = os.environ.get("ZCODE_CREDENTIAL_SECRET")
    if env:
        return env
    return f"zcode-credential-fallback:{sys.platform}:{os.path.expanduser('~')}:{os.environ.get('USERNAME', os.environ.get('USER', ''))}"


def _decrypt(field_value: str) -> str:
    try:
        from cryptography.hazmat.primitives.ciphers.aead import AESGCM  # noqa: PLC0415
    except ImportError:
        raise SystemExit("缺少 cryptography:pip install cryptography")
    if not field_value.startswith("enc:v1:"):
        return field_value  # 未加密存储的历史形态
    parts = field_value[len("enc:v1:"):].split(".")
    unpad = lambda s: base64.urlsafe_b64decode(s + "=" * (-len(s) % 4))
    nonce, tag, ct = unpad(parts[0]), unpad(parts[1]), unpad(parts[2])
    key = hashlib.sha256(_secret().encode()).digest()
    return AESGCM(key).decrypt(nonce, ct + tag, None).decode("utf-8")


def load_tokens() -> tuple[str, str]:
    if not os.path.exists(CREDENTIALS):
        raise SystemExit(f"未找到 ZCode 凭据文件:{CREDENTIALS}")
    with open(CREDENTIALS, encoding="utf-8") as f:
        creds = json.load(f)
    jwt = creds.get("zcodejwttoken")
    bm = creds.get("oauth:bigmodel:access_token")
    if not jwt or not bm:
        raise SystemExit("凭据文件缺少 zcodejwttoken 或 oauth:bigmodel:access_token(未登录?)")
    return _decrypt(jwt), _decrypt(bm)


def probe(jwt: str, bm: str) -> str:
    import requests  # noqa: PLC0415

    headers = {
        "Authorization": f"Bearer {jwt}",
        "X-Bigmodel-Authorization": f"Bearer {bm}",
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }

    def _post(payload: dict) -> requests.Response:
        return requests.post(GATEWAY, headers=headers, json=payload, timeout=30)

    resp = _post({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-03-26", "capabilities": {},
                   "clientInfo": {"name": "rclaw-token-check", "version": "0.0.1"}},
    })
    text = resp.text
    if '"result"' not in text or "serverInfo" not in text:
        if "no permission" in text:
            return "FAILED:网关返回 no permission——这不是 ZCode JWT,确认凭据文件来源"
        if "coding plan is required" in text:
            return "FAILED:网关返回 coding plan is required——JWT 有效但 X-Bigmodel-Authorization 缺失/无效"
        return f"UNKNOWN:{text[:200]}"
    # initialize 通过≠配额可用:余额耗尽/设备超限只在真实工具调用时暴露(2026-09-04 实测),
    # 因此 probe 以一次最小工具调用为准,而不是只握手。
    resp = _post({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "search_stocks", "arguments": {"domain": "stock", "query": "贵州茅台 最新价"}}})
    text = resp.text
    if '"isError":true' in text or '"isError": true' in text:
        if "余额不足" in text:
            return "FAILED:工具调用实测失败——网关返回「余额不足,请先充值」(数据源套餐额度耗尽,需充值)"
        if "Device exceed limit" in text or "-1303" in text:
            return "FAILED:工具调用实测失败——网关返回设备数超限([-1303] Device exceed limit),需在账号侧释放/绑定设备"
        m = re.search(r'"text":"(.{0,160})', text)
        return f"FAILED:工具调用实测失败:{m.group(1) if m else text[:160]}"
    if '"result"' in text:
        return "OK:initialize + 真实工具调用均通过,网关鉴权与配额可用"
    return f"UNKNOWN:{text[:200]}"


def write_toml(path: str, jwt: str, bm: str) -> int:
    """只改 [mcp.servers.*.headers] 下的两组 Bearer 值,其余内容(含注释)逐字保留。"""
    with open(path, encoding="utf-8") as f:
        s = f.read()
    patterns = [
        # 行首锚定,避免 Authorization 误匹配 X-Bigmodel-Authorization 的尾部
        (r'(?m)^([ 	]*Authorization\s*=\s*")Bearer [^"]*(")', rf"\g<1>Bearer {jwt}\g<2>"),
        (r'(?m)^([ 	]*X-Bigmodel-Authorization\s*=\s*")Bearer [^"]*(")', rf"\g<1>Bearer {bm}\g<2>"),
    ]
    n = 0
    for pat, rep in patterns:
        s, k = re.subn(pat, rep, s)
        n += k
    with open(path, "w", encoding="utf-8", newline="") as f:
        f.write(s)
    return n


def main() -> None:
    ap = argparse.ArgumentParser(description="提取 ZCode 金融 MCP 双令牌(解密本机凭据)")
    ap.add_argument("--print", action="store_true", help="明文输出两个令牌(默认脱敏)")
    ap.add_argument("--probe", action="store_true", help="提取后实测网关 initialize + 真实工具调用")
    ap.add_argument("--write", metavar="PATH", help="就地更新指定 rclaw.toml 的两组 headers")
    args = ap.parse_args()

    jwt, bm = load_tokens()
    stamp = _dt.datetime.now().isoformat(timespec="seconds")

    if args.write:
        n = write_toml(args.write, jwt, bm)
        print(f"[{stamp}] 已更新 {args.write} 的 {n} 处 header 值(重启 rclaw 后生效)")
    if args.print:
        print(f"Authorization={jwt}")
        print(f"X-Bigmodel-Authorization={bm}")
    else:
        print(f"[{stamp}] zcodejwttoken: {jwt[:6]}…(len={len(jwt)})")
        print(f"[{stamp}] bigmodel token: {bm[:6]}…(len={len(bm)})")
        iat = _issued_at(jwt)
        if iat:
            days = (_dt.datetime.now(_dt.timezone.utc) - iat).total_seconds() / 86400
            print(f"[{stamp}] 令牌签发于 {iat.isoformat()}(约 {days:.1f} 天前);两份令牌均无 exp 声明,"
                  f"有效性以服务器端会话为准——用 --probe 实测,不要按天数猜")
    if args.probe:
        print("probe:", probe(jwt, bm))


def _issued_at(token: str):
    """解码 JWT 载荷的 iat(不验签、不输出令牌本体);无 iat 返回 None。"""
    try:
        payload = token.split(".")[1]
        payload += "=" * (-len(payload) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))
        return _dt.datetime.fromtimestamp(claims["iat"], _dt.timezone.utc) if "iat" in claims else None
    except Exception:  # noqa: BLE001 — 非 JWT 形态或无 iat 时不阻塞主流程
        return None


if __name__ == "__main__":
    main()
