#!/usr/bin/env python3
"""Report Audit Tool for AI Berkshire.

数据抽检工具：从研究报告中抽取15%的财务数据点（至少3项或全量），与可靠信源比对，
通过则准出，不通过则打回并说明原因。

Zero external dependencies — uses only Python stdlib.
Requires Python >= 3.7.

工作流程（五步）：
  Step 1 — 全量检查显式审计事实区：
    python3 tools/report_audit.py lint --report reports/xxx.md

  Step 2 — 提取数据点，随机抽样15%（至少3项或全量）：
    python3 tools/report_audit.py extract --report reports/xxx.md \
      --output tmp/audit-results.json

  Step 3 — Agent 对抽检清单中的每个数据点，从两个独立可靠信源取数，
            原样填写来源数值、来源单位、原文摘录、直接 URL 和页内定位；
            并用 source-snapshot 将联网工具保存的页面正文绑定到 URL；
            fetched_value 保持 null，由 verdict 统一换算。

  Step 4 — 一次性预检全部类型、URL、摘录、正文快照和单位问题：
    python3 tools/report_audit.py preflight --results-file tmp/audit-results.json \
      --report reports/xxx.md

  Step 5 — 输入核验结果，输出准出/打回判决：
    python3 tools/report_audit.py verdict --results-file tmp/audit-results.json \
      --report reports/xxx.md

  预览抽检清单（不做网络验证）：
    python3 tools/report_audit.py extract --report reports/xxx.md --dry-run
"""

import argparse
import errno
import hashlib
import ipaddress
import json
import math
import os
import re
import sys
import time
from urllib.parse import urlsplit

if os.name == 'nt':
    for stream in (sys.stdout, sys.stderr):
        if hasattr(stream, 'reconfigure'):
            stream.reconfigure(encoding='utf-8')

# ---------------------------------------------------------------------------
# 数据点提取：从 Markdown 报告中识别财务数字
# ---------------------------------------------------------------------------

# 匹配模式：数字 + 单位，前面有上下文标签
# 例：收入：1,239亿元、PE 18.8x、毛利率 56%、市值 ~$5,670亿
_PATTERNS = [
    # 百分比
    (r'([\d,，\.]+)\s*%',                        '%',    'percent'),
    # 亿元/亿美元/亿港元
    (r'([\d,，\.]+)\s*亿(元|美元|港元|RMB|USD|HKD)?', '亿',    'hundred_million'),
    # 倍数 PE/PB/PS
    (r'([\d,，\.]+)\s*[xX倍]',                   'x',    'multiple'),
    # 万亿
    (r'([\d,，\.]+)\s*万亿',                      '万亿', 'trillion'),
    # 美元绝对值（B/T）
    (r'\$\s*([\d,，\.]+)\s*([BMT亿])',             '$',    'usd_abs'),
    # 纯整数（如市值、收入、用户数等，出现在表格 | 里）
    (r'\|\s*[~约]?\$?([\d,，\.]+)\s*\|',          '',     'table_num'),
]

_LABEL_RE = re.compile(
    r'(?P<label>[^\|\n：:]{2,25})[：:\s]+[~约]?\$?(?P<num>[\d,，\.]+)\s*(?P<unit>亿[元美港]?元?|万亿|[xX倍]|%|[BMT])?'
)

_TABLE_ROW_RE = re.compile(
    r'\|\s*(?P<label>[^|]{1,40})\s*\|\s*[~约]?\$?(?P<num>[\d,，\.]+)\s*(?P<unit>亿[元美港]?元?|万亿|[xX倍]|%|[BMT])?\s*\|'
)

_NON_FACT_CUES = (
    '假设', '假定', '情景', '乐观', '中性', '悲观', '目标pe', '目标pb', '目标价',
    '建议', '加仓', '减仓', '买入信号', '卖出信号', '概率', '置信度', '评分',
    '误差', '验证', '安全边际', '敏感性', '预测', '预计', '预期', '估算', '估计',
    '行业常识', '止损', '阈值', '上行空间', '下行空间',
)


def _clean_num(s: str) -> float:
    """把带逗号、中文逗号的数字字符串转为 float。"""
    s = s.replace(',', '').replace('，', '').strip()
    # 归一化各类正负号为 ASCII：'−'(U+2212)、'–'(U+2013)、'－'(U+FF0D)、'＋'(U+FF0B)
    for ch in ('−', '–', '－'):
        s = s.replace(ch, '-')
    s = s.replace('＋', '+')
    parenthesized = s.startswith('(')
    if parenthesized:
        s = s[1:].strip()
        if s.endswith(')'):
            s = s[:-1].strip()
    s = s.replace('$', '')
    try:
        value = float(s)
        return -abs(value) if parenthesized else value
    except ValueError:
        return None


def _is_valid_label(label: str) -> bool:
    """判断标签是否是有意义的财务字段名，过滤噪声。"""
    label = label.strip()
    # 太短
    if len(label) < 2:
        return False
    # 纯数字或纯年份
    if re.fullmatch(r'[\d\s年季度Q]+', label):
        return False
    # 以符号/markdown标记开头
    if re.match(r'^[+\-\*#\|~\$>_`]', label):
        return False
    # 含有 markdown 粗体/代码标记
    if '**' in label or '`' in label or '__' in label:
        return False
    # 标签含有纯增速符号（如 +56%、-13% 单独作标签）
    if re.fullmatch(r'[+\-]?\d+(\.\d+)?%', label):
        return False
    # 常见无意义标签
    _SKIP = {'来源', 'sources', 'source', '说明', '注意', '备注', '数据来源',
             'n/a', '—', '-', '/', '合计', 'total', '单位', '趋势'}
    if label.lower() in _SKIP:
        return False
    return True


def _is_auditable_fact(label: str, local_context: str) -> bool:
    """排除估值输入、主观判断和条件阈值，只让外部事实进入来源抽检。

    这类数字本来就没有两个独立“真实值”。把它们混入样本会诱导执行者拿情景
    假设冒充外部复核结果，正是本次日志里改写第二来源以强行准出的触发条件。
    """
    context = f'{label} {local_context}'.lower()
    if any(cue in context for cue in _NON_FACT_CUES):
        return False
    # 财务表常用 2026E / FY27F 表示预测值；它们不是已经发生、可由两个来源
    # 复核出的历史事实。A/Actual 不在此列。
    return re.search(r'(?:20\d{2}|fy\d{2,4}|q[1-4])\s*[ef](?:\b|[^a-z])', context) is None


_COUNT_UNIT_PATTERN = (
    r'(?:(?:万亿|十亿|百万|亿|万|千)?'
    r'(?:家门店|门店|股|人|户|个|家|台|辆|项|座))'
)
_PHYSICAL_UNIT_PATTERN = (
    r'(?:(?:万亿|十亿|百万|亿|万|千)?'
    r'(?:立方英尺|立方米|千瓦时|平方米|桶油当量|桶|吨|升))'
)
_PRICE_UNIT_PATTERN = (
    r'(?:(?i:人民币|RMB|CNY|美元|USD|US\$|港元|HKD|HK\$|元|\$)'
    r'[/／](?i:克|盎司|桶|升|吨|股|g|oz|bbl|l|share))'
)
_AUDIT_UNIT_PATTERN = (
    rf'(?:{_PRICE_UNIT_PATTERN}|{_PHYSICAL_UNIT_PATTERN}|{_COUNT_UNIT_PATTERN}|'
    r'亿[元美港]?元?|万亿|[xX倍]|%|[BMT])'
)
# 符号位须涵盖 ASCII 正负号、Unicode 减号(U+2212)、en-dash(U+2013)、全角正负号，
# 否则 "-1.72%" 会被抓成 "1.72"，核验时符号相反、偏差 200%，产生假打回。
_AUDIT_NUMBER_TOKEN = (
    r'\(?[+\-−–－]?\$?[+\-−–－]?(?:\d[\d,，]*(?:\.\d+)?|\.\d+)\)?'
)

# 两列表格行：| 标签 | 数值 unit |（专为财务报告的 KV 表设计）
_KV_TABLE_RE = re.compile(
    rf'^\|\s*(?P<label>[^|*\n]{{2,40}}?)\s*\|\s*[~约]?(?P<num>{_AUDIT_NUMBER_TOKEN})\s*'
    rf'(?P<unit>{_AUDIT_UNIT_PATTERN})?\)?\s*[\|（\(]'
)

# 带标签的 KV 行：标签：数值 单位
_KV_LABEL_RE = re.compile(
    r'(?P<label>[\u4e00-\u9fa5A-Za-z][^\|\n：:*]{1,30})[：:]\s*[~约]?'
    rf'(?P<num>{_AUDIT_NUMBER_TOKEN})\s*'
    rf'(?P<unit>{_AUDIT_UNIT_PATTERN})?\)?'
)

_UNIT_CONTEXT_RE = re.compile(
    r'('
    rf'{_PHYSICAL_UNIT_PATTERN}'
    rf'|{_COUNT_UNIT_PATTERN}'
    rf'|{_PRICE_UNIT_PATTERN}'
    r'|(?:RMB|CNY|USD|HKD)\s*(?:trillion|billion|million|thousand|[TBMK])'
    r'|(?:trillion|billion|million|thousand)\s*(?:RMB|CNY|USD|HKD)'
    r'|人民币(?:万亿|十亿|亿|百万|万|千)?元'
    r'|万亿(?:美元|港元|元)?'
    r'|十亿(?:美元|港元|元)?'
    r'|亿(?:美元|港元|元)?'
    r'|百万(?:美元|港元|元)?'
    r'|万(?:美元|港元|元)'
    r'|千(?:美元|港元|元)'
    r'|美元|港元|元'
    r'|trillion|billion|million|thousand'
    r'|万|千'
    r'|百分比|%'
    r'|倍|(?<![A-Za-z])[xX](?![A-Za-z])'
    r')',
    re.IGNORECASE,
)

_CELL_VALUE_RE = re.compile(
    rf'[~约]?(?P<num>{_AUDIT_NUMBER_TOKEN})\s*'
    rf'(?P<unit>{_AUDIT_UNIT_PATTERN})?\)?',
    re.IGNORECASE,
)


def _is_year_metadata_number(text: str, match) -> bool:
    raw_num = match.group('num')
    normalized_num = re.sub(r'[\s()$,+，−-]', '', raw_num)
    after_num = text[match.end('num'):]
    return bool(
        re.fullmatch(r'20\d{2}', normalized_num)
        and re.match(r'\s*年(?:度)?', after_num)
    )


def _is_metadata_or_range_number(text: str, match) -> bool:
    """排除年份元数据和区间端点，避免把它们与后续单位拼成伪事实。"""
    if _is_year_metadata_number(text, match):
        return True

    # ``82-91%`` / ``82~91%`` 是区间，不是两个可独立核验的单值。
    raw_num = match.group('num')
    after_num = text[match.end('num'):]
    if re.match(r'\s*[-–—~～至到]\s*[+\-−]?\d', after_num):
        return True
    before_num = text[:match.start('num')]
    if (
        raw_num.lstrip().startswith(('-', '−'))
        and re.search(r'\d\s*$', before_num)
    ) or re.search(r'\d\s*[-–—~～至到]\s*$', before_num):
        return True
    return False


def _first_auditable_value_match(text: str, start: int = 0):
    clause_end = len(text)
    for match in _CELL_VALUE_RE.finditer(text, start):
        if match.start() >= clause_end:
            return None
        if _is_year_metadata_number(text, match):
            clause_break = re.search(r'[；;。，,]', text[match.end():])
            if clause_break:
                clause_end = min(
                    clause_end, match.end() + clause_break.start()
                )
            continue
        if _is_metadata_or_range_number(text, match):
            # A range makes the current value ambiguous; never jump over it and
            # assign a later field's scalar to this label.
            return None
        else:
            return match
    return None


def _unit_from_context(cell_suffix: str, *declared_contexts: str) -> str:
    """优先从行名/列标题取单位，再检查当前数值后的单元格文本。"""
    for value in declared_contexts:
        if not isinstance(value, str):
            continue
        # 行名和标题可能含公司名或自然语言中的“亿/万/千”；只接受括号、
        # 方括号、斜杠后缀或显式“单位：”中的单位，避免把“亿纬锂能”等误判。
        candidates = re.findall(r'[（(【\[]([^）)】\]]{1,40})[）)】\]]', value)
        declared = re.search(r'(?:单位\s*[：:]|[/／])\s*([^\s|]{1,24})\s*$', value)
        if declared:
            candidates.append(declared.group(1))
        for candidate in candidates:
            match = _UNIT_CONTEXT_RE.search(candidate)
            if match:
                return match.group(1).strip()

    # 单元格中的首个数值可能是金额，后面的括号才是同比百分比，例如
    # ``844.92(+8.12%)``。只有行名/标题没有声明单位时才用这个尾随上下文。
    if isinstance(cell_suffix, str):
        match = _UNIT_CONTEXT_RE.search(cell_suffix)
        if match:
            return match.group(1).strip()
    return ''


def _currency_qualified_unit(number_token: str, unit: str) -> str:
    """保留数值前货币符号，避免 ``$5.2B`` 在提取后退化成无币种的 ``B``。"""
    if '$' not in number_token:
        return unit
    normalized = unit.lower()
    if any(token in normalized for token in ('usd', '美元', 'us$')):
        return unit
    return f'USD {unit}'.strip()


def _is_md_table_separator(line: str) -> bool:
    """Recognize a Markdown separator row with or without outer pipes."""
    cells = line.strip().split('|')
    if cells and not cells[0].strip():
        cells = cells[1:]
    if cells and not cells[-1].strip():
        cells = cells[:-1]
    return len(cells) >= 2 and all(
        re.fullmatch(r':?-{3,}:?', cell.strip()) is not None
        for cell in cells
    )


def _parse_md_tables(lines: list) -> list:
    """解析 Markdown 表格，返回标签、列名、值、单位、行号、原行和当前单元格。"""
    results = []
    i = 0
    code_fence = None
    while i < len(lines):
        line = lines[i].strip()
        if code_fence is not None:
            if line.startswith(code_fence):
                code_fence = None
            i += 1
            continue
        if line.startswith(('```', '~~~')):
            code_fence = line[:3]
            i += 1
            continue
        # 检测表头行（含 | 且不是分隔行）
        if '|' in line and not _is_md_table_separator(line):
            header_cells = line.split('|')
            if header_cells and not header_cells[0].strip():
                header_cells = header_cells[1:]
            if header_cells and not header_cells[-1].strip():
                header_cells = header_cells[:-1]
            headers_raw = [h.strip().strip('*_').strip() for h in header_cells]
            # 下一行应是分隔行
            if i + 1 < len(lines) and _is_md_table_separator(lines[i + 1]):
                i += 2  # 跳过分隔行
                # 读数据行
                while i < len(lines):
                    dline = lines[i].strip()
                    if not dline or '|' not in dline:
                        break
                    data_cells = dline.split('|')
                    if data_cells and not data_cells[0].strip():
                        data_cells = data_cells[1:]
                    if data_cells and not data_cells[-1].strip():
                        data_cells = data_cells[:-1]
                    cells = [c.strip().strip('*_~').strip() for c in data_cells]
                    if len(cells) < 2:
                        i += 1
                        continue
                    row_label = cells[0]
                    for col_idx, cell in enumerate(cells[1:], start=1):
                        col_header = headers_raw[col_idx] if col_idx < len(headers_raw) else f'列{col_idx}'
                        # 提取 cell 中的数字+单位
                        m = _first_auditable_value_match(cell)
                        if m:
                            val = _clean_num(m.group('num'))
                            unit = (m.group('unit') or '').strip() or _unit_from_context(
                                cell[m.end():],
                                row_label,
                                col_header,
                                headers_raw[0] if headers_raw else '',
                            )
                            unit = _currency_qualified_unit(m.group('num'), unit)
                            if val is not None and abs(val) <= 1e15:
                                results.append(
                                    (row_label, col_header, val, unit, i + 1, dline, cell)
                                )
                    i += 1
                continue
        i += 1
    return results


_AUDIT_SCOPE_START = '<!-- audit:scope:start -->'
_AUDIT_SCOPE_END = '<!-- audit:scope:end -->'


def _has_explicit_audit_scope(md_text: str) -> bool:
    return _AUDIT_SCOPE_START in md_text or _AUDIT_SCOPE_END in md_text


def _scoped_report_text(md_text: str) -> str:
    """只保留显式审计区，同时保留原始行号。

    新报告把可由外部信源核验的事实放进唯一的审计区。叙述、日期、年龄、
    情景假设和操作阈值留在区外，从源头避免启发式提取器把它们当成事实。
    没有标记的旧报告仍按旧行为解析；CLI 严格模式会在生成审计包前拒绝它。
    """
    lines = md_text.split('\n')
    starts = [i for i, line in enumerate(lines) if line.strip() == _AUDIT_SCOPE_START]
    ends = [i for i, line in enumerate(lines) if line.strip() == _AUDIT_SCOPE_END]
    if not starts and not ends:
        return md_text
    if len(starts) != 1 or len(ends) != 1 or starts[0] >= ends[0]:
        raise ValueError(
            '报告必须包含且只包含一对有序的 '
            f'{_AUDIT_SCOPE_START} / {_AUDIT_SCOPE_END} 标记'
        )
    start, end = starts[0], ends[0]
    return '\n'.join(
        line if start < index < end else ''
        for index, line in enumerate(lines)
    )


def extract_data_points(md_text: str) -> list:
    """从 Markdown 报告中提取所有可识别的财务数据点。

    覆盖三类结构：
      1. 多列 Markdown 表格（最主要的来源）：(行标签 + 列标题) → 数值
      2. 带冒号的 KV 行：标签：数值 单位
      3. 含 Markdown 加粗/代码标记的 KV 行

    返回 list of dict：
      {id, label, reported_value, unit, raw_text, line_number}
    """
    md_text = _scoped_report_text(md_text)
    points = []
    seen = set()

    def _add(label, val, unit, lineno, raw, local_context=None):
        label = re.sub(r'[\*_`]+', '', label).strip()
        if not _is_valid_label(label):
            return
        if not _is_auditable_fact(label, local_context or label):
            return
        if val is None or abs(val) > 1e15:
            return
        # 过滤纯年份/季度
        if re.fullmatch(r'(20\d{2}|Q[1-4]|\d{4}\s*Q[1-4])', label.strip()):
            return
        key = f"{label}|{round(val,4)}|{unit}"
        if key in seen:
            return
        seen.add(key)
        points.append({
            'id': len(points) + 1,
            'label': label,
            'reported_value': val,
            'unit': unit,
            'raw_text': raw[:120],
            'line_number': lineno,
        })

    lines = md_text.split('\n')
    code_fence = None

    # --- 1. 多列表格 ---
    for row_label, col_header, val, unit, lineno, raw, cell in _parse_md_tables(lines):
        # 跳过无意义行标签
        if not _is_valid_label(row_label):
            continue
        # 跳过无意义列标题（YoY增速列单独标注，不作为待核验数据）
        if col_header.upper() in ('YOY', 'YOY增速', '增速', '同比', '变化', '趋势', '说明', '备注'):
            continue
        # label = "行标签 · 列标题"（若列标题是行标签的补充）
        if col_header and col_header != row_label:
            label = f"{row_label} · {col_header}"
        else:
            label = row_label
        # 只用本数值自己的行标签、列标题和单元格判断是否属于事实。不能因为同一行
        # 另一个“预计/建议”列，就把真实历史收入也从抽检池排除。
        _add(label, val, unit, lineno, raw, f'{row_label} {col_header} {cell}')

    # --- 2. KV 冒号行 ---
    for lineno, line in enumerate(lines, start=1):
        stripped = line.strip()
        if code_fence is not None:
            if stripped.startswith(code_fence):
                code_fence = None
            continue
        if stripped.startswith(('```', '~~~')):
            code_fence = stripped[:3]
            continue
        if stripped.startswith('> ') or re.match(r'^#{1,6}\s', stripped):
            continue
        if '|' in stripped:
            continue  # 表格已在上面处理

        normalized = stripped.replace('**', '').replace('__', '').replace('`', '')
        for m in _KV_LABEL_RE.finditer(normalized):
            label = m.group('label')
            # 只使用与当前标签直接相邻的数值。若它是区间或年份，应跳过当前
            # 标签，不能越过分号把后续字段的值错误归到这个标签。
            value_match = m
            if _is_year_metadata_number(normalized, value_match):
                clause_break = re.search(r'[；;。，,]', normalized[value_match.end():])
                clause_end = (
                    value_match.end() + clause_break.start()
                    if clause_break
                    else len(normalized)
                )
                candidate = _first_auditable_value_match(
                    normalized, value_match.end('num')
                )
                if candidate is None or candidate.start() >= clause_end:
                    continue
                value_match = candidate
            elif _is_metadata_or_range_number(normalized, value_match):
                continue
            val = _clean_num(value_match.group('num'))
            trailing_clause = re.split(
                r'[；;。，,]', normalized[value_match.end():], maxsplit=1
            )[0]
            unit = (value_match.group('unit') or '').strip() or _unit_from_context(
                trailing_clause, label
            )
            unit = _currency_qualified_unit(value_match.group('num'), unit)
            local = f'{normalized[m.start():value_match.end()]} {trailing_clause[:16]}'
            _add(label, val, unit, lineno, stripped, local)

    return points


def sample_points(points: list, ratio: float = 0.15, seed: int = None) -> list:
    """按 seed 的稳定哈希秩抽取 ratio 比例的数据点，样本不足时最多取全量。"""
    if (
        isinstance(ratio, bool)
        or not isinstance(ratio, (int, float))
        or not math.isfinite(float(ratio))
        or not 0 < ratio <= 1
    ):
        raise ValueError('ratio 必须是位于 (0, 1] 的有限数值')
    n = max(3, math.ceil(len(points) * ratio))
    n = min(n, len(points))
    stable_seed = 0 if seed is None else seed

    def rank(point):
        identity = json.dumps(
            _fact_identity(point),
            ensure_ascii=False,
            separators=(',', ':'),
        )
        return (
            hashlib.sha256(f'{stable_seed}\0{identity}'.encode('utf-8')).digest(),
            identity,
        )

    sampled = sorted(points, key=rank)[:n]
    # 按行号排序，方便人工比对
    return sorted(sampled, key=lambda p: p['line_number'])


# ---------------------------------------------------------------------------
# 准出/打回判决
# ---------------------------------------------------------------------------

_TOLERANCE = 0.01   # 1% 容差
_AUDIT_SCHEMA_VERSION = 5
_AUDIT_SAMPLE_RATIO = 0.15
_SAMPLE_POLICY = 'population-sha256-v2'
_SOURCE_SNAPSHOT_POLICY = 'source-bound-text-v1'
_EVIDENCE_LEDGER_SCHEMA_VERSION = 6
_PASS_RECEIPT_SCHEMA_VERSION = 1
_MAX_SOURCE_SNAPSHOT_BYTES = 10 * 1024 * 1024
_AUDIT_POINT_FIELDS = (
    'id', 'label', 'reported_value', 'unit', 'line_number', 'raw_text',
)


def _pct_diff(reported: float, fetched: float) -> float:
    """相对偏差 (absolute)。"""
    if reported == 0:
        return 0.0 if fetched == 0 else float('inf')
    return abs(reported - fetched) / abs(reported)


def _pair_pct_diff(first: float, second: float) -> float:
    """两个来源之间的对称相对偏差，避免选择哪一个作分母改变判决。"""
    scale = max(abs(first), abs(second))
    if scale == 0:
        return 0.0
    return abs(first - second) / scale


def _normalized_source_path(path: str) -> str:
    """规范化 URL 路径中的无保留百分号编码和点段。"""
    def normalize_percent_encoding(match):
        value = int(match.group(1), 16)
        char = chr(value)
        return char if re.fullmatch(r'[A-Za-z0-9\-._~]', char) else f'%{value:02X}'

    path = re.sub(r'%([0-9a-fA-F]{2})', normalize_percent_encoding, path)
    normalized_segments = []
    for segment in path.split('/'):
        if segment in ('', '.'):
            continue
        if segment == '..':
            if normalized_segments:
                normalized_segments.pop()
            continue
        normalized_segments.append(segment)
    return '/' + '/'.join(normalized_segments)


def _source_publisher(source: str) -> str:
    """返回 URL 的发布方域名；非法或不可定位来源返回空串。"""
    if not isinstance(source, str) or not source or source != source.strip():
        return ''
    if any(ch.isspace() or ord(ch) < 0x20 or ord(ch) == 0x7f for ch in source):
        return ''
    try:
        parsed = urlsplit(source)
        host = parsed.hostname
        parsed.port
    except ValueError:
        return ''
    if (
        parsed.scheme not in ('http', 'https')
        or not host
        or parsed.username is not None
        or parsed.password is not None
    ):
        return ''
    # 编码后的斜杠、反斜杠或二次编码百分号在不同代理/站点上可能被再次解码，
    # 让检测器看到“具体路径”、实际请求却落到首页。审计证据宁可要求换用明确 URL。
    if '\\' in parsed.path or re.search(r'%(?:2f|5c|25)', parsed.path, re.IGNORECASE):
        return ''
    # 查询参数不能把发布方首页伪装成直接证据；证据 URL 必须定位到具体路径。
    if _normalized_source_path(parsed.path) == '/':
        return ''
    host = host.rstrip('.').lower()
    try:
        ipaddress.ip_address(host)
        return ''
    except ValueError:
        pass
    if host == 'localhost' or host.endswith(('.localhost', '.local', '.internal')):
        return ''
    try:
        host = host.encode('idna').decode('ascii')
    except UnicodeError:
        return ''
    if '.' not in host:
        return ''
    host_parts = host.split('.')
    if (
        all(part.isdigit() for part in host_parts)
        or host in {'example.com', 'example.org', 'example.net'}
        or host.endswith(('.example', '.invalid', '.test'))
        or any(
            not part
            or re.fullmatch(r'[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?', part) is None
            for part in host_parts
        )
    ):
        return ''
    if host.startswith('www.'):
        host = host[4:]
    parts = host.split('.')
    compound_suffixes = {
        'com.cn', 'net.cn', 'org.cn', 'gov.cn', 'com.hk', 'com.tw',
        'co.uk', 'com.au', 'co.jp',
    }
    suffix2 = '.'.join(parts[-2:])
    width = 3 if suffix2 in compound_suffixes and len(parts) >= 3 else 2
    return '.'.join(parts[-width:])


def _canonical_source_key(source: str) -> str:
    """返回证据账本使用的稳定来源位置，忽略可随意刷新的 query/fragment。

    账本按“同一抽检项 + 同一发布页面路径”锁定首次观察。若把完整 URL 字符串直接
    当键，执行者只需给失败来源追加 ``?retry=1`` 或 ``#table`` 就能规避锁定。
    query/fragment 仍保留在原始 source 中供人工定位，但不产生新的账本身份。
    """
    if not _source_publisher(source):
        return ''
    parsed = urlsplit(source)
    host = parsed.hostname.rstrip('.').lower().encode('idna').decode('ascii')
    if host.startswith('www.'):
        host = host[4:]
    port = parsed.port
    default_port = (
        (parsed.scheme.lower() == 'http' and port == 80)
        or (parsed.scheme.lower() == 'https' and port == 443)
    )
    authority = host if port is None or default_port else f'{host}:{port}'
    path = _normalized_source_path(parsed.path)
    # http/https 和 www 变体通常仍指向同一发布页面，不能被用来刷新证据身份。
    return f'{authority}{path}'


def write_source_snapshot(source: str, content: str, output_path: str) -> str:
    """把联网工具取得的页面正文绑定到来源 URL，返回快照文件 SHA-256。"""
    if not _source_publisher(source):
        raise ValueError('source 必须是可定位证据的完整 http/https URL')
    if not isinstance(content, str) or not content.strip():
        raise ValueError('来源正文不能为空')
    payload = {
        'schema_version': 1,
        'source': source,
        'content_sha256': hashlib.sha256(content.encode('utf-8')).hexdigest(),
        'content': content,
    }
    rendered = (json.dumps(payload, ensure_ascii=False, indent=2) + '\n').encode('utf-8')
    if len(rendered) > _MAX_SOURCE_SNAPSHOT_BYTES:
        raise ValueError('来源快照超过 10 MiB 上限；请保存与证据相关的正文内容')
    parent = os.path.dirname(os.path.abspath(output_path))
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(output_path, 'wb') as handle:
        handle.write(rendered)
    return hashlib.sha256(rendered).hexdigest()


def _normalized_excerpt(value: str) -> str:
    return re.sub(r'\s+', ' ', value).strip()


def _validate_source_snapshot(
    item: dict,
    suffix: str,
    snapshot_base_dir: str,
) -> list:
    location = f'抽检项 {item.get("id", "?")} 第 {1 if not suffix else 2} 来源'
    path_field = f'fetched_snapshot{suffix}'
    digest_field = f'fetched_snapshot_sha256{suffix}'
    source_field = f'fetched_source{suffix}'
    evidence_field = f'fetched_evidence{suffix}'
    snapshot_path = item.get(path_field)
    expected_digest = item.get(digest_field)
    errors = []
    if not isinstance(snapshot_path, str) or not snapshot_path.strip():
        return [f'{location}.{path_field} 必须指向 source-snapshot 生成的正文快照']
    digest_valid = (
        isinstance(expected_digest, str)
        and re.fullmatch(r'[0-9a-f]{64}', expected_digest) is not None
    )
    if not digest_valid:
        errors.append(f'{location}.{digest_field} 必须是快照文件的 64 位小写 SHA-256')
    resolved = (
        snapshot_path
        if os.path.isabs(snapshot_path)
        else os.path.join(snapshot_base_dir, snapshot_path)
    )
    try:
        with open(resolved, 'rb') as handle:
            raw = handle.read(_MAX_SOURCE_SNAPSHOT_BYTES + 1)
    except OSError as exc:
        return errors + [f'{location}.{path_field} 无法读取：{exc}']
    if len(raw) > _MAX_SOURCE_SNAPSHOT_BYTES:
        return errors + [f'{location}.{path_field} 超过 10 MiB 上限']
    actual_digest = hashlib.sha256(raw).hexdigest()
    if digest_valid and actual_digest != expected_digest:
        errors.append(f'{location}.{digest_field} 与快照文件内容不一致')
    try:
        snapshot = json.loads(raw.decode('utf-8'))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        errors.append(f'{location}.{path_field} 不是有效 UTF-8 JSON 快照：{exc}')
        return errors
    if not isinstance(snapshot, dict) or snapshot.get('schema_version') != 1:
        errors.append(f'{location}.{path_field} 快照 schema_version 必须为 1')
        return errors
    source = item.get(source_field, '')
    if snapshot.get('source') != source:
        errors.append(f'{location}.{path_field} 绑定的 URL 与 {source_field} 不一致')
    content = snapshot.get('content')
    if not isinstance(content, str) or not content.strip():
        errors.append(f'{location}.{path_field} 缺少来源正文')
        return errors
    content_digest = hashlib.sha256(content.encode('utf-8')).hexdigest()
    if snapshot.get('content_sha256') != content_digest:
        errors.append(f'{location}.{path_field} 的正文哈希无效')
    evidence = item.get(evidence_field, '')
    if (
        isinstance(evidence, str)
        and _normalized_excerpt(evidence) not in _normalized_excerpt(content)
    ):
        errors.append(f'{location}.{evidence_field} 不是来源正文快照中的连续原文')
    return errors


def _audit_point(point: dict) -> dict:
    return {field: point.get(field) for field in _AUDIT_POINT_FIELDS}


def _sample_digest(results: list) -> str:
    payload = json.dumps(
        [_audit_point(item) for item in results],
        ensure_ascii=False,
        sort_keys=True,
        separators=(',', ':'),
    ).encode('utf-8')
    return hashlib.sha256(payload).hexdigest()


def _evidence_scope_digest(results: list) -> str:
    """与排版、行号和样本顺序无关的证据账本身份。"""
    identities = sorted(
        (
            str(item.get('label')),
            item.get('reported_value'),
            str(item.get('unit')),
        )
        for item in results
    )
    payload = json.dumps(
        identities,
        ensure_ascii=False,
        separators=(',', ':'),
    ).encode('utf-8')
    return hashlib.sha256(payload).hexdigest()


def _fact_identity(point: dict) -> tuple:
    """不随行号、排版和提取顺序变化的事实身份。"""
    return (
        str(point.get('label')),
        point.get('reported_value'),
        str(point.get('unit')),
    )


def _fact_identity_key(point: dict) -> str:
    payload = json.dumps(
        _fact_identity(point),
        ensure_ascii=False,
        separators=(',', ':'),
    ).encode('utf-8')
    return hashlib.sha256(payload).hexdigest()


def _population_digest(points: list) -> str:
    """完整事实池摘要不含 ID/行号/顺序，避免用重排或空白变化刷新样本。"""
    payload = json.dumps(
        sorted(_fact_identity(point) for point in points),
        ensure_ascii=False,
        separators=(',', ':'),
    ).encode('utf-8')
    return hashlib.sha256(payload).hexdigest()


def _population_sample_seed(population_digest: str) -> int:
    """从完整事实池确定 seed；无关空白或叙述改动不能用来反复刷新样本。"""
    digest = hashlib.sha256(
        f'{_SAMPLE_POLICY}\0{population_digest}'.encode('ascii')
    ).digest()
    return int.from_bytes(digest[:8], 'big') & ((1 << 63) - 1)


def _markdown_table_cells(line: str) -> list:
    cells = line.strip().split('|')
    if cells and not cells[0].strip():
        cells = cells[1:]
    if cells and not cells[-1].strip():
        cells = cells[:-1]
    return [cell.strip() for cell in cells]


def _audit_scope_table_errors(md_text: str, points: list) -> list:
    """强制审计区使用唯一的两列表格，每个数据行只承载一个数值事实。"""
    if not _has_explicit_audit_scope(md_text):
        return []
    lines = md_text.split('\n')
    starts = [i for i, line in enumerate(lines) if line.strip() == _AUDIT_SCOPE_START]
    ends = [i for i, line in enumerate(lines) if line.strip() == _AUDIT_SCOPE_END]
    if len(starts) != 1 or len(ends) != 1 or starts[0] >= ends[0]:
        return []  # 标记错误由 _scoped_report_text 统一报告。

    start, end = starts[0], ends[0]
    separators = [
        index
        for index in range(start + 1, end)
        if _is_md_table_separator(lines[index])
    ]
    if len(separators) != 1:
        return ['审计区必须且只能包含一个 Markdown 两列表格']
    separator = separators[0]
    header = separator - 1
    if header <= start or '|' not in lines[header]:
        return ['审计区表格分隔行前必须有两列表头']

    data_rows = []
    index = separator + 1
    while index < end and '|' in lines[index]:
        data_rows.append(index)
        index += 1
    if not data_rows:
        return ['审计区两列表格至少需要一个事实数据行']

    errors = []
    table_rows = {header, separator, *data_rows}
    for row in sorted(table_rows):
        cells = _markdown_table_cells(lines[row])
        if len(cells) != 2:
            errors.append(
                f'第 {row + 1} 行审计表必须恰好两列；来源、备注和其他数字移到审计区外'
            )

    points_by_line = {}
    for point in points:
        points_by_line.setdefault(point['line_number'], []).append(point)
    for row in data_rows:
        line_number = row + 1
        cells = _markdown_table_cells(lines[row])
        value_numbers = (
            list(_EVIDENCE_NUMBER_RE.finditer(cells[1]))
            if len(cells) >= 2
            else []
        )
        if len(value_numbers) != 1:
            errors.append(f'第 {line_number} 行报告值单元格必须恰好包含一个数值')
        if len(points_by_line.get(line_number, [])) != 1:
            errors.append(f'第 {line_number} 行必须能提取出且只能提取出一个可核验事实')

    data_line_numbers = {row + 1 for row in data_rows}
    for line_number in sorted(points_by_line):
        if line_number not in data_line_numbers:
            errors.append(f'第 {line_number} 行的可核验事实不在审计区两列表格数据行中')
    return errors


def _report_path_digest(report_path: str = None) -> str:
    normalized = (
        os.path.normcase(os.path.realpath(report_path))
        if report_path
        else ''
    )
    return hashlib.sha256(normalized.encode('utf-8')).hexdigest()


def collect_report_lint_errors(md_text: str, require_scope: bool = True) -> list:
    """一次性检查完整事实池，避免抽样后才逐项发现格式问题。"""
    errors = []
    has_scope = _has_explicit_audit_scope(md_text)
    if require_scope and not has_scope:
        errors.append(
            '缺少显式审计区；请把可外部核验的事实表放在 '
            f'{_AUDIT_SCOPE_START} 与 {_AUDIT_SCOPE_END} 之间'
        )
    try:
        points = extract_data_points(md_text)
    except ValueError as exc:
        return [str(exc)]
    errors.extend(_audit_scope_table_errors(md_text, points))
    if not points:
        errors.append('审计区没有可识别的数据事实')
        return errors
    for point in points:
        location = f'第 {point["line_number"]} 行 {point["label"]!r}'
        unit = point.get('unit', '')
        if not unit:
            errors.append(f'{location} 缺少显式单位')
        elif _unit_scale(unit) is None:
            errors.append(f'{location} 使用不支持或不完整的单位 {unit!r}')
        raw = point.get('raw_text', '')
        if re.search(
            r'(?:^|[|\s（(])(?:~|≈|[<>≥≤]|约(?:为)?|大约|近|超过|不超过|'
            r'不低于|至少|至多|逾|不足)\s*[+\-−]?\d',
            raw,
        ):
            errors.append(f'{location} 是近似值；请移出事实审计区或改用已核实原值')
    return errors


def validate_report_for_audit(md_text: str, require_scope: bool = True) -> list:
    errors = collect_report_lint_errors(md_text, require_scope=require_scope)
    if errors:
        raise ValueError('报告事实池预检失败：\n- ' + '\n- '.join(errors))
    return extract_data_points(md_text)


def build_audit_bundle(
    md_text: str,
    report_path: str = None,
    strict: bool = False,
) -> dict:
    """生成与报告内容及本次随机样本绑定的可填写审计包。"""
    report_hash = hashlib.sha256(md_text.encode('utf-8')).hexdigest()
    all_points = (
        validate_report_for_audit(md_text, require_scope=True)
        if strict
        else extract_data_points(md_text)
    )
    population_digest = _population_digest(all_points)
    actual_seed = _population_sample_seed(population_digest)
    sampled = sample_points(all_points, ratio=_AUDIT_SAMPLE_RATIO, seed=actual_seed)
    results = []
    for point in sampled:
        item = dict(point)
        item.update({
            'fetched_raw_value': None,
            'fetched_raw_unit': '',
            'fetched_evidence': '',
            'fetched_value': None,
            'fetched_source': '',
            'fetched_locator': '',
            'fetched_raw_value2': None,
            'fetched_raw_unit2': '',
            'fetched_evidence2': '',
            'fetched_value2': None,
            'fetched_source2': '',
            'fetched_locator2': '',
            'fetched_snapshot': '',
            'fetched_snapshot_sha256': '',
            'fetched_snapshot2': '',
            'fetched_snapshot_sha2562': '',
        })
        results.append(item)
    return {
        'schema_version': _AUDIT_SCHEMA_VERSION,
        'report_text_sha256': report_hash,
        'report_path_sha256': _report_path_digest(report_path),
        'sample_policy': _SAMPLE_POLICY,
        'audit_scope_required': strict,
        'source_snapshot_policy': _SOURCE_SNAPSHOT_POLICY if strict else 'none',
        'population_digest': population_digest,
        'sample_ratio': _AUDIT_SAMPLE_RATIO,
        'sample_seed': actual_seed,
        'sample_count': len(results),
        'sample_digest': _sample_digest(results),
        'evidence_scope_digest': _evidence_scope_digest(results),
        'results': results,
    }


def validate_audit_bundle(bundle: dict, md_text: str, report_path: str = None) -> list:
    """复算报告与随机样本，拒绝改值、删项、换样本或复用旧报告的结果文件。"""
    if not isinstance(bundle, dict):
        raise ValueError('审计文件根节点必须是 extract 生成的对象，旧版结果数组不再可准出')
    schema_version = bundle.get('schema_version')
    if isinstance(schema_version, bool) or schema_version != _AUDIT_SCHEMA_VERSION:
        raise ValueError(
            f'schema_version 必须为 {_AUDIT_SCHEMA_VERSION}；请重新运行 extract 生成审计包'
        )
    expected_report_hash = hashlib.sha256(md_text.encode('utf-8')).hexdigest()
    if bundle.get('report_text_sha256') != expected_report_hash:
        raise ValueError('报告内容已变化或审计包来自另一份报告；请重新运行 extract')
    if bundle.get('report_path_sha256') != _report_path_digest(report_path):
        raise ValueError('报告路径已变化或审计包来自另一份报告路径；请对实际报告重新运行 extract')

    if bundle.get('sample_policy') != _SAMPLE_POLICY:
        raise ValueError('sample_policy 已变化；请重新运行 extract 生成审计包')
    scope_required = bundle.get('audit_scope_required')
    if not isinstance(scope_required, bool):
        raise ValueError('audit_scope_required 无效；请重新运行 extract 生成审计包')
    expected_snapshot_policy = _SOURCE_SNAPSHOT_POLICY if scope_required else 'none'
    if bundle.get('source_snapshot_policy') != expected_snapshot_policy:
        raise ValueError('source_snapshot_policy 已变化；请重新运行 extract 生成审计包')
    ratio = bundle.get('sample_ratio')
    if isinstance(ratio, bool) or ratio != _AUDIT_SAMPLE_RATIO:
        raise ValueError(f'sample_ratio 必须保持固定抽样比例 {_AUDIT_SAMPLE_RATIO:.0%}')
    expected_all_points = (
        validate_report_for_audit(md_text, require_scope=True)
        if scope_required
        else extract_data_points(md_text)
    )
    expected_population_digest = _population_digest(expected_all_points)
    if bundle.get('population_digest') != expected_population_digest:
        raise ValueError('population_digest 无效；完整事实池已变化，请重新运行 extract')
    seed = bundle.get('sample_seed')
    expected_seed = _population_sample_seed(expected_population_digest)
    if isinstance(seed, bool) or seed != expected_seed:
        raise ValueError('sample_seed 与报告内容不匹配；不得换 seed 选择更有利的样本')
    results = bundle.get('results')
    if not isinstance(results, list) or not all(isinstance(item, dict) for item in results):
        raise ValueError('results 必须是对象数组')

    expected = sample_points(
        expected_all_points,
        ratio=_AUDIT_SAMPLE_RATIO,
        seed=expected_seed,
    )
    sample_count = bundle.get('sample_count')
    if (
        isinstance(sample_count, bool)
        or not isinstance(sample_count, int)
        or sample_count != len(expected)
        or len(results) != len(expected)
    ):
        raise ValueError('抽检项数量已变化；不得删除或增加样本，请重新运行 extract')
    expected_points = [_audit_point(point) for point in expected]
    actual_points = [_audit_point(item) for item in results]
    if actual_points != expected_points:
        raise ValueError(
            '抽检项的 id、标签、报告值、单位、行号或原文已被修改；只能填写来源证据字段'
        )
    expected_digest = _sample_digest(expected)
    if bundle.get('sample_digest') != expected_digest:
        raise ValueError('sample_digest 无效；审计样本已被修改，请重新运行 extract')
    expected_evidence_scope = _evidence_scope_digest(expected)
    if bundle.get('evidence_scope_digest') != expected_evidence_scope:
        raise ValueError('evidence_scope_digest 无效；证据账本身份已被修改')
    return results


_EVIDENCE_NUMBER_RE = re.compile(
    r'\(\s*[+\-−]?\s*\$?\s*\d[\d,，]*(?:\.\d+)?\s*\)'
    r'|[+\-−]?\s*\$?\s*\d[\d,，]*(?:\.\d+)?'
)


_COUNT_BASE_UNITS = ('股', '人', '户', '个', '家', '台', '辆', '项', '座', '门店', '家门店')
_COUNT_SCALE_PREFIXES = (
    ('万亿', 1e12), ('十亿', 1e9), ('百万', 1e6),
    ('亿', 1e8), ('万', 1e4), ('千', 1e3),
)


def _count_unit_scale(normalized: str):
    if normalized in _COUNT_BASE_UNITS:
        return 1.0
    for prefix, scale in _COUNT_SCALE_PREFIXES:
        if normalized.startswith(prefix) and normalized[len(prefix):] in _COUNT_BASE_UNITS:
            return scale
    return None


_PHYSICAL_BASE_UNITS = {
    '桶油当量': ('quantity_boe', 1.0),
    '桶': ('quantity_barrel', 1.0),
    '立方米': ('quantity_volume', 1.0),
    '立方英尺': ('quantity_volume', 0.028316846592),
    '升': ('quantity_volume', 0.001),
    '吨': ('quantity_mass', 1.0),
    '千瓦时': ('quantity_energy', 1.0),
    '平方米': ('quantity_area', 1.0),
}


def _physical_unit_scale(normalized: str):
    for base_unit, (dimension, base_scale) in _PHYSICAL_BASE_UNITS.items():
        if normalized == base_unit:
            return dimension, base_scale, None
        for prefix, scale in _COUNT_SCALE_PREFIXES:
            if normalized == f'{prefix}{base_unit}':
                return dimension, scale * base_scale, None
    return None


_PRICE_CURRENCIES = {
    '元': 'CNY', '人民币': 'CNY', 'rmb': 'CNY', 'cny': 'CNY',
    '美元': 'USD', 'usd': 'USD', 'us$': 'USD', '$': 'USD',
    '港元': 'HKD', 'hkd': 'HKD', 'hk$': 'HKD',
}
_PRICE_DENOMINATORS = {
    # 质量价格统一换算为“每克”。
    '克': ('price_mass', 1.0),
    'g': ('price_mass', 1.0),
    '盎司': ('price_mass', 1.0 / 31.1034768),
    'oz': ('price_mass', 1.0 / 31.1034768),
    '吨': ('price_mass', 1.0 / 1_000_000),
    # 体积价格统一换算为“每桶”。
    '桶': ('price_volume', 1.0),
    'bbl': ('price_volume', 1.0),
    '升': ('price_volume', 158.9872949),
    'l': ('price_volume', 158.9872949),
    '股': ('price_share', 1.0),
    'share': ('price_share', 1.0),
}


def _price_unit_scale(normalized: str):
    parts = re.split(r'[/／]', normalized)
    if len(parts) != 2:
        return None
    currency = _PRICE_CURRENCIES.get(parts[0])
    denominator = _PRICE_DENOMINATORS.get(parts[1])
    if currency is None or denominator is None:
        return None
    dimension, scale = denominator
    return dimension, scale, currency


def _unit_scale(unit: str):
    """返回 (量纲, 相对基础单位倍数, 币种)；不支持的单位返回 None。"""
    if not isinstance(unit, str):
        return None
    normalized = re.sub(r'[\s,.，。]', '', unit).lower()
    if normalized == '':
        return ('scalar', 1.0, None)
    count_scale = _count_unit_scale(normalized)
    if count_scale is not None:
        return ('scalar', count_scale, None)
    physical_spec = _physical_unit_scale(normalized)
    if physical_spec is not None:
        return physical_spec
    price_spec = _price_unit_scale(normalized)
    if price_spec is not None:
        return price_spec
    if normalized in ('%', 'percent', 'percentage', '百分比'):
        return ('percent', 1.0, None)
    if normalized in ('x', '倍'):
        return ('multiple', 1.0, None)
    magnitude_units = {
        '元': 1.0,
        'rmb': 1.0,
        'cny': 1.0,
        '美元': 1.0,
        'usd': 1.0,
        '港元': 1.0,
        'hkd': 1.0,
        '千': 1e3,
        '千元': 1e3,
        '千美元': 1e3,
        '千港元': 1e3,
        'thousand': 1e3,
        'k': 1e3,
        '万': 1e4,
        '万元': 1e4,
        '万美元': 1e4,
        '万港元': 1e4,
        '百万': 1e6,
        '百万元': 1e6,
        '百万美元': 1e6,
        '百万港元': 1e6,
        'million': 1e6,
        'm': 1e6,
        '亿': 1e8,
        '亿元': 1e8,
        '亿美元': 1e8,
        '亿港元': 1e8,
        '十亿': 1e9,
        '十亿元': 1e9,
        '十亿美元': 1e9,
        '十亿港元': 1e9,
        'billion': 1e9,
        'b': 1e9,
        '万亿': 1e12,
        '万亿元': 1e12,
        '万亿美元': 1e12,
        '万亿港元': 1e12,
        'trillion': 1e12,
        't': 1e12,
    }
    magnitude = magnitude_units.get(normalized)
    currency = None
    if magnitude is None:
        # 英文来源常把币种和数量级分开写成 “RMB million / USD billion”。
        # 空白已被去掉，因此同时接受前缀和后缀顺序。
        for currency_token, currency_code in (
            ('人民币', 'CNY'), ('rmb', 'CNY'), ('cny', 'CNY'),
            ('美元', 'USD'), ('usd', 'USD'),
            ('港元', 'HKD'), ('hkd', 'HKD'),
        ):
            if normalized.startswith(currency_token):
                magnitude = magnitude_units.get(normalized[len(currency_token):])
            elif normalized.endswith(currency_token):
                magnitude = magnitude_units.get(normalized[:-len(currency_token)])
            if magnitude is not None:
                currency = currency_code
                break
    if magnitude is None:
        return None
    if currency is None and normalized in (
        # 中文报告中的裸数量级默认采用人民币口径；若实际是美元/港元，报告必须
        # 明写币种。否则 USD billion 可被机械换算成“亿”并错误通过人民币报告。
        '元', 'rmb', 'cny', '千', '千元', '万', '万元', '百万', '百万元',
        '亿', '亿元', '十亿', '十亿元', '万亿', '万亿元',
    ):
        currency = 'CNY'
    elif currency is None and normalized in (
        '美元', 'usd', '千美元', '万美元', '百万美元', '亿美元', '十亿美元',
        '万亿美元',
    ):
        currency = 'USD'
    elif currency is None and normalized in (
        '港元', 'hkd', '千港元', '万港元', '百万港元', '亿港元', '十亿港元',
        '万亿港元',
    ):
        currency = 'HKD'
    return ('magnitude', magnitude, currency)


def _normalize_fetched_value(raw_value: float, raw_unit: str, report_unit: str) -> float:
    raw_spec = _unit_scale(raw_unit)
    report_spec = _unit_scale(report_unit)
    if raw_spec is None:
        raise ValueError(f'不支持的来源原始单位 {raw_unit!r}')
    if report_spec is None:
        raise ValueError(f'不支持的报告单位 {report_unit!r}')
    if raw_spec[0] != report_spec[0]:
        raise ValueError(
            f'来源单位 {raw_unit!r} 与报告单位 {report_unit!r} 量纲不一致'
        )
    if raw_spec[2] and report_spec[2] and raw_spec[2] != report_spec[2]:
        raise ValueError(
            f'来源单位 {raw_unit!r} 与报告单位 {report_unit!r} 币种不一致'
        )
    normalized = float(raw_value) * raw_spec[1] / report_spec[1]
    if not math.isfinite(normalized):
        raise ValueError('来源数值换算后不是有限数值')
    return normalized


def _evidence_contains_raw_value(evidence: str, raw_value: float) -> bool:
    """确认来源摘录确实出现填写的原始数值，而不是只写一段结论说明。"""
    for match in _EVIDENCE_NUMBER_RE.finditer(evidence):
        parsed = _clean_num(match.group(0).replace(' ', ''))
        if parsed is not None and math.isclose(
            parsed, float(raw_value), rel_tol=1e-12, abs_tol=1e-12
        ):
            return True
    return False


def _compact_observation_text(value: str) -> str:
    return re.sub(r'[\s,，。.:：；;()（）\[\]【】]', '', value).lower()


def _raw_unit_aliases(raw_unit: str) -> set:
    normalized = _compact_observation_text(raw_unit)
    aliases = {normalized} if normalized else {''}
    if normalized in ('%', 'percent', 'percentage', '百分比'):
        aliases.update({'%', 'percent', 'percentage', '百分比'})
    elif normalized in ('x', '倍'):
        aliases.update({'x', '倍'})
    elif normalized in ('元', 'rmb', 'cny', '人民币'):
        aliases.update({'元', 'rmb', 'cny', '人民币'})
    elif normalized in ('美元', 'usd', 'us$', '$'):
        aliases.update({'美元', 'usd', 'us$', '$'})
    elif normalized in ('港元', 'hkd', 'hk$'):
        aliases.update({'港元', 'hkd', 'hk$'})
    return {_compact_observation_text(alias) for alias in aliases}


def _evidence_contains_raw_observation(
    evidence: str,
    raw_value: float,
    raw_unit: str,
) -> bool:
    """要求原始数值与单位相邻，避免“10年…2.5%”伪装成“10%”。"""
    aliases = _raw_unit_aliases(raw_unit)
    if aliases == {''}:
        return _evidence_contains_raw_value(evidence, raw_value)
    for match in _EVIDENCE_NUMBER_RE.finditer(evidence):
        parsed = _clean_num(match.group(0))
        if parsed is None or not math.isclose(parsed, raw_value, rel_tol=1e-12, abs_tol=1e-12):
            continue
        before = _compact_observation_text(evidence[max(0, match.start() - 24):match.start()])
        after = _compact_observation_text(evidence[match.end():match.end() + 24])
        if any(
            alias and (after.startswith(alias) or before.endswith(alias))
            for alias in aliases
        ):
            return True
        # 英文财报常写成 ``RMB 100 million``，币种在数值前、数量级在后。
        normalized = _compact_observation_text(raw_unit)
        for currency in ('rmb', 'cny', 'usd', 'hkd'):
            if normalized.startswith(currency):
                magnitude = normalized[len(currency):]
                if before.endswith(currency) and after.startswith(magnitude):
                    return True
    return False


def _evidence_currency(evidence: str):
    """从短证据摘录识别币种；同时出现多币种时返回 CONFLICT。"""
    lowered = evidence.lower()
    currencies = set()
    if any(token in lowered for token in ('hk$', 'hkd', '港元')):
        currencies.add('HKD')
    if any(token in lowered for token in ('us$', 'usd', '美元')):
        currencies.add('USD')
    # 排除已经由 HK$/US$ 识别的美元符号；裸 $ 按国际财务来源惯例视为 USD。
    scrubbed = lowered.replace('hk$', '').replace('us$', '')
    if '$' in scrubbed:
        currencies.add('USD')
    cny_text = lowered.replace('美元', '').replace('港元', '')
    if any(
        token in lowered
        for token in (
            'rmb', 'cny', '人民币', '千元', '万元', '百万元', '亿元', '十亿元',
            '万亿元',
        )
    ) or re.search(r'\d[\d,，.]*(?:\s*)元', cny_text):
        currencies.add('CNY')
    if len(currencies) > 1:
        return 'CONFLICT'
    return next(iter(currencies)) if currencies else None


def collect_verdict_validation_errors(
    results: list,
    snapshot_base_dir: str = None,
    require_snapshots: bool = False,
):
    """一次收集全部可确定的输入问题，避免 Agent 每轮只看到第一个错误。"""
    errors = []
    normalizations = {}
    if not isinstance(results, list) or not results:
        return ['results 至少需要一个抽检项'], normalizations

    seen_ids = set()
    for index, item in enumerate(results):
        location = f'results[{index}]'
        item_errors = []
        if not isinstance(item, dict):
            errors.append(f'{location} 必须是对象')
            continue

        item_id = item.get('id')
        if 'id' not in item:
            item_errors.append(f'{location}.id 缺失')
        elif isinstance(item_id, bool) or not isinstance(item_id, (int, str)):
            item_errors.append(f'{location}.id 必须是整数或字符串')
        elif isinstance(item_id, str) and not item_id.strip():
            item_errors.append(f'{location}.id 不能为空')
        else:
            normalized_id = str(item_id).strip()
            if normalized_id in seen_ids:
                item_errors.append(f'{location}.id 与前面的抽检项重复')
            seen_ids.add(normalized_id)

        if not isinstance(item.get('label'), str) or not item['label'].strip():
            item_errors.append(f'{location}.label 必须是非空字符串')

        string_fields = (
            'unit', 'raw_text', 'fetched_source', 'fetched_source2',
            'fetched_locator', 'fetched_locator2', 'fetched_raw_unit',
            'fetched_raw_unit2', 'fetched_evidence', 'fetched_evidence2',
            'fetched_snapshot', 'fetched_snapshot2',
            'fetched_snapshot_sha256', 'fetched_snapshot_sha2562',
        )
        string_valid = {}
        for field in string_fields:
            string_valid[field] = field not in item or isinstance(item[field], str)
            if not string_valid[field]:
                item_errors.append(f'{location}.{field} 必须是字符串')

        if 'line_number' in item:
            line_number = item['line_number']
            if (
                isinstance(line_number, bool)
                or not isinstance(line_number, int)
                or line_number < 0
            ):
                item_errors.append(f'{location}.line_number 必须是非负整数')

        numeric_valid = {}
        for field in ('reported_value', 'fetched_raw_value', 'fetched_raw_value2'):
            value = item.get(field)
            numeric_valid[field] = (
                not isinstance(value, bool)
                and isinstance(value, (int, float))
                and math.isfinite(float(value))
            )
            if isinstance(value, bool) or not isinstance(value, (int, float)):
                item_errors.append(f'{location}.{field} 必须是数值')
            elif not math.isfinite(float(value)):
                item_errors.append(f'{location}.{field} 必须是有限数值')

        # 归一化复核值只能由本脚本根据“来源原始值 + 原始单位”计算。
        for value_field in ('fetched_value', 'fetched_value2'):
            if value_field not in item or item[value_field] is not None:
                item_errors.append(
                    f'{location}.{value_field} 必须保持为 null；该值由 verdict 计算'
                )

        publishers = []
        normalized_values = []
        report_unit = item.get('unit', '')
        report_unit_valid = isinstance(report_unit, str)
        for suffix in ('', '2'):
            source_field = f'fetched_source{suffix}'
            locator_field = f'fetched_locator{suffix}'
            raw_value_field = f'fetched_raw_value{suffix}'
            raw_unit_field = f'fetched_raw_unit{suffix}'
            evidence_field = f'fetched_evidence{suffix}'

            source = item.get(source_field)
            publisher = _source_publisher(source)
            publishers.append(publisher)
            if not publisher:
                item_errors.append(
                    f'{location}.{source_field} 必须是可定位证据的完整 http/https URL，'
                    '不能填写来源名称、计算说明、首页、内网地址或口径解释'
                )

            locator = item.get(locator_field)
            locator_valid = isinstance(locator, str) and len(locator.strip()) >= 4
            if not locator_valid:
                item_errors.append(
                    f'{location}.{locator_field} 必须具体说明页码、表名、字段名或正文定位'
                )
            elif locator.strip().lower() in {
                '同上', '同报告', '报告值', '与报告一致', 'same as report', 'n/a',
                '网页', '页面', '正文', '表格', 'table', 'page', 'source',
            }:
                item_errors.append(f'{location}.{locator_field} 不是可核对的页内定位')

            evidence = item.get(evidence_field)
            evidence_valid = (
                isinstance(evidence, str)
                and 4 <= len(evidence.strip()) <= 1200
            )
            if not evidence_valid:
                item_errors.append(
                    f'{location}.{evidence_field} 必须是 4..1200 字的来源原文摘录'
                )

            raw_value = item.get(raw_value_field)
            raw_unit = item.get(raw_unit_field, '')
            raw_unit_valid = isinstance(raw_unit, str)
            if not (evidence_valid and numeric_valid[raw_value_field] and raw_unit_valid):
                normalized_values.append(None)
                continue

            contains_value = _evidence_contains_raw_value(evidence, raw_value)
            if not contains_value:
                item_errors.append(
                    f'{location}.{evidence_field} 未出现 {raw_value_field} 的原始数值'
                )

            if contains_value and not _evidence_contains_raw_observation(
                evidence, raw_value, raw_unit
            ):
                item_errors.append(
                    f'{location}.{evidence_field} 中原始数值未与 '
                    f'{raw_unit_field} 对应来源单位相邻出现'
                )

            evidence_currency = _evidence_currency(evidence)
            if evidence_currency == 'CONFLICT':
                item_errors.append(
                    f'{location}.{evidence_field} 同时出现多种币种；'
                    '请缩小摘录并明确本次取值口径'
                )
                # 币种冲突不妨碍继续检查单位是否受支持、两个单位量纲是否一致。
                evidence_currency = None

            raw_spec = _unit_scale(raw_unit)
            report_spec = _unit_scale(report_unit) if report_unit_valid else None
            if (
                evidence_currency
                and raw_spec is not None
                and raw_spec[2]
                and evidence_currency != raw_spec[2]
            ):
                item_errors.append(
                    f'{location}.{evidence_field} 的币种与 {raw_unit_field} 不一致'
                )
            if (
                evidence_currency
                and report_spec is not None
                and report_spec[2]
                and evidence_currency != report_spec[2]
            ):
                item_errors.append(
                    f'{location}.{evidence_field} 的币种与报告单位不一致，不能直接比较'
                )
            if (
                raw_spec is not None
                and report_spec is not None
                and raw_spec[0] == report_spec[0] == 'magnitude'
            ):
                source_currency = raw_spec[2] or evidence_currency
                report_currency = report_spec[2]
                if report_currency and not source_currency:
                    item_errors.append(
                        f'{location}.{raw_unit_field} 未声明币种，且来源摘录也无法确定币种'
                    )
                if source_currency and not report_currency:
                    item_errors.append(
                        f'{location}.unit 未声明币种，不能与有明确币种的来源直接比较'
                    )

            try:
                normalized = _normalize_fetched_value(raw_value, raw_unit, report_unit)
            except ValueError as exc:
                item_errors.append(f'{location}.{raw_unit_field}: {exc}')
                normalized = None
            normalized_values.append(normalized)

        if publishers[0] and publishers[0] == publishers[1]:
            item_errors.append(
                f'{location} 的两个来源同属 {publishers[0]}，不构成独立交叉验证'
            )

        if require_snapshots:
            base_dir = snapshot_base_dir or os.getcwd()
            for suffix in ('', '2'):
                item_errors.extend(
                    _validate_source_snapshot(item, suffix, base_dir)
                )

        if not item_errors and all(value is not None for value in normalized_values):
            normalizations[index] = tuple(normalized_values)
        errors.extend(item_errors)

    return errors, normalizations


def validate_verdict_results(
    results: list,
    snapshot_base_dir: str = None,
    require_snapshots: bool = False,
):
    """预检全部 JSON 字段；无误后再统一写入脚本计算的归一化值。"""
    errors, normalizations = collect_verdict_validation_errors(
        results,
        snapshot_base_dir=snapshot_base_dir,
        require_snapshots=require_snapshots,
    )
    if errors:
        rendered = '\n- '.join(errors)
        raise ValueError(f'审计预检发现 {len(errors)} 个问题：\n- {rendered}')
    for index, normalized_values in normalizations.items():
        results[index]['fetched_value'], results[index]['fetched_value2'] = normalized_values


def _evidence_observation(item: dict, suffix: str) -> dict:
    return {
        'slot': 2 if suffix == '2' else 1,
        'source': item[f'fetched_source{suffix}'],
        'source_key': _canonical_source_key(item[f'fetched_source{suffix}']),
        'locator': item[f'fetched_locator{suffix}'],
        'raw_value': item[f'fetched_raw_value{suffix}'],
        'raw_unit': item.get(f'fetched_raw_unit{suffix}', ''),
        'evidence': item[f'fetched_evidence{suffix}'],
        'snapshot_sha256': item.get(f'fetched_snapshot_sha256{suffix}', ''),
    }


def _evidence_ledger_path(
    results_path: str,
    bundle: dict,
    report_path: str = None,
) -> str:
    """账本不依赖结果文件名、报告路径或排版。

    CLI 判决统一写入 Skill 级目录，避免把同一报告复制/改名后生成新路径哈希来刷新
    首次观察。未提供 report_path 的库调用保留结果文件同目录，便于隔离测试与嵌入使用。
    """
    if report_path:
        package_dir = os.path.dirname(os.path.dirname(os.path.realpath(__file__)))
        ledger_dir = os.path.join(package_dir, '.report-audit-evidence')
    else:
        ledger_dir = os.path.join(
            os.path.dirname(os.path.abspath(results_path)),
            '.report-audit-evidence',
        )
    return os.path.join(
        ledger_dir,
        f'v{_EVIDENCE_LEDGER_SCHEMA_VERSION}.'
        f'{bundle["population_digest"][:32]}.'
        f'{bundle["evidence_scope_digest"][:32]}.json',
    )


def _update_evidence_ledger(
    results_path: str,
    bundle: dict,
    results: list,
    report_path: str = None,
) -> str:
    """锁定同一直接 URL 的首次证据观察，禁止失败后悄悄改值再跑。

    账本不是对同权限 shell 的密码学防护；它用于把普通工作流中的规格投机变成显式
    篡改。两个来源槽位在首次判决时锁定，不能在看到结果后改值或换 URL。
    """
    ledger_path = _evidence_ledger_path(results_path, bundle, report_path)
    expected_header = {
        'schema_version': _EVIDENCE_LEDGER_SCHEMA_VERSION,
        'population_digest': bundle['population_digest'],
        'evidence_scope_digest': bundle['evidence_scope_digest'],
    }
    if os.path.exists(ledger_path):
        try:
            with open(ledger_path, 'r', encoding='utf-8') as handle:
                ledger = json.load(handle)
        except (OSError, json.JSONDecodeError) as exc:
            raise ValueError(f'证据账本无法读取，禁止绕过：{exc}') from exc
        for key, value in expected_header.items():
            if ledger.get(key) != value:
                raise ValueError(f'证据账本 {key} 不匹配，禁止覆盖或复用')
        observations = ledger.get('observations')
        if not isinstance(observations, dict):
            raise ValueError('证据账本 observations 无效，禁止覆盖或复用')
    else:
        ledger = dict(expected_header)
        observations = {}
        ledger['observations'] = observations

    changed = False
    for item in results:
        point_id = str(item['id'])
        point_key = _fact_identity_key(item)
        point_observations = observations.setdefault(point_key, [])
        if not isinstance(point_observations, list):
            raise ValueError(f'证据账本抽检项 {point_id}（{point_key[:12]}）无效')
        if not all(isinstance(observation, dict) for observation in point_observations):
            raise ValueError(
                f'证据账本抽检项 {point_id}（{point_key[:12]}）含无效观察'
            )
        for suffix in ('', '2'):
            current = _evidence_observation(item, suffix)
            prior = next(
                (
                    observation
                    for observation in point_observations
                    if observation.get('slot') == current['slot']
                ),
                None,
            )
            if prior is not None:
                normalized_prior = dict(prior)
                normalized_prior['source_key'] = (
                    normalized_prior.get('source_key')
                    or _canonical_source_key(normalized_prior.get('source', ''))
                )
            else:
                normalized_prior = None
            if normalized_prior is not None and normalized_prior != current:
                raise ValueError(
                    f'抽检项 {point_id} 的第 {current["slot"]} 来源已在首次判决中锁定；'
                    '不得在看到判决结果后更换 URL、原始值、单位、摘录或定位。'
                    '应修正报告后重新提取审计包，或如实保持打回并交由人工复核。'
                )
            if prior is None:
                point_observations.append(current)
                changed = True

    if changed or not os.path.exists(ledger_path):
        parent = os.path.dirname(ledger_path)
        if parent:
            os.makedirs(parent, exist_ok=True)
        temporary = f'{ledger_path}.{os.getpid()}.tmp'
        try:
            with open(temporary, 'w', encoding='utf-8', newline='\n') as handle:
                json.dump(ledger, handle, ensure_ascii=False, indent=2, sort_keys=True)
                handle.write('\n')
            os.replace(temporary, ledger_path)
        except OSError as exc:
            try:
                if os.path.exists(temporary):
                    os.remove(temporary)
            except OSError:
                pass
            raise ValueError(f'证据账本写入失败，禁止在无账本状态下判决：{exc}') from exc
    return ledger_path


def enforce_evidence_ledger(
    results_path: str,
    bundle: dict,
    results: list,
    report_path: str = None,
) -> str:
    """串行更新证据账本，避免多个审计 Agent 同时首次判决时互相覆盖。

    使用操作系统文件锁而不是“发现锁文件超时就删除”。后者在两个等待者并发时可能
    刚好删掉第三个进程新建的锁；系统锁会在进程崩溃后自动释放，留下的文件本身无害。
    """
    ledger_path = _evidence_ledger_path(results_path, bundle, report_path)
    try:
        os.makedirs(os.path.dirname(ledger_path), exist_ok=True)
    except OSError as exc:
        raise ValueError(f'证据账本目录创建失败，禁止无账本判决：{exc}') from exc
    lock_path = f'{ledger_path}.lock'
    try:
        lock_handle = open(lock_path, 'a+b')
        lock_handle.seek(0, os.SEEK_END)
        if lock_handle.tell() == 0:
            lock_handle.write(b'\0')
            lock_handle.flush()
    except OSError as exc:
        raise ValueError(f'证据账本锁打开失败，禁止无锁判决：{exc}') from exc

    acquired = False
    for _ in range(100):
        try:
            lock_handle.seek(0)
            if os.name == 'nt':
                import msvcrt
                msvcrt.locking(lock_handle.fileno(), msvcrt.LK_NBLCK, 1)
            else:
                import fcntl
                fcntl.flock(lock_handle.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
            acquired = True
            break
        except OSError as exc:
            if exc.errno not in (errno.EACCES, errno.EAGAIN):
                lock_handle.close()
                raise ValueError(f'证据账本锁获取失败：{exc}') from exc
            time.sleep(0.05)
    if not acquired:
        lock_handle.close()
        raise ValueError('证据账本正由另一审计任务更新；请稍后重试，不要删除锁文件')
    try:
        return _update_evidence_ledger(
            results_path, bundle, results, report_path=report_path
        )
    finally:
        try:
            lock_handle.seek(0)
            if os.name == 'nt':
                import msvcrt
                msvcrt.locking(lock_handle.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                import fcntl
                fcntl.flock(lock_handle.fileno(), fcntl.LOCK_UN)
        except OSError as exc:
            print(f'⚠ 证据账本锁释放失败：{exc}', file=sys.stderr)
        finally:
            lock_handle.close()


def _pass_receipt_path(report_path: str) -> str:
    return f'{os.path.abspath(report_path)}.audit-pass.json'


def _validate_receipt_ledger(ledger_bytes: bytes, bundle: dict) -> str:
    try:
        ledger = json.loads(ledger_bytes.decode('utf-8'))
    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise ValueError(f'证据账本不是有效 UTF-8 JSON：{exc}') from exc
    expected_header = {
        'schema_version': _EVIDENCE_LEDGER_SCHEMA_VERSION,
        'population_digest': bundle['population_digest'],
        'evidence_scope_digest': bundle['evidence_scope_digest'],
    }
    if not isinstance(ledger, dict):
        raise ValueError('证据账本根节点必须是对象')
    for field, expected in expected_header.items():
        if ledger.get(field) != expected:
            raise ValueError(f'证据账本 {field} 与当前报告不匹配')
    observations = ledger.get('observations')
    if not isinstance(observations, dict):
        raise ValueError('证据账本 observations 无效')
    for item in bundle['results']:
        point_observations = observations.get(_fact_identity_key(item))
        if not isinstance(point_observations, list):
            raise ValueError(f'证据账本缺少抽检项 {item["id"]} 的观察')
        if not all(isinstance(observation, dict) for observation in point_observations):
            raise ValueError(f'证据账本抽检项 {item["id"]} 含无效观察')
        slots = {
            observation.get('slot')
            for observation in point_observations
        }
        if slots != {1, 2}:
            raise ValueError(f'证据账本抽检项 {item["id"]} 未完整锁定两个来源')
        if any(
            re.fullmatch(r'[0-9a-f]{64}', observation.get('snapshot_sha256', '')) is None
            for observation in point_observations
        ):
            raise ValueError(f'证据账本抽检项 {item["id"]} 缺少有效来源快照哈希')
    return hashlib.sha256(ledger_bytes).hexdigest()


def write_pass_receipt(report_path: str, bundle: dict, ledger_path: str) -> str:
    """为已准出的当前报告写入可复核旁路凭证，供每日增量任务选择基线。"""
    expected_ledger_path = _evidence_ledger_path('', bundle, report_path=report_path)
    if os.path.realpath(ledger_path) != os.path.realpath(expected_ledger_path):
        raise ValueError('证据账本路径与当前报告不匹配，不能生成准出凭证')
    try:
        with open(ledger_path, 'rb') as handle:
            ledger_bytes = handle.read()
    except OSError as exc:
        raise ValueError(f'证据账本无法读取，不能生成准出凭证：{exc}') from exc
    ledger_digest = _validate_receipt_ledger(ledger_bytes, bundle)
    payload = {
        'schema_version': _PASS_RECEIPT_SCHEMA_VERSION,
        'audit_schema_version': _AUDIT_SCHEMA_VERSION,
        'verdict': 'PASS',
        'issued_at_unix': int(time.time()),
        'report_text_sha256': bundle['report_text_sha256'],
        'report_path_sha256': bundle['report_path_sha256'],
        'source_snapshot_policy': bundle['source_snapshot_policy'],
        'population_digest': bundle['population_digest'],
        'sample_digest': bundle['sample_digest'],
        'evidence_scope_digest': bundle['evidence_scope_digest'],
        'evidence_ledger_sha256': ledger_digest,
    }
    receipt_path = _pass_receipt_path(report_path)
    temporary = f'{receipt_path}.{os.getpid()}.tmp'
    try:
        with open(temporary, 'w', encoding='utf-8', newline='\n') as handle:
            json.dump(payload, handle, ensure_ascii=False, indent=2, sort_keys=True)
            handle.write('\n')
        os.replace(temporary, receipt_path)
    except OSError as exc:
        try:
            if os.path.exists(temporary):
                os.remove(temporary)
        except OSError:
            pass
        raise ValueError(f'准出凭证写入失败：{exc}') from exc
    return receipt_path


def validate_pass_receipt(report_path: str) -> dict:
    """验证报告未变化、当前审计规则未变化且对应证据账本仍完整。"""
    receipt_path = _pass_receipt_path(report_path)
    try:
        with open(report_path, 'r', encoding='utf-8') as handle:
            report_text = handle.read()
        with open(receipt_path, 'r', encoding='utf-8') as handle:
            receipt = json.load(handle)
    except (OSError, json.JSONDecodeError) as exc:
        raise ValueError(f'准出凭证无法读取：{exc}') from exc
    if not isinstance(receipt, dict):
        raise ValueError('准出凭证根节点必须是对象')
    if receipt.get('schema_version') != _PASS_RECEIPT_SCHEMA_VERSION:
        raise ValueError('准出凭证版本已过期；请重新执行审计判决')
    if receipt.get('audit_schema_version') != _AUDIT_SCHEMA_VERSION:
        raise ValueError('审计规则已变化；请重新执行审计判决')
    if receipt.get('verdict') != 'PASS':
        raise ValueError('凭证未记录准出结论')

    bundle = build_audit_bundle(report_text, report_path=report_path, strict=True)
    bound_fields = (
        'report_text_sha256', 'report_path_sha256', 'source_snapshot_policy',
        'population_digest', 'sample_digest', 'evidence_scope_digest',
    )
    for field in bound_fields:
        if receipt.get(field) != bundle.get(field):
            raise ValueError(f'准出凭证的 {field} 与当前报告不匹配')

    ledger_path = _evidence_ledger_path('', bundle, report_path=report_path)
    try:
        with open(ledger_path, 'rb') as handle:
            ledger_bytes = handle.read()
    except OSError as exc:
        raise ValueError(f'准出凭证对应的证据账本无法读取：{exc}') from exc
    actual_ledger_digest = _validate_receipt_ledger(ledger_bytes, bundle)
    if receipt.get('evidence_ledger_sha256') != actual_ledger_digest:
        raise ValueError('准出凭证对应的证据账本已变化')
    return receipt


def render_verdict(results: list, report_name: str = "") -> dict:
    """
    根据核验结果输出准出/打回判决。

    results: list of dict，每项包含：
      - id, label, reported_value, unit
      - fetched_raw_value, fetched_raw_unit, fetched_evidence,
        fetched_source, fetched_locator
      - 第二来源对应的 *2 字段
      - fetched_value / fetched_value2 必须保持 null，由校验阶段换算

    返回：
      {
        'verdict': 'PASS' | 'FAIL',
        'pass_count': int,
        'fail_count': int,
        'total': int,
        'fail_items': [...],
        'summary': str,
      }
    """
    BOLD = '\033[1m'
    RED = '\033[91m'
    GREEN = '\033[92m'
    YELLOW = '\033[93m'
    RESET = '\033[0m'

    print('=' * 70)
    print(f'{BOLD}报告数据抽检 — 准出/打回判决{RESET}')
    if report_name:
        print(f'报告：{report_name}')
    print('=' * 70)
    print()

    fail_items = []
    warn_items = []

    for item in results:
        label = item.get('label', '?')
        reported = float(item.get('reported_value', 0))
        unit = item.get('unit', '')
        fetched = item.get('fetched_value')
        source = item.get('fetched_source', '?')
        fetched2 = item.get('fetched_value2')
        source2 = item.get('fetched_source2', '')

        # --- 主来源比对 ---
        if fetched is None:
            # 没有提供核验值 → 记录待核验，最终打回
            print(f'  ⬜ [{item["id"]:>2}] {label[:35]:35s} {reported:>12.2f} {unit}  →  [未提供核验值，跳过]')
            continue

        fetched = float(fetched)
        diff1 = _pct_diff(reported, fetched)

        # --- 第二来源比对（如有）---
        diff2 = None
        source_diff = None
        if fetched2 is not None:
            fetched2 = float(fetched2)
            diff2 = _pct_diff(reported, fetched2)
            source_diff = _pair_pct_diff(fetched, fetched2)

        # 判断
        pass1 = diff1 <= _TOLERANCE
        pass2 = (diff2 is None) or (diff2 <= _TOLERANCE)
        # 两个来源各自贴近报告并不等于彼此一致。例如 99.1 / 100.9 相对报告
        # 100 都在 1% 内，但两来源相差约 1.8%。若仍准出，会让冲突证据共同为
        # 报告背书，也会再次诱发执行者“修饰”第二来源数值。
        sources_agree = source_diff is None or source_diff <= _TOLERANCE

        if pass1 and pass2 and sources_agree:
            status = f'{GREEN}✅ 通过{RESET}'
            detail = f'{source}: {fetched:.2f} (偏差 {diff1*100:.2f}%)'
            if diff2 is not None:
                detail += f'  |  {source2}: {fetched2:.2f} (偏差 {diff2*100:.2f}%)'
        elif not pass1 and (diff2 is None or not pass2):
            status = f'{RED}❌ 不通过{RESET}'
            detail = f'{source}: {fetched:.2f} (偏差 {diff1*100:.2f}%)'
            if diff2 is not None:
                detail += f'  |  {source2}: {fetched2:.2f} (偏差 {diff2*100:.2f}%)'
            fail_items.append({
                'id': item['id'],
                'label': label,
                'reported': reported,
                'unit': unit,
                'fetched': fetched,
                'source': source,
                'fetched2': fetched2,
                'source2': source2,
                'diff1_pct': round(diff1 * 100, 2),
                'diff2_pct': round(diff2 * 100, 2) if diff2 is not None else None,
                'raw_text': item.get('raw_text', ''),
                'line_number': item.get('line_number', 0),
            })
        else:
            # 一个来源通过，一个不通过 → 记录警告，最终仍打回
            status = f'{YELLOW}⚠️  警告{RESET}'
            detail = f'{source}: {fetched:.2f} (偏差 {diff1*100:.2f}%)'
            if diff2 is not None:
                detail += f'  |  {source2}: {fetched2:.2f} (偏差 {diff2*100:.2f}%)'
            warn_items.append({
                'id': item['id'], 'label': label,
                'reported': reported, 'unit': unit,
                'diff1_pct': round(diff1 * 100, 2),
                'diff2_pct': round(diff2 * 100, 2) if diff2 is not None else None,
                'source_diff_pct': (
                    round(source_diff * 100, 2)
                    if source_diff is not None
                    else None
                ),
            })

        print(f'  {status} [{item["id"]:>2}] {label[:35]:35s}  报告: {reported:>12.2f} {unit}')
        print(f'              {" " * 38}{detail}')

    print()
    print('-' * 70)

    total = len([r for r in results if r.get('fetched_value') is not None])
    missing_items = [r for r in results if r.get('fetched_value') is None]
    missing_count = len(missing_items)
    fail_count = len(fail_items)
    warn_count = len(warn_items)
    pass_count = total - fail_count - warn_count

    print(f'  抽检项: {len(results)}  |  已核验: {total}  |  通过: {GREEN}{pass_count}{RESET}  |  警告: {YELLOW}{warn_count}{RESET}  |  不通过: {RED}{fail_count}{RESET}  |  待核验: {missing_count}')
    print()

    if fail_count == 0 and warn_count == 0 and missing_count == 0:
        print(f'{BOLD}{GREEN}【准出】所有抽检数据通过，报告可发布。{RESET}')
        verdict = 'PASS'
    else:
        print(f'{BOLD}{RED}【打回】报告存在不一致、未通过或未完成核验的数据点，需复核后重审。{RESET}')
        print()
        print(f'{BOLD}打回原因：{RESET}')
        for fi in fail_items:
            print(f'  ❌ 第 {fi["line_number"]} 行 | {fi["label"]}')
            print(f'     报告值：{fi["reported"]} {fi["unit"]}')
            print(f'     {fi["source"]}：{fi["fetched"]}  （偏差 {fi["diff1_pct"]}%）')
            if fi.get('fetched2') is not None:
                print(f'     {fi["source2"]}：{fi["fetched2"]}  （偏差 {fi["diff2_pct"]}%）')
            print(f'     原文：{fi["raw_text"][:80]}')
            print()
        for item in missing_items:
            print(f'  ❌ [{item["id"]}] {item["label"]}：未提供 fetched_value')
        verdict = 'FAIL'

    if warn_count > 0:
        print(f'{YELLOW}注意：{warn_count} 个数据点两来源结果不一致（超过1%），可能是口径差异（GAAP/Non-GAAP或汇率），请人工复核。{RESET}')
        for wi in warn_items:
            source_diff = (
                f'；来源间偏差: {wi["source_diff_pct"]}%'
                if wi.get('source_diff_pct') is not None
                else ''
            )
            print(
                f'  ⚠️  {wi["label"]}  报告:{wi["reported"]} {wi["unit"]}  '
                f'偏差: {wi["diff1_pct"]}% / {wi["diff2_pct"]}%{source_diff}'
            )

    print('=' * 70)

    return {
        'verdict': verdict,
        'pass_count': pass_count,
        'warn_count': warn_count,
        'fail_count': fail_count,
        'missing_count': missing_count,
        'total': total,
        'fail_items': fail_items,
        'warn_items': warn_items,
    }


# ---------------------------------------------------------------------------
# CLI Entry Point
# ---------------------------------------------------------------------------

def main():
    parser = argparse.ArgumentParser(
        description='Report Audit Tool — 研究报告数据抽检工具',
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
工作流程：

  Step 1 — 全量检查显式审计事实区：
    python3 tools/report_audit.py lint --report reports/腾讯/腾讯-research-20260408.md

  Step 2 — 提取数据点并随机抽样 15%，输出抽检清单：
    python3 tools/report_audit.py extract --report reports/腾讯/腾讯-research-20260408.md \
      --output tmp/腾讯-audit.json

  Step 3 — Agent 对清单中每个数据点，从两个独立可靠信源取数，
            原样填入来源原始值、单位、原文摘录、直接 URL 和页内定位；
            把联网工具保存的页面正文用 source-snapshot 绑定到对应 URL；
            fetched_value/fetched_value2 保持 null，由脚本换算。判决失败时修正报告
            或如实保持打回，禁止改写/更换来源或把来源原始值、摘录改成报告值。

  Step 4 — 一次性预检全部输入问题：
    python3 tools/report_audit.py preflight --results-file tmp/audit-results.json \
      --report reports/腾讯/腾讯-research-20260408.md

  Step 5 — 输入核验结果，输出准出/打回判决：
    python3 tools/report_audit.py verdict --results-file tmp/audit-results.json \
      --report reports/腾讯/腾讯-research-20260408.md
    准出时自动在报告旁写入 .audit-pass.json 凭证。

  一步预览（只打印抽检清单，不核验）：
    python3 tools/report_audit.py extract --report reports/xxx.md --dry-run

  抽样比例固定为 15%（不足 20 项时至少抽 3 项或全量），seed 由完整事实池摘要推导；
  结果文件不能换 seed 或比例。
        """)

    sub = parser.add_subparsers(dest='command')

    lint = sub.add_parser('lint', help='全量检查报告事实池与单位，不做随机抽样')
    lint.add_argument('--report', required=True, help='报告文件路径（Markdown）')

    snapshot = sub.add_parser(
        'source-snapshot',
        help='把联网工具保存的来源正文绑定为可复核快照',
    )
    snapshot.add_argument('--source', required=True, help='正文对应的直接证据 URL')
    snapshot.add_argument('--input', required=True, help='联网工具保存的 UTF-8 页面正文')
    snapshot.add_argument('--output', required=True, help='输出的 source snapshot JSON')

    verify = sub.add_parser('verify-pass', help='验证报告旁的准出凭证和证据账本')
    verify.add_argument('--report', required=True, help='待确认为有效基线的 Markdown 报告')

    # extract
    ext = sub.add_parser('extract', help='从报告提取数据点并随机抽样')
    ext.add_argument('--report', required=True, help='报告文件路径（Markdown）')
    ext.add_argument('--dry-run', action='store_true', help='只打印，不输出 JSON')
    ext.add_argument(
        '--output',
        help='把带报告哈希与样本摘要的可填写审计包直接写入 UTF-8 JSON 文件',
    )

    # preflight
    pre = sub.add_parser('preflight', help='一次性预检全部证据字段，不写证据账本')
    pre.add_argument(
        '--results-file',
        required=True,
        help='extract 生成并已填写来源证据字段的 UTF-8 审计包文件',
    )
    pre.add_argument('--report', required=True, help='被审计 Markdown 报告的实际路径')

    # verdict
    vrd = sub.add_parser('verdict', help='根据核验结果输出准出/打回判决')
    vrd.add_argument(
        '--results-file',
        required=True,
        help='extract 生成并已填写来源证据字段的 UTF-8 审计包文件',
    )
    vrd.add_argument('--report', required=True, help='被审计 Markdown 报告的实际路径')
    vrd.add_argument('--output-json', action='store_true', help='将判决结果以 JSON 输出到 stdout')

    args = parser.parse_args()

    if args.command == 'lint':
        if not os.path.isfile(args.report):
            print(f'❌ 文件不存在: {args.report}', file=sys.stderr)
            sys.exit(1)
        try:
            with open(args.report, 'r', encoding='utf-8') as f:
                text = f.read()
            points = validate_report_for_audit(text, require_scope=True)
        except (OSError, ValueError) as exc:
            print(f'❌ 报告事实池预检失败: {exc}', file=sys.stderr)
            sys.exit(1)
        print(f'✅ 报告事实池预检通过：{len(points)} 个事实均有明确且可换算的单位')

    elif args.command == 'source-snapshot':
        try:
            with open(args.input, 'rb') as f:
                raw_content = f.read(_MAX_SOURCE_SNAPSHOT_BYTES + 1)
            if len(raw_content) > _MAX_SOURCE_SNAPSHOT_BYTES:
                raise ValueError('来源正文超过 10 MiB 上限')
            content = raw_content.decode('utf-8')
            digest = write_source_snapshot(args.source, content, args.output)
        except (OSError, UnicodeDecodeError, ValueError) as exc:
            print(f'❌ 来源快照生成失败: {exc}', file=sys.stderr)
            sys.exit(1)
        print(f'快照文件：{os.path.abspath(args.output)}')
        print(f'SHA256={digest}')

    elif args.command == 'verify-pass':
        if not os.path.isfile(args.report):
            print(f'❌ 报告文件不存在: {args.report}', file=sys.stderr)
            sys.exit(1)
        try:
            validate_pass_receipt(args.report)
        except ValueError as exc:
            print(f'❌ 准出凭证无效: {exc}', file=sys.stderr)
            sys.exit(1)
        print(f'✅ 准出凭证有效：{os.path.abspath(args.report)}')

    elif args.command == 'extract':
        if not os.path.isfile(args.report):
            print(f'❌ 文件不存在: {args.report}', file=sys.stderr)
            sys.exit(1)

        with open(args.report, 'r', encoding='utf-8') as f:
            text = f.read()

        try:
            bundle = build_audit_bundle(text, report_path=args.report, strict=True)
            all_points = validate_report_for_audit(text, require_scope=True)
        except ValueError as exc:
            print(f'❌ 报告事实池预检失败: {exc}', file=sys.stderr)
            sys.exit(1)
        sampled = bundle['results']

        print('=' * 70)
        print(f'报告数据抽检清单')
        print(f'文件：{args.report}')
        print(f'总提取数据点：{len(all_points)}  |  抽样比例：{_AUDIT_SAMPLE_RATIO:.0%}  |  抽检数量：{len(sampled)}')
        print(
            f'随机种子：{bundle["sample_seed"]}'
            '（由完整事实池摘要确定，审计包不能改比例或换 seed）'
        )
        print('=' * 70)
        print()
        print(f'{"ID":>3}  {"行号":>5}  {"数据标签":<35}  {"报告值":>12}  {"单位"}')
        print(f'{"─"*3}  {"─"*5}  {"─"*35}  {"─"*12}  {"─"*6}')
        for p in sampled:
            print(f'{p["id"]:>3}  {p["line_number"]:>5}  {p["label"][:35]:<35}  {p["reported_value"]:>12.2f}  {p["unit"]}')
        print()
        print('↑ 请对每项从两个独立信源取数，并记录原始值/单位、原文摘录、直接 URL、定位与正文快照：')
        print('  美股：macrotrends.net（主）+ stockanalysis.com（副）')
        print('  港股：aastocks.com（主）+ macrotrends ADR（副）')
        print('  A股： eastmoney.com（主）+ cninfo.com.cn（副）')
        print()

        if not args.dry_run:
            rendered = json.dumps(bundle, ensure_ascii=False, indent=2)
            if args.output:
                output_path = os.path.abspath(args.output)
                parent = os.path.dirname(output_path)
                if parent:
                    os.makedirs(parent, exist_ok=True)
                with open(output_path, 'w', encoding='utf-8', newline='\n') as f:
                    f.write(rendered)
                    f.write('\n')
                print(f'审计包已写入：{output_path}')
            else:
                print(
                    '抽检审计包 JSON（只填写 results 中的来源证据字段后，'
                    '依次传给 preflight 和 verdict）：'
                )
                print()
                print(rendered)

    elif args.command == 'preflight':
        try:
            with open(args.results_file, 'r', encoding='utf-8') as f:
                bundle = json.load(f)
        except (json.JSONDecodeError, OSError) as e:
            print(f'❌ JSON 解析失败: {e}', file=sys.stderr)
            sys.exit(1)
        if not os.path.isfile(args.report):
            print(f'❌ 报告文件不存在: {args.report}', file=sys.stderr)
            sys.exit(1)
        try:
            with open(args.report, 'r', encoding='utf-8') as f:
                report_text = f.read()
            results = validate_audit_bundle(bundle, report_text, report_path=args.report)
            validate_verdict_results(
                results,
                snapshot_base_dir=os.path.dirname(os.path.abspath(args.results_file)),
                require_snapshots=(
                    bundle.get('source_snapshot_policy') == _SOURCE_SNAPSHOT_POLICY
                ),
            )
        except (ValueError, OSError) as e:
            print(f'❌ 审计预检失败: {e}', file=sys.stderr)
            sys.exit(1)
        print(f'✅ 审计预检通过：{len(results)} 个抽检项的类型、URL、正文快照、摘录和单位均有效')

    elif args.command == 'verdict':
        try:
            with open(args.results_file, 'r', encoding='utf-8') as f:
                bundle = json.load(f)
        except (json.JSONDecodeError, OSError) as e:
            print(f'❌ JSON 解析失败: {e}', file=sys.stderr)
            sys.exit(1)
        if not os.path.exists(args.report):
            print(f'❌ 报告文件不存在: {args.report}', file=sys.stderr)
            sys.exit(1)
        try:
            with open(args.report, 'r', encoding='utf-8') as f:
                report_text = f.read()
            results = validate_audit_bundle(bundle, report_text, report_path=args.report)
            validate_verdict_results(
                results,
                snapshot_base_dir=os.path.dirname(os.path.abspath(args.results_file)),
                require_snapshots=(
                    bundle.get('source_snapshot_policy') == _SOURCE_SNAPSHOT_POLICY
                ),
            )
            ledger_path = enforce_evidence_ledger(
                args.results_file,
                bundle,
                results,
                report_path=args.report,
            )
        except (ValueError, OSError) as e:
            print(f'❌ results JSON 内容无效: {e}', file=sys.stderr)
            sys.exit(1)

        print(f'证据观察账本：{ledger_path}')
        outcome = render_verdict(results, report_name=args.report)
        if outcome['verdict'] == 'PASS':
            try:
                receipt_path = write_pass_receipt(args.report, bundle, ledger_path)
            except ValueError as exc:
                print(f'❌ 准出凭证生成失败: {exc}', file=sys.stderr)
                sys.exit(1)
            print(f'准出凭证：{receipt_path}')

        if args.output_json:
            print(json.dumps(outcome, ensure_ascii=False, indent=2))

        # 非零退出码表示打回，方便 CI/脚本判断
        sys.exit(0 if outcome['verdict'] == 'PASS' else 1)

    else:
        parser.print_help()


if __name__ == '__main__':
    main()
