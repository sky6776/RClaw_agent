//! cron 任务的数据结构与持久化。
//!
//! cron 任务的持久化（`cron_job_t` 与 load/save）。
//! 调度逻辑（周期扫描、到期触发）放在 runtime 层；本模块只负责
//! 「数据形状 + 读写 `agent/cron.json`」，便于工具层与调度层共享同一类型。

use std::collections::HashSet;
use std::path::Path;

use chrono::{Datelike, TimeZone};
use lunar_lite::{lunar_to_solar, solar_to_lunar, validate_lunar_date, LunarDate, SolarDate};
use rand::Rng;
use serde::{Deserialize, Serialize};

use crate::fs::{atomic_write, Layout};
use crate::{ensure_file, Result, StoreError};

/// 调度类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CronKind {
    /// 周期性：按固定秒数间隔重复。
    Every,
    /// 一次性：在指定 unix 时间戳触发。
    At,
    /// 日历重复：每天在本地时区的指定钟点触发。
    Daily,
    /// 日历重复：每周在本地时区的指定星期与钟点触发。
    Weekly,
    /// 日历重复：每月在本地时区的指定日期与钟点触发。
    Monthly,
    /// 日历重复：每个农历月在本地时区的指定农历日与钟点触发。
    LunarMonthly,
    /// 日历重复：每年在本地时区的指定阳历月日与钟点触发。
    Yearly,
    /// 日历重复：每年在本地时区的指定农历月日与钟点触发。
    LunarYearly,
    /// 日历重复：每年阳历日期区间内每天在指定钟点触发。
    YearlyRange,
    /// 日历重复：每年农历日期区间内每天在指定钟点触发。
    LunarYearlyRange,
}

/// 单个 cron 任务。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    /// 8 位十六进制 ID。
    pub id: String,
    /// 简短名称。
    pub name: String,
    /// 是否启用。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 调度类型。
    pub kind: CronKind,
    /// `Every` 的间隔秒数。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub interval_s: Option<u32>,
    /// `At` 的触发时间戳（unix 秒）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub at_epoch: Option<i64>,
    /// `Daily` 的本地钟点（`HH:MM[:SS]`）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub at_clock: Option<String>,
    /// `Weekly` 的星期，1=周一，7=周日。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub weekday: Option<u8>,
    /// `Monthly` 的阳历日（1..=31）或 `LunarMonthly` 的农历日（1..=30）。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub day_of_month: Option<u8>,
    /// `Yearly`/`LunarYearly` 的月，或区间任务的开始月。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub month: Option<u8>,
    /// `Yearly`/`LunarYearly` 的日，或区间任务的开始日。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub day: Option<u8>,
    /// `YearlyRange`/`LunarYearlyRange` 的结束月；与 `duration_days` 二选一。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_month: Option<u8>,
    /// `YearlyRange`/`LunarYearlyRange` 的结束日；与 `duration_days` 二选一。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_day: Option<u8>,
    /// 年度区间的持续天数；与 `end_month`/`end_day` 二选一。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_days: Option<u16>,
    /// 农历任务的开始日期是否为闰月；未提供时按非闰月。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub leap_month: Option<bool>,
    /// 农历区间结束日期是否为闰月；未提供时按非闰月。
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub end_leap_month: Option<bool>,
    /// 触发时注入 inbound 的消息正文。
    pub message: String,
    /// 回复通道（默认 system）。
    #[serde(default)]
    pub channel: String,
    /// 回复 chat_id（默认 cron）。
    #[serde(default)]
    pub chat_id: String,
    /// 上次触发时间戳。
    #[serde(default)]
    pub last_run: i64,
    /// 下次触发时间戳。
    #[serde(default)]
    pub next_run: i64,
    /// `At` 任务触发后是否自动删除。
    #[serde(default)]
    pub delete_after_run: bool,
}

fn default_true() -> bool {
    true
}

const MAX_YEARLY_RANGE_DAYS: u16 = 400;
const MAX_YEARLY_LOOKAHEAD_DAYS: i64 = 366 * 25;
const MAX_LUNAR_MONTHLY_LOOKAHEAD_DAYS: i64 = 400;

impl CronJob {
    /// 生成一个新的 8 位十六进制 ID。
    pub fn generate_id() -> String {
        let r: u32 = rand::thread_rng().gen();
        format!("{r:08x}")
    }

    /// 校验任务语义是否「可调度」（修复 P3「加载后未校验任务语义」）。
    ///
    /// 反序列化能接受【语义非法】的任务（例如 `kind=every` 缺失或为 0 的 `interval_s`），
    /// 它们会通过类型层、却在运行时出问题：`process_due_jobs` 推进 `next_run` 时用
    /// `interval_s.unwrap_or(0)`，缺省/为 0 → `next_run = now` → 每个 tick 都到期、
    /// 疯狂触发。本方法在 [`load_jobs`] 里被逐条调用，把不通过的条目就地【禁用】，
    /// 而非当成可调度任务接受。
    ///
    /// 判定规则：
    /// - `Every`：`interval_s` 必须存在且 `> 0`；
    /// - `At`：`at_epoch` 必须存在且 `> 0`。
    ///   旧实现只检查 at_epoch 存在——at_epoch 为 0 或负数的任务会被视为可调度，
    ///   随后补齐 next_run 会写成同样的非正数；调度器要求 next_run > 0，导致
    ///   任务保持 enabled 但永远不触发。这里要求 at_epoch 为正，非法值在加载时
    ///   由 `validate_and_repair_jobs` 禁用并持久化。
    pub fn is_schedulable(&self) -> bool {
        match self.kind {
            CronKind::Every => self.interval_s.map(|s| s > 0).unwrap_or(false),
            CronKind::At => self.at_epoch.map(|e| e > 0).unwrap_or(false),
            CronKind::Daily => self
                .at_clock
                .as_deref()
                .and_then(parse_daily_time)
                .is_some(),
            CronKind::Weekly => {
                self.weekday.is_some_and(|d| (1..=7).contains(&d))
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
            CronKind::Monthly => {
                self.day_of_month.is_some_and(|d| (1..=31).contains(&d))
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
            CronKind::LunarMonthly => {
                self.day_of_month.is_some_and(|d| (1..=30).contains(&d))
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
            CronKind::Yearly => {
                self.month
                    .zip(self.day)
                    .is_some_and(|(m, d)| gregorian_month_day_can_exist(m, d))
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
            CronKind::LunarYearly => {
                self.month
                    .zip(self.day)
                    .is_some_and(|(m, d)| lunar_month_day_can_exist(m, d))
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
            CronKind::YearlyRange => {
                self.month
                    .zip(self.day)
                    .is_some_and(|(m, d)| gregorian_month_day_can_exist(m, d))
                    && yearly_range_has_end(self, false)
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
            CronKind::LunarYearlyRange => {
                self.month
                    .zip(self.day)
                    .is_some_and(|(m, d)| lunar_month_day_can_exist(m, d))
                    && yearly_range_has_end(self, true)
                    && self
                        .at_clock
                        .as_deref()
                        .and_then(parse_daily_time)
                        .is_some()
            }
        }
    }
}

/// 解析每日固定钟点（本地时区）。接受 `HH:MM` 或 `HH:MM:SS`。
pub fn parse_daily_time(input: &str) -> Option<chrono::NaiveTime> {
    let s = input.trim();
    for fmt in ["%H:%M:%S", "%H:%M"] {
        if let Ok(t) = chrono::NaiveTime::parse_from_str(s, fmt) {
            return Some(t);
        }
    }
    None
}

/// 计算 `after_ts` 之后的下一次本地每日钟点触发时间。
pub fn next_daily_epoch_after(at_clock: &str, after_ts: i64) -> std::result::Result<i64, String> {
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析每日钟点 '{at_clock}'，请用 '11:00' 格式"))?;
    let after_utc = chrono::DateTime::from_timestamp(after_ts, 0)
        .ok_or_else(|| format!("非法时间戳: {after_ts}"))?;
    let after_local = after_utc.with_timezone(&chrono::Local);
    let mut cand = after_local.date_naive().and_time(nt);
    if cand <= after_local.naive_local() {
        cand += chrono::Duration::days(1);
    }
    chrono::Local
        .from_local_datetime(&cand)
        .single()
        .map(|dt| dt.timestamp())
        .ok_or_else(|| format!("时间 {at_clock} 在本地时区有歧义，请换写法"))
}

/// 计算 `after_ts` 之后的下一次本地每周固定星期/钟点触发时间。
pub fn next_weekly_epoch_after(
    weekday: u8,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    if !(1..=7).contains(&weekday) {
        return Err(format!("weekday 必须在 1..=7，当前为 {weekday}"));
    }
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析每周钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_utc = chrono::DateTime::from_timestamp(after_ts, 0)
        .ok_or_else(|| format!("非法时间戳: {after_ts}"))?;
    let after_local = after_utc.with_timezone(&chrono::Local);
    let current = after_local.weekday().number_from_monday() as i64;
    let target = i64::from(weekday);
    let mut days_ahead = (target - current + 7) % 7;
    let mut cand = (after_local.date_naive() + chrono::Duration::days(days_ahead)).and_time(nt);
    if cand <= after_local.naive_local() {
        days_ahead += 7;
        cand = (after_local.date_naive() + chrono::Duration::days(days_ahead)).and_time(nt);
    }
    chrono::Local
        .from_local_datetime(&cand)
        .single()
        .map(|dt| dt.timestamp())
        .ok_or_else(|| format!("时间 {at_clock} 在本地时区有歧义，请换写法"))
}

/// 计算 `after_ts` 之后的下一次本地每月固定日期/钟点触发时间。
pub fn next_monthly_epoch_after(
    day_of_month: u8,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    if !(1..=31).contains(&day_of_month) {
        return Err(format!("day_of_month 必须在 1..=31，当前为 {day_of_month}"));
    }
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析每月钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_utc = chrono::DateTime::from_timestamp(after_ts, 0)
        .ok_or_else(|| format!("非法时间戳: {after_ts}"))?;
    let after_local = after_utc.with_timezone(&chrono::Local);
    let mut year = after_local.year();
    let mut month = after_local.month();

    for _ in 0..24 {
        if let Some(date) = chrono::NaiveDate::from_ymd_opt(year, month, u32::from(day_of_month)) {
            let cand = date.and_time(nt);
            if cand > after_local.naive_local() {
                return chrono::Local
                    .from_local_datetime(&cand)
                    .single()
                    .map(|dt| dt.timestamp())
                    .ok_or_else(|| format!("时间 {at_clock} 在本地时区有歧义，请换写法"));
            }
        }
        month += 1;
        if month > 12 {
            month = 1;
            year += 1;
        }
    }

    Err("无法在未来 24 个月内找到合法的每月触发日期".into())
}

pub fn next_lunar_monthly_epoch_after(
    day_of_month: u8,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    if !(1..=30).contains(&day_of_month) {
        return Err(format!(
            "农历 day_of_month 必须在 1..=30，当前为 {day_of_month}"
        ));
    }
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析农历每月钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_local = local_datetime_after(after_ts)?;

    for offset in 0..=MAX_LUNAR_MONTHLY_LOOKAHEAD_DAYS {
        let date = after_local.date_naive() + chrono::Duration::days(offset);
        let cand = date.and_time(nt);
        if cand <= after_local.naive_local() {
            continue;
        }
        let Some(lunar) = solar_to_lunar_date(date) else {
            continue;
        };
        if lunar.day == day_of_month {
            return local_epoch(cand, at_clock);
        }
    }

    Err(format!(
        "无法在未来 {MAX_LUNAR_MONTHLY_LOOKAHEAD_DAYS} 天内找到合法的农历每月触发日期"
    ))
}

pub fn next_yearly_epoch_after(
    month: u8,
    day: u8,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    if !gregorian_month_day_can_exist(month, day) {
        return Err(format!("阳历日期必须真实存在，当前为 {month}-{day}"));
    }
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析每年钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_local = local_datetime_after(after_ts)?;

    for year in after_local.year()..=(after_local.year() + 25) {
        let Some(date) = chrono::NaiveDate::from_ymd_opt(year, u32::from(month), u32::from(day))
        else {
            continue;
        };
        let cand = date.and_time(nt);
        if cand > after_local.naive_local() {
            return local_epoch(cand, at_clock);
        }
    }

    Err("无法在未来 25 年内找到合法的每年触发日期".into())
}

pub fn next_lunar_yearly_epoch_after(
    month: u8,
    day: u8,
    leap_month: bool,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    if !lunar_month_day_can_exist(month, day) {
        return Err(format!(
            "农历日期必须在 1..=12 月、1..=30 日内，当前为 {month}-{day}"
        ));
    }
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析农历每年钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_local = local_datetime_after(after_ts)?;
    let start_year = solar_to_lunar_date(after_local.date_naive())
        .map(|d| d.year)
        .unwrap_or(after_local.year())
        - 1;

    for lunar_year in start_year..=(start_year + 50) {
        let Some(date) = lunar_to_naive_date(lunar_year, month, day, leap_month) else {
            continue;
        };
        let cand = date.and_time(nt);
        if cand > after_local.naive_local() {
            return local_epoch(cand, at_clock);
        }
    }

    Err("无法在未来 50 个农历年内找到合法的农历每年触发日期".into())
}

pub fn lunar_clock_to_epoch(
    year: i32,
    month: u8,
    day: u8,
    leap_month: bool,
    at_clock: &str,
) -> std::result::Result<i64, String> {
    if !lunar_month_day_can_exist(month, day) {
        return Err(format!(
            "农历日期必须在 1..=12 月、1..=30 日内，当前为 {month}-{day}"
        ));
    }
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析农历一次性钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let date = lunar_to_naive_date(year, month, day, leap_month).ok_or_else(|| {
        format!("农历日期不存在: year={year} month={month} day={day} leap_month={leap_month}")
    })?;
    local_epoch(date.and_time(nt), at_clock)
}

pub fn next_yearly_range_epoch_after(
    start_month: u8,
    start_day: u8,
    end_month: Option<u8>,
    end_day: Option<u8>,
    duration_days: Option<u16>,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    validate_yearly_range_shape(
        start_month,
        start_day,
        end_month,
        end_day,
        duration_days,
        false,
    )?;
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析年度区间钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_local = local_datetime_after(after_ts)?;

    for offset in 0..=MAX_YEARLY_LOOKAHEAD_DAYS {
        let date = after_local.date_naive() + chrono::Duration::days(offset);
        let cand = date.and_time(nt);
        if cand <= after_local.naive_local() {
            continue;
        }
        if gregorian_date_in_yearly_range(
            date,
            start_month,
            start_day,
            end_month,
            end_day,
            duration_days,
        ) {
            return local_epoch(cand, at_clock);
        }
    }

    Err("无法在未来 25 年内找到合法的年度区间触发日期".into())
}

#[allow(clippy::too_many_arguments)]
pub fn next_lunar_yearly_range_epoch_after(
    start_month: u8,
    start_day: u8,
    start_leap_month: bool,
    end_month: Option<u8>,
    end_day: Option<u8>,
    end_leap_month: bool,
    duration_days: Option<u16>,
    at_clock: &str,
    after_ts: i64,
) -> std::result::Result<i64, String> {
    validate_yearly_range_shape(
        start_month,
        start_day,
        end_month,
        end_day,
        duration_days,
        true,
    )?;
    let nt = parse_daily_time(at_clock)
        .ok_or_else(|| format!("无法解析农历年度区间钟点 '{at_clock}'，请用 '09:00' 格式"))?;
    let after_local = local_datetime_after(after_ts)?;

    for offset in 0..=MAX_YEARLY_LOOKAHEAD_DAYS {
        let date = after_local.date_naive() + chrono::Duration::days(offset);
        let cand = date.and_time(nt);
        if cand <= after_local.naive_local() {
            continue;
        }
        if lunar_date_in_yearly_range(
            date,
            start_month,
            start_day,
            start_leap_month,
            end_month,
            end_day,
            end_leap_month,
            duration_days,
        ) {
            return local_epoch(cand, at_clock);
        }
    }

    Err("无法在未来 25 年内找到合法的农历年度区间触发日期".into())
}

fn local_datetime_after(
    after_ts: i64,
) -> std::result::Result<chrono::DateTime<chrono::Local>, String> {
    chrono::DateTime::from_timestamp(after_ts, 0)
        .ok_or_else(|| format!("非法时间戳 {after_ts}"))
        .map(|dt| dt.with_timezone(&chrono::Local))
}

fn local_epoch(cand: chrono::NaiveDateTime, at_clock: &str) -> std::result::Result<i64, String> {
    chrono::Local
        .from_local_datetime(&cand)
        .single()
        .map(|dt| dt.timestamp())
        .ok_or_else(|| format!("时间 {at_clock} 在本地时区有歧义，请换写法"))
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

fn yearly_range_has_end(job: &CronJob, lunar: bool) -> bool {
    let duration_ok = job
        .duration_days
        .is_some_and(|d| (1..=MAX_YEARLY_RANGE_DAYS).contains(&d));
    let end_ok = job.end_month.zip(job.end_day).is_some_and(|(m, d)| {
        if lunar {
            lunar_month_day_can_exist(m, d)
        } else {
            gregorian_month_day_can_exist(m, d)
        }
    });
    duration_ok ^ end_ok
}

fn validate_yearly_range_shape(
    start_month: u8,
    start_day: u8,
    end_month: Option<u8>,
    end_day: Option<u8>,
    duration_days: Option<u16>,
    lunar: bool,
) -> std::result::Result<(), String> {
    let start_ok = if lunar {
        lunar_month_day_can_exist(start_month, start_day)
    } else {
        gregorian_month_day_can_exist(start_month, start_day)
    };
    if !start_ok {
        return Err("年度区间开始日期无效".into());
    }

    let duration_ok = duration_days.is_some_and(|d| (1..=MAX_YEARLY_RANGE_DAYS).contains(&d));
    let end_ok = end_month.zip(end_day).is_some_and(|(m, d)| {
        if lunar {
            lunar_month_day_can_exist(m, d)
        } else {
            gregorian_month_day_can_exist(m, d)
        }
    });
    match (duration_ok, end_ok) {
        (true, false) | (false, true) => Ok(()),
        (false, false) => Err("年度区间需要 duration_days 或 end_month/end_day".into()),
        (true, true) => Err("duration_days 与 end_month/end_day 只能提供一种".into()),
    }
}

fn gregorian_date_in_yearly_range(
    date: chrono::NaiveDate,
    start_month: u8,
    start_day: u8,
    end_month: Option<u8>,
    end_day: Option<u8>,
    duration_days: Option<u16>,
) -> bool {
    for start_year in [date.year() - 1, date.year(), date.year() + 1] {
        let Some(start) = chrono::NaiveDate::from_ymd_opt(
            start_year,
            u32::from(start_month),
            u32::from(start_day),
        ) else {
            continue;
        };
        let Some(end) = gregorian_range_end(start, end_month, end_day, duration_days) else {
            continue;
        };
        if date >= start && date <= end {
            return true;
        }
    }
    false
}

fn gregorian_range_end(
    start: chrono::NaiveDate,
    end_month: Option<u8>,
    end_day: Option<u8>,
    duration_days: Option<u16>,
) -> Option<chrono::NaiveDate> {
    if let Some(days) = duration_days {
        return Some(start + chrono::Duration::days(i64::from(days) - 1));
    }

    let end_month = end_month?;
    let end_day = end_day?;
    let mut end_year = start.year();
    let mut end =
        chrono::NaiveDate::from_ymd_opt(end_year, u32::from(end_month), u32::from(end_day))?;
    if end < start {
        end_year += 1;
        end = chrono::NaiveDate::from_ymd_opt(end_year, u32::from(end_month), u32::from(end_day))?;
    }
    Some(end)
}

#[allow(clippy::too_many_arguments)]
fn lunar_date_in_yearly_range(
    solar_date: chrono::NaiveDate,
    start_month: u8,
    start_day: u8,
    start_leap_month: bool,
    end_month: Option<u8>,
    end_day: Option<u8>,
    end_leap_month: bool,
    duration_days: Option<u16>,
) -> bool {
    let Some(lunar) = solar_to_lunar_date(solar_date) else {
        return false;
    };
    for lunar_year in [lunar.year - 1, lunar.year, lunar.year + 1] {
        let Some((start, end)) = lunar_range_bounds(
            lunar_year,
            start_month,
            start_day,
            start_leap_month,
            end_month,
            end_day,
            end_leap_month,
            duration_days,
        ) else {
            continue;
        };
        if solar_date >= start && solar_date <= end {
            return true;
        }
    }
    false
}

#[allow(clippy::too_many_arguments)]
fn lunar_range_bounds(
    lunar_year: i32,
    start_month: u8,
    start_day: u8,
    start_leap_month: bool,
    end_month: Option<u8>,
    end_day: Option<u8>,
    end_leap_month: bool,
    duration_days: Option<u16>,
) -> Option<(chrono::NaiveDate, chrono::NaiveDate)> {
    let start = lunar_to_naive_date(lunar_year, start_month, start_day, start_leap_month)?;
    let end = if let Some(days) = duration_days {
        start + chrono::Duration::days(i64::from(days) - 1)
    } else {
        let end_month = end_month?;
        let end_day = end_day?;
        let mut end = lunar_to_naive_date(lunar_year, end_month, end_day, end_leap_month)?;
        if end < start {
            end = lunar_to_naive_date(lunar_year + 1, end_month, end_day, end_leap_month)?;
        }
        end
    };
    Some((start, end))
}

fn lunar_to_naive_date(
    year: i32,
    month: u8,
    day: u8,
    leap_month: bool,
) -> Option<chrono::NaiveDate> {
    let lunar = LunarDate {
        year,
        month,
        day,
        is_leap_month: leap_month,
    };
    validate_lunar_date(lunar).ok()?;
    let solar = lunar_to_solar(lunar).ok()?;
    chrono::NaiveDate::from_ymd_opt(solar.year, u32::from(solar.month), u32::from(solar.day))
}

fn solar_to_lunar_date(date: chrono::NaiveDate) -> Option<LunarDate> {
    solar_to_lunar(SolarDate {
        year: date.year(),
        month: date.month() as u8,
        day: date.day() as u8,
    })
    .ok()
}

/// 从 `cron.json` 读取全部任务。文件不存在时返回空列表（不报错）。
///
/// **损坏处理（修复「损坏 cron.json 被静默当空表、随后覆盖真实任务」P2）**：旧实现把
/// 解析失败或结构不认识都降级为 `Vec::new()`，于是 `CronController` 会以【空任务表】启动，
/// 下一次任意保存（cron_add / 调度推进）就会用空表覆盖掉用户磁盘上的真实 cron.json，
/// 等于把损坏升级为「永久丢失」。现在改为：一旦发现损坏（JSON 解析失败、根结构不认识、
/// 或 `jobs` 反序列化失败），把原文件【隔离】为 `<cron.json>.corrupt-<8-hex>`，并打
/// error 级日志指引人工恢复，再以空表启动——这样原数据得以保留在隔离副本里，本次启动
/// 的空表写入也不会破坏它。
pub fn load_jobs<P: AsRef<Path>>(cron_file: P) -> Result<Vec<CronJob>> {
    let cron_file = cron_file.as_ref();
    if !cron_file.exists() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(cron_file)?;
    // 兼容 {"jobs":[...]} 包装与裸数组两种写法。
    let root: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            if quarantine_corrupt(cron_file, format!("JSON 解析失败: {e}")) {
                return Ok(Vec::new());
            }
            return Err(StoreError::Other(
                "cron.json 损坏且隔离失败，拒绝以空任务表启动以免后续保存覆盖原损坏文件".into(),
            ));
        }
    };
    let jobs_val = match root.get("jobs") {
        Some(arr) => arr,
        None if root.is_array() => &root,
        None => {
            if quarantine_corrupt(cron_file, "根结构不认识（非 {\"jobs\":[...]} 也非裸数组）")
            {
                return Ok(Vec::new());
            }
            return Err(StoreError::Other(
                "cron.json 损坏且隔离失败，拒绝以空任务表启动以免后续保存覆盖原损坏文件".into(),
            ));
        }
    };
    match serde_json::from_value::<Vec<CronJob>>(jobs_val.clone()) {
        Ok(jobs) => {
            let (jobs, repaired) = validate_and_repair_jobs(cron_file, jobs);
            if repaired {
                // 这里必须立即把修复后的 disabled 状态写回磁盘，而不是只改内存。
                // 否则日志里的「已禁用」只是本进程内的临时状态：进程重启后，同一个
                // 非法 enabled 任务还会从旧 cron.json 重新加载，继续重复告警/重复修复。
                // 若此处写盘失败，直接把错误返回给调用方，让启动路径感知「修复未落盘」；
                // 这样比一边声称已修复、一边把旧文件留在磁盘上更诚实，也更容易排查权限
                // 或磁盘故障。
                save_jobs(cron_file, &jobs)?;
            }
            Ok(jobs)
        }
        Err(e) => {
            if quarantine_corrupt(cron_file, format!("jobs 字段反序列化失败: {e}")) {
                return Ok(Vec::new());
            }
            Err(StoreError::Other(
                "cron.json 损坏且隔离失败，拒绝以空任务表启动以免后续保存覆盖原损坏文件".into(),
            ))
        }
    }
}

/// 校验加载的任务表，把语义非法的条目【就地禁用】（保留数据、不丢失），并打 warn 日志。
/// 同时为可调度但缺失 `next_run` 的启用任务补齐 `next_run`。
///
/// 修复 P3：反序列化能通过类型层、但语义非法的任务（如 `kind=every` 缺 `interval_s`），
/// 调度时会因 `unwrap_or(0)` 让 `next_run = now`、每个 tick 到期。这里在返回前逐条
/// 校验（[`CronJob::is_schedulable`]），把不通过的【禁用】（`enabled = false`，任务
/// 仍留在表里，用户可见可改）而非当成可调度任务接受——既不让坏任务疯狂触发，也不
/// 静默丢弃用户数据。已经 `enabled = false` 的坏任务不再重复告警。
///
/// 修复 P2（缺失 `next_run` 永远不触发）：`CronJob.next_run` 有 serde 默认值 `0`。
/// 旧格式或手工编辑的 cron 条目若有合法 `kind/interval_s/at_epoch` 但缺 `next_run`，
/// `is_schedulable()` 认为有效、调度层要求 `next_run > 0`，结果既不禁用也不触发。
/// 这里对可调度的启用任务在 `next_run <= 0` 时自动补齐并打 info 日志。
///
/// 返回值中的 `bool` 表示是否真的修改过任务表。调用方据此决定是否立刻保存，保证
/// 「已禁用」不是只存在于内存里的假修复。
fn validate_and_repair_jobs(cron_file: &Path, mut jobs: Vec<CronJob>) -> (Vec<CronJob>, bool) {
    let mut repaired = false;
    // 本次调用时刻的 unix 时间戳，用于补齐缺失的 next_run。
    let now = chrono::Utc::now().timestamp();
    // 跟踪已出现的 id，发现重复时重新生成。
    // add_job 只保证新任务 id 不冲突，但旧版或手工编辑的 cron.json 可能携带重复 id。
    // 调度器用 dispatched.iter().find 按 id 匹配推进状态，cron_remove 也按 id 过滤；
    // 重复 id 会让一个入队或删除操作命中多个任务，产生不可预期行为。
    // 使用 HashSet<String> 而非 &str：id 字符串很短（8 字符），clone 成本极低，
    // 且避免了引用借用与后续 j.id 赋值之间的冲突。
    let mut seen: HashSet<String> = HashSet::with_capacity(jobs.len());
    for j in jobs.iter_mut() {
        // 校验并修复 cron 任务 id：必须是非空 8 位小写 hex 且全局唯一。
        let id_is_valid = |id: &str| -> bool {
            !id.is_empty() && id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit())
        };
        let id_needs_repair = !id_is_valid(&j.id) || !seen.insert(j.id.clone());
        if id_needs_repair {
            let old_id = j.id.clone();
            // 循环生成直至合法且唯一，避免随机碰撞。
            let new_id = loop {
                let candidate = CronJob::generate_id();
                if id_is_valid(&candidate) && seen.insert(candidate.clone()) {
                    break candidate;
                }
                // 极端碰撞：generate_id 使用 u32 hex，碰撞概率极低，循环有界。
            };
            tracing::warn!(
                "cron 任务 id 非法/重复已重新生成（{}: 旧 id={old_id} → 新 id={new_id} name={}），\
                 原 id 无效或重复，请检查并修正配置",
                cron_file.display(),
                j.name,
            );
            j.id = new_id;
            repaired = true;
        }
        if j.enabled && !j.is_schedulable() {
            tracing::warn!(
                "cron 任务语义无效，已禁用（{}: id={} name={} kind={:?} interval_s={:?} at_epoch={:?} at_clock={:?} weekday={:?} day_of_month={:?} month={:?} day={:?} end_month={:?} end_day={:?} duration_days={:?} leap_month={:?} end_leap_month={:?}），\
                 请修正配置后重新启用",
                cron_file.display(),
                j.id,
                j.name,
                j.kind,
                j.interval_s,
                j.at_epoch,
                j.at_clock,
                j.weekday,
                j.day_of_month,
                j.month,
                j.day,
                j.end_month,
                j.end_day,
                j.duration_days,
                j.leap_month,
                j.end_leap_month,
            );
            j.enabled = false;
            repaired = true;
        }
        // 可调度且启用的任务，若 next_run 缺失或为 0 → 自动补齐并持久化，
        // 避免任务被加载后永远不触发（serde 默认 0 导致调度层跳过）。
        if j.enabled && j.is_schedulable() && j.next_run <= 0 {
            let repaired_next = match j.kind {
                // Every：下一次触发 = 当前时刻 + 间隔秒数。
                // 用 now + interval_s 而非直接 now，避免加载瞬间就触发——给调度层至少
                // 一个 tick 的缓冲，也符合「周期性任务从此刻开始计时」的用户预期。
                CronKind::Every => {
                    let interval = j.interval_s.unwrap_or(3600) as i64;
                    Ok(now.saturating_add(interval))
                }
                // At：用 at_epoch 作为触发时间（若已过期则由调度层处理）。
                CronKind::At => Ok(j.at_epoch.unwrap_or(now)),
                // Daily：用本地日历钟点补齐下一次触发。
                CronKind::Daily => next_daily_epoch_after(j.at_clock.as_deref().unwrap_or(""), now),
                // Weekly / Monthly：用本地日历规则补齐下一次触发。
                CronKind::Weekly => next_weekly_epoch_after(
                    j.weekday.unwrap_or(0),
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
                CronKind::Monthly => next_monthly_epoch_after(
                    j.day_of_month.unwrap_or(0),
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
                CronKind::LunarMonthly => next_lunar_monthly_epoch_after(
                    j.day_of_month.unwrap_or(0),
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
                CronKind::Yearly => next_yearly_epoch_after(
                    j.month.unwrap_or(0),
                    j.day.unwrap_or(0),
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
                CronKind::LunarYearly => next_lunar_yearly_epoch_after(
                    j.month.unwrap_or(0),
                    j.day.unwrap_or(0),
                    j.leap_month.unwrap_or(false),
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
                CronKind::YearlyRange => next_yearly_range_epoch_after(
                    j.month.unwrap_or(0),
                    j.day.unwrap_or(0),
                    j.end_month,
                    j.end_day,
                    j.duration_days,
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
                CronKind::LunarYearlyRange => next_lunar_yearly_range_epoch_after(
                    j.month.unwrap_or(0),
                    j.day.unwrap_or(0),
                    j.leap_month.unwrap_or(false),
                    j.end_month,
                    j.end_day,
                    j.end_leap_month.unwrap_or(false),
                    j.duration_days,
                    j.at_clock.as_deref().unwrap_or(""),
                    now,
                ),
            };
            match repaired_next {
                Ok(next_run) => {
                    j.next_run = next_run;
                    tracing::info!(
                        "cron 任务 {} (id={} name={}) 缺失 next_run，已自动补齐为 {}",
                        cron_file.display(),
                        j.id,
                        j.name,
                        j.next_run,
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "cron 任务缺失 next_run 且无法按日历规则补齐，已禁用（{}: id={} name={} kind={:?} error={}），\
                         请修正配置后重新启用",
                        cron_file.display(),
                        j.id,
                        j.name,
                        j.kind,
                        e,
                    );
                    j.enabled = false;
                    j.next_run = 0;
                }
            }
            repaired = true;
        }
    }
    (jobs, repaired)
}

/// 把损坏的 cron 文件隔离为 `<原名>.corrupt-<8-hex>`，保留原数据供人工恢复。
///
/// 隔离名带随机后缀，避免多次损坏互相覆盖、丢失样本。隔离失败（如权限不足）时仅打日志
/// 并返回 `false`——此时调用方应【拒绝以空表启动】，防止后续 `save_jobs` 把新内容写到
/// 原路径覆盖掉唯一损坏副本，导致人工恢复连副本都没了。
///
/// 返回 `true` 表示隔离成功（文件已移走），调用方可安全以空表启动。
fn quarantine_corrupt(path: &Path, reason: impl AsRef<str>) -> bool {
    let reason = reason.as_ref();
    let nonce: u32 = rand::thread_rng().gen();
    let parent = path.parent().unwrap_or_else(|| Path::new(""));
    let fname = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cron.json".to_string());
    let quarantined = parent.join(format!("{fname}.corrupt-{nonce:08x}"));
    match std::fs::rename(path, &quarantined) {
        Ok(()) => {
            tracing::error!(
                "cron.json 已损坏被隔离（{reason}），原文件移至 {} 供人工恢复；本次以空任务表启动。",
                quarantined.display()
            );
            true
        }
        Err(e) => {
            tracing::error!(
                "cron.json 损坏（{reason}）且隔离失败 ({e})；原损坏文件保留未动，拒绝以空任务表启动以免后续保存覆盖。"
            );
            false
        }
    }
}

/// 把全部任务写回 `cron.json`（原子写）。
pub fn save_jobs<P: AsRef<Path>>(cron_file: P, jobs: &[CronJob]) -> Result<()> {
    ensure_file(&cron_file, "{\"jobs\":[]}\n")?;
    let wrapper = serde_json::json!({ "jobs": jobs });
    let pretty = serde_json::to_string_pretty(&wrapper)?;
    atomic_write(cron_file, pretty.as_bytes())
}

/// 便捷：按 layout 读取。
pub fn load_jobs_layout(layout: &Layout) -> Result<Vec<CronJob>> {
    load_jobs(&layout.cron_file)
}

/// 便捷：按 layout 写回。
pub fn save_jobs_layout(layout: &Layout, jobs: &[CronJob]) -> Result<()> {
    save_jobs(&layout.cron_file, jobs)
}

#[cfg(test)]
mod tests;
