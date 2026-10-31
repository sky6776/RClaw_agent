import contextlib
import io
import os
import sys
import tempfile
import types
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


class AutoFetchReportPdfTests(unittest.TestCase):
    """研报 PDF 下载/提取管线：magic 门、尝试命名、桥错误降级。

    回归背景（真机 2026-10-31）：release 构建被 R8 剥离 PdfRuntime 桥方法 →
    15 篇研报逐字相同的 AttributeError；东财接口偶发 984 字节 JS 挑战页冒充
    PDF；downloaded 恒 0 导致 15 篇互相覆盖 report_1.pdf。
    """

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.out_dir = self.tmp.name

    @staticmethod
    def pdf_report(n):
        return {
            "org": "国信证券",
            "title": f"研报{n}",
            "pdf_url": f"https://pdf.dfcfw.com/pdf/report_{n}.pdf",
        }

    @staticmethod
    def fake_response(payload):
        resp = mock.Mock()
        resp.read.return_value = payload
        return resp

    @mock.patch.dict(os.environ, {"RCLAW_ANDROID": "1"})
    @mock.patch.object(fetch_all, "_android_extract_chart_pages")
    @mock.patch("urllib.request.urlopen")
    def test_challenge_page_rejected_and_deleted(self, urlopen, extract):
        urlopen.return_value = self.fake_response(b"<script>var _0x649a='x';</script>" * 10)
        extract.side_effect = AssertionError("magic 门应拦截，不应进入提取")

        pages = fetch_all.auto_fetch_report_pdf([self.pdf_report(1)], self.out_dir)

        self.assertEqual(pages, [])
        extract.assert_not_called()
        rdir = os.path.join(self.out_dir, "research_reports")
        self.assertEqual(os.listdir(rdir), [], "挑战页文件应被删除")

    @mock.patch.dict(os.environ, {"RCLAW_ANDROID": "1"})
    @mock.patch.object(fetch_all, "_android_extract_chart_pages", return_value=[])
    @mock.patch("urllib.request.urlopen")
    def test_success_stops_at_max_reports_with_independent_names(self, urlopen, extract):
        urlopen.return_value = self.fake_response(b"%PDF-1.4\n" + b"x" * 300)
        reports = [self.pdf_report(i) for i in range(1, 6)]

        pages = fetch_all.auto_fetch_report_pdf(reports, self.out_dir, max_reports=2)

        self.assertEqual(pages, [])
        # 提取"成功"计满上限即停；每篇独立命名，失败不互相覆盖。
        self.assertEqual(extract.call_count, 2)
        names = [os.path.basename(c.args[0]) for c in extract.call_args_list]
        self.assertEqual(names, ["report_1.pdf", "report_2.pdf"])

    @mock.patch.dict(os.environ, {"RCLAW_ANDROID": "1"})
    @mock.patch.object(
        fetch_all, "_android_extract_chart_pages", side_effect=RuntimeError("提取失败")
    )
    @mock.patch("urllib.request.urlopen")
    def test_attempts_capped_when_extraction_always_fails(self, urlopen, extract):
        urlopen.return_value = self.fake_response(b"%PDF-1.4\n" + b"x" * 300)
        reports = [self.pdf_report(i) for i in range(1, 6)]

        # 预期内的失败输出不污染测试输出（print→stdout / traceback→stderr）。
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            pages = fetch_all.auto_fetch_report_pdf(reports, self.out_dir, max_reports=2)

        self.assertEqual(pages, [])
        # 成功数恒 0 → 尝试上限接管：4 次（max_attempts = max_reports * 2）即停，
        # 不空跑完整个研报列表。
        self.assertEqual(extract.call_count, 4)
        names = [os.path.basename(c.args[0]) for c in extract.call_args_list]
        self.assertEqual(
            names, ["report_1.pdf", "report_2.pdf", "report_3.pdf", "report_4.pdf"]
        )
        self.assertIn("RuntimeError", stderr.getvalue())

    @mock.patch.dict(os.environ, {"RCLAW_ANDROID": "1"})
    @mock.patch("urllib.request.urlopen")
    def test_bridge_attribute_error_skips_report_and_continues(self, urlopen):
        urlopen.return_value = self.fake_response(b"%PDF-1.4\n" + b"x" * 300)
        bridge = types.ModuleType("rclaw_android_runtime")

        def _boom(*args, **kwargs):
            raise AttributeError(
                "type object 'PdfRuntime' has no attribute 'pageCount'"
            )

        bridge.pdf_page_count = _boom
        bridge.render_pdf_page = _boom
        # 捕获预期内的失败输出（print→stdout / traceback→stderr），既验证
        # 升级提示与栈存在，又不污染测试输出。
        stdout, stderr = io.StringIO(), io.StringIO()
        with mock.patch.dict(sys.modules, {"rclaw_android_runtime": bridge}):
            with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
                pages = fetch_all.auto_fetch_report_pdf(
                    [self.pdf_report(1), self.pdf_report(2)], self.out_dir
                )

        self.assertEqual(pages, [])
        # 单篇桥失败不中断批次：两篇都尝试了。
        self.assertEqual(urlopen.call_count, 2)
        self.assertIn("宿主 PDF 桥不可用", stdout.getvalue())
        self.assertIn("AttributeError", stderr.getvalue())

    def test_leading_junk_before_magic_is_still_pdf(self):
        # PDF 规范允许 %PDF- 前有前导垃圾字节：前 1024 字节内找到即接受。
        pdf = os.path.join(self.out_dir, "lead.pdf")
        with open(pdf, "wb") as f:
            f.write(b"\x00\n garbage" + b"%PDF-1.4\n" + b"x" * 200)
        self.assertTrue(fetch_all._looks_like_pdf(pdf))

    def test_plain_script_is_not_pdf(self):
        pdf = os.path.join(self.out_dir, "fake.pdf")
        with open(pdf, "wb") as f:
            f.write(b"<script>function a(a){}</script>")
        self.assertFalse(fetch_all._looks_like_pdf(pdf))


if __name__ == "__main__":
    unittest.main()
