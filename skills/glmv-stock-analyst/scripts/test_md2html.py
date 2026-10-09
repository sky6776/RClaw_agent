import contextlib
import importlib
import io
import json
import os
import sys
import tempfile
import unittest
from datetime import datetime
from pathlib import Path
from unittest import mock


# 命名与发布测试不依赖 HTML 渲染；最小环境缺少可选渲染包时只在导入期间补桩。
stubbed_modules = []
for optional_module in ("bleach", "markdown", "PIL", "PIL.Image"):
    try:
        importlib.import_module(optional_module)
    except ModuleNotFoundError:
        sys.modules[optional_module] = mock.MagicMock()
        stubbed_modules.append(optional_module)

import md2html

for optional_module in reversed(stubbed_modules):
    sys.modules.pop(optional_module, None)


class Md2HtmlTests(unittest.TestCase):
    def setUp(self):
        self.temp_dir = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp_dir.cleanup)
        self.root = Path(self.temp_dir.name)

    def make_report(self, task_dir, summary, content="# report\n"):
        report_dir = self.root / task_dir
        report_dir.mkdir()
        report_path = report_dir / "report.md"
        report_path.write_text(content, encoding="utf-8")
        (report_dir / "summary.json").write_text(
            json.dumps(summary, ensure_ascii=False), encoding="utf-8"
        )
        return report_path

    def publish(self, source, channel, recv_name, expect_ext):
        channel_flag = channel.removesuffix("qq").upper()
        with mock.patch.dict(
            os.environ,
            {
                f"RCLAW_{channel_flag}_ENABLED": "1",
                "RCLAW_MEDIA_DIR": str(self.root / "media"),
                "RCLAW_SOURCE_CHANNEL": channel,
            },
            clear=False,
        ):
            marker = md2html._publish_media_file(
                str(source),
                channel,
                caption="详细报告",
                recv_name=recv_name,
                expect_ext=expect_ext,
            )
        self.assertIsNotNone(marker)
        return marker

    def test_names_include_stock_identity_and_second_precision_generation_time(self):
        report = self.make_report(
            "601857_20260723_2018",
            {"stock_name": "中国石油", "stock_code": "601857"},
        )

        names = md2html.report_attachment_names(
            str(report), datetime(2026, 7, 23, 20, 18, 55)
        )

        self.assertEqual(
            names,
            (
                "中国石油_601857_20260723_201855.html",
                "中国石油_601857_20260723_201855.md",
            ),
        )

    def test_names_are_cross_platform_safe_and_fall_back_to_directory_code(self):
        report = self.make_report(
            "600398.SS_20260723_2018", {"stock_name": "海/澜:之家"}
        )

        names = md2html.report_attachment_names(
            str(report), datetime(2026, 7, 23, 1, 2, 3)
        )

        self.assertEqual(
            names,
            (
                "海_澜_之家_600398.SS_20260723_010203.html",
                "海_澜_之家_600398.SS_20260723_010203.md",
            ),
        )
        for name in names:
            self.assertNotRegex(name, r'[<>:"/\\|?*\]\s]')

    def test_default_timestamp_comes_from_report_mtime(self):
        report = self.make_report(
            "00700_20260723_2018",
            {"stock_name": "腾讯控股", "stock_code": "00700"},
        )
        generated_at = datetime(2026, 7, 23, 20, 18, 55)
        os.utime(report, (generated_at.timestamp(), generated_at.timestamp()))

        names = md2html.report_attachment_names(str(report))

        self.assertEqual(names[0], "腾讯控股_00700_20260723_201855.html")
        self.assertEqual(names[1], "腾讯控股_00700_20260723_201855.md")

    def test_blank_stock_name_uses_nonempty_fallback(self):
        report = self.make_report(
            "601857_20260723_2018",
            {"stock_name": "  ", "stock_code": "601857"},
        )

        names = md2html.report_attachment_names(
            str(report), datetime(2026, 7, 23, 20, 18, 55)
        )

        self.assertEqual(names[0], "股票_601857_20260723_201855.html")

    def test_missing_summary_identity_falls_back_to_report_heading(self):
        report = self.make_report(
            "00700_20260723_2018",
            {},
            "# 📊 腾讯控股 (00700.HK) 股票分析报告\n",
        )

        names = md2html.report_attachment_names(
            str(report), datetime(2026, 7, 23, 20, 18, 55)
        )

        self.assertEqual(names[0], "腾讯控股_00700.HK_20260723_201855.html")

    def test_weixin_staged_basename_is_recipient_visible_name(self):
        source = self.root / "report.html"
        source.write_text("<h1>report</h1>", encoding="utf-8")
        recv_name = "中国石油_601857_20260723_201855.html"

        marker = self.publish(source, "weixin", recv_name, ".html")

        staged_matches = list((self.root / "media" / "out").glob(f"*/{recv_name}"))
        self.assertEqual(len(staged_matches), 1)
        staged = staged_matches[0]
        self.assertIn(str(staged).replace("\\", "/"), marker)
        self.assertIn("[:weixin-media type=file", marker)

    def test_generic_channel_marker_uses_sanitized_recipient_name(self):
        source = self.root / "report.md"
        source.write_text("# report", encoding="utf-8")

        marker = self.publish(
            source,
            "teams",
            "海/澜:之家_600398_20260723_010203.md",
            ".md",
        )

        self.assertIn('name="海_澜_之家_600398_20260723_010203.md"', marker)
        self.assertIn("[:teams-media type=file", marker)

    def test_same_recipient_name_keeps_distinct_staged_contents(self):
        source = self.root / "report.md"
        recv_name = "中国石油_601857_20260723_201855.md"
        source.write_text("first", encoding="utf-8")
        first_marker = self.publish(source, "weixin", recv_name, ".md")
        source.write_text("other", encoding="utf-8")
        second_marker = self.publish(source, "weixin", recv_name, ".md")

        staged = list((self.root / "media" / "out").glob(f"*/{recv_name}"))
        self.assertEqual(len(staged), 2)
        self.assertEqual(
            {path.read_text(encoding="utf-8") for path in staged},
            {"first", "other"},
        )
        self.assertNotEqual(first_marker, second_marker)

    def test_republishing_same_file_uses_new_delivery_identity(self):
        source = self.root / "report.md"
        source.write_text("same", encoding="utf-8")
        recv_name = "中国石油_601857_20260723_201855.md"

        first_marker = self.publish(source, "weixin", recv_name, ".md")
        second_marker = self.publish(source, "weixin", recv_name, ".md")

        self.assertNotEqual(first_marker, second_marker)
        self.assertEqual(
            first_marker.split(' path="', 1)[1].split('"', 1)[0],
            second_marker.split(' path="', 1)[1].split('"', 1)[0],
        )
        self.assertRegex(first_marker, r'delivery="[0-9a-f]{32}"')
        self.assertRegex(second_marker, r'delivery="[0-9a-f]{32}"')

    def test_main_publishes_html_and_md_with_the_same_napcatqq_stem(self):
        report = self.make_report(
            "601857_20260723_2018",
            {"stock_name": "中国石油", "stock_code": "601857"},
        )
        generated_at = datetime(2026, 7, 23, 20, 18, 55)
        os.utime(report, (generated_at.timestamp(), generated_at.timestamp()))
        html_path = report.parent / "report.html"
        output = io.StringIO()

        with (
            mock.patch.dict(
                os.environ,
                {
                    "RCLAW_NAPCAT_ENABLED": "1",
                    "RCLAW_MEDIA_DIR": str(self.root / "media"),
                    "RCLAW_SOURCE_CHANNEL": "napcatqq",
                },
                clear=False,
            ),
            mock.patch.object(
                sys,
                "argv",
                [
                    "md2html.py",
                    str(html_path),
                    "-i",
                    str(report),
                    "--source-channel",
                    # 与运行时来源故意冲突：marker 必须仍归属 napcat。
                    "weixin",
                ],
            ),
            mock.patch.object(md2html, "convert", return_value="<html></html>"),
            contextlib.redirect_stdout(output),
        ):
            md2html.main()

        markers = [
            line
            for line in output.getvalue().splitlines()
            if line.startswith("MEDIA_MARKER=")
        ]
        self.assertEqual(len(markers), 2)
        self.assertIn('name="中国石油_601857_20260723_201855.html"', markers[0])
        self.assertIn('name="中国石油_601857_20260723_201855.md"', markers[1])

    def test_main_emits_no_partial_receipt_when_markdown_staging_fails(self):
        report = self.make_report(
            "601857_20260723_2018",
            {"stock_name": "中国石油", "stock_code": "601857"},
        )
        html_path = report.parent / "report.html"
        output = io.StringIO()

        with (
            mock.patch.dict(
                os.environ,
                {
                    "RCLAW_NAPCAT_ENABLED": "1",
                    "RCLAW_MEDIA_DIR": str(self.root / "media"),
                    "RCLAW_SOURCE_CHANNEL": "napcat",
                },
                clear=False,
            ),
            mock.patch.object(
                sys,
                "argv",
                ["md2html.py", str(html_path), "-i", str(report)],
            ),
            mock.patch.object(md2html, "convert", return_value="<html></html>"),
            mock.patch.object(
                md2html,
                "_publish_media_file",
                side_effect=["[:napcat-media type=file path=\"html\"]", None],
            ),
            contextlib.redirect_stdout(output),
        ):
            md2html.main()

        self.assertNotIn("MEDIA_MARKER=", output.getvalue())


if __name__ == "__main__":
    unittest.main()
