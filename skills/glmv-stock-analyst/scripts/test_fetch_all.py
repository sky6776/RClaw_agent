import sys
import unittest
from unittest import mock
from urllib.parse import parse_qs, urlsplit

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(errors="replace")

import fetch_all


class FetchReportsTests(unittest.TestCase):
    @mock.patch.object(fetch_all, "_http_json")
    def test_fetch_reports_stops_when_recent_year_has_15_reports(self, http_json):
        http_json.return_value = {
            "data": [self.report("601857", f"TARGET-{i}") for i in range(15)]
        }

        reports = fetch_all.fetch_reports("601857")

        self.assertEqual(http_json.call_count, 1)
        query = parse_qs(urlsplit(http_json.call_args.args[0]).query)
        self.assertEqual(query["code"], ["601857"])
        self.assertNotEqual(query["beginTime"], ["0001-01-01"])
        self.assertNotIn("stockCodes", query)
        self.assertEqual(len(reports), 15)

    @mock.patch.object(fetch_all, "_http_json")
    def test_fetch_reports_expands_history_and_filters_other_stocks(self, http_json):
        http_json.side_effect = [
            {"data": [self.report("601857", "RECENT")]},
            {
                "data": [
                    self.report("601857", "RECENT"),
                    self.report("601857", "OLDER", "2025-01-01"),
                    self.report("002335", "OTHER"),
                ]
            },
        ]

        reports = fetch_all.fetch_reports("601857")

        self.assertEqual(http_json.call_count, 2)
        first_query = parse_qs(urlsplit(http_json.call_args_list[0].args[0]).query)
        second_query = parse_qs(urlsplit(http_json.call_args_list[1].args[0]).query)
        self.assertNotEqual(first_query["beginTime"], ["0001-01-01"])
        self.assertEqual(second_query["beginTime"], ["0001-01-01"])
        self.assertEqual([r["infoCode"] for r in reports], ["RECENT", "OLDER"])
        self.assertTrue(all(r["stockCode"] == "601857" for r in reports))

    @mock.patch.object(fetch_all, "_http_json")
    def test_fetch_reports_keeps_recent_results_when_history_request_fails(
        self, http_json
    ):
        http_json.side_effect = [
            {"data": [self.report("601857", "RECENT")]},
            None,
        ]

        reports = fetch_all.fetch_reports("601857")

        self.assertEqual(http_json.call_count, 2)
        self.assertEqual([r["infoCode"] for r in reports], ["RECENT"])

    def test_report_match_prefers_metadata_and_uses_title_only_for_legacy_rows(self):
        target = self.report("601857", "TARGET")
        target["title"] = "龙头一体化优势持续增强"
        self.assertTrue(fetch_all._report_matches_stock(target, "中国石油", "601857"))

        wrong = self.report("002335", "OTHER")
        wrong["title"] = "中国石油601857深度报告"
        self.assertFalse(fetch_all._report_matches_stock(wrong, "中国石油", "601857"))

        legacy = {"title": "中国石油601857深度报告"}
        self.assertTrue(fetch_all._report_matches_stock(legacy, "中国石油", "601857"))

    @mock.patch.object(fetch_all, "_http_json")
    def test_fetch_reports_skips_malformed_rows(self, http_json):
        target = self.report("601857", "TARGET")
        target["title"] = None
        target["publishDate"] = 20260430
        http_json.side_effect = [
            {"data": [None, "bad-row", target]},
            None,
        ]

        reports = fetch_all.fetch_reports("601857")

        self.assertEqual([r["infoCode"] for r in reports], ["TARGET"])
        self.assertEqual(reports[0]["title"], "")
        self.assertEqual(reports[0]["date"], "20260430")

    @staticmethod
    def report(stock_code, info_code, date="2026-04-30"):
        return {
            "stockCode": stock_code,
            "stockName": "中国石油" if stock_code == "601857" else "其他股票",
            "orgSName": "国信证券",
            "emRatingName": "增持",
            "ratingChange": 3,
            "title": "研报标题",
            "publishDate": f"{date} 00:00:00.000",
            "infoCode": info_code,
        }


if __name__ == "__main__":
    unittest.main()
