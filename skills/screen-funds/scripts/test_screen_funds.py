#!/usr/bin/env python3
"""CLI-level regression tests for cross-file preflight aggregation."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import screen_funds


class ScreenFundsPreflightTests(unittest.TestCase):
    def test_invalid_manifest_does_not_hide_independent_row_errors(self) -> None:
        manifest = {
            "schema_version": 1,
            "scope": "subset",
            "universe_name": "test",
            "snapshot_date": screen_funds.date.today().isoformat(),
            "sources": [{"id": "broken"}],
        }
        rows = [
            {
                "fund_code": "bad",
                "fund_name": "",
                "share_class": "",
                "strategy_id": "",
                "ranking_share": "not-bool",
                "cohort": "",
                "category": "",
                "currency": "",
                "status": "",
                "snapshot_date": "not-a-date",
                "inception_date": "not-a-date",
                "metrics_as_of": "not-a-date",
                "source_ids": "",
            }
        ]

        _, _, errors = screen_funds.collect_analysis_input_errors(
            rows, manifest, {}
        )
        message = "\n".join(errors)

        self.assertIn("manifest.sources[0].publisher", message)
        self.assertIn("row 2.fund_code", message)
        self.assertIn("row 2.snapshot_date", message)
        self.assertIn("row 2.fund_name", message)
        self.assertIn("row 2.ranking_share", message)

    def test_row_error_does_not_hide_independent_strategy_errors(self) -> None:
        today = screen_funds.date.today()
        manifest = {
            "scope": "all-market",
            "snapshot_date": today,
            "expected_share_count": 2,
            "expected_strategy_count": 2,
            "source_ids": {"S1"},
            "enumeration_source_ids": {"S1"},
            "source_roles": {
                "S1": {"universe-enumeration", "identity", "metrics"}
            },
            "source_as_of": {"S1": today},
        }
        common = {
            "snapshot_date": today.isoformat(),
            "inception_date": "2020-01-01",
            "metrics_as_of": today.isoformat(),
            "source_ids": "S1",
            "strategy_id": "same-strategy",
            "ranking_share": True,
            "cohort": "cohort-a",
            "category": "category-a",
            "currency": "CNY",
            "share_class": "A",
            "status": "active",
        }
        rows = [
            {
                **common,
                "fund_code": "000001",
                "fund_name": "",
                "inception_date": "not-a-date",
            },
            {**common, "fund_code": "000002", "fund_name": "valid"},
        ]

        message = "\n".join(screen_funds.collect_row_errors(rows, manifest))

        self.assertIn("fund_name", message)
        self.assertIn("inception_date", message)
        self.assertIn("exactly one ranking_share=true", message)
        self.assertIn("expected_strategy_count=2", message)

    def test_parse_failure_does_not_hide_valid_sibling_semantic_errors(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            root = Path(temp_dir)
            universe = root / "universe.csv"
            manifest = root / "manifest.json"
            config = root / "config.json"
            output = root / "result.json"
            universe.write_text("wrong_header\nvalue\n", encoding="utf-8")
            manifest.write_text("{not-json", encoding="utf-8")
            config.write_text(
                json.dumps(
                    {
                        "schema_version": 1,
                        "top_n_per_cohort": 1,
                        "minimum_cohort_size": 2,
                        "hard_filters": [
                            {"id": "", "field": "", "op": "bad"},
                            {"id": "", "field": "", "op": "also-bad"},
                        ],
                        "scorecards": {
                            "测试": [
                                {
                                    "field": "metric",
                                    "direction": "higher",
                                    "weight": 1,
                                }
                            ]
                        },
                    },
                    ensure_ascii=False,
                ),
                encoding="utf-8",
            )
            with self.assertRaises(ValueError) as caught:
                screen_funds.main(
                    [
                        str(universe),
                        "--manifest",
                        str(manifest),
                        "--config",
                        str(config),
                        "--output",
                        str(output),
                    ]
                )
        message = str(caught.exception)
        self.assertIn("missing CSV column", message)
        self.assertIn("manifest", message)
        self.assertIn("config.hard_filters[0]", message)
        self.assertIn("config.hard_filters[1]", message)


if __name__ == "__main__":
    unittest.main()
