import importlib
import os
import sys
import tempfile
import unittest
from unittest import mock

if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(errors="replace")

import fetch_ir


class DownloadPdfTests(unittest.TestCase):
    """download_pdf 的 %PDF- 魔数门 + 安卓顶层 fitz 门控（回归：真机 2026-10-31）。"""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.dest = os.path.join(self.tmp.name, "ir.pdf")

    @staticmethod
    def fake_response(chunks):
        resp = mock.Mock()
        resp.read.side_effect = list(chunks) + [b""]
        return resp

    @mock.patch("urllib.request.urlopen")
    def test_rejects_non_pdf_challenge_page(self, urlopen):
        urlopen.return_value = self.fake_response([b"<script>" + b"x" * 2000])

        self.assertFalse(
            fetch_ir.download_pdf("https://pdf.dfcfw.com/a.pdf", self.dest)
        )
        self.assertFalse(os.path.exists(self.dest), "挑战页文件应被删除")

    @mock.patch("urllib.request.urlopen")
    def test_accepts_real_pdf(self, urlopen):
        payload = b"%PDF-1.4\n" + b"x" * 4096
        urlopen.return_value = self.fake_response([payload[:1024], payload[1024:]])

        self.assertTrue(
            fetch_ir.download_pdf("https://pdf.dfcfw.com/a.pdf", self.dest)
        )
        self.assertTrue(os.path.exists(self.dest))

    @mock.patch("urllib.request.urlopen")
    def test_short_reads_do_not_break_magic_sniff(self, urlopen):
        # read(amt) 允许短读：%PDF- 被拆到两次网络读时不得误判为挑战页
        # （3 字节首读 + 补读凑满嗅探窗口）。
        urlopen.return_value = self.fake_response(
            [b"%PD", b"F-1.4\n" + b"x" * 2000, b""]
        )

        self.assertTrue(
            fetch_ir.download_pdf("https://pdf.dfcfw.com/a.pdf", self.dest)
        )
        self.assertTrue(os.path.exists(self.dest))

    @mock.patch("urllib.request.urlopen")
    def test_leading_junk_before_magic_is_accepted(self, urlopen):
        # PDF 规范允许 %PDF- 前有前导垃圾字节（1024 字节窗口内）。
        urlopen.return_value = self.fake_response(
            [b"\x00\n garbage header here", b"%PDF-1.4\n" + b"x" * 2000, b""]
        )

        self.assertTrue(
            fetch_ir.download_pdf("https://pdf.dfcfw.com/a.pdf", self.dest)
        )
        self.assertTrue(os.path.exists(self.dest))

    def test_android_never_imports_fitz(self):
        # APK 宿主顶层不 import fitz（垫片已删除）：门控直接置 None，
        # pdf_to_pages 走 RCLAW_ANDROID 分支的 pypdf + PdfRenderer 桥。
        old = os.environ.get("RCLAW_ANDROID")
        try:
            os.environ["RCLAW_ANDROID"] = "1"
            with mock.patch.dict(sys.modules):
                sys.modules.pop("fitz", None)
                reloaded = importlib.reload(fetch_ir)
                self.assertIsNone(reloaded.fitz)
                self.assertNotIn("fitz", sys.modules)
        finally:
            if old is None:
                os.environ.pop("RCLAW_ANDROID", None)
            else:
                os.environ["RCLAW_ANDROID"] = old
            # 还原模块级状态，避免影响其他用例。
            importlib.reload(fetch_ir)


if __name__ == "__main__":
    unittest.main()
