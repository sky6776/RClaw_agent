#!/usr/bin/env python3
"""watch-positions akshare 兜底数据脚本(无金融 MCP 配置时的数据轨)。

子命令(均输出 JSON 到 stdout;出错输出 {"error": ...} 并以非零码退出):
  quotes         --codes 600519.SH,00700.HK[,AAPL]        实时/最新快照
  hist           --code 600519.SH --start 2026-08-01 --end 2026-09-03 [--adjust ""]
                 日频 K 线(收盘/涨跌幅/成交量…)
  index          --codes 000300,000001                     指数快照
  index_hist     --code 000300 --start ... --end ...       指数日频
  events         --code 600519.SH [--months 6]             股权质押比例 + 限售解禁
  announcements  --code 600519.SH [--days 30] [--limit 20] 公告列表(巨潮)
  news           --code 600519.SH [--limit 15]             个股新闻(东财)

代码形态:.SH/.SZ/.HK 后缀与裸代码均可;美股 .O(纳斯达克)/.N(纽交所)/.AMEX。
依赖:akshare(以及其携带的 pandas 等)。缺依赖时错误信息带安装指引;
Termux 上原生依赖(numpy/pandas)用 `pkg install python-numpy python-pandas` 或 TUR,
不要用 pip 源码编译。
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import sys
import time

# akshare 内部用 tqdm 画进度条且可能写进 stdout,污染 JSON 输出——一律禁用
os.environ.setdefault("TQDM_DISABLE", "1")

IS_TERMUX = "/com.termux" in sys.prefix or "/data/data/com.termux" in sys.executable


def _emit(data) -> None:
    try:
        sys.stdout.reconfigure(encoding="utf-8")
    except Exception:
        pass
    print(json.dumps(data, ensure_ascii=False, default=str))


def _fail(message: str, code: int = 2) -> None:
    _emit({"error": message})
    raise SystemExit(code)


def _patch_requests_ua() -> None:
    """akshare 用 python-requests 默认 UA 请求东财会被服务端直接断连;
    在模块级 requests.get 上注入浏览器 UA / Referer 后恢复稳定。"""
    import requests  # noqa: PLC0415

    if getattr(requests.get, "_rclaw_ua_patched", False):
        return
    original_get = requests.get

    def patched_get(url, **kwargs):
        headers = dict(kwargs.pop("headers", None) or {})
        headers.setdefault("User-Agent", _UA)
        headers.setdefault("Referer", "https://quote.eastmoney.com/")
        return original_get(url, headers=headers, **kwargs)

    patched_get._rclaw_ua_patched = True  # type: ignore[attr-defined]
    requests.get = patched_get  # type: ignore[assignment]


def _retry_call(func, *args, attempts: int = 4, **kwargs):
    """东财接口按服务器轮换,部分节点随机断连/空回复——统一带间隔重试。"""
    import time  # noqa: PLC0415

    last_exc: Exception | None = None
    for attempt in range(attempts):
        try:
            return func(*args, **kwargs)
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
            if attempt + 1 < attempts:
                time.sleep(1.2)
    raise last_exc  # type: ignore[misc]


def _import_akshare():
    try:
        import akshare  # noqa: PLC0415
    except ImportError:
        if IS_TERMUX:
            hint = (
                "Termux 安装指引:pkg install python numpy pandas —— 再 "
                "pip install akshare requests;不要用 pip 直接装 numpy/pandas(会源码编译失败)"
            )
        else:
            hint = "pip install akshare(python>=3.9)"
        _fail(f"缺少 akshare:{hint}")
    _patch_requests_ua()
    return akshare


def _norm_code(code: str) -> tuple[str, str]:
    """'600519.SH' -> ('600519', 'SH');裸代码按规则推断市场。"""
    code = code.strip().upper()
    if "." in code:
        symbol, market = code.split(".", 1)
    else:
        symbol, market = code, ""
    if not market:
        if symbol.isdigit() and len(symbol) == 6:
            market = "SH" if symbol.startswith(("6", "9", "5")) else "SZ"
        elif symbol.isdigit() and len(symbol) == 5:
            market = "HK"
        else:
            market = "O"
    if market not in {"SH", "SZ", "HK", "O", "N", "AMEX", "AAPL"}:
        _fail(f"不支持的市场后缀:{code}(支持 .SH/.SZ/.HK/.O/.N/.AMEX)")
    return symbol, market


def _market_segment(market: str) -> str:
    return {"SH": "1", "SZ": "0", "HK": "116", "O": "105", "N": "106", "AMEX": "107"}.get(market, "0")


_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
)
# push2 单标的快照字段 -> 输出键名(值为百分数的字段在 meta 里注明口径)
_PUSH2_FIELDS = "f43,f44,f45,f46,f47,f48,f57,f58,f60,f168,f169,f170"
_PUSH2_KEYS = {
    "f57": "代码", "f58": "名称", "f43": "最新价", "f44": "最高", "f45": "最低",
    "f46": "今开", "f60": "昨收", "f47": "成交量", "f48": "成交额",
    "f168": "换手率", "f169": "涨跌额", "f170": "涨跌幅",
}


def _push2_quote(secid: str, retries: int = 4) -> dict | None:
    """东方财富 push2 单标的快照:每标的一次请求,带 UA/Referer 与重试。

    东财按服务器轮换响应,部分节点会断连或返回空 data——两者都重试。
    """
    import time  # noqa: PLC0415
    import urllib.parse  # noqa: PLC0415

    try:
        import requests  # noqa: PLC0415
    except ImportError:
        _fail("缺少 requests:pip install requests")
    url = (
        "https://push2.eastmoney.com/api/qt/stock/get?"
        + urllib.parse.urlencode({"secid": secid, "invt": "2", "fltt": "2", "fields": _PUSH2_FIELDS})
    )
    headers = {"User-Agent": _UA, "Referer": "https://quote.eastmoney.com/"}
    for attempt in range(retries):
        try:
            resp = requests.get(url, headers=headers, timeout=10)
            data = (resp.json() or {}).get("data")
        except Exception:  # noqa: BLE001 — 空回复/断连时轮换重试
            data = None
        if data and data.get("f57") not in (None, "-"):
            return {_PUSH2_KEYS[k]: data[k] for k in _PUSH2_KEYS if data.get(k) not in (None, "-")}
        if attempt + 1 < retries:
            time.sleep(0.8)
    return None


def _tencent_quote(secid: str) -> dict | None:
    """腾讯 qt.gtimg.cn 快照:push2 不可达(本网络实测 TCP 被重置)时的同键回退,仅 SH/SZ。

    secid 形如 sh600519 / sz000001。输出键与 _push2_quote 一致;
    腾讯成交额口径为万元,乘 1e4 对齐 push2 f48(元)。港股/美股字段口径差异大,不在此回退。
    """
    try:
        import requests  # noqa: PLC0415
    except ImportError:
        _fail("缺少 requests:pip install requests")

    def _num(idx: int) -> float | None:
        v = parts[idx]
        return float(v) if v not in ("", "-") else None

    try:
        resp = requests.get(f"https://qt.gtimg.cn/q={secid}", headers={"User-Agent": _UA}, timeout=10)
        payload = resp.text.split('"')
        if len(payload) < 2:
            return None
        parts = payload[1].split("~")
        if len(parts) < 39 or not parts[3]:
            return None
        turnover = _num(37)
        return {
            "代码": parts[2], "名称": parts[1], "最新价": _num(3), "最高": _num(33),
            "最低": _num(34), "今开": _num(5), "昨收": _num(4),
            "成交量": _num(36), "成交额": turnover * 1e4 if turnover is not None else None,
            "换手率": _num(38), "涨跌额": _num(31), "涨跌幅": _num(32),
        }
    except Exception:  # noqa: BLE001 — 回退源失败按无数据处理
        return None


def _df_records(df) -> list[dict]:
    return json.loads(df.to_json(orient="records", force_ascii=False))


def _pick(records: list[dict], symbol: str, keys=("代码", "symbol", "code")) -> list[dict]:
    out = []
    for row in records:
        for k in keys:
            if str(row.get(k, "")).upper() == symbol.upper():
                out.append(row)
                break
    return out


def _cmd_quotes(args) -> None:
    result = {}
    for raw in args.codes.split(","):
        symbol, market = _norm_code(raw)
        secid = f"{_market_segment(market)}.{symbol}"
        quote = _push2_quote(secid)
        if quote is None and market in {"SH", "SZ"}:
            quote = _tencent_quote(f"{market.lower()}{symbol}")
        result[f"{symbol}.{market}"] = quote
    _emit({
        "source": "东方财富 push2(经 ak_fallback);push2 不可达时 SH/SZ 单标的回退腾讯 qt.gtimg.cn",
        "as_of": _dt.datetime.now().isoformat(timespec="seconds"),
        "unit_note": "涨跌幅/换手率为百分数口径:2.57 表示 +2.57%",
        "quotes": result,
    })


def _tencent_klines(secid: str, start: str, end: str, adjust: str, rows: int = 800) -> list[dict]:
    """腾讯行情 K 线(本网络下东财 push2his 域不可达,腾讯源稳定)。

    secid 形如 sh600519 / sz000001 / hk00700 / usAAPL.O / sh000300。
    返回 [{日期, 开盘, 收盘, 最高, 最低, 成交量}](价格为不复权或按 adjust 指定复权)。
    """
    import requests  # noqa: PLC0415

    # 复权口径走 fqkline(键 qfqday/hfqday);不复权必须走 kline(键 day)——
    # 在 fqkline 上省略 fq 段会得到 code:1 的错误应答。
    fq = adjust if adjust in ("qfq", "hfq") else ""
    if fq:
        url = "https://web.ifzq.gtimg.cn/appstock/app/fqkline/get"
        param = f"{secid},day,{start},{end},{rows},{fq}"
    else:
        url = "https://web.ifzq.gtimg.cn/appstock/app/kline/kline"
        param = f"{secid},day,{start},{end},{rows}"
    last_exc: Exception | None = None
    for attempt in range(5):
        try:
            resp = requests.get(
                url,
                params={"param": param},
                headers={"User-Agent": _UA, "Referer": "https://gu.qq.com/"},
                timeout=10,
            )
            data = resp.json()["data"][secid]
            key = "qfqday" if fq == "qfq" and "qfqday" in data else ("hfqday" if fq == "hfq" and "hfqday" in data else "day")
            raw = data.get(key) or []
            if raw:  # 腾讯在突发请求下会返回空载荷,视为可重试
                out = []
                for item in raw:
                    # 行格式: [日期, 开盘, 收盘, 最高, 最低, 成交量, ...]
                    out.append({
                        "日期": item[0], "开盘": float(item[1]), "收盘": float(item[2]),
                        "最高": float(item[3]), "最低": float(item[4]),
                        "成交量": float(item[5]) if len(item) > 5 else None,
                    })
                return out
            last_exc = RuntimeError("腾讯行情返回空载荷(疑似限流)")
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
        time.sleep(1.5 + attempt)
    raise last_exc  # type: ignore[misc]


def _stock_secid(symbol: str, market: str) -> str:
    if market in {"SH", "SZ"}:
        return f"{market.lower()}{symbol}"
    if market == "HK":
        return f"hk{symbol}"  # 腾讯港股代码保留前导零:hk00700
    if market == "O":
        return f"us{symbol}.OQ"  # 腾讯纳斯达克后缀是 .OQ
    if market == "N":
        return f"us{symbol}.N"
    if market == "AMEX":
        return f"us{symbol}.AM"
    _fail(f"hist 不支持的市场:{market}")


def _cmd_hist(args) -> None:
    symbol, market = _norm_code(args.code)
    rows = _tencent_klines(_stock_secid(symbol, market), args.start, args.end, args.adjust)
    _emit({
        "source": "腾讯行情(经 ak_fallback)",
        "code": f"{symbol}.{market}",
        "unit_note": "价格为该复权口径;涨跌幅需由相邻收盘价自行计算 [测算]",
        "rows": rows,
    })


def _us_symbol(symbol: str) -> str:
    import re  # noqa: PLC0415

    return re.sub(r"\.(O|N|AMEX)$", "", symbol, flags=re.IGNORECASE)


def _index_secid(code: str) -> str:
    """指数代码 -> push2 secid。6 位数字:000xxx 视为上证系指数,399xxx/880xxx 视为深证系;
    命名指数(HSI/HSTECH/DJIA/SPX/NDX 等)走 100.<name>。"""
    code = code.strip().upper()
    if code.isdigit() and len(code) == 6:
        if code.startswith("000"):
            return f"1.{code}"
        if code.startswith("399"):
            return f"0.{code}"
        _fail(f"无法识别的指数代码:{code}(示例:000300 沪深300、399001 深证成指、HSI 恒生指数)")
    if code.isalpha():
        return f"100.{code}"
    _fail(f"无法识别的指数代码:{code}")


def _cmd_index(args) -> None:
    result = {}
    for raw in args.codes.split(","):
        code = raw.strip().upper()
        secid = _index_secid(code)
        result[code] = _push2_quote(secid)
        if result[code] is None and secid[:2] in {"1.", "0."}:
            result[code] = _tencent_quote(f"{'sh' if secid.startswith('1.') else 'sz'}{code}")
    _emit({
        "source": "东方财富 push2(经 ak_fallback);push2 不可达时沪深指数回退腾讯 qt.gtimg.cn",
        "as_of": _dt.datetime.now().isoformat(timespec="seconds"),
        "unit_note": "涨跌幅为百分数口径:-0.09 表示 -0.09%",
        "index": result,
    })


def _cmd_index_hist(args) -> None:
    code = args.code.strip().upper()
    if code.startswith("000"):
        secid = f"sh{code}"
    elif code.startswith("399"):
        secid = f"sz{code}"
    elif code.isalpha():
        secid = f"hk{code}"
    else:
        _fail(f"无法识别的指数代码:{code}(示例:000300、399001、HSI)")
    rows = _tencent_klines(secid, args.start, args.end, "")
    _emit({
        "source": "腾讯行情(经 ak_fallback)",
        "code": code,
        "unit_note": "涨跌幅需由相邻收盘价自行计算 [测算]",
        "rows": rows,
    })


def _cmd_events(args) -> None:
    ak = _import_akshare()
    symbol, market = _norm_code(args.code)
    if market not in {"SH", "SZ"}:
        _fail("events 子命令目前仅支持 A 股(.SH/.SZ);港美股请走通用网络搜索并如实标注")
    out: dict[str, object] = {"code": f"{symbol}.{market}"}
    today = _dt.date.today()
    # 质押比例是周五更新的周度截面:取最近一个周五
    last_friday = today - _dt.timedelta(days=(today.weekday() - 4) % 7 or 7 if today.weekday() >= 4 else today.weekday() + 3)
    try:
        table = _retry_call(ak.stock_gpzy_pledge_ratio_em, date=last_friday.strftime("%Y%m%d"))
        rows = _pick(_df_records(table), symbol, keys=("股票代码",))
        out["pledge_ratio"] = rows[0] if rows else None
        out["pledge_ratio_date"] = last_friday.strftime("%Y-%m-%d")
    except Exception as exc:  # noqa: BLE001 — 单项失败不拖垮整个事件查询
        out["pledge_ratio_error"] = str(exc)[:200]
    try:
        queue = _retry_call(ak.stock_restricted_release_queue_em, symbol=symbol)
        out["restricted_release_upcoming"] = _df_records(queue) if queue is not None else []
        out["restricted_release_note"] = "为东财待解禁队列;已发生的解禁请用公告正文核对"
    except Exception as exc:  # noqa: BLE001
        out["restricted_release_error"] = str(exc)[:200]
    out["source"] = "东方财富(经 akshare)"
    _emit(out)


def _cmd_announcements(args) -> None:
    """东财公告 API 直连(cninfo 接口在当前 akshare 版本已失效)。"""
    import requests  # noqa: PLC0415

    symbol, market = _norm_code(args.code)
    if market not in {"SH", "SZ", "HK"}:
        _fail("announcements 支持 .SH/.SZ/.HK;美股请走通用网络搜索并如实标注")
    url = "https://np-anotice-stock.eastmoney.com/api/security/ann"
    headers = {"User-Agent": _UA, "Referer": "https://data.eastmoney.com/"}
    params = {
        "sr": "-1", "page_size": "50", "page_index": "1",
        "ann_type": "A" if market in {"SH", "SZ"} else "H",
        "client_source": "web", "stock_list": symbol.lstrip("0") if market == "HK" else symbol,
        "f_node": "0", "s_node": "0",
    }
    last_exc: Exception | None = None
    for _ in range(3):
        try:
            resp = requests.get(url, params=params, headers=headers, timeout=10)
            items = resp.json()["data"]["list"] or []
            break
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
            items = []
            time.sleep(1.2)
    else:
        _fail(f"东财公告接口失败:{last_exc}")
    end = _dt.date.today()
    start = end - _dt.timedelta(days=args.days)
    rows = []
    for it in items:
        date_str = (it.get("notice_date") or "")[:10]
        if not date_str or not (start.isoformat() <= date_str <= end.isoformat()):
            continue
        rows.append({
            "公告日期": date_str,
            "公告标题": it.get("title"),
            "art_code": it.get("art_code"),
        })
        if len(rows) >= args.limit:
            break
    _emit({
        "source": "东方财富公告(经 ak_fallback)",
        "code": f"{symbol}.{market}",
        "window": f"{start}~{end}",
        "rows": rows,
    })


def _cmd_news(args) -> None:
    ak = _import_akshare()
    symbol, market = _norm_code(args.code)
    df = _retry_call(ak.stock_news_em, symbol=symbol)
    rows = _df_records(df)[: args.limit]
    _emit({"source": "东方财富(经 akshare)", "code": f"{symbol}.{market}", "rows": rows})


def main() -> None:
    parser = argparse.ArgumentParser(description="watch-positions akshare 兜底数据脚本")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("quotes", help="实时/最新快照")
    p.add_argument("--codes", required=True, help="逗号分隔,如 600519.SH,00700.HK,AAPL")
    p.set_defaults(func=_cmd_quotes)

    p = sub.add_parser("hist", help="日频 K 线")
    p.add_argument("--code", required=True)
    p.add_argument("--start", required=True, help="YYYY-MM-DD")
    p.add_argument("--end", required=True, help="YYYY-MM-DD")
    p.add_argument("--adjust", default="", help='复权:""(不复权)/"qfq"/"hfq"')
    p.set_defaults(func=_cmd_hist)

    p = sub.add_parser("index", help="指数快照")
    p.add_argument("--codes", required=True, help="逗号分隔,如 000300,000001")
    p.set_defaults(func=_cmd_index)

    p = sub.add_parser("index_hist", help="指数日频")
    p.add_argument("--code", required=True)
    p.add_argument("--start", required=True)
    p.add_argument("--end", required=True)
    p.set_defaults(func=_cmd_index_hist)

    p = sub.add_parser("events", help="股权质押比例 + 限售解禁(A 股)")
    p.add_argument("--code", required=True)
    p.add_argument("--months", type=int, default=6, help="解禁回看月数")
    p.set_defaults(func=_cmd_events)

    p = sub.add_parser("announcements", help="公告列表(巨潮)")
    p.add_argument("--code", required=True)
    p.add_argument("--days", type=int, default=30)
    p.add_argument("--limit", type=int, default=20)
    p.set_defaults(func=_cmd_announcements)

    p = sub.add_parser("news", help="个股新闻(东财)")
    p.add_argument("--code", required=True)
    p.add_argument("--limit", type=int, default=15)
    p.set_defaults(func=_cmd_news)

    args = parser.parse_args()
    try:
        args.func(args)
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001 — 顶层兜底:任何取数失败都以 JSON 错误退出
        _fail(f"{type(exc).__name__}: {exc}"[:500])


if __name__ == "__main__":
    main()
