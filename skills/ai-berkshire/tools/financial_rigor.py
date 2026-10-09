#!/usr/bin/env python3
"""Financial Rigor Toolkit for AI Berkshire.

Command-line tool for verifying financial data accuracy during investment research.
Called by AI Berkshire skills at critical validation checkpoints.

Zero external dependencies — uses only Python stdlib (decimal, json, math, argparse).
Requires Python >= 3.7.

Usage (called automatically by Skills, no manual execution needed):
    python3 tools/financial_rigor.py verify-market-cap --price 510 --shares 9.11e9 --reported 4.65e12 --currency HKD
    python3 tools/financial_rigor.py verify-valuation --price 510 --eps 23.5 --bvps 120 --fcf-per-share 18 --dividend 2.4
    python3 tools/financial_rigor.py cross-validate --field revenue --values-file tmp/revenue.json --unit 亿
    python3 tools/financial_rigor.py benford --values-file tmp/benford.json
    python3 tools/financial_rigor.py calc --expr '510 * 9.11e9'
"""

import argparse
import json
import math
import re
import sys
from decimal import Decimal, Context, ROUND_HALF_EVEN, InvalidOperation
from pathlib import Path


def configure_stdio():
    """Keep Chinese diagnostics and status symbols portable on Windows shells."""
    for stream in (sys.stdout, sys.stderr):
        reconfigure = getattr(stream, "reconfigure", None)
        if reconfigure:
            reconfigure(encoding="utf-8", errors="replace")


# ---------------------------------------------------------------------------
# Exact Decimal Engine (no floating-point drift)
# ---------------------------------------------------------------------------

_CTX = Context(prec=28, rounding=ROUND_HALF_EVEN)


def exact(value) -> Decimal:
    """Convert any numeric to exact Decimal, avoiding float traps."""
    if isinstance(value, Decimal):
        return value
    if isinstance(value, float):
        return Decimal(str(value))
    return Decimal(str(value))


def fmt_number(d: Decimal, unit: str = "") -> str:
    """Format large numbers in human-readable form (亿/万亿/B/T)."""
    v = float(d)
    abs_v = abs(v)
    if unit in ("亿", "亿元", "亿港元", "亿美元"):
        if abs_v >= 10000:
            return f"{v/10000:.2f}万亿{unit[1:] if len(unit) > 1 else ''}"
        return f"{v:.2f}{unit}"
    if abs_v >= 1e12:
        return f"{v/1e12:.2f}T"
    if abs_v >= 1e9:
        return f"{v/1e9:.2f}B"
    if abs_v >= 1e6:
        return f"{v/1e6:.2f}M"
    return f"{v:,.2f}"


# ---------------------------------------------------------------------------
# 1. Market Cap Verification (股价×总股本 vs 报告市值)
# ---------------------------------------------------------------------------

def verify_market_cap(price, shares, reported_cap, currency=""):
    """Verify market cap = price × shares, compare with reported value."""
    p = exact(price)
    s = exact(shares)
    r = exact(reported_cap)

    calculated = _CTX.multiply(p, s)
    deviation = (
        _CTX.multiply(_CTX.divide(abs(calculated - r), abs(r)), Decimal("100"))
        if r != 0
        else (Decimal("0") if calculated == 0 else Decimal("Infinity"))
    )

    print("=" * 60)
    print("市值验算 (Market Cap Verification)")
    print("=" * 60)
    print(f"  股价 (Price):       {p} {currency}")
    print(f"  总股本 (Shares):    {fmt_number(s)}")
    print(f"  计算市值:           {fmt_number(calculated)} {currency}")
    print(f"  报告市值:           {fmt_number(r)} {currency}")
    print(f"  偏差:               {deviation:.2f}%")
    print()

    if deviation > 5:
        print(f"  ❌ 警告: 偏差 {deviation:.1f}% > 5%, 请检查:")
        print(f"     - 股本是否为最新（回购/增发）?")
        print(f"     - 单位是否一致（港币 vs 人民币 vs 美元）?")
        print(f"     - 股价是否为最新?")
        return False
    elif deviation > 1:
        print(f"  ⚠️  偏差 {deviation:.1f}% 在可接受范围, 可能因股价波动/股本变化")
        return True
    else:
        print(f"  ✅ 验证通过, 偏差仅 {deviation:.2f}%")
        return True


# ---------------------------------------------------------------------------
# 2. Valuation Metrics Verification (估值指标验算)
# ---------------------------------------------------------------------------

def verify_valuation(price, eps=None, bvps=None, fcf_per_share=None,
                     dividend=None, revenue_per_share=None):
    """Calculate and verify key valuation ratios from raw inputs."""
    p = exact(price)

    print("=" * 60)
    print("估值指标验算 (Valuation Verification)")
    print("=" * 60)
    print(f"  当前股价: {p}")
    print()

    results = {}

    if eps is not None:
        e = exact(eps)
        if e != 0:
            pe = _CTX.divide(p, e)
            print(f"  PE (TTM):  {p} / {e} = {pe:.2f}x")
            results["PE"] = float(pe)
            # Earnings yield
            ey = _CTX.divide(e, p) * 100
            print(f"  盈利收益率: {ey:.2f}%")
        else:
            print(f"  PE: EPS为0, 无法计算")

    if bvps is not None:
        b = exact(bvps)
        if b != 0:
            pb = _CTX.divide(p, b)
            print(f"  PB:        {p} / {b} = {pb:.2f}x")
            results["PB"] = float(pb)
            if eps is not None and e != 0:
                roe = _CTX.divide(e, b) * 100
                print(f"  ROE:       {e} / {b} = {roe:.2f}%")
                results["ROE"] = float(roe)

    if fcf_per_share is not None:
        f = exact(fcf_per_share)
        if f != 0:
            fcf_yield = _CTX.divide(f, p) * 100
            pfcf = _CTX.divide(p, f)
            print(f"  P/FCF:     {p} / {f} = {pfcf:.2f}x")
            print(f"  FCF Yield: {fcf_yield:.2f}%")
            results["P_FCF"] = float(pfcf)
            results["FCF_Yield"] = float(fcf_yield)

    if dividend is not None:
        d = exact(dividend)
        if p != 0:
            div_yield = _CTX.divide(d, p) * 100
            print(f"  股息率:    {d} / {p} = {div_yield:.2f}%")
            results["Dividend_Yield"] = float(div_yield)

    if revenue_per_share is not None:
        r = exact(revenue_per_share)
        if r != 0:
            ps = _CTX.divide(p, r)
            print(f"  PS:        {p} / {r} = {ps:.2f}x")
            results["PS"] = float(ps)

    print()
    print("  ✅ 以上指标均使用精确十进制计算, 无浮点误差")
    return results


# ---------------------------------------------------------------------------
# 3. Cross-Source Data Validation (多源交叉验证)
# ---------------------------------------------------------------------------

def cross_validate(field_name, source_values: dict, unit="", tolerance_pct=2.0):
    """Compare a data point across multiple sources, flag discrepancies."""
    print("=" * 60)
    print(f"交叉验证: {field_name} (Cross-Validation)")
    print("=" * 60)

    values = {k: exact(v) for k, v in source_values.items()}
    tolerance = exact(tolerance_pct)
    sources = list(values.keys())
    nums = list(values.values())

    # Find median as reference
    sorted_vals = sorted(nums)
    n = len(sorted_vals)
    median = (
        sorted_vals[n // 2]
        if n % 2 == 1
        else _CTX.divide(
            _CTX.add(sorted_vals[n // 2 - 1], sorted_vals[n // 2]),
            Decimal("2"),
        )
    )

    print(f"  数据来源数: {len(sources)}")
    print(f"  参考中位数: {fmt_number(median)} {unit}")
    print()

    all_ok = True
    for src, val in values.items():
        dev = (
            _CTX.multiply(_CTX.divide(abs(val - median), abs(median)), Decimal("100"))
            if median != 0
            else (Decimal("0") if val == 0 else Decimal("Infinity"))
        )
        status = "✅" if dev <= tolerance else "❌"
        if dev > tolerance:
            all_ok = False
        print(f"  {status} {src:20s}: {fmt_number(val)} {unit}  (偏差 {dev:.2f}%)")

    print()
    if all_ok:
        print(f"  ✅ 所有来源偏差 ≤ {tolerance_pct}%, 数据一致")
    else:
        print(f"  ⚠️  存在来源偏差 > {tolerance_pct}%, 请核实差异原因")
        print(f"     建议: 优先采用公司年报/交易所数据")

    # Consensus value
    consensus = median
    print(f"\n  共识值 (加权中位数): {fmt_number(consensus)} {unit}")
    return {"consensus": float(consensus), "all_consistent": all_ok}


# ---------------------------------------------------------------------------
# 4. Benford's Law Quick Check (财务数据造假检测)
# ---------------------------------------------------------------------------

_BENFORD = {d: math.log10(1 + 1/d) for d in range(1, 10)}


def benford_check(values: list):
    """Quick Benford's Law check on a list of financial values."""
    print("=" * 60)
    print("Benford定律检测 (Financial Data Fabrication Check)")
    print("=" * 60)

    # Extract leading digits
    digits = []
    for v in values:
        v = abs(float(v))
        if v > 0:
            sig = 10 ** (math.log10(v) - math.floor(math.log10(v)))
            d = int(sig)
            if 1 <= d <= 9:
                digits.append(d)

    n = len(digits)
    if n < 50:
        print(f"  ⚠️  样本量不足: {n} < 50, Benford分析不可靠")
        return None

    # Observed distribution
    counts = {}
    for d in digits:
        counts[d] = counts.get(d, 0) + 1
    observed = {d: counts.get(d, 0) / n for d in range(1, 10)}

    # MAD (Nigrini's Mean Absolute Deviation)
    mad = sum(abs(observed.get(d, 0) - _BENFORD[d]) for d in range(1, 10)) / 9

    # Chi-square
    chi2 = sum((counts.get(d, 0) - _BENFORD[d] * n) ** 2 / (_BENFORD[d] * n) for d in range(1, 10))

    # Conformity
    if mad < 0.006:
        conformity = "Close (高度符合)"
    elif mad < 0.012:
        conformity = "Acceptable (可接受)"
    elif mad < 0.015:
        conformity = "Marginally Acceptable (边缘)"
    else:
        conformity = "Nonconforming (不符合 ⚠️)"

    print(f"  样本量:    {n}")
    print(f"  MAD:       {mad:.6f}")
    print(f"  Chi-sq:    {chi2:.2f}")
    print(f"  符合度:    {conformity}")
    print()

    # Digit distribution table
    print(f"  {'首位数':>6} {'观测':>8} {'Benford期望':>12} {'偏差':>8}")
    print(f"  {'-'*6} {'-'*8} {'-'*12} {'-'*8}")
    for d in range(1, 10):
        obs = observed.get(d, 0)
        exp = _BENFORD[d]
        dev = obs - exp
        flag = " ⚠️" if abs(dev) > 0.03 else ""
        print(f"  {d:>6d} {obs:>8.3f} {exp:>12.3f} {dev:>+8.3f}{flag}")

    print()
    is_ok = mad < 0.015
    if is_ok:
        print("  ✅ 数据首位数字分布符合Benford定律")
    else:
        print("  ❌ 数据首位数字分布异常, 可能存在人为调整")
        print("     提示: 不符合Benford定律不一定是造假, 但值得进一步调查")

    return {"mad": mad, "chi2": chi2, "conformity": conformity, "is_conforming": is_ok}


# ---------------------------------------------------------------------------
# 5. Exact Calculator (精确计算器)
# ---------------------------------------------------------------------------

_NUMBER_TOKEN = re.compile(
    r"(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+\-]?\d+)?"
)


def _evaluate_decimal_expression(expr: str) -> Decimal:
    """Parse the supported arithmetic grammar without converting through float."""
    position = 0

    def skip_space():
        nonlocal position
        while position < len(expr) and expr[position].isspace():
            position += 1

    def parse_expression():
        nonlocal position
        result = parse_term()
        while True:
            skip_space()
            if position >= len(expr) or expr[position] not in "+-":
                return result
            operator = expr[position]
            position += 1
            operand = parse_term()
            result = (
                _CTX.add(result, operand)
                if operator == "+"
                else _CTX.subtract(result, operand)
            )

    def parse_term():
        nonlocal position
        result = parse_factor()
        while True:
            skip_space()
            if position >= len(expr) or expr[position] not in "*/":
                return result
            operator = expr[position]
            position += 1
            operand = parse_factor()
            result = (
                _CTX.multiply(result, operand)
                if operator == "*"
                else _CTX.divide(result, operand)
            )

    def parse_factor():
        nonlocal position
        skip_space()
        if position >= len(expr):
            raise ValueError("表达式意外结束")
        if expr[position] in "+-":
            operator = expr[position]
            position += 1
            operand = parse_factor()
            return operand if operator == "+" else _CTX.minus(operand)
        if expr[position] == "(":
            position += 1
            result = parse_expression()
            skip_space()
            if position >= len(expr) or expr[position] != ")":
                raise ValueError("缺少右括号")
            position += 1
            return result
        match = _NUMBER_TOKEN.match(expr, position)
        if match is None:
            raise ValueError(f"位置 {position + 1} 需要数字")
        position = match.end()
        return Decimal(match.group(0))

    result = parse_expression()
    skip_space()
    if position != len(expr):
        raise ValueError(f"位置 {position + 1} 存在不支持的字符")
    return result


def exact_calc(expr: str):
    """Evaluate a financial expression with exact decimal arithmetic.

    Supports: +, -, *, /, (), numbers (including scientific notation).
    """
    print("=" * 60)
    print("精确计算 (Exact Calculator)")
    print("=" * 60)

    try:
        d_result = _evaluate_decimal_expression(expr)
        print(f"  表达式: {expr}")
        print(f"  结果:   {fmt_number(d_result)}")
        print(f"  精确值: {d_result}")
        # Preserve the helper's historical return type; all arithmetic and the
        # printed audit value above remain Decimal-based.
        return float(d_result)
    except Exception as e:
        print(f"  ❌ 计算错误: {e}")
        return None


# ---------------------------------------------------------------------------
# 6. Three-Scenario Valuation (三情景估值)
# ---------------------------------------------------------------------------

def three_scenario_valuation(current_price, current_eps, shares_billion,
                             growth_optimistic, growth_neutral, growth_pessimistic,
                             pe_optimistic, pe_neutral, pe_pessimistic,
                             years=3, currency=""):
    """Calculate three-scenario target prices with exact arithmetic."""
    print("=" * 60)
    print("三情景估值模型 (Three-Scenario Valuation)")
    print("=" * 60)

    p = exact(current_price)
    eps = exact(current_eps)
    shares = exact(shares_billion)

    scenarios = [
        ("乐观 (Bull)", growth_optimistic, pe_optimistic),
        ("中性 (Base)", growth_neutral, pe_neutral),
        ("悲观 (Bear)", growth_pessimistic, pe_pessimistic),
    ]

    print(f"  当前股价: {p} {currency}")
    print(f"  当前EPS:  {eps}")
    print(f"  预测期:   {years}年")
    print()
    print(f"  {'情景':12} {'年增速':>8} {'目标PE':>8} {'目标EPS':>10} {'目标股价':>10} {'涨跌幅':>8}")
    print(f"  {'-'*12} {'-'*8} {'-'*8} {'-'*10} {'-'*10} {'-'*8}")

    for name, growth, pe in scenarios:
        g = exact(growth)
        target_pe = exact(pe)
        # Future EPS = current EPS × (1 + growth)^years
        future_eps = eps
        for _ in range(years):
            future_eps = _CTX.multiply(future_eps, _CTX.add(Decimal("1"), g))
        target_price = _CTX.multiply(future_eps, target_pe)
        change = _CTX.multiply(
            _CTX.divide(target_price - p, p),
            Decimal("100"),
        )

        print(f"  {name:12} {float(g)*100:>7.0f}% {float(target_pe):>7.0f}x "
              f"{float(future_eps):>10.2f} {float(target_price):>9.1f} {change:>+7.1f}%")

    print()
    print("  ✅ 所有计算使用精确十进制, 结果可审计复现")


# ---------------------------------------------------------------------------
# CLI Entry Point
# ---------------------------------------------------------------------------

def finite_decimal(value: str) -> Decimal:
    """Parse a finite decimal CLI value without an intermediate float."""
    try:
        number = Decimal(value)
    except (InvalidOperation, ValueError) as exc:
        raise argparse.ArgumentTypeError(f"无效数值: {value}") from exc
    if not number.is_finite() or not math.isfinite(float(number)):
        raise argparse.ArgumentTypeError(f"数值必须有限: {value}")
    return number


def positive_decimal(value: str) -> Decimal:
    """Parse a finite number greater than zero."""
    number = finite_decimal(value)
    if number <= 0:
        raise argparse.ArgumentTypeError(f"数值必须大于 0: {value}")
    return number


def non_negative_decimal(value: str) -> Decimal:
    """Parse a finite number greater than or equal to zero."""
    number = finite_decimal(value)
    if number < 0:
        raise argparse.ArgumentTypeError(f"数值必须大于或等于 0: {value}")
    return number


def positive_int(value: str) -> int:
    """Parse an integer greater than zero."""
    try:
        number = int(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"无效整数: {value}") from exc
    if number <= 0:
        raise argparse.ArgumentTypeError(f"整数必须大于 0: {value}")
    return number


def decimal_growth_rate(value: str) -> Decimal:
    """Parse a decimal growth rate and reject ambiguous percentage points."""
    rate = finite_decimal(value)
    if not -1 <= rate <= 1:
        raise argparse.ArgumentTypeError(
            f"增长率必须使用小数且位于 -1..1；例如 5%=0.05，不能写 {value}"
        )
    return rate


def load_json_input(inline_value, file_path, expected_type, label):
    """Load an inline/file JSON argument and report portable CLI errors."""
    try:
        raw_value = (
            Path(file_path).read_text(encoding="utf-8")
            if file_path
            else inline_value
        )
        value = json.loads(
            raw_value,
            parse_float=Decimal,
            parse_int=Decimal,
            parse_constant=Decimal,
        )
    except (OSError, json.JSONDecodeError) as exc:
        raise argparse.ArgumentTypeError(f"{label} JSON 读取失败: {exc}") from exc
    if not isinstance(value, expected_type):
        expected = "对象" if expected_type is dict else "数组"
        raise argparse.ArgumentTypeError(f"{label} JSON 根节点必须是{expected}")
    return value


def validate_numeric_collection(value, label, minimum_items=1):
    """Reject empty, non-numeric, boolean, NaN, and infinite JSON values."""
    items = value.items() if isinstance(value, dict) else enumerate(value)
    items = list(items)
    if len(items) < minimum_items:
        raise argparse.ArgumentTypeError(
            f"{label} 至少需要 {minimum_items} 个数值"
        )
    for key, number in items:
        if isinstance(value, dict) and (not isinstance(key, str) or not key.strip()):
            raise argparse.ArgumentTypeError(f"{label} 的来源名称必须是非空字符串")
        if isinstance(number, bool) or not isinstance(number, (int, float, Decimal)):
            raise argparse.ArgumentTypeError(f"{label}[{key}] 必须是数值")
        decimal_number = exact(number)
        if (
            not decimal_number.is_finite()
            or not math.isfinite(float(decimal_number))
        ):
            raise argparse.ArgumentTypeError(f"{label}[{key}] 必须是有限数值")


def main():
    configure_stdio()
    parser = argparse.ArgumentParser(
        description="Financial Rigor Toolkit — 金融数据严谨性验证工具",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s verify-market-cap --price 510 --shares 9.11e9 --reported 4.65e12 --currency HKD
  %(prog)s verify-valuation --price 510 --eps 23.5 --bvps 120
  %(prog)s cross-validate --field revenue --values-file tmp/revenue.json --unit 亿
  %(prog)s benford --values-file tmp/benford.json
  %(prog)s calc --expr '510 * 9.11e9'
        """)

    sub = parser.add_subparsers(dest="command")

    # verify-market-cap
    mc = sub.add_parser("verify-market-cap", help="验算市值 = 股价 × 总股本")
    mc.add_argument("--price", type=positive_decimal, required=True)
    mc.add_argument("--shares", type=positive_decimal, required=True, help="总股本")
    mc.add_argument("--reported", type=positive_decimal, required=True, help="报告市值")
    mc.add_argument("--currency", default="", help="币种")

    # verify-valuation
    val = sub.add_parser("verify-valuation", help="验算估值指标")
    val.add_argument("--price", type=positive_decimal, required=True)
    val.add_argument("--eps", type=finite_decimal, default=None)
    val.add_argument("--bvps", type=finite_decimal, default=None, help="每股净资产")
    val.add_argument("--fcf-per-share", type=finite_decimal, default=None)
    val.add_argument("--dividend", type=finite_decimal, default=None, help="每股股息")
    val.add_argument("--revenue-per-share", type=finite_decimal, default=None)

    # cross-validate
    cv = sub.add_parser("cross-validate", help="多源交叉验证")
    cv.add_argument("--field", required=True, help="数据字段名")
    cv_values = cv.add_mutually_exclusive_group(required=True)
    cv_values.add_argument("--values", help="JSON: {来源: 数值}")
    cv_values.add_argument(
        "--values-file",
        help="包含 {来源: 数值} JSON 的 UTF-8 文件；Windows cmd 下优先使用",
    )
    cv.add_argument("--unit", default="")
    cv.add_argument(
        "--tolerance",
        type=non_negative_decimal,
        default=Decimal("2.0"),
        help="容差百分比",
    )

    # benford
    bf = sub.add_parser("benford", help="Benford定律检测")
    bf_values = bf.add_mutually_exclusive_group(required=True)
    bf_values.add_argument("--values", help="JSON数组")
    bf_values.add_argument(
        "--values-file",
        help="包含 JSON 数组的 UTF-8 文件；Windows cmd 下优先使用",
    )

    # calc
    ca = sub.add_parser("calc", help="精确计算")
    ca.add_argument("--expr", required=True, help="算术表达式")

    # three-scenario
    ts = sub.add_parser("three-scenario", help="三情景估值")
    ts.add_argument("--price", type=positive_decimal, required=True)
    ts.add_argument("--eps", type=finite_decimal, required=True)
    ts.add_argument("--shares", type=positive_decimal, required=True, help="总股本(亿)")
    ts.add_argument("--growth", nargs=3, type=decimal_growth_rate, required=True,
                    help="三情景年增速小数 (乐观 中性 悲观)，如 15%%=0.15、5%%=0.05")
    ts.add_argument("--pe", nargs=3, type=positive_decimal, required=True,
                    help="三情景目标PE, 如 25 20 15")
    ts.add_argument("--years", type=positive_int, default=3)
    ts.add_argument("--currency", default="")

    args = parser.parse_args()

    if args.command == "verify-market-cap":
        verify_market_cap(args.price, args.shares, args.reported, args.currency)
    elif args.command == "verify-valuation":
        verify_valuation(args.price, args.eps, args.bvps, args.fcf_per_share,
                        args.dividend, args.revenue_per_share)
    elif args.command == "cross-validate":
        try:
            values = load_json_input(
                args.values, args.values_file, dict, "cross-validate"
            )
            validate_numeric_collection(values, "cross-validate", minimum_items=2)
        except argparse.ArgumentTypeError as exc:
            parser.error(str(exc))
        cross_validate(args.field, values, args.unit, args.tolerance)
    elif args.command == "benford":
        try:
            values = load_json_input(args.values, args.values_file, list, "benford")
            validate_numeric_collection(values, "benford")
        except argparse.ArgumentTypeError as exc:
            parser.error(str(exc))
        benford_check(values)
    elif args.command == "calc":
        exact_calc(args.expr)
    elif args.command == "three-scenario":
        three_scenario_valuation(
            args.price, args.eps, args.shares,
            args.growth[0], args.growth[1], args.growth[2],
            args.pe[0], args.pe[1], args.pe[2],
            args.years, args.currency)
    else:
        parser.print_help()


if __name__ == "__main__":
    main()
