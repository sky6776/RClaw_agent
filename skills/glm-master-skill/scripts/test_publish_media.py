import contextlib
import io
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import publish_media


class PublishMediaTests(unittest.TestCase):
    def test_external_channels_share_media_sandbox(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "artifact.txt"
            source.write_text("ok", encoding="utf-8")
            media_root = root / "shared-media"
            env = {
                "RCLAW_MEDIA_DIR": str(media_root),
                "RCLAW_SOURCE_CHANNEL": "",
                "RCLAW_WEIXIN_ENABLED": "1",
                "RCLAW_FUTURE_CHANNEL_ENABLED": "1",
            }

            markers = {}
            for channel in ("weixin", "future-channel"):
                output = io.StringIO()
                argv = [
                    "publish_media.py",
                    str(source),
                    "--type",
                    "file",
                ]
                channel_env = dict(env)
                if channel == "weixin":
                    argv.extend(["--source-channel", channel])
                else:
                    channel_env["RCLAW_SOURCE_CHANNEL"] = channel
                    # 故意传入冲突回退值，证明运行时真实来源不会被模型参数改写。
                    argv.extend(["--source-channel", "weixin"])
                with (
                    mock.patch.dict(os.environ, channel_env, clear=False),
                    mock.patch.object(sys, "argv", argv),
                    contextlib.redirect_stdout(output),
                ):
                    publish_media.main()
                markers[channel] = output.getvalue().strip()

            self.assertTrue(markers["weixin"].startswith("MEDIA_MARKER=[:weixin-media "))
            self.assertTrue(
                markers["future-channel"].startswith(
                    "MEDIA_MARKER=[:future-channel-media "
                )
            )
            shared_out = (media_root / "out").resolve().as_posix()
            self.assertIn(f'path="{shared_out}/', markers["weixin"])
            self.assertIn(f'path="{shared_out}/', markers["future-channel"])
            self.assertRegex(markers["weixin"], r'delivery="[0-9a-f]{32}"')
            self.assertRegex(markers["future-channel"], r'delivery="[0-9a-f]{32}"')

    def test_republishing_same_content_gets_a_new_delivery_identity(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "artifact.txt"
            source.write_text("same bytes", encoding="utf-8")
            media_root = root / "media"

            def publish():
                output = io.StringIO()
                with (
                    mock.patch.dict(
                        os.environ,
                        {
                            "RCLAW_MEDIA_DIR": str(media_root),
                            "RCLAW_SOURCE_CHANNEL": "weixin",
                            "RCLAW_WEIXIN_ENABLED": "1",
                        },
                        clear=False,
                    ),
                    mock.patch.object(
                        sys,
                        "argv",
                        ["publish_media.py", str(source), "--type", "file"],
                    ),
                    contextlib.redirect_stdout(output),
                ):
                    publish_media.main()
                return output.getvalue().strip()

            first = publish()
            second = publish()

            self.assertNotEqual(first, second)
            self.assertEqual(
                first.split(' path="', 1)[1].split('"', 1)[0],
                second.split(' path="', 1)[1].split('"', 1)[0],
            )

    def test_same_path_metadata_cannot_overwrite_an_older_staged_marker(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "artifact.txt"
            source.write_text("first", encoding="utf-8")
            original = source.stat()
            media_root = root / "media"

            def publish():
                output = io.StringIO()
                with (
                    mock.patch.dict(
                        os.environ,
                        {
                            "RCLAW_MEDIA_DIR": str(media_root),
                            "RCLAW_SOURCE_CHANNEL": "weixin",
                            "RCLAW_WEIXIN_ENABLED": "1",
                        },
                        clear=False,
                    ),
                    mock.patch.object(
                        sys,
                        "argv",
                        ["publish_media.py", str(source), "--type", "file"],
                    ),
                    contextlib.redirect_stdout(output),
                ):
                    publish_media.main()
                return Path(
                    output.getvalue().split('path="', 1)[1].split('"', 1)[0]
                )

            first_staged = publish()
            source.write_text("other", encoding="utf-8")
            os.utime(source, ns=(original.st_atime_ns, original.st_mtime_ns))
            second_staged = publish()

            self.assertNotEqual(first_staged, second_staged)
            self.assertEqual(first_staged.read_text(encoding="utf-8"), "first")
            self.assertEqual(second_staged.read_text(encoding="utf-8"), "other")


if __name__ == "__main__":
    unittest.main()
