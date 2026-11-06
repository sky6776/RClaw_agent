"""check_task 状态矩阵测试（unittest；本机未安装 pytest）。

覆盖 `save/no-save × pending/terminal-ok/terminal-fail/unknown/缺 status`、
下载调用参数与真实下载失败路径。排队/运行中是 exit 0 + pending:true 的正常
中间态，不是工具执行失败。

运行：python skills/seedance-video/scripts/test_seedance_check_pending.py -v
"""

import importlib.util
import io
import json
import tempfile
import unittest
import urllib.error
from contextlib import redirect_stdout
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

_CLI_PATH = Path(__file__).with_name("seedance_video_cli.py")
_spec = importlib.util.spec_from_file_location("seedance_video_cli", _CLI_PATH)
cli = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(cli)

CONFIG = {
    "task_url_template": "https://example.invalid/tasks/{task_id}",
    "download_timeout_sec": 1,
    "max_download_bytes": 1024,
}
VIDEO_URL = "https://example.invalid/v.mp4"


class _SingleChunkResponse:
    """urlopen 替身：第一次 read 返回给定内容，下一次返回 EOF。"""

    def __init__(self, payload):
        self._chunks = [payload, b""]

    def __enter__(self):
        return self

    def __exit__(self, *_exc):
        return False

    def read(self, _size=-1):
        return self._chunks.pop(0)


class CheckTaskMatrixTest(unittest.TestCase):
    def _run(self, *, status, video_url=None, save="output/x.mp4", download=None):
        args = SimpleNamespace(task_id=" cgt-1 ", save=save)
        summary = {
            "task_id": "cgt-1",
            "status": status,
            "video_url": video_url,
            "output_format": "mp4",
        }
        download_mock = mock.MagicMock()
        if download is not None:
            download_mock.side_effect = download
        else:
            download_mock.return_value = Path("output/x.mp4")
        buf = io.StringIO()
        with mock.patch.object(cli, "request_json", return_value={"id": "cgt-1"}), \
                mock.patch.object(cli, "summarize_task", return_value=summary), \
                mock.patch.object(cli, "download_media", download_mock), \
                redirect_stdout(buf):
            try:
                cli.run_check(args, CONFIG)
                code = 0
            except SystemExit as exc:
                code = exc.code
        lines = [line for line in buf.getvalue().splitlines() if line.strip()]
        payload = json.loads(lines[-1]) if lines else None
        return code, payload, download_mock

    def test_save_running_is_pending_success(self):
        code, payload, download = self._run(status="running")
        self.assertEqual(code, 0)
        self.assertTrue(payload["ok"])
        self.assertTrue(payload["pending"])
        self.assertFalse(payload["saved"])
        self.assertEqual(payload["task_id"], "cgt-1")
        download.assert_not_called()

    def test_no_save_running_is_pending_success(self):
        code, payload, download = self._run(status="queued", save=None)
        self.assertEqual(code, 0)
        self.assertTrue(payload["ok"])
        self.assertTrue(payload["pending"])
        self.assertFalse(payload["saved"])
        download.assert_not_called()

    def test_save_failed_is_terminal_error(self):
        code, payload, download = self._run(status="failed")
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "TASK_FAILED")
        download.assert_not_called()

    def test_no_save_failed_is_successful_query(self):
        code, payload, download = self._run(status="failed", save=None)
        self.assertEqual(code, 0)
        self.assertEqual(payload["status"], "failed")
        self.assertNotIn("pending", payload)
        download.assert_not_called()

    def test_save_succeeded_downloads(self):
        code, payload, download = self._run(status="succeeded", video_url=VIDEO_URL)
        self.assertEqual(code, 0)
        self.assertTrue(payload["ok"])
        self.assertTrue(payload["saved"])
        self.assertEqual(Path(payload["saved_file"]), Path("output/x.mp4"))
        self.assertNotIn("pending", payload)
        download.assert_called_once_with(
            VIDEO_URL, Path("output/x.mp4"), 1, 1024, "video"
        )

    def test_no_save_succeeded_is_query_only(self):
        code, payload, download = self._run(
            status="succeeded", video_url=VIDEO_URL, save=None
        )
        self.assertEqual(code, 0)
        self.assertEqual(payload["status"], "succeeded")
        self.assertNotIn("pending", payload)
        self.assertNotIn("saved", payload)
        download.assert_not_called()

    def test_save_succeeded_without_url_is_error(self):
        code, payload, download = self._run(status="succeeded")
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "MISSING_VIDEO_URL")
        download.assert_not_called()

    def test_unknown_status_is_error(self):
        code, payload, download = self._run(status="mystery")
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "POLL_FAILED")
        download.assert_not_called()

    def test_no_save_unknown_status_is_error(self):
        code, payload, download = self._run(status="mystery", save=None)
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "POLL_FAILED")
        download.assert_not_called()

    def test_no_save_missing_status_is_error(self):
        code, payload, download = self._run(status=None, save=None)
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "POLL_FAILED")
        download.assert_not_called()

    def test_missing_status_is_error(self):
        code, payload, download = self._run(status=None)
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "POLL_FAILED")
        download.assert_not_called()

    def test_all_terminal_fail_statuses_are_errors(self):
        for status in ("cancelled", "expired"):
            with self.subTest(status=status):
                code, payload, download = self._run(status=status)
                self.assertEqual(code, 1)
                self.assertEqual(payload["error"]["code"], "TASK_" + status.upper())
                download.assert_not_called()

    def test_no_save_terminal_fail_statuses_are_successful_queries(self):
        for status in ("cancelled", "expired"):
            with self.subTest(status=status):
                code, payload, download = self._run(status=status, save=None)
                self.assertEqual(code, 0)
                self.assertEqual(payload["status"], status)
                self.assertNotIn("pending", payload)
                download.assert_not_called()

    def test_download_failure_is_error(self):
        def failing_download(*_args, **_kwargs):
            cli.fail("DOWNLOAD_FAILED", "下载失败")

        code, payload, download = self._run(
            status="succeeded", video_url=VIDEO_URL, download=failing_download
        )
        self.assertEqual(code, 1)
        self.assertEqual(payload["error"]["code"], "DOWNLOAD_FAILED")
        download.assert_called_once()

    def test_real_download_network_error_is_error(self):
        args = SimpleNamespace(task_id="cgt-1", save="output/x.mp4")
        summary = {
            "task_id": "cgt-1",
            "status": "succeeded",
            "video_url": VIDEO_URL,
            "output_format": "mp4",
        }
        buf = io.StringIO()
        with mock.patch.object(cli, "request_json", return_value={"id": "cgt-1"}), \
                mock.patch.object(cli, "summarize_task", return_value=summary), \
                mock.patch.object(
                    cli.urllib.request,
                    "urlopen",
                    side_effect=urllib.error.URLError("boom"),
                ), \
                redirect_stdout(buf):
            with self.assertRaises(SystemExit) as raised:
                cli.run_check(args, CONFIG)
        self.assertEqual(raised.exception.code, 1)
        lines = [line for line in buf.getvalue().splitlines() if line.strip()]
        payload = json.loads(lines[-1])
        self.assertEqual(payload["error"]["code"], "DOWNLOAD_FAILED")

    def test_real_download_bad_video_format_is_error(self):
        with tempfile.TemporaryDirectory() as tmp:
            save = Path(tmp) / "out.mp4"
            args = SimpleNamespace(task_id="cgt-1", save=str(save))
            summary = {
                "task_id": "cgt-1",
                "status": "succeeded",
                "video_url": VIDEO_URL,
                "output_format": "mp4",
            }
            buf = io.StringIO()
            with mock.patch.object(cli, "request_json", return_value={"id": "cgt-1"}), \
                    mock.patch.object(cli, "summarize_task", return_value=summary), \
                    mock.patch.object(
                        cli.urllib.request,
                        "urlopen",
                        return_value=_SingleChunkResponse(b"not a video file"),
                    ), \
                    redirect_stdout(buf):
                with self.assertRaises(SystemExit) as raised:
                    cli.run_check(args, CONFIG)
            self.assertEqual(raised.exception.code, 1)
            lines = [line for line in buf.getvalue().splitlines() if line.strip()]
            payload = json.loads(lines[-1])
            self.assertEqual(payload["error"]["code"], "DOWNLOAD_FAILED")
            self.assertFalse(save.exists(), "魔数不符时必须清理半成品文件")


if __name__ == "__main__":
    unittest.main()
