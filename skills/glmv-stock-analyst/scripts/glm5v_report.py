#!/usr/bin/env python3
"""
Call GLM-5V-Turbo for stock chart analysis and write report artifacts.

Input:  stock_data_output/{code}_{timestamp}/ created by fetch_all.py.
Output: report.md, meta.json, and glm5v_result.json in the same directory,
        plus a compact JSON result on stdout for RClaw to return directly.
        webchat_summary contains the complete chat-window analysis report.

This is the only step in this skill that requires a multimodal LLM. Data
collection and chart generation are handled by fetch_all.py without LLM vision.
"""

import argparse
import base64
import json
import math
import re
import sys
import time
from datetime import date
from pathlib import Path

import requests


SCRIPT_DIR = Path(__file__).resolve().parent
SKILL_DIR = SCRIPT_DIR.parent
DEFAULT_CONFIG = SKILL_DIR / "config.toml"
ANALYSIS_GUIDELINES = SKILL_DIR / "references" / "analysis_guidelines.md"
REPORT_TEMPLATE = SKILL_DIR / "references" / "report_template.md"
HK_STOCK_KNOWLEDGE = SKILL_DIR / "references" / "hk_stock_knowledge.md"
SENSITIVE_COMPANIES = SKILL_DIR / "references" / "sensitive_companies.md"

# 408/504 是请求超时类；429 是速率/并发限制（大模型 key 共享时常见，code=1302），
# 立刻重试只会继续撞限，因此退避后再试。
RETRYABLE_STATUS_CODES = {408, 429, 504}
RETRY_STATUS_BACKOFF_SEC = 5
RETRYABLE_STREAM_EXCEPTIONS = (
    requests.exceptions.Timeout,
    requests.exceptions.ConnectionError,
    requests.exceptions.ChunkedEncodingError,
    requests.exceptions.ContentDecodingError,
)

IMAGE_LABELS = {
    "kline": "日K线图",
    "kline_weekly": "周K线图",
    "kline_monthly": "月K线图",
    "kline_intraday": "分时图",
    "valuation": "估值图",
    "capital_flow": "资金流向图",
    "macro": "宏观环境图",
    "adr_premium": "ADR溢价图",
    "financials_trend": "营收趋势图",
}

IMAGE_PRIORITY = [
    "kline",
    "kline_weekly",
    "kline_monthly",
    "kline_intraday",
    "capital_flow",
    "valuation",
    "financials_trend",
    "adr_premium",
    "macro",
]

META_DEFAULTS = {
    "投资观点": None,
    "行动建议": None,
    "风险等级": None,
    "观点置信度": None,
    "数据截至": None,
    "币种": None,
    "现价": None,
    "目标价": None,
    "目标期限月": None,
    "上行空间_pct": None,
    "估值方法": None,
    "止损位": None,
    "支撑位": [],
    "压力位": [],
    "PE_ttm": None,
    "PB": None,
    "股息率_pct": None,
}
META_ENUMS = {
    "投资观点": {"看多", "中性", "看空"},
    "行动建议": {"买入", "观望", "回避"},
    "风险等级": {"低", "中", "高"},
}
META_TEXT_LIMITS = {"数据截至": 128, "币种": 16, "估值方法": 80}
META_CURRENCIES = {"CNY", "HKD", "USD"}
META_NUMBER_KEYS = {
    "观点置信度",
    "现价",
    "目标价",
    "目标期限月",
    "上行空间_pct",
    "止损位",
    "PE_ttm",
    "PB",
    "股息率_pct",
}


class ConfigError(Exception):
    pass


class StreamInterruptedError(RuntimeError):
    pass


def strip_comment(line):
    in_quote = False
    quote = ""
    escaped = False
    for i, ch in enumerate(line):
        if escaped:
            escaped = False
            continue
        if ch == "\\":
            escaped = True
            continue
        if ch in ("'", '"'):
            if in_quote and ch == quote:
                in_quote = False
                quote = ""
            elif not in_quote:
                in_quote = True
                quote = ch
        elif ch == "#" and not in_quote:
            return line[:i]
    return line


def parse_toml_value(raw):
    raw = raw.strip()
    if not raw:
        return ""
    if (raw[0], raw[-1]) in (('"', '"'), ("'", "'")):
        return raw[1:-1]
    lower = raw.lower()
    if lower in ("true", "false"):
        return lower == "true"
    try:
        if "." in raw:
            return float(raw)
        return int(raw)
    except ValueError:
        return raw


def load_config(path, require_key=True):
    path = Path(path)
    if not path.exists():
        raise ConfigError(f"缺少配置文件: {path}")

    cfg = {}
    for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        line = strip_comment(line).strip()
        if not line or line.startswith("["):
            continue
        if "=" not in line:
            raise ConfigError(f"{path}:{line_no} 不是 key = value 格式")
        key, raw = line.split("=", 1)
        cfg[key.strip()] = parse_toml_value(raw)

    key = str(cfg.get("api_key") or "").strip()
    if require_key and not key:
        raise ConfigError(
            f"未配置 API key，请填写配置文件 {path} 中的 api_key。"
        )
    cfg["_api_key"] = key

    base_url = str(cfg.get("base_url") or "").strip()
    model = str(cfg.get("model") or "").strip()
    if not base_url:
        raise ConfigError("base_url 不能为空")
    if not model:
        raise ConfigError("model 不能为空")

    for name in (
        "connect_timeout_sec",
        "timeout_sec",
        "max_tokens",
        "image_limit",
        "max_json_chars",
    ):
        value = cfg.get(name)
        if not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise ConfigError(f"{name} 必须为正整数")

    # 墙钟预算（可选配置）：timeout_sec 只是流式读取的空闲超时，挡不住服务端
    # 持续滴流 reasoning 而永不产出的挂起，曾耗满 skill.toml 的 25 分钟 Manifest
    # 超时后被强杀、任务目录无任何产物。attempt 限制单次尝试的墙钟，total 限制
    # 含重试与返工的整次调用，保证脚本先于 Manifest 超时干净报错。
    for name, default in (("attempt_timeout_sec", 480), ("total_timeout_sec", 1440)):
        value = cfg.get(name)
        if value is None:
            cfg[name] = default
        elif not isinstance(value, int) or isinstance(value, bool) or value <= 0:
            raise ConfigError(f"{name} 必须为正整数")

    max_retries = cfg.get("max_retries")
    if (
        not isinstance(max_retries, int)
        or isinstance(max_retries, bool)
        or max_retries < 0
    ):
        raise ConfigError("max_retries 必须为非负整数")

    if str(cfg.get("auth") or "").strip().lower() != "bearer":
        raise ConfigError('auth 必须为 "bearer"')
    if not str(cfg.get("max_tokens_key") or "").strip():
        raise ConfigError("max_tokens_key 不能为空")

    thinking = str(cfg.get("thinking") or "").strip().lower()
    if thinking != "enabled":
        raise ConfigError(
            'thinking 必须保持为 "enabled"；GLM-5V-Turbo 流式调用与重试时禁止关闭思考模式'
        )
    cfg["thinking"] = "enabled"
    if cfg.get("stream") is not True:
        raise ConfigError("stream 必须保持为 true")
    return cfg


def chat_endpoint(base_url):
    base = str(base_url).strip().rstrip("/")
    if base.endswith("/chat/completions"):
        return base
    return f"{base}/chat/completions"


def read_json(path):
    with Path(path).open("r", encoding="utf-8-sig") as f:
        return json.load(f)


def compact_json(data, max_chars):
    text = json.dumps(data, ensure_ascii=False, indent=2, default=str)
    if len(text) <= max_chars:
        return text
    return text[:max_chars] + "\n... JSON 已截断，请优先依据未截断字段和图片分析。"


def read_optional_text(path):
    if not path:
        return ""
    p = Path(path)
    if not p.exists():
        raise FileNotFoundError(f"找不到补充上下文文件: {p}")
    return p.read_text(encoding="utf-8")


def read_reference(path):
    path = Path(path)
    if not path.exists():
        return ""
    return path.read_text(encoding="utf-8")


def resolve_image_path(raw_path, output_dir):
    p = Path(str(raw_path))
    if not p.is_absolute():
        p = Path(output_dir) / p
    return p


def collect_images(summary, output_dir, limit):
    images = summary.get("images") or {}
    pairs = []
    seen = set()

    def add(key, raw_path):
        if not raw_path:
            return
        path = resolve_image_path(raw_path, output_dir)
        try:
            path = path.resolve()
        except OSError:
            return
        if not path.exists() or path in seen:
            return
        seen.add(path)
        pairs.append((IMAGE_LABELS.get(key, key), path))

    for key in IMAGE_PRIORITY:
        add(key, images.get(key))
    for key, raw_path in images.items():
        add(key, raw_path)

    if not pairs:
        for path in sorted(Path(output_dir).glob("*")):
            if path.suffix.lower() in (".png", ".jpg", ".jpeg", ".webp"):
                add(path.stem, path)

    return pairs[: max(0, int(limit))]


def image_block(path):
    data = base64.b64encode(path.read_bytes()).decode("utf-8")
    return {"type": "image_url", "image_url": {"url": data}}


def build_prompt(summary, data_json, image_pairs, user_request, extra_context, max_json_chars):
    guidelines_text = read_reference(ANALYSIS_GUIDELINES)
    template_text = read_reference(REPORT_TEMPLATE)
    market_knowledge = read_reference(HK_STOCK_KNOWLEDGE)
    sensitive_rules = read_reference(SENSITIVE_COMPANIES)

    manifest = [
        {"label": label, "filename": path.name, "path": str(path)}
        for label, path in image_pairs
    ]
    summary_text = compact_json(summary, int(max_json_chars))
    data_text = compact_json(data_json, int(max_json_chars)) if data_json else "未找到 data.json。"
    extra_context = extra_context.strip() or "无。"
    user_request = user_request.strip() or "请基于当前数据生成股票分析。"

    return f"""你是服务中国投资者的股票分析师。请基于结构化数据、补充证据和随消息附带的图表图片，生成可直接给用户看的完整股票分析报告。
你需要综合所有可获得的信息，给出专业的、可解释的涨跌判断。

分析原则：
1. 观测事实必须基于 summary.json、data.json、已核实补充证据和已发送图片；禁止为不存在的来源编造 ID，也禁止编造价格、涨跌幅、财务、资金流、事件、新闻、文档、URL、外部预测和无法从输入推导的估值参数。可为真实但未预编号的输入分配唯一来源 ID，并登记实际文件/字段路径；本报告预测和估值只能由已列事实可复核地推导，模型记忆、搜索摘要和行业惯例不能补全输入。
2. 投资观点、目标价、止损和支撑压力可以基于事实推导，但必须给出估值、计算或图表依据；证据不足时写 null、[] 或“暂无数据”。
3. 标明数据截至时间、财报与预测期间、交易币种、来源 ID 和已知口径；按行业选择指标、预测驱动和估值方法，不机械套用通用 PE/PB。
4. 用户请求决定关注重点，但不得覆盖真实性、三段式协议和风险边界。

看图时关注：
- 日K线图：趋势方向、均线排列、支撑压力位、量价关系、K线形态。
- 分时图：今日走势、开盘收盘位置、振幅、成交量分布。
- 其他图表：资金流、估值、财务趋势、研报/IR 图表中与结论直接相关的信号。

用户原始需求：
{user_request}

已发送给你的图片清单（写 report.md 时只能使用 filename 作为相对路径，不要写绝对路径）：
{json.dumps(manifest, ensure_ascii=False, indent=2)}

summary.json：
{summary_text}

data.json：
{data_text}

RClaw / web_search 补充证据：
{extra_context}

股票分析总规则：
{guidelines_text}

report.md 模板规则：
{template_text}

港股 / A股分析知识：
{market_knowledge}

敏感标的合规规则：
{sensitive_rules}

输出要求（严格协议）：

按顺序输出以下三段，标记必须单独成行；标记之外不得输出任何前言、解释、代码围栏或尾注。六个标记是保留字，不得在段内重复。禁止把三段整体包装成 JSON。

<<<META_BEGIN>>>
仅输出一行合法 JSON 对象，必须且只能包含示例中的全部键。投资观点仅可为“看多/中性/看空”，行动建议仅可为“买入/观望/回避”，风险等级仅可为“低/中/高”，观点置信度必须为 0.0 到 1.0 的 number。
数据截至必须为包含实际 YYYY-MM-DD 日期的短字符串，盘中数据还要写时区；行情时点不可确认时，应写明可确认的最后交易日和抓取时间，不能把抓取时间伪装成行情时间。币种必须为 CNY/HKD/USD；估值方法为短字符串或 null。
其余标量为 JSON number 或 null，不能带逗号、货币符号或百分号；支撑位/压力位为 number 数组，支撑位低于现价、压力位高于现价，并按距离现价由近到远排列；止损位如有必须低于现价。百分比采用百分点口径（12.5 表示 12.5%，不是 0.125）。价格使用标的交易币种；目标价、支撑位、压力位或止损位任一非 null/非空时，现价也必须有效。
目标价非 null 时，目标期限月必须为 6-12 的整数且估值方法必须非 null；估值基准日价值未滚动至该目标期限时，目标价和目标期限月均填 null，只在详细报告标明估值日公允价值。现价和目标价均有效时，上行空间_pct 必须等于 (目标价/现价-1)*100，任一缺失时填 null。PE 为负或无意义时填 null。合法格式示例（值必须替换为真实结论，日期占位符不能原样输出）：
{{"投资观点":"中性","行动建议":"观望","风险等级":"中","观点置信度":0.75,"数据截至":"YYYY-MM-DD HH:MM TZ","币种":"HKD","现价":null,"目标价":null,"目标期限月":null,"上行空间_pct":null,"估值方法":null,"止损位":null,"支撑位":[],"压力位":[],"PE_ttm":null,"PB":null,"股息率_pct":null}}
<<<META_END>>>
<<<WEBSUMMARY_BEGIN>>>
聊天窗口可直接阅读的精炼完整报告，约 600-1200 个中文字符，原生 Markdown 书写，不放文本折线图，不要假设用户会看 HTML 报告。必须包含 6 个部分：
- 第1部分：标题，格式为“📊 {{股票名}} ({{代码}}) 快速总结”。
- 第2部分：核心数据，写明行情截至时间、币种、股价、市值、财报期间、2-4 个行业相关财务指标、可得的关键预测/估值以及带口径的资金面；关键预测注明“公司指引/外部一致预期/本报告情景”归属，没有则写“暂无数据”。
- 第3部分：近期走势分析（重点），用 3-6 句话说明观察窗口、阶段、关键转折、技术状态、量价验证及其与基本面的关系；每句话都要有事实或可验证推导支撑。
- 第4部分：多空对比表，每侧 2-4 条，必须事实驱动，不写空话。
- 第5部分：结论 + 操作建议，分别列出投资观点（🟢看多/🟡中性/🔴看空）、行动建议（买入/观望/回避）、风险等级（低/中/高）和观点置信度；按已持仓者、观望者、短线/激进者给出触发条件、失效条件，特别提示只写适用事项。
- 第6部分：详细报告提示，写“📄 详细报告已生成，系统会按当前会话通道发送附件或打开本地报告。”
<<<WEBSUMMARY_END>>>
<<<REPORT_BEGIN>>>
完整 Markdown 图文报告，默认约 4000-8000 个中文字符；数据丰富时可按需超过，不得为满足长度省略必需内容。严格遵守上方股票分析总规则和 report.md 模板，尤其是逐项来源、未来 2-3 年预测、利润桥、三表勾稽、适用时的 DCF 显性折现/净债务/股权价值桥/稀释股数，以及行业替代估值。输入不足时明确缺失，不机械外推。有相关图片时只能引用清单中实际存在的 filename；清单为空或图片缺失时明确说明，不生成图片链接。提供关键依据、计算过程和可核验推导，不输出冗长的内部思维过程。目标价须写明币种、估值方法、基准情景和未来 6-12 个月内的具体期限；估值基准日不同于目标日时说明滚动方法，未滚动结果只称估值日公允价值；证据不足则不设目标价。末尾只保留一次风险提示/免责声明。
<<<REPORT_END>>>

一致性约束：META、聊天摘要和详细报告中共有的投资观点、行动建议、风险等级、观点置信度、现价、目标价、期限、上行空间、支撑压力和止损必须一致；聊天摘要与详细报告的预测口径和三种情景必须一致。观测事实来自输入并可追溯至来源登记；推导值必须解释依据。所有建议均为带条件的研究判断，不承诺收益；以上分析仅供参考，不构成投资建议。
"""


def parse_stream_response(resp, deadline=None):
    """Aggregate one OpenAI-compatible SSE response without retaining reasoning.

    deadline 为可选的 time.monotonic() 墙钟上限：服务端持续滴流数据但迟迟不产出
    正文时，空闲超时不会触发，由它在下一条数据到达时中断本次尝试。
    """
    content_parts = []
    finish_reason = ""
    saw_done = False
    aggregate = {}

    # SSE is UTF-8. Setting this explicitly also avoids requests defaulting some
    # text/* responses to ISO-8859-1 when the server omits a charset.
    resp.encoding = "utf-8"
    for line in resp.iter_lines(decode_unicode=True):
        if deadline is not None and time.monotonic() > deadline:
            raise StreamInterruptedError("GLM-5V-Turbo 流式尝试超出单次墙钟上限")
        if isinstance(line, bytes):
            try:
                line = line.decode("utf-8")
            except UnicodeDecodeError as exc:
                raise StreamInterruptedError(
                    "GLM-5V-Turbo SSE 流包含不完整的 UTF-8 数据"
                ) from exc
        if not line:
            continue
        if line.startswith("data: "):
            payload = line[6:]
        elif line.startswith("data:"):
            payload = line[5:].lstrip()
        else:
            # Ignore SSE comments and event/id/retry fields, matching RClaw.
            continue

        if payload.strip() == "[DONE]":
            saw_done = True
            break

        try:
            event = json.loads(payload)
        except json.JSONDecodeError as exc:
            raise StreamInterruptedError(
                "GLM-5V-Turbo SSE 流包含不完整的 JSON 事件"
            ) from exc
        if not isinstance(event, dict):
            raise RuntimeError("GLM-5V-Turbo SSE 事件必须是 JSON 对象")

        error = event.get("error")
        if error is not None:
            if isinstance(error, dict):
                detail = str(
                    error.get("message")
                    or error.get("code")
                    or error.get("type")
                    or "服务端未提供错误详情"
                )
            else:
                detail = str(error)
            raise RuntimeError(f"GLM-5V-Turbo 流式响应返回错误: {detail}")

        for key in ("id", "request_id", "created", "model"):
            if key not in aggregate and key in event:
                aggregate[key] = event[key]
        if isinstance(event.get("usage"), dict):
            aggregate["usage"] = event["usage"]

        choices = event.get("choices")
        if choices is None:
            continue
        if not isinstance(choices, list):
            raise RuntimeError("GLM-5V-Turbo SSE choices 必须是数组")
        if not choices:
            continue
        choice = choices[0]
        if not isinstance(choice, dict):
            raise RuntimeError("GLM-5V-Turbo SSE choices[0] 必须是对象")

        delta = choice.get("delta")
        if delta is not None:
            if not isinstance(delta, dict):
                raise RuntimeError("GLM-5V-Turbo SSE delta 必须是对象")
            content = delta.get("content")
            if content is not None:
                if not isinstance(content, str):
                    raise RuntimeError("GLM-5V-Turbo SSE delta.content 必须是字符串")
                content_parts.append(content)
            # Deliberately ignore delta.reasoning_content: receiving it keeps the
            # stream alive, but reasoning must not be printed or persisted.

        current_finish = choice.get("finish_reason")
        if current_finish is not None:
            if not isinstance(current_finish, str):
                raise RuntimeError("GLM-5V-Turbo SSE finish_reason 必须是字符串")
            if current_finish:
                if finish_reason and current_finish != finish_reason:
                    raise RuntimeError("GLM-5V-Turbo SSE finish_reason 前后不一致")
                finish_reason = current_finish

    if not finish_reason:
        ending = "收到 [DONE] 但缺少 finish_reason" if saw_done else "EOF 前未收到终止事件"
        raise StreamInterruptedError(f"GLM-5V-Turbo 流式响应不完整：{ending}")

    aggregate["choices"] = [
        {
            "index": 0,
            "message": {"role": "assistant", "content": "".join(content_parts)},
            "finish_reason": finish_reason,
        }
    ]
    return aggregate


def _http_error_detail(resp):
    resp.encoding = "utf-8"
    try:
        return json.dumps(resp.json(), ensure_ascii=False)
    except ValueError:
        return resp.text


def call_glm5v(cfg, prompt, image_pairs, session_deadline=None):
    content = [image_block(path) for _, path in image_pairs]
    content.append({"type": "text", "text": prompt})

    body = {
        "model": str(cfg["model"]),
        "messages": [{"role": "user", "content": content}],
    }
    max_tokens = int(cfg.get("max_tokens") or 0)
    if max_tokens > 0:
        max_tokens_key = str(cfg.get("max_tokens_key") or "max_tokens").strip()
        body[max_tokens_key] = max_tokens

    body["thinking"] = {"type": "enabled"}
    body["stream"] = cfg["stream"]

    auth = str(cfg.get("auth") or "bearer").strip().lower()
    if auth != "bearer":
        raise ConfigError("glm5v_report.py 目前仅支持 auth = \"bearer\"")

    endpoint = chat_endpoint(cfg["base_url"])
    connect_timeout_sec = int(cfg["connect_timeout_sec"])
    timeout_sec = int(cfg["timeout_sec"])
    max_retries = int(cfg["max_retries"])
    attempt_timeout_sec = int(cfg["attempt_timeout_sec"])
    if session_deadline is None:
        session_deadline = time.monotonic() + int(cfg["total_timeout_sec"])
    for attempt in range(max_retries + 1):
        remaining = session_deadline - time.monotonic()
        if remaining <= 30:
            raise RuntimeError(
                "GLM-5V-Turbo 总时长预算（total_timeout_sec）已耗尽，"
                "不再发起尝试；请缩小图表数量或降低 max_tokens 后重试"
            )
        attempt_deadline = time.monotonic() + min(attempt_timeout_sec, remaining)
        try:
            response = requests.post(
                endpoint,
                headers={
                    "Authorization": f"Bearer {cfg['_api_key']}",
                    "Content-Type": "application/json",
                },
                json=body,
                timeout=(connect_timeout_sec, timeout_sec),
                stream=True,
            )
            with response as resp:
                if resp.status_code in RETRYABLE_STATUS_CODES:
                    if attempt >= max_retries:
                        raise RuntimeError(
                            f"GLM-5V-Turbo 经过 {attempt + 1} 次尝试后仍请求失败"
                            f"（最后一次 HTTP {resp.status_code}）；"
                            "思考模式始终保持 enabled"
                        )
                    print(
                        f"WARNING: GLM-5V-Turbo 返回 HTTP {resp.status_code}，"
                        f"退避 {RETRY_STATUS_BACKOFF_SEC} 秒后保持 thinking=enabled 重试"
                        f"（{attempt + 1}/{max_retries}）。",
                        file=sys.stderr,
                    )
                    time.sleep(RETRY_STATUS_BACKOFF_SEC)
                    continue
                if not resp.ok:
                    raise RuntimeError(
                        f"GLM-5V-Turbo 请求失败: HTTP {resp.status_code}\n"
                        f"{_http_error_detail(resp)}"
                    )
                try:
                    return parse_stream_response(resp, attempt_deadline)
                except StreamInterruptedError as exc:
                    if attempt >= max_retries:
                        raise RuntimeError(
                            f"GLM-5V-Turbo 经过 {attempt + 1} 次尝试后流式响应仍中断；"
                            "已丢弃未完成内容，思考模式始终保持 enabled"
                        ) from exc
                    print(
                        "WARNING: GLM-5V-Turbo 流式响应未完整结束，已丢弃本次内容，"
                        f"将保持 thinking=enabled 重试（{attempt + 1}/{max_retries}）。",
                        file=sys.stderr,
                    )
                    continue
        except RETRYABLE_STREAM_EXCEPTIONS as exc:
            if attempt >= max_retries:
                raise RuntimeError(
                    f"GLM-5V-Turbo 经过 {attempt + 1} 次尝试后连接或流式读取仍失败"
                    f"（连接超时 {connect_timeout_sec} 秒、流读取空闲超时 {timeout_sec} 秒）；"
                    "已丢弃未完成内容，思考模式始终保持 enabled"
                ) from exc
            print(
                "WARNING: GLM-5V-Turbo 连接或流式读取失败，已丢弃本次内容，"
                f"将保持 thinking=enabled 重试（{attempt + 1}/{max_retries}）。",
                file=sys.stderr,
            )
            continue
    raise RuntimeError("GLM-5V-Turbo 流式请求未返回结果")


def extract_message_text(raw_response):
    try:
        msg = raw_response["choices"][0]["message"]
    except (KeyError, IndexError, TypeError) as exc:
        raise RuntimeError(f"响应中缺少 choices[0].message: {exc}") from exc
    content = msg.get("content")
    if isinstance(content, list):
        parts = []
        for item in content:
            if isinstance(item, dict) and item.get("type") == "text":
                parts.append(str(item.get("text") or ""))
            elif isinstance(item, str):
                parts.append(item)
        return "\n".join(parts).strip()
    return str(content or "").strip()


def _section(text, name):
    """取出 <<<NAME_BEGIN>>> ... <<<NAME_END>>> 之间的内容与闭合状态。

    END 标记缺失或错放到下一段之后时，最多取到下一段 BEGIN，避免混入后续内容。
    """
    begin = f"<<<{name}_BEGIN>>>"
    end = f"<<<{name}_END>>>"
    start = text.find(begin)
    if start == -1:
        return "", False
    start += len(begin)
    remainder = text[start:]
    next_begin = re.search(r"<<<(?:META|WEBSUMMARY|REPORT)_BEGIN>>>", remainder)
    finish = text.find(end, start)
    if finish != -1 and (not next_begin or finish - start < next_begin.start()):
        return text[start:finish].strip(), True

    if next_begin:
        remainder = remainder[: next_begin.start()]
    return remainder.strip(), False


def _protocol_structure_issues(text):
    """返回三段式协议的全部结构问题。"""
    markers = [
        "<<<META_BEGIN>>>",
        "<<<META_END>>>",
        "<<<WEBSUMMARY_BEGIN>>>",
        "<<<WEBSUMMARY_END>>>",
        "<<<REPORT_BEGIN>>>",
        "<<<REPORT_END>>>",
    ]
    lines = text.strip().splitlines()
    positions = []
    issues = []
    for marker in markers:
        exact_positions = [index for index, line in enumerate(lines) if line == marker]
        if text.count(marker) != 1 or len(exact_positions) != 1:
            issues.append(f"标记 {marker} 必须唯一且独占一行")
            continue
        positions.append(exact_positions[0])

    if len(positions) >= 2 and positions != sorted(positions):
        issues.append("现有协议标记顺序错误")
    if len(positions) == len(markers) and (
        positions[0] != 0 or positions[-1] != len(lines) - 1
    ):
        issues.append("协议标记之外存在文本")

    return issues


def _empty_meta():
    return {
        key: ([] if isinstance(default, list) else default)
        for key, default in META_DEFAULTS.items()
    }


def _is_number(value):
    return (
        isinstance(value, (int, float))
        and not isinstance(value, bool)
        and math.isfinite(value)
    )


def validate_meta_with_issues(parsed, expected_currency):
    """严格校验并规范化 META；返回规范化结果、状态和全部问题。"""
    meta = _empty_meta()
    issues = []
    expected = set(META_DEFAULTS)
    actual = set(parsed)
    missing = sorted(expected - actual)
    extra = sorted(actual - expected)
    if missing:
        issues.append("缺少键: " + ",".join(missing))
    if extra:
        issues.append("包含未定义键: " + ",".join(extra))

    for key, allowed in META_ENUMS.items():
        value = parsed.get(key)
        if value in allowed:
            meta[key] = value
        else:
            issues.append(f"{key} 枚举无效")

    if meta["投资观点"] == "看空" and meta["行动建议"] == "买入":
        issues.append("投资观点为看空时，行动建议不能为买入")

    for key, max_len in META_TEXT_LIMITS.items():
        value = parsed.get(key)
        if value is None:
            continue
        if isinstance(value, str) and value.strip() and len(value.strip()) <= max_len:
            meta[key] = value.strip()
        else:
            issues.append(f"{key} 必须为短字符串或 null")

    currency = meta["币种"]
    if currency not in META_CURRENCIES:
        issues.append("币种必须为 CNY、HKD 或 USD")
        meta["币种"] = None
    if expected_currency in META_CURRENCIES and meta["币种"] != expected_currency:
        issues.append(f"币种与输入市场不一致，应为 {expected_currency}")
        meta["币种"] = expected_currency
    if meta["数据截至"] is None:
        issues.append("数据截至不能为空")
    else:
        date_match = re.search(
            r"\b\d{4}-(?:0[1-9]|1[0-2])-(?:0[1-9]|[12]\d|3[01])\b",
            meta["数据截至"],
        )
        try:
            if date_match is None:
                raise ValueError
            date.fromisoformat(date_match.group(0))
        except ValueError:
            issues.append("数据截至必须包含 YYYY-MM-DD 格式的实际日期")
            meta["数据截至"] = None

    for key in META_NUMBER_KEYS:
        value = parsed.get(key)
        if value is None:
            continue
        if _is_number(value):
            meta[key] = value
        else:
            issues.append(f"{key} 必须为 number 或 null")

    for key in ("支撑位", "压力位"):
        value = parsed.get(key)
        if not isinstance(value, list):
            issues.append(f"{key} 必须为 number 数组")
            continue
        valid_levels = [item for item in value if _is_number(item) and item > 0]
        if len(valid_levels) != len(value):
            issues.append(f"{key} 含非正数或非 number")
        meta[key] = valid_levels

    confidence = meta["观点置信度"]
    if confidence is None or not 0 <= confidence <= 1:
        issues.append("观点置信度必须在 0.0 到 1.0 之间")
        meta["观点置信度"] = None

    for key in ("现价", "目标价", "止损位", "PE_ttm", "PB"):
        value = meta[key]
        if value is not None and value <= 0:
            issues.append(f"{key} 必须为正数或 null")
            meta[key] = None

    divy = meta["股息率_pct"]
    if divy is not None and divy < 0:
        issues.append("股息率_pct 必须为非负数或 null")
        meta["股息率_pct"] = None

    horizon = meta["目标期限月"]
    if horizon is not None:
        if not float(horizon).is_integer() or not 6 <= horizon <= 12:
            issues.append("目标期限月必须为 6-12 的整数或 null")
            meta["目标期限月"] = None
        else:
            meta["目标期限月"] = int(horizon)

    current, target = meta["现价"], meta["目标价"]
    if target is not None and current is None:
        issues.append("目标价非空时必须提供有效现价")
        meta["目标价"] = None
        target = None
    if target is not None:
        if meta["目标期限月"] is None:
            issues.append("目标价非空时必须提供目标期限月")
        if meta["估值方法"] is None:
            issues.append("目标价非空时必须提供估值方法")
    elif meta["目标期限月"] is not None:
        issues.append("目标价为空时，目标期限月必须为 null")
        meta["目标期限月"] = None

    if current is not None and target is not None:
        calculated = round((target / current - 1) * 100, 2)
        supplied = meta["上行空间_pct"]
        if supplied is None or not math.isclose(supplied, calculated, abs_tol=0.05):
            issues.append("上行空间_pct 与现价/目标价不一致，已按公式重算")
        meta["上行空间_pct"] = calculated
    elif meta["上行空间_pct"] is not None:
        issues.append("缺少有效现价或目标价时，上行空间_pct 必须为 null")
        meta["上行空间_pct"] = None

    if current is not None:
        invalid_support = [value for value in meta["支撑位"] if value >= current]
        invalid_resistance = [value for value in meta["压力位"] if value <= current]
        if invalid_support:
            issues.append("支撑位必须低于现价")
            meta["支撑位"] = [value for value in meta["支撑位"] if value < current]
        if invalid_resistance:
            issues.append("压力位必须高于现价")
            meta["压力位"] = [value for value in meta["压力位"] if value > current]
        for key in ("支撑位", "压力位"):
            meta[key].sort(key=lambda value: abs(value - current))
        stop = meta["止损位"]
        if stop is not None and stop >= current:
            issues.append("止损位必须低于现价")
            meta["止损位"] = None
    else:
        if meta["支撑位"] or meta["压力位"] or meta["止损位"] is not None:
            issues.append("支撑位、压力位或止损位非空时必须提供现价")
            meta["支撑位"] = []
            meta["压力位"] = []
            meta["止损位"] = None

    issues = list(dict.fromkeys(issues))
    if issues:
        print("WARNING: META schema 校验失败: " + "; ".join(issues), file=sys.stderr)
    return meta, not issues, issues


def validate_meta(parsed, expected_currency):
    """严格校验并规范化 META；返回 (normalized_meta, schema_ok)。"""
    meta, valid, _ = validate_meta_with_issues(parsed, expected_currency)
    return meta, valid


def parse_response_with_issues(model_text, expected_currency):
    """解析三段式响应，并返回全部可修复的协议问题。

    - META 段必须闭合、为合法 JSON 对象并通过严格 schema 校验。
    - protocol_valid 表示结构、META 和三个产物均有效。
    - 无效时保留可解析内容并返回 issues，供自动修复循环使用。
    """
    issues = _protocol_structure_issues(model_text)
    structure_valid = not issues
    if issues:
        print("WARNING: 三段式协议结构无效: " + "; ".join(issues), file=sys.stderr)
    meta_raw, meta_complete = _section(model_text, "META")
    meta = _empty_meta()
    meta_valid = False
    if "<<<META_BEGIN>>>" in model_text and not meta_complete:
        issues.append("META 段缺少 <<<META_END>>>")
        print("WARNING: META 段缺少 <<<META_END>>>，尝试使用已截取内容。", file=sys.stderr)
    if meta_raw:
        try:
            parsed = json.loads(meta_raw)
            if isinstance(parsed, dict):
                meta, meta_valid, meta_issues = validate_meta_with_issues(
                    parsed, expected_currency
                )
                issues.extend(f"META: {issue}" for issue in meta_issues)
            else:
                issues.append("META 段顶层不是 JSON 对象")
                print("WARNING: META 段顶层不是 JSON 对象，schema 校验失败。", file=sys.stderr)
        except json.JSONDecodeError as exc:
            issues.append(f"META 段 JSON 解析失败: {exc.msg}")
            print("WARNING: META 段 JSON 解析失败，schema 校验失败。", file=sys.stderr)
    elif "<<<META_BEGIN>>>" not in model_text:
        issues.append("未找到 <<<META_BEGIN>>> 段")
        print("WARNING: 未找到 <<<META_BEGIN>>> 段，三段式协议不完整。", file=sys.stderr)
    else:
        issues.append("META 段为空")
    webchat, webchat_complete = _section(model_text, "WEBSUMMARY")
    report_md, report_complete = _section(model_text, "REPORT")
    if not report_md:
        issues.append("REPORT 段为空或缺失")
    if not webchat:
        issues.append("WEBSUMMARY 段为空或缺失")
    if webchat and not webchat_complete:
        issues.append("WEBSUMMARY 段缺少 <<<WEBSUMMARY_END>>>")
    if report_md and not report_complete:
        issues.append("REPORT 段缺少 <<<REPORT_END>>>")
    meta_single_line = len(meta_raw.splitlines()) == 1
    if meta_raw and not meta_single_line:
        issues.append("META 段必须仅包含一行 JSON")
        print("WARNING: META 段必须仅包含一行 JSON。", file=sys.stderr)
    protocol_valid = (
        structure_valid
        and meta_complete
        and meta_single_line
        and meta_valid
        and bool(webchat)
        and bool(report_md)
        and webchat_complete
        and report_complete
    )
    return meta, webchat, report_md, protocol_valid, list(dict.fromkeys(issues))


def parse_response(model_text, expected_currency):
    """解析三段式响应，返回 (meta, webchat, report_md, protocol_valid)。"""
    meta, webchat, report_md, protocol_valid, _ = parse_response_with_issues(
        model_text, expected_currency
    )
    if not report_md:
        raise RuntimeError(
            "未能从模型响应中解析出 <<<REPORT_BEGIN>>> 段，无法生成报告。"
            " 模型可能未遵循三段式格式。"
        )
    return meta, webchat, report_md, protocol_valid


def canonical_response(meta, webchat, report_md):
    """把可解析内容重排为唯一、单行 META 的规范三段式响应。"""
    return "\n".join(
        [
            "<<<META_BEGIN>>>",
            json.dumps(meta, ensure_ascii=False, separators=(",", ":"), allow_nan=False),
            "<<<META_END>>>",
            "<<<WEBSUMMARY_BEGIN>>>",
            webchat.strip(),
            "<<<WEBSUMMARY_END>>>",
            "<<<REPORT_BEGIN>>>",
            report_md.strip(),
            "<<<REPORT_END>>>",
        ]
    )


def build_repair_prompt(original_prompt, issues):
    """要求模型从原任务重新生成完整协议；不设置固定返工次数。"""
    issue_text = "\n".join(f"- {issue}" for issue in issues)
    return f"""{original_prompt}

【程序结构审计返工】
上一版输出未通过确定性审计。请从原始证据重新生成完整结果，不要解释返工过程，
不要输出协议之外的文字，不要编造缺失事实。必须一次输出完整的 META、WEBSUMMARY、
REPORT 三段，所有六个标记唯一、独占一行且顺序正确；META 必须是一行合法 JSON。

当前全部审计问题：
{issue_text}
"""


def repair_progress(previous_issue_count, no_progress_rounds, issues):
    """Track real audit progress without imposing a fixed total repair limit."""
    issue_count = len(issues)
    if previous_issue_count is None or issue_count < previous_issue_count:
        no_progress_rounds = 0
    else:
        no_progress_rounds += 1
    return issue_count, no_progress_rounds, no_progress_rounds >= 2


def _finish_reason(raw_response):
    try:
        return str((raw_response.get("choices") or [{}])[0].get("finish_reason") or "")
    except (IndexError, KeyError, AttributeError, TypeError):
        return ""


def _replace_message_text(raw_response, model_text):
    raw_response["choices"][0]["message"]["content"] = model_text


def audit_model_response(raw_response, expected_currency):
    """Audit one model response and apply lossless/deterministic canonicalization."""
    finish = _finish_reason(raw_response)
    if finish not in {"", "stop", "length"}:
        raise RuntimeError(f"GLM 生成未正常完成（finish_reason={finish}）")
    model_text = extract_message_text(raw_response)
    meta, webchat, report_md, valid, issues = parse_response_with_issues(
        model_text, expected_currency
    )
    if finish == "length":
        issues.append("模型输出被长度上限截断（finish_reason=length）")
        valid = False

    # 只有六个标记都唯一且 META 语义本身有效时，才允许做标记重排和单行 JSON 压缩。
    # 重复内容标记无法无损确定应保留哪一段，必须交给模型返工。
    # validate_meta_with_issues 会用默认值替代坏字段；若在 META 无效时把该结果写回，
    # 会把有损降级伪装成“无损规范化”并错误准出。
    markers_unique = all(
        model_text.count(marker) == 1
        for marker in (
            "<<<META_BEGIN>>>",
            "<<<META_END>>>",
            "<<<WEBSUMMARY_BEGIN>>>",
            "<<<WEBSUMMARY_END>>>",
            "<<<REPORT_BEGIN>>>",
            "<<<REPORT_END>>>",
        )
    )
    meta_semantically_valid = (
        markers_unique
        and not any(
            issue.startswith("META:")
            or "META 段 JSON" in issue
            or "META 段顶层" in issue
            or "META 段为空" in issue
            for issue in issues
        )
    )
    if webchat and report_md and meta_semantically_valid:
        canonical = canonical_response(meta, webchat, report_md)
        (
            canonical_meta,
            canonical_webchat,
            canonical_report,
            canonical_valid,
            canonical_issues,
        ) = parse_response_with_issues(canonical, expected_currency)
        if finish == "length":
            canonical_issues.append("模型输出被长度上限截断（finish_reason=length）")
            canonical_valid = False
        if canonical_valid or len(canonical_issues) < len(issues):
            model_text = canonical
            meta = canonical_meta
            webchat = canonical_webchat
            report_md = canonical_report
            valid = canonical_valid
            issues = canonical_issues
            _replace_message_text(raw_response, canonical)

    return (
        meta,
        webchat,
        report_md,
        valid,
        list(dict.fromkeys(issues)),
        finish,
        model_text,
    )


def _fmt_num(value, fmt="{}"):
    """数值格式化：None/空返回空串，其余按 fmt（如 '{:.2f}'）。"""
    if value is None or value == "":
        return ""
    try:
        return fmt.format(value)
    except (TypeError, ValueError):
        return str(value)


def _fmt_pct(value, fmt):
    text = _fmt_num(value, fmt)
    return text if text.rstrip().endswith("%") else text + "%"


def _levels(value):
    """支撑/压力位：列表用 '/' 连接，单值直返，空返 ''。"""
    if isinstance(value, list):
        return "/".join(_fmt_num(x) for x in value if x not in (None, ""))
    return _fmt_num(value)


def format_chat_kpi(meta):
    """把 META 渲染成聊天的纯文本 KPI，字段缺失则跳过；末尾空行分隔。"""
    if not isinstance(meta, dict) or not meta:
        return ""

    conclusion = []
    view = meta.get("投资观点")
    if view:
        view_s = f"观点：{view}"
        conf = meta.get("观点置信度")
        if conf not in (None, ""):
            view_s += f"（置信 {_fmt_num(conf, '{:.2f}')}）"
        conclusion.append(f"📌 {view_s}")
    action = meta.get("行动建议")
    if action:
        conclusion.append(f"🧭 建议：{action}")
    risk = meta.get("风险等级")
    if risk:
        conclusion.append(f"⚠ 风险：{risk}")

    market = []
    as_of = meta.get("数据截至")
    if as_of:
        market.append(f"🕒 数据截至 {as_of}")

    currency = str(meta.get("币种") or "").strip()

    def price_text(value):
        prefix = f"{currency} " if currency else ""
        return prefix + _fmt_num(value)

    price = meta.get("现价")
    if price not in (None, ""):
        market.append(f"💹 现价 {price_text(price)}")

    target = meta.get("目标价")
    if target not in (None, ""):
        seg = f"🎯 目标价 {price_text(target)}"
        details = []
        upside = meta.get("上行空间_pct")
        if upside not in (None, ""):
            sign = "+" if (isinstance(upside, (int, float)) and upside > 0) else ""
            details.append(f"{sign}{_fmt_pct(upside, '{:.1f}')}")
        horizon = meta.get("目标期限月")
        if horizon not in (None, ""):
            details.append(f"{_fmt_num(horizon)}个月")
        method = meta.get("估值方法")
        if method:
            details.append(str(method))
        if details:
            seg += "（" + " · ".join(details) + "）"
        market.append(seg)

    vals = []
    for key, label, fmt in (("PE_ttm", "PE", "{}"), ("PB", "PB", "{}")):
        v = meta.get(key)
        if v not in (None, ""):
            vals.append(f"{label} {_fmt_num(v, fmt)}")
    divy = meta.get("股息率_pct")
    if divy not in (None, ""):
        vals.append(f"股息率 {_fmt_pct(divy, '{:.2f}')}")
    if vals:
        market.append("💰 " + " · ".join(vals))

    technical = []
    sup, res = _levels(meta.get("支撑位")), _levels(meta.get("压力位"))
    if sup:
        technical.append(f"支撑 {sup}")
    if res:
        technical.append(f"压力 {res}")
    stop = meta.get("止损位")
    if stop not in (None, ""):
        technical.append(f"止损 {_fmt_num(stop)}")

    lines = []
    if conclusion:
        lines.append(" | ".join(conclusion))
    if market:
        lines.append(" | ".join(market))
    if technical:
        lines.append("📊 " + " | ".join(technical))
    if not lines:
        return ""
    return "\n".join(lines) + "\n\n"


def write_outputs(
    output_dir,
    webchat,
    report_md,
    raw_response,
    protocol_valid,
    image_pairs,
    finish_reason="",
    meta=None,
):
    output_dir = Path(output_dir)
    report_path = output_dir / "report.md"
    result_path = output_dir / "glm5v_result.json"
    meta_path = output_dir / "meta.json"

    report_path.write_text(report_md.rstrip() + "\n", encoding="utf-8")
    meta_path.write_text(
        json.dumps(meta or {}, ensure_ascii=False, indent=2, default=str), encoding="utf-8"
    )

    result = {
        "webchat_summary": webchat,
        "report_md": str(report_path),
        "meta": meta or {},
        "meta_path": str(meta_path),
        "protocol_valid": protocol_valid,
        "finish_reason": finish_reason,
        "images_sent": [{"label": label, "path": str(path)} for label, path in image_pairs],
        "raw_response": raw_response,
    }
    result_path.write_text(
        json.dumps(result, ensure_ascii=False, indent=2, default=str), encoding="utf-8"
    )
    return report_path, result_path


def main():
    parser = argparse.ArgumentParser(description="用 GLM-5V-Turbo 生成股票分析报告")
    parser.add_argument("output_dir", help="fetch_all.py 生成的任务输出目录")
    parser.add_argument("--config", default=str(DEFAULT_CONFIG), help="GLM 配置文件")
    parser.add_argument("--user-request", default="", help="用户原始需求")
    parser.add_argument("--extra-context", default="", help="来源登记与补充证据文本")
    parser.add_argument("--extra-context-file", default="", help="来源登记与补充证据文件")
    parser.add_argument("--dry-run", action="store_true", help="只校验输入并打印请求摘要，不调用 API")
    args = parser.parse_args()

    output_dir = Path(args.output_dir).resolve()
    summary_path = output_dir / "summary.json"
    data_path = output_dir / "data.json"
    if not summary_path.exists():
        raise FileNotFoundError(f"找不到 summary.json: {summary_path}")

    cfg = load_config(args.config, require_key=not args.dry_run)
    summary = read_json(summary_path)
    data_json = read_json(data_path) if data_path.exists() else None
    image_pairs = collect_images(summary, output_dir, int(cfg.get("image_limit") or 8))
    extra_context = "\n\n".join(
        part
        for part in [args.extra_context, read_optional_text(args.extra_context_file)]
        if part
    )
    prompt = build_prompt(
        summary,
        data_json,
        image_pairs,
        args.user_request,
        extra_context,
        int(cfg.get("max_json_chars") or 60000),
    )

    if args.dry_run:
        dry = {
            "endpoint": chat_endpoint(cfg["base_url"]),
            "model": cfg["model"],
            "thinking": cfg.get("thinking"),
            "stream": cfg.get("stream"),
            "timeout_sec": cfg.get("timeout_sec"),
            "attempt_timeout_sec": cfg.get("attempt_timeout_sec"),
            "total_timeout_sec": cfg.get("total_timeout_sec"),
            "max_retries": cfg.get("max_retries"),
            "prompt_chars": len(prompt),
            "image_count": len(image_pairs),
            "images": [{"label": label, "path": str(path)} for label, path in image_pairs],
        }
        print(json.dumps(dry, ensure_ascii=False, indent=2))
        return 0

    session_deadline = time.monotonic() + int(cfg["total_timeout_sec"])
    raw = call_glm5v(cfg, prompt, image_pairs, session_deadline)
    repair_round = 0
    previous_issue_count = None
    no_progress_rounds = 0
    seen_states = set()
    while True:
        (
            meta,
            webchat,
            report_md,
            protocol_valid,
            audit_issues,
            finish,
            _,
        ) = audit_model_response(raw, summary.get("currency"))
        if protocol_valid:
            break

        state = (finish, tuple(audit_issues))
        (
            current_issue_count,
            no_progress_rounds,
            stalled,
        ) = repair_progress(
            previous_issue_count,
            no_progress_rounds,
            audit_issues,
        )
        cycle_detected = state in seen_states and previous_issue_count is not None
        if stalled or cycle_detected:
            raise RuntimeError(
                "GLM 三段式结构返工连续两轮没有减少审计问题或进入循环，"
                "已停止继续消耗请求；"
                "当前全部问题: " + "; ".join(audit_issues)
            )
        seen_states.add(state)
        previous_issue_count = current_issue_count
        repair_round += 1
        print(
            f"WARNING: GLM 输出未通过结构审计，开始第 {repair_round} 次返工；"
            "返工次数不设固定上限，仅在连续无进展或循环时停止。"
            f" 当前问题: {'; '.join(audit_issues)}",
            file=sys.stderr,
        )
        repair_prompt = build_repair_prompt(prompt, audit_issues)
        raw = call_glm5v(cfg, repair_prompt, image_pairs, session_deadline)

    kpi = format_chat_kpi(meta)
    if kpi:
        webchat = kpi + webchat
    report_path, result_path = write_outputs(
        output_dir,
        webchat,
        report_md,
        raw,
        protocol_valid,
        image_pairs,
        finish,
        meta=meta,
    )

    stdout_result = {
        "webchat_summary": webchat,
        "report_md": str(report_path),
        "meta": meta,
        "result_json": str(result_path),
        "protocol_valid": protocol_valid,
        "finish_reason": finish,
    }
    print("__GLM5V_RESULT_BEGIN__")
    print(json.dumps(stdout_result, ensure_ascii=False, indent=2))
    print("__GLM5V_RESULT_END__")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        raise SystemExit(1)
