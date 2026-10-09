#!/usr/bin/env python3
"""read-macro akshare 兜底数据脚本(无金融 MCP 配置时的宏观数据轨)。

子命令(均输出 JSON 到 stdout;出错输出 {"error": ...} 并以非零码退出):
  macro        --series cpi,ppi,pmi,gdp,m2,shrzgm,lpr,reserves   宏观序列(每条返回最近 N 行)
  rate         --rows 12                                          中美国债关键期限收益率
  index-value  --symbol 沪深300 [--pe-rolling 滚动市盈率]           指数 PE/PB 历史与分位(自算,窗口=数据实际起点)
  index-hist   --symbol 000300 --start YYYY-MM-DD --end YYYY-MM-DD  指数价格序列(腾讯行情)

依赖:akshare(以及其携带的 pandas 等)。缺依赖时错误信息带安装指引;
Termux 上原生依赖(numpy/pandas)用 `pkg install python-numpy python-pandas`,不要 pip 源码编译。
口径提醒:本脚本为公开接口口径(东方财富/乐咕乐股/中国货币网等),与万得 EDB 不同——
来源标注写实际数据方,不得标成万得/同花顺。
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

_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
)


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


def _import_akshare():
    try:
        import akshare  # noqa: PLC0415
    except ImportError:
        if IS_TERMUX:
            hint = "Termux: pkg install python numpy pandas 后再 pip install akshare requests"
        else:
            hint = "pip install akshare(python>=3.9)"
        _fail(f"缺少 akshare:{hint}")
    _patch_requests_ua()
    return akshare


def _retry_call(func, *args, attempts: int = 4, **kwargs):
    """东财/公开数据接口按服务器轮换,部分节点随机断连——统一带间隔重试。"""
    last_exc: Exception | None = None
    for attempt in range(attempts):
        try:
            return func(*args, **kwargs)
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
            if attempt + 1 < attempts:
                time.sleep(1.2)
    raise last_exc  # type: ignore[misc]


def _df_records(df) -> list[dict]:
    return json.loads(df.to_json(orient="records", force_ascii=False))


# ---------------------------------------------------------------- macro 序列
MACRO_SERIES = {
    "cpi": ("macro_china_cpi_monthly", "CPI 月度(东方财富:今值/预测值/前值)"),
    "ppi": ("macro_china_ppi", "PPI 当月同比"),
    "pmi": ("macro_china_pmi", "制造业/非制造业 PMI"),
    "gdp": ("macro_china_gdp", "GDP 季度同比"),
    "m2": ("macro_china_money_supply", "M2/M1/M0 数量与同比"),
    "shrzgm": ("macro_china_shrzgm", "社会融资规模增量(当月)"),
    "lpr": ("macro_china_lpr", "LPR 1Y/5Y 报价"),
    "reserves": ("macro_china_foreign_exchange_gold", "国家外汇储备与黄金储备(月度)"),
}


def _cmd_macro(args) -> None:
    ak = _import_akshare()
    out = {}
    for raw in args.series.split(","):
        key = raw.strip().lower()
        if not key:
            continue
        if key not in MACRO_SERIES:
            _fail(
                f"不支持的序列:{key}。支持:{','.join(MACRO_SERIES)};"
                "其余序列(如 DR007、信用利差、省级 EDB)在兜底轨为 源不可用"
            )
        func_name, label = MACRO_SERIES[key]
        try:
            df = _retry_call(getattr(ak, func_name))
            # 接口返回顺序不一(部分新行在前):按第一个日期/期间列升序排稳后再取尾部
            date_col = next(
                (c for c in ("日期", "月份", "季度", "统计时间", "TRADE_DATE") if c in df.columns),
                None,
            )
            if date_col is not None:
                df = df.sort_values(date_col)
            rows = _df_records(df.tail(args.rows))
            out[key] = {"label": label, "interface": func_name, "latest": rows[-1] if rows else None, "rows": rows}
        except Exception as exc:  # noqa: BLE001 — 单序列失败不拖垮整批
            out[key] = {"label": label, "error": f"{type(exc).__name__}: {exc}"[:200]}
    _emit({
        "source": "东方财富/公开统计接口(经 akshare,akshare 兜底轨)",
        "as_of": _dt.datetime.now().isoformat(timespec="seconds"),
        "unit_note": "各序列单位/口径以返回列名为准,发布滞后以最新报告期与检索时间差观测 [测算]",
        "series": out,
    })


# ---------------------------------------------------------------- rate
def _cmd_rate(args) -> None:
    ak = _import_akshare()
    df = _retry_call(ak.bond_zh_us_rate)
    _emit({
        "source": "中国货币网/东方财富(经 akshare)",
        "rows": _df_records(df.tail(args.rows)),
        "unit_note": "收益率为百分数口径;含中国国债 2/5/10/30 年与美债对应期限、10y-2y 利差",
    })


# ---------------------------------------------------------------- index-value
def _cmd_index_value(args) -> None:
    ak = _import_akshare()
    out = {"symbol": args.symbol, "windows": {}}
    try:
        pe = _retry_call(ak.stock_index_pe_lg, symbol=args.symbol)
    except Exception as exc:  # noqa: BLE001
        _fail(f"乐咕乐股接口失败({args.symbol}):{exc}"[:200])
    pe_col = "滚动市盈率" if args.pe_col in ("滚动市盈率", "ttm") else args.pe_col
    if pe_col not in pe.columns:
        _fail(f"列 {pe_col} 不存在;可用列:{','.join(pe.columns)}")
    dates = pe["日期"].tolist()
    values = pe[pe_col].tolist()
    today = _dt.date.today()

    def percentile(window_days: int | None) -> dict:
        if window_days is None:
            sub = [(d, v) for d, v in zip(dates, values) if v is not None]
            label = f"全部可用历史({dates[0]} 起)"
        else:
            cutoff = today - _dt.timedelta(days=window_days)
            sub = [(d, v) for d, v in zip(dates, values) if v is not None and str(d) >= cutoff.isoformat()]
            label = f"近 {window_days // 365} 年({sub[0][0] if sub else 'n/a'} 起)"
        if len(sub) < 30:
            return {"error": f"样本不足({len(sub)} 点)"}
        vs = sorted(v for _, v in sub)
        cur = sub[-1][1]
        rank = sum(1 for v in vs if v < cur)
        return {
            "当前值": cur, "分位": round(100.0 * rank / len(vs), 2),
            "排名": rank + 1, "样本数": len(vs), "窗口": label,
            "标签": "[测算](基于公开 PE 历史,方法:当前值在窗口内的经验分位)",
        }

    out["pe"] = {"列": pe_col, "数据起点": str(dates[0]), "最新日期": str(dates[-1]),
                 "windows": {"1y": percentile(365), "3y": percentile(365 * 3), "full": percentile(None)}}
    try:
        pb = _retry_call(ak.stock_index_pb_lg, symbol=args.symbol)
        out["pb"] = {"数据起点": str(pb['日期'].iloc[0]),
                     "最新": _df_records(pb.tail(1))[0]}
    except Exception as exc:  # noqa: BLE001
        out["pb"] = {"error": str(exc)[:200]}
    _emit({
        "source": "乐咕乐股(经 akshare;指数 PE/PB 历史公开口径)",
        "as_of": _dt.datetime.now().isoformat(timespec="seconds"),
        "result": out,
    })


# ---------------------------------------------------------------- index-hist(腾讯行情,同 watch-positions/ak_fallback)
def _tencent_klines(secid: str, start: str, end: str, rows: int = 800) -> list[dict]:
    import requests  # noqa: PLC0415

    url = "https://web.ifzq.gtimg.cn/appstock/app/kline/kline"
    param = f"{secid},day,{start},{end},{rows}"
    last_exc: Exception | None = None
    for attempt in range(5):
        try:
            resp = requests.get(
                url, params={"param": param},
                headers={"User-Agent": _UA, "Referer": "https://gu.qq.com/"}, timeout=10,
            )
            data = resp.json()["data"][secid]
            raw = data.get("day") or []
            if raw:
                return [{"日期": r[0], "收盘": float(r[2])} for r in raw]
            last_exc = RuntimeError("腾讯行情返回空载荷(疑似限流)")
        except Exception as exc:  # noqa: BLE001
            last_exc = exc
        time.sleep(1.5 + attempt)
    raise last_exc  # type: ignore[misc]


def _cmd_index_hist(args) -> None:
    code = args.symbol.strip()
    if code.isdigit() and len(code) == 6:
        secid = ("sh" if code.startswith("000") else "sz") + code
        secid = "sh" + code if code.startswith("000") else secid
    else:
        _fail(f"index-hist 目前支持 6 位数字指数代码;命名指数请用 index-value 或通用搜索")
    rows = _tencent_klines(secid, args.start, args.end)
    _emit({"source": "腾讯行情(经 ak_fallback)", "code": code, "rows": rows})


def main() -> None:
    parser = argparse.ArgumentParser(description="read-macro akshare 兜底数据脚本")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("macro", help="宏观序列")
    p.add_argument("--series", required=True, help=f"逗号分隔:{','.join(MACRO_SERIES)}")
    p.add_argument("--rows", type=int, default=14, help="每条序列返回的最近行数")
    p.set_defaults(func=_cmd_macro)

    p = sub.add_parser("rate", help="中美国债收益率")
    p.add_argument("--rows", type=int, default=12)
    p.set_defaults(func=_cmd_rate)

    p = sub.add_parser("index-value", help="指数 PE 历史与分位")
    p.add_argument("--symbol", default="沪深300")
    p.add_argument("--pe-col", default="滚动市盈率", help="PE 列(滚动市盈率/静态市盈率/滚动市盈率中位数等)")
    p.set_defaults(func=_cmd_index_value)

    p = sub.add_parser("index-hist", help="指数价格序列")
    p.add_argument("--symbol", required=True, help="6 位指数代码,如 000300")
    p.add_argument("--start", required=True)
    p.add_argument("--end", required=True)
    p.set_defaults(func=_cmd_index_hist)

    args = parser.parse_args()
    try:
        args.func(args)
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001
        _fail(f"{type(exc).__name__}: {exc}"[:500])


if __name__ == "__main__":
    main()
