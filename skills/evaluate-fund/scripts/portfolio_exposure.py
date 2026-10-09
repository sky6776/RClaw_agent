#!/usr/bin/env python3
"""Aggregate fund holdings into portfolio exposures and overlap diagnostics.

Weights and limits are decimals: 0.125 means 12.5%. The script deliberately
separates mutually exclusive exposure dimensions from non-exclusive risk tags.
"""

from __future__ import annotations

import argparse
import json
import math
import sys
from collections import defaultdict
from datetime import date, datetime
from pathlib import Path
from typing import Any, Iterable


EXCLUSIVE_DIMENSIONS = (
    "industry",
    "sector",
    "market_board",
    "region",
    "currency",
    "style_size",
    "style_orientation",
    "duration_bucket",
    "credit_quality",
)
FUND_DIMENSIONS = {
    "fund_category": "category",
    "manager": "manager",
    "fund_company": "fund_company",
    "index_or_strategy": "index_or_strategy",
}
UNKNOWN = "未披露/其他"
EPSILON = 1e-9
SKILL_DIR = Path(__file__).resolve().parent.parent


def parse_day(value: str) -> date:
    for fmt in ("%Y-%m-%d", "%Y/%m/%d", "%Y%m%d"):
        try:
            return datetime.strptime(value.strip(), fmt).date()
        except ValueError:
            pass
    raise ValueError(f"unsupported date: {value!r}")


def ratio(value: Any, field: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{field} must be a number between 0 and 1")
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field} must be a number between 0 and 1") from exc
    if not math.isfinite(number) or number < 0 or number > 1 + EPSILON:
        raise ValueError(f"{field} must be between 0 and 1")
    return min(number, 1.0)


def positive_number(value: Any, field: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{field} must be a positive number")
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field} must be a positive number") from exc
    if not math.isfinite(number) or number <= 0:
        raise ValueError(f"{field} must be a positive number")
    return number


def nonnegative_number(value: Any, field: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{field} must be a non-negative number")
    try:
        number = float(value)
    except (TypeError, ValueError) as exc:
        raise ValueError(f"{field} must be a non-negative number") from exc
    if not math.isfinite(number) or number < 0:
        raise ValueError(f"{field} must be a non-negative number")
    return number


def validate_mapping(values: Any, field: str) -> dict[str, float]:
    if not isinstance(values, dict) or not values:
        raise ValueError(f"{field} must be a non-empty object")
    cleaned: dict[str, float] = {}
    for key, value in values.items():
        if not isinstance(key, str) or not key.strip():
            raise ValueError(f"{field} categories must be non-empty strings")
        name = key.strip()
        if name in cleaned:
            raise ValueError(f"{field} contains duplicate category after trimming: {name}")
        cleaned[name] = ratio(value, f"{field}.{name}")
    if sum(cleaned.values()) > 1 + EPSILON:
        raise ValueError(f"{field} weights exceed 1")
    return cleaned


def validate_nonexclusive_mapping(values: Any, field: str) -> dict[str, float]:
    if not isinstance(values, dict) or not values:
        raise ValueError(f"{field} must be a non-empty object")
    cleaned: dict[str, float] = {}
    for key, value in values.items():
        if not isinstance(key, str) or not key.strip():
            raise ValueError(f"{field} categories must be non-empty strings")
        name = key.strip()
        if name in cleaned:
            raise ValueError(f"{field} contains duplicate category after trimming: {name}")
        cleaned[name] = ratio(value, f"{field}.{name}")
    return cleaned


def resolve_weights(payload: dict[str, Any]) -> tuple[list[tuple[dict[str, Any], float]], float, float, str]:
    funds = payload.get("funds")
    if not isinstance(funds, list) or not funds:
        raise ValueError("funds must be a non-empty array")
    if any(not isinstance(fund, dict) for fund in funds):
        raise ValueError("every funds item must be an object")

    has_weights = ["weight" in fund for fund in funds]
    has_values = ["market_value" in fund for fund in funds]
    if all(has_weights) and not any(has_values):
        if "cash_market_value" in payload:
            raise ValueError("cash_market_value cannot be used in weight mode")
        resolved = []
        for index, fund in enumerate(funds):
            weight = ratio(fund["weight"], f"funds[{index}].weight")
            if weight <= 0:
                raise ValueError(f"funds[{index}].weight must be greater than zero")
            resolved.append((fund, weight))
        cash = ratio(payload.get("cash_weight", 0), "cash_weight")
        total = sum(weight for _, weight in resolved) + cash
        if total > 1 + EPSILON:
            raise ValueError("fund weights plus cash_weight exceed 1")
        return resolved, cash, max(0.0, 1.0 - total), "weight"

    if all(has_values) and not any(has_weights):
        if "cash_weight" in payload:
            raise ValueError("cash_weight cannot be used in market_value mode")
        raw_values = [
            positive_number(fund["market_value"], f"funds[{index}].market_value")
            for index, fund in enumerate(funds)
        ]
        raw_cash = nonnegative_number(payload.get("cash_market_value", 0), "cash_market_value")
        total_value = sum(raw_values) + raw_cash
        if total_value <= 0:
            raise ValueError("total portfolio market value must be positive")
        resolved = [(fund, value / total_value) for fund, value in zip(funds, raw_values)]
        return resolved, raw_cash / total_value, 0.0, "market_value"

    raise ValueError(
        "use either weight for every fund or market_value for every fund; do not mix the two modes"
    )


def add_exposure(target: dict[str, float], category: str, amount: float) -> None:
    target[category] += amount


def derive_dimension(fund: dict[str, Any], dimension: str, fund_index: int) -> dict[str, float]:
    exposures = fund.get("exposures", {})
    if exposures is not None and not isinstance(exposures, dict):
        raise ValueError(f"funds[{fund_index}].exposures must be an object")
    if isinstance(exposures, dict) and dimension in exposures:
        return validate_mapping(exposures[dimension], f"funds[{fund_index}].exposures.{dimension}")

    derived: dict[str, float] = defaultdict(float)
    holdings = fund.get("holdings", [])
    if holdings is None:
        holdings = []
    if not isinstance(holdings, list):
        raise ValueError(f"funds[{fund_index}].holdings must be an array")
    total = 0.0
    for holding_index, holding in enumerate(holdings):
        if not isinstance(holding, dict):
            raise ValueError(f"funds[{fund_index}].holdings[{holding_index}] must be an object")
        holding_weight = ratio(
            holding.get("weight"),
            f"funds[{fund_index}].holdings[{holding_index}].weight",
        )
        total += holding_weight
        raw_category = holding.get(dimension)
        if raw_category is not None and not isinstance(raw_category, str):
            raise ValueError(
                f"funds[{fund_index}].holdings[{holding_index}].{dimension} must be a string"
            )
        category = (raw_category or "未知").strip() or "未知"
        derived[category] += holding_weight
    if total > 1 + EPSILON:
        raise ValueError(f"funds[{fund_index}].holdings weights exceed 1")
    return dict(derived)


def exposure_rows(values: dict[str, float]) -> list[dict[str, Any]]:
    return [
        {"name": name, "weight": weight}
        for name, weight in sorted(values.items(), key=lambda item: (-item[1], item[0]))
        if weight > EPSILON
    ]


def actual_weight(
    dimension: str,
    category: str,
    exposure_maps: dict[str, dict[str, float]],
    fund_weights: dict[str, float],
    securities: dict[str, dict[str, Any]],
) -> float:
    if dimension == "fund":
        return fund_weights.get(category, 0.0)
    if dimension == "underlying_security":
        key = category.upper()
        if key in securities:
            return float(securities[key]["weight"])
        matches = [item for item in securities.values() if item["name"] == category]
        return sum(float(item["weight"]) for item in matches)
    if dimension not in exposure_maps:
        raise ValueError(f"unsupported target/limit dimension: {dimension}")
    return exposure_maps[dimension].get(category, 0.0)


def evaluate_targets(
    targets: Any,
    exposure_maps: dict[str, dict[str, float]],
    fund_weights: dict[str, float],
    securities: dict[str, dict[str, Any]],
) -> list[dict[str, Any]]:
    if targets in (None, {}):
        return []
    if not isinstance(targets, dict):
        raise ValueError("targets must be an object")
    gaps: list[dict[str, Any]] = []
    for dimension, categories in targets.items():
        if not isinstance(dimension, str) or not dimension.strip():
            raise ValueError("targets dimensions must be non-empty strings")
        dimension = dimension.strip()
        if not isinstance(categories, dict):
            raise ValueError(f"targets.{dimension} must be an object")
        for category, band in categories.items():
            if not isinstance(category, str) or not category.strip():
                raise ValueError(f"targets.{dimension} categories must be non-empty strings")
            category = category.strip()
            if not isinstance(band, dict) or not ({"min", "max"} & set(band)):
                raise ValueError(f"targets.{dimension}.{category} needs min and/or max")
            minimum = ratio(band.get("min", 0), f"targets.{dimension}.{category}.min")
            maximum = ratio(band.get("max", 1), f"targets.{dimension}.{category}.max")
            if minimum > maximum:
                raise ValueError(f"targets.{dimension}.{category} min exceeds max")
            actual = actual_weight(dimension, category, exposure_maps, fund_weights, securities)
            if actual < minimum - EPSILON:
                gaps.append(
                    {
                        "dimension": dimension,
                        "category": category,
                        "actual": actual,
                        "minimum": minimum,
                        "maximum": maximum,
                        "status": "below",
                        "adjustment_to_nearest_bound": minimum - actual,
                    }
                )
            elif actual > maximum + EPSILON:
                gaps.append(
                    {
                        "dimension": dimension,
                        "category": category,
                        "actual": actual,
                        "minimum": minimum,
                        "maximum": maximum,
                        "status": "above",
                        "adjustment_to_nearest_bound": maximum - actual,
                    }
                )
    return gaps


def evaluate_limits(
    limits: Any,
    exposure_maps: dict[str, dict[str, float]],
    fund_weights: dict[str, float],
    securities: dict[str, dict[str, Any]],
) -> list[dict[str, Any]]:
    if limits in (None, []):
        return []
    if not isinstance(limits, list):
        raise ValueError("limits must be an array")
    breaches: list[dict[str, Any]] = []
    for index, item in enumerate(limits):
        if not isinstance(item, dict) or "dimension" not in item or "max" not in item:
            raise ValueError(f"limits[{index}] needs dimension and max")
        if not isinstance(item["dimension"], str) or not item["dimension"].strip():
            raise ValueError(f"limits[{index}].dimension must be a non-empty string")
        dimension = item["dimension"].strip()
        maximum = ratio(item["max"], f"limits[{index}].max")
        category = item.get("category")
        if category is not None and (
            not isinstance(category, str) or not category.strip()
        ):
            raise ValueError(f"limits[{index}].category must be a non-empty string")
        if isinstance(category, str):
            category = category.strip()
        if category is None:
            if dimension == "fund":
                candidates = fund_weights.items()
            elif dimension == "underlying_security":
                candidates = ((key, float(value["weight"])) for key, value in securities.items())
            elif dimension in exposure_maps:
                candidates = (
                    (name, weight)
                    for name, weight in exposure_maps[dimension].items()
                    if name not in {UNKNOWN, "未知", "未配置"}
                )
            else:
                raise ValueError(f"unsupported limit dimension: {dimension}")
        else:
            candidates = [
                (
                    str(category),
                    actual_weight(dimension, str(category), exposure_maps, fund_weights, securities),
                )
            ]
        for candidate, actual in candidates:
            if actual > maximum + EPSILON:
                breaches.append(
                    {
                        "dimension": dimension,
                        "category": candidate,
                        "actual": actual,
                        "maximum": maximum,
                        "excess": actual - maximum,
                    }
                )
    return sorted(breaches, key=lambda item: -float(item["excess"]))


def _collect_call(errors: list[str], callback) -> Any:
    try:
        return callback()
    except (TypeError, ValueError) as exc:
        errors.append(str(exc))
        return None


def _preflight_mapping(
    errors: list[str], values: Any, field: str, *, nonexclusive: bool = False
) -> None:
    if not isinstance(values, dict) or not values:
        errors.append(f"{field} must be a non-empty object")
        return
    cleaned: dict[str, float] = {}
    seen_names: set[str] = set()
    for key, value in values.items():
        if not isinstance(key, str) or not key.strip():
            errors.append(f"{field} categories must be non-empty strings")
            continue
        name = key.strip()
        if name in seen_names:
            errors.append(f"{field} contains duplicate category after trimming: {name}")
            continue
        seen_names.add(name)
        parsed = _collect_call(
            errors, lambda value=value, name=name: ratio(value, f"{field}.{name}")
        )
        if parsed is not None:
            cleaned[name] = parsed
    if not nonexclusive and sum(cleaned.values()) > 1 + EPSILON:
        errors.append(f"{field} weights exceed 1")


def collect_portfolio_errors(payload: dict[str, Any]) -> list[str]:
    """Collect independent structural/value errors before aggregating exposures."""
    errors: list[str] = []
    funds = payload.get("funds")
    if not isinstance(funds, list) or not funds:
        errors.append("funds must be a non-empty array")
        funds = []
    for index, fund in enumerate(funds):
        if not isinstance(fund, dict):
            errors.append(f"funds[{index}] must be an object")
    valid_funds = [
        (index, fund) for index, fund in enumerate(funds) if isinstance(fund, dict)
    ]

    raw_analysis_date = payload.get("analysis_date", date.today().isoformat())
    if not isinstance(raw_analysis_date, str):
        errors.append("analysis_date must be a date string")
    else:
        _collect_call(errors, lambda: parse_day(raw_analysis_date))
    base_currency = payload.get("base_currency")
    if base_currency is not None and (
        not isinstance(base_currency, str) or not base_currency.strip()
    ):
        errors.append("base_currency must be a non-empty string when supplied")

    for field in ("cash_weight", "cash_market_value"):
        if field in payload:
            parser = ratio if field == "cash_weight" else nonnegative_number
            _collect_call(errors, lambda field=field, parser=parser: parser(payload[field], field))
    if "cash_weight" in payload and "cash_market_value" in payload:
        errors.append("cash_weight and cash_market_value cannot be used together")

    has_weights = ["weight" in fund for _, fund in valid_funds]
    has_values = ["market_value" in fund for _, fund in valid_funds]
    weight_mode = bool(valid_funds) and all(has_weights) and not any(has_values)
    market_value_mode = bool(valid_funds) and all(has_values) and not any(has_weights)
    if valid_funds and not (weight_mode or market_value_mode):
        errors.append(
            "use either weight for every fund or market_value for every fund; "
            "do not mix the two modes"
        )
    if weight_mode and "cash_market_value" in payload:
        errors.append("cash_market_value cannot be used in weight mode")
    if market_value_mode and "cash_weight" in payload:
        errors.append("cash_weight cannot be used in market_value mode")
    normalized_base_currency = (
        base_currency.strip().upper()
        if isinstance(base_currency, str) and base_currency.strip()
        else None
    )
    if market_value_mode and base_currency is None:
        errors.append("base_currency is required in market_value mode")

    seen_labels: set[str] = set()
    allowed_target_dimensions = {
        "fund",
        "underlying_security",
        "asset_class",
        *EXCLUSIVE_DIMENSIONS,
        *FUND_DIMENSIONS,
        "risk_tag",
    }
    for fund_index, fund in valid_funds:
        if "weight" in fund:
            weight = _collect_call(
                errors,
                lambda fund=fund, fund_index=fund_index: ratio(
                    fund["weight"], f"funds[{fund_index}].weight"
                ),
            )
            if weight is not None and weight <= 0:
                errors.append(f"funds[{fund_index}].weight must be greater than zero")
        if "market_value" in fund:
            _collect_call(
                errors,
                lambda fund=fund, fund_index=fund_index: positive_number(
                    fund["market_value"], f"funds[{fund_index}].market_value"
                ),
            )

        for field in (
            "name",
            "category",
            "manager",
            "fund_company",
            "index_or_strategy",
            "market_value_currency",
        ):
            value = fund.get(field)
            if value is not None and not isinstance(value, str):
                errors.append(f"funds[{fund_index}].{field} must be a string")
        for field in ("code", "share_class", "trading_venue"):
            value = fund.get(field)
            if value is not None and (
                not isinstance(value, str) or not value.strip()
            ):
                errors.append(f"funds[{fund_index}].{field} must be a non-empty string")
        trading_venue = fund.get("trading_venue")
        if (
            isinstance(trading_venue, str)
            and trading_venue.strip()
            and trading_venue.strip() not in {"场内", "场外"}
        ):
            errors.append(f"funds[{fund_index}].trading_venue must be 场内 or 场外")

        name = fund.get("name") if isinstance(fund.get("name"), str) else ""
        code = fund.get("code") if isinstance(fund.get("code"), str) else ""
        display_name = name.strip() or code.strip() or f"基金{fund_index + 1}"
        label = (
            f"{display_name} [{code.strip()}]"
            if name.strip() and code.strip()
            else display_name
        )
        if label in seen_labels:
            errors.append(f"duplicate fund identity: {label}")
        seen_labels.add(label)

        if "risk_score" in fund:
            score = _collect_call(
                errors,
                lambda fund=fund, fund_index=fund_index: nonnegative_number(
                    fund["risk_score"], f"funds[{fund_index}].risk_score"
                ),
            )
            if score is not None and not 1 <= score <= 5:
                errors.append(f"funds[{fund_index}].risk_score must be between 1 and 5")
        if fund.get("asset_allocation") is not None:
            _preflight_mapping(
                errors,
                fund["asset_allocation"],
                f"funds[{fund_index}].asset_allocation",
            )

        exposures = fund.get("exposures", {})
        if exposures is not None and not isinstance(exposures, dict):
            errors.append(f"funds[{fund_index}].exposures must be an object")
        elif isinstance(exposures, dict):
            for dimension, mapping in exposures.items():
                if not isinstance(dimension, str) or not dimension.strip():
                    errors.append(
                        f"funds[{fund_index}].exposures dimensions must be non-empty strings"
                    )
                    continue
                normalized_dimension = dimension.strip()
                if normalized_dimension != dimension:
                    errors.append(
                        f"funds[{fund_index}].exposures dimension has surrounding "
                        f"whitespace: {dimension!r}"
                    )
                if normalized_dimension not in {*EXCLUSIVE_DIMENSIONS, "risk_tag"}:
                    errors.append(
                        f"funds[{fund_index}].exposures has unsupported dimension: "
                        f"{normalized_dimension}"
                    )
                _preflight_mapping(
                    errors,
                    mapping,
                    f"funds[{fund_index}].exposures.{normalized_dimension}",
                    nonexclusive=normalized_dimension == "risk_tag",
                )

        holdings = fund.get("holdings", [])
        if holdings is None:
            holdings = []
        if not isinstance(holdings, list):
            errors.append(f"funds[{fund_index}].holdings must be an array")
            holdings = []
        total = 0.0
        seen_securities: set[str] = set()
        for holding_index, holding in enumerate(holdings):
            prefix = f"funds[{fund_index}].holdings[{holding_index}]"
            if not isinstance(holding, dict):
                errors.append(f"{prefix} must be an object")
                continue
            weight = _collect_call(
                errors, lambda holding=holding, prefix=prefix: ratio(holding.get("weight"), f"{prefix}.weight")
            )
            if weight is not None:
                total += weight
            for field in ("ticker", "name"):
                value = holding.get(field)
                if value is not None and not isinstance(value, str):
                    errors.append(f"{prefix}.{field} must be a string")
            ticker = holding.get("ticker") if isinstance(holding.get("ticker"), str) else ""
            holding_name = holding.get("name") if isinstance(holding.get("name"), str) else ""
            security_id = ticker.strip().upper() or holding_name.strip()
            if not security_id:
                errors.append(f"{prefix} needs name or ticker")
            elif security_id in seen_securities:
                errors.append(
                    f"funds[{fund_index}].holdings contains duplicate security: {security_id}"
                )
            seen_securities.add(security_id)
            for dimension in EXCLUSIVE_DIMENSIONS:
                category = holding.get(dimension)
                if category is not None and not isinstance(category, str):
                    errors.append(f"{prefix}.{dimension} must be a string")
            tags = holding.get("risk_tags", [])
            if tags is None:
                tags = []
            if not isinstance(tags, list):
                errors.append(f"{prefix}.risk_tags must be an array")
            else:
                for tag_index, tag in enumerate(tags):
                    if not isinstance(tag, str) or not tag.strip():
                        errors.append(
                            f"{prefix}.risk_tags[{tag_index}] must be a non-empty string"
                        )
        if total > 1 + EPSILON:
            errors.append(f"funds[{fund_index}].holdings weights exceed 1")
        holdings_as_of = fund.get("holdings_as_of")
        if holdings_as_of is not None:
            if not isinstance(holdings_as_of, str) or not holdings_as_of.strip():
                errors.append(f"funds[{fund_index}].holdings_as_of must be a date string")
            else:
                _collect_call(errors, lambda: parse_day(holdings_as_of))
        if market_value_mode:
            market_value_currency = fund.get("market_value_currency")
            if isinstance(market_value_currency, str) and not market_value_currency.strip():
                errors.append(
                    f"funds[{fund_index}].market_value_currency must be a non-empty string"
                )
            elif (
                isinstance(market_value_currency, str)
                and normalized_base_currency is not None
                and market_value_currency.strip().upper() != normalized_base_currency
            ):
                errors.append(
                    f"funds[{fund_index}].market_value must be converted to "
                    f"{normalized_base_currency}"
                )

    targets = payload.get("targets")
    if targets not in (None, {}):
        if not isinstance(targets, dict):
            errors.append("targets must be an object")
        else:
            for raw_dimension, categories in targets.items():
                if not isinstance(raw_dimension, str) or not raw_dimension.strip():
                    errors.append("targets dimensions must be non-empty strings")
                    continue
                dimension = raw_dimension.strip()
                if dimension not in allowed_target_dimensions:
                    errors.append(f"unsupported target/limit dimension: {dimension}")
                if not isinstance(categories, dict):
                    errors.append(f"targets.{dimension} must be an object")
                    continue
                for raw_category, band in categories.items():
                    if not isinstance(raw_category, str) or not raw_category.strip():
                        errors.append(
                            f"targets.{dimension} categories must be non-empty strings"
                        )
                        continue
                    category = raw_category.strip()
                    if not isinstance(band, dict) or not ({"min", "max"} & set(band)):
                        errors.append(f"targets.{dimension}.{category} needs min and/or max")
                        continue
                    unknown_band_keys = sorted(
                        repr(key)
                        for key in band
                        if not isinstance(key, str) or key not in {"min", "max"}
                    )
                    if unknown_band_keys:
                        errors.append(
                            f"targets.{dimension}.{category} contains unknown key(s): "
                            f"{', '.join(unknown_band_keys)}"
                        )
                    minimum = _collect_call(
                        errors,
                        lambda band=band, dimension=dimension, category=category: ratio(
                            band.get("min", 0), f"targets.{dimension}.{category}.min"
                        ),
                    )
                    maximum = _collect_call(
                        errors,
                        lambda band=band, dimension=dimension, category=category: ratio(
                            band.get("max", 1), f"targets.{dimension}.{category}.max"
                        ),
                    )
                    if minimum is not None and maximum is not None and minimum > maximum:
                        errors.append(f"targets.{dimension}.{category} min exceeds max")

    limits = payload.get("limits")
    if limits not in (None, []):
        if not isinstance(limits, list):
            errors.append("limits must be an array")
        else:
            for index, item in enumerate(limits):
                if not isinstance(item, dict):
                    errors.append(f"limits[{index}] needs dimension and max")
                    continue
                unknown_limit_keys = sorted(
                    repr(key)
                    for key in item
                    if not isinstance(key, str)
                    or key not in {"dimension", "category", "max"}
                )
                if unknown_limit_keys:
                    errors.append(
                        f"limits[{index}] contains unknown key(s): "
                        f"{', '.join(unknown_limit_keys)}"
                    )
                dimension = item.get("dimension")
                if not isinstance(dimension, str) or not dimension.strip():
                    errors.append(f"limits[{index}].dimension must be a non-empty string")
                elif dimension.strip() not in allowed_target_dimensions:
                    errors.append(f"unsupported limit dimension: {dimension.strip()}")
                if "max" not in item:
                    errors.append(f"limits[{index}] needs dimension and max")
                else:
                    _collect_call(
                        errors,
                        lambda item=item, index=index: ratio(
                            item["max"], f"limits[{index}].max"
                        ),
                    )
                category = item.get("category")
                if category is not None and (
                    not isinstance(category, str) or not category.strip()
                ):
                    errors.append(f"limits[{index}].category must be a non-empty string")

    if funds and all(isinstance(fund, dict) for fund in funds):
        try:
            resolve_weights(payload)
        except ValueError as exc:
            errors.append(str(exc))
    return list(dict.fromkeys(errors))


def analyze_portfolio(payload: dict[str, Any]) -> dict[str, Any]:
    preflight_errors = collect_portfolio_errors(payload)
    if preflight_errors:
        raise ValueError(
            f"input preflight found {len(preflight_errors)} problem(s):\n"
            + "\n".join(f"- {error}" for error in preflight_errors)
        )
    resolved, cash_weight, unallocated_weight, weight_mode = resolve_weights(payload)
    raw_analysis_date = payload.get("analysis_date", date.today().isoformat())
    if not isinstance(raw_analysis_date, str):
        raise ValueError("analysis_date must be a date string")
    analysis_day = parse_day(raw_analysis_date)
    base_currency = payload.get("base_currency")
    if base_currency is not None and (
        not isinstance(base_currency, str) or not base_currency.strip()
    ):
        raise ValueError("base_currency must be a non-empty string when supplied")
    normalized_base_currency = base_currency.strip().upper() if isinstance(base_currency, str) else None
    if weight_mode == "market_value" and not normalized_base_currency:
        raise ValueError("base_currency is required in market_value mode")
    if weight_mode == "market_value":
        for fund_index, (fund, _) in enumerate(resolved):
            market_value_currency = fund.get("market_value_currency")
            if market_value_currency is None:
                continue
            if not isinstance(market_value_currency, str) or not market_value_currency.strip():
                raise ValueError(
                    f"funds[{fund_index}].market_value_currency must be a non-empty string"
                )
            if market_value_currency.strip().upper() != normalized_base_currency:
                raise ValueError(
                    f"funds[{fund_index}].market_value must be converted to {normalized_base_currency}"
                )
    exposures: dict[str, dict[str, float]] = {
        dimension: defaultdict(float)
        for dimension in ("asset_class", *EXCLUSIVE_DIMENSIONS, *FUND_DIMENSIONS, "risk_tag")
    }
    exposures["asset_class"]["现金"] += cash_weight
    if unallocated_weight > EPSILON:
        exposures["asset_class"]["未配置"] += unallocated_weight

    securities: dict[str, dict[str, Any]] = {}
    fund_weights: dict[str, float] = {}
    fund_rows: list[dict[str, Any]] = []
    risk_score_total = 0.0
    risk_score_coverage = 0.0
    warnings: list[str] = []

    for fund_index, (fund, fund_weight) in enumerate(resolved):
        raw_name = fund.get("name")
        if raw_name is not None and not isinstance(raw_name, str):
            raise ValueError(f"funds[{fund_index}].name must be a string")
        explicit_name = (raw_name or "").strip()
        raw_code = fund.get("code")
        if raw_code is not None and not isinstance(raw_code, str):
            raise ValueError(
                f"funds[{fund_index}].code must be a string so leading zeros are preserved"
            )
        fund_code = (raw_code or "").strip()
        if raw_code is not None and not fund_code:
            raise ValueError(f"funds[{fund_index}].code must not be blank")
        identity_fields: dict[str, str | None] = {}
        for field in ("share_class", "trading_venue"):
            raw_value = fund.get(field)
            if raw_value is not None and (
                not isinstance(raw_value, str) or not raw_value.strip()
            ):
                raise ValueError(f"funds[{fund_index}].{field} must be a non-empty string")
            identity_fields[field] = raw_value.strip() if raw_value is not None else None
        if identity_fields["trading_venue"] not in {None, "场内", "场外"}:
            raise ValueError(f"funds[{fund_index}].trading_venue must be 场内 or 场外")
        display_name = explicit_name or fund_code or f"基金{fund_index + 1}"
        label = f"{display_name} [{fund_code}]" if explicit_name and fund_code else display_name
        if label in fund_weights:
            raise ValueError(f"duplicate fund identity: {label}")
        fund_weights[label] = fund_weight
        fund_rows.append(
            {
                "name": display_name,
                "code": fund_code or None,
                "share_class": identity_fields["share_class"],
                "trading_venue": identity_fields["trading_venue"],
                "label": label,
                "weight": fund_weight,
            }
        )

        for output_dimension, field in FUND_DIMENSIONS.items():
            raw_category = fund.get(field)
            if raw_category is not None and not isinstance(raw_category, str):
                raise ValueError(f"funds[{fund_index}].{field} must be a string")
            category = (raw_category or "未知").strip() or "未知"
            add_exposure(exposures[output_dimension], category, fund_weight)

        if "risk_score" in fund:
            score = nonnegative_number(fund["risk_score"], f"funds[{fund_index}].risk_score")
            if not math.isfinite(score) or score < 1 or score > 5:
                raise ValueError(f"funds[{fund_index}].risk_score must be between 1 and 5")
            risk_score_total += score * fund_weight
            risk_score_coverage += fund_weight

        asset_allocation = fund.get("asset_allocation")
        if asset_allocation is not None:
            allocation = validate_mapping(asset_allocation, f"funds[{fund_index}].asset_allocation")
            for category, inner_weight in allocation.items():
                add_exposure(exposures["asset_class"], category, fund_weight * inner_weight)
            residual = 1.0 - sum(allocation.values())
        else:
            residual = 1.0
        if residual > EPSILON:
            add_exposure(exposures["asset_class"], UNKNOWN, fund_weight * residual)

        for dimension in EXCLUSIVE_DIMENSIONS:
            mapping = derive_dimension(fund, dimension, fund_index)
            for category, inner_weight in mapping.items():
                add_exposure(exposures[dimension], category, fund_weight * inner_weight)
            residual = 1.0 - sum(mapping.values())
            if residual > EPSILON:
                add_exposure(exposures[dimension], UNKNOWN, fund_weight * residual)

        holdings = fund.get("holdings")
        if holdings is None:
            holdings = []
        if not isinstance(holdings, list):
            raise ValueError(f"funds[{fund_index}].holdings must be an array")
        fund_exposures = fund.get("exposures")
        if fund_exposures is None:
            fund_exposures = {}
        if not isinstance(fund_exposures, dict):
            raise ValueError(f"funds[{fund_index}].exposures must be an object")
        has_direct_risk_tags = "risk_tag" in fund_exposures
        direct_risk_tags = fund_exposures.get("risk_tag")
        if has_direct_risk_tags:
            risk_mapping = validate_nonexclusive_mapping(
                direct_risk_tags,
                f"funds[{fund_index}].exposures.risk_tag",
            )
            for tag, inner_weight in risk_mapping.items():
                add_exposure(exposures["risk_tag"], tag, fund_weight * inner_weight)
        holdings_total = 0.0
        seen_fund_securities: set[str] = set()
        for holding_index, holding in enumerate(holdings):
            if not isinstance(holding, dict):
                raise ValueError(f"funds[{fund_index}].holdings[{holding_index}] must be an object")
            holding_weight = ratio(
                holding.get("weight"),
                f"funds[{fund_index}].holdings[{holding_index}].weight",
            )
            holdings_total += holding_weight
            raw_ticker = holding.get("ticker")
            if raw_ticker is not None and not isinstance(raw_ticker, str):
                raise ValueError(
                    f"funds[{fund_index}].holdings[{holding_index}].ticker must be a string"
                )
            ticker = (raw_ticker or "").strip().upper()
            raw_holding_name = holding.get("name")
            if raw_holding_name is not None and not isinstance(raw_holding_name, str):
                raise ValueError(
                    f"funds[{fund_index}].holdings[{holding_index}].name must be a string"
                )
            holding_name = (raw_holding_name or "").strip()
            if not ticker and not holding_name:
                raise ValueError(
                    f"funds[{fund_index}].holdings[{holding_index}] needs name or ticker"
                )
            holding_name = holding_name or ticker
            security_id = ticker or holding_name
            if security_id in seen_fund_securities:
                raise ValueError(
                    f"funds[{fund_index}].holdings contains duplicate security: {security_id}"
                )
            seen_fund_securities.add(security_id)
            if security_id not in securities:
                securities[security_id] = {
                    "id": security_id,
                    "name": holding_name,
                    "weight": 0.0,
                    "funds": set(),
                }
                if not ticker:
                    warnings.append(
                        f"{label}: {holding_name} has no ticker and is matched by name."
                    )
            securities[security_id]["weight"] += fund_weight * holding_weight
            securities[security_id]["funds"].add(label)
            tags = holding.get("risk_tags")
            if tags is None:
                tags = []
            if not isinstance(tags, list):
                raise ValueError(
                    f"funds[{fund_index}].holdings[{holding_index}].risk_tags must be an array"
                )
            normalized_tags: set[str] = set()
            for tag_index, value in enumerate(tags):
                if not isinstance(value, str) or not value.strip():
                    raise ValueError(
                        f"funds[{fund_index}].holdings[{holding_index}].risk_tags[{tag_index}] must be a non-empty string"
                    )
                normalized_tags.add(value.strip())
            if not has_direct_risk_tags:
                for tag in normalized_tags:
                    add_exposure(exposures["risk_tag"], tag, fund_weight * holding_weight)
        if holdings_total > 1 + EPSILON:
            raise ValueError(f"funds[{fund_index}].holdings weights exceed 1")

        as_of = fund.get("holdings_as_of")
        if as_of is not None:
            if not isinstance(as_of, str) or not as_of.strip():
                raise ValueError(f"funds[{fund_index}].holdings_as_of must be a date string")
            holdings_day = parse_day(as_of)
            age_days = (analysis_day - holdings_day).days
            if age_days < 0:
                warnings.append(f"{label}: holdings_as_of is later than analysis_date.")
            elif age_days > 120:
                warnings.append(f"{label}: disclosed holdings are {age_days} days old.")
        elif holdings:
            warnings.append(f"{label}: holdings_as_of is missing.")

    security_rows = []
    for item in securities.values():
        funds = sorted(item["funds"])
        security_rows.append(
            {
                "id": item["id"],
                "name": item["name"],
                "weight": item["weight"],
                "fund_count": len(funds),
                "funds": funds,
            }
        )
    security_rows.sort(key=lambda item: (-float(item["weight"]), str(item["id"])))
    overlap_rows = [item for item in security_rows if int(item["fund_count"]) >= 2]
    underlying_security_coverage = sum(float(item["weight"]) for item in security_rows)

    fund_weight_total = sum(fund_weights.values())
    normalized_hhi = (
        sum((weight / fund_weight_total) ** 2 for weight in fund_weights.values())
        if fund_weight_total > 0
        else None
    )
    sorted_fund_weights = sorted(fund_weights.values(), reverse=True)
    exposure_maps = {name: dict(values) for name, values in exposures.items()}
    coverage = {
        dimension: max(
            0.0,
            sum(values.values())
            - values.get(UNKNOWN, 0.0)
            - values.get("未知", 0.0)
            - values.get("未配置", 0.0),
        )
        for dimension, values in exposure_maps.items()
        if dimension != "risk_tag"
    }
    targets = evaluate_targets(payload.get("targets"), exposure_maps, fund_weights, securities)
    breaches = evaluate_limits(payload.get("limits"), exposure_maps, fund_weights, securities)

    return {
        "metadata": {
            "analysis_date": analysis_day.isoformat(),
            "base_currency": normalized_base_currency,
            "ratio_unit": "decimal",
            "weight_mode": weight_mode,
            "exclusive_dimensions": ["asset_class", *EXCLUSIVE_DIMENSIONS, *FUND_DIMENSIONS],
            "non_exclusive_dimensions": ["risk_tag"],
        },
        "portfolio_summary": {
            "fund_count": len(fund_weights),
            "fund_weight": fund_weight_total,
            "cash_weight": cash_weight,
            "unallocated_weight": unallocated_weight,
            "top_1_fund_weight": sum(sorted_fund_weights[:1]),
            "top_3_fund_weight": sum(sorted_fund_weights[:3]),
            "top_5_fund_weight": sum(sorted_fund_weights[:5]),
            "fund_hhi_normalized": normalized_hhi,
            "effective_fund_count": (
                1.0 / normalized_hhi
                if normalized_hhi is not None and normalized_hhi > 0
                else None
            ),
            "weighted_risk_score": risk_score_total / risk_score_coverage if risk_score_coverage else None,
            "risk_score_coverage": risk_score_coverage,
            "underlying_security_coverage": underlying_security_coverage,
            "underlying_security_uncovered_weight": max(
                0.0, fund_weight_total - underlying_security_coverage
            ),
        },
        "fund_weights": sorted(fund_rows, key=lambda item: (-float(item["weight"]), item["label"])),
        "exposures": {name: exposure_rows(values) for name, values in exposure_maps.items()},
        "exposure_coverage": coverage,
        "underlying_securities": security_rows,
        "overlapping_securities": overlap_rows,
        "target_gaps": targets,
        "limit_breaches": breaches,
        "warnings": [
            *warnings,
            "Underlying holdings reflect disclosed report dates, not real-time positions.",
            "Risk tags are non-exclusive and can sum to more than 100%.",
            "Unknown and undisclosed exposure must not be silently redistributed.",
        ],
    }


def run_self_test() -> None:
    payload = {
        "analysis_date": "2026-07-21",
        "cash_weight": 0.10,
        "funds": [
            {
                "name": "成长基金",
                "code": "000001",
                "share_class": "A",
                "trading_venue": "场外",
                "weight": 0.50,
                "category": "主动股票",
                "risk_score": 4,
                "asset_allocation": {"股票": 0.90, "现金": 0.10},
                "exposures": {"industry": {"信息技术": 0.60, "消费": 0.30}},
                "holdings_as_of": "2026-06-30",
                "holdings": [
                    {
                        "name": "腾讯控股",
                        "ticker": "0700.HK",
                        "weight": 0.10,
                        "industry": "信息技术",
                        "risk_tags": ["监管", "汇率"],
                    }
                ],
            },
            {
                "name": "均衡基金",
                "weight": 0.40,
                "category": "混合",
                "risk_score": 3,
                "asset_allocation": {"股票": 0.60, "债券": 0.35, "现金": 0.05},
                "exposures": {"industry": {"信息技术": 0.40, "金融": 0.30}},
                "holdings_as_of": "2026-06-30",
                "holdings": [
                    {
                        "name": "腾讯控股",
                        "ticker": "0700.HK",
                        "weight": 0.05,
                        "industry": "信息技术",
                        "risk_tags": ["监管"],
                    }
                ],
            },
        ],
        "targets": {"asset_class": {"股票": {"min": 0.50, "max": 0.65}}},
        "limits": [
            {"dimension": "industry", "category": "信息技术", "max": 0.40},
            {"dimension": "fund", "max": 0.45},
        ],
    }
    result = analyze_portfolio(payload)
    securities = result["underlying_securities"]
    assert securities[0]["id"] == "0700.HK"
    assert math.isclose(securities[0]["weight"], 0.07)
    assert securities[0]["fund_count"] == 2
    assert any(item["dimension"] == "industry" for item in result["limit_breaches"])
    assert any(item["dimension"] == "fund" for item in result["limit_breaches"])
    assert result["target_gaps"][0]["status"] == "above"
    assert result["fund_weights"][0]["share_class"] == "A"
    assert result["fund_weights"][0]["trading_venue"] == "场外"
    assert math.isclose(result["portfolio_summary"]["underlying_security_coverage"], 0.07)
    assert math.isclose(
        result["portfolio_summary"]["underlying_security_uncovered_weight"],
        0.83,
    )
    same_name = analyze_portfolio(
        {
            "funds": [
                {"name": "同名基金", "code": "000001", "weight": 0.5},
                {"name": "同名基金", "code": "000002", "weight": 0.5},
            ]
        }
    )
    assert same_name["fund_weights"][0]["label"] in {
        "同名基金 [000001]",
        "同名基金 [000002]",
    }
    market_value_result = analyze_portfolio(
        {
            "base_currency": "cny",
            "cash_market_value": 100.0,
            "funds": [
                {
                    "name": "市值基金",
                    "market_value": 900.0,
                    "market_value_currency": "CNY",
                }
            ],
        }
    )
    assert market_value_result["metadata"]["base_currency"] == "CNY"
    assert math.isclose(market_value_result["portfolio_summary"]["cash_weight"], 0.1)
    try:
        analyze_portfolio(
            {"funds": [{"name": "无身份持仓", "weight": 1.0, "holdings": [{"weight": 0.1}]}]}
        )
    except ValueError:
        pass
    else:
        raise AssertionError("holdings without name and ticker must fail")
    try:
        analyze_portfolio({"funds": [{"name": "零权重", "weight": 0.0}]})
    except ValueError:
        pass
    else:
        raise AssertionError("zero-weight funds must fail")
    try:
        analyze_portfolio(
            {
                "base_currency": "CNY",
                "funds": [
                    {
                        "name": "异币种",
                        "market_value": 100.0,
                        "market_value_currency": "HKD",
                    }
                ],
            }
        )
    except ValueError:
        pass
    else:
        raise AssertionError("unconverted market values must fail")
    invalid_payloads = [
        {
            "analysis_date": 20260721,
            "funds": [{"name": "日期类型错误", "weight": 1.0}],
        },
        {
            "funds": [
                {
                    "name": "持仓日期类型错误",
                    "weight": 1.0,
                    "holdings_as_of": 20260721,
                }
            ],
        },
        {
            "funds": [{"name": "目标类型错误", "weight": 1.0}],
            "targets": {1: {"股票": {"max": 0.5}}},
        },
        {
            "funds": [
                {
                    "name": "风险标签类型错误",
                    "weight": 1.0,
                    "holdings": [{"name": "测试证券", "weight": 0.1, "risk_tags": 0}],
                }
            ],
        },
        {
            "funds": [
                {
                    "name": "分类类型错误",
                    "weight": 1.0,
                    "holdings": [{"name": "测试证券", "weight": 0.1, "industry": 1}],
                }
            ],
        },
        {
            "funds": [{"name": "限制类型错误", "weight": 1.0}],
            "limits": [{"dimension": "fund", "category": 1, "max": 0.5}],
        },
        {
            "funds": [
                {
                    "name": "重复分类",
                    "weight": 1.0,
                    "asset_allocation": {"股票": 0.4, " 股票 ": 0.4},
                }
            ],
        },
        {
            "funds": [
                {
                    "name": "份额类型错误",
                    "share_class": 1,
                    "weight": 1.0,
                }
            ],
        },
        {
            "funds": [
                {
                    "name": "交易渠道错误",
                    "trading_venue": "银行",
                    "weight": 1.0,
                }
            ],
        },
    ]
    for invalid_payload in invalid_payloads:
        try:
            analyze_portfolio(invalid_payload)
        except ValueError:
            pass
        else:
            raise AssertionError(f"invalid payload must fail: {invalid_payload!r}")
    batch_invalid = {
        "analysis_date": 20260731,
        "funds": [
            {
                "name": 1,
                "share_class": 2,
                "weight": -0.1,
                "holdings": [{"weight": -1, "risk_tags": [None]}],
            },
            {
                "name": "第二只",
                "trading_venue": "银行",
                "weight": 0,
                "holdings_as_of": "not-a-date",
            },
        ],
        "targets": {
            "not-a-dimension": {
                "坏目标": {"min": 0.8, "max": 0.2},
            }
        },
        "limits": [
            {"dimension": "fund", "category": 1, "max": 2},
        ],
    }
    try:
        analyze_portfolio(batch_invalid)
    except ValueError as exc:
        message = str(exc)
        assert "analysis_date" in message
        assert "funds[0].weight" in message
        assert "funds[1].weight" in message
        assert "trading_venue" in message
        assert "not-a-dimension" in message
        assert "min exceeds max" in message
        assert "limits[0].category" in message
    else:
        raise AssertionError("all invalid portfolio items must be reported together")
    print("self-test passed")


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Aggregate look-through exposures for a fund portfolio.")
    parser.add_argument("portfolio_json", nargs="?", type=Path, help="portfolio input JSON")
    parser.add_argument("--output", type=Path, help="write analysis JSON to this path instead of stdout")
    parser.add_argument("--self-test", action="store_true", help="run deterministic built-in tests")
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.self_test:
        run_self_test()
        return 0
    if args.portfolio_json is None:
        raise ValueError("portfolio_json is required unless --self-test is used")
    if args.output and args.output.expanduser().resolve() == args.portfolio_json.expanduser().resolve():
        raise ValueError("--output must be different from the portfolio input JSON")
    if args.output:
        try:
            args.output.expanduser().resolve().relative_to(SKILL_DIR)
        except ValueError:
            pass
        else:
            raise ValueError("--output must be outside the skill directory")
    payload = json.loads(args.portfolio_json.read_text(encoding="utf-8-sig"))
    if not isinstance(payload, dict):
        raise ValueError("portfolio input must be a JSON object")
    result = analyze_portfolio(payload)
    serialized = json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False)
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(serialized + "\n", encoding="utf-8")
    else:
        print(serialized)
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
