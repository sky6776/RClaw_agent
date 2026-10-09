use super::*;
use tempfile::tempdir;

#[test]
fn roundtrip_jobs() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let jobs = vec![CronJob {
        id: "abcd1234".into(),
        name: "remind".into(),
        enabled: true,
        kind: CronKind::Every,
        interval_s: Some(60),
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
        message: "drink water".into(),
        channel: "system".into(),
        chat_id: "cron".into(),
        last_run: 0,
        next_run: 100,
        delete_after_run: false,
    }];
    save_jobs(&path, &jobs).unwrap();
    let loaded = load_jobs(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].id, "abcd1234");
    assert_eq!(loaded[0].kind, CronKind::Every);
}

/// P2：损坏的 cron.json 不得被静默当空表加载（否则下次 save 会用空表覆盖真实任务）；
/// 应被隔离为 `.corrupt-<hex>` 副本，并以空表启动，原数据保留待人工恢复。
#[test]
fn corrupt_cron_file_is_quarantined() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    std::fs::write(&path, "{ this is not valid json").unwrap();

    let jobs = load_jobs(&path).unwrap();
    assert!(jobs.is_empty(), "损坏文件应以空表启动，而非报错中断");
    // 原路径被隔离走（不再以损坏内容留在原位）。
    assert!(!path.exists(), "原损坏文件应已被移走隔离");
    // 出现一个 .corrupt-<hex> 副本。
    let mut found_quarantine = false;
    for entry in std::fs::read_dir(dir.path()).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.starts_with("cron.json.corrupt-") {
            found_quarantine = true;
            break;
        }
    }
    assert!(found_quarantine, "应生成隔离副本 cron.json.corrupt-<hex>");
}

/// P3：加载时校验任务语义——`kind=every` 且 interval_s=0 必须被禁用，
/// 否则调度时 unwrap_or(0) 会让它每个 tick 到期、疯狂触发。
#[test]
fn invalid_every_job_is_disabled_on_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let bad = CronJob {
        id: "dead0000".into(),
        name: "bad-every".into(),
        enabled: true,
        kind: CronKind::Every,
        interval_s: Some(0), // 0 → 非法
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
        message: "m".into(),
        channel: "system".into(),
        chat_id: "cron".into(),
        last_run: 0,
        next_run: 0,
        delete_after_run: false,
    };
    save_jobs(&path, &[bad]).unwrap();
    let loaded = load_jobs(&path).unwrap();
    assert_eq!(loaded.len(), 1, "非法任务应被禁用但保留，而非丢弃");
    assert!(
        !loaded[0].enabled,
        "interval_s=0 的 every 任务应在加载时被禁用"
    );

    // 修复不能只停留在内存里；磁盘上的 cron.json 也必须已经写回 disabled 状态。
    let text = std::fs::read_to_string(&path).unwrap();
    let root: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        root["jobs"][0]["enabled"],
        serde_json::Value::Bool(false),
        "修复后的 disabled 状态必须持久化，避免重启后重复加载非法 enabled 任务"
    );
}

/// P3：缺 interval_s 的 every 任务同样应被禁用。
#[test]
fn missing_interval_every_job_is_disabled_on_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let bad = CronJob {
        id: "miss0000".into(),
        name: "missing-interval".into(),
        enabled: true,
        kind: CronKind::Every,
        interval_s: None, // 缺失 → 非法
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
        message: "m".into(),
        channel: "system".into(),
        chat_id: "cron".into(),
        last_run: 0,
        next_run: 0,
        delete_after_run: false,
    };
    save_jobs(&path, &[bad]).unwrap();
    let loaded = load_jobs(&path).unwrap();
    assert!(!loaded[0].enabled, "缺 interval_s 的 every 任务应被禁用");
}

/// P3：合法任务（interval_s > 0）不得被误禁用。
#[test]
fn valid_every_job_stays_enabled_on_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let good = CronJob {
        id: "good0001".into(),
        name: "good-every".into(),
        enabled: true,
        kind: CronKind::Every,
        interval_s: Some(60),
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
        message: "m".into(),
        channel: "system".into(),
        chat_id: "cron".into(),
        last_run: 0,
        next_run: 100,
        delete_after_run: false,
    };
    save_jobs(&path, &[good]).unwrap();
    let loaded = load_jobs(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].enabled, "interval_s=60 的合法任务应保持启用");
}

/// Daily：下一次触发必须对齐到本地日历钟点，而不是「创建时刻 + 24h」。
#[test]
fn next_daily_epoch_aligns_to_local_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_daily_epoch_after("23:59", now).unwrap();
    assert!(ts > now, "下一次 daily 触发必须在未来: ts={ts} now={now}");
    assert!(
        ts <= now + 25 * 3600,
        "下一次 23:59 不应超过约 24h: ts={ts} now={now}"
    );

    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.format("%H:%M").to_string(), "23:59");
}

/// Daily：合法的每日任务加载后应保持启用，并在缺 next_run 时自动补齐。
#[test]
fn valid_daily_job_fills_next_run_on_load() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let daily = CronJob {
        id: "day00001".into(),
        name: "daily".into(),
        enabled: true,
        kind: CronKind::Daily,
        interval_s: None,
        at_epoch: None,
        at_clock: Some("23:59".into()),
        weekday: None,
        day_of_month: None,
        month: None,
        day: None,
        end_month: None,
        end_day: None,
        duration_days: None,
        leap_month: None,
        end_leap_month: None,
        message: "m".into(),
        channel: "system".into(),
        chat_id: "cron".into(),
        last_run: 0,
        next_run: 0,
        delete_after_run: false,
    };
    save_jobs(&path, &[daily]).unwrap();
    let loaded = load_jobs(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(loaded[0].enabled, "合法 daily 任务应保持启用");
    assert_eq!(loaded[0].kind, CronKind::Daily);
    assert!(loaded[0].next_run > chrono::Utc::now().timestamp());
}

#[test]
fn load_disables_calendar_job_when_next_run_cannot_be_repaired() {
    let now_lunar_year = solar_to_lunar_date(chrono::Local::now().date_naive())
        .unwrap()
        .year;
    let start_lunar_year = now_lunar_year - 1;
    let (month, day) = (1..=12)
        .flat_map(|month| (1..=30).map(move |day| (month, day)))
        .find(|(month, day)| {
            (start_lunar_year..=(start_lunar_year + 50)).all(|year| {
                validate_lunar_date(LunarDate {
                    year,
                    month: *month,
                    day: *day,
                    is_leap_month: true,
                })
                .is_err()
            })
        })
        .expect("测试区间内应存在未来 50 个农历年都不存在的闰月日期");

    let dir = tempdir().unwrap();
    let path = dir.path().join("cron.json");
    let job = CronJob {
        id: "lunarbad".into(),
        name: "bad-lunar-leap".into(),
        enabled: true,
        kind: CronKind::LunarYearly,
        interval_s: None,
        at_epoch: None,
        at_clock: Some("09:00".into()),
        weekday: None,
        day_of_month: None,
        month: Some(month),
        day: Some(day),
        end_month: None,
        end_day: None,
        duration_days: None,
        leap_month: Some(true),
        end_leap_month: None,
        message: "m".into(),
        channel: "system".into(),
        chat_id: "cron".into(),
        last_run: 0,
        next_run: 0,
        delete_after_run: false,
    };

    save_jobs(&path, &[job]).unwrap();
    let loaded = load_jobs(&path).unwrap();
    assert_eq!(loaded.len(), 1);
    assert!(
        !loaded[0].enabled,
        "无法补齐真实 next_run 的日历任务应禁用，而不是排到兜底时间"
    );
    assert_eq!(loaded[0].next_run, 0);
}

#[test]
fn next_weekly_epoch_aligns_to_weekday_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_weekly_epoch_after(2, "09:00", now).unwrap();
    assert!(ts > now);
    assert!(ts <= now + 8 * 24 * 3600);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.weekday().number_from_monday(), 2);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
}

#[test]
fn next_monthly_epoch_aligns_to_day_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_monthly_epoch_after(12, "09:00", now).unwrap();
    assert!(ts > now);
    assert!(ts <= now + 32 * 24 * 3600);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.day(), 12);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
}

#[test]
fn next_lunar_monthly_epoch_aligns_to_lunar_day_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_lunar_monthly_epoch_after(15, "09:00", now).unwrap();
    assert!(ts > now);
    assert!(ts <= now + 61 * 24 * 3600);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
    let lunar = solar_to_lunar_date(local.date_naive()).unwrap();
    assert_eq!(lunar.day, 15);
}

#[test]
fn next_lunar_monthly_epoch_handles_long_gap_between_day_30s() {
    let start = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).unwrap();
    let mut day_30s = Vec::new();
    for offset in 0..(366 * 80) {
        let date = start + chrono::Duration::days(offset);
        if solar_to_lunar_date(date).is_some_and(|lunar| lunar.day == 30) {
            day_30s.push(date);
        }
    }
    let (prev, next) = day_30s
        .windows(2)
        .find_map(|pair| {
            let gap = pair[1].signed_duration_since(pair[0]).num_days();
            (gap > 60).then_some((pair[0], pair[1]))
        })
        .expect("测试区间内应存在相邻农历三十间隔超过 60 天的样本");

    let after = local_epoch(prev.and_hms_opt(10, 0, 0).unwrap(), "10:00").unwrap();
    let ts = next_lunar_monthly_epoch_after(30, "09:00", after).unwrap();
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);

    assert_eq!(local.date_naive(), next);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
    let lunar = solar_to_lunar_date(local.date_naive()).unwrap();
    assert_eq!(lunar.day, 30);
}

#[test]
fn next_yearly_epoch_aligns_to_month_day_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_yearly_epoch_after(7, 12, "09:00", now).unwrap();
    assert!(ts > now);
    assert!(ts <= now + 367 * 24 * 3600);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.month(), 7);
    assert_eq!(local.day(), 12);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
}

#[test]
fn next_lunar_yearly_epoch_aligns_to_lunar_date_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_lunar_yearly_epoch_after(5, 6, false, "09:00", now).unwrap();
    assert!(ts > now);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
    let lunar = solar_to_lunar_date(local.date_naive()).unwrap();
    assert_eq!((lunar.month, lunar.day, lunar.is_leap_month), (5, 6, false));
}

#[test]
fn lunar_clock_to_epoch_aligns_to_lunar_date_and_clock() {
    let ts = lunar_clock_to_epoch(2099, 8, 6, false, "09:00").unwrap();
    assert!(ts > chrono::Utc::now().timestamp());
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
    let lunar = solar_to_lunar_date(local.date_naive()).unwrap();
    assert_eq!(
        (lunar.year, lunar.month, lunar.day, lunar.is_leap_month),
        (2099, 8, 6, false)
    );
}

#[test]
fn next_yearly_range_epoch_aligns_to_window_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts = next_yearly_range_epoch_after(6, 1, None, None, Some(3), "09:00", now).unwrap();
    assert!(ts > now);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.month(), 6);
    assert!((1..=3).contains(&local.day()));
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
}

#[test]
fn next_lunar_yearly_range_epoch_aligns_to_lunar_window_and_clock() {
    let now = chrono::Utc::now().timestamp();
    let ts =
        next_lunar_yearly_range_epoch_after(5, 6, false, None, None, false, Some(2), "09:00", now)
            .unwrap();
    assert!(ts > now);
    let local = chrono::DateTime::from_timestamp(ts, 0)
        .unwrap()
        .with_timezone(&chrono::Local);
    assert_eq!(local.format("%H:%M").to_string(), "09:00");
    let lunar = solar_to_lunar_date(local.date_naive()).unwrap();
    assert_eq!(lunar.month, 5);
    assert!((6..=7).contains(&lunar.day));
    assert!(!lunar.is_leap_month);
}
