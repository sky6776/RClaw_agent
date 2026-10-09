#!/usr/bin/env python3
"""Compute auditable fund and benchmark metrics from a total-return CSV.

The script uses only the Python standard library. All ratios in the JSON output
are decimals: 0.125 means 12.5%.
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import statistics
import sys
from dataclasses import dataclass
from datetime import date, datetime, timedelta
from pathlib import Path
from typing import Iterable, Sequence


MIN_RETURN_OBSERVATIONS = 12
WINDOWS = (("1y", 1), ("3y", 3), ("5y", 5), ("10y", 10))
SKILL_DIR = Path(__file__).resolve().parent.parent


@dataclass(frozen=True)
class Observation:
    day: date
    fund: float
    benchmark: float | None = None


def validation_error(errors: Sequence[str]) -> ValueError:
    unique = list(dict.fromkeys(errors))
    return ValueError(
        f"input preflight found {len(unique)} problem(s):\n"
        + "\n".join(f"- {error}" for error in unique)
    )


def parse_day(value: str) -> date:
    value = value.strip()
    for fmt in ("%Y-%m-%d", "%Y/%m/%d", "%Y%m%d", "%m/%d/%Y"):
        try:
            return datetime.strptime(value, fmt).date()
        except ValueError:
            pass
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).date()
    except ValueError as exc:
        raise ValueError(f"unsupported date: {value!r}") from exc


def parse_level(value: str, field: str, row_number: int) -> float:
    try:
        number = float(value.replace(",", "").strip())
    except ValueError as exc:
        raise ValueError(f"row {row_number}: invalid {field} value {value!r}") from exc
    if not math.isfinite(number) or number <= 0:
        raise ValueError(f"row {row_number}: {field} must be finite and greater than zero")
    return number


def validate_observations(observations: Sequence[Observation]) -> None:
    errors: list[str] = []
    if len(observations) < 2:
        errors.append("at least two observations are required")
    for index, item in enumerate(observations):
        if not isinstance(item, Observation):
            errors.append(f"observation {index}: must be an Observation")
            continue
        if not isinstance(item.day, date):
            errors.append(f"observation {index}: day must be a date")
        if (
            isinstance(item.fund, bool)
            or not isinstance(item.fund, (int, float))
            or not math.isfinite(item.fund)
            or item.fund <= 0
        ):
            errors.append(
                f"observation {index}: fund level must be finite and greater than zero"
            )
        if item.benchmark is not None and (
            isinstance(item.benchmark, bool)
            or not isinstance(item.benchmark, (int, float))
            or not math.isfinite(item.benchmark)
            or item.benchmark <= 0
        ):
            errors.append(
                f"observation {index}: benchmark level must be finite and greater than zero"
            )
        if (
            index
            and isinstance(item.day, date)
            and isinstance(observations[index - 1], Observation)
            and isinstance(observations[index - 1].day, date)
            and item.day <= observations[index - 1].day
        ):
            errors.append(
                f"observation {index}: date must be later than observation {index - 1}"
            )
    if errors:
        raise validation_error(errors)


def read_csv(
    csv_path: Path,
    date_col: str,
    fund_col: str,
    benchmark_col: str | None,
) -> list[Observation]:
    columns = [date_col, fund_col] + ([benchmark_col] if benchmark_col else [])
    if any(not column.strip() for column in columns):
        raise ValueError("CSV column names must not be blank")
    if len(set(columns)) != len(columns):
        raise ValueError("date, fund, and benchmark columns must be distinct")
    observations: list[Observation] = []
    errors: list[str] = []
    seen_dates: set[date] = set()
    with csv_path.open("r", encoding="utf-8-sig", newline="") as handle:
        reader = csv.DictReader(handle)
        if reader.fieldnames is None:
            raise ValueError("CSV has no header")
        duplicate_headers = sorted(
            {name for name in reader.fieldnames if reader.fieldnames.count(name) > 1}
        )
        if duplicate_headers:
            raise ValueError(f"duplicate CSV column(s): {', '.join(duplicate_headers)}")
        required = [date_col, fund_col] + ([benchmark_col] if benchmark_col else [])
        missing = [column for column in required if column not in reader.fieldnames]
        if missing:
            raise ValueError(f"missing CSV column(s): {', '.join(missing)}")

        for row_number, row in enumerate(reader, start=2):
            if None in row:
                errors.append(
                    f"row {row_number}: contains more values than the CSV header"
                )
            raw_day = (row.get(date_col) or "").strip()
            raw_fund = (row.get(fund_col) or "").strip()
            raw_benchmark = (row.get(benchmark_col) or "").strip() if benchmark_col else ""
            if not raw_day and not raw_fund and not raw_benchmark:
                continue
            missing_required = not raw_day or not raw_fund
            if missing_required:
                errors.append(f"row {row_number}: date and fund values are required")
            day = None
            fund = None
            if raw_day:
                try:
                    day = parse_day(raw_day)
                except ValueError as exc:
                    errors.append(f"row {row_number}: {exc}")
                else:
                    if day in seen_dates:
                        errors.append(
                            f"row {row_number}: duplicate date: {day.isoformat()}"
                        )
                    seen_dates.add(day)
            if raw_fund:
                try:
                    fund = parse_level(raw_fund, fund_col, row_number)
                except ValueError as exc:
                    errors.append(str(exc))
            benchmark = None
            if benchmark_col:
                if raw_benchmark:
                    try:
                        benchmark = parse_level(
                            raw_benchmark, benchmark_col, row_number
                        )
                    except ValueError as exc:
                        errors.append(str(exc))
            if missing_required or day is None or fund is None:
                continue
            observations.append(
                Observation(
                    day=day,
                    fund=fund,
                    benchmark=benchmark,
                )
            )

    observations.sort(key=lambda item: item.day)
    if len(observations) < 2:
        errors.append("at least two valid observations are required")
    if errors:
        raise validation_error(errors)
    return observations


def infer_frequency(days: Sequence[date]) -> tuple[int, str, float]:
    intervals = [(later - earlier).days for earlier, later in zip(days, days[1:])]
    median_days = float(statistics.median(intervals))
    if median_days <= 3:
        return 252, "daily", median_days
    if median_days <= 10:
        return 52, "weekly", median_days
    if median_days <= 45:
        return 12, "monthly", median_days
    if median_days <= 120:
        return 4, "quarterly", median_days
    return 1, "annual", median_days


def returns(levels: Sequence[float]) -> list[float]:
    return [current / previous - 1.0 for previous, current in zip(levels, levels[1:])]


def sample_covariance(left: Sequence[float], right: Sequence[float]) -> float:
    if len(left) != len(right) or len(left) < 2:
        raise ValueError("covariance requires aligned samples")
    left_mean = statistics.fmean(left)
    right_mean = statistics.fmean(right)
    return sum((x - left_mean) * (y - right_mean) for x, y in zip(left, right)) / (len(left) - 1)


def correlation(left: Sequence[float], right: Sequence[float]) -> float | None:
    if len(left) < 2:
        return None
    left_sd = statistics.stdev(left)
    right_sd = statistics.stdev(right)
    if left_sd == 0 or right_sd == 0:
        return None
    return sample_covariance(left, right) / (left_sd * right_sd)


def max_drawdown(days: Sequence[date], levels: Sequence[float]) -> dict[str, object]:
    peak_value = levels[0]
    peak_day = days[0]
    worst_value = 0.0
    worst_peak_value = peak_value
    worst_peak_day = peak_day
    trough_day = days[0]

    for current_day, level in zip(days, levels):
        if level > peak_value:
            peak_value = level
            peak_day = current_day
        drawdown = level / peak_value - 1.0
        if drawdown < worst_value:
            worst_value = drawdown
            worst_peak_value = peak_value
            worst_peak_day = peak_day
            trough_day = current_day

    recovery_day = None
    if worst_value < 0:
        for current_day, level in zip(days, levels):
            if current_day > trough_day and level >= worst_peak_value:
                recovery_day = current_day
                break

    return {
        "value": worst_value,
        "peak_date": worst_peak_day.isoformat(),
        "trough_date": trough_day.isoformat(),
        "recovery_date": recovery_day.isoformat() if recovery_day else None,
    }


def annual_cagr(first: float, last: float, years: float) -> float | None:
    if years < 0.75:
        return None
    return (last / first) ** (1.0 / years) - 1.0


def series_stats(
    days: Sequence[date],
    levels: Sequence[float],
    periods_per_year: int,
    risk_free_rate: float | None,
) -> dict[str, object]:
    period_returns = returns(levels)
    years = (days[-1] - days[0]).days / 365.2425
    cumulative = levels[-1] / levels[0] - 1.0
    cagr = annual_cagr(levels[0], levels[-1], years)
    drawdown = max_drawdown(days, levels)

    volatility = None
    sharpe = None
    sortino = None
    if len(period_returns) >= MIN_RETURN_OBSERVATIONS:
        volatility = statistics.stdev(period_returns) * math.sqrt(periods_per_year)
        if risk_free_rate is not None:
            risk_free_period = (1.0 + risk_free_rate) ** (1.0 / periods_per_year) - 1.0
            excess = [value - risk_free_period for value in period_returns]
            excess_sd = statistics.stdev(excess)
            if excess_sd > 0:
                sharpe = statistics.fmean(excess) / excess_sd * math.sqrt(periods_per_year)
            downside = [min(0.0, value) for value in excess]
            downside_deviation = math.sqrt(statistics.fmean(value * value for value in downside))
            if downside_deviation > 0:
                sortino = statistics.fmean(excess) / downside_deviation * math.sqrt(periods_per_year)

    calmar = None
    if cagr is not None and float(drawdown["value"]) < 0:
        calmar = cagr / abs(float(drawdown["value"]))

    return {
        "observations": len(levels),
        "return_observations": len(period_returns),
        "start_date": days[0].isoformat(),
        "end_date": days[-1].isoformat(),
        "years": years,
        "cumulative_return": cumulative,
        "cagr": cagr,
        "annualized_volatility": volatility,
        "max_drawdown": drawdown,
        "sharpe_ratio": sharpe,
        "sortino_ratio": sortino,
        "calmar_ratio": calmar,
    }


def relative_stats(
    observations: Sequence[Observation],
    risk_free_rate: float | None,
) -> dict[str, object] | None:
    aligned = [item for item in observations if item.benchmark is not None]
    if len(aligned) < 2:
        return None
    days = [item.day for item in aligned]
    periods_per_year, _, _ = infer_frequency([item.day for item in observations])
    fund_levels = [item.fund for item in aligned]
    benchmark_levels = [float(item.benchmark) for item in aligned]
    paired_returns = [
        (
            current.fund / previous.fund - 1.0,
            float(current.benchmark) / float(previous.benchmark) - 1.0,
        )
        for previous, current in zip(observations, observations[1:])
        if previous.benchmark is not None and current.benchmark is not None
    ]
    fund_returns = [pair[0] for pair in paired_returns]
    benchmark_returns = [pair[1] for pair in paired_returns]
    years = (days[-1] - days[0]).days / 365.2425
    active_returns = [fund - benchmark for fund, benchmark in zip(fund_returns, benchmark_returns)]

    fund_cagr = annual_cagr(fund_levels[0], fund_levels[-1], years)
    benchmark_cagr = annual_cagr(benchmark_levels[0], benchmark_levels[-1], years)
    tracking_error = None
    information_ratio = None
    beta = None
    alpha = None
    corr = None
    hit_rate = None
    upside_capture = None
    downside_capture = None

    if len(active_returns) >= MIN_RETURN_OBSERVATIONS:
        tracking_error = statistics.stdev(active_returns) * math.sqrt(periods_per_year)
        if tracking_error > 0:
            information_ratio = statistics.fmean(active_returns) * periods_per_year / tracking_error
        benchmark_variance = statistics.variance(benchmark_returns)
        if benchmark_variance > 0:
            beta = sample_covariance(fund_returns, benchmark_returns) / benchmark_variance
            if risk_free_rate is not None:
                risk_free_period = (1.0 + risk_free_rate) ** (1.0 / periods_per_year) - 1.0
                alpha = statistics.fmean(
                    (fund - risk_free_period) - beta * (benchmark - risk_free_period)
                    for fund, benchmark in zip(fund_returns, benchmark_returns)
                ) * periods_per_year
        corr = correlation(fund_returns, benchmark_returns)
        hit_rate = sum(fund > benchmark for fund, benchmark in zip(fund_returns, benchmark_returns)) / len(fund_returns)

        up_pairs = [(fund, benchmark) for fund, benchmark in zip(fund_returns, benchmark_returns) if benchmark > 0]
        down_pairs = [(fund, benchmark) for fund, benchmark in zip(fund_returns, benchmark_returns) if benchmark < 0]
        if up_pairs:
            benchmark_up = statistics.fmean(pair[1] for pair in up_pairs)
            if benchmark_up != 0:
                upside_capture = statistics.fmean(pair[0] for pair in up_pairs) / benchmark_up
        if down_pairs:
            benchmark_down = statistics.fmean(pair[1] for pair in down_pairs)
            if benchmark_down != 0:
                downside_capture = statistics.fmean(pair[0] for pair in down_pairs) / benchmark_down

    return {
        "aligned_observations": len(aligned),
        "aligned_return_observations": len(paired_returns),
        "start_date": days[0].isoformat(),
        "end_date": days[-1].isoformat(),
        "fund_cumulative_return": fund_levels[-1] / fund_levels[0] - 1.0,
        "benchmark_cumulative_return": benchmark_levels[-1] / benchmark_levels[0] - 1.0,
        "fund_cagr": fund_cagr,
        "benchmark_cagr": benchmark_cagr,
        "excess_cagr": fund_cagr - benchmark_cagr if fund_cagr is not None and benchmark_cagr is not None else None,
        "tracking_error": tracking_error,
        "information_ratio": information_ratio,
        "beta": beta,
        "alpha_annualized": alpha,
        "correlation": corr,
        "period_hit_rate": hit_rate,
        "upside_capture": upside_capture,
        "downside_capture": downside_capture,
    }


def select_window(observations: Sequence[Observation], years: int) -> list[Observation]:
    cutoff = observations[-1].day - timedelta(days=round(365.2425 * years))
    return [item for item in observations if item.day >= cutoff]


def analyze(observations: Sequence[Observation], risk_free_rate: float | None) -> dict[str, object]:
    validate_observations(observations)
    if risk_free_rate is not None and (
        isinstance(risk_free_rate, bool)
        or not isinstance(risk_free_rate, (int, float))
        or not math.isfinite(risk_free_rate)
        or risk_free_rate <= -1.0
    ):
        raise ValueError("risk-free rate must be finite and greater than -1")
    periods_per_year, frequency, median_interval = infer_frequency([item.day for item in observations])
    periods: dict[str, object] = {}
    warnings = [
        "Input levels must be total-return/adjusted NAV values for total-return conclusions.",
        "All ratios are decimals; 0.125 means 12.5%.",
    ]
    if risk_free_rate is None:
        warnings.append("No risk-free rate supplied; Sharpe, Sortino, and alpha are null.")

    def calculate_period(name: str, selected: Sequence[Observation]) -> None:
        days = [item.day for item in selected]
        fund_levels = [item.fund for item in selected]
        fund = series_stats(days, fund_levels, periods_per_year, risk_free_rate)
        benchmark = None
        aligned = [item for item in selected if item.benchmark is not None]
        if len(aligned) == len(selected):
            benchmark_periods_per_year, _, _ = infer_frequency([item.day for item in aligned])
            benchmark = series_stats(
                [item.day for item in aligned],
                [float(item.benchmark) for item in aligned],
                benchmark_periods_per_year,
                risk_free_rate,
            )
        periods[name] = {
            "fund": fund,
            "benchmark": benchmark,
            "relative": relative_stats(selected, risk_free_rate),
        }

    calculate_period("all", observations)
    available_days = (observations[-1].day - observations[0].day).days
    for name, years in WINDOWS:
        if available_days < 365.2425 * years * 0.9:
            warnings.append(f"Insufficient history for a representative {name} window; omitted.")
            continue
        selected = select_window(observations, years)
        if len(selected) >= 2:
            calculate_period(name, selected)

    if len(observations) - 1 < MIN_RETURN_OBSERVATIONS:
        warnings.append(
            f"Fewer than {MIN_RETURN_OBSERVATIONS} return observations; volatility and risk-adjusted metrics are null."
        )
    elif len(observations) - 1 < periods_per_year:
        warnings.append(
            "The series contains less than one inferred year of returns; annualized risk metrics are statistically fragile."
        )
    benchmark_count = sum(item.benchmark is not None for item in observations)
    if 0 < benchmark_count < len(observations):
        warnings.append(
            "Benchmark contains missing values; benchmark series statistics are omitted and periodic relative metrics use only consecutive aligned returns."
        )

    return {
        "metadata": {
            "ratio_unit": "decimal",
            "frequency_inferred": frequency,
            "periods_per_year": periods_per_year,
            "median_interval_days": median_interval,
            "risk_free_rate_annual": risk_free_rate,
            "fund_observations": len(observations),
            "benchmark_observations": benchmark_count,
        },
        "periods": periods,
        "warnings": warnings,
    }


def run_self_test() -> None:
    start = date(2020, 1, 1)
    growth = [
        Observation(
            start + timedelta(days=30 * index),
            100.0 * (1.01**index),
            100.0 * (1.008**index),
        )
        for index in range(25)
    ]
    result = analyze(growth, 0.02)
    assert result["metadata"]["periods_per_year"] == 12
    assert result["periods"]["all"]["fund"]["cumulative_return"] > 0.26
    assert result["periods"]["all"]["fund"]["max_drawdown"]["recovery_date"] is None
    assert result["periods"]["all"]["relative"]["excess_cagr"] > 0
    no_risk_free = analyze(growth, None)
    assert no_risk_free["periods"]["all"]["relative"]["alpha_annualized"] is None

    drawdown_observations = [
        Observation(start, 100.0),
        Observation(start + timedelta(days=30), 120.0),
        Observation(start + timedelta(days=60), 90.0),
        Observation(start + timedelta(days=90), 130.0),
    ]
    drawdown = max_drawdown(
        [item.day for item in drawdown_observations],
        [item.fund for item in drawdown_observations],
    )
    assert math.isclose(float(drawdown["value"]), -0.25)
    assert drawdown["peak_date"] == "2020-01-31"
    assert drawdown["trough_date"] == "2020-03-01"
    assert drawdown["recovery_date"] == "2020-03-31"

    missing_benchmark = [
        Observation(start, 100.0, 100.0),
        Observation(start + timedelta(days=30), 101.0, None),
        Observation(start + timedelta(days=60), 102.0, 102.0),
    ]
    missing_result = analyze(missing_benchmark, None)
    assert missing_result["periods"]["all"]["benchmark"] is None
    assert missing_result["periods"]["all"]["relative"]["aligned_return_observations"] == 0
    try:
        analyze(growth, math.nan)
    except ValueError:
        pass
    else:
        raise AssertionError("non-finite risk-free rate must fail")
    try:
        analyze(list(reversed(growth)), None)
    except ValueError:
        pass
    else:
        raise AssertionError("non-increasing observation dates must fail")
    batch_invalid = [
        Observation(start, -1.0, -2.0),
        Observation(start, math.nan, 0.0),
    ]
    try:
        validate_observations(batch_invalid)
    except ValueError as exc:
        message = str(exc)
        assert "observation 0: fund level" in message
        assert "observation 0: benchmark level" in message
        assert "observation 1: fund level" in message
        assert "observation 1: benchmark level" in message
        assert "observation 1: date" in message
    else:
        raise AssertionError("all invalid observations must be reported together")
    print("self-test passed")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Compute fund and benchmark metrics from total-return/adjusted NAV levels."
    )
    parser.add_argument("csv_path", nargs="?", type=Path, help="UTF-8 CSV containing dates and NAV levels")
    parser.add_argument("--date-col", default="date", help="date column name (default: date)")
    parser.add_argument("--fund-col", default="fund", help="fund level column name (default: fund)")
    parser.add_argument("--benchmark-col", help="optional benchmark level column name")
    parser.add_argument(
        "--risk-free-rate",
        type=float,
        help="annual risk-free rate as a decimal; omit rather than assuming zero",
    )
    parser.add_argument("--output", type=Path, help="write JSON to this path instead of stdout")
    parser.add_argument("--self-test", action="store_true", help="run deterministic built-in tests")
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.self_test:
        run_self_test()
        return 0
    if args.csv_path is None:
        raise ValueError("csv_path is required unless --self-test is used")
    if args.output and args.output.expanduser().resolve() == args.csv_path.expanduser().resolve():
        raise ValueError("--output must be different from the input CSV")
    if args.output:
        try:
            args.output.expanduser().resolve().relative_to(SKILL_DIR)
        except ValueError:
            pass
        else:
            raise ValueError("--output must be outside the skill directory")

    observations = read_csv(args.csv_path, args.date_col, args.fund_col, args.benchmark_col)
    result = analyze(observations, args.risk_free_rate)
    payload = json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(payload + "\n", encoding="utf-8")
    else:
        print(payload)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
