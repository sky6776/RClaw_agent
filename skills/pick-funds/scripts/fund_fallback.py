#!/usr/bin/env python3
"""pick-funds akshare 兜底数据脚本(无金融 MCP 配置时的基金数据轨)。

子命令(均输出 JSON 到 stdout;出错输出 {"error": ...} 并以非零码退出):
  rank      --type 混合 [--exclude-words 债,指数,联接] [--sort 近1年] [--top 15]  全市场基金筛选(名称包含匹配)
  profile   --code 000001                                               单基金基础档案
  nav       --code 000001 [--rows 500]                                  累计净值序列
  holdings  --code 000001 [--year 2025]                                 按季度前十大持仓

依赖:akshare。缺依赖时错误信息带安装指引;Termux 上原生依赖用 pkg/TUR。
口径提醒:本脚本为天天基金/东财公开口径,与同花顺/万得不同——来源标注写实际数据方。
「近N年」收益为累计口径;年化需自行 (1+R)^(1/n)−1 复算并标 [测算]。
"""
from __future__ import annotations

import argparse
import datetime as _dt
import json
import os
import sys
import time

os.environ.setdefault("TQDM_DISABLE", "1")

IS_TERMUX = "/com.termux" in sys.prefix or "/data/data/com.termux" in sys.executable
_UA = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/126.0 Safari/537.36"
)

# rank 表的规模列是「合并规模」口径的近似:东财排名表无直接规模列,规模走 profile 查询。
RETURN_COLS = ["近1周", "近1月", "近3月", "近6月", "近1年", "近2年", "近3年", "今年来", "成立来"]


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
        headers.setdefault("Referer", "https://fund.eastmoney.com/")
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


def _to_float(value):
    try:
        v = str(value).replace("%", "").replace(",", "")
        return float(v) if v not in ("", "--", "None") else None
    except Exception:  # noqa: BLE001
        return None


# ---------------------------------------------------------------- rank(筛选引擎)
def _cmd_rank(args) -> None:
    ak = _import_akshare()
    df = _retry_call(ak.fund_open_fund_rank_em, symbol="全部")
    records = _df_records(df)
    out = []
    for row in records:
        name = str(row.get("基金简称", ""))
        if args.type and args.type not in name:
            continue
        if args.exclude_words and any(w in name for w in args.exclude_words.split(",")):
            continue
        score = _to_float(row.get(args.sort))
        if score is None:
            continue
        if args.min_return is not None and score < args.min_return:
            continue
        row["_sort值"] = score
        out.append(row)
    out.sort(key=lambda r: r["_sort值"], reverse=args.order != "asc")
    out = out[: args.top]
    for row in out:
        row.pop("_sort值", None)
    _emit({
        "source": "天天基金/东方财富(经 akshare)",
        "as_of": _dt.datetime.now().isoformat(timespec="seconds"),
        "criteria": {
            "名称含": args.type, "排除名称含": args.exclude_words or "(无)",
            "排序字段": args.sort, "阈值": args.min_return, "返回条数": len(out),
        },
        "unit_note": "区间收益为累计口径(%);年化需自行复算 [测算];规模请对入选基金逐一 profile 查询",
        "rows": out,
    })


# ---------------------------------------------------------------- profile
def _cmd_profile(args) -> None:
    ak = _import_akshare()
    code = args.code.strip()
    result = {}
    try:
        basic = _retry_call(ak.fund_individual_basic_info_xq, symbol=code)
        result["basic_info"] = {r["item"]: r["value"] for r in _df_records(basic)}
    except Exception as exc:  # noqa: BLE001
        result["basic_info_error"] = f"{type(exc).__name__}: {exc}"[:200]
    try:
        nav = _retry_call(ak.fund_open_fund_info_em, symbol=code, indicator="单位净值走势")
        latest = _df_records(nav.tail(1))
        result["latest_nav"] = latest[0] if latest else None
        result["nav_rows_total"] = len(nav)
    except Exception as exc:  # noqa: BLE001
        result["nav_error"] = f"{type(exc).__name__}: {exc}"[:200]
    _emit({
        "source": "天天基金/雪球(经 akshare)",
        "code": code,
        "result": result,
        "note": "历任经理名册/托管费/销售服务费/持有人结构在兜底轨为 源不可用",
    })


# ---------------------------------------------------------------- nav
def _cmd_nav(args) -> None:
    ak = _import_akshare()
    df = _retry_call(ak.fund_open_fund_info_em, symbol=args.code.strip(), indicator=args.indicator)
    _emit({
        "source": "天天基金(经 akshare)",
        "code": args.code, "indicator": args.indicator,
        "rows": _df_records(df.tail(args.rows)),
        "unit_note": "RBSA/区间收益请用累计净值;周频收益自行重采样 [测算]",
    })


# ---------------------------------------------------------------- holdings
def _cmd_holdings(args) -> None:
    ak = _import_akshare()
    df = _retry_call(ak.fund_portfolio_hold_em, symbol=args.code.strip(), date=str(args.year))
    rows = _df_records(df)
    by_quarter: dict[str, list[dict]] = {}
    for r in rows:
        by_quarter.setdefault(str(r.get("季度")), []).append(r)
    _emit({
        "source": "天天基金(经 akshare;基金定期报告转引,二手口径)",
        "code": args.code, "year": args.year, "quarters": by_quarter,
        "unit_note": "占净值比例为前十大口径(季度披露);全持仓为半年度,兜底轨不可得",
    })


def main() -> None:
    parser = argparse.ArgumentParser(description="pick-funds akshare 兜底数据脚本")
    sub = parser.add_subparsers(dest="command", required=True)

    p = sub.add_parser("rank", help="全市场基金筛选")
    p.add_argument("--type", default="混合", help="名称包含的类型词(混合/股票/债券/指数/ETF…);rank 表无类型列,按名称包含匹配")
    p.add_argument("--exclude-words", default="债,指数,联接,QDII,FOF", help="名称含这些词则排除,逗号分隔")
    p.add_argument("--sort", default="近1年", choices=RETURN_COLS, help="排序字段(累计口径)")
    p.add_argument("--min-return", type=float, default=None, help="排序字段的最小阈值(%%)")
    p.add_argument("--order", default="desc", choices=["desc", "asc"])
    p.add_argument("--top", type=int, default=15)
    p.set_defaults(func=_cmd_rank)

    p = sub.add_parser("profile", help="单基金基础档案")
    p.add_argument("--code", required=True)
    p.set_defaults(func=_cmd_profile)

    p = sub.add_parser("nav", help="净值序列")
    p.add_argument("--code", required=True)
    p.add_argument("--indicator", default="累计净值走势", choices=["累计净值走势", "单位净值走势", "累计收益率走势"])
    p.add_argument("--rows", type=int, default=500)
    p.set_defaults(func=_cmd_nav)

    p = sub.add_parser("holdings", help="按季度前十大持仓")
    p.add_argument("--code", required=True)
    p.add_argument("--year", type=int, default=_dt.date.today().year)
    p.set_defaults(func=_cmd_holdings)

    args = parser.parse_args()
    try:
        args.func(args)
    except SystemExit:
        raise
    except Exception as exc:  # noqa: BLE001
        _fail(f"{type(exc).__name__}: {exc}"[:500])


if __name__ == "__main__":
    main()
