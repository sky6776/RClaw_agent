#!/usr/bin/env python3
"""Deterministic tests for GLM report protocol auditing and canonical repair."""

from __future__ import annotations

import json
import unittest

import glm5v_report


def valid_meta() -> dict[str, object]:
    return {
        "投资观点": "中性",
        "行动建议": "观望",
        "风险等级": "中",
        "观点置信度": 0.6,
        "数据截至": "2026-07-31",
        "币种": "CNY",
        "现价": 10.0,
        "目标价": None,
        "目标期限月": None,
        "上行空间_pct": None,
        "估值方法": None,
        "止损位": 9.0,
        "支撑位": [9.5],
        "压力位": [11.0],
        "PE_ttm": 15.0,
        "PB": 2.0,
        "股息率_pct": 1.5,
    }


def response(text: str, finish_reason: str = "stop") -> dict[str, object]:
    return {
        "choices": [
            {
                "message": {"role": "assistant", "content": text},
                "finish_reason": finish_reason,
            }
        ]
    }


class ProtocolAuditTests(unittest.TestCase):
    def test_canonicalizes_fixable_structure_without_another_model_call(self) -> None:
        text = (
            "多余前言\n"
            "<<<META_BEGIN>>>\n"
            + json.dumps(valid_meta(), ensure_ascii=False, indent=2)
            + "\n<<<META_END>>>\n"
            "<<<WEBSUMMARY_BEGIN>>>\n聊天报告\n<<<WEBSUMMARY_END>>>\n"
            "<<<REPORT_BEGIN>>>\n# 完整报告\n<<<REPORT_END>>>"
        )
        raw = response(text)
        meta, webchat, report, valid, issues, finish, canonical = (
            glm5v_report.audit_model_response(raw, "CNY")
        )
        self.assertTrue(valid)
        self.assertEqual(issues, [])
        self.assertEqual(finish, "stop")
        self.assertEqual(webchat, "聊天报告")
        self.assertEqual(report, "# 完整报告")
        self.assertEqual(meta["币种"], "CNY")
        self.assertTrue(canonical.startswith("<<<META_BEGIN>>>"))
        self.assertEqual(
            glm5v_report.extract_message_text(raw),
            canonical,
        )

    def test_length_finish_never_becomes_valid_by_adding_markers(self) -> None:
        canonical = glm5v_report.canonical_response(
            valid_meta(), "聊天报告", "# 完整报告"
        )
        _, _, _, valid, issues, finish, _ = glm5v_report.audit_model_response(
            response(canonical, "length"), "CNY"
        )
        self.assertFalse(valid)
        self.assertEqual(finish, "length")
        self.assertTrue(any("长度上限" in issue for issue in issues))

    def test_invalid_meta_is_not_replaced_with_normalized_defaults(self) -> None:
        invalid_meta = valid_meta()
        invalid_meta.pop("现价")
        original = glm5v_report.canonical_response(
            invalid_meta, "聊天报告", "# 完整报告"
        )
        raw = response(original)

        _, _, _, valid, issues, _, audited = glm5v_report.audit_model_response(
            raw, "CNY"
        )

        self.assertFalse(valid)
        self.assertTrue(any(issue.startswith("META:") for issue in issues))
        self.assertEqual(audited, original)
        self.assertEqual(glm5v_report.extract_message_text(raw), original)

    def test_duplicate_content_marker_is_not_silently_discarded(self) -> None:
        original = glm5v_report.canonical_response(
            valid_meta(), "聊天报告", "# 完整报告"
        ).replace(
            "<<<REPORT_END>>>",
            "<<<REPORT_END>>>\n<<<REPORT_BEGIN>>>\n# 另一份报告\n<<<REPORT_END>>>",
        )
        raw = response(original)

        _, _, _, valid, issues, _, audited = glm5v_report.audit_model_response(
            raw, "CNY"
        )

        self.assertFalse(valid)
        self.assertTrue(any("REPORT_BEGIN" in issue for issue in issues))
        self.assertEqual(audited, original)
        self.assertEqual(glm5v_report.extract_message_text(raw), original)

    def test_impossible_meta_date_is_rejected(self) -> None:
        invalid_meta = valid_meta()
        invalid_meta["数据截至"] = "2026-02-31"

        _, valid, issues = glm5v_report.validate_meta_with_issues(
            invalid_meta, "CNY"
        )

        self.assertFalse(valid)
        self.assertTrue(any("实际日期" in issue for issue in issues))

    def test_all_protocol_issues_are_returned_for_repair(self) -> None:
        _, _, _, valid, issues = glm5v_report.parse_response_with_issues(
            "<<<META_BEGIN>>>\n{}\n<<<META_END>>>", "CNY"
        )
        self.assertFalse(valid)
        joined = "\n".join(issues)
        self.assertIn("WEBSUMMARY", joined)
        self.assertIn("REPORT", joined)
        self.assertIn("META:", joined)

    def test_repair_prompt_has_no_fixed_attempt_limit(self) -> None:
        prompt = glm5v_report.build_repair_prompt("原任务", ["问题一", "问题二"])
        self.assertIn("问题一", prompt)
        self.assertIn("问题二", prompt)
        self.assertNotIn("最多", prompt)

    def test_repair_progress_requires_issue_count_to_decrease(self) -> None:
        previous = None
        stalled_rounds = 0
        previous, stalled_rounds, stop = glm5v_report.repair_progress(
            previous, stalled_rounds, ["a", "b", "c"]
        )
        self.assertFalse(stop)
        previous, stalled_rounds, stop = glm5v_report.repair_progress(
            previous, stalled_rounds, ["x", "y", "z"]
        )
        self.assertFalse(stop)
        previous, stalled_rounds, stop = glm5v_report.repair_progress(
            previous, stalled_rounds, ["new-1", "new-2", "new-3"]
        )
        self.assertTrue(stop)

        previous, stalled_rounds, stop = glm5v_report.repair_progress(
            previous, stalled_rounds, ["one", "two"]
        )
        self.assertFalse(stop)
        self.assertEqual(stalled_rounds, 0)


if __name__ == "__main__":
    unittest.main()
