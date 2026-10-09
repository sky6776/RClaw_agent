use super::*;
use rclaw_store::Layout;
use serde_json::json;

/// 构造一个绑定临时 layout 的 CronAdd（任务表落临时目录，互不影响）。
fn make_adder() -> (tempfile::TempDir, Arc<CronController>, CronAdd) {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path().join("agent"));
    std::fs::create_dir_all(&layout.base).unwrap();
    let cron = Arc::new(CronController::load(layout).unwrap());
    let adder = CronAdd::new(cron.clone());
    (dir, cron, adder)
}

/// P2：interval_s 为超过 u32::MAX 的正数时，不得被 `as u32` 截断成错误值（甚至 0），
/// 必须直接拒绝。
#[tokio::test]
async fn cron_add_rejects_huge_interval() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "x",
            "schedule_type": "every",
            "interval_s": i64::MAX,
            "message": "hi"
        }))
        .await;
    assert!(r.is_error, "超大 interval_s 必须被拒绝而非截断");
}

/// P2：delay_s 超出上限（或导致触发时间溢出）必须拒绝。
#[tokio::test]
async fn cron_add_rejects_huge_delay() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "x",
            "schedule_type": "after",
            "delay_s": i64::MAX,
            "message": "hi"
        }))
        .await;
    assert!(r.is_error, "超大 delay_s 必须被拒绝");
}

/// 正常范围内的 interval 仍可成功添加（回归保护）。
#[tokio::test]
async fn cron_add_accepts_normal_interval() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "remind",
            "schedule_type": "every",
            "interval_s": 60,
            "message": "drink water"
        }))
        .await;
    assert!(!r.is_error, "正常 interval 应成功: {r:?}");
    assert_eq!(cron.list_snapshot().len(), 1);
}

#[tokio::test]
async fn cron_add_rejects_every_with_at_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "bad",
            "schedule_type": "every",
            "interval_s": 86400,
            "at_clock": "11:00",
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "every + at_clock 必须拒绝，避免误以为能对齐固定钟点"
    );
}

#[tokio::test]
async fn cron_add_daily_creates_daily_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "daily",
            "schedule_type": "daily",
            "at_clock": "23:59",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "daily 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::Daily);
    assert_eq!(jobs[0].at_clock.as_deref(), Some("23:59"));
    assert!(jobs[0].next_run > chrono::Utc::now().timestamp());
}

#[tokio::test]
async fn cron_add_weekly_creates_weekly_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "weekly",
            "schedule_type": "weekly",
            "weekday": 2,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "weekly 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::Weekly);
    assert_eq!(jobs[0].weekday, Some(2));
    assert_eq!(jobs[0].at_clock.as_deref(), Some("09:00"));
}

#[tokio::test]
async fn cron_add_monthly_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "monthly",
            "schedule_type": "monthly",
            "day_of_month": 12,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "monthly 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

#[tokio::test]
async fn cron_add_monthly_creates_monthly_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "monthly",
            "schedule_type": "monthly",
            "day_of_month": 12,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "monthly 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::Monthly);
    assert_eq!(jobs[0].day_of_month, Some(12));
    assert_eq!(jobs[0].at_clock.as_deref(), Some("09:00"));
}

#[tokio::test]
async fn cron_add_lunar_monthly_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "water-bill",
            "schedule_type": "lunar_monthly",
            "day_of_month": 15,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "lunar_monthly 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

#[tokio::test]
async fn cron_add_lunar_monthly_creates_lunar_monthly_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "water-bill",
            "schedule_type": "lunar_monthly",
            "day_of_month": 15,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "lunar_monthly 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::LunarMonthly);
    assert_eq!(jobs[0].day_of_month, Some(15));
    assert_eq!(jobs[0].at_clock.as_deref(), Some("09:00"));
}

#[tokio::test]
async fn cron_add_yearly_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "vacation",
            "schedule_type": "yearly",
            "month": 6,
            "day": 1,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "yearly 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

#[tokio::test]
async fn cron_add_yearly_creates_yearly_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "birthday",
            "schedule_type": "yearly",
            "month": 7,
            "day": 12,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "yearly 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::Yearly);
    assert_eq!(jobs[0].month, Some(7));
    assert_eq!(jobs[0].day, Some(12));
    assert_eq!(jobs[0].at_clock.as_deref(), Some("09:00"));
}

#[tokio::test]
async fn cron_add_lunar_yearly_creates_lunar_yearly_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "lunar-birthday",
            "schedule_type": "lunar_yearly",
            "month": 5,
            "day": 6,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "lunar_yearly 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::LunarYearly);
    assert_eq!(jobs[0].month, Some(5));
    assert_eq!(jobs[0].day, Some(6));
    assert_eq!(jobs[0].leap_month, Some(false));
}

#[tokio::test]
async fn cron_add_lunar_yearly_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "lunar-birthday",
            "schedule_type": "lunar_yearly",
            "month": 5,
            "day": 6,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "lunar_yearly 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

#[tokio::test]
async fn cron_add_yearly_range_creates_yearly_range_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "vacation-window",
            "schedule_type": "yearly_range",
            "month": 6,
            "day": 1,
            "duration_days": 5,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "yearly_range 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::YearlyRange);
    assert_eq!(jobs[0].month, Some(6));
    assert_eq!(jobs[0].day, Some(1));
    assert_eq!(jobs[0].duration_days, Some(5));
}

#[tokio::test]
async fn cron_add_yearly_range_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "vacation-window",
            "schedule_type": "yearly_range",
            "month": 6,
            "day": 1,
            "duration_days": 5,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "yearly_range 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

#[tokio::test]
async fn cron_add_yearly_range_requires_duration_or_end() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "vacation-window",
            "schedule_type": "yearly_range",
            "month": 6,
            "day": 1,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "yearly_range 缺 duration_days/end_month/end_day 应拒绝，促使 agent 追问区间"
    );
}

#[tokio::test]
async fn cron_add_lunar_yearly_range_creates_lunar_yearly_range_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "lunar-window",
            "schedule_type": "lunar_yearly_range",
            "month": 5,
            "day": 6,
            "end_month": 5,
            "end_day": 8,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "lunar_yearly_range 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::LunarYearlyRange);
    assert_eq!(jobs[0].month, Some(5));
    assert_eq!(jobs[0].day, Some(6));
    assert_eq!(jobs[0].end_month, Some(5));
    assert_eq!(jobs[0].end_day, Some(8));
    assert_eq!(jobs[0].leap_month, Some(false));
    assert_eq!(jobs[0].end_leap_month, Some(false));
}

#[tokio::test]
async fn cron_add_lunar_yearly_range_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "lunar-window",
            "schedule_type": "lunar_yearly_range",
            "month": 5,
            "day": 6,
            "duration_days": 3,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "lunar_yearly_range 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

#[tokio::test]
async fn cron_add_lunar_clock_creates_one_shot_at_job() {
    let (_g, cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "lunar-once",
            "schedule_type": "lunar_clock",
            "year": 2099,
            "month": 8,
            "day": 6,
            "at_clock": "09:00",
            "message": "m"
        }))
        .await;
    assert!(!r.is_error, "lunar_clock 应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].kind, CronKind::At);
    assert!(jobs[0].at_epoch.unwrap_or(0) > chrono::Utc::now().timestamp());
    assert!(jobs[0].delete_after_run, "lunar_clock 默认触发后删除");
}

#[tokio::test]
async fn cron_add_lunar_clock_requires_clock() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "lunar-once",
            "schedule_type": "lunar_clock",
            "year": 2099,
            "month": 8,
            "day": 6,
            "message": "m"
        }))
        .await;
    assert!(
        r.is_error,
        "lunar_clock 缺 at_clock 应拒绝，促使 agent 追问具体时间"
    );
}

/// clock 调度类型：完整日期时间（未来）应解析为正确的本地 epoch。
/// 用 2099 年这种明确未来的固定日期，断言不依赖「现在」的具体值。
#[test]
fn parse_clock_full_datetime_future() {
    let ts = parse_clock_to_epoch("2099-01-01 00:00").unwrap();
    // 2099-01-01 在任何时区都是遥远的未来，epoch 必为很大的正数。
    assert!(ts > 4_000_000_000, "2099 年的 epoch 应足够大: {ts}");
}

/// clock 调度类型：完整日期时间在过去必须被拒绝（与 `at` 一致，避免立刻过期）。
#[test]
fn parse_clock_past_datetime_rejected() {
    // 1970-01-02 在任何时区都是过去。
    assert!(parse_clock_to_epoch("1970-01-02 00:00").is_err());
}

/// clock 调度类型：仅钟点应解析为「下一个」该钟点（未来约 24h 内）。
#[test]
fn parse_clock_time_only_resolves_future() {
    let now = chrono::Utc::now().timestamp();
    let ts = parse_clock_to_epoch("23:59").unwrap();
    assert!(ts > now, "下一个 23:59 应在未来: ts={ts} now={now}");
    assert!(
        ts <= now + 25 * 3600,
        "仅钟点解析不应超过约 24h: ts={ts} now={now}"
    );
}

/// clock 调度类型：无法解析的格式必须报错（而非退化成某种默认时间）。
#[test]
fn parse_clock_invalid_format_rejected() {
    assert!(parse_clock_to_epoch("not a time").is_err());
    assert!(parse_clock_to_epoch("25:99").is_err());
    assert!(parse_clock_to_epoch("   ").is_err());
}

/// clock 调度类型端到端：at_clock 解析出的 next_run 必须与解析函数一致，
/// 且最终落盘的是一次性 At 任务（delete_after_run 默认 true）。
#[tokio::test]
async fn cron_add_clock_type_end_to_end() {
    let (_g, cron, adder) = make_adder();
    let at_clock = "2099-01-01 00:00";
    let expected = parse_clock_to_epoch(at_clock).unwrap();
    let r = adder
        .execute(json!({
            "name": "future",
            "schedule_type": "clock",
            "at_clock": at_clock,
            "message": "hi"
        }))
        .await;
    assert!(!r.is_error, "clock 类型应成功: {r:?}");
    let jobs = cron.list_snapshot();
    assert_eq!(jobs.len(), 1, "应落盘 1 个任务");
    assert_eq!(jobs[0].kind, CronKind::At);
    assert_eq!(
        jobs[0].next_run, expected,
        "next_run 必须由 at_clock 解析得出"
    );
    assert!(jobs[0].delete_after_run, "一次性 clock 任务默认触发后删除");
}

/// clock 调度类型：缺 at_clock 必须报错，不得静默退化为当前时间。
#[tokio::test]
async fn cron_add_clock_missing_at_clock_rejected() {
    let (_g, _cron, adder) = make_adder();
    let r = adder
        .execute(json!({
            "name": "x",
            "schedule_type": "clock",
            "message": "hi"
        }))
        .await;
    assert!(r.is_error, "缺 at_clock 必须报错");
}
