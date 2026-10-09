#!/usr/bin/env python3
"""Regression tests for complete-batch GLM media input preflight."""

from __future__ import annotations

import importlib.util
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace


REPO_ROOT = Path(__file__).resolve().parents[3]


def load_module(name: str, relative_path: str):
    path = REPO_ROOT / relative_path
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


glm5v = load_module(
    "batch_test_glm5v", "skills/glm5v-multimodal/scripts/glm5v.py"
)
caption = load_module(
    "batch_test_caption", "skills/glmv-caption/scripts/glmv_caption.py"
)
prompt_gen = load_module(
    "batch_test_prompt_gen", "skills/glmv-prompt-gen/scripts/prompt_gen.py"
)
doc_writing = load_module(
    "batch_test_doc_writing",
    "skills/glmv-doc-based-writing/scripts/doc_based_writing.py",
)
resume_screen = load_module(
    "batch_test_resume_screen",
    "skills/glmv-resume-screen/scripts/resume_screen.py",
)


class BatchPreflightTests(unittest.TestCase):
    def test_base_multimodal_reports_every_group_problem(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            missing_one = str(Path(temp_dir) / "missing-one.png")
            missing_two = str(Path(temp_dir) / "missing-two.jpg")
            args = SimpleNamespace(
                image=[missing_one, missing_two],
                image_url=["https://"],
                video=[],
                video_url=["not-a-url"],
                file=[],
                file_url=[],
            )
            with self.assertRaises(ValueError) as caught:
                glm5v.select_media(args, {"image_limit": 8, "file_limit": 8})
        message = str(caught.exception)
        self.assertIn("missing-one.png", message)
        self.assertIn("missing-two.jpg", message)
        self.assertIn("https://", message)
        self.assertIn("not-a-url", message)
        self.assertIn("只能选择", message)

    def test_caption_and_prompt_generation_report_both_missing_images(self) -> None:
        images = ["missing-one.png", "missing-two.jpg"]
        caption_errors = caption.validate_inputs(images, None, None, "glm-5v-turbo")
        prompt_errors = prompt_gen.validate_inputs(images, None, "glm-5v-turbo")
        for errors in (caption_errors, prompt_errors):
            joined = "\n".join(errors)
            self.assertIn("missing-one.png", joined)
            self.assertIn("missing-two.jpg", joined)

    def test_document_and_resume_preflight_report_whole_batch(self) -> None:
        files = ["missing-one.pdf", "missing-two.docx", "https://"]
        for errors in (
            doc_writing.validate_document_inputs(files, ""),
            resume_screen.validate_resume_inputs(files, ""),
        ):
            joined = "\n".join(errors)
            self.assertIn("missing-one.pdf", joined)
            self.assertIn("missing-two.docx", joined)
            self.assertIn("https://", joined)
            self.assertIn("empty", joined)


if __name__ == "__main__":
    unittest.main()
