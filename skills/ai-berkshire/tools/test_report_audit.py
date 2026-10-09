import contextlib
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import uuid
from unittest import mock

import report_audit


def valid_result():
    return {
        'id': 1,
        'label': '营业收入 · 2025',
        'reported_value': 100.0,
        'unit': '亿',
        'line_number': 3,
        'raw_text': '| 营业收入 | 100亿 |',
        'fetched_raw_value': 100.0,
        'fetched_raw_unit': '亿元',
        'fetched_evidence': '合并利润表显示营业收入为 100 亿元。',
        'fetched_value': None,
        'fetched_source': 'https://data.eastmoney.com/report/123.html',
        'fetched_locator': '主要财务指标表，营业收入行',
        'fetched_raw_value2': 10000.0,
        'fetched_raw_unit2': '百万元',
        'fetched_evidence2': '年度报告合并利润表：营业收入 10,000 百万元。',
        'fetched_value2': None,
        'fetched_source2': 'https://www.cninfo.com.cn/new/disclosure/detail/456',
        'fetched_locator2': '年度报告第 12 页，合并利润表',
    }


class ReportAuditTests(unittest.TestCase):
    def test_verdict_rejects_descriptions_that_are_not_source_urls(self):
        item = valid_result()
        item['fetched_source2'] = '估值假设，非市场数据'

        with self.assertRaisesRegex(ValueError, '完整 http/https URL'):
            report_audit.validate_verdict_results([item])

    def test_verdict_requires_two_independent_publishers(self):
        item = valid_result()
        item['fetched_source2'] = 'https://quote.eastmoney.com/company/600000.html'

        with self.assertRaisesRegex(ValueError, '不构成独立交叉验证'):
            report_audit.validate_verdict_results([item])

    def test_verdict_rejects_a_publisher_homepage_as_direct_evidence(self):
        item = valid_result()
        item['fetched_source2'] = 'https://www.cninfo.com.cn/'

        with self.assertRaisesRegex(ValueError, '完整 http/https URL'):
            report_audit.validate_verdict_results([item])

    def test_verdict_accepts_two_traceable_independent_sources(self):
        item = valid_result()
        report_audit.validate_verdict_results([item])
        self.assertEqual(item['fetched_value'], 100.0)
        self.assertEqual(item['fetched_value2'], 100.0)

    def test_verdict_accepts_explicit_base_cny_units(self):
        item = valid_result()
        item['reported_value'] = 100.0
        item['unit'] = '元'
        item['fetched_raw_value'] = 100.0
        item['fetched_raw_unit'] = '元'
        item['fetched_evidence'] = '营业收入为 100 元。'
        item['fetched_raw_value2'] = 100.0
        item['fetched_raw_unit2'] = 'CNY'
        item['fetched_evidence2'] = 'Revenue was CNY 100.'

        report_audit.validate_verdict_results([item])

    def test_verdict_accepts_count_units_without_treating_them_as_cny(self):
        item = valid_result()
        item['label'] = '总股本 · 2025'
        item['reported_value'] = 48.03
        item['unit'] = '亿股'
        item['raw_text'] = '| 总股本 | 48.03亿股 |'
        item['fetched_raw_value'] = 4803000000.0
        item['fetched_raw_unit'] = '股'
        item['fetched_evidence'] = '公司公告显示总股本为 4,803,000,000 股。'
        item['fetched_raw_value2'] = 48.03
        item['fetched_raw_unit2'] = '亿股'
        item['fetched_evidence2'] = '证券资料显示总股本为 48.03 亿股。'

        report_audit.validate_verdict_results([item])

        self.assertEqual(item['fetched_value'], 48.03)
        self.assertEqual(item['fetched_value2'], 48.03)

    def test_verdict_normalizes_supported_price_per_unit_aliases(self):
        item = valid_result()
        item['label'] = '布伦特原油价格'
        item['reported_value'] = 87.0
        item['unit'] = '美元/桶'
        item['raw_text'] = '| 布伦特原油价格 | 87美元/桶 |'
        item['fetched_raw_value'] = 87.0
        item['fetched_raw_unit'] = 'USD/bbl'
        item['fetched_evidence'] = 'Brent settled at 87 USD/bbl.'
        item['fetched_raw_value2'] = 87.0
        item['fetched_raw_unit2'] = '美元/桶'
        item['fetched_evidence2'] = '布伦特原油收盘价格为 87 美元/桶。'

        report_audit.validate_verdict_results([item])

        self.assertEqual(item['fetched_value'], 87.0)
        self.assertEqual(item['fetched_value2'], 87.0)

    def test_extracted_price_unit_variants_remain_normalizable(self):
        for unit in ('CNY/吨', 'HK$/oz', 'RMB/g', 'USD/share'):
            self.assertIsNotNone(report_audit._unit_scale(unit), unit)

    def test_verdict_rejects_sources_that_conflict_even_if_each_is_near_report(self):
        item = valid_result()
        item['fetched_raw_value'] = 99.1
        item['fetched_evidence'] = '合并利润表显示营业收入为 99.1 亿元。'
        item['fetched_raw_value2'] = 10090.0
        item['fetched_evidence2'] = '年度报告合并利润表：营业收入 10,090 百万元。'

        report_audit.validate_verdict_results([item])
        with contextlib.redirect_stdout(io.StringIO()):
            outcome = report_audit.render_verdict([item])

        self.assertEqual(outcome['verdict'], 'FAIL')
        self.assertEqual(outcome['warn_count'], 1)
        self.assertGreater(outcome['warn_items'][0]['source_diff_pct'], 1)

    def test_verdict_rejects_model_supplied_normalized_values(self):
        item = valid_result()
        item['fetched_value2'] = item['reported_value']

        with self.assertRaisesRegex(ValueError, '必须保持为 null'):
            report_audit.validate_verdict_results([item])

    def test_verdict_requires_raw_value_and_unit_in_evidence_excerpt(self):
        item = valid_result()
        item['fetched_evidence2'] = '年度报告合并利润表，数值与报告一致。'

        with self.assertRaisesRegex(ValueError, '未出现 fetched_raw_value2'):
            report_audit.validate_verdict_results([item])

    def test_verdict_rejects_mixed_currency_evidence(self):
        item = valid_result()
        item['fetched_evidence2'] = (
            '营业收入 10,000 百万元（折合 USD 1,400 million）。'
        )

        with self.assertRaisesRegex(ValueError, '同时出现多种币种'):
            report_audit.validate_verdict_results([item])

    def test_verdict_does_not_treat_usd_billions_as_bare_chinese_yi(self):
        item = valid_result()
        item['fetched_raw_value2'] = 10.0
        item['fetched_raw_unit2'] = '十亿美元'
        item['fetched_evidence2'] = '年度报告合并利润表：营业收入 10 十亿美元。'

        with self.assertRaisesRegex(ValueError, '币种.*不一致'):
            report_audit.validate_verdict_results([item])

    def test_verdict_rejects_ambiguous_source_currency(self):
        item = valid_result()
        item['fetched_raw_value2'] = 10000.0
        item['fetched_raw_unit2'] = 'million'
        item['fetched_evidence2'] = 'Annual revenue was 10,000 million.'

        with self.assertRaisesRegex(ValueError, '未声明币种'):
            report_audit.validate_verdict_results([item])

    def test_verdict_rejects_a_smaller_unit_hidden_inside_the_excerpt(self):
        item = valid_result()
        item['fetched_raw_value2'] = 10000.0
        item['fetched_raw_unit2'] = '元'
        item['fetched_evidence2'] = '年度报告合并利润表：营业收入 10,000 百万元。'

        with self.assertRaisesRegex(ValueError, '来源单位'):
            report_audit.validate_verdict_results([item])

    def test_verdict_rejects_private_or_root_query_source_urls(self):
        for source in (
            'http://127.0.0.1/report/1',
            'https://cninfo.com.cn/?id=1',
            'https://cninfo.com.cn/%2e/?id=1',
            'https://cninfo.com.cn/%2f?id=1',
            'https://cninfo.com.cn/%252e/?id=1',
            'https://cninfo.com.cn/report/\x1b[31m',
        ):
            item = valid_result()
            item['fetched_source2'] = source
            with self.assertRaisesRegex(ValueError, '完整 http/https URL'):
                report_audit.validate_verdict_results([item])

    def test_extract_excludes_scenario_assumptions_and_action_thresholds(self):
        report = """
| 指标 | 数值 |
|---|---|
| 营业收入 | 100亿元 |
| 乐观情景年增速 | 8% |
| 中性目标PE | 12x |
| 加仓信号 | 75 |
| 预测收入 · 2026E | 120亿元 |
"""
        labels = [point['label'] for point in report_audit.extract_data_points(report)]

        self.assertTrue(any('营业收入' in label for label in labels))
        self.assertFalse(any('乐观' in label for label in labels))
        self.assertFalse(any('目标PE' in label for label in labels))
        self.assertFalse(any('加仓' in label for label in labels))
        self.assertFalse(any('2026E' in label for label in labels))

    def test_non_fact_column_does_not_hide_historical_fact_in_same_row(self):
        report = """
| 指标 | 2025A | 预计2026E |
|---|---:|---:|
| 营业收入 | 100亿元 | 120亿元 |
"""
        labels = [point['label'] for point in report_audit.extract_data_points(report)]

        self.assertTrue(any('2025A' in label for label in labels))
        self.assertFalse(any('2026E' in label for label in labels))

    def test_extract_infers_units_from_row_or_table_header_only(self):
        report = """
| 指标 | 2025 |
|---|---:|
| 营业收入（亿元） | 100 |
| 亿纬锂能客户数 | 10 |

| 指标（百万元） | 2025 |
|---|---:|
| 经营现金流 | 1200 |
"""
        points = report_audit.extract_data_points(report)

        revenue = next(point for point in points if '营业收入' in point['label'])
        cash_flow = next(point for point in points if '经营现金流' in point['label'])
        customer = next(point for point in points if '亿纬锂能客户数' in point['label'])
        self.assertEqual(revenue['unit'], '亿元')
        self.assertEqual(cash_flow['unit'], '百万元')
        self.assertEqual(customer['unit'], '')

    def test_extract_prefers_declared_amount_unit_over_inline_growth_rate(self):
        report = """
| 指标 | 2024 |
|---|---:|
| 营业收入（亿元） | 844.92(+8.12%) |
"""
        revenue = next(
            point
            for point in report_audit.extract_data_points(report)
            if '营业收入' in point['label']
        )

        self.assertEqual(revenue['reported_value'], 844.92)
        self.assertEqual(revenue['unit'], '亿元')

    def test_extract_preserves_dollar_currency_on_scaled_values(self):
        report = """
| 指标 | 2025 |
|---|---:|
| 营业收入 | $5.2B |
"""
        point = next(
            point
            for point in report_audit.extract_data_points(report)
            if '营业收入' in point['label']
        )

        self.assertEqual(point['unit'], 'USD B')

    def test_extract_skips_year_prefix_and_uses_the_following_value(self):
        report = """
| 指标 | 数值 |
|---|---:|
| 研发投入 | 2025年研发费用2.12亿元 |

研发费用：2025年度研发投入2.12亿元
"""
        points = [
            point
            for point in report_audit.extract_data_points(report)
            if '研发' in point['label']
        ]

        self.assertEqual(len(points), 2)
        self.assertTrue(all(point['reported_value'] == 2.12 for point in points))
        self.assertTrue(all(point['unit'] == '亿元' for point in points))

    def test_extract_does_not_turn_a_range_into_negative_single_values(self):
        report = """
| 指标 | 数值 |
|---|---:|
| 市占率区间 | 82-91% |
| 另一市占率区间 | 82-91%；营业收入120亿元 |
"""

        self.assertEqual(report_audit.extract_data_points(report), [])

    def test_kv_range_does_not_capture_the_next_fields_value(self):
        report = "市占率区间：82-91%；营业收入：120亿元"

        points = report_audit.extract_data_points(report)

        self.assertFalse(any('市占率区间' in point['label'] for point in points))
        revenue = next(point for point in points if '营业收入' in point['label'])
        self.assertEqual(revenue['reported_value'], 120)
        self.assertEqual(revenue['unit'], '亿元')

    def test_extract_preserves_count_units(self):
        report = """
| 指标 | 数值 |
|---|---:|
| 总股本 | 48.03亿股 |
| 门店数量 | 7330家 |
| 海外门店 | 1.2万家门店 |
| 布伦特原油价格 | 87美元/桶 |
| WTI原油价格 | 86 USD/bbl |
"""
        points = report_audit.extract_data_points(report)
        shares = next(point for point in points if '总股本' in point['label'])
        stores = next(point for point in points if '门店数量' in point['label'])
        overseas_stores = next(point for point in points if '海外门店' in point['label'])
        oil = next(point for point in points if '布伦特原油价格' in point['label'])
        wti = next(point for point in points if 'WTI原油价格' in point['label'])

        self.assertEqual(shares['unit'], '亿股')
        self.assertEqual(report_audit._unit_scale(shares['unit']), ('scalar', 1e8, None))
        self.assertEqual(stores['unit'], '家')
        self.assertEqual(report_audit._unit_scale(stores['unit']), ('scalar', 1.0, None))
        self.assertEqual(overseas_stores['unit'], '万家门店')
        self.assertEqual(
            report_audit._unit_scale(overseas_stores['unit']),
            ('scalar', 1e4, None),
        )
        self.assertEqual(oil['unit'], '美元/桶')
        self.assertEqual(wti['unit'], 'USD/bbl')

    def test_explicit_audit_scope_excludes_narrative_numbers(self):
        report = """
最新季报为2026Q1，顾东升54岁。十年后护城河是否还在？
<!-- audit:scope:start -->
| 指标 | 2025 |
|---|---:|
| 营业收入（亿元） | 100 |
| 门店数量（家） | 7330 |
<!-- audit:scope:end -->
存货周转天数超过400天时重新评估。
"""

        points = report_audit.extract_data_points(report)

        self.assertEqual(len(points), 2)
        self.assertTrue(any('营业收入' in point['label'] for point in points))
        self.assertTrue(any('门店数量' in point['label'] for point in points))
        self.assertFalse(any(point['reported_value'] in (1, 54, 10, 400) for point in points))

    def test_strict_report_lint_returns_all_unit_problems_before_sampling(self):
        report = """
<!-- audit:scope:start -->
| 指标 | 2025 |
|---|---:|
| 营业收入 | 100 |
| 归母净利润（亿元） | ~10 |
| 第二个无单位指标 | 20 |
<!-- audit:scope:end -->
"""

        errors = report_audit.collect_report_lint_errors(report)

        self.assertTrue(any('缺少显式单位' in error for error in errors))
        self.assertTrue(any('近似值' in error for error in errors))

    def test_lint_rejects_common_approximation_markers(self):
        for marker in (
            '≈', '>', '≤', '约为', '大约', '近', '超过', '不超过',
            '不低于', '至少', '至多', '逾', '不足',
        ):
            with self.subTest(marker=marker):
                report = f'''\
<!-- audit:scope:start -->
| 事实字段 | 报告值 |
|---|---:|
| 2025年营业收入 | {marker}100亿元 |
<!-- audit:scope:end -->
'''
                errors = report_audit.collect_report_lint_errors(report)
                self.assertTrue(any('近似值' in error for error in errors), errors)

    def test_lint_rejects_extra_columns_and_multiple_values_per_fact(self):
        report = '''\
<!-- audit:scope:start -->
| 事实字段 | 报告值 | 来源 |
|---|---:|---|
| 2025年营业收入 | 100亿元（同比10%） | 年报2025 |
<!-- audit:scope:end -->
'''

        errors = report_audit.collect_report_lint_errors(report)

        self.assertTrue(any('必须恰好两列' in error for error in errors), errors)
        self.assertTrue(any('恰好包含一个数值' in error for error in errors), errors)

    def test_extract_and_normalize_operating_volume_units(self):
        report = """
<!-- audit:scope:start -->
| 指标 | 2025 |
|---|---:|
| 油气当量产量（百万桶） | 1841.9 |
| 天然气销量（亿立方米） | 3147.13 |
| 天然气产量（十亿立方英尺） | 5150.3 |
| 成品油销量（万吨） | 16081.1 |
| 风光发电量（亿千瓦时） | 79.3 |
<!-- audit:scope:end -->
"""
        points = report_audit.validate_report_for_audit(report)
        units = {point['unit'] for point in points}

        self.assertEqual(
            units,
            {'百万桶', '亿立方米', '十亿立方英尺', '万吨', '亿千瓦时'},
        )
        self.assertEqual(
            report_audit._normalize_fetched_value(18.419, '亿桶', '百万桶'),
            1841.9,
        )
        self.assertAlmostEqual(
            report_audit._normalize_fetched_value(31471300, '万立方米', '亿立方米'),
            3147.13,
        )
        self.assertAlmostEqual(
            report_audit._normalize_fetched_value(1, '十亿立方英尺', '亿立方米'),
            0.28316846592,
        )

    def test_evidence_requires_value_and_unit_to_describe_same_observation(self):
        item = valid_result()
        item['reported_value'] = 10.0
        item['unit'] = '%'
        item['fetched_raw_value'] = 10.0
        item['fetched_raw_unit'] = '%'
        item['fetched_evidence'] = '10年国债收益率约2.5%，提供下行保护。'
        item['fetched_raw_value2'] = 10.0
        item['fetched_raw_unit2'] = '%'
        item['fetched_evidence2'] = '该指标为10%。'

        with self.assertRaisesRegex(ValueError, '来源单位相邻出现'):
            report_audit.validate_verdict_results([item])

    def test_source_snapshot_binds_excerpt_to_the_same_url(self):
        item = valid_result()
        with tempfile.TemporaryDirectory() as directory:
            for suffix in ('', '2'):
                source = item[f'fetched_source{suffix}']
                evidence = item[f'fetched_evidence{suffix}']
                path = os.path.join(directory, f'source{suffix or "1"}.json')
                digest = report_audit.write_source_snapshot(
                    source,
                    f'页面标题\n{evidence}\n页面结束',
                    path,
                )
                item[f'fetched_snapshot{suffix}'] = os.path.basename(path)
                item[f'fetched_snapshot_sha256{suffix}'] = digest

            report_audit.validate_verdict_results(
                [item],
                snapshot_base_dir=directory,
                require_snapshots=True,
            )

            item['fetched_source2'] += '?announcement=other'
            item['fetched_snapshot_sha2562'] = 'bad-digest'
            with self.assertRaises(ValueError) as raised:
                report_audit.validate_verdict_results(
                    [item],
                    snapshot_base_dir=directory,
                    require_snapshots=True,
                )
            self.assertIn('64 位小写 SHA-256', str(raised.exception))
            self.assertIn('绑定的 URL', str(raised.exception))

    def test_source_snapshot_rejects_oversized_output_before_writing(self):
        with tempfile.TemporaryDirectory() as directory:
            output = os.path.join(directory, 'snapshot.json')
            with mock.patch.object(report_audit, '_MAX_SOURCE_SNAPSHOT_BYTES', 100):
                with self.assertRaisesRegex(ValueError, '超过 10 MiB'):
                    report_audit.write_source_snapshot(
                        'https://example-data.com/report/1',
                        'x' * 100,
                        output,
                    )
            self.assertFalse(os.path.exists(output))

    def test_pass_receipt_binds_report_and_ledger(self):
        report = '''\
<!-- audit:scope:start -->
| 事实字段 | 报告值 |
|---|---:|
| 2025年营业收入 | 100亿元 |
| 2025年净利润 | 10亿元 |
| 2025年现金流 | 12亿元 |
<!-- audit:scope:end -->
'''
        with tempfile.TemporaryDirectory() as directory:
            report_path = os.path.join(directory, 'report.md')
            ledger_path = os.path.join(directory, 'ledger.json')
            with open(report_path, 'w', encoding='utf-8') as handle:
                handle.write(report)
            bundle = report_audit.build_audit_bundle(
                report,
                report_path=report_path,
                strict=True,
            )
            ledger = {
                'schema_version': report_audit._EVIDENCE_LEDGER_SCHEMA_VERSION,
                'population_digest': bundle['population_digest'],
                'evidence_scope_digest': bundle['evidence_scope_digest'],
                'observations': {
                    report_audit._fact_identity_key(item): [
                        {'slot': 1, 'snapshot_sha256': 'a' * 64},
                        {'slot': 2, 'snapshot_sha256': 'b' * 64},
                    ]
                    for item in bundle['results']
                },
            }
            with open(ledger_path, 'w', encoding='utf-8') as handle:
                json.dump(ledger, handle)
            with open(ledger_path, 'rb') as handle:
                original_ledger = handle.read()

            with mock.patch.object(
                report_audit,
                '_evidence_ledger_path',
                return_value=ledger_path,
            ):
                receipt_path = report_audit.write_pass_receipt(
                    report_path,
                    bundle,
                    ledger_path,
                )
                report_audit.validate_pass_receipt(report_path)

                with open(ledger_path, 'ab') as handle:
                    handle.write(b'\n')
                with self.assertRaisesRegex(ValueError, '证据账本已变化'):
                    report_audit.validate_pass_receipt(report_path)
                with open(ledger_path, 'wb') as handle:
                    handle.write(original_ledger)

                with open(report_path, 'a', encoding='utf-8') as handle:
                    handle.write('\n报告已变化。\n')
                with self.assertRaisesRegex(ValueError, 'report_text_sha256'):
                    report_audit.validate_pass_receipt(report_path)

            self.assertTrue(os.path.isfile(receipt_path))

    def test_new_ledger_version_uses_a_distinct_path_from_legacy_ledgers(self):
        bundle = report_audit.build_audit_bundle('| 指标 | 2025 |\n|---|---:|\n| 收入（亿元） | 100 |')
        path = report_audit._evidence_ledger_path('audit.json', bundle)
        legacy_name = (
            f'{bundle["population_digest"][:32]}.'
            f'{bundle["evidence_scope_digest"][:32]}.json'
        )

        self.assertNotEqual(os.path.basename(path), legacy_name)
        self.assertTrue(
            os.path.basename(path).startswith(
                f'v{report_audit._EVIDENCE_LEDGER_SCHEMA_VERSION}.'
            )
        )

    def test_preflight_collects_errors_across_multiple_items(self):
        first = valid_result()
        first['fetched_raw_value'] = '100'
        second = valid_result()
        second['id'] = 2
        second['fetched_source2'] = 'https://www.cninfo.com.cn/'

        with self.assertRaises(ValueError) as raised:
            report_audit.validate_verdict_results([first, second])

        message = str(raised.exception)
        self.assertIn('results[0].fetched_raw_value 必须是数值', message)
        self.assertIn('results[1].fetched_source2 必须是可定位证据', message)

    def test_preflight_cli_passes_without_creating_evidence_ledger(self):
        unique = uuid.uuid4().hex
        report = f"""
| 指标 | 数值 |
|---|---:|
| 收入{unique} | 100亿元 |
| 利润{unique} | 10亿元 |
| 现金流{unique} | 12亿元 |
"""
        with tempfile.TemporaryDirectory() as directory:
            report_path = os.path.join(directory, 'report.md')
            results_path = os.path.join(directory, 'audit.json')
            with open(report_path, 'w', encoding='utf-8') as handle:
                handle.write(report)

            bundle = report_audit.build_audit_bundle(report, report_path=report_path)
            for item in bundle['results']:
                value = item['reported_value']
                item.update({
                    'fetched_raw_value': value,
                    'fetched_raw_unit': '亿元',
                    'fetched_evidence': f'公告财务表显示该指标为 {value:g} 亿元。',
                    'fetched_source': (
                        f'https://data.eastmoney.com/report/{unique}-{item["id"]}.html'
                    ),
                    'fetched_locator': '财务指标表对应行',
                    'fetched_raw_value2': value,
                    'fetched_raw_unit2': '亿元',
                    'fetched_evidence2': f'年度报告披露该指标为 {value:g} 亿元。',
                    'fetched_source2': (
                        f'https://www.cninfo.com.cn/disclosure/{unique}-{item["id"]}'
                    ),
                    'fetched_locator2': '年度报告财务指标表对应行',
                })
            with open(results_path, 'w', encoding='utf-8') as handle:
                json.dump(bundle, handle, ensure_ascii=False)

            ledger_path = report_audit._evidence_ledger_path(
                results_path, bundle, report_path
            )
            self.assertFalse(os.path.exists(ledger_path))

            completed = subprocess.run(
                [
                    sys.executable,
                    report_audit.__file__,
                    'preflight',
                    '--results-file',
                    results_path,
                    '--report',
                    report_path,
                ],
                capture_output=True,
                text=True,
                encoding='utf-8',
                check=False,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn('审计预检通过', completed.stdout)
            self.assertFalse(os.path.exists(ledger_path))

    def test_extract_cli_requires_explicit_scope(self):
        report = """
| 指标 | 数值 |
|---|---:|
| 营业收入 | 100亿元 |
| 净利润 | 10亿元 |
| 现金流 | 12亿元 |
"""
        with tempfile.TemporaryDirectory() as directory:
            report_path = os.path.join(directory, 'report.md')
            output_path = os.path.join(directory, 'audit.json')
            with open(report_path, 'w', encoding='utf-8') as handle:
                handle.write(report)

            completed = subprocess.run(
                [
                    sys.executable,
                    report_audit.__file__,
                    'extract',
                    '--report',
                    report_path,
                    '--output',
                    output_path,
                ],
                capture_output=True,
                text=True,
                encoding='utf-8',
                check=False,
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertIn('缺少显式审计区', completed.stderr)
            self.assertFalse(os.path.exists(output_path))

    def test_strict_preflight_requires_bound_source_snapshots(self):
        report = """
<!-- audit:scope:start -->
| 事实字段 | 报告值 |
|---|---:|
| 2025年营业收入 | 100亿元 |
| 2025年净利润 | 10亿元 |
| 2025年现金流 | 12亿元 |
<!-- audit:scope:end -->
"""
        with tempfile.TemporaryDirectory() as directory:
            report_path = os.path.join(directory, 'report.md')
            results_path = os.path.join(directory, 'audit.json')
            with open(report_path, 'w', encoding='utf-8') as handle:
                handle.write(report)
            bundle = report_audit.build_audit_bundle(
                report,
                report_path=report_path,
                strict=True,
            )
            for item in bundle['results']:
                value = item['reported_value']
                item.update({
                    'fetched_raw_value': value,
                    'fetched_raw_unit': '亿元',
                    'fetched_evidence': f'公告披露该指标为 {value:g} 亿元。',
                    'fetched_source': f'https://data.eastmoney.com/report/{item["id"]}.html',
                    'fetched_locator': '财务指标表对应行',
                    'fetched_raw_value2': value,
                    'fetched_raw_unit2': '亿元',
                    'fetched_evidence2': f'年报披露该指标为 {value:g} 亿元。',
                    'fetched_source2': f'https://www.cninfo.com.cn/disclosure/{item["id"]}',
                    'fetched_locator2': '年度报告财务指标表对应行',
                })
            with open(results_path, 'w', encoding='utf-8') as handle:
                json.dump(bundle, handle, ensure_ascii=False)

            completed = subprocess.run(
                [
                    sys.executable,
                    report_audit.__file__,
                    'preflight',
                    '--results-file',
                    results_path,
                    '--report',
                    report_path,
                ],
                capture_output=True,
                text=True,
                encoding='utf-8',
                check=False,
            )

            self.assertNotEqual(completed.returncode, 0)
            self.assertIn('fetched_snapshot', completed.stderr)

            for item in bundle['results']:
                for suffix in ('', '2'):
                    snapshot_path = os.path.join(
                        directory,
                        f'{item["id"]}-{suffix or "1"}.snapshot.json',
                    )
                    digest = report_audit.write_source_snapshot(
                        item[f'fetched_source{suffix}'],
                        f'页面标题\n{item[f"fetched_evidence{suffix}"]}\n页面结尾',
                        snapshot_path,
                    )
                    item[f'fetched_snapshot{suffix}'] = os.path.basename(snapshot_path)
                    item[f'fetched_snapshot_sha256{suffix}'] = digest
            with open(results_path, 'w', encoding='utf-8') as handle:
                json.dump(bundle, handle, ensure_ascii=False)

            completed = subprocess.run(
                [
                    sys.executable,
                    report_audit.__file__,
                    'preflight',
                    '--results-file',
                    results_path,
                    '--report',
                    report_path,
                ],
                capture_output=True,
                text=True,
                encoding='utf-8',
                check=False,
            )

            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertIn('审计预检通过', completed.stdout)

    def test_audit_bundle_is_bound_to_report_and_sampled_values(self):
        report = """
| 指标 | 2024 | 2025 |
|---|---:|---:|
| 营业收入 | 90亿元 | 100亿元 |
| 净利润 | 9亿元 | 10亿元 |
"""
        bundle = report_audit.build_audit_bundle(report)

        results = report_audit.validate_audit_bundle(bundle, report)
        self.assertEqual(len(results), 3)

        bundle['results'][0]['reported_value'] += 1
        with self.assertRaisesRegex(ValueError, '报告值'):
            report_audit.validate_audit_bundle(bundle, report)

    def test_audit_bundle_rejects_removed_items_and_changed_report(self):
        report = """
| 指标 | 2025 |
|---|---:|
| 营业收入 | 100亿元 |
| 净利润 | 10亿元 |
| 经营现金流 | 12亿元 |
"""
        bundle = report_audit.build_audit_bundle(report)
        bundle['results'].pop()

        with self.assertRaisesRegex(ValueError, '数量已变化'):
            report_audit.validate_audit_bundle(bundle, report)

        fresh = report_audit.build_audit_bundle(report)
        with self.assertRaisesRegex(ValueError, '报告内容已变化'):
            report_audit.validate_audit_bundle(fresh, report + '\n补充')

    def test_audit_bundle_rejects_changed_ratio_or_seed(self):
        report = """
| 指标 | 2025 |
|---|---:|
| 营业收入 | 100亿元 |
| 净利润 | 10亿元 |
| 经营现金流 | 12亿元 |
"""
        bundle = report_audit.build_audit_bundle(report)
        bundle['sample_ratio'] = 0.01
        with self.assertRaisesRegex(ValueError, '固定抽样比例'):
            report_audit.validate_audit_bundle(bundle, report)

        bundle = report_audit.build_audit_bundle(report)
        bundle['sample_seed'] += 1
        with self.assertRaisesRegex(ValueError, '不得换 seed'):
            report_audit.validate_audit_bundle(bundle, report)

    def test_reordering_facts_does_not_refresh_population_or_sample(self):
        rows = [
            '| 营业收入 | 100亿元 |',
            '| 净利润 | 10亿元 |',
            '| 经营现金流 | 12亿元 |',
            '| 总资产 | 300亿元 |',
            '| 净资产 | 150亿元 |',
            '| 现金余额 | 80亿元 |',
            '| 总负债 | 150亿元 |',
            '| 资本开支 | 20亿元 |',
        ]
        report1 = '| 指标 | 2025 |\n|---|---:|\n' + '\n'.join(rows)
        report2 = '| 指标 | 2025 |\n|---|---:|\n' + '\n'.join(reversed(rows))

        first = report_audit.build_audit_bundle(report1)
        second = report_audit.build_audit_bundle(report2)

        self.assertEqual(first['population_digest'], second['population_digest'])
        self.assertEqual(first['sample_seed'], second['sample_seed'])
        self.assertEqual(
            {
                (item['label'], item['reported_value'], item['unit'])
                for item in first['results']
            },
            {
                (item['label'], item['reported_value'], item['unit'])
                for item in second['results']
            },
        )

    def test_evidence_ledger_locks_first_observation_for_same_url(self):
        item = valid_result()
        bundle = {
            'report_path_sha256': '0' * 64,
            'population_digest': 'a' * 64,
            'evidence_scope_digest': 'b' * 64,
        }
        with tempfile.TemporaryDirectory() as directory:
            results_path = os.path.join(directory, 'audit.json')
            with open(results_path, 'w', encoding='utf-8') as handle:
                handle.write('{}')
            report_audit.enforce_evidence_ledger(results_path, bundle, [item])

            item['id'] = 99
            item['fetched_raw_value2'] = 9999.0
            item['fetched_evidence2'] = '年度报告合并利润表：营业收入 9,999 百万元。'
            with self.assertRaisesRegex(ValueError, '首次判决中锁定'):
                report_audit.enforce_evidence_ledger(results_path, bundle, [item])

    def test_evidence_ledger_canonicalizes_same_page_url_variants(self):
        item = valid_result()
        bundle = {
            'report_path_sha256': '1' * 64,
            'population_digest': 'c' * 64,
            'evidence_scope_digest': 'd' * 64,
        }
        with tempfile.TemporaryDirectory() as directory:
            results_path = os.path.join(directory, 'audit.json')
            with open(results_path, 'w', encoding='utf-8') as handle:
                handle.write('{}')
            report_audit.enforce_evidence_ledger(results_path, bundle, [item])

            item['fetched_source2'] = (
                'http://cninfo.com.cn/new/disclosure/other/../detail/'
                '%34%35%36/?retry=1#income'
            )
            item['fetched_raw_value2'] = 9999.0
            item['fetched_evidence2'] = '年度报告合并利润表：营业收入 9,999 百万元。'
            with self.assertRaisesRegex(ValueError, '第 2 来源'):
                report_audit.enforce_evidence_ledger(results_path, bundle, [item])

    def test_evidence_ledger_rejects_source_shopping_after_a_failed_verdict(self):
        item = valid_result()
        bundle = {
            'report_path_sha256': '7' * 64,
            'population_digest': '7' * 64,
            'evidence_scope_digest': '8' * 64,
        }
        with tempfile.TemporaryDirectory() as directory:
            results_path = os.path.join(directory, 'audit.json')
            report_audit.enforce_evidence_ledger(
                results_path, bundle, [item]
            )

            item['fetched_source2'] = (
                'https://www.cninfo.com.cn/new/disclosure/detail/999'
            )
            item['fetched_locator2'] = '另一公告第 8 页，合并利润表'
            with self.assertRaisesRegex(ValueError, '不得在看到判决结果后更换 URL'):
                report_audit.enforce_evidence_ledger(
                    results_path, bundle, [item]
                )

    def test_evidence_ledger_identity_ignores_results_filename(self):
        item = valid_result()
        bundle = {
            'report_path_sha256': '2' * 64,
            'population_digest': 'e' * 64,
            'evidence_scope_digest': 'f' * 64,
        }
        with tempfile.TemporaryDirectory() as directory:
            first_path = os.path.join(directory, 'first-audit.json')
            second_path = os.path.join(directory, 'renamed-audit.json')
            report_audit.enforce_evidence_ledger(first_path, bundle, [item])

            item['fetched_raw_value2'] = 9999.0
            item['fetched_evidence2'] = '年度报告合并利润表：营业收入 9,999 百万元。'
            with self.assertRaisesRegex(ValueError, '首次判决中锁定'):
                report_audit.enforce_evidence_ledger(second_path, bundle, [item])

    def test_evidence_ledger_identity_ignores_report_copy_or_rename(self):
        bundle = {
            'report_path_sha256': '9' * 64,
            'population_digest': '1' * 64,
            'evidence_scope_digest': '2' * 64,
        }
        with tempfile.TemporaryDirectory() as directory:
            first = report_audit._evidence_ledger_path(
                os.path.join(directory, 'first-audit.json'),
                bundle,
                os.path.join(directory, 'reports', 'first.md'),
            )
            renamed = report_audit._evidence_ledger_path(
                os.path.join(directory, 'renamed-audit.json'),
                {**bundle, 'report_path_sha256': '8' * 64},
                os.path.join(directory, 'copied', 'renamed.md'),
            )

            self.assertEqual(first, renamed)

    def test_evidence_ledger_reuses_an_unlocked_file_left_by_a_crash(self):
        item = valid_result()
        bundle = {
            'report_path_sha256': '3' * 64,
            'population_digest': 'a' * 64,
            'evidence_scope_digest': 'c' * 64,
        }
        with tempfile.TemporaryDirectory() as directory:
            results_path = os.path.join(directory, 'audit.json')
            ledger_path = report_audit._evidence_ledger_path(
                results_path, bundle
            )
            os.makedirs(os.path.dirname(ledger_path), exist_ok=True)
            lock_path = f'{ledger_path}.lock'
            with open(lock_path, 'w', encoding='ascii') as handle:
                handle.write('crashed')

            actual = report_audit.enforce_evidence_ledger(
                results_path, bundle, [item]
            )

            self.assertEqual(actual, ledger_path)
            self.assertTrue(os.path.isfile(ledger_path))
            self.assertTrue(os.path.isfile(lock_path))


if __name__ == '__main__':
    unittest.main()
