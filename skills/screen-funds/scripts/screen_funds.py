#!/usr/bin/env python3
"""Build an auditable, cohort-relative shortlist from a normalized fund universe CSV."""

from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import re
import sys
from collections import Counter, defaultdict
from datetime import date, datetime
from pathlib import Path
from typing import Any, Iterable


SKILL_DIR = Path(__file__).resolve().parent.parent
MISSING_TOKENS = {"", "na", "n/a", "null", "none", "--"}
REQUIRED_COLUMNS = (
    "snapshot_date",
    "fund_code",
    "fund_name",
    "share_class",
    "strategy_id",
    "ranking_share",
    "cohort",
    "category",
    "currency",
    "status",
    "inception_date",
    "metrics_as_of",
    "source_ids",
)
STRING_COLUMNS = set(REQUIRED_COLUMNS) - {"ranking_share"}
VALID_DIRECTIONS = {"higher", "lower", "lower_abs"}
VALID_MISSING_POLICIES = {"exclude", "review"}
VALID_OPERATORS = {"eq", "neq", "in", "not_in", "gte", "lte", "between", "truthy", "falsy"}
VALID_SOURCE_ROLES = {"universe-enumeration", "identity", "metrics", "validation"}


def reject_unknown_keys(value: dict[str, Any], allowed: set[str], field: str) -> None:
    unknown = [key for key in value if not isinstance(key, str) or key not in allowed]
    if unknown:
        rendered = ", ".join(sorted((repr(key) for key in unknown)))
        raise ValueError(f"{field} contains unknown key(s): {rendered}")


def parse_day(value: Any, field: str) -> date:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field} must be a non-empty date string")
    raw = value.strip()
    for fmt in ("%Y-%m-%d", "%Y/%m/%d", "%Y%m%d"):
        try:
            return datetime.strptime(raw, fmt).date()
        except ValueError:
            pass
    raise ValueError(f"{field} has unsupported date: {value!r}")


def parse_bool(value: Any, field: str) -> bool:
    if isinstance(value, bool):
        return value
    if isinstance(value, str):
        normalized = value.strip().lower()
        if normalized in {"true", "1", "yes", "y"}:
            return True
        if normalized in {"false", "0", "no", "n"}:
            return False
    raise ValueError(f"{field} must be a boolean")


def scalar(value: Any) -> Any:
    if value is None or isinstance(value, (bool, int, float)):
        return value
    if not isinstance(value, str):
        return value
    stripped = value.strip()
    if stripped.lower() in MISSING_TOKENS:
        return None
    if stripped.lower() in {"true", "false"}:
        return stripped.lower() == "true"
    return stripped


def nonempty_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{field} must be a non-empty string")
    return value.strip()


def positive_int(value: Any, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        raise ValueError(f"{field} must be a positive integer")
    return value


def nonnegative_int(value: Any, field: str) -> int:
    if isinstance(value, bool) or not isinstance(value, int) or value < 0:
        raise ValueError(f"{field} must be a non-negative integer")
    return value


def finite_number(value: Any, field: str) -> float:
    if isinstance(value, bool):
        raise ValueError(f"{field} must be a finite number")
    if isinstance(value, str) and re.fullmatch(
        r"[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][+-]?\d+)?", value.strip()
    ):
        value = float(value.strip())
    if not isinstance(value, (int, float)) or not math.isfinite(value):
        raise ValueError(f"{field} must be a finite number")
    return float(value)


def validate_filter_scalar(value: Any, field: str) -> None:
    if value is None or isinstance(value, (dict, list)):
        raise ValueError(f"{field} must be a non-null scalar")
    if isinstance(value, (int, float)) and not isinstance(value, bool) and not math.isfinite(value):
        raise ValueError(f"{field} must be finite")
    if not isinstance(value, (str, bool, int, float)):
        raise ValueError(f"{field} has unsupported scalar type")


def parse_json_object(text: str, label: str) -> dict[str, Any]:
    def pairs_hook(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
        value: dict[str, Any] = {}
        for key, item in pairs:
            if key in value:
                raise ValueError(f"{label} contains duplicate JSON key: {key}")
            value[key] = item
        return value

    def reject_constant(value: str) -> None:
        raise ValueError(f"{label} contains non-standard number: {value}")

    value = json.loads(
        text,
        object_pairs_hook=pairs_hook,
        parse_constant=reject_constant,
    )
    if not isinstance(value, dict):
        raise ValueError(f"{label} must be a JSON object")
    return value


def read_json_object(path: Path, label: str) -> dict[str, Any]:
    return parse_json_object(path.read_text(encoding="utf-8-sig"), label)


def validate_manifest(manifest: dict[str, Any]) -> dict[str, Any]:
    reject_unknown_keys(
        manifest,
        {
            "schema_version",
            "scope",
            "universe_name",
            "snapshot_date",
            "expected_share_count",
            "expected_strategy_count",
            "max_enumeration_age_days",
            "sources",
        },
        "manifest",
    )
    schema_version = manifest.get("schema_version")
    if isinstance(schema_version, bool) or not isinstance(schema_version, int) or schema_version != 1:
        raise ValueError("manifest.schema_version must be 1")
    scope = nonempty_string(manifest.get("scope"), "manifest.scope")
    if scope not in {"all-market", "subset"}:
        raise ValueError("manifest.scope must be all-market or subset")
    snapshot = parse_day(manifest.get("snapshot_date"), "manifest.snapshot_date")
    if snapshot > date.today():
        raise ValueError("manifest.snapshot_date is in the future")
    universe_name = nonempty_string(manifest.get("universe_name"), "manifest.universe_name")
    sources = manifest.get("sources")
    if not isinstance(sources, list) or not sources:
        raise ValueError("manifest.sources must be a non-empty array")
    cleaned_sources: list[dict[str, Any]] = []
    seen_ids: set[str] = set()
    enumeration_source_ids: set[str] = set()
    source_roles: dict[str, set[str]] = {}
    source_as_of: dict[str, date] = {}
    for index, item in enumerate(sources):
        if not isinstance(item, dict):
            raise ValueError(f"manifest.sources[{index}] must be an object")
        reject_unknown_keys(
            item,
            {
                "id",
                "publisher",
                "locator",
                "snapshot_file",
                "content_sha256",
                "roles",
                "acquired_at",
                "as_of",
            },
            f"manifest.sources[{index}]",
        )
        source_id = nonempty_string(item.get("id"), f"manifest.sources[{index}].id")
        if source_id in seen_ids:
            raise ValueError(f"duplicate manifest source id: {source_id}")
        seen_ids.add(source_id)
        publisher = nonempty_string(item.get("publisher"), f"manifest.sources[{index}].publisher")
        locator = nonempty_string(item.get("locator"), f"manifest.sources[{index}].locator")
        snapshot_file = nonempty_string(
            item.get("snapshot_file"), f"manifest.sources[{index}].snapshot_file"
        )
        content_sha256 = nonempty_string(
            item.get("content_sha256"), f"manifest.sources[{index}].content_sha256"
        ).lower()
        if not re.fullmatch(r"[0-9a-f]{64}", content_sha256):
            raise ValueError(f"manifest.sources[{index}].content_sha256 must be 64 hex characters")
        roles = item.get("roles")
        if not isinstance(roles, list) or not roles:
            raise ValueError(f"manifest.sources[{index}].roles must be a non-empty array")
        cleaned_roles: list[str] = []
        for role_index, role in enumerate(roles):
            role_name = nonempty_string(role, f"manifest.sources[{index}].roles[{role_index}]")
            if role_name not in VALID_SOURCE_ROLES:
                raise ValueError(f"manifest.sources[{index}] has unsupported role: {role_name}")
            if role_name in cleaned_roles:
                raise ValueError(f"manifest.sources[{index}].roles contains duplicate: {role_name}")
            cleaned_roles.append(role_name)
        if "universe-enumeration" in cleaned_roles:
            enumeration_source_ids.add(source_id)
        source_roles[source_id] = set(cleaned_roles)
        acquired_at = parse_day(item.get("acquired_at"), f"manifest.sources[{index}].acquired_at")
        as_of = parse_day(item.get("as_of"), f"manifest.sources[{index}].as_of")
        if acquired_at > date.today():
            raise ValueError(f"manifest.sources[{index}].acquired_at is in the future")
        if as_of > snapshot:
            raise ValueError(f"manifest.sources[{index}].as_of exceeds snapshot_date")
        if as_of > acquired_at:
            raise ValueError(f"manifest.sources[{index}].as_of exceeds acquired_at")
        source_as_of[source_id] = as_of
        cleaned_sources.append(
            {
                "id": source_id,
                "publisher": publisher,
                "locator": locator,
                "snapshot_file": snapshot_file,
                "content_sha256": content_sha256,
                "roles": cleaned_roles,
                "acquired_at": acquired_at.isoformat(),
                "as_of": as_of.isoformat(),
            }
        )
    expected_shares = manifest.get("expected_share_count")
    expected_strategies = manifest.get("expected_strategy_count")
    max_enumeration_age = manifest.get("max_enumeration_age_days")
    if scope == "all-market":
        if not enumeration_source_ids:
            raise ValueError("all-market manifest requires a universe-enumeration source")
        max_enumeration_age = nonnegative_int(
            max_enumeration_age, "manifest.max_enumeration_age_days"
        )
        expected_shares = positive_int(expected_shares, "manifest.expected_share_count")
        expected_strategies = positive_int(expected_strategies, "manifest.expected_strategy_count")
    else:
        if max_enumeration_age is not None:
            max_enumeration_age = nonnegative_int(
                max_enumeration_age, "manifest.max_enumeration_age_days"
            )
            if not enumeration_source_ids:
                raise ValueError(
                    "manifest.max_enumeration_age_days requires a universe-enumeration source"
                )
        if expected_shares is not None:
            expected_shares = positive_int(expected_shares, "manifest.expected_share_count")
        if expected_strategies is not None:
            expected_strategies = positive_int(expected_strategies, "manifest.expected_strategy_count")
    enumeration_age_days = None
    if enumeration_source_ids:
        freshest_enumeration = max(source_as_of[source_id] for source_id in enumeration_source_ids)
        enumeration_age_days = (snapshot - freshest_enumeration).days
        if max_enumeration_age is not None and enumeration_age_days > max_enumeration_age:
            raise ValueError(
                f"freshest universe-enumeration source is {enumeration_age_days} days old; "
                f"limit is {max_enumeration_age}"
            )
    return {
        "schema_version": 1,
        "scope": scope,
        "snapshot_date": snapshot,
        "universe_name": universe_name,
        "expected_share_count": expected_shares,
        "expected_strategy_count": expected_strategies,
        "max_enumeration_age_days": max_enumeration_age,
        "enumeration_age_days": enumeration_age_days,
        "sources": cleaned_sources,
        "source_ids": seen_ids,
        "enumeration_source_ids": enumeration_source_ids,
        "source_roles": source_roles,
        "source_as_of": source_as_of,
    }


def validate_config(config: dict[str, Any]) -> dict[str, Any]:
    reject_unknown_keys(
        config,
        {
            "schema_version",
            "top_n_per_cohort",
            "minimum_cohort_size",
            "hard_filters",
            "scorecards",
            "cohort_settings",
        },
        "config",
    )
    schema_version = config.get("schema_version")
    if isinstance(schema_version, bool) or not isinstance(schema_version, int) or schema_version != 1:
        raise ValueError("config.schema_version must be 1")
    top_n = positive_int(config.get("top_n_per_cohort"), "config.top_n_per_cohort")
    minimum_size = positive_int(config.get("minimum_cohort_size"), "config.minimum_cohort_size")
    if minimum_size < 2:
        raise ValueError("config.minimum_cohort_size must be at least 2")
    hard_filters = config.get("hard_filters", [])
    if not isinstance(hard_filters, list):
        raise ValueError("config.hard_filters must be an array")
    cleaned_filters: list[dict[str, Any]] = []
    seen_filter_ids: set[str] = set()
    for index, item in enumerate(hard_filters):
        if not isinstance(item, dict):
            raise ValueError(f"config.hard_filters[{index}] must be an object")
        reject_unknown_keys(
            item,
            {"id", "field", "op", "value", "missing", "cohorts"},
            f"config.hard_filters[{index}]",
        )
        filter_id = nonempty_string(item.get("id"), f"config.hard_filters[{index}].id")
        if filter_id in seen_filter_ids:
            raise ValueError(f"duplicate hard filter id: {filter_id}")
        seen_filter_ids.add(filter_id)
        field = nonempty_string(item.get("field"), f"config.hard_filters[{index}].field")
        operator = nonempty_string(item.get("op"), f"config.hard_filters[{index}].op")
        if operator not in VALID_OPERATORS:
            raise ValueError(f"unsupported operator in hard filter {filter_id}: {operator}")
        missing = item.get("missing", "review")
        if missing not in VALID_MISSING_POLICIES:
            raise ValueError(f"hard filter {filter_id} missing must be exclude or review")
        if operator not in {"truthy", "falsy"} and "value" not in item:
            raise ValueError(f"hard filter {filter_id} requires value")
        if operator in {"truthy", "falsy"} and "value" in item:
            raise ValueError(f"hard filter {filter_id} must not provide value for {operator}")
        expected = item.get("value")
        if operator in {"in", "not_in", "between"} and not isinstance(expected, list):
            raise ValueError(f"hard filter {filter_id} value must be an array")
        if operator in {"in", "not_in"} and not expected:
            raise ValueError(f"hard filter {filter_id} value must not be empty")
        if field in STRING_COLUMNS and operator in {"gte", "lte", "between", "truthy", "falsy"}:
            raise ValueError(
                f"hard filter {filter_id} cannot use {operator} on required string field {field}"
            )
        if field in STRING_COLUMNS and operator in {"eq", "neq"} and not isinstance(expected, str):
            raise ValueError(
                f"hard filter {filter_id} value for required string field {field} must be a string"
            )
        if field in STRING_COLUMNS and operator in {"in", "not_in"} and any(
            not isinstance(value, str) for value in expected
        ):
            raise ValueError(
                f"hard filter {filter_id} values for required string field {field} must be strings"
            )
        if operator in {"eq", "neq"}:
            validate_filter_scalar(expected, f"hard filter {filter_id} value")
        if operator in {"in", "not_in"}:
            for value_index, value in enumerate(expected):
                validate_filter_scalar(value, f"hard filter {filter_id} value[{value_index}]")
        if operator == "between" and len(expected) != 2:
            raise ValueError(f"hard filter {filter_id} between requires two values")
        if operator in {"gte", "lte"}:
            finite_number(expected, f"hard filter {filter_id} value")
        if operator == "between":
            lower = finite_number(expected[0], f"hard filter {filter_id} lower value")
            upper = finite_number(expected[1], f"hard filter {filter_id} upper value")
            if lower > upper:
                raise ValueError(f"hard filter {filter_id} lower value exceeds upper value")
        raw_cohorts = item.get("cohorts")
        cohorts: list[str] | None = None
        if raw_cohorts is not None:
            if not isinstance(raw_cohorts, list) or not raw_cohorts:
                raise ValueError(f"hard filter {filter_id} cohorts must be a non-empty array")
            cohorts = []
            for cohort_index, raw_cohort in enumerate(raw_cohorts):
                cohort = nonempty_string(
                    raw_cohort, f"config.hard_filters[{index}].cohorts[{cohort_index}]"
                )
                if cohort in cohorts:
                    raise ValueError(f"hard filter {filter_id} contains duplicate cohort: {cohort}")
                cohorts.append(cohort)
        cleaned_filters.append(
            {
                "id": filter_id,
                "field": field,
                "op": operator,
                "value": expected,
                "missing": missing,
                "cohorts": cohorts,
            }
        )
    scorecards = config.get("scorecards")
    if not isinstance(scorecards, dict) or not scorecards:
        raise ValueError("config.scorecards must be a non-empty object")
    cleaned_scorecards: dict[str, list[dict[str, Any]]] = {}
    for raw_cohort, metrics in scorecards.items():
        cohort = nonempty_string(raw_cohort, "config.scorecards cohort")
        if cohort in cleaned_scorecards:
            raise ValueError(f"duplicate scorecard cohort after trimming: {cohort}")
        if not isinstance(metrics, list) or not metrics:
            raise ValueError(f"config.scorecards.{cohort} must be a non-empty array")
        cleaned_metrics: list[dict[str, Any]] = []
        seen_fields: set[str] = set()
        for index, item in enumerate(metrics):
            if not isinstance(item, dict):
                raise ValueError(f"config.scorecards.{cohort}[{index}] must be an object")
            reject_unknown_keys(
                item,
                {"field", "direction", "weight", "valid_min", "valid_max"},
                f"config.scorecards.{cohort}[{index}]",
            )
            field = nonempty_string(item.get("field"), f"config.scorecards.{cohort}[{index}].field")
            if field in seen_fields:
                raise ValueError(f"duplicate score field in cohort {cohort}: {field}")
            seen_fields.add(field)
            direction = nonempty_string(
                item.get("direction"), f"config.scorecards.{cohort}[{index}].direction"
            )
            if direction not in VALID_DIRECTIONS:
                raise ValueError(f"invalid direction for {cohort}.{field}: {direction}")
            weight = finite_number(item.get("weight"), f"config.scorecards.{cohort}[{index}].weight")
            if weight <= 0:
                raise ValueError(f"weight must be positive for {cohort}.{field}")
            valid_min = item.get("valid_min")
            valid_max = item.get("valid_max")
            if valid_min is not None:
                valid_min = finite_number(valid_min, f"valid_min for {cohort}.{field}")
            if valid_max is not None:
                valid_max = finite_number(valid_max, f"valid_max for {cohort}.{field}")
            if valid_min is not None and valid_max is not None and valid_min > valid_max:
                raise ValueError(f"valid_min exceeds valid_max for {cohort}.{field}")
            cleaned_metrics.append(
                {
                    "field": field,
                    "direction": direction,
                    "weight": weight,
                    "valid_min": valid_min,
                    "valid_max": valid_max,
                }
            )
        total_weight = sum(item["weight"] for item in cleaned_metrics)
        if not math.isfinite(total_weight) or total_weight <= 0:
            raise ValueError(f"total score weight must be finite and positive for cohort {cohort}")
        for item in cleaned_metrics:
            item["normalized_weight"] = item["weight"] / total_weight
        cleaned_scorecards[cohort] = cleaned_metrics
    for item in cleaned_filters:
        unknown = sorted(set(item["cohorts"] or []) - set(cleaned_scorecards))
        if unknown:
            raise ValueError(
                f"hard filter {item['id']} references unknown scorecard cohort(s): {', '.join(unknown)}"
            )
    raw_settings = config.get("cohort_settings", {})
    if not isinstance(raw_settings, dict):
        raise ValueError("config.cohort_settings must be an object")
    normalized_settings: dict[str, Any] = {}
    for raw_cohort, raw_setting in raw_settings.items():
        cohort = nonempty_string(raw_cohort, "config.cohort_settings cohort")
        if cohort in normalized_settings:
            raise ValueError(f"duplicate cohort_settings key after trimming: {cohort}")
        normalized_settings[cohort] = raw_setting
    unknown_settings = sorted(set(normalized_settings) - set(cleaned_scorecards))
    if unknown_settings:
        raise ValueError(
            f"config.cohort_settings contains unknown cohort(s): {', '.join(unknown_settings)}"
        )
    cohort_settings: dict[str, dict[str, int]] = {}
    for cohort in cleaned_scorecards:
        raw_setting = normalized_settings.get(cohort, {})
        if not isinstance(raw_setting, dict):
            raise ValueError(f"config.cohort_settings.{cohort} must be an object")
        reject_unknown_keys(
            raw_setting,
            {"top_n", "minimum_cohort_size"},
            f"config.cohort_settings.{cohort}",
        )
        cohort_top_n = positive_int(
            raw_setting.get("top_n", top_n), f"config.cohort_settings.{cohort}.top_n"
        )
        cohort_minimum = positive_int(
            raw_setting.get("minimum_cohort_size", minimum_size),
            f"config.cohort_settings.{cohort}.minimum_cohort_size",
        )
        if cohort_minimum < 2:
            raise ValueError(
                f"config.cohort_settings.{cohort}.minimum_cohort_size must be at least 2"
            )
        cohort_settings[cohort] = {
            "top_n": cohort_top_n,
            "minimum_cohort_size": cohort_minimum,
        }
    return {
        "schema_version": 1,
        "top_n_per_cohort": top_n,
        "minimum_cohort_size": minimum_size,
        "hard_filters": cleaned_filters,
        "scorecards": cleaned_scorecards,
        "cohort_settings": cohort_settings,
    }


def validation_error(errors: list[str]) -> ValueError:
    unique = list(dict.fromkeys(errors))
    return ValueError(
        f"input preflight found {len(unique)} problem(s):\n"
        + "\n".join(f"- {error}" for error in unique)
    )


def _collect_error(errors: list[str], callback) -> Any:
    try:
        return callback()
    except (OSError, TypeError, ValueError) as exc:
        errors.append(str(exc))
        return None


def _collect_labeled_error(errors: list[str], label: str, callback) -> Any:
    try:
        return callback()
    except (OSError, TypeError, ValueError) as exc:
        errors.append(f"{label}: {exc}")
        return None


def collect_manifest_source_errors(manifest: dict[str, Any]) -> list[str]:
    """Collect source-entry errors independently so one bad source cannot hide another."""
    errors: list[str] = []
    _collect_error(
        errors,
        lambda: reject_unknown_keys(
            manifest,
            {
                "schema_version",
                "scope",
                "universe_name",
                "snapshot_date",
                "expected_share_count",
                "expected_strategy_count",
                "max_enumeration_age_days",
                "sources",
            },
            "manifest",
        ),
    )
    if manifest.get("schema_version") != 1 or isinstance(
        manifest.get("schema_version"), bool
    ):
        errors.append("manifest.schema_version must be 1")
    scope = _collect_error(
        errors, lambda: nonempty_string(manifest.get("scope"), "manifest.scope")
    )
    if scope is not None and scope not in {"all-market", "subset"}:
        errors.append("manifest.scope must be all-market or subset")
    _collect_error(
        errors,
        lambda: nonempty_string(manifest.get("universe_name"), "manifest.universe_name"),
    )
    snapshot = _collect_error(
        errors,
        lambda: parse_day(manifest.get("snapshot_date"), "manifest.snapshot_date"),
    )
    if snapshot is not None and snapshot > date.today():
        errors.append("manifest.snapshot_date is in the future")
    for field in ("expected_share_count", "expected_strategy_count"):
        if scope == "all-market" or manifest.get(field) is not None:
            _collect_error(
                errors,
                lambda field=field: positive_int(
                    manifest.get(field), f"manifest.{field}"
                ),
            )
    max_enumeration_age = None
    if scope == "all-market" or manifest.get("max_enumeration_age_days") is not None:
        max_enumeration_age = _collect_error(
            errors,
            lambda: nonnegative_int(
                manifest.get("max_enumeration_age_days"),
                "manifest.max_enumeration_age_days",
            ),
        )
    sources = manifest.get("sources")
    if not isinstance(sources, list) or not sources:
        errors.append("manifest.sources must be a non-empty array")
        return errors
    seen_ids: set[str] = set()
    allowed = {
        "id",
        "publisher",
        "locator",
        "snapshot_file",
        "content_sha256",
        "roles",
        "acquired_at",
        "as_of",
    }
    has_enumeration_source = False
    enumeration_dates: list[date] = []
    enumeration_date_uncertain = False
    roles_uncertain = False
    for index, item in enumerate(sources):
        prefix = f"manifest.sources[{index}]"
        if not isinstance(item, dict):
            errors.append(f"{prefix} must be an object")
            continue
        _collect_error(errors, lambda item=item, prefix=prefix: reject_unknown_keys(item, allowed, prefix))
        source_id = _collect_error(
            errors, lambda item=item, prefix=prefix: nonempty_string(item.get("id"), f"{prefix}.id")
        )
        if source_id is not None:
            if source_id in seen_ids:
                errors.append(f"duplicate manifest source id: {source_id}")
            seen_ids.add(source_id)
        for field in ("publisher", "locator", "snapshot_file"):
            _collect_error(
                errors,
                lambda item=item, prefix=prefix, field=field: nonempty_string(
                    item.get(field), f"{prefix}.{field}"
                ),
            )
        digest = _collect_error(
            errors,
            lambda item=item, prefix=prefix: nonempty_string(
                item.get("content_sha256"), f"{prefix}.content_sha256"
            ).lower(),
        )
        if digest is not None and not re.fullmatch(r"[0-9a-f]{64}", digest):
            errors.append(f"{prefix}.content_sha256 must be 64 hex characters")
        roles = item.get("roles")
        if not isinstance(roles, list) or not roles:
            errors.append(f"{prefix}.roles must be a non-empty array")
            roles_uncertain = True
            cleaned_roles: set[str] = set()
        else:
            seen_roles: set[str] = set()
            cleaned_roles = set()
            for role_index, role in enumerate(roles):
                role_name = _collect_error(
                    errors,
                    lambda role=role, role_index=role_index, prefix=prefix: nonempty_string(
                        role, f"{prefix}.roles[{role_index}]"
                    ),
                )
                if role_name is None:
                    roles_uncertain = True
                    continue
                if role_name not in VALID_SOURCE_ROLES:
                    errors.append(f"{prefix} has unsupported role: {role_name}")
                    roles_uncertain = True
                    continue
                if role_name in seen_roles:
                    errors.append(f"{prefix}.roles contains duplicate: {role_name}")
                seen_roles.add(role_name)
                cleaned_roles.add(role_name)
        acquired_at = _collect_error(
            errors,
            lambda item=item, prefix=prefix: parse_day(
                item.get("acquired_at"), f"{prefix}.acquired_at"
            ),
        )
        as_of = _collect_error(
            errors,
            lambda item=item, prefix=prefix: parse_day(item.get("as_of"), f"{prefix}.as_of"),
        )
        if acquired_at is not None and acquired_at > date.today():
            errors.append(f"{prefix}.acquired_at is in the future")
        if as_of is not None and snapshot is not None and as_of > snapshot:
            errors.append(f"{prefix}.as_of exceeds snapshot_date")
        if as_of is not None and acquired_at is not None and as_of > acquired_at:
            errors.append(f"{prefix}.as_of exceeds acquired_at")
        if "universe-enumeration" in cleaned_roles:
            has_enumeration_source = True
            if as_of is None:
                enumeration_date_uncertain = True
            else:
                enumeration_dates.append(as_of)

    if (
        scope == "all-market"
        and not has_enumeration_source
        and not roles_uncertain
    ):
        errors.append("all-market manifest requires a universe-enumeration source")
    if (
        scope == "subset"
        and manifest.get("max_enumeration_age_days") is not None
        and not has_enumeration_source
        and not roles_uncertain
    ):
        errors.append(
            "manifest.max_enumeration_age_days requires a universe-enumeration source"
        )
    if (
        snapshot is not None
        and max_enumeration_age is not None
        and enumeration_dates
        and not enumeration_date_uncertain
    ):
        enumeration_age = (snapshot - max(enumeration_dates)).days
        if enumeration_age > max_enumeration_age:
            errors.append(
                f"freshest universe-enumeration source is {enumeration_age} days old; "
                f"limit is {max_enumeration_age}"
            )
    return errors


def collect_config_item_errors(config: dict[str, Any]) -> list[str]:
    """Collect per-filter and per-scorecard errors before cross-config validation."""
    errors: list[str] = []
    _collect_error(
        errors,
        lambda: reject_unknown_keys(
            config,
            {
                "schema_version",
                "top_n_per_cohort",
                "minimum_cohort_size",
                "hard_filters",
                "scorecards",
                "cohort_settings",
            },
            "config",
        ),
    )
    if config.get("schema_version") != 1 or isinstance(
        config.get("schema_version"), bool
    ):
        errors.append("config.schema_version must be 1")
    _collect_error(
        errors,
        lambda: positive_int(config.get("top_n_per_cohort"), "config.top_n_per_cohort"),
    )
    minimum_size = _collect_error(
        errors,
        lambda: positive_int(
            config.get("minimum_cohort_size"), "config.minimum_cohort_size"
        ),
    )
    if minimum_size is not None and minimum_size < 2:
        errors.append("config.minimum_cohort_size must be at least 2")
    hard_filters = config.get("hard_filters", [])
    if not isinstance(hard_filters, list):
        errors.append("config.hard_filters must be an array")
    if isinstance(hard_filters, list):
        seen_ids: set[str] = set()
        for index, item in enumerate(hard_filters):
            prefix = f"config.hard_filters[{index}]"
            if not isinstance(item, dict):
                errors.append(f"{prefix} must be an object")
                continue
            _collect_error(
                errors,
                lambda item=item, prefix=prefix: reject_unknown_keys(
                    item,
                    {"id", "field", "op", "value", "missing", "cohorts"},
                    prefix,
                ),
            )
            filter_id = _collect_error(
                errors,
                lambda item=item, prefix=prefix: nonempty_string(item.get("id"), f"{prefix}.id"),
            )
            if filter_id is not None:
                if filter_id in seen_ids:
                    errors.append(f"duplicate hard filter id: {filter_id}")
                seen_ids.add(filter_id)
            field = _collect_error(
                errors,
                lambda item=item, prefix=prefix: nonempty_string(
                    item.get("field"), f"{prefix}.field"
                ),
            )
            operator = _collect_error(
                errors,
                lambda item=item, prefix=prefix: nonempty_string(
                    item.get("op"), f"{prefix}.op"
                ),
            )
            if operator is not None and operator not in VALID_OPERATORS:
                errors.append(f"unsupported operator in {prefix}: {operator}")
            if operator in VALID_OPERATORS:
                expected = item.get("value")
                if operator not in {"truthy", "falsy"} and "value" not in item:
                    errors.append(f"{prefix}.value is required for {operator}")
                if operator in {"truthy", "falsy"} and "value" in item:
                    errors.append(f"{prefix}.value must be omitted for {operator}")
                if operator in {"in", "not_in", "between"} and "value" in item:
                    if not isinstance(expected, list):
                        errors.append(f"{prefix}.value must be an array for {operator}")
                    else:
                        if operator in {"in", "not_in"} and not expected:
                            errors.append(f"{prefix}.value must not be empty for {operator}")
                        if operator == "between" and len(expected) != 2:
                            errors.append(f"{prefix}.value must contain two values for between")
                        if operator in {"in", "not_in"}:
                            for value_index, value in enumerate(expected):
                                _collect_error(
                                    errors,
                                    lambda value=value, value_index=value_index, prefix=prefix: validate_filter_scalar(
                                        value, f"{prefix}.value[{value_index}]"
                                    ),
                                )
                        elif len(expected) == 2:
                            lower = _collect_error(
                                errors,
                                lambda: finite_number(expected[0], f"{prefix}.value[0]"),
                            )
                            upper = _collect_error(
                                errors,
                                lambda: finite_number(expected[1], f"{prefix}.value[1]"),
                            )
                            if lower is not None and upper is not None and lower > upper:
                                errors.append(f"{prefix}.value lower bound exceeds upper bound")
                elif operator in {"eq", "neq"} and "value" in item:
                    _collect_error(
                        errors,
                        lambda: validate_filter_scalar(expected, f"{prefix}.value"),
                    )
                elif operator in {"gte", "lte"} and "value" in item:
                    _collect_error(
                        errors,
                        lambda: finite_number(expected, f"{prefix}.value"),
                    )
                if (
                    field in STRING_COLUMNS
                    and operator in {"gte", "lte", "between", "truthy", "falsy"}
                ):
                    errors.append(
                        f"{prefix} cannot use {operator} on required string field {field}"
                    )
                if (
                    field in STRING_COLUMNS
                    and operator in {"eq", "neq"}
                    and "value" in item
                    and not isinstance(expected, str)
                ):
                    errors.append(f"{prefix}.value must be a string for field {field}")
                if (
                    field in STRING_COLUMNS
                    and operator in {"in", "not_in"}
                    and isinstance(expected, list)
                    and any(not isinstance(value, str) for value in expected)
                ):
                    errors.append(f"{prefix}.value entries must be strings for field {field}")
            missing = item.get("missing", "review")
            if (
                not isinstance(missing, str)
                or missing not in VALID_MISSING_POLICIES
            ):
                errors.append(f"{prefix}.missing must be exclude or review")
            cohorts = item.get("cohorts")
            if cohorts is not None:
                if not isinstance(cohorts, list) or not cohorts:
                    errors.append(f"{prefix}.cohorts must be a non-empty array")
                else:
                    seen_cohorts: set[str] = set()
                    for cohort_index, value in enumerate(cohorts):
                        cohort = _collect_error(
                            errors,
                            lambda value=value, cohort_index=cohort_index, prefix=prefix: nonempty_string(
                                value, f"{prefix}.cohorts[{cohort_index}]"
                            ),
                        )
                        if cohort is not None and cohort in seen_cohorts:
                            errors.append(f"{prefix}.cohorts contains duplicate: {cohort}")
                        if cohort is not None:
                            seen_cohorts.add(cohort)

    scorecards = config.get("scorecards")
    if not isinstance(scorecards, dict) or not scorecards:
        errors.append("config.scorecards must be a non-empty object")
    if isinstance(scorecards, dict):
        seen_scorecard_cohorts: set[str] = set()
        for raw_cohort, metrics in scorecards.items():
            cohort = _collect_error(
                errors,
                lambda raw_cohort=raw_cohort: nonempty_string(
                    raw_cohort, "config.scorecards cohort"
                ),
            )
            label = cohort or str(raw_cohort)
            if cohort is not None:
                if cohort in seen_scorecard_cohorts:
                    errors.append(
                        f"duplicate scorecard cohort after trimming: {cohort}"
                    )
                seen_scorecard_cohorts.add(cohort)
            if not isinstance(metrics, list) or not metrics:
                errors.append(f"config.scorecards.{label} must be a non-empty array")
                continue
            seen_fields: set[str] = set()
            parsed_weights: list[float] = []
            for index, item in enumerate(metrics):
                prefix = f"config.scorecards.{label}[{index}]"
                if not isinstance(item, dict):
                    errors.append(f"{prefix} must be an object")
                    continue
                _collect_error(
                    errors,
                    lambda item=item, prefix=prefix: reject_unknown_keys(
                        item,
                        {"field", "direction", "weight", "valid_min", "valid_max"},
                        prefix,
                    ),
                )
                field = _collect_error(
                    errors,
                    lambda item=item, prefix=prefix: nonempty_string(
                        item.get("field"), f"{prefix}.field"
                    ),
                )
                if field is not None:
                    if field in seen_fields:
                        errors.append(f"duplicate score field in cohort {label}: {field}")
                    seen_fields.add(field)
                direction = _collect_error(
                    errors,
                    lambda item=item, prefix=prefix: nonempty_string(
                        item.get("direction"), f"{prefix}.direction"
                    ),
                )
                if direction is not None and direction not in VALID_DIRECTIONS:
                    errors.append(f"invalid direction for {label}.{field}: {direction}")
                weight = _collect_error(
                    errors,
                    lambda item=item, prefix=prefix: finite_number(
                        item.get("weight"), f"{prefix}.weight"
                    ),
                )
                if weight is not None and weight <= 0:
                    errors.append(f"{prefix}.weight must be positive")
                elif weight is not None:
                    parsed_weights.append(weight)
                bounds: dict[str, float] = {}
                for bound in ("valid_min", "valid_max"):
                    if item.get(bound) is not None:
                        parsed = _collect_error(
                            errors,
                            lambda item=item, prefix=prefix, bound=bound: finite_number(
                                item[bound], f"{prefix}.{bound}"
                            ),
                        )
                        if parsed is not None:
                            bounds[bound] = parsed
                if (
                    "valid_min" in bounds
                    and "valid_max" in bounds
                    and bounds["valid_min"] > bounds["valid_max"]
                ):
                    errors.append(f"{prefix}.valid_min exceeds valid_max")
            if parsed_weights and not math.isfinite(sum(parsed_weights)):
                errors.append(
                    f"total score weight must be finite for cohort {label}"
                )
        for index, item in enumerate(hard_filters if isinstance(hard_filters, list) else []):
            if not isinstance(item, dict) or not isinstance(item.get("cohorts"), list):
                continue
            unknown = sorted(
                {
                    value.strip()
                    for value in item["cohorts"]
                    if isinstance(value, str) and value.strip()
                }
                - seen_scorecard_cohorts
            )
            if unknown:
                errors.append(
                    f"config.hard_filters[{index}] references unknown scorecard "
                    f"cohort(s): {', '.join(unknown)}"
                )
    raw_settings = config.get("cohort_settings", {})
    if not isinstance(raw_settings, dict):
        errors.append("config.cohort_settings must be an object")
    elif isinstance(scorecards, dict):
        known_cohorts = {
            cohort.strip()
            for cohort in scorecards
            if isinstance(cohort, str) and cohort.strip()
        }
        seen_settings: set[str] = set()
        for raw_cohort, setting in raw_settings.items():
            cohort = _collect_error(
                errors,
                lambda raw_cohort=raw_cohort: nonempty_string(
                    raw_cohort, "config.cohort_settings cohort"
                ),
            )
            if cohort is None:
                continue
            prefix = f"config.cohort_settings.{cohort}"
            if cohort in seen_settings:
                errors.append(f"duplicate cohort_settings key after trimming: {cohort}")
            seen_settings.add(cohort)
            if cohort not in known_cohorts:
                errors.append(f"config.cohort_settings contains unknown cohort: {cohort}")
            if not isinstance(setting, dict):
                errors.append(f"{prefix} must be an object")
                continue
            _collect_error(
                errors,
                lambda setting=setting, prefix=prefix: reject_unknown_keys(
                    setting, {"top_n", "minimum_cohort_size"}, prefix
                ),
            )
            if "top_n" in setting:
                _collect_error(
                    errors,
                    lambda setting=setting, prefix=prefix: positive_int(
                        setting["top_n"], f"{prefix}.top_n"
                    ),
                )
            if "minimum_cohort_size" in setting:
                cohort_minimum = _collect_error(
                    errors,
                    lambda setting=setting, prefix=prefix: positive_int(
                        setting["minimum_cohort_size"],
                        f"{prefix}.minimum_cohort_size",
                    ),
                )
                if cohort_minimum is not None and cohort_minimum < 2:
                    errors.append(f"{prefix}.minimum_cohort_size must be at least 2")
    return errors


def collect_row_errors(
    rows: list[dict[str, Any]], manifest: dict[str, Any] | None
) -> list[str]:
    """Validate every CSV row and all independently decidable cross-row rules."""
    errors: list[str] = []
    seen_codes: set[str] = set()
    strategy_rows: dict[str, list[dict[str, Any]]] = defaultdict(list)
    has_invalid_strategy_id = False
    for index, raw in enumerate(rows):
        if not isinstance(raw, dict):
            errors.append(f"row {index + 2} must be an object")
            has_invalid_strategy_id = True
            continue
        row_number = raw.get("_row_number", index + 2)
        prefix = f"row {row_number}"

        raw_code = raw.get("fund_code")
        fund_code = _collect_error(
            errors,
            lambda raw_code=raw_code, prefix=prefix: nonempty_string(
                raw_code, f"{prefix}.fund_code"
            ),
        )
        if fund_code is not None:
            if not re.fullmatch(r"\d{6}", fund_code):
                errors.append(f"{prefix}.fund_code must be a six-digit string")
            else:
                code = fund_code
                if code in seen_codes:
                    errors.append(f"duplicate fund_code: {code}")
                seen_codes.add(code)

        snapshot_day = _collect_error(
            errors,
            lambda raw=raw, prefix=prefix: parse_day(
                raw.get("snapshot_date"), f"{prefix}.snapshot_date"
            ),
        )
        if (
            snapshot_day is not None
            and manifest is not None
            and snapshot_day != manifest["snapshot_date"]
        ):
            errors.append(f"{prefix}.snapshot_date does not match manifest")
        inception = _collect_error(
            errors,
            lambda raw=raw, prefix=prefix: parse_day(
                raw.get("inception_date"), f"{prefix}.inception_date"
            ),
        )
        raw_metrics_as_of = raw.get("metrics_as_of")
        metrics_as_of = None
        if not (
            raw_metrics_as_of is None
            or isinstance(raw_metrics_as_of, str)
            and raw_metrics_as_of.strip().lower() in MISSING_TOKENS
        ):
            metrics_as_of = _collect_error(
                errors,
                lambda raw_metrics_as_of=raw_metrics_as_of, prefix=prefix: parse_day(
                    raw_metrics_as_of, f"{prefix}.metrics_as_of"
                ),
            )
        if (
            inception is not None
            and manifest is not None
            and inception > manifest["snapshot_date"]
        ):
            errors.append(f"{prefix}.inception_date exceeds snapshot_date")
        if (
            metrics_as_of is not None
            and manifest is not None
            and metrics_as_of > manifest["snapshot_date"]
        ):
            errors.append(f"{prefix}.metrics_as_of exceeds snapshot_date")
        if (
            metrics_as_of is not None
            and inception is not None
            and metrics_as_of < inception
        ):
            errors.append(f"{prefix}.metrics_as_of precedes inception_date")

        listed_sources = _collect_error(
            errors,
            lambda raw=raw, prefix=prefix: nonempty_string(
                raw.get("source_ids"), f"{prefix}.source_ids"
            ),
        )
        row_sources: list[str] = []
        if listed_sources is not None:
            row_sources = [
                item.strip() for item in listed_sources.split(";") if item.strip()
            ]
            if not row_sources:
                errors.append(
                    f"{prefix}.source_ids must contain at least one source id"
                )
            if len(row_sources) != len(set(row_sources)):
                errors.append(f"{prefix}.source_ids contains duplicates")
            if manifest is not None:
                unknown_sources = sorted(
                    set(row_sources) - manifest["source_ids"]
                )
                if unknown_sources:
                    errors.append(
                        f"{prefix}.source_ids not found in manifest: "
                        f"{', '.join(unknown_sources)}"
                    )
                known_sources = [
                    source_id
                    for source_id in row_sources
                    if source_id in manifest["source_ids"]
                ]
                if (
                    manifest["scope"] == "all-market"
                    and not (
                        set(known_sources)
                        & manifest["enumeration_source_ids"]
                    )
                ):
                    errors.append(
                        f"{prefix}.source_ids does not include a "
                        "universe-enumeration source"
                    )
                if not any(
                    manifest["source_roles"][source_id]
                    & {"identity", "universe-enumeration"}
                    for source_id in known_sources
                ):
                    errors.append(
                        f"{prefix}.source_ids does not include an identity source"
                    )
                if metrics_as_of is not None:
                    metric_sources = [
                        source_id
                        for source_id in known_sources
                        if "metrics" in manifest["source_roles"][source_id]
                    ]
                    if not metric_sources:
                        errors.append(
                            f"{prefix}.source_ids does not include a metrics source"
                        )
                    elif metrics_as_of > max(
                        manifest["source_as_of"][source_id]
                        for source_id in metric_sources
                    ):
                        errors.append(
                            f"{prefix}.metrics_as_of exceeds its cited metrics "
                            "source date"
                        )

        for field in (
            "fund_name",
            "share_class",
            "cohort",
            "category",
            "currency",
            "status",
        ):
            _collect_error(
                errors,
                lambda raw=raw, prefix=prefix, field=field: nonempty_string(
                    raw.get(field), f"{prefix}.{field}"
                ),
            )

        ranking_share = _collect_error(
            errors,
            lambda raw=raw, prefix=prefix: parse_bool(
                raw.get("ranking_share"), f"{prefix}.ranking_share"
            ),
        )
        strategy_id = _collect_error(
            errors,
            lambda raw=raw, prefix=prefix: nonempty_string(
                raw.get("strategy_id"), f"{prefix}.strategy_id"
            ),
        )
        if strategy_id is None:
            has_invalid_strategy_id = True
        else:
            comparable: dict[str, str] = {}
            for field in ("cohort", "category", "currency"):
                raw_value = raw.get(field)
                if isinstance(raw_value, str) and raw_value.strip():
                    value = raw_value.strip()
                    comparable[field] = value.upper() if field == "currency" else value
            strategy_rows[strategy_id].append(
                {"ranking_share": ranking_share, **comparable}
            )

    for strategy_id, members in strategy_rows.items():
        known_rankings = [
            item["ranking_share"]
            for item in members
            if item["ranking_share"] is not None
        ]
        ranking_count = sum(known_rankings)
        if ranking_count > 1 or (
            len(known_rankings) == len(members) and ranking_count != 1
        ):
            errors.append(
                f"strategy {strategy_id} must have exactly one ranking_share=true row"
            )
        for field in ("cohort", "category", "currency"):
            values = {item[field] for item in members if field in item}
            if len(values) > 1:
                errors.append(
                    f"strategy {strategy_id} has inconsistent {field} values"
                )

    expected_shares = (
        manifest.get("expected_share_count") if manifest is not None else None
    )
    if expected_shares is not None and len(rows) != expected_shares:
        errors.append(
            f"manifest expected_share_count={expected_shares}, "
            f"universe has {len(rows)} row(s)"
        )
    expected_strategies = (
        manifest.get("expected_strategy_count") if manifest is not None else None
    )
    if expected_strategies is not None and (
        len(strategy_rows) > expected_strategies
        or (
            not has_invalid_strategy_id
            and len(strategy_rows) != expected_strategies
        )
    ):
        errors.append(
            f"manifest expected_strategy_count={expected_strategies}, "
            f"universe has {len(strategy_rows)} strategy/strategies"
        )
    if not errors and manifest is not None:
        try:
            normalize_rows(rows, manifest)
        except ValueError as exc:
            errors.append(str(exc))
    return errors


def collect_analysis_input_errors(
    rows: list[dict[str, Any]], manifest_raw: dict[str, Any], config_raw: dict[str, Any]
) -> tuple[dict[str, Any] | None, dict[str, Any] | None, list[str]]:
    errors = [
        *collect_manifest_source_errors(manifest_raw),
        *collect_config_item_errors(config_raw),
    ]
    manifest = _collect_error(errors, lambda: validate_manifest(manifest_raw))
    config = _collect_error(errors, lambda: validate_config(config_raw))
    errors.extend(collect_row_errors(rows, manifest))
    return manifest, config, list(dict.fromkeys(errors))


def validate_analysis_inputs(
    rows: list[dict[str, Any]], manifest_raw: dict[str, Any], config_raw: dict[str, Any]
) -> tuple[dict[str, Any], dict[str, Any]]:
    manifest, config, errors = collect_analysis_input_errors(
        rows, manifest_raw, config_raw
    )
    if errors:
        raise validation_error(errors)
    assert manifest is not None and config is not None
    return manifest, config


def verify_source_snapshots(manifest: dict[str, Any], manifest_dir: Path) -> set[Path]:
    verified_paths: set[Path] = set()
    errors: list[str] = []
    sources = manifest.get("sources")
    if not isinstance(sources, list):
        return verified_paths
    for source in sources:
        if not isinstance(source, dict):
            continue
        source_id = source.get("id")
        snapshot_file = source.get("snapshot_file")
        digest = source.get("content_sha256")
        if (
            not isinstance(source_id, str)
            or not source_id.strip()
            or not isinstance(snapshot_file, str)
            or not snapshot_file.strip()
            or not isinstance(digest, str)
            or re.fullmatch(r"[0-9a-fA-F]{64}", digest.strip()) is None
        ):
            continue
        source_id = source_id.strip()
        expected_digest = digest.strip().lower()
        snapshot_path = Path(snapshot_file).expanduser()
        if not snapshot_path.is_absolute():
            snapshot_path = manifest_dir / snapshot_path
        snapshot_path = snapshot_path.resolve()
        if not snapshot_path.is_file():
            errors.append(
                f"source snapshot file not found for {source_id}: {snapshot_path}"
            )
            continue
        try:
            actual = sha256_file(snapshot_path)
        except OSError as exc:
            errors.append(
                f"source snapshot file cannot be read for {source_id}: "
                f"{snapshot_path}: {exc}"
            )
            continue
        if actual != expected_digest:
            errors.append(
                f"source snapshot SHA-256 mismatch for {source_id}: "
                f"expected {expected_digest}, got {actual}"
            )
            continue
        verified_paths.add(snapshot_path)
    if errors:
        raise validation_error(errors)
    return verified_paths


def read_universe(
    path: Path, *, return_errors: bool = False
) -> list[dict[str, Any]] | tuple[list[dict[str, Any]], list[str]]:
    with path.open("r", encoding="utf-8-sig", newline="") as handle:
        reader = csv.DictReader(handle)
        if reader.fieldnames is None:
            raise ValueError("universe CSV has no header")
        if any(name is None or not name.strip() or name != name.strip() for name in reader.fieldnames):
            raise ValueError("CSV headers must be non-empty and must not have surrounding whitespace")
        duplicate_headers = sorted({name for name in reader.fieldnames if reader.fieldnames.count(name) > 1})
        if duplicate_headers:
            raise ValueError(f"duplicate CSV column(s): {', '.join(duplicate_headers)}")
        missing = [name for name in REQUIRED_COLUMNS if name not in reader.fieldnames]
        if missing:
            raise ValueError(f"missing CSV column(s): {', '.join(missing)}")
        rows: list[dict[str, Any]] = []
        errors: list[str] = []
        for row_number, raw in enumerate(reader, start=2):
            if None in raw:
                errors.append(
                    f"row {row_number}: contains more values than the CSV header"
                )
            if all(
                not (value or "").strip()
                for key, value in raw.items()
                if key is not None
            ):
                continue
            row = {
                key: (value.strip() if key in STRING_COLUMNS and isinstance(value, str) else scalar(value))
                for key, value in raw.items()
                if key is not None
            }
            row["_row_number"] = row_number
            rows.append(row)
    if errors and not return_errors:
        raise validation_error(errors)
    if not rows:
        if return_errors:
            errors.append("universe CSV contains no data rows")
            return rows, errors
        raise ValueError("universe CSV contains no data rows")
    if return_errors:
        return rows, errors
    return rows


def normalize_rows(
    rows: list[dict[str, Any]], manifest: dict[str, Any]
) -> tuple[list[dict[str, Any]], dict[str, str]]:
    snapshot = manifest["snapshot_date"]
    source_ids = manifest["source_ids"]
    enumeration_source_ids = manifest["enumeration_source_ids"]
    source_roles = manifest["source_roles"]
    source_as_of = manifest["source_as_of"]
    normalized: list[dict[str, Any]] = []
    seen_codes: set[str] = set()
    strategies: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for index, raw in enumerate(rows):
        row_number = raw.get("_row_number", index + 2)
        prefix = f"row {row_number}"
        fund_code = nonempty_string(raw.get("fund_code"), f"{prefix}.fund_code")
        if not re.fullmatch(r"\d{6}", fund_code):
            raise ValueError(f"{prefix}.fund_code must be a six-digit string")
        if fund_code in seen_codes:
            raise ValueError(f"duplicate fund_code: {fund_code}")
        seen_codes.add(fund_code)
        snapshot_day = parse_day(raw.get("snapshot_date"), f"{prefix}.snapshot_date")
        if snapshot_day != snapshot:
            raise ValueError(f"{prefix}.snapshot_date does not match manifest")
        inception = parse_day(raw.get("inception_date"), f"{prefix}.inception_date")
        raw_metrics_as_of = raw.get("metrics_as_of")
        metrics_as_of = None
        if not (
            raw_metrics_as_of is None
            or isinstance(raw_metrics_as_of, str)
            and raw_metrics_as_of.strip().lower() in MISSING_TOKENS
        ):
            metrics_as_of = parse_day(raw_metrics_as_of, f"{prefix}.metrics_as_of")
        if inception > snapshot:
            raise ValueError(f"{prefix}.inception_date exceeds snapshot_date")
        if metrics_as_of is not None and metrics_as_of > snapshot:
            raise ValueError(f"{prefix}.metrics_as_of exceeds snapshot_date")
        if metrics_as_of is not None and metrics_as_of < inception:
            raise ValueError(f"{prefix}.metrics_as_of precedes inception_date")
        listed_sources = nonempty_string(raw.get("source_ids"), f"{prefix}.source_ids")
        row_sources = [item.strip() for item in listed_sources.split(";") if item.strip()]
        if not row_sources:
            raise ValueError(f"{prefix}.source_ids must contain at least one source id")
        if len(row_sources) != len(set(row_sources)):
            raise ValueError(f"{prefix}.source_ids contains duplicates")
        unknown_sources = sorted(set(row_sources) - source_ids)
        if unknown_sources:
            raise ValueError(f"{prefix}.source_ids not found in manifest: {', '.join(unknown_sources)}")
        if manifest["scope"] == "all-market" and not (set(row_sources) & enumeration_source_ids):
            raise ValueError(f"{prefix}.source_ids does not include a universe-enumeration source")
        if not any(source_roles[source_id] & {"identity", "universe-enumeration"} for source_id in row_sources):
            raise ValueError(f"{prefix}.source_ids does not include an identity source")
        if metrics_as_of is not None:
            metric_sources = [
                source_id for source_id in row_sources if "metrics" in source_roles[source_id]
            ]
            if not metric_sources:
                raise ValueError(f"{prefix}.source_ids does not include a metrics source")
            if metrics_as_of > max(source_as_of[source_id] for source_id in metric_sources):
                raise ValueError(f"{prefix}.metrics_as_of exceeds its cited metrics source date")
        ranking_share = parse_bool(raw.get("ranking_share"), f"{prefix}.ranking_share")
        strategy_id = nonempty_string(raw.get("strategy_id"), f"{prefix}.strategy_id")
        row = dict(raw)
        row.update(
            {
                "fund_code": fund_code,
                "fund_name": nonempty_string(raw.get("fund_name"), f"{prefix}.fund_name"),
                "share_class": nonempty_string(raw.get("share_class"), f"{prefix}.share_class"),
                "strategy_id": strategy_id,
                "ranking_share": ranking_share,
                "cohort": nonempty_string(raw.get("cohort"), f"{prefix}.cohort"),
                "category": nonempty_string(raw.get("category"), f"{prefix}.category"),
                "currency": nonempty_string(raw.get("currency"), f"{prefix}.currency").upper(),
                "status": nonempty_string(raw.get("status"), f"{prefix}.status"),
                "snapshot_date": snapshot.isoformat(),
                "inception_date": inception.isoformat(),
                "metrics_as_of": metrics_as_of.isoformat() if metrics_as_of else None,
                "source_ids": row_sources,
                "history_years": (snapshot - inception).days / 365.2425,
                "metric_age_days": (snapshot - metrics_as_of).days if metrics_as_of else None,
            }
        )
        row.pop("_row_number", None)
        normalized.append(row)
        strategies[strategy_id].append(row)
    ranking_codes: dict[str, str] = {}
    for strategy_id, members in strategies.items():
        ranking = [item for item in members if item["ranking_share"]]
        if len(ranking) != 1:
            raise ValueError(f"strategy {strategy_id} must have exactly one ranking_share=true row")
        comparable_fields = ("cohort", "category", "currency")
        for field in comparable_fields:
            values = {item[field] for item in members}
            if len(values) != 1:
                raise ValueError(f"strategy {strategy_id} has inconsistent {field} values")
        ranking_codes[strategy_id] = ranking[0]["fund_code"]
        for item in members:
            item["share_count_in_strategy"] = len(members)
            item["strategy_ranking_code"] = ranking[0]["fund_code"]
    expected_shares = manifest["expected_share_count"]
    expected_strategies = manifest["expected_strategy_count"]
    if expected_shares is not None and len(normalized) != expected_shares:
        raise ValueError(
            f"manifest expected_share_count={expected_shares}, normalized universe has {len(normalized)}"
        )
    if expected_strategies is not None and len(strategies) != expected_strategies:
        raise ValueError(
            f"manifest expected_strategy_count={expected_strategies}, normalized universe has {len(strategies)}"
        )
    return normalized, ranking_codes


def equal_value(actual: Any, expected: Any, field: str) -> bool:
    if field in STRING_COLUMNS:
        return actual == expected
    if isinstance(expected, (int, float)) and not isinstance(expected, bool):
        return finite_number(actual, field) == float(expected)
    return actual == expected


def compare(actual: Any, operator: str, expected: Any, field: str) -> bool:
    if operator == "eq":
        return equal_value(actual, expected, field)
    if operator == "neq":
        return not equal_value(actual, expected, field)
    if operator == "in":
        return any(equal_value(actual, item, field) for item in expected)
    if operator == "not_in":
        return not any(equal_value(actual, item, field) for item in expected)
    if operator == "truthy":
        if not isinstance(actual, bool):
            raise ValueError(f"{field} must be boolean for truthy filter")
        return actual
    if operator == "falsy":
        if not isinstance(actual, bool):
            raise ValueError(f"{field} must be boolean for falsy filter")
        return not actual
    actual_number = finite_number(actual, field)
    if operator == "gte":
        return actual_number >= finite_number(expected, f"filter value for {field}")
    if operator == "lte":
        return actual_number <= finite_number(expected, f"filter value for {field}")
    if operator == "between":
        lower = finite_number(expected[0], f"filter lower value for {field}")
        upper = finite_number(expected[1], f"filter upper value for {field}")
        if lower > upper:
            raise ValueError(f"filter lower value exceeds upper value for {field}")
        return lower <= actual_number <= upper
    raise ValueError(f"unsupported operator: {operator}")


def percentile_table(values: list[float], direction: str) -> dict[float, float]:
    transformed = [abs(item) if direction == "lower_abs" else item for item in values]
    if len(transformed) == 1:
        return {transformed[0]: 50.0}
    counts = Counter(transformed)
    result: dict[float, float] = {}
    lower_count = 0
    total = len(transformed)
    for target in sorted(counts):
        equal = counts[target]
        better = lower_count if direction == "higher" else total - lower_count - equal
        result[target] = (better + (equal - 1) / 2) / (total - 1) * 100.0
        lower_count += equal
    return result


def analyze(
    rows: list[dict[str, Any]], manifest_raw: dict[str, Any], config_raw: dict[str, Any]
) -> dict[str, Any]:
    if not isinstance(rows, list) or not rows or any(not isinstance(row, dict) for row in rows):
        raise ValueError("rows must be a non-empty array of objects")
    if not isinstance(manifest_raw, dict) or not isinstance(config_raw, dict):
        raise ValueError("manifest and config must be objects")
    manifest, config = validate_analysis_inputs(rows, manifest_raw, config_raw)
    normalized, ranking_codes = normalize_rows(rows, manifest)
    results: dict[str, dict[str, Any]] = {}
    eligible_by_cohort: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for row in normalized:
        code = row["fund_code"]
        result = {
            "fund_code": code,
            "fund_name": row["fund_name"],
            "share_class": row["share_class"],
            "strategy_id": row["strategy_id"],
            "strategy_ranking_code": ranking_codes[row["strategy_id"]],
            "cohort": row["cohort"],
            "status": "pending",
            "reasons": [],
            "score": None,
            "rank_in_cohort": None,
            "shortlisted": False,
            "metric_percentiles": {},
            "data": row,
        }
        results[code] = result
        if not row["ranking_share"]:
            result["status"] = "non_ranking_share"
            result["reasons"].append("同策略非排名份额；保留供后续 A/C/场内外选择")
            continue
        hard_failures: list[str] = []
        review_reasons: list[str] = []
        if row["metrics_as_of"] is None:
            review_reasons.append("缺少 metrics_as_of，不能进入排名")
        for item in config["hard_filters"]:
            if item["cohorts"] is not None and row["cohort"] not in item["cohorts"]:
                continue
            actual = row.get(item["field"])
            if actual is None:
                reason = f"{item['id']}: 缺少 {item['field']}"
                (hard_failures if item["missing"] == "exclude" else review_reasons).append(reason)
                continue
            try:
                passed = compare(actual, item["op"], item["value"], item["field"])
            except ValueError as exc:
                review_reasons.append(f"{item['id']}: {exc}")
                continue
            if not passed:
                hard_failures.append(
                    f"{item['id']}: {item['field']}={actual!r} 未满足 {item['op']} {item['value']!r}"
                )
        if hard_failures:
            result["status"] = "hard_filter_fail"
            result["reasons"].extend(hard_failures + review_reasons)
            continue
        if review_reasons:
            result["status"] = "insufficient_data"
            result["reasons"].extend(review_reasons)
            continue
        scorecard = config["scorecards"].get(row["cohort"])
        if scorecard is None:
            result["status"] = "no_scorecard"
            result["reasons"].append("该 cohort 未配置评分卡")
            continue
        missing_metrics = [item["field"] for item in scorecard if row.get(item["field"]) is None]
        if missing_metrics:
            result["status"] = "insufficient_data"
            result["reasons"].append(f"缺少评分指标: {', '.join(missing_metrics)}")
            continue
        bad_metrics: list[str] = []
        out_of_range_metrics: list[str] = []
        for item in scorecard:
            try:
                value = finite_number(row[item["field"]], item["field"])
            except ValueError:
                bad_metrics.append(item["field"])
                continue
            if item["valid_min"] is not None and value < item["valid_min"]:
                out_of_range_metrics.append(item["field"])
            elif item["valid_max"] is not None and value > item["valid_max"]:
                out_of_range_metrics.append(item["field"])
        if bad_metrics:
            result["status"] = "insufficient_data"
            result["reasons"].append(f"评分指标不是有限数值: {', '.join(bad_metrics)}")
            continue
        if out_of_range_metrics:
            result["status"] = "insufficient_data"
            result["reasons"].append(
                f"评分指标超出配置的有效范围: {', '.join(out_of_range_metrics)}"
            )
            continue
        eligible_by_cohort[row["cohort"]].append(row)

    for cohort, eligible in eligible_by_cohort.items():
        settings = config["cohort_settings"][cohort]
        if len(eligible) < settings["minimum_cohort_size"]:
            for row in eligible:
                result = results[row["fund_code"]]
                result["status"] = "small_cohort"
                result["reasons"].append(
                    f"可比样本仅 {len(eligible)}，低于 minimum_cohort_size={settings['minimum_cohort_size']}"
                )
            continue
        scorecard = config["scorecards"][cohort]
        percentile_tables: dict[str, dict[float, float]] = {}
        for metric in scorecard:
            field = metric["field"]
            values = [finite_number(item[field], field) for item in eligible]
            percentile_tables[field] = percentile_table(values, metric["direction"])
        for row in eligible:
            result = results[row["fund_code"]]
            total = 0.0
            for metric in scorecard:
                field = metric["field"]
                value = finite_number(row[field], field)
                lookup = abs(value) if metric["direction"] == "lower_abs" else value
                rank_score = percentile_tables[field][lookup]
                result["metric_percentiles"][field] = rank_score
                total += rank_score * metric["normalized_weight"]
            result["score"] = total
            result["status"] = "ranked"
        ordered = sorted(
            (results[row["fund_code"]] for row in eligible),
            key=lambda item: (-float(item["score"]), item["fund_code"]),
        )
        previous_score: float | None = None
        previous_rank = 0
        for index, result in enumerate(ordered, start=1):
            score = float(result["score"])
            rank = previous_rank if previous_score is not None and score == previous_score else index
            result["rank_in_cohort"] = rank
            result["shortlisted"] = rank <= settings["top_n"]
            previous_score = score
            previous_rank = rank

    ordered_results = sorted(
        results.values(), key=lambda item: (item["cohort"], item["rank_in_cohort"] or 10**9, item["fund_code"])
    )
    status_counts = Counter(item["status"] for item in ordered_results)
    share_count_check_performed = manifest["expected_share_count"] is not None
    strategy_count_check_performed = manifest["expected_strategy_count"] is not None
    share_count_matches = (
        len(normalized) == manifest["expected_share_count"]
        if share_count_check_performed
        else None
    )
    strategy_count_matches = (
        len(ranking_codes) == manifest["expected_strategy_count"]
        if strategy_count_check_performed
        else None
    )
    performed_count_checks = [
        value for value in (share_count_matches, strategy_count_matches) if value is not None
    ]
    results_by_cohort: dict[str, list[dict[str, Any]]] = defaultdict(list)
    for item in ordered_results:
        results_by_cohort[item["cohort"]].append(item)
    cohort_summary: dict[str, dict[str, Any]] = {}
    for cohort, members in sorted(results_by_cohort.items()):
        cohort_summary[cohort] = {
            "share_rows": len(members),
            "ranking_strategies": sum(item["data"]["ranking_share"] for item in members),
            "ranked": sum(item["status"] == "ranked" for item in members),
            "shortlisted": sum(item["shortlisted"] for item in members),
            "status_counts": dict(sorted(Counter(item["status"] for item in members).items())),
        }
    return {
        "metadata": {
            "schema_version": 1,
            "universe_name": manifest["universe_name"],
            "scope": manifest["scope"],
            "snapshot_date": manifest["snapshot_date"].isoformat(),
            "share_count": len(normalized),
            "strategy_count": len(ranking_codes),
            "source_count": len(manifest["sources"]),
            "score_method": "weighted within-cohort percentile; ties share rank and cutoff",
            "ratio_unit": "input metrics must use decimals unless the field definition says otherwise",
            "source_snapshot_hashes_verified": False,
        },
        "universe_sources": manifest["sources"],
        "universe_validation": {
            "expected_share_count": manifest["expected_share_count"],
            "actual_share_count": len(normalized),
            "share_count_check_performed": share_count_check_performed,
            "share_count_matches": share_count_matches,
            "expected_strategy_count": manifest["expected_strategy_count"],
            "actual_strategy_count": len(ranking_codes),
            "strategy_count_check_performed": strategy_count_check_performed,
            "strategy_count_matches": strategy_count_matches,
            "all_performed_count_checks_match": (
                all(value is True for value in performed_count_checks)
                if performed_count_checks
                else None
            ),
            "enumeration_source_ids": sorted(manifest["enumeration_source_ids"]),
            "max_enumeration_age_days": manifest["max_enumeration_age_days"],
            "enumeration_age_days": manifest["enumeration_age_days"],
        },
        "screening_config": config,
        "summary": {
            "status_counts": dict(sorted(status_counts.items())),
            "ranked_strategy_count": status_counts.get("ranked", 0),
            "shortlisted_strategy_count": sum(item["shortlisted"] for item in ordered_results),
            "cohorts": cohort_summary,
        },
        "shortlist": {
            cohort: [
                {
                    "fund_code": item["fund_code"],
                    "fund_name": item["fund_name"],
                    "share_class": item["share_class"],
                    "strategy_id": item["strategy_id"],
                    "score": item["score"],
                    "rank_in_cohort": item["rank_in_cohort"],
                }
                for item in members
                if item["shortlisted"]
            ]
            for cohort, members in sorted(results_by_cohort.items())
            if any(item["shortlisted"] for item in members)
        },
        "audit_rows": ordered_results,
        "warnings": [
            "入围名单只是研究队列，不是买入建议。",
            "分数只表示各自可比 cohort 内的相对位置。",
            "非排名份额仍需按持有期、渠道和实际费率另行选择。",
        ],
    }


def run_self_test() -> None:
    today = date.today().isoformat()
    manifest = {
        "schema_version": 1,
        "scope": "all-market",
        "universe_name": "测试公募基金全集",
        "snapshot_date": "2026-07-21",
        "expected_share_count": 4,
        "expected_strategy_count": 3,
        "max_enumeration_age_days": 1,
        "sources": [
            {
                "id": "S1",
                "publisher": "测试发布者",
                "locator": "https://example.invalid/universe",
                "snapshot_file": "raw/test-universe.json",
                "content_sha256": "0" * 64,
                "roles": ["universe-enumeration", "identity", "metrics"],
                "acquired_at": today,
                "as_of": "2026-07-21",
            }
        ],
    }
    config = {
        "schema_version": 1,
        "top_n_per_cohort": 1,
        "minimum_cohort_size": 2,
        "hard_filters": [
            {"id": "active", "field": "status", "op": "eq", "value": "active", "missing": "exclude"},
            {"id": "history", "field": "history_years", "op": "gte", "value": 1, "missing": "review"},
            {
                "id": "other_cohort_only",
                "field": "unused_metric",
                "op": "gte",
                "value": 1,
                "missing": "exclude",
                "cohorts": ["测试空组"],
            },
        ],
        "cohort_settings": {
            "主动权益|CNY|同类基准": {"top_n": 1, "minimum_cohort_size": 2}
        },
        "scorecards": {
            "主动权益|CNY|同类基准": [
                {"field": "sharpe_3y", "direction": "higher", "weight": 0.6},
                {"field": "expense_rate", "direction": "lower", "weight": 0.4},
            ],
            "测试空组": [{"field": "unused_metric", "direction": "higher", "weight": 1.0}],
        },
    }
    common = {
        "snapshot_date": "2026-07-21",
        "cohort": "主动权益|CNY|同类基准",
        "category": "主动股票",
        "currency": "CNY",
        "inception_date": "2020-01-01",
        "metrics_as_of": "2026-07-20",
        "source_ids": "S1",
    }
    rows = [
        {**common, "fund_code": "000001", "fund_name": "甲A", "share_class": "A", "strategy_id": "甲", "ranking_share": "true", "status": "active", "sharpe_3y": 1.2, "expense_rate": 0.008},
        {**common, "fund_code": "000002", "fund_name": "甲C", "share_class": "C", "strategy_id": "甲", "ranking_share": "false", "status": "active", "sharpe_3y": 1.1, "expense_rate": 0.012},
        {**common, "fund_code": "000003", "fund_name": "乙A", "share_class": "A", "strategy_id": "乙", "ranking_share": "true", "status": "active", "sharpe_3y": 0.8, "expense_rate": 0.010},
        {**common, "fund_code": "000004", "fund_name": "丙A", "share_class": "A", "strategy_id": "丙", "ranking_share": "true", "status": "terminated", "sharpe_3y": 2.0, "expense_rate": 0.005},
    ]
    result = analyze(rows, manifest, config)
    assert result["metadata"]["share_count"] == 4
    assert result["metadata"]["strategy_count"] == 3
    assert result["universe_validation"]["all_performed_count_checks_match"] is True
    assert result["shortlist"]["主动权益|CNY|同类基准"][0]["fund_code"] == "000001"
    statuses = {item["fund_code"]: item["status"] for item in result["audit_rows"]}
    assert statuses["000002"] == "non_ranking_share"
    assert statuses["000004"] == "hard_filter_fail"
    assert compare("1.20", "eq", 1.2, "numeric_text")
    assert compare("000001", "eq", "000001", "fund_code")
    assert not compare("000001", "eq", 1, "fund_code")
    assert compare("2", "in", [1, 2, 3], "numeric_text")
    bad_manifest = dict(manifest)
    bad_manifest["expected_share_count"] = 5
    try:
        analyze(rows, bad_manifest, config)
    except ValueError:
        pass
    else:
        raise AssertionError("manifest count mismatch must fail")
    subset_manifest = json.loads(json.dumps(manifest))
    subset_manifest["scope"] = "subset"
    subset_manifest.pop("expected_share_count")
    subset_manifest.pop("expected_strategy_count")
    subset_manifest.pop("max_enumeration_age_days")
    subset_result = analyze(rows, subset_manifest, config)
    assert subset_result["universe_validation"]["share_count_matches"] is None
    assert subset_result["universe_validation"]["strategy_count_matches"] is None
    assert subset_result["universe_validation"]["all_performed_count_checks_match"] is None
    unverifiable_age_manifest = json.loads(json.dumps(subset_manifest))
    unverifiable_age_manifest["sources"][0]["roles"].remove("universe-enumeration")
    unverifiable_age_manifest["max_enumeration_age_days"] = 1
    try:
        analyze(rows, unverifiable_age_manifest, config)
    except ValueError:
        pass
    else:
        raise AssertionError("enumeration age limit without an enumeration source must fail")
    for invalid_json in ('{"schema_version":1,"schema_version":1}', '{"value":NaN}'):
        try:
            parse_json_object(invalid_json, "self-test")
        except ValueError:
            pass
        else:
            raise AssertionError("duplicate keys and non-standard numbers must fail")
    invalid_schema = dict(manifest)
    invalid_schema["schema_version"] = True
    try:
        analyze(rows, invalid_schema, config)
    except ValueError:
        pass
    else:
        raise AssertionError("boolean schema_version must fail")
    invalid_config = dict(config)
    invalid_config["top_n_per_cohrot"] = 1
    try:
        analyze(rows, manifest, invalid_config)
    except ValueError:
        pass
    else:
        raise AssertionError("unknown config keys must fail")
    terminated_manifest = dict(manifest)
    terminated_manifest["expected_share_count"] = 1
    terminated_manifest["expected_strategy_count"] = 1
    terminated_manifest["max_enumeration_age_days"] = 0
    terminated_row = {
        **common,
        "fund_code": "000099",
        "fund_name": "已终止基金",
        "share_class": "A",
        "strategy_id": "已终止",
        "ranking_share": "true",
        "status": "terminated",
        "metrics_as_of": "",
    }
    terminated_result = analyze([terminated_row], terminated_manifest, config)
    assert terminated_result["audit_rows"][0]["status"] == "hard_filter_fail"
    assert terminated_result["audit_rows"][0]["data"]["metrics_as_of"] is None
    bounded_config = json.loads(json.dumps(config))
    bounded_config["scorecards"]["主动权益|CNY|同类基准"][1]["valid_min"] = 0.0
    bad_metric_rows = [dict(item) for item in rows]
    bad_metric_rows[0]["expense_rate"] = -0.01
    bounded_result = analyze(bad_metric_rows, manifest, bounded_config)
    bounded_status = {item["fund_code"]: item["status"] for item in bounded_result["audit_rows"]}
    assert bounded_status["000001"] == "insufficient_data"
    bad_sources = json.loads(json.dumps(manifest))
    bad_sources["sources"].append(
        {
            **bad_sources["sources"][0],
            "id": "S2",
            "publisher": "",
            "locator": "",
        }
    )
    bad_sources["sources"][0]["publisher"] = ""
    try:
        analyze(rows, bad_sources, config)
    except ValueError as exc:
        message = str(exc)
        assert "manifest.sources[0].publisher" in message
        assert "manifest.sources[1].publisher" in message
        assert "manifest.sources[1].locator" in message
    else:
        raise AssertionError("all invalid manifest sources must be reported together")
    invalid_rows_manifest = dict(manifest)
    invalid_rows_manifest["expected_share_count"] = 2
    invalid_rows_manifest["expected_strategy_count"] = 2
    invalid_rows = [
        {**rows[0], "fund_code": "bad-one", "strategy_id": "坏甲"},
        {**rows[2], "fund_code": "bad-two", "strategy_id": "坏乙"},
    ]
    try:
        analyze(invalid_rows, invalid_rows_manifest, config)
    except ValueError as exc:
        message = str(exc)
        assert "row 2.fund_code" in message
        assert "row 3.fund_code" in message
    else:
        raise AssertionError("all invalid universe rows must be reported together")
    print("self-test passed")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Screen a normalized domestic fund universe by comparable cohort.")
    parser.add_argument("universe_csv", nargs="?", type=Path, help="normalized UTF-8 fund universe CSV")
    parser.add_argument("--manifest", type=Path, help="universe manifest JSON")
    parser.add_argument("--config", type=Path, help="screening config JSON")
    parser.add_argument("--output", type=Path, help="write screening JSON outside the skill directory")
    parser.add_argument("--self-test", action="store_true", help="run deterministic built-in tests")
    return parser


def main(argv: Iterable[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    if args.self_test:
        run_self_test()
        return 0
    if not all((args.universe_csv, args.manifest, args.config, args.output)):
        raise ValueError("universe_csv, --manifest, --config, and --output are required")
    paths = [
        args.universe_csv.expanduser().resolve(),
        args.manifest.expanduser().resolve(),
        args.config.expanduser().resolve(),
    ]
    output = args.output.expanduser().resolve()
    if output in paths:
        raise ValueError("--output must differ from every input file")
    try:
        output.relative_to(SKILL_DIR)
    except ValueError:
        pass
    else:
        raise ValueError("--output must be outside the skill directory")
    read_errors: list[str] = []
    universe_result = _collect_labeled_error(
        read_errors,
        "universe CSV",
        lambda: read_universe(paths[0], return_errors=True),
    )
    rows = None
    if universe_result is not None:
        rows, universe_errors = universe_result
        read_errors.extend(f"universe CSV: {error}" for error in universe_errors)
    manifest = _collect_labeled_error(
        read_errors, "manifest", lambda: read_json_object(paths[1], "manifest")
    )
    config = _collect_labeled_error(
        read_errors, "config", lambda: read_json_object(paths[2], "config")
    )
    input_errors = list(read_errors)
    validated_manifest = None
    validated_config = None
    if isinstance(manifest, dict):
        input_errors.extend(collect_manifest_source_errors(manifest))
        validated_manifest = _collect_error(
            input_errors, lambda: validate_manifest(manifest)
        )
    if isinstance(config, dict):
        input_errors.extend(collect_config_item_errors(config))
        validated_config = _collect_error(
            input_errors, lambda: validate_config(config)
        )
    if isinstance(rows, list):
        input_errors.extend(collect_row_errors(rows, validated_manifest))
    source_snapshot_paths: set[Path] = set()
    snapshot_manifest = (
        validated_manifest if validated_manifest is not None else manifest
    )
    if isinstance(snapshot_manifest, dict):
        try:
            source_snapshot_paths = verify_source_snapshots(
                snapshot_manifest, paths[1].parent
            )
        except ValueError as exc:
            input_errors.extend(
                line[2:] if line.startswith("- ") else line
                for line in str(exc).splitlines()[1:]
            )
    if input_errors:
        raise validation_error(input_errors)
    assert (
        isinstance(rows, list)
        and isinstance(manifest, dict)
        and isinstance(config, dict)
        and validated_manifest is not None
        and validated_config is not None
    )
    if output in source_snapshot_paths:
        raise ValueError("--output must differ from every registered source snapshot file")
    result = analyze(rows, manifest, config)
    result["metadata"]["source_snapshot_hashes_verified"] = True
    result["metadata"]["input_sha256"] = {
        "universe_csv": sha256_file(paths[0]),
        "manifest": sha256_file(paths[1]),
        "config": sha256_file(paths[2]),
    }
    payload = json.dumps(result, ensure_ascii=False, indent=2, allow_nan=False)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(payload + "\n", encoding="utf-8")
    print(f"OUTPUT_JSON={output}")
    print(f"SHORTLISTED={result['summary']['shortlisted_strategy_count']}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except (OSError, ValueError, json.JSONDecodeError) as error:
        print(f"error: {error}", file=sys.stderr)
        raise SystemExit(2)
