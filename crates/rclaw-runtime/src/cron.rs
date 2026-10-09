//! cron 调度服务。
//!
//! 周期扫描共享任务表，到期则把任务文本注入 inbound（作为 `[CRON_TASK]` 消息），
//! 并更新 next_run / 触发后删除一次性任务。持久化由 [`CronController`] 负责。

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use tokio::sync::mpsc;

use rclaw_core::{BusMessage, CRON_TASK_PREFIX};
use rclaw_store::cron_store::{
    next_daily_epoch_after, next_lunar_monthly_epoch_after, next_lunar_yearly_epoch_after,
    next_lunar_yearly_range_epoch_after, next_monthly_epoch_after, next_weekly_epoch_after,
    next_yearly_epoch_after, next_yearly_range_epoch_after, save_jobs_layout, CronJob, CronKind,
};
use rclaw_tools::CronController;

const TICK_DELAY_TOLERANCE: Duration = Duration::from_secs(5);
const SLOW_CRON_STEP_WARN_AFTER: Duration = Duration::from_secs(2);

pub struct CronService {
    controller: Arc<CronController>,
    inbound_tx: mpsc::Sender<BusMessage>,
    check_interval_sec: u64,
}

impl CronService {
    pub fn new(
        controller: Arc<CronController>,
        inbound_tx: mpsc::Sender<BusMessage>,
        check_interval_sec: u64,
    ) -> Self {
        CronService {
            controller,
            inbound_tx,
            check_interval_sec,
        }
    }

    /// 启动调度循环（永不返回，直到进程退出）。
    pub async fn run(self) {
        let interval = Duration::from_secs(self.check_interval_sec.max(1));
        tracing::info!(check_interval_sec = interval.as_secs(), "cron 调度器已启动");

        loop {
            let sleep_started = Instant::now();
            tokio::time::sleep(interval).await;
            let tick_started = Instant::now();
            let sleep_elapsed = tick_started.duration_since(sleep_started);
            if sleep_elapsed.saturating_sub(interval) > TICK_DELAY_TOLERANCE {
                tracing::warn!(
                    check_interval_sec = interval.as_secs(),
                    elapsed_ms = sleep_elapsed.as_millis(),
                    "cron tick 延迟，调度器在预期时间未获得执行"
                );
            } else {
                tracing::debug!(elapsed_ms = sleep_elapsed.as_millis(), "cron tick 开始");
            }

            // P2：每个 tick 先【独立】重试落盘上次写盘失败遗留的 dirty 快照，直到成功。
            //
            // 这一步与「处理到期任务」解耦，是修复「一次性任务删除/禁用后磁盘不收敛、
            // 重启复活」的关键：调度推进状态时若 `save_jobs_layout` 失败，内存虽然已
            // 推进（防本进程重复触发），但被删除/禁用的一次性任务此后不再「到期」，
            // 后续 tick 可能没有任何到期任务、从而不会进入 `process_due_jobs` 的保存路径，
            // 磁盘就会长期停在旧状态。此处主动 `flush_dirty` 保证：即便没有到期任务，
            // 脏快照也会被反复重写落盘直到收敛，重启后任务不再「复活」。
            //
            // `flush_dirty` 会进入与 add/remove/runtime 相同的 `jobs` 锁写盘串行区：
            // 这样 dirty 旧快照不会和较新的任务变更交错落盘，避免把磁盘倒写回旧状态。
            // 它没有 `.await`，只是在磁盘 I/O 期间短暂阻塞其它 cron 写操作。
            let flush_started = Instant::now();
            self.controller.flush_dirty();
            let flush_elapsed = flush_started.elapsed();
            if flush_elapsed >= SLOW_CRON_STEP_WARN_AFTER {
                tracing::warn!(
                    elapsed_ms = flush_elapsed.as_millis(),
                    "cron flush_dirty 耗时过长，可能在等待任务锁或磁盘 I/O"
                );
            }

            let process_started = Instant::now();
            self.process_due_jobs().await;
            let process_elapsed = process_started.elapsed();
            if process_elapsed >= SLOW_CRON_STEP_WARN_AFTER {
                tracing::warn!(
                    elapsed_ms = process_elapsed.as_millis(),
                    "cron 到期任务处理耗时过长，可能在等待 inbound 队列"
                );
            }
        }
    }

    /// 扫描到期任务并触发。
    ///
    /// 三段式，同时满足两条正确性红线：
    ///
    /// 1. **先入队、再改状态**（修复「丢任务」）：到期任务必须成功投递 inbound 后，
    ///    其 `next_run` / `enabled` / 是否删除 才会被推进。旧实现先在锁内改状态再用
    ///    `try_send` 投递——队列满或已关闭时静默失败，但状态已变：一次性提醒被删后再也
    ///    不执行，周期任务也跳过本次触发。
    ///
    /// 2. **推进后替换内存，写盘失败也替换**（同时修「内存/磁盘分叉」与「重复投递」）：
    ///    状态推进（next_run / enabled / 删除）后，把【新表】写回 `cron.json`；无论写盘
    ///    成功与否，都用新表替换内存。
    ///    - 写盘【成功】：内存与磁盘一致。
    ///    - 写盘【失败】：磁盘停在旧状态，但内存已推进——本进程不会在下一个 tick 再次
    ///      投递【同一批到期任务】（否则磁盘故障期间一次性提醒被反复执行、周期任务被
    ///      反复触发）；磁盘落后会在后续某次成功写盘时收敛。旧实现改完内存后 `let _ = persist()`
    ///      吞掉错误，重启后一次性任务会「复活」、已触发的提醒会重复跑——现在写盘失败会打
    ///      error 级告警，绝不静默。
    async fn process_due_jobs(&self) {
        let now = Utc::now().timestamp();
        let handle = self.controller.jobs_handle();

        // 1) 锁内只快照到期任务（clone），不改动任何状态。
        let due: Vec<CronJob> = {
            let guard = handle.lock().unwrap();
            guard
                .iter()
                .filter(|j| j.enabled && j.next_run > 0 && j.next_run <= now)
                .cloned()
                .collect()
        };
        if due.is_empty() {
            return;
        }

        let max_delay_sec = due
            .iter()
            .map(|job| now.saturating_sub(job.next_run))
            .max()
            .unwrap_or(0);
        tracing::info!(
            due_count = due.len(),
            max_delay_sec,
            "cron 检测到到期任务，开始投递"
        );

        // 2) 锁外 reserve（可能 await）+ 持锁「确认仍 due → 构造消息 → permit.send」。
        //    使用 reserve 关闭 TOCTOU 窗口：
        //    1. 先在锁外 reserve（可能 await）——拿到容量后队列不会再满；
        //    2. 持锁重新确认任务仍 due；
        //    3. 若仍 due → 在【同一把锁】内构造消息并 `permit.send`（无 await），期间锁不释放；
        //    4. 若不再 due → 释放锁并 drop permit，预留容量归还 channel。
        //
        //    **修复 P2「cron 检查与发送仍有竞态」**：旧实现在「持锁确认 still_due」后【先释放锁】，
        //    再到锁外构造消息并 `permit.send`。在多线程 tokio runtime 下，cron_remove 可在
        //    「锁释放 → send」之间删除任务，导致【已删除】的任务仍被投递（agent 会执行一条
        //    本应作废的提醒/周期任务）。现在把 still_due 检查与 permit.send 放进【同一个锁作用域】，
        //    二者之间没有任何 await 点、锁也不释放，并发 cron_remove 无法插入——
        //    已删除/禁用的任务绝不可能被投递。
        //    `permit.send` 本身无 await（同步方法），持有 `std::sync::MutexGuard` 不会让 future
        //    变成 `!Send`，可安全用于 tokio。
        let mut dispatched: Vec<CronJob> = Vec::new();
        for snap in &due {
            // 2.1 锁外 reserve（可能 await）。reserve 成功即代表 channel 未关闭且有容量。
            let permit = match self.inbound_tx.reserve().await {
                Ok(p) => p,
                Err(_) => {
                    tracing::warn!(
                        "cron 入队失败（inbound 已关闭，疑似进程退出），保持 due 待下次重试: id={} name={}",
                        snap.id,
                        snap.name,
                    );
                    continue;
                }
            };
            // 2.2 持锁完成「确认仍 due → 构造消息 → permit.send」，整段无 await，锁不释放。
            //     返回 Some(job) 表示已成功投递（加入 dispatched）；None 表示已不再 due（跳过）。
            let sent: Option<CronJob> = {
                let guard = handle.lock().unwrap();
                let still_due = guard
                    .iter()
                    .any(|j| j.id == snap.id && j.enabled && j.next_run > 0 && j.next_run <= now);
                if !still_due {
                    tracing::info!(
                        "cron 任务在入队前已不再 due（可能被并发删除/禁用），跳过: id={} name={}",
                        snap.id,
                        snap.name,
                    );
                    // 退出此块：guard 与 permit 同时 drop → 容量归还 channel。
                    None
                } else {
                    let task_msg = format!(
                        "{prefix}\nJob name: {name}\nJob message: {msg}\n\
                         Instruction: 本消息由本地 cron 调度器生成，请把它视为要替用户完成的工作。",
                        prefix = CRON_TASK_PREFIX,
                        name = snap.name,
                        msg = snap.message,
                    );
                    // capacity 已由 reserve 确保，发送不会阻塞也不会失败。
                    // 本 tokio 版本（1.52）的 `Permit::send` 返回 `()`（infallible）：reserve 成功
                    // 即代表 channel 未关闭且已预留容量，且本段无 await 点、接收端无法在此期间被 drop，
                    // 故发送必然成功——这也是把 send 放进锁内安全的原因（无 await、不阻塞、不会失败）。
                    permit.send(BusMessage::new_cron_job(
                        snap.channel.clone(),
                        snap.chat_id.clone(),
                        snap.id.clone(),
                        snap.name.clone(),
                        task_msg,
                    ));
                    Some(snap.clone())
                }
            };
            if let Some(job) = sent {
                dispatched.push(job);
            }
        }
        if dispatched.is_empty() {
            return;
        }

        tracing::info!(
            dispatched_count = dispatched.len(),
            "cron 到期任务已投递到 inbound 队列"
        );

        // 3) 持锁贯穿「计算新状态 → 写盘 → 替换内存」，整段关键区【无 await】。
        //
        //    两层防御合在一起，同时关掉两类丢失窗口：
        //
        //    a) **save↔swap 窗口（P1）**：旧实现是「锁内算 new_jobs → 释放锁去写盘 →
        //       重新加锁替换」。写盘期间若有并发的 cron_add/cron_remove 成功落盘（它们
        //       基于尚未被本操作替换的旧内存计算），本操作随后用【写盘前的旧快照】整体
        //       替换内存，会把那次并发变更静默覆盖。现在关键区从读内存一直延伸到 save
        //       成功并替换内存，期间锁不释放，并发 add/remove 被阻塞，无从插入。
        //
        //    b) **snapshot↔await 窗口**：上面第 2 步的入队是 `.await`，期间锁是放开的，
        //       可能发生并发增删（用户 cancel/新建）。因此这里仍按【稳定 id】二次校验：
        //       只推进「本次成功入队、且当前仍在表里、仍 due」的任务，其余原样保留，
        //       既不覆盖并发增删，也规避一次性任务删除带来的索引漂移。
        //
        //    关键区内的 `save_jobs_layout` 是同步磁盘 I/O、不含 `.await`，因此持有
        //    `std::sync::MutexGuard` 不会让 future 变成 `!Send`，可安全用于 tokio。
        {
            let mut guard = handle.lock().unwrap();
            let mut new_jobs: Vec<CronJob> = Vec::with_capacity(guard.len());
            for mut j in guard.iter().cloned() {
                // 只推进「本次成功入队、且当前仍 due」的任务；其余原样保留。
                let matched = dispatched.iter().find(|d| d.id == j.id).is_some()
                    && j.enabled
                    && j.next_run > 0
                    && j.next_run <= now;
                if matched {
                    let kind = j.kind;
                    let delete_after_run = j.delete_after_run;
                    if kind == CronKind::At && delete_after_run {
                        // 一次性且需删除：不入新表（等价于删除）。
                        continue;
                    }
                    // 推进 last_run / next_run（一次性不删除则禁用）。
                    j.last_run = now;
                    match kind {
                        CronKind::At => {
                            j.enabled = false;
                            j.next_run = 0;
                        }
                        CronKind::Every => {
                            if let Some(interval) = j.interval_s.filter(|s| *s > 0) {
                                j.next_run = now + i64::from(interval);
                            } else {
                                // 理论上非法 every 任务会在 load/add 边界被禁用或拒绝；这里是
                                // 运行时最后一道保险，防止其它代码直接改 jobs_handle 后制造
                                // interval_s=None/0 的任务。不能再用 unwrap_or(0)，否则 next_run
                                // 会被推进到 now，下一轮 tick 继续到期并反复触发。
                                tracing::warn!(
                                    "cron 周期任务缺少正数 interval_s，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::Daily => {
                            if let Some(clock) =
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty())
                            {
                                match next_daily_epoch_after(clock, now) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 每日任务 at_clock 非法，已在运行时禁用: id={} name={} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 每日任务缺少 at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::Weekly => {
                            if let (Some(weekday), Some(clock)) = (
                                j.weekday.filter(|d| (1..=7).contains(d)),
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_weekly_epoch_after(weekday, clock, now) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 每周任务配置非法，已在运行时禁用: id={} name={} weekday={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.weekday,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 每周任务缺少 weekday/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::Monthly => {
                            if let (Some(day), Some(clock)) = (
                                j.day_of_month.filter(|d| (1..=31).contains(d)),
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_monthly_epoch_after(day, clock, now) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 每月任务配置非法，已在运行时禁用: id={} name={} day_of_month={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.day_of_month,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 每月任务缺少 day_of_month/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::LunarMonthly => {
                            if let (Some(day), Some(clock)) = (
                                j.day_of_month.filter(|d| (1..=30).contains(d)),
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_lunar_monthly_epoch_after(day, clock, now) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 农历每月任务配置非法，已在运行时禁用: id={} name={} day_of_month={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.day_of_month,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 农历每月任务缺少 day_of_month/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::Yearly => {
                            if let (Some(month), Some(day), Some(clock)) = (
                                j.month,
                                j.day,
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_yearly_epoch_after(month, day, clock, now) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 每年任务配置非法，已在运行时禁用: id={} name={} month={:?} day={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.month,
                                            j.day,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 每年任务缺少 month/day/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::LunarYearly => {
                            if let (Some(month), Some(day), Some(clock)) = (
                                j.month,
                                j.day,
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_lunar_yearly_epoch_after(
                                    month,
                                    day,
                                    j.leap_month.unwrap_or(false),
                                    clock,
                                    now,
                                ) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 农历每年任务配置非法，已在运行时禁用: id={} name={} month={:?} day={:?} leap_month={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.month,
                                            j.day,
                                            j.leap_month,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 农历每年任务缺少 month/day/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::YearlyRange => {
                            if let (Some(month), Some(day), Some(clock)) = (
                                j.month,
                                j.day,
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_yearly_range_epoch_after(
                                    month,
                                    day,
                                    j.end_month,
                                    j.end_day,
                                    j.duration_days,
                                    clock,
                                    now,
                                ) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 每年区间任务配置非法，已在运行时禁用: id={} name={} month={:?} day={:?} end_month={:?} end_day={:?} duration_days={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.month,
                                            j.day,
                                            j.end_month,
                                            j.end_day,
                                            j.duration_days,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 每年区间任务缺少 month/day/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                        CronKind::LunarYearlyRange => {
                            if let (Some(month), Some(day), Some(clock)) = (
                                j.month,
                                j.day,
                                j.at_clock.as_deref().filter(|s| !s.trim().is_empty()),
                            ) {
                                match next_lunar_yearly_range_epoch_after(
                                    month,
                                    day,
                                    j.leap_month.unwrap_or(false),
                                    j.end_month,
                                    j.end_day,
                                    j.end_leap_month.unwrap_or(false),
                                    j.duration_days,
                                    clock,
                                    now,
                                ) {
                                    Ok(next) => j.next_run = next,
                                    Err(e) => {
                                        tracing::warn!(
                                            "cron 农历每年区间任务配置非法，已在运行时禁用: id={} name={} month={:?} day={:?} leap_month={:?} end_month={:?} end_day={:?} end_leap_month={:?} duration_days={:?} at_clock={:?} error={}",
                                            j.id,
                                            j.name,
                                            j.month,
                                            j.day,
                                            j.leap_month,
                                            j.end_month,
                                            j.end_day,
                                            j.end_leap_month,
                                            j.duration_days,
                                            j.at_clock,
                                            e
                                        );
                                        j.enabled = false;
                                        j.next_run = 0;
                                    }
                                }
                            } else {
                                tracing::warn!(
                                    "cron 农历每年区间任务缺少 month/day/at_clock，已在运行时禁用: id={} name={}",
                                    j.id,
                                    j.name
                                );
                                j.enabled = false;
                                j.next_run = 0;
                            }
                        }
                    }
                }
                new_jobs.push(j);
            }
            // 4) 写盘（持锁，同步 I/O、无 await）。无论写盘【成功与否】，都把推进后的
            //    new_jobs 替换进内存——这是修复「写盘失败导致每 tick 重复投递到期任务」的关键：
            //
            //    - 旧实现：写盘失败 → `return` 保持内存不变。但到期任务在步骤 2 已经入队
            //      inbound，内存里它们的 next_run 仍 <= now，于是【下一个 tick】会把【同一批
            //      任务再次入队】——磁盘故障期间，一次性提醒被反复执行、周期任务被反复触发。
            //
            //    - 现在：写盘失败也替换内存（next_run 已推进 / 一次性已禁用或删除），本进程
            //      【不会】再投递同一批任务；磁盘暂时落后于内存，待后续某次成功写盘（下个
            //      tick 重试、或用户 cron_add/remove 触发的持久化）时重新收敛。代价是：若进程
            //      在磁盘仍落后时【重启】，磁盘旧状态可能让任务多触发一次——这远好于「每 tick
            //      重复触发」，且磁盘故障本就是需要排查的异常，error 级告警会明确提示。
            //
            //    这与 P1「持锁贯穿 save+swap」并不冲突：P1 关心的是 save【成功】后并发
            //    add/remove 用旧快照互相覆盖的窗口，靠「关键区不释放锁」已消除；这里改的
            //    仅是 save【失败】时是否替换内存，关键区同样全程持锁，无新增覆盖窗口。
            //
            //    P2 补强（dirty 重试）：写盘失败时，【额外】把这份推进后的 new_jobs 登记为
            //    `dirty` 快照。本进程靠「内存已推进」杜绝了重复触发，但磁盘仍落后——若被
            //    删除/禁用的是一次性任务，它们此后不再到期，后续 tick 可能没有到期任务、
            //    不会再回到本保存路径。靠 `run` 循环里每个 tick 的 `flush_dirty`，这份 dirty
            //    快照会被独立反复重写落盘直到收敛，磁盘不再长期落后，重启后任务不再复活。
            //    写盘成功时则 `clear_dirty`：内存与磁盘已收敛，无需重试。
            if let Err(e) = save_jobs_layout(self.controller.layout(), &new_jobs) {
                tracing::error!(
                    "cron 调度状态写盘失败，内存已推进以防本进程重复触发；已登记 dirty 快照，\
                     将在后续 tick（含无到期任务的 tick）由 flush_dirty 独立重试落盘直到收敛: {e}"
                );
                // 登记 dirty：供 run 循环的 flush_dirty 重试（clone 一份，new_jobs 紧接着替换内存）。
                self.controller.mark_dirty(new_jobs.clone());
            } else {
                // 写盘成功：内存与磁盘已收敛，清除可能残留的旧 dirty 标记。
                self.controller.clear_dirty();
            }
            // 5) 无论写盘成败，都替换内存（仍在同一把锁内，无并发覆盖窗口）。
            *guard = new_jobs;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_store::Layout;

    fn daily_job() -> CronJob {
        CronJob {
            id: String::new(),
            name: "daily-order".into(),
            enabled: true,
            kind: CronKind::Daily,
            interval_s: None,
            at_epoch: None,
            at_clock: Some("11:00".into()),
            weekday: None,
            day_of_month: None,
            month: None,
            day: None,
            end_month: None,
            end_day: None,
            duration_days: None,
            leap_month: None,
            end_leap_month: None,
            message: "订饭".into(),
            channel: "napcat".into(),
            chat_id: "user:1".into(),
            last_run: 0,
            next_run: 0,
            delete_after_run: false,
        }
    }

    #[tokio::test]
    async fn process_due_daily_job_reschedules_to_calendar_clock() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        std::fs::create_dir_all(&layout.base).unwrap();
        let controller = Arc::new(CronController::load(layout).unwrap());
        controller.add(daily_job()).unwrap();

        let due_ts = Utc::now().timestamp() - 1;
        {
            let handle = controller.jobs_handle();
            let mut jobs = handle.lock().unwrap();
            jobs[0].next_run = due_ts;
        }

        let (tx, mut rx) = mpsc::channel(4);
        let service = CronService::new(controller.clone(), tx, 60);
        service.process_due_jobs().await;

        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(msg.is_cron_task());
        let dispatched_job_id = msg
            .cron_job_id()
            .expect("调度器投递的 Cron 必须携带 job id")
            .to_string();
        assert_eq!(msg.cron_job_name(), Some("daily-order"));
        assert_eq!(msg.channel, "napcat");
        assert_eq!(msg.chat_id, "user:1");
        assert!(msg.content.contains("daily-order"));
        assert!(msg.content.contains("订饭"));

        let jobs = controller.list_snapshot();
        assert_eq!(jobs.len(), 1);
        assert_eq!(dispatched_job_id, jobs[0].id);
        assert!(jobs[0].enabled);
        assert!(jobs[0].last_run > due_ts);
        assert!(jobs[0].last_run <= Utc::now().timestamp());
        assert!(jobs[0].next_run > Utc::now().timestamp());
        let next_local = chrono::DateTime::from_timestamp(jobs[0].next_run, 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(next_local.format("%H:%M").to_string(), "11:00");
    }
}
