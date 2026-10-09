//! cron 任务控制器：被 cron 工具与运行时调度器共享的同一份任务表。
//!
//! cron 任务表的共享数据层：`s_jobs` + load/save + add/remove
//! （周期扫描/触发放到 rclaw-runtime）。
//!
//! 共享模型：`Arc<Mutex<Vec<CronJob>>>` —— 工具（来自 LLM 工具调用线程）与
//! 调度器（后台 tick）操作同一份内存，变更后调用 [`CronController::persist`] 落盘。

use std::sync::{Arc, Mutex};

use chrono::Utc;
use rclaw_core::{CHANNEL_CLI, CHANNEL_SYSTEM, CHANNEL_WEIXIN};
use rclaw_store::cron_store::{
    load_jobs_layout, next_daily_epoch_after, next_lunar_monthly_epoch_after,
    next_lunar_yearly_epoch_after, next_lunar_yearly_range_epoch_after, next_monthly_epoch_after,
    next_weekly_epoch_after, next_yearly_epoch_after, next_yearly_range_epoch_after,
    save_jobs_layout, CronJob, CronKind,
};
use rclaw_store::fs::Layout;
use rclaw_store::StoreError;

/// 共享的 cron 任务表 + 持久化句柄。
#[derive(Clone)]
pub struct CronController {
    jobs: Arc<Mutex<Vec<CronJob>>>,
    layout: Layout,
    /// **待重试落盘的脏快照（修复 P2「写盘失败后内存推进无后续重试」）**。
    ///
    /// 语义：`Some(jobs)` 表示「内存已经推进到这一份状态、但磁盘 `cron.json` 仍落后
    /// （上一次 `save_jobs_layout` 失败），需要在不依赖到期任务的前提下独立重试落盘，
    /// 直到成功后置为 `None`」。`None` 表示内存与磁盘已收敛（或尚无未保存的推进）。
    ///
    /// 为什么必须独立重试：调度推进一次性任务（删除/禁用）后，若当时写盘失败，内存里
    /// 这些任务已经不在「到期」集合中，后续 tick 可能【没有任何到期任务】——于是不会
    /// 再进入 `process_due_jobs` 的保存路径，磁盘会长期停在旧状态，进程一旦重启，被
    /// 删除/禁用的一次性任务就会从旧磁盘「复活」。`flush_dirty` 由调度循环每个 tick
    /// 主动调用，把这份脏快照重写落盘，直到收敛，从而堵住这个窗口。
    ///
    /// 锁序约定：本结构内凡是需要同时拿两把锁，都恒按「`jobs` → `dirty`」的顺序获取，
    /// 绝不反向，因此不存在死锁。`flush_dirty` 也遵守同一规则，并且会持有 `jobs` 锁
    /// 贯穿 dirty 快照落盘，确保它不会和 `add`/`remove`/调度推进的落盘互相穿插。
    dirty: Arc<Mutex<Option<Vec<CronJob>>>>,
}

impl CronController {
    /// 从 cron.json 载入任务表（文件缺失则空表）。
    pub fn load(layout: Layout) -> Result<Self, StoreError> {
        let jobs = load_jobs_layout(&layout)?;
        Ok(CronController {
            jobs: Arc::new(Mutex::new(jobs)),
            layout,
            // 启动时磁盘即权威来源，无未保存推进。
            dirty: Arc::new(Mutex::new(None)),
        })
    }

    /// 直接拿到共享任务表句柄（供运行时调度器 tick 使用）。
    pub fn jobs_handle(&self) -> Arc<Mutex<Vec<CronJob>>> {
        self.jobs.clone()
    }

    pub fn layout(&self) -> &Layout {
        &self.layout
    }

    /// 把当前内存任务表整体写回 cron.json（原子写）。
    pub fn persist(&self) -> Result<(), StoreError> {
        let jobs = self.jobs.lock().unwrap();
        save_jobs_layout(&self.layout, &jobs)?;
        // `persist` 是公开方法，虽然当前主要写盘路径走 add/remove/runtime，但这里成功
        // 写入的是【当前内存完整状态】。若不清理旧 dirty，后续 flush_dirty 可能把更早的
        // 脏快照再次写回磁盘，造成状态倒退。
        self.clear_dirty();
        Ok(())
    }

    // -------------------------------------------------------------------------
    // P2：dirty（待重试落盘）快照管理
    //
    // 调度推进状态时若 `save_jobs_layout` 失败，必须把【内存已推进但未落盘】的那一份
    // 任务表登记下来，否则后续 tick 在没有到期任务时不会再次触发保存，磁盘旧状态长期
    // 不收敛、重启后一次性任务会「复活」（详见 [`CronController::dirty`] 字段注释）。
    // -------------------------------------------------------------------------

    /// 是否存在待重试落盘的 dirty 快照。
    pub fn has_dirty(&self) -> bool {
        self.dirty.lock().unwrap().is_some()
    }

    /// 把一份【内存已推进但尚未落盘】的任务表登记为 dirty，供后续 tick 重试落盘。
    ///
    /// 调用方通常是调度推进路径（`process_due_jobs`）：在写盘失败时，把推进后的
    /// `new_jobs`（内存即将替换成的版本）登记进来，保证它最终一定会被重写进磁盘。
    pub fn mark_dirty(&self, jobs: Vec<CronJob>) {
        *self.dirty.lock().unwrap() = Some(jobs);
    }

    /// 清除 dirty 标记：内存与磁盘已收敛，无需再重试落盘。
    ///
    /// 任何一次「成功落盘且该落盘内容已覆盖 dirty 快照」的操作都应调用本方法：
    /// `add`/`remove` 成功后（它们落盘的内容 ⊇ dirty 快照）、`process_due_jobs`
    /// 写盘成功后、`flush_dirty` 重试成功后。
    pub fn clear_dirty(&self) {
        *self.dirty.lock().unwrap() = None;
    }

    /// 重试落盘 dirty 快照（由调度循环每个 tick 在处理到期任务之前主动调用）。
    ///
    /// 返回值约定（**故意不抛 `Err`**，避免调度循环因磁盘故障反复打 error / 反复 panic）：
    /// - 无 dirty（`None`）→ 返回 `true`：已收敛，无需重试；
    /// - 有 dirty 且重试写盘成功 → 清除 dirty，返回 `true`；
    /// - 有 dirty 且重试写盘仍失败 → **保留** dirty 快照不动，返回 `false`，
    ///   下一个 tick 继续重试，直到磁盘恢复。
    ///
    /// 注意：这里会同时持有 `jobs` 与 `dirty` 锁直到落盘结束。这是刻意的：cron 的所有
    /// 写盘路径都必须被同一把 `jobs` 锁串行化，否则 dirty 旧快照可能和 add/remove 的
    /// 新状态交错写盘，最终把磁盘倒退回旧版本。dirty 锁也一并持有，避免外部直接调用
    /// `mark_dirty` 时在本次 flush 中途替换快照又被误清。
    pub fn flush_dirty(&self) -> bool {
        // 先拿 jobs，再拿 dirty，严格遵守字段注释里的全局锁序。
        // `_jobs_guard` 不直接读写任务表，它的作用是把本次 dirty 落盘纳入 cron 全局
        // 写盘串行区：add/remove/runtime 都必须先拿到同一把锁，因此无法在这里穿插写盘。
        let _jobs_guard = self.jobs.lock().unwrap();
        let mut dirty = self.dirty.lock().unwrap();
        let Some(jobs) = dirty.clone() else {
            return true;
        };
        // 持锁执行同步磁盘 I/O。这里牺牲一点并发性，换取「不会被旧快照倒写」的确定性。
        match save_jobs_layout(&self.layout, &jobs) {
            Ok(()) => {
                // 重试成功：清除的正是刚刚持锁写入的 dirty 快照。
                *dirty = None;
                tracing::info!("cron dirty 快照重试落盘成功，内存与磁盘已收敛");
                true
            }
            Err(e) => {
                // 磁盘仍未恢复：保留 dirty 快照（不动），下个 tick 再试。仅 warn，不抛错。
                tracing::warn!("cron dirty 快照重试落盘仍失败，保留 dirty 待下个 tick 重试: {e}");
                false
            }
        }
    }

    /// 任务表快照（克隆）。
    pub fn list_snapshot(&self) -> Vec<CronJob> {
        self.jobs.lock().unwrap().clone()
    }

    /// 新增一个任务：生成 id、规整目的地、计算 next_run，并入表持久化。
    /// 返回生成的 id。
    ///
    /// 两条正确性红线（均在同一把锁的关键区内完成）：
    ///
    /// 1. **先持久化、再改内存**：先把【新任务表】写盘成功，才把内存替换为新表。
    ///    写盘失败时调用者收到错误、内存保持原样，内存与 `cron.json` 永不分叉
    ///    （旧实现先 push 进内存再 persist，失败时内存里已多了任务、磁盘却没有，
    ///    重启后任务丢失或行为不一致）。
    ///
    /// 2. **持锁贯穿「生成新表 + save + 替换」**：消除旧实现「锁内克隆快照 → 释放锁去
    ///    写盘 → 重新加锁整体替换」的并发覆盖窗口。旧顺序里，写盘期间若有另一个任务
    ///    成功执行了 add/remove（它会基于【还没被本操作替换的旧内存】计算并落盘），
    ///    那么本操作写盘成功后用【写盘前的旧快照】整体替换内存，会把那次并发变更静默
    ///    覆盖丢失。现在关键区从读内存一直延伸到 save 成功并替换内存，期间锁不释放，
    ///    任何并发 add/remove 都被阻塞到本操作完成，无从插入。
    ///    （关键区内的 `save_jobs_layout` 是同步磁盘 I/O、不含 `.await`，因此持有
    ///    `std::sync::MutexGuard` 不会让 future 变成 `!Send`，可安全用于 tokio。）
    ///
    /// 3. **id 唯一性**：在持锁状态下生成并查重 8-hex id，碰撞则重试。
    pub fn add(&self, mut job: CronJob) -> Result<String, StoreError> {
        sanitize_destination(&mut job);
        job.enabled = true;
        job.last_run = 0;
        // Controller 是公共边界，不能只依赖 cron_add 工具层校验。外部调用方若直接传入
        // interval_s=None/0 的 Every 任务，旧逻辑会算出 next_run=now，运行时每个 tick
        // 都会认为它到期。这里在入表前统一拒绝不可调度任务。
        compute_initial_next_run(&mut job)?;

        // 持锁完成「生成唯一 id → 构造新表 → 写盘 → 替换内存」，整段无 await。
        let mut guard = self.jobs.lock().unwrap();

        // id 唯一性：8-hex 随机 id（32-bit）不查重时存在碰撞可能；一旦碰撞，`remove`
        // 与调度匹配会命中多个任务、行为混乱。这里在锁内反复生成直到与现有任务不冲突
        // （生日悖论下 4G 空间碰撞概率极低，循环几乎总是一次命中，重试仅作兜底）。
        job.id = loop {
            let candidate = CronJob::generate_id();
            if !guard.iter().any(|j| j.id == candidate) {
                break candidate;
            }
        };
        let id = job.id.clone();

        let mut new_jobs = guard.clone();
        new_jobs.push(job);
        // 先写盘：失败则内存不变（guard 仍在锁内、未被改写），直接把错误抛给调用者。
        save_jobs_layout(&self.layout, &new_jobs)?;
        // 写盘成功，才把内存整体替换为新表（仍在同一把锁内，无并发覆盖窗口）。
        *guard = new_jobs;
        // P2：本次成功落盘的内容（含全部内存状态 + 新任务）已【覆盖】任何遗留的 dirty
        // 快照——dirty 快照是更早的「未落盘推进」，现在磁盘已更新到更新的版本，旧的
        // dirty 不再需要重试，清除以防它把磁盘倒退回旧状态。
        self.clear_dirty();
        Ok(id)
    }

    /// 按 id 删除任务。
    ///
    /// 返回 `Ok(true)` 表示命中并已持久化；`Ok(false)` 表示无此任务（内存与磁盘都未改）；
    /// `Err(_)` 表示命中但持久化失败——此时内存**未**改动，保持与磁盘一致，
    /// 调用者可据此向用户报错或重试（旧实现直接吞掉 persist 错误，删除只发生在内存，
    /// 重启后任务会「复活」）。
    ///
    /// 与 [`add`](Self::add) 一致，「构造新表 → 写盘 → 替换内存」全程持同一把锁，
    /// 消除写盘期间并发 add/remove 用旧快照互相覆盖的窗口（P1）。
    pub fn remove(&self, id: &str) -> Result<bool, StoreError> {
        // 持锁完成「过滤新表 → 写盘 → 替换内存」，整段无 await，杜绝并发覆盖窗口。
        let mut guard = self.jobs.lock().unwrap();
        let before = guard.len();
        let new_jobs: Vec<CronJob> = guard.iter().filter(|j| j.id != id).cloned().collect();
        if new_jobs.len() == before {
            // 未命中：内存与磁盘都不改。
            return Ok(false);
        }
        // 命中：先写盘成功，再替换内存（同一把锁内）。
        save_jobs_layout(&self.layout, &new_jobs)?;
        *guard = new_jobs;
        // P2：与 add 同理，本次成功落盘已覆盖遗留 dirty 快照，清除之。
        self.clear_dirty();
        Ok(true)
    }
}

// ---------------------------------------------------------------------------
// 目的地规整
// ---------------------------------------------------------------------------

/// 规整任务的 channel/chat_id，避免落到无效目的地。
///
/// - channel 空 → system；
/// - channel=weixin 但 chat_id 非法（空或 "cron"）→ 回退 system:cron；
/// - channel=cli 但 chat_id 非法 → cli:cli；
/// - 其它 channel 且 chat_id 空 → cron。
fn sanitize_destination(job: &mut CronJob) {
    if job.channel.is_empty() {
        job.channel = CHANNEL_SYSTEM.into();
    }
    match job.channel.as_str() {
        ch if ch == CHANNEL_WEIXIN => {
            if job.chat_id.is_empty() || job.chat_id == "cron" {
                job.channel = CHANNEL_SYSTEM.into();
                job.chat_id = "cron".into();
            }
        }
        CHANNEL_CLI => {
            if job.chat_id.is_empty() || job.chat_id == "cron" {
                job.chat_id = CHANNEL_CLI.into();
            }
        }
        _ => {
            if job.chat_id.is_empty() {
                job.chat_id = "cron".into();
            }
        }
    }
}

/// 计算新建任务的首次触发时间。
///
/// 这是 `CronController::add` 的最后一道语义防线：工具层会先校验用户输入，但 controller
/// 本身也是公开 API，测试或其它模块可以绕过工具层直接构造 `CronJob`。因此这里不能使用
/// `unwrap_or(0)` 这类「看似兜底、实际会制造每 tick 到期任务」的默认值；缺少必要字段时
/// 必须返回错误，让调用方修正输入。
fn compute_initial_next_run(job: &mut CronJob) -> Result<(), StoreError> {
    let now = Utc::now().timestamp();
    match job.kind {
        CronKind::Every => {
            let Some(interval) = job.interval_s.filter(|s| *s > 0) else {
                return Err(StoreError::Other(
                    "周期 cron 任务需要正数 interval_s，已拒绝写入不可调度任务".into(),
                ));
            };
            let interval = i64::from(interval);
            job.next_run = now + interval;
        }
        CronKind::At => {
            let Some(at) = job.at_epoch else {
                return Err(StoreError::Other(
                    "一次性 cron 任务需要 at_epoch，已拒绝写入不可调度任务".into(),
                ));
            };
            if at > now {
                job.next_run = at;
            } else {
                return Err(StoreError::Other(
                    "一次性 cron 任务的 at_epoch 已过期，已拒绝写入不可调度任务".into(),
                ));
            }
        }
        CronKind::Daily => {
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "每日 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run = next_daily_epoch_after(clock, now).map_err(StoreError::Other)?;
        }
        CronKind::Weekly => {
            let Some(weekday) = job.weekday.filter(|d| (1..=7).contains(d)) else {
                return Err(StoreError::Other(
                    "每周 cron 任务需要 weekday=1..=7，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "每周 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run =
                next_weekly_epoch_after(weekday, clock, now).map_err(StoreError::Other)?;
        }
        CronKind::Monthly => {
            let Some(day) = job.day_of_month.filter(|d| (1..=31).contains(d)) else {
                return Err(StoreError::Other(
                    "每月 cron 任务需要 day_of_month=1..=31，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "每月 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run = next_monthly_epoch_after(day, clock, now).map_err(StoreError::Other)?;
        }
        CronKind::LunarMonthly => {
            let Some(day) = job.day_of_month.filter(|d| (1..=30).contains(d)) else {
                return Err(StoreError::Other(
                    "农历每月 cron 任务需要 day_of_month=1..=30，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "农历每月 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run =
                next_lunar_monthly_epoch_after(day, clock, now).map_err(StoreError::Other)?;
        }
        CronKind::Yearly => {
            let Some((month, day)) = job.month.zip(job.day) else {
                return Err(StoreError::Other(
                    "每年 cron 任务需要 month/day，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "每年 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run =
                next_yearly_epoch_after(month, day, clock, now).map_err(StoreError::Other)?;
        }
        CronKind::LunarYearly => {
            let Some((month, day)) = job.month.zip(job.day) else {
                return Err(StoreError::Other(
                    "农历每年 cron 任务需要 month/day，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "农历每年 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run = next_lunar_yearly_epoch_after(
                month,
                day,
                job.leap_month.unwrap_or(false),
                clock,
                now,
            )
            .map_err(StoreError::Other)?;
        }
        CronKind::YearlyRange => {
            let Some((month, day)) = job.month.zip(job.day) else {
                return Err(StoreError::Other(
                    "年度区间 cron 任务需要 month/day，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "年度区间 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run = next_yearly_range_epoch_after(
                month,
                day,
                job.end_month,
                job.end_day,
                job.duration_days,
                clock,
                now,
            )
            .map_err(StoreError::Other)?;
        }
        CronKind::LunarYearlyRange => {
            let Some((month, day)) = job.month.zip(job.day) else {
                return Err(StoreError::Other(
                    "农历年度区间 cron 任务需要 month/day，已拒绝写入不可调度任务".into(),
                ));
            };
            let Some(clock) = job.at_clock.as_deref().filter(|s| !s.trim().is_empty()) else {
                return Err(StoreError::Other(
                    "农历年度区间 cron 任务需要 at_clock，已拒绝写入不可调度任务".into(),
                ));
            };
            job.next_run = next_lunar_yearly_range_epoch_after(
                month,
                day,
                job.leap_month.unwrap_or(false),
                job.end_month,
                job.end_day,
                job.end_leap_month.unwrap_or(false),
                job.duration_days,
                clock,
                now,
            )
            .map_err(StoreError::Other)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_store::Layout;

    fn controller() -> CronController {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        std::fs::create_dir_all(&layout.base).unwrap();
        std::mem::forget(dir);
        // load 会因文件缺失返回空表。
        CronController::load(layout).unwrap()
    }

    #[test]
    fn add_list_remove_roundtrip() {
        let c = controller();
        let job = CronJob {
            id: String::new(),
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
            next_run: 0,
            delete_after_run: false,
        };
        let id = c.add(job).unwrap();
        assert_eq!(c.list_snapshot().len(), 1);
        assert!(c.remove(&id).unwrap());
        assert_eq!(c.list_snapshot().len(), 0);
    }

    /// Controller 本身是公共 API，必须拒绝绕过工具层校验的不可调度任务。
    #[test]
    fn add_rejects_invalid_every_job() {
        let c = controller();
        let job = CronJob {
            id: String::new(),
            name: "bad".into(),
            enabled: true,
            kind: CronKind::Every,
            interval_s: Some(0),
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
            message: "bad".into(),
            channel: "system".into(),
            chat_id: "cron".into(),
            last_run: 0,
            next_run: 0,
            delete_after_run: false,
        };
        let err = c.add(job).unwrap_err();
        assert!(
            err.to_string().contains("interval_s"),
            "应明确提示 interval_s 非法: {err}"
        );
        assert!(c.list_snapshot().is_empty(), "非法任务不得进入内存任务表");
    }

    /// P2：dirty 快照状态机——mark → has → flush（可写 layout）清除。
    #[test]
    fn dirty_snapshot_flush_clears_state() {
        let c = controller();
        assert!(!c.has_dirty(), "初始无 dirty");
        assert!(
            c.flush_dirty(),
            "无 dirty 时 flush_dirty 返回 true（视为已收敛）"
        );

        let job = CronJob {
            id: "abc00001".into(),
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
            message: "drink".into(),
            channel: "system".into(),
            chat_id: "cron".into(),
            last_run: 0,
            next_run: 100,
            delete_after_run: false,
        };
        c.mark_dirty(vec![job]);
        assert!(c.has_dirty(), "mark_dirty 后应处于 dirty");

        // flush_dirty 在可写 layout 上落盘成功 → 清除 dirty，返回 true。
        assert!(c.flush_dirty(), "可写 layout 上 flush_dirty 应成功");
        assert!(!c.has_dirty(), "flush 成功后 dirty 应被清除");
        // flush 只落盘 dirty 快照，不改动内存 jobs（实际流程里内存已等于该快照）。
        assert_eq!(c.list_snapshot().len(), 0, "flush 不应改动内存 jobs");
    }

    /// P2：成功的 add/remove 落盘会【覆盖】遗留 dirty 快照，必须清除 dirty，
    /// 否则 dirty 会在后续 tick 把磁盘倒退回更旧的快照。
    #[test]
    fn successful_add_and_remove_clear_dirty() {
        let c = controller();

        // 成功 add 覆盖 dirty。
        c.mark_dirty(vec![CronJob {
            id: "stale000".into(),
            name: "stale".into(),
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
            message: "stale".into(),
            channel: "system".into(),
            chat_id: "cron".into(),
            last_run: 0,
            next_run: 100,
            delete_after_run: false,
        }]);
        assert!(c.has_dirty());
        let job = CronJob {
            id: String::new(),
            name: "new".into(),
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
            next_run: 0,
            delete_after_run: false,
        };
        let id = c.add(job).unwrap();
        assert!(!c.has_dirty(), "成功 add 落盘应覆盖并清除 dirty");

        // 成功 remove 同样清除 dirty。
        c.mark_dirty(vec![CronJob {
            id: "stale001".into(),
            name: "stale2".into(),
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
            message: "stale2".into(),
            channel: "system".into(),
            chat_id: "cron".into(),
            last_run: 0,
            next_run: 100,
            delete_after_run: false,
        }]);
        assert!(c.has_dirty());
        assert!(c.remove(&id).unwrap());
        assert!(!c.has_dirty(), "成功 remove 落盘应覆盖并清除 dirty");
    }

    #[test]
    fn add_daily_job_aligns_initial_next_run() {
        let c = controller();
        let job = CronJob {
            id: String::new(),
            name: "daily-remind".into(),
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
        let id = c.add(job).unwrap();
        let jobs = c.list_snapshot();
        let saved = jobs.iter().find(|j| j.id == id).unwrap();
        assert_eq!(saved.kind, CronKind::Daily);
        assert_eq!(saved.at_clock.as_deref(), Some("23:59"));
        assert!(saved.next_run > chrono::Utc::now().timestamp());
        let local = chrono::DateTime::from_timestamp(saved.next_run, 0)
            .unwrap()
            .with_timezone(&chrono::Local);
        assert_eq!(local.format("%H:%M").to_string(), "23:59");
    }
}
