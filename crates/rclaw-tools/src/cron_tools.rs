//! cron 工具：cron_add / cron_list / cron_remove。
//!
//! cron 工具。三者共享 [`CronController`]（与运行时调度同一份任务表）。

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use serde_json::Value;

use rclaw_store::cron_store::{lunar_clock_to_epoch, parse_daily_time, CronJob, CronKind};
use rclaw_store::StoreError;

use crate::cron_controller::CronController;
use crate::{Tool, ToolResult};

/// 取字符串字段。
fn s<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}
/// 取数字字段（秒/时间戳）。
fn n(input: &Value, key: &str) -> Option<i64> {
    input.get(key).and_then(|v| v.as_i64())
}

fn b(input: &Value, key: &str) -> Option<bool> {
    input.get(key).and_then(|v| v.as_bool())
}

fn gregorian_month_day_can_exist(month: u8, day: u8) -> bool {
    matches!(
        (month, day),
        (1 | 3 | 5 | 7 | 8 | 10 | 12, 1..=31) | (4 | 6 | 9 | 11, 1..=30) | (2, 1..=29)
    )
}

fn lunar_month_day_can_exist(month: u8, day: u8) -> bool {
    (1..=12).contains(&month) && (1..=30).contains(&day)
}

type RangeTail = (Option<u8>, Option<u8>, Option<u16>);

fn parse_month_day(input: &Value, label: &str, lunar: bool) -> Result<(u8, u8), String> {
    let month = match n(input, "month") {
        Some(x) if (1..=12).contains(&x) => x as u8,
        _ => return Err(format!("'{label}' 需要 month=1..=12")),
    };
    let day = match n(input, "day") {
        Some(x) if (1..=31).contains(&x) => x as u8,
        _ => return Err(format!("'{label}' 需要 day=1..=31")),
    };
    let ok = if lunar {
        lunar_month_day_can_exist(month, day)
    } else {
        gregorian_month_day_can_exist(month, day)
    };
    if !ok {
        let calendar = if lunar { "农历" } else { "阳历" };
        return Err(format!("'{label}' 的 {calendar}日期无效: {month}-{day}"));
    }
    Ok((month, day))
}

fn parse_range_tail(input: &Value, label: &str, lunar: bool) -> Result<RangeTail, String> {
    const MAX_YEARLY_RANGE_DAYS: i64 = 400;

    let duration_days = match n(input, "duration_days") {
        Some(x) if (1..=MAX_YEARLY_RANGE_DAYS).contains(&x) => Some(x as u16),
        Some(_) => {
            return Err(format!(
                "'{label}' 的 duration_days 需要在 1..={MAX_YEARLY_RANGE_DAYS} 内"
            ))
        }
        None => None,
    };
    let end_month_raw = n(input, "end_month");
    let end_day_raw = n(input, "end_day");
    let end_pair = match (end_month_raw, end_day_raw) {
        (Some(m), Some(d)) if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            let month = m as u8;
            let day = d as u8;
            let ok = if lunar {
                lunar_month_day_can_exist(month, day)
            } else {
                gregorian_month_day_can_exist(month, day)
            };
            if !ok {
                return Err(format!("'{label}' 的结束日期无效: {month}-{day}"));
            }
            Some((month, day))
        }
        (Some(_), Some(_)) => return Err(format!("'{label}' 的 end_month/end_day 无效")),
        (Some(_), None) | (None, Some(_)) => {
            return Err(format!("'{label}' 需要同时提供 end_month 和 end_day"))
        }
        (None, None) => None,
    };

    match (duration_days, end_pair) {
        (Some(_), Some(_)) => Err(format!(
            "'{label}' 的 duration_days 与 end_month/end_day 只能提供一种"
        )),
        (None, None) => Err(format!("'{label}' 需要 duration_days 或 end_month/end_day")),
        (duration_days, end_pair) => {
            Ok((end_pair.map(|x| x.0), end_pair.map(|x| x.1), duration_days))
        }
    }
}

struct ParsedSchedule {
    kind: CronKind,
    interval_s: Option<u32>,
    at_epoch: Option<i64>,
    at_clock: Option<String>,
    weekday: Option<u8>,
    day_of_month: Option<u8>,
    month: Option<u8>,
    day: Option<u8>,
    end_month: Option<u8>,
    end_day: Option<u8>,
    duration_days: Option<u16>,
    leap_month: Option<bool>,
    end_leap_month: Option<bool>,
    delete_after_run: bool,
}

impl ParsedSchedule {
    fn new(kind: CronKind) -> Self {
        Self {
            kind,
            interval_s: None,
            at_epoch: None,
            at_clock: None,
            weekday: None,
            day_of_month: None,
            month: None,
            day: None,
            end_month: None,
            end_day: None,
            duration_days: None,
            leap_month: None,
            end_leap_month: None,
            delete_after_run: false,
        }
    }
}

/// 把 `next_run` 渲染成「本地绝对时间 + 距今时长」，供日志与工具返回文案共用。
///
/// 修复 cron_add「不透明」问题：旧返回只给裸 epoch（如 `epoch=1817000000`），模型无法
/// 判断自己设的时间是否与用户预期一致——例如本想「2 分钟后」却传了更大的 delay，或用了
/// `at` + 瞎猜的 epoch 落到几小时以后，模型都看不出来、错了也无反馈。现在同时给出
/// 「2026-06-28 19:51:03（约 10 分钟后）」：模型可据此自查、必要时 cron_remove 重设；
/// 日志里留同款可读时间，便于排查「任务添加后迟迟不触发」类问题。
fn render_next_run(next_run: i64) -> String {
    let now = Utc::now().timestamp();
    let abs = match chrono::DateTime::from_timestamp(next_run, 0) {
        Some(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%Y-%m-%d %H:%M:%S")
            .to_string(),
        None => next_run.to_string(),
    };
    let delta = next_run - now;
    let rel = if delta <= 0 {
        "已到期".to_string()
    } else if delta < 60 {
        format!("{delta} 秒后")
    } else if delta < 3600 {
        format!("约 {} 分钟后", delta / 60)
    } else if delta < 86400 {
        format!("约 {} 小时 {} 分钟后", delta / 3600, (delta % 3600) / 60)
    } else {
        format!("约 {} 天后", delta / 86400)
    };
    format!("{abs}（{rel}）")
}

/// 把 `clock` 调度类型的「本地钟点 / 日期时间」字符串解析为 unix epoch。
///
/// 让 LLM 用自然钟点（`"22:00"`、`"2026-06-28 22:00"`）表达触发时间，由代码用 chrono
/// 算出 epoch——把最易错的「日历 + 本地时区 → epoch」换算从 LLM 手里拿走，交给确定性
/// 代码。这从根上消除「LLM 手算 epoch 差好几天」（如把今晚 22:00 算成 6 天后）类问题。
///
/// 接受两种写法（设备本地时区）：
/// - 完整日期时间：`%Y-%m-%d %H:%M[:%S]`，如 `2026-06-28 22:00`。须在未来，否则报错。
/// - 仅钟点：`%H:%M[:%S]`，如 `22:00` → 取「下一个」该钟点（今天已过则取明天）。
///
/// 解析失败、或本地时区下该时刻有歧义（夏令时切换）时返回 `Err`，由调用方转成工具错误。
fn parse_clock_to_epoch(input: &str) -> Result<i64, String> {
    let s = input.trim();
    let now_local = chrono::Local::now();
    let now_ts = now_local.timestamp();

    // 1) 完整日期时间：必须在未来（与 `at` 一致，过期则拒绝，避免设一个立刻过期的时间）。
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            let ts = local_ndt_to_epoch(&ndt)
                .ok_or_else(|| format!("时间 {s} 在本地时区有歧义（可能跨夏令时），请换写法"))?;
            if ts <= now_ts {
                return Err(format!("时间 {s} 已过期，请用未来时间"));
            }
            return Ok(ts);
        }
    }
    // 2) 仅钟点：取下一个该钟点（今天已过则明天同一时刻）。
    for fmt in ["%H:%M:%S", "%H:%M"] {
        if let Ok(nt) = chrono::NaiveTime::parse_from_str(s, fmt) {
            let mut cand = now_local.date_naive().and_time(nt);
            if cand <= now_local.naive_local() {
                cand += chrono::Duration::days(1);
            }
            return local_ndt_to_epoch(&cand)
                .ok_or_else(|| format!("时间 {s} 在本地时区有歧义，请用完整日期时间"));
        }
    }
    Err(format!(
        "无法解析时间 '{s}'：请用 '22:00' 或 '2026-06-28 22:00' 格式（本地时区）"
    ))
}

/// 本地 `NaiveDateTime` → unix epoch；该时刻在本地时区有歧义（夏令时）时返回 `None`。
fn local_ndt_to_epoch(ndt: &chrono::NaiveDateTime) -> Option<i64> {
    chrono::Local
        .from_local_datetime(ndt)
        .single()
        .map(|dt| dt.timestamp())
}

/// `interval_s` 允许的上限（约 1 年，365 天）。超过几乎一定是配置失误；
/// 上限同时配合 `u32::try_from` 防止超大 i64 被 `as u32` 截断成 0 / 错误调度。
const MAX_INTERVAL_S: u32 = 365 * 24 * 3600;
/// `delay_s`（`after` 一次性）允许的上限，与 interval 对齐。
const MAX_DELAY_S: i64 = (365 * 24 * 3600) as i64;

// ===========================================================================
// cron_add
// ===========================================================================

pub struct CronAdd {
    cron: Arc<CronController>,
}

impl CronAdd {
    pub fn new(cron: Arc<CronController>) -> Self {
        CronAdd { cron }
    }
}

#[async_trait]
impl Tool for CronAdd {
    fn name(&self) -> &str {
        "cron_add"
    }
    fn description(&self) -> &str {
        "Schedule a recurring or one-shot task. Use 'daily'/'weekly'/'monthly'/'lunar_monthly'/'yearly' for fixed local \
         calendar times (daily at_clock like '11:00'; weekly weekday=1..7 Monday..Sunday + at_clock; \
         monthly day_of_month=1..31 + at_clock; lunar_monthly day_of_month=1..30 + at_clock; \
         yearly month/day + at_clock). Use lunar schedule types only when the user explicitly says lunar. \
         Use 'yearly_range'/'lunar_yearly_range' for annual daily windows. Prefer 'clock' for a one-shot specific time of day \
         (at_clock like '22:00' or '2026-06-28 22:00', local timezone, no epoch math; \
         use full date-time when the user says today/tomorrow/a specific date). Use 'lunar_clock' for \
         one-shot explicit lunar dates (year/month/day + at_clock). \
         'after'=one-shot relative delay (delay_s seconds). 'every'=fixed-second interval from creation time. \
         'at'=raw unix ts (avoid, requires hand-computing epoch which is error-prone)."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{
                "name":{"type":"string"},
                "schedule_type":{"type":"string","description":"'daily' | 'weekly' | 'monthly' | 'lunar_monthly' | 'yearly' | 'lunar_yearly' | 'yearly_range' | 'lunar_yearly_range' | 'clock' | 'lunar_clock' | 'after' | 'at' | 'every'"},
                "at_clock":{"type":"string","description":"日历重复类型必填本地时区钟点，如 '11:00'；'clock' 必填本地时区钟点/日期时间，如 '22:00' 或 '2026-06-28 22:00'；用户说今天/明天/某日时用完整日期时间"},
                "weekday":{"type":"integer","description":"'weekly' 必填，1=周一 ... 7=周日"},
                "day_of_month":{"type":"integer","description":"'monthly' 必填阳历日 1..=31；'lunar_monthly' 必填农历日 1..=30"},
                "year":{"type":"integer","description":"'lunar_clock' 必填农历年；用户说今年时用当前年份"},
                "month":{"type":"integer","description":"'yearly'/'lunar_yearly'/'lunar_clock' 的月；'*_range' 的开始月。未显式说农历时按阳历"},
                "day":{"type":"integer","description":"'yearly'/'lunar_yearly'/'lunar_clock' 的日；'*_range' 的开始日"},
                "end_month":{"type":"integer","description":"年度区间结束月；与 duration_days 二选一"},
                "end_day":{"type":"integer","description":"年度区间结束日；与 duration_days 二选一"},
                "duration_days":{"type":"integer","description":"年度区间持续天数；与 end_month/end_day 二选一，最大 400"},
                "leap_month":{"type":"boolean","description":"农历开始日期是否闰月；用户说“闰五月”才设 true，默认 false"},
                "end_leap_month":{"type":"boolean","description":"农历区间结束日期是否闰月；用户明确说闰月才设 true，默认 false"},
                "delay_s":{"type":"integer","description":"秒，'after' 必填"},
                "interval_s":{"type":"integer","description":"秒，'every' 必填"},
                "at_epoch":{"type":"integer","description":"unix 时间戳，'at' 必填（不推荐，需手算 epoch 易错，优先用 clock）"},
                "message":{"type":"string","description":"触发时给用户的提醒内容"},
                "channel":{"type":"string"},
                "chat_id":{"type":"string"}
            },
            "required":["name","schedule_type","message"]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let name = match s(&input, "name") {
            Some(x) => x,
            None => return ToolResult::err("缺少必填字段 name"),
        };
        let schedule_type = match s(&input, "schedule_type") {
            Some(x) => x,
            None => return ToolResult::err("缺少必填字段 schedule_type"),
        };
        let message = match s(&input, "message") {
            Some(x) if !x.is_empty() => x,
            _ => return ToolResult::err("缺少必填字段 message（且不能为空）"),
        };

        let require_calendar_clock = |label: &str, example: &str| -> Result<String, String> {
            let clock = s(&input, "at_clock")
                .map(str::trim)
                .filter(|x| !x.is_empty())
                .ok_or_else(|| {
                    format!(
                        "'{label}' 需要 at_clock（如 '{example}'，本地时区）；用户未说明几点时请先追问"
                    )
                })?;
            if parse_daily_time(clock).is_none() {
                return Err(format!(
                    "{label} at_clock 无法解析，请用 '{example}' 或 '{example}:00' 格式"
                ));
            }
            Ok(clock.to_string())
        };

        let parsed = match schedule_type {
            "every" => {
                if s(&input, "at_clock").is_some_and(|x| !x.trim().is_empty()) {
                    return ToolResult::err(
                        "'every' 是从创建时刻起算的固定秒间隔，不能用 at_clock 对齐到每天固定钟点；每天固定钟点请用 schedule_type='daily' + at_clock",
                    );
                }
                let iv = match n(&input, "interval_s") {
                    Some(x) if x > 0 => match u32::try_from(x) {
                        Ok(v) if v <= MAX_INTERVAL_S => v,
                        _ => {
                            return ToolResult::err(format!(
                                "interval_s 超出允许范围 (1..={MAX_INTERVAL_S})，已拒绝以避免截断/错误调度"
                            ))
                        }
                    },
                    _ => return ToolResult::err("'every' 需要正数 interval_s"),
                };
                let mut p = ParsedSchedule::new(CronKind::Every);
                p.interval_s = Some(iv);
                p
            }
            "daily" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'daily' 已表示每天固定钟点，不需要 interval_s；请只提供 at_clock",
                    );
                }
                let mut p = ParsedSchedule::new(CronKind::Daily);
                p.at_clock = match require_calendar_clock("daily", "11:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "weekly" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'weekly' 已表示每周固定星期/钟点，不需要 interval_s；请提供 weekday 和 at_clock",
                    );
                }
                let weekday = match n(&input, "weekday") {
                    Some(x) if (1..=7).contains(&x) => x as u8,
                    _ => {
                        return ToolResult::err("'weekly' 需要 weekday=1..=7（1=周一 ... 7=周日）")
                    }
                };
                let mut p = ParsedSchedule::new(CronKind::Weekly);
                p.weekday = Some(weekday);
                p.at_clock = match require_calendar_clock("weekly", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "monthly" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'monthly' 已表示每月固定日期/钟点，不需要 interval_s；请提供 day_of_month 和 at_clock",
                    );
                }
                let day = match n(&input, "day_of_month") {
                    Some(x) if (1..=31).contains(&x) => x as u8,
                    _ => return ToolResult::err("'monthly' 需要 day_of_month=1..=31"),
                };
                let mut p = ParsedSchedule::new(CronKind::Monthly);
                p.day_of_month = Some(day);
                p.at_clock = match require_calendar_clock("monthly", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "lunar_monthly" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'lunar_monthly' 已表示每个农历月固定日期/钟点，不需要 interval_s；请提供 day_of_month 和 at_clock",
                    );
                }
                let day = match n(&input, "day_of_month") {
                    Some(x) if (1..=30).contains(&x) => x as u8,
                    _ => return ToolResult::err("'lunar_monthly' 需要 day_of_month=1..=30"),
                };
                let mut p = ParsedSchedule::new(CronKind::LunarMonthly);
                p.day_of_month = Some(day);
                p.at_clock = match require_calendar_clock("lunar_monthly", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "yearly" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'yearly' 已表示每年固定阳历日期/钟点，不需要 interval_s；请提供 month/day 和 at_clock",
                    );
                }
                let (month, day) = match parse_month_day(&input, "yearly", false) {
                    Ok(x) => x,
                    Err(e) => return ToolResult::err(e),
                };
                let mut p = ParsedSchedule::new(CronKind::Yearly);
                p.month = Some(month);
                p.day = Some(day);
                p.at_clock = match require_calendar_clock("yearly", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "lunar_yearly" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'lunar_yearly' 已表示每年固定农历日期/钟点，不需要 interval_s；请提供 month/day 和 at_clock",
                    );
                }
                let (month, day) = match parse_month_day(&input, "lunar_yearly", true) {
                    Ok(x) => x,
                    Err(e) => return ToolResult::err(e),
                };
                let mut p = ParsedSchedule::new(CronKind::LunarYearly);
                p.month = Some(month);
                p.day = Some(day);
                p.leap_month = Some(b(&input, "leap_month").unwrap_or(false));
                p.at_clock = match require_calendar_clock("lunar_yearly", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "yearly_range" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'yearly_range' 已表示每年日期区间内每天固定钟点，不需要 interval_s；请提供 month/day、duration_days 或 end_month/end_day，以及 at_clock",
                    );
                }
                let (month, day) = match parse_month_day(&input, "yearly_range", false) {
                    Ok(x) => x,
                    Err(e) => return ToolResult::err(e),
                };
                let (end_month, end_day, duration_days) =
                    match parse_range_tail(&input, "yearly_range", false) {
                        Ok(x) => x,
                        Err(e) => return ToolResult::err(e),
                    };
                let mut p = ParsedSchedule::new(CronKind::YearlyRange);
                p.month = Some(month);
                p.day = Some(day);
                p.end_month = end_month;
                p.end_day = end_day;
                p.duration_days = duration_days;
                p.at_clock = match require_calendar_clock("yearly_range", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "lunar_yearly_range" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'lunar_yearly_range' 已表示每年农历日期区间内每天固定钟点，不需要 interval_s；请提供 month/day、duration_days 或 end_month/end_day，以及 at_clock",
                    );
                }
                let (month, day) = match parse_month_day(&input, "lunar_yearly_range", true) {
                    Ok(x) => x,
                    Err(e) => return ToolResult::err(e),
                };
                let (end_month, end_day, duration_days) =
                    match parse_range_tail(&input, "lunar_yearly_range", true) {
                        Ok(x) => x,
                        Err(e) => return ToolResult::err(e),
                    };
                let mut p = ParsedSchedule::new(CronKind::LunarYearlyRange);
                p.month = Some(month);
                p.day = Some(day);
                p.end_month = end_month;
                p.end_day = end_day;
                p.duration_days = duration_days;
                p.leap_month = Some(b(&input, "leap_month").unwrap_or(false));
                p.end_leap_month = Some(b(&input, "end_leap_month").unwrap_or(false));
                p.at_clock = match require_calendar_clock("lunar_yearly_range", "09:00") {
                    Ok(x) => Some(x),
                    Err(e) => return ToolResult::err(e),
                };
                p
            }
            "lunar_clock" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'lunar_clock' 是一次性农历日期任务，不能配合 interval_s 重复；每年农历日期请用 schedule_type='lunar_yearly'",
                    );
                }
                let year = match n(&input, "year") {
                    Some(x) if (1..=9999).contains(&x) => x as i32,
                    _ => return ToolResult::err("'lunar_clock' 需要 year=1..=9999"),
                };
                let (month, day) = match parse_month_day(&input, "lunar_clock", true) {
                    Ok(x) => x,
                    Err(e) => return ToolResult::err(e),
                };
                let clock = match require_calendar_clock("lunar_clock", "09:00") {
                    Ok(x) => x,
                    Err(e) => return ToolResult::err(e),
                };
                let at = match lunar_clock_to_epoch(
                    year,
                    month,
                    day,
                    b(&input, "leap_month").unwrap_or(false),
                    &clock,
                ) {
                    Ok(ts) => ts,
                    Err(e) => return ToolResult::err(e),
                };
                if at <= Utc::now().timestamp() {
                    return ToolResult::err("农历一次性日期时间已过期");
                }
                let mut p = ParsedSchedule::new(CronKind::At);
                p.at_epoch = Some(at);
                p.delete_after_run = input
                    .get("delete_after_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                p
            }
            "at" => {
                let at = match n(&input, "at_epoch") {
                    Some(x) => x,
                    None => return ToolResult::err("'at' 需要 at_epoch"),
                };
                if at <= Utc::now().timestamp() {
                    return ToolResult::err("at_epoch 已过期");
                }
                let mut p = ParsedSchedule::new(CronKind::At);
                p.at_epoch = Some(at);
                p.delete_after_run = input
                    .get("delete_after_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                p
            }
            "after" => {
                let delay = match n(&input, "delay_s") {
                    Some(x) if x > 0 && x <= MAX_DELAY_S => x,
                    _ => {
                        return ToolResult::err(format!(
                            "'after' 需要 1..={MAX_DELAY_S} 范围内的正数 delay_s"
                        ))
                    }
                };
                let now = Utc::now().timestamp();
                let at = match now.checked_add(delay) {
                    Some(a) => a,
                    None => return ToolResult::err("delay_s 计算触发时间时溢出"),
                };
                let mut p = ParsedSchedule::new(CronKind::At);
                p.at_epoch = Some(at);
                p.delete_after_run = input
                    .get("delete_after_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                p
            }
            "clock" => {
                if n(&input, "interval_s").is_some() {
                    return ToolResult::err(
                        "'clock' 是一次性钟点任务，不能配合 interval_s 重复；每天固定钟点请用 schedule_type='daily' + at_clock",
                    );
                }
                let clock =
                    match s(&input, "at_clock") {
                        Some(x) if !x.is_empty() => x,
                        _ => return ToolResult::err(
                            "'clock' 需要 at_clock（如 '22:00' 或 '2026-06-28 22:00'，本地时区）",
                        ),
                    };
                let at = match parse_clock_to_epoch(clock) {
                    Ok(ts) => ts,
                    Err(e) => return ToolResult::err(e),
                };
                let mut p = ParsedSchedule::new(CronKind::At);
                p.at_epoch = Some(at);
                p.delete_after_run = input
                    .get("delete_after_run")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                p
            }
            other => return ToolResult::err(format!("未知 schedule_type: {other}")),
        };

        let job = CronJob {
            id: String::new(), // add() 内部生成
            name: name.to_string(),
            enabled: true,
            kind: parsed.kind,
            interval_s: parsed.interval_s,
            at_epoch: parsed.at_epoch,
            at_clock: parsed.at_clock,
            weekday: parsed.weekday,
            day_of_month: parsed.day_of_month,
            month: parsed.month,
            day: parsed.day,
            end_month: parsed.end_month,
            end_day: parsed.end_day,
            duration_days: parsed.duration_days,
            leap_month: parsed.leap_month,
            end_leap_month: parsed.end_leap_month,
            message: message.to_string(),
            channel: s(&input, "channel").unwrap_or("").to_string(),
            chat_id: s(&input, "chat_id").unwrap_or("").to_string(),
            last_run: 0,
            next_run: 0,
            delete_after_run: parsed.delete_after_run,
        };

        match self.cron.add(job) {
            Ok(id) => {
                // 读回真实 next_run 做提示与日志。
                let snap = self.cron.list_snapshot();
                let saved = snap.iter().find(|j| j.id == id);
                // 诊断日志：记录入参与最终 next_run（含人类可读时间）。
                // 曾出现「任务添加成功却长期不触发」：模型传了过大的 delay、或用 at + 瞎猜的
                // epoch，next_run 落到很久以后，任务一直 enabled 却永不到期。旧实现只记工具
                // 名、不记入参，事后无从定位。现在把 schedule 入参 + next_run + 「本地时间(距今)」
                // 一并留证，下次复现即可一眼定位是参数错误还是调度问题。
                if let Some(j) = saved {
                    tracing::info!(
                        "cron_add 已落盘: id={id} name={name} kind={kind:?} delay_s={delay} \
                         at_epoch={at} at_clock={clock} weekday={weekday} day_of_month={dom} \
                         month={month} day={day} end_month={end_month} end_day={end_day} \
                         duration_days={duration_days} leap_month={leap_month} end_leap_month={end_leap_month} \
                         interval_s={iv} delete_after_run={del} \
                         next_run={nr} {when}",
                        kind = j.kind,
                        delay = n(&input, "delay_s").unwrap_or(0),
                        at = j.at_epoch.unwrap_or(0),
                        clock = j
                            .at_clock
                            .as_deref()
                            .unwrap_or_else(|| s(&input, "at_clock").unwrap_or("")),
                        iv = j.interval_s.unwrap_or(0),
                        weekday = j.weekday.unwrap_or(0),
                        dom = j.day_of_month.unwrap_or(0),
                        month = j.month.unwrap_or(0),
                        day = j.day.unwrap_or(0),
                        end_month = j.end_month.unwrap_or(0),
                        end_day = j.end_day.unwrap_or(0),
                        duration_days = j.duration_days.unwrap_or(0),
                        leap_month = j.leap_month.unwrap_or(false),
                        end_leap_month = j.end_leap_month.unwrap_or(false),
                        del = j.delete_after_run,
                        nr = j.next_run,
                        when = render_next_run(j.next_run),
                    );
                }
                let hint = match saved {
                    Some(j) if j.kind == CronKind::Every => {
                        format!(
                            "已添加周期任务 '{name}' (id={id})，每 {}s 触发，首次触发 {}。",
                            j.interval_s.unwrap_or(0),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::Daily => {
                        format!(
                            "已添加每日任务 '{name}' (id={id})，每天 {} 触发，首次触发 {}。",
                            j.at_clock.as_deref().unwrap_or(""),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::Weekly => {
                        format!(
                            "已添加每周任务 '{name}' (id={id})，每周 {} {} 触发，首次触发 {}。",
                            j.weekday.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::Monthly => {
                        format!(
                            "已添加每月任务 '{name}' (id={id})，每月 {} 号 {} 触发，首次触发 {}。",
                            j.day_of_month.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::LunarMonthly => {
                        format!(
                            "已添加农历每月任务 '{name}' (id={id})，每个农历月 {} 日 {} 触发，首次触发 {}。",
                            j.day_of_month.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::Yearly => {
                        format!(
                            "已添加每年任务 '{name}' (id={id})，每年阳历 {}-{} {} 触发，首次触发 {}。",
                            j.month.unwrap_or(0),
                            j.day.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::LunarYearly => {
                        format!(
                            "已添加每年农历任务 '{name}' (id={id})，每年农历 {}{}-{} {} 触发，首次触发 {}。",
                            if j.leap_month.unwrap_or(false) { "闰" } else { "" },
                            j.month.unwrap_or(0),
                            j.day.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::YearlyRange => {
                        let tail = match (j.duration_days, j.end_month, j.end_day) {
                            (Some(days), _, _) => format!("持续 {days} 天"),
                            (_, Some(m), Some(d)) => format!("直到阳历 {m}-{d}"),
                            _ => "区间结束未设置".to_string(),
                        };
                        format!(
                            "已添加每年区间任务 '{name}' (id={id})，每年从阳历 {}-{} 开始每天 {} 触发，{}，首次触发 {}。",
                            j.month.unwrap_or(0),
                            j.day.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            tail,
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) if j.kind == CronKind::LunarYearlyRange => {
                        let tail = match (j.duration_days, j.end_month, j.end_day) {
                            (Some(days), _, _) => format!("持续 {days} 天"),
                            (_, Some(m), Some(d)) => format!(
                                "直到农历 {}{m}-{d}",
                                if j.end_leap_month.unwrap_or(false) {
                                    "闰"
                                } else {
                                    ""
                                }
                            ),
                            _ => "区间结束未设置".to_string(),
                        };
                        format!(
                            "已添加每年农历区间任务 '{name}' (id={id})，每年从农历 {}{}-{} 开始每天 {} 触发，{}，首次触发 {}。",
                            if j.leap_month.unwrap_or(false) { "闰" } else { "" },
                            j.month.unwrap_or(0),
                            j.day.unwrap_or(0),
                            j.at_clock.as_deref().unwrap_or(""),
                            tail,
                            render_next_run(j.next_run)
                        )
                    }
                    Some(j) => {
                        format!(
                            "已添加一次性任务 '{name}' (id={id})，触发时间 {}。{}",
                            render_next_run(j.next_run),
                            if j.delete_after_run {
                                " 触发后自动删除。"
                            } else {
                                ""
                            }
                        )
                    }
                    None => format!("已添加任务 '{name}' (id={id})。"),
                };
                ToolResult::ok(hint)
            }
            Err(StoreError::Other(e)) => ToolResult::err(format!("添加失败: {e}")),
            Err(e) => ToolResult::err(format!("添加失败: {e}")),
        }
    }
}

// ===========================================================================
// cron_list
// ===========================================================================

pub struct CronList {
    cron: Arc<CronController>,
}

impl CronList {
    pub fn new(cron: Arc<CronController>) -> Self {
        CronList { cron }
    }
}

#[async_trait]
impl Tool for CronList {
    fn name(&self) -> &str {
        "cron_list"
    }
    fn description(&self) -> &str {
        "List all scheduled cron jobs."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({"type":"object","properties":{},"required":[]})
    }
    async fn execute(&self, _input: Value) -> ToolResult {
        let jobs = self.cron.list_snapshot();
        if jobs.is_empty() {
            return ToolResult::ok("当前没有定时任务。");
        }
        let mut buf = format!("定时任务 ({}):\n", jobs.len());
        for (i, j) in jobs.iter().enumerate() {
            match j.kind {
                CronKind::Every => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" — 每 {}s，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.interval_s.unwrap_or(0),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::Daily => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" — 每天 {}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.at_clock.as_deref().unwrap_or(""),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::Weekly => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" — 每周 {} {}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.weekday.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::Monthly => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - 每月 {} 号 {}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.day_of_month.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::LunarMonthly => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - 每个农历月 {} 日 {}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.day_of_month.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::Yearly => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - 每年阳历 {}-{} {}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.month.unwrap_or(0),
                        j.day.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::LunarYearly => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - 每年农历 {}{}-{} {}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        if j.leap_month.unwrap_or(false) {
                            "闰"
                        } else {
                            ""
                        },
                        j.month.unwrap_or(0),
                        j.day.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::YearlyRange => {
                    let tail = match (j.duration_days, j.end_month, j.end_day) {
                        (Some(days), _, _) => format!("持续 {days} 天"),
                        (_, Some(m), Some(d)) => format!("直到阳历 {m}-{d}"),
                        _ => "区间结束未设置".to_string(),
                    };
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - 每年阳历 {}-{} 起每天 {}，{}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.month.unwrap_or(0),
                        j.day.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        tail,
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::LunarYearlyRange => {
                    let tail = match (j.duration_days, j.end_month, j.end_day) {
                        (Some(days), _, _) => format!("持续 {days} 天"),
                        (_, Some(m), Some(d)) => format!(
                            "直到农历 {}{m}-{d}",
                            if j.end_leap_month.unwrap_or(false) {
                                "闰"
                            } else {
                                ""
                            }
                        ),
                        _ => "区间结束未设置".to_string(),
                    };
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - 每年农历 {}{}-{} 起每天 {}，{}，{}，next={}，ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        if j.leap_month.unwrap_or(false) { "闰" } else { "" },
                        j.month.unwrap_or(0),
                        j.day.unwrap_or(0),
                        j.at_clock.as_deref().unwrap_or(""),
                        tail,
                        if j.enabled { "启用" } else { "禁用" },
                        j.next_run,
                        j.channel,
                        j.chat_id
                    ));
                }
                CronKind::At => {
                    buf.push_str(&format!(
                        "  {}. [{}] \"{}\" - at {}，{}，{}ch={}:{}\n",
                        i + 1,
                        j.id,
                        j.name,
                        j.at_epoch.unwrap_or(0),
                        if j.enabled { "启用" } else { "禁用" },
                        if j.delete_after_run {
                            "(auto-delete) "
                        } else {
                            ""
                        },
                        j.channel,
                        j.chat_id
                    ));
                }
            }
        }
        ToolResult::ok(buf)
    }
}

// ===========================================================================
// cron_remove
// ===========================================================================

pub struct CronRemove {
    cron: Arc<CronController>,
}

impl CronRemove {
    pub fn new(cron: Arc<CronController>) -> Self {
        CronRemove { cron }
    }
}

#[async_trait]
impl Tool for CronRemove {
    fn name(&self) -> &str {
        "cron_remove"
    }
    fn description(&self) -> &str {
        "Remove a scheduled cron job by its id."
    }
    fn input_schema(&self) -> Value {
        serde_json::json!({
            "type":"object",
            "properties":{"job_id":{"type":"string","description":"8-char job id"}},
            "required":["job_id"]
        })
    }
    async fn execute(&self, input: Value) -> ToolResult {
        let id = match s(&input, "job_id") {
            Some(x) if !x.is_empty() => x,
            _ => return ToolResult::err("缺少 'job_id' 字段"),
        };
        match self.cron.remove(id) {
            Ok(true) => ToolResult::ok(format!("OK: 已删除任务 {id}")),
            Ok(false) => ToolResult::err(format!("未找到任务 '{id}'")),
            // 命中但持久化失败：内存与磁盘保持一致（均未删），向用户报错而非静默成功。
            Err(e) => ToolResult::err(format!("删除任务 '{id}' 持久化失败: {e}")),
        }
    }
}

#[cfg(test)]
mod tests;
