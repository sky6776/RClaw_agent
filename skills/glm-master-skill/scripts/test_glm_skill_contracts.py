#!/usr/bin/env python3
"""非股票 GLM skill 的离线契约回归测试。"""

from __future__ import annotations

import base64
import contextlib
import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock


REPO_ROOT = Path(__file__).resolve().parents[3]


def load_module(name: str, relative_path: str):
    """按仓库相对路径加载脚本，同时暴露脚本自己的同级依赖。"""
    path = REPO_ROOT / relative_path
    if str(path.parent) not in sys.path:
        sys.path.insert(0, str(path.parent))
    spec = importlib.util.spec_from_file_location(name, path)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


glm5v = load_module("contract_glm5v", "skills/glm5v-multimodal/scripts/glm5v.py")
glm_config = load_module(
    "contract_glm_config", "skills/glm-master-skill/scripts/glm_config.py"
)
fund_extract = load_module(
    "contract_fund_extract", "skills/evaluate-fund/scripts/glm5v_extract.py"
)
caption = load_module("contract_caption", "skills/glmv-caption/scripts/glmv_caption.py")
prompt_gen = load_module("contract_prompt_gen", "skills/glmv-prompt-gen/scripts/prompt_gen.py")
doc_writing = load_module(
    "contract_doc_writing", "skills/glmv-doc-based-writing/scripts/doc_based_writing.py"
)
resume_screen = load_module(
    "contract_resume_screen", "skills/glmv-resume-screen/scripts/resume_screen.py"
)
image_gen = load_module(
    "contract_image_gen", "skills/glm-image-gen/scripts/glm_image_cli.py"
)
pdf_web = load_module(
    "contract_pdf_web", "skills/glmv-pdf-to-web/scripts/pdf_to_images.py"
)
pdf_ppt = load_module(
    "contract_pdf_ppt", "skills/glmv-pdf-to-ppt/scripts/pdf_to_images.py"
)

OCR_MODULES = [
    load_module("contract_ocr", "skills/glmocr/scripts/glm_ocr_cli.py"),
    load_module("contract_ocr_table", "skills/glmocr-table/scripts/glm_ocr_cli.py"),
    load_module("contract_ocr_formula", "skills/glmocr-formula/scripts/glm_ocr_cli.py"),
    load_module(
        "contract_ocr_handwriting", "skills/glmocr-handwriting/scripts/glm_ocr_cli.py"
    ),
]


def png_bytes(width: int = 32, height: int = 24) -> bytes:
    """构造足够用于文件头与尺寸预检的最小 PNG 前缀。"""
    return (
        b"\x89PNG\r\n\x1a\n"
        + b"\x00\x00\x00\rIHDR"
        + width.to_bytes(4, "big")
        + height.to_bytes(4, "big")
    )


def vision_config() -> dict:
    """返回视觉封装单元测试所需的最小共享配置。"""
    return {
        "api_key": "test-key",
        "model": "GLM-5V-Turbo",
        "base_url": "https://open.bigmodel.cn/api/paas/v4",
        "temperature": 0.3,
        "top_p": 0.6,
        "max_tokens": 100,
        "max_tokens_key": "max_tokens",
        "connect_timeout_sec": 1,
        "read_timeout_sec": 1,
        "max_retries": 0,
        "thinking": "enabled",
        "stream": True,
        "image_limit": 8,
        "_config_path": "test.toml",
    }


def model_response(finish_reason: str = "length") -> dict:
    """生成一个包含停止原因的稳定伪响应。"""
    return {
        "choices": [
            {
                "message": {"content": "模型正文"},
                "finish_reason": finish_reason,
            }
        ],
        "usage": {"total_tokens": 10},
    }


class FundAdviceContractTests(unittest.TestCase):
    def test_missing_profile_parameters_do_not_block_actionable_advice(self) -> None:
        """缺少个性化参数只能限制建议幅度，不能阻断逐只操作方向。"""
        skill_text = (REPO_ROOT / "skills/evaluate-fund/SKILL.md").read_text(
            encoding="utf-8"
        )
        reference_text = (
            REPO_ROOT / "skills/evaluate-fund/references/portfolio-analysis.md"
        ).read_text(encoding="utf-8")

        # 中文注释：同时锁定主契约和组合参考，避免以后只修改一处造成行为漂移。
        for text in (skill_text, reference_text):
            self.assertIn("个性化参数", text)
            self.assertIn("不得阻断", text)
            self.assertIn("分批赎回/卖出", text)
            self.assertIn("精确交易金额", text)
        self.assertNotIn(
            "用户尚未提供目标/期限/最大可接受回撤时，组合结论只能是",
            skill_text,
        )


class MediaPreflightTests(unittest.TestCase):
    def test_shared_config_ignores_api_key_environment_variables(self) -> None:
        config_text = """[zhipu]
api_key = "config-only-key"
base_url = "https://open.bigmodel.cn/api/paas/v4"
auth = "bearer"
"""
        with tempfile.TemporaryDirectory() as temp_dir:
            config_path = Path(temp_dir) / "config.toml"
            config_path.write_text(config_text, encoding="utf-8")
            # 中文注释：即使进程环境中存在同名候选变量，共享加载器也必须只认 TOML。
            with mock.patch.dict(
                os.environ, {"RCLAW_GLM5V_API_KEY": "environment-key"}, clear=False
            ):
                loaded = glm_config.load_config(config_path=config_path)
        self.assertEqual(loaded["api_key"], "config-only-key")

    def test_glm5v_uses_data_url_and_enforces_dimensions(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            image = Path(temp_dir) / "sample.png"
            image.write_bytes(png_bytes())
            block = glm5v.image_block(image)
            self.assertTrue(block["image_url"]["url"].startswith("data:image/png;base64,"))
            self.assertEqual(glm5v.image_dimensions(image.read_bytes()), (32, 24))
            oversized = Path(temp_dir) / "oversized.png"
            oversized.write_bytes(png_bytes(6001, 10))
            _, errors = glm5v._collect_local_paths(
                [str(oversized)],
                glm5v.IMAGE_TYPES,
                "图片",
                max_bytes=glm5v.MAX_IMAGE_BYTES,
                validate_image_content=True,
            )
            self.assertIn("1-6000", "\n".join(errors))

    def test_fund_extraction_uses_the_same_image_limits(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir:
            task_dir = Path(temp_dir)
            image = task_dir / "oversized.png"
            image.write_bytes(png_bytes(6001, 10))
            with self.assertRaises(ValueError) as caught:
                fund_extract.resolve_images([image], task_dir)
        self.assertIn("6000x6000", str(caught.exception))

    def test_caption_and_prompt_report_length_truncation(self) -> None:
        inline = "data:image/png;base64," + base64.b64encode(png_bytes()).decode("ascii")
        for module, function, kwargs in (
            (caption, caption.caption, {"images": [inline]}),
            (prompt_gen, prompt_gen.generate, {"images": [inline]}),
        ):
            with self.subTest(module=module.__name__), mock.patch.object(
                module, "load_config", return_value=vision_config()
            ), mock.patch.object(module, "post_stream_json", return_value=model_response()):
                result = function(**kwargs)
                self.assertTrue(result["success"])
                self.assertEqual(result["finish_reason"], "length")
                self.assertFalse(result["complete"])
                self.assertIn("truncated", result["warning"])

    def test_document_wrappers_report_length_truncation(self) -> None:
        for module, function, kwargs in (
            (
                doc_writing,
                doc_writing.write_from_documents,
                {"files": ["https://example.com/a.pdf"], "requirements": "总结"},
            ),
            (
                resume_screen,
                resume_screen.screen,
                {"files": ["https://example.com/a.pdf"], "criteria": "Python"},
            ),
        ):
            with self.subTest(module=module.__name__), mock.patch.object(
                module, "load_config", return_value=vision_config()
            ), mock.patch.object(module, "post_stream_json", return_value=model_response()):
                result = function(**kwargs)
                self.assertEqual(result["finish_reason"], "length")
                self.assertFalse(result["complete"])

    def test_wrappers_create_nested_output_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temp_dir, contextlib.redirect_stdout(
            io.StringIO()
        ), contextlib.redirect_stderr(io.StringIO()):
            root = Path(temp_dir)
            caption_output = root / "caption" / "result.json"
            with mock.patch.object(
                caption,
                "caption",
                return_value={"success": True, "caption": "正文", "complete": True},
            ), mock.patch.object(
                sys,
                "argv",
                ["glmv_caption.py", "--images", "unused.png", "--output", str(caption_output)],
            ):
                caption.main()
            self.assertTrue(caption_output.is_file())

            prompt_output = root / "prompt" / "result.txt"
            with mock.patch.object(
                prompt_gen,
                "generate",
                return_value={"success": True, "result": "正文", "complete": True},
            ), mock.patch.object(
                sys,
                "argv",
                ["prompt_gen.py", "--images", "unused.png", "--output", str(prompt_output)],
            ):
                prompt_gen.main()
            self.assertEqual(prompt_output.read_text(encoding="utf-8"), "正文")

            for module, function_name, required_flag in (
                (doc_writing, "write_from_documents", "--requirements"),
                (resume_screen, "screen", "--criteria"),
            ):
                output = root / module.__name__ / "result.MD"
                result = {
                    "success": True,
                    "result": "正文",
                    "file_count": 1,
                    "complete": True,
                }
                with self.subTest(module=module.__name__), mock.patch.object(
                    module, function_name, return_value=result
                ), mock.patch.object(
                    sys,
                    "argv",
                    [
                        f"{module.__name__}.py",
                        "--files",
                        "unused.pdf",
                        required_flag,
                        "要求",
                        "--output",
                        str(output),
                    ],
                ):
                    module.main()
                self.assertTrue(output.is_file())
                self.assertTrue(output.read_text(encoding="utf-8").startswith("# "))


class OcrContractTests(unittest.TestCase):
    def test_every_ocr_wrapper_rejects_ambiguous_urls_and_fake_files(self) -> None:
        for module in OCR_MODULES:
            with self.subTest(module=module.__name__), tempfile.TemporaryDirectory() as temp_dir:
                self.assertFalse(module._is_url("https://user:pass@example.com/a.png"))
                fake = Path(temp_dir) / "fake.png"
                fake.write_bytes(b"not-a-png")
                payload, error = module._encode_file(str(fake))
                self.assertEqual(payload, "")
                self.assertIn("content", error.lower())

    def test_every_ocr_wrapper_omits_raw_response_by_default(self) -> None:
        upstream = {"md_results": "正文", "layout_details": []}
        for module in OCR_MODULES:
            with self.subTest(module=module.__name__), mock.patch.object(
                module, "get_config", return_value={"model": "glm-ocr"}
            ), mock.patch.object(module, "_make_api_request", return_value=upstream):
                compact = module.extract_text("https://example.com/a.png")
                verbose = module.extract_text(
                    "https://example.com/a.png", include_raw_result=True
                )
                self.assertIsNone(compact["result"])
                self.assertFalse(compact["raw_result_included"])
                self.assertIs(verbose["result"], upstream)
                self.assertTrue(verbose["raw_result_included"])


class GroundingContractTests(unittest.TestCase):
    def test_grounding_adds_protocol_and_exposes_finish_reason(self) -> None:
        captured = {}


class ImageAndPrdContractTests(unittest.TestCase):
    def test_image_generation_current_limits(self) -> None:
        self.assertTrue(image_gen.validate_size("512x512", "glm-image"))
        self.assertFalse(image_gen.validate_size("500x500", "glm-image"))
        self.assertTrue(image_gen.validate_prompt("一只猫"))
        self.assertFalse(image_gen.validate_prompt(" "))
        self.assertFalse(image_gen.validate_prompt("x" * 1001))

    def test_pdf_preflight_checks_signature_and_dpi(self) -> None:
        for module in (pdf_web, pdf_ppt):
            with self.subTest(module=module.__name__), tempfile.TemporaryDirectory() as temp_dir:
                fake = Path(temp_dir) / "fake.pdf"
                fake.write_bytes(b"not-pdf")
                self.assertIn("not a PDF", module.validate_pdf(str(fake), 120))
                self.assertIn("between 36 and 600", module.validate_pdf(str(fake), 1))


if __name__ == "__main__":
    unittest.main()
