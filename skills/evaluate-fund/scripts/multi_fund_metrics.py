#!/usr/bin/env python3
"""Compare multiple fund total-return series on one common date range.

The input CSV must contain one date column and one level column per fund. All
ratios in the JSON output are decimals: 0.125 means 12.5%.
"""

from __future__ import annotations

import argparse
import csv
import itertools
import json
import sys
import tempfile
from datetime import date, timedelta
from pathlib import Path
from typing import Iterable

import fund_metrics


SKILL_DIR = Path(__file__).resolve().parent.parent


def read_multi_csv(
    csv_path: Path,
    date_col: str,
    fund_cols: list[str],
    benchmark_col: str | None,
) -> dict[str, list[fund_metrics.Observation]]:
    if len(fund_cols) < 2:
        raise ValueError("at least two --fund-cols values are required")
    if len(set(fund_cols)) != len(fund_cols):
        raise ValueError("--fund-cols contains duplicate column names")
    if any(not name.strip() for name in [date_col, *fund_cols] + ([benchmark_col] if benchmark_col else [])):
        raise ValueError("CSV column names must not be blank")
    if (
        date_col in fund_cols
        or (benchmark_col and benchmark_col in fund_cols)
        or (benchmark_col and benchmark_col == date_col)
    ):
        raise ValueError("date, fund, and benchmark columns must be distinct")

    series = {name: [] for name in fund_cols}
    seen_dates: set[date] = set()
    parsed_dates: set[date] = set()
    errors: list[str] = []
    with csv_path.open("r", encoding="utf-8-sig", newline="") as handle:
        reader = csv.DictReader(handle)
        if reader.fieldnames is None:
            raise ValueError("CSV has no header")
        duplicate_headers = sorted(
            {name for name in reader.fieldnames if reader.fieldnames.count(name) > 1}
        )
        if duplicate_headers:
            raise ValueError(f"duplicate CSV column(s): {', '.join(duplicate_headers)}")
        required = [date_col, *fund_cols] + ([benchmark_col] if benchmark_col else [])
        missing = [column for column in required if column not in reader.fieldnames]
        if missing:
            raise ValueError(f"missing CSV column(s): {', '.join(missing)}")

        for row_number, row in enumerate(reader, start=2):
            if None in row:
                errors.append(
                    f"row {row_number}: contains more values than the CSV header"
                )
            raw_day = (row.get(date_col) or "").strip()
            raw_funds = [(row.get(name) or "").strip() for name in fund_cols]
            raw_benchmark = (row.get(benchmark_col) or "").strip() if benchmark_col else ""
            if not raw_day and not any(raw_funds) and not raw_benchmark:
                continue
            missing_required = not raw_day or any(not value for value in raw_funds)
            if missing_required:
                errors.append(
                    f"row {row_number}: date and every fund value are required for a common comparison"
                )
            day = None
            if raw_day:
                try:
                    day = fund_metrics.parse_day(raw_day)
                except ValueError as exc:
                    errors.append(f"row {row_number}: {exc}")
            duplicate_day = day is not None and day in parsed_dates
            if duplicate_day:
                errors.append(f"row {row_number}: duplicate date: {day.isoformat()}")
            if day is not None:
                parsed_dates.add(day)

            benchmark = None
            if benchmark_col:
                if raw_benchmark:
                    try:
                        benchmark = fund_metrics.parse_level(
                            raw_benchmark, benchmark_col, row_number
                        )
                    except ValueError as exc:
                        errors.append(str(exc))
            parsed_funds: list[tuple[str, float]] = []
            for name, raw_value in zip(fund_cols, raw_funds):
                if not raw_value:
                    continue
                try:
                    parsed_funds.append(
                        (
                            name,
                            fund_metrics.parse_level(raw_value, name, row_number),
                        )
                    )
                except ValueError as exc:
                    errors.append(str(exc))
            if (
                missing_required
                or day is None
                or len(parsed_funds) != len(fund_cols)
                or duplicate_day
            ):
                continue
            seen_dates.add(day)
            for name, value in parsed_funds:
                series[name].append(
                    fund_metrics.Observation(
                        day=day,
                        fund=value,
                        benchmark=benchmark,
                    )
                )

    if len(seen_dates) < 2:
        errors.append("at least two valid common observations are required")
    if errors:
        raise fund_metrics.validation_error(errors)
    for observations in series.values():
        observations.sort(key=lambda item: item.day)
    return series


def _metric_row(name: str, period: dict[str, object]) -> dict[str, object]:
    fund = period["fund"]
    relative = period["relative"]
    assert isinstance(fund, dict)
    assert relative is None or isinstance(relative, dict)
    return {
        "fund": name,
        "start_date": fund["start_date"],
        "end_date": fund["end_date"],
        "cumulative_return": fund["cumulative_return"],
        "cagr": fund["cagr"],
        "annualized_volatility": fund["annualized_volatility"],
        "max_drawdown": fund["max_drawdown"],
        "sharpe_ratio": fund["sharpe_ratio"],
        "sortino_ratio": fund["sortino_ratio"],
        "calmar_ratio": fund["calmar_ratio"],
        "excess_cagr": relative.get("excess_cagr") if relative else None,
        "tracking_error": relative.get("tracking_error") if relative else None,
        "information_ratio": relative.get("information_ratio") if relative else None,
        "beta": relative.get("beta") if relative else None,
    }


def analyze_multi(
    series: dict[str, list[fund_metrics.Observation]],
    risk_free_rate: float | None,
) -> dict[str, object]:
    if len(series) < 2:
        raise ValueError("at least two fund series are required")
    first_series = next(iter(series.values()))
    common_dates = [item.day for item in first_series]
    if len(common_dates) < 2:
        raise ValueError("at least two common observations are required")
    for name, observations in series.items():
        if [item.day for item in observations] != common_dates:
            raise ValueError(f"fund series is not aligned to the common date grid: {name}")

    analyses = {
        name: fund_metrics.analyze(observations, risk_free_rate)
        for name, observations in series.items()
    }
    common_periods = set.intersection(
        *(set(result["periods"].keys()) for result in analyses.values())
    )
    ordered_periods = [name for name, _ in fund_metrics.WINDOWS if name in common_periods]
    ordered_periods = ["all", *ordered_periods]
    comparison = {
        period_name: [
            _metric_row(fund_name, analyses[fund_name]["periods"][period_name])
            for fund_name in series
        ]
        for period_name in ordered_periods
    }

    correlations: dict[str, list[dict[str, object]]] = {}
    for period_name in ordered_periods:
        years = next((years for name, years in fund_metrics.WINDOWS if name == period_name), None)
        selected = {
            name: (
                observations
                if years is None
                else fund_metrics.select_window(observations, years)
            )
            for name, observations in series.items()
        }
        rows: list[dict[str, object]] = []
        for left, right in itertools.combinations(series, 2):
            left_returns = fund_metrics.returns([item.fund for item in selected[left]])
            right_returns = fund_metrics.returns([item.fund for item in selected[right]])
            rows.append(
                {
                    "fund_a": left,
                    "fund_b": right,
                    "return_observations": len(left_returns),
                    "correlation": fund_metrics.correlation(left_returns, right_returns),
                }
            )
        correlations[period_name] = rows
    return {
        "metadata": {
            "ratio_unit": "decimal",
            "fund_columns": list(series),
            "fund_count": len(series),
            "common_observations": len(first_series),
            "common_start_date": first_series[0].day.isoformat(),
            "common_end_date": first_series[-1].day.isoformat(),
            "risk_free_rate_annual": risk_free_rate,
            "comparison_policy": "common dates; no cross-category composite ranking",
        },
        "comparison": comparison,
        "pairwise_correlations": correlations,
        "funds": analyses,
        "warnings": [
            "Every fund is calculated from the same rows and date range.",
            "Compare composite scores only when fund type, objective, currency, benchmark, and fee basis are comparable.",
            "Input levels must be total-return or adjusted NAV values for total-return conclusions.",
        ],
    }


def run_self_test() -> None:
    start = date(2020, 1, 1)
    series = {
        "steady": [
            fund_metrics.Observation(
                start + timedelta(days=30 * index),
                100.0 * (1.01**index),
                100.0 * (1.008**index),
            )
            for index in range(25)
        ],
        "faster": [
            fund_metrics.Observation(
                start + timedelta(days=30 * index),
                100.0 * (1.012**index),
                100.0 * (1.008**index),
            )
            for index in range(25)
        ],
    }
    result = analyze_multi(series, 0.02)
    assert result["metadata"]["fund_count"] == 2
    assert len(result["comparison"]["all"]) == 2
    assert result["pairwise_correlations"]["all"][0]["correlation"] is not None
    assert (
        result["funds"]["faster"]["periods"]["all"]["fund"]["cumulative_return"]
        > result["funds"]["steady"]["periods"]["all"]["fund"]["cumulative_return"]
    )
    misaligned = {**series, "faster": series["faster"][1:]}
    try:
        analyze_multi(misaligned, 0.02)
    except ValueError:
        pass
    else:
        raise AssertionError("misaligned fund series must fail")
    with tempfile.TemporaryDirectory() as temp_dir:
        invalid_csv = Path(temp_dir) / "invalid.csv"
        invalid_csv.write_text(
            "date,fund_a,fund_b,benchmark\n"
            "not-a-date,bad,100,100\n"
            "2026-07-31,100,0,bad\n"
            "2026-08-01,,101,101\n",
            encoding="utf-8",
        )
        try:
            read_multi_csv(
                invalid_csv,
                "date",
                ["fund_a", "fund_b"],
                "benchmark",
            )
        except ValueError as exc:
            message = str(exc)
            assert "row 2" in message
            assert "row 3" in message
            assert "row 4" in message
        else:
            raise AssertionError("all invalid common-period rows must be reported together")
    print("self-test passed")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Compute common-period metrics for multiple total-return fund series."
    )
    parser.add_argument("csv_path", nargs="?", type=Path, help="UTF-8 CSV containing dates and NAV levels")
    parser.add_argument("--date-col", default="date", help="date column name (default: date)")
    parser.add_argument("--fund-cols", nargs="+", help="two or more fund level column names")
    parser.add_argument("--benchmark-col", help="optional common benchmark level column")
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
    if args.csv_path is None or not args.fund_cols:
        raise ValueError("csv_path and --fund-cols are required unless --self-test is used")
    if args.output and args.output.expanduser().resolve() == args.csv_path.expanduser().resolve():
        raise ValueError("--output must be different from the input CSV")
    if args.output:
        try:
            args.output.expanduser().resolve().relative_to(SKILL_DIR)
        except ValueError:
            pass
        else:
            raise ValueError("--output must be outside the skill directory")
    series = read_multi_csv(args.csv_path, args.date_col, args.fund_cols, args.benchmark_col)
    result = analyze_multi(series, args.risk_free_rate)
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
