//! 一轮同步（§9.3）。
//!
//! 完整的一轮按固定顺序做：核对空间 → 换身份（有记着的话）→ 发布扫描 → 封段、上传 →
//! 拉取 → 合入 → 文件类数据的对比 → 快照与回收。只拉取的一轮（发送前、窗口获得焦点、
//! 打开会话）只做拉取与合入，有收获再约一轮完整的。
//!
//! 这里的代码都跑在引擎自己的线程上，由 [`super::worker`] 一轮一轮地调；两轮不会同时
//! 进行。数据库事务、文件读写、加解密放到阻塞线程池上（[`crate::blocking`]），网络请求
//! 期间不持库锁。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use rclaw_store::sync_db::{self, SEAL_SEALED};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

use super::local;
use super::space::Live;
use super::Shared;
use crate::attach::{self, now_ms, Backlog};
use crate::codec;
use crate::config::SyncConfig;
use crate::db::{self, scope, BLOB_REMOTE};
use crate::error::{Result, SyncError};
use crate::files;
use crate::identity::{self, Identity};
use crate::map;
use crate::merge::{self, MergeReport};
use crate::publish::{self, ScanReport, Settings};
use crate::pull;
use crate::remote;
use crate::seal::{self, SealOptions};
use crate::secrets::Secrets;
use crate::snapshot;
use crate::status;
use crate::trash;
use crate::upload::{self, UploadOptions, RUNNING_MAX};
use crate::wire;
use crate::{blocking, Ctx, SyncEvent};

/// `space.json` 每隔这么久重新核对一次：别的设备重置了同步空间的话，在这里发现。
const SPACE_CHECK_EVERY: Duration = Duration::from_secs(15 * 60);
/// 回收站里过了保留期的条目每隔这么久清一次（打开同步空间时先清过一次）。
const TRASH_SWEEP_EVERY: Duration = Duration::from_secs(24 * 3600);
/// 一轮里最多封、传这么多个段；还有剩的马上再来一轮。
const SEGMENTS_PER_ROUND: usize = 8;
/// 一个段的明文预算（字节）。
const SEGMENT_BUDGET: usize = 4 * 1024 * 1024;
const SEGMENT_BUDGET_LOW_MEMORY: usize = 1024 * 1024;
/// 一个汇总包的明文上限（字节）：打包和读包都要把它整个放进内存。
const PACK_PLAIN_MAX: usize = 32 * 1024 * 1024;
const PACK_PLAIN_MAX_LOW_MEMORY: usize = 8 * 1024 * 1024;
/// 存储「被回退或清空」连续这么多轮才报错（§9.6：它可能只是有延迟）。
const REGRESSED_ROUNDS: u32 = 3;
/// 加入方一直没见到别的设备时，等这么久再发布本机存量（§8.8）。
const PRIME_WAIT: Duration = Duration::from_secs(10 * 60);
/// 某台设备的日志读不下来时，为它最多等这么久再发布本机存量。
const PRIME_PEER_WAIT_MS: i64 = 10 * 60 * 1000;
/// 吃过快照、换过身份之后的那次清点（§7.5）碰上会话正忙时，最多等这么久；过了就不再
/// 拦着发布——没清点完的线程记着，空下来照样清点。
const STALE_BUSY_WAIT: Duration = Duration::from_secs(10 * 60);
/// 文件类数据每隔这么久做一次不看指纹缓存的完整对比。
const FILES_FORCE_EVERY: Duration = Duration::from_secs(10 * 60);
/// 文件对比有事情没做成（文件正在被改、有 turn 在跑）时，隔这么久重试，最多连着试几次。
const FILES_RETRY_AFTER: Duration = Duration::from_secs(5);
const FILES_RETRY_MAX: u32 = 3;
/// 文件对比要先取回附件（技能里的大文件）时，一轮里最多来回这么多趟。
const FILES_PASSES: usize = 3;
/// 快照每轮写这么久，然后让出引擎；没写完的隔这么久接着写。
const SNAP_STEP: Duration = Duration::from_secs(5);
const SNAP_CONTINUE: Duration = Duration::from_millis(200);
/// 自动下载的小图（§8.7）：大小上限、一轮里最多排多少个。
const AUTO_FETCH_MAX: u64 = 1024 * 1024;
const AUTO_FETCH_PER_ROUND: usize = 32;
const IMAGE_SUFFIXES: [&str; 6] = [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"];
/// 向别的设备要过、存储上还没有的附件：隔这么久再去取一次，一轮里最多试这么多个。
/// 间隔一路拉长到一天一次——等的是另一台设备上线，那可能是很多天以后的事；等到头还
/// 没有就收场（[`attach::on_missing`]），不再来取。
const WANT_BACKOFF: [Duration; 6] = [
    Duration::from_secs(30),
    Duration::from_secs(2 * 60),
    Duration::from_secs(10 * 60),
    Duration::from_secs(3600),
    Duration::from_secs(6 * 3600),
    Duration::from_secs(24 * 3600),
];
/// 有设备说过它拿不出来的：只再等一小会儿（看别的设备有没有同样的一份），间隔到这
/// 一档为止——等够了的那一次取不到就收场，不能让它排到几个小时以后。
const WANT_BACKOFF_GONE_MAX: usize = 2;
const WANTS_PER_ROUND: usize = 8;
/// 发现本机有新消息之后等这么久再发布（§7.1：turn 结束后去抖）。
pub(crate) const PUBLISH_DEBOUNCE: Duration = Duration::from_secs(3);

const METERED_PROMPT: &str =
    "现在用的是按流量计费的网络。第一次同步要传的内容可能比较多，确认之后才会开始";
const ROTATED_TITLE: &str = "本机换用了新的同步身份";
const ROTATED_DETAIL: &str =
    "同步时发现，存储上记在本机名下的同步数据和本机的对不上：多半是本机的数据库或\
     数据目录被还原、复制过。为了不弄乱其他设备上的内容，本机已经换用一个新的同步身份，并\
     把本机的内容重新发布了一遍，不需要你做什么。如果确实有另一份同样的数据目录也在同步，\
     两份数据从此是两台独立的设备。";

/// 一轮的结果。
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// 这一轮有进展，而且还有活要接着干：马上再来一轮。
    pub again: bool,
    /// 过这么久再来一轮（快照写到一半、文件对比要重试）。
    pub retry_after: Option<Duration>,
    /// 只拉取的一轮有了收获：要补一轮完整的（应答、对比文件、取小图）。
    pub follow_up: bool,
}

impl Outcome {
    fn retry_in(&mut self, after: Duration) {
        self.retry_after = Some(self.retry_after.map_or(after, |old| old.min(after)));
    }
}

/// 会话同步的范围（kv `sync/range`）。范围变宽时，之前因为不在范围内而跳过的会话 op 要
/// 重新过一遍。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Range {
    #[serde(default)]
    sessions: bool,
    #[serde(default)]
    channels: Vec<String>,
    #[serde(default)]
    attachments: bool,
}

impl Range {
    fn of(s: &Settings) -> Range {
        let mut channels = if s.scopes.sessions {
            s.channels.clone()
        } else {
            Vec::new()
        };
        channels.sort();
        channels.dedup();
        Range {
            sessions: s.scopes.sessions,
            channels,
            attachments: s.scopes.attachments,
        }
    }

    /// 比 `old` 多出了要同步的会话（会话同步从关到开，或多了通道）。
    fn widens(&self, old: &Range) -> bool {
        self.channels
            .iter()
            .any(|channel| !old.channels.contains(channel))
    }
}

/// 一轮开头从库里读出来的标记。
struct Flags {
    /// 首次同步已经做完（§8.8）。
    initial: bool,
    /// 计费网络下的首次同步得到过确认。
    metered_ok: bool,
    /// 空间里现有的内容已经拉取、合入过一遍：可以发布本机存量了。
    primed: bool,
    /// 这个空间是本机建的。
    creator: bool,
    /// 用户选过的存量附件上传范围。
    backlog: Option<Backlog>,
    range: Option<Range>,
}

fn read_flags(conn: &Connection, space: &str) -> Result<Flags> {
    let is_space = |key: &str| -> Result<bool> {
        Ok(db::kv_json::<String>(conn, key)?.as_deref() == Some(space))
    };
    Ok(Flags {
        initial: is_space(db::KV_INITIAL)?,
        metered_ok: is_space(db::KV_METERED_OK)?,
        primed: db::kv_json::<bool>(conn, db::KV_PRIMED)? == Some(true),
        creator: is_space(db::KV_CREATOR)?,
        backlog: db::kv_json(conn, db::KV_BACKLOG)?,
        range: db::kv_json(conn, db::KV_RANGE)?,
    })
}

/// 同步范围变宽了：各设备的日志从头再读一遍。读过的位置、收件箱里还留着的 op、「跳过
/// 了哪些线程」的记录都清掉；已经合入的内容再读到时靠幂等消化。日志开头已被回收的设备
/// 会转去找全量快照（§9.5）。
fn reread_logs(conn: &Connection) -> Result<()> {
    for (device, mut peer) in db::peers(conn)? {
        // 本机用过的旧身份：自己发的那一段仍然不必取回。
        peer.fetched = peer.own_until.unwrap_or(0);
        peer.fetched_seg = None;
        peer.from_snap = false;
        peer.pull_incomplete = true;
        peer.processed = peer.fetched;
        peer.head_tag.clear();
        peer.halted = None;
        peer.error = None;
        peer.error_rounds = 0;
        peer.error_since_ms = 0;
        db::put_peer_state(conn, &device, &peer)?;
    }
    conn.execute(
        "DELETE FROM sync_state WHERE scope IN (?1, ?2, ?3)",
        params![scope::RSEG, scope::SKIP, scope::INSEG],
    )?;
    conn.execute("DELETE FROM sync_inbox", [])?;
    publish::reset_cursor(conn)
}

/// 在阻塞线程池上跑一段只碰数据库和本机文件的活。
async fn on_pool<T, F>(ctx: &Arc<Ctx>, f: F) -> Result<T>
where
    F: FnOnce(&Ctx) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    let ctx = ctx.clone();
    blocking(move || f(&ctx)).await
}

fn task_failed(error: tokio::task::JoinError) -> SyncError {
    SyncError::other(format!("后台任务失败: {error}"))
}

/// 各设备的日志是不是都读到头了：取回的都处理过（或那份日志停住了），而且读得下来。
/// 读不下来的设备，对象可能只是还没到齐，等一阵；一直读不到就不等它了。
fn logs_read(peers: &[(String, db::PeerState)], now: i64) -> bool {
    peers.iter().all(|(_, peer)| {
        let read = peer.halted.is_some() || peer.processed >= peer.fetched;
        let reachable = peer.error_since_ms == 0 || now - peer.error_since_ms >= PRIME_PEER_WAIT_MS;
        let complete = !peer.pull_incomplete || !peer.is_active(now);
        read && reachable && complete
    })
}

/// 能不能开始发布本机存量：本机建的空间不用等；加入的空间要等现有内容都拉取、合入过
/// 一遍（§8.8）——这一轮的拉取没有剩的（`more` 为假），各设备的日志都读到头了。一台别
/// 的设备都没见到时，可能只是它们的文件还没同步过来，等够了（`waited_long`）才放行。
///
/// 本机吃过全量快照、换过身份之后还没清点（[`Session::sweep_stale`]）时不放行，本机建的
/// 空间也一样：手里那些别处已经删掉的分支的旧样子不能当成存量再发出去。
fn may_prime(
    conn: &Connection,
    creator: bool,
    more: bool,
    waited_long: bool,
    now: i64,
) -> Result<bool> {
    if db::kv_json::<bool>(conn, db::KV_STALE_CHECK)? == Some(true) {
        return Ok(false);
    }
    if creator {
        return Ok(true);
    }
    if more {
        return Ok(false);
    }
    let peers = db::peers(conn)?;
    Ok(logs_read(&peers, now) && (!peers.is_empty() || waited_long))
}

/// 一次拉取（连同快照补齐）的结果。
struct Pulled {
    /// 写进收件箱的 op 数。
    ops: usize,
    /// 还有没取完的。
    more: bool,
    /// 拉取之后又吃了全量快照：快照之后的那段日志这一轮还没读。
    booted: bool,
}

/// 引擎连着一个同步空间期间的状态：从打开到换配置、退出空间或引擎停止为止。
pub(crate) struct Session {
    pub live: Arc<Live>,
    pub identity: Identity,
    /// 已经完整地跑完过一轮。
    pub warm: bool,
    /// 下一轮做全量的：所有会话重新扫描，悬空区、墓碑全部复查，文件类数据不看缓存。
    pub full: bool,
    /// 下一轮不等回执攒够时间就写 head（转入后台、手动「立即同步」）。
    pub force_head: bool,
    space_checked: Option<Instant>,
    /// 上次清理回收站的时刻（打开同步空间时清过一次，从那时算起）。
    trash_swept: Instant,
    /// 上次运行写到一半的快照已经清理过。
    cleaned: bool,
    merge_memo: merge::Memo,
    snap_memo: snapshot::Memo,
    pull_memo: pull::Memo,
    files_cache: files::Cache,
    files_forced: Option<Instant>,
    files_retry: u32,
    writer: Option<snapshot::Writer>,
    /// 存储「被回退或清空」已经连续多少轮。
    regressed: u32,
    /// 第一次检查「能不能发布本机存量」的时刻。
    prime_wait: Option<Instant>,
    /// 吃过快照、换过身份之后的那次清点第一次碰上会话正忙的时刻。
    stale_wait: Option<Instant>,
    /// 「存量附件要不要问用户」这次运行里已经看过。
    backlog_checked: bool,
    /// 已经自动排过下载的小图（每个只排一次）。
    auto_fetched: HashSet<String>,
    /// 点名要过的附件：已经重试的次数、上次重试的时刻。
    want_tries: HashMap<String, (usize, Instant)>,
    /// 本机变更检查：上次看到的消息表最大行 id；上次有没有会话因为正忙而搁下。
    seen_max: i64,
    busy_pending: bool,
    /// 上次看到的「运行了一阵的 turn」所在的会话。
    running_seen: Vec<(String, String)>,
}

impl Session {
    pub(crate) fn new(live: Live, identity: Identity) -> Session {
        Session {
            live: Arc::new(live),
            identity,
            warm: false,
            full: true,
            force_head: false,
            space_checked: None,
            trash_swept: Instant::now(),
            cleaned: false,
            merge_memo: merge::Memo::default(),
            snap_memo: snapshot::Memo::default(),
            pull_memo: pull::Memo::default(),
            files_cache: files::Cache::default(),
            files_forced: None,
            files_retry: 0,
            writer: None,
            regressed: 0,
            prime_wait: None,
            stale_wait: None,
            backlog_checked: false,
            auto_fetched: HashSet::new(),
            want_tries: HashMap::new(),
            seen_max: -1,
            busy_pending: false,
            running_seen: Vec::new(),
        }
    }

    pub(crate) fn settings(&self, shared: &Shared, config: &SyncConfig) -> Settings {
        local::settings_of(
            config,
            shared.sessions_supported,
            self.identity.device.clone(),
            self.identity.my_ids.clone(),
        )
    }

    async fn flags(&self, ctx: &Arc<Ctx>) -> Result<Flags> {
        let space = self.live.space_id.clone();
        on_pool(ctx, move |ctx| {
            db::read(ctx.db(), |conn| read_flags(conn, &space))
        })
        .await
    }

    // -- 完整的一轮 ---------------------------------------------------------

    pub(crate) async fn full_round(
        &mut self,
        shared: &Shared,
        config: &SyncConfig,
        secrets: &mut Secrets,
    ) -> Result<Outcome> {
        let ctx = &shared.ctx;
        let live = self.live.clone();
        let (transport, keys) = (live.transport(), &live.keys);
        let mut out = Outcome::default();
        let mut status_changed = false;
        let mut s = self.settings(shared, config);

        // 计费网络下的首次同步要先得到确认（§8.8）：在发任何请求之前查。
        let mut flags = self.flags(ctx).await?;
        if !flags.initial && !flags.metered_ok && ctx.host.is_metered() {
            return Err(SyncError::NeedsUser(METERED_PROMPT.into()));
        }

        // 空间还在不在。
        if self
            .space_checked
            .is_none_or(|at| at.elapsed() >= SPACE_CHECK_EVERY)
        {
            remote::open_space(transport, keys, &live.space_id, false).await?;
            self.space_checked = Some(Instant::now());
        }
        if !self.cleaned && self.writer.is_none() {
            match snapshot::cleanup(ctx, transport, &self.identity.device).await {
                Ok(()) => self.cleaned = true,
                Err(error) => tracing::debug!("清理上次没写完的快照失败（下一轮再试）: {error}"),
            }
        }

        // 记着「要换身份」（§9.6）：先换，再做别的。换身份会清掉「已经拉取合入过一遍」
        // 的标记，所以标记要重读。
        let todo = on_pool(ctx, |ctx| db::read(ctx.db(), identity::rotation_todo)).await?;
        if let Some(todo) = todo {
            self.rotate(shared, &s, secrets, &todo.reason).await?;
            s = self.settings(shared, config);
            flags = self.flags(ctx).await?;
            status_changed = true;
        }
        self.apply_range(ctx, &s, &mut flags).await?;
        // 新登记的附件直接排队上传：附件同步开着、首次同步做完、用户选过存量的范围。
        let fresh = s.scopes.attachments && flags.initial && flags.backlog.is_some();

        // 1. 发布扫描。空间里现有的内容拉取合入过一遍之后才发布本机存量。
        if self.full {
            let (s, platform, version) =
                (s.clone(), shared.platform.clone(), shared.version.clone());
            on_pool(ctx, move |ctx| {
                identity::announce(ctx, &s, &platform, &version)
            })
            .await?;
        }
        let scan = if flags.primed {
            let (s, full) = (s.clone(), self.full);
            on_pool(ctx, move |ctx| publish::scan_sessions(ctx, &s, full)).await?
        } else {
            ScanReport::default()
        };
        // 有会话正忙被搁下：两轮之间的检查要盯着它，空下来就再扫。
        self.busy_pending |= scan.skipped_busy;

        // 2–7. 封段、落盘点、身份自检、上传段、写 head、记账。
        let running = self.running_sids(ctx, &s).await?;
        let (budget, pack_plain_max) = if shared.low_memory {
            (SEGMENT_BUDGET_LOW_MEMORY, PACK_PLAIN_MAX_LOW_MEMORY)
        } else {
            (SEGMENT_BUDGET, PACK_PLAIN_MAX)
        };
        let mut regressed: Option<String> = None;
        for turn in 0..SEGMENTS_PER_ROUND {
            let sealed = {
                let (keys, device, attachment_max) =
                    (keys.clone(), s.device.clone(), s.attachment_max);
                on_pool(ctx, move |ctx| {
                    seal::seal_next(
                        ctx,
                        &SealOptions {
                            keys: &keys,
                            device: &device,
                            budget,
                            attachment_max,
                            upload_attachments: fresh,
                        },
                    )
                })
                .await?
            };
            if sealed.is_some() {
                shared.set_state("syncing", None);
            }
            let report = upload::upload_round(
                ctx,
                transport,
                &UploadOptions {
                    keys,
                    device: &s.device,
                    instance: &self.identity.instance,
                    instances: &self.identity.instances,
                    running: running.clone(),
                    pack_plain_max,
                    rollup_every: codec::ROLLUP_EVERY,
                    force_head: self.force_head,
                },
            )
            .await?;
            if let Some(reason) = report.rotate {
                // 记下来，下一轮开头去换。
                on_pool(ctx, move |ctx| {
                    db::write(ctx.db(), |tx| identity::request_rotation(tx, &reason, &[]))
                })
                .await?;
                out.again = true;
                return Ok(out);
            }
            if let Some(reason) = report.regressed {
                self.regressed += 1;
                regressed = Some(reason);
                break;
            }
            if report.head_written {
                self.regressed = 0;
                self.force_head = false;
            }
            if sealed.is_none() && report.uploaded.is_none() {
                break;
            }
            // 队列里多半还有：这一轮的名额用完了，或者有调用在等引擎回话，都留给下一轮。
            if turn + 1 == SEGMENTS_PER_ROUND || shared.interrupted() {
                out.again = true;
                break;
            }
        }

        // 8. 拉取。还没读过任何设备的日志时，先试着从最新的全量快照起步（§9.5）。
        if !self.snap_memo.tried_new {
            let boot =
                snapshot::bootstrap_new(ctx, transport, keys, &s, &mut self.snap_memo).await?;
            if boot.applied > 0 {
                shared.set_state("syncing", None);
            }
        }
        let pulled = self.pull(shared, &s, &mut out).await?;

        // 9. 合入。
        let merged = self.merge(shared, &s, self.full).await?;
        status_changed |= merged.status_changed;
        if merged.rotate {
            out.again = true;
        }
        if self.full || merged.applied > 0 {
            let (s, keys) = (s.clone(), keys.clone());
            let answered =
                on_pool(ctx, move |ctx| merge::answer_wants(ctx, &s, &keys, fresh)).await?;
            if answered > 0 {
                out.again = true;
            }
            on_pool(ctx, |ctx| wire::settle_aliases(&ctx.env, None)).await?;
        }

        // 各设备的日志都读到头了：清点别处已经删掉的分支留在本机的旧样子（§7.5）。
        self.sweep_stale(shared, &s, &pulled).await?;

        // 空间里现有的内容都拉取合入过了：从下一轮起发布本机存量。
        let mut primed = flags.primed;
        if !primed && self.try_prime(ctx, flags.creator, pulled.more).await? {
            primed = true;
            self.full = true;
            out.again = true;
        }

        // 文件类数据（记忆、人格、技能……）：本机变更排队发布，远端变更落到本机文件。
        if primed {
            self.reconcile(shared, &s, &mut out, &mut status_changed)
                .await?;
        }

        // 首次同步收尾（§8.8）。发布扫描这一轮跑过、没有会话被搁下、本机的东西都传完了。
        let mut initial = flags.initial;
        if !initial && flags.primed && !scan.skipped_busy && !pulled.more {
            let space = live.space_id.clone();
            initial = on_pool(ctx, move |ctx| {
                db::write(ctx.db(), |tx| {
                    if map::outbox_len(tx)? > 0 || db::log_state(tx)?.pending.is_some() {
                        return Ok(false);
                    }
                    db::kv_put_json(tx, db::KV_INITIAL, &space)?;
                    Ok(true)
                })
            })
            .await?;
            if initial {
                status_changed = true;
                self.backlog_checked = false;
                // 存量附件的范围在首次同步做完之前就选好了：现在照它排队。
                if let (true, Some(choice)) = (s.scopes.attachments, flags.backlog) {
                    on_pool(ctx, move |ctx| {
                        attach::queue_backlog(ctx, choice).map(|_| ())
                    })
                    .await?;
                }
            }
        }
        if s.scopes.attachments && initial && flags.backlog.is_none() && !self.backlog_checked {
            self.backlog_checked = true;
            // 本机没有存量附件：不用问，以后新登记的直接上传。有的话面板上会问。
            on_pool(ctx, |ctx| {
                if attach::backlog_size(ctx)? == (0, 0) {
                    db::write(ctx.db(), |tx| {
                        db::kv_put_json(tx, db::KV_BACKLOG, &Backlog::None)
                    })?;
                }
                Ok(())
            })
            .await?;
            status_changed = true;
        }

        // 附件：叫醒上传任务；小图自动取回；点名要过的隔一阵再去取。
        shared.uploads.notify_one();
        let wants = shared.wants.swap(false, Ordering::Relaxed);
        if self.full || merged.applied > 0 || wants || !self.want_tries.is_empty() {
            self.fetch_attachments(shared, &s, self.full || merged.applied > 0)
                .await?;
        }

        // 全量快照与回收（§9.5）。它们出错不算这一轮失败。
        if primed && initial {
            // 这一轮的拉取还有剩的：手里的内容不是最新的，不在这时候开始写新的快照。
            self.write_snapshot(shared, &s, fresh, !pulled.more, &mut out)
                .await;
            match snapshot::collect_garbage(
                ctx,
                transport,
                keys,
                &s,
                &mut self.snap_memo,
                now_ms(),
                shared.low_memory,
            )
            .await
            {
                // 回收位置前移了：带着新位置的 head 要写出去。
                Ok(report) => out.again |= report.floor.is_some(),
                Err(error) => tracing::debug!("回收同步数据失败（下次再试）: {error}"),
            }
        }

        // 回收站里过了保留期的条目：应用一直开着的设备上，每天清一次。清不成不算这一轮
        // 失败。
        if self.trash_swept.elapsed() >= TRASH_SWEEP_EVERY {
            self.trash_swept = Instant::now();
            let keep_days = s.trash_days;
            match on_pool(ctx, move |ctx| Ok(trash::sweep(&ctx.paths, keep_days))).await {
                Ok(0) => {}
                Ok(removed) => {
                    tracing::info!("回收站里 {removed} 个过了保留期的条目已清理");
                    status_changed = true;
                }
                Err(error) => tracing::debug!("清理回收站失败（下次再试）: {error}"),
            }
        }

        self.full = false;
        self.warm = true;
        shared.mark_synced();
        if status_changed {
            shared.emit(SyncEvent::Status);
        }
        // 存储被回退或清空：别的都照常做完，最后才报。
        if let Some(reason) = regressed {
            if self.regressed >= REGRESSED_ROUNDS {
                return Err(SyncError::other(format!(
                    "存储上的同步数据被回退或清空了（{reason}）。本机已经停止上传，不会自动修复；\
                     存储恢复之后会自己接上，恢复不了的话请用「重置同步空间」"
                )));
            }
        }
        Ok(out)
    }

    // -- 只拉取的一轮 -------------------------------------------------------

    pub(crate) async fn pull_round(
        &mut self,
        shared: &Shared,
        config: &SyncConfig,
    ) -> Result<Outcome> {
        let mut out = Outcome::default();
        let s = self.settings(shared, config);
        let pulled = self.pull(shared, &s, &mut out).await?;
        let merged = self.merge(shared, &s, false).await?;
        if merged.status_changed {
            shared.emit(SyncEvent::Status);
        }
        out.follow_up =
            out.again || pulled.more || merged.applied > 0 || merged.dirty.any() || merged.rotate;
        out.again = false;
        Ok(out)
    }

    // -- 各步 ---------------------------------------------------------------

    /// 换身份（§9.6）。
    async fn rotate(
        &mut self,
        shared: &Shared,
        s: &Settings,
        secrets: &mut Secrets,
        reason: &str,
    ) -> Result<()> {
        let live = self.live.clone();
        // 正在写的快照是旧身份名下的：不要了。
        if let Some(writer) = self.writer.take() {
            writer.abort(&shared.ctx, live.transport()).await;
        }
        let rotated = identity::rotate(
            &shared.ctx,
            live.transport(),
            &live.keys,
            s,
            secrets,
            &self.identity,
            reason,
        )
        .await?;
        self.identity = rotated.identity;
        self.merge_memo = merge::Memo::default();
        self.snap_memo = snapshot::Memo::default();
        // 本机的会话都在，不需要「新设备从快照起步」。
        self.snap_memo.tried_new = true;
        self.files_cache.clear();
        self.full = true;
        self.regressed = 0;
        self.prime_wait = None;
        // 给用户留句话。写不进去只打日志（`notice` 自己管），不算换身份失败。
        let old = rotated.old;
        let _ = on_pool(&shared.ctx, move |ctx| {
            status::notice(&ctx.paths, "rotated", &old, ROTATED_TITLE, ROTATED_DETAIL);
            Ok(())
        })
        .await;
        Ok(())
    }

    /// 记下这一轮用的同步范围；比上次宽了就把各设备的日志从头再读。
    async fn apply_range(&mut self, ctx: &Arc<Ctx>, s: &Settings, flags: &mut Flags) -> Result<()> {
        let now = Range::of(s);
        if flags.range.as_ref() == Some(&now) {
            return Ok(());
        }
        let old = flags.range.take();
        let widened = old.as_ref().is_some_and(|old| now.widens(old));
        // 附件同步从关到开：存量附件的范围重新问一次。
        let attachments_on = old
            .as_ref()
            .is_some_and(|old| !old.attachments && now.attachments);
        let stored = now.clone();
        on_pool(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                if widened {
                    reread_logs(tx)?;
                }
                if attachments_on {
                    sync_db::kv_del(tx, db::KV_BACKLOG)?;
                }
                db::kv_put_json(tx, db::KV_RANGE, &stored)
            })
        })
        .await?;
        if widened {
            self.full = true;
        }
        if attachments_on {
            flags.backlog = None;
            self.backlog_checked = false;
        }
        flags.range = Some(now);
        Ok(())
    }

    /// 运行提示（§7.4）：turn 已经跑了一阵的会话里，参与同步的那些的线程 id。
    async fn running_sids(&mut self, ctx: &Arc<Ctx>, s: &Settings) -> Result<Vec<String>> {
        let mut running = ctx.gate.running_sessions();
        running.sort();
        running.dedup();
        self.running_seen = running.clone();
        if running.is_empty() || !s.scopes.sessions {
            return Ok(Vec::new());
        }
        let s = s.clone();
        on_pool(ctx, move |ctx| {
            db::read(ctx.db(), |conn| {
                let mut sids = Vec::new();
                for (channel, chat_id) in &running {
                    if !publish::eligible(conn, &s, channel, chat_id)? {
                        continue;
                    }
                    if let Some(session) = map::session_map_by_key(conn, channel, chat_id)? {
                        if session.sealed == SEAL_SEALED {
                            sids.push(session.sid);
                        }
                    }
                }
                sids.sort();
                sids.dedup();
                sids.truncate(RUNNING_MAX);
                Ok(sids)
            })
        })
        .await
    }

    /// 拉取（§9.3 第 8 步），日志开头已被回收的设备顺带用全量快照补齐。
    async fn pull(&mut self, shared: &Shared, s: &Settings, out: &mut Outcome) -> Result<Pulled> {
        let ctx = &shared.ctx;
        let live = self.live.clone();
        let (transport, keys) = (live.transport(), &live.keys);
        let budget = if shared.low_memory {
            pull::PULL_BUDGET_LOW_MEMORY
        } else {
            pull::PULL_BUDGET
        };
        let report =
            pull::pull(ctx, transport, keys, &s.device, budget, &mut self.pull_memo).await?;
        shared.last_pull_ms.store(now_ms(), Ordering::Relaxed);
        // 存储够得着：附件的上传不必等这一轮跑完。
        shared.storage_ok.store(true, Ordering::Relaxed);
        let mut pulled = Pulled {
            ops: report.ops,
            more: report.more,
            booted: false,
        };
        if pulled.more {
            out.again = true;
        }
        if on_pool(ctx, |ctx| db::read(ctx.db(), snapshot::needs_bootstrap)).await? {
            let boot = snapshot::bootstrap(ctx, transport, keys, s, &mut self.snap_memo).await?;
            pulled.ops += boot.ops;
            // 吃完快照，接着它之后的日志要再拉取一轮。
            pulled.booted = boot.applied > 0;
            out.again |= pulled.booted;
        }
        if pulled.ops > 0 {
            shared.set_state("syncing", None);
        }
        Ok(pulled)
    }

    /// 合入（§9.3 第 9 步），并把结果通知宿主。
    async fn merge(&mut self, shared: &Shared, s: &Settings, full: bool) -> Result<MergeReport> {
        let ctx = &shared.ctx;
        let memo = std::mem::take(&mut self.merge_memo);
        let (pool_ctx, pool_s) = (ctx.clone(), s.clone());
        let (merged, memo) = tokio::task::spawn_blocking(move || {
            let mut memo = memo;
            let merged = merge::merge_round(&pool_ctx, &pool_s, &mut memo, full);
            (merged, memo)
        })
        .await
        .map_err(task_failed)?;
        self.merge_memo = memo;
        let merged = merged?;
        if merged.applied > 0 {
            // 「已处理」的位置前移了：回执要写进 head（没有新段的话最多攒 10 分钟）。
            on_pool(ctx, |ctx| {
                db::write(ctx.db(), |tx| {
                    let mut log = db::log_state(tx)?;
                    if !log.head_dirty {
                        log.head_dirty = true;
                        db::put_log_state(tx, &log)?;
                    }
                    Ok(())
                })
            })
            .await?;
        }
        if !merged.chats.is_empty() || merged.list_changed {
            shared.emit(SyncEvent::Sessions {
                chats: merged.chats.iter().cloned().collect(),
                list: merged.list_changed,
            });
        }
        Ok(merged)
    }

    /// 清点「别处已经删掉的分支更早的样子」（§7.5）：合入时记下的线程，以及——本机吃过
    /// 全量快照、换过身份之后——所有带链的墓碑。只在各设备的日志都读到头的时候做：这一
    /// 轮的拉取没有剩的、不是刚吃完快照（它之后的日志还没读），取回的也都处理过了。
    ///
    /// 「吃过快照 / 换过身份」的记号在清点完之后才去掉，在那之前不发布本机存量
    /// （[`Session::try_prime`]）。有会话正忙就等它，但不一直等：到点照样放行，没清点完的
    /// 线程记着，空下来再清点——最坏是一条旧样子被发布出去，别的设备照着链把它清掉。
    async fn sweep_stale(&mut self, shared: &Shared, s: &Settings, pulled: &Pulled) -> Result<()> {
        if pulled.more || pulled.booted {
            return Ok(());
        }
        let ctx = &shared.ctx;
        let now = now_ms();
        let pool_s = s.clone();
        let swept = on_pool(ctx, move |ctx| {
            let (all, due) = db::read(ctx.db(), |conn| {
                let all = db::kv_json::<bool>(conn, db::KV_STALE_CHECK)? == Some(true);
                let due = merge::stale_pending(conn)? && logs_read(&db::peers(conn)?, now);
                Ok((all, due))
            })?;
            if !due {
                return Ok(None);
            }
            Ok(Some((all, merge::sweep_stale(ctx, &pool_s, all)?)))
        })
        .await?;
        let Some((all, sweep)) = swept else {
            return Ok(());
        };
        if !sweep.report.chats.is_empty() || sweep.report.list_changed {
            shared.emit(SyncEvent::Sessions {
                chats: sweep.report.chats.iter().cloned().collect(),
                list: sweep.report.list_changed,
            });
        }
        if !all {
            return Ok(());
        }
        let waited = if sweep.pending {
            self.stale_wait.get_or_insert_with(Instant::now).elapsed()
        } else {
            Duration::ZERO
        };
        if !sweep.pending || waited >= STALE_BUSY_WAIT {
            self.stale_wait = None;
            on_pool(ctx, |ctx| {
                db::write(ctx.db(), |tx| Ok(sync_db::kv_del(tx, db::KV_STALE_CHECK)?))
            })
            .await?;
        }
        Ok(())
    }

    /// 能不能开始发布本机存量（[`may_prime`]）。可以就记下来。
    async fn try_prime(&mut self, ctx: &Arc<Ctx>, creator: bool, more: bool) -> Result<bool> {
        let since = *self.prime_wait.get_or_insert_with(Instant::now);
        let waited_long = since.elapsed() >= PRIME_WAIT;
        let now = now_ms();
        on_pool(ctx, move |ctx| {
            db::write(ctx.db(), |tx| {
                let ready = may_prime(tx, creator, more, waited_long, now)?;
                if ready {
                    db::kv_put_json(tx, db::KV_PRIMED, &true)?;
                }
                Ok(ready)
            })
        })
        .await
    }

    /// 文件类数据的对比（§8）。
    async fn reconcile(
        &mut self,
        shared: &Shared,
        s: &Settings,
        out: &mut Outcome,
        status_changed: &mut bool,
    ) -> Result<()> {
        let ctx = &shared.ctx;
        let live = self.live.clone();
        let mut force = self.full
            || self
                .files_forced
                .is_none_or(|at| at.elapsed() >= FILES_FORCE_EVERY);
        if force {
            self.files_forced = Some(Instant::now());
        }
        let mut retry = false;
        for _ in 0..FILES_PASSES {
            let cache = std::mem::take(&mut self.files_cache);
            let (pool_ctx, pool_s, keys) = (ctx.clone(), s.clone(), live.keys.clone());
            let (report, cache) = tokio::task::spawn_blocking(move || {
                let mut cache = cache;
                let report = files::reconcile(&pool_ctx, &pool_s, &keys, &mut cache, force);
                (report, cache)
            })
            .await
            .map_err(task_failed)?;
            self.files_cache = cache;
            force = false;
            for what in &report.reload {
                ctx.host.reload(*what);
            }
            *status_changed |= report.status_changed;
            // 排进队列的 op 下一轮封段上传。
            out.again |= report.published > 0;
            if report.applied > 0 {
                shared.set_state("syncing", None);
            }
            retry = report.retry;
            if report.want_blobs.is_empty() {
                break;
            }
            // 技能里的大文件走附件通道：取回来再对比一遍。向别的设备要过、还在等的不在
            // 这里取——存储上一直没有的话，每一轮都要白问一遍；它们由 `fetch_attachments`
            // 隔一阵去取，取到之后的下一轮这个版本就装得上。
            let untried = {
                let blobs = report.want_blobs.clone();
                on_pool(ctx, move |ctx| {
                    db::read(ctx.db(), |conn| {
                        let mut untried = Vec::with_capacity(blobs.len());
                        for blob in blobs {
                            if attach::untried(conn, &blob)? {
                                untried.push(blob);
                            }
                        }
                        Ok(untried)
                    })
                })
                .await?
            };
            if untried.len() < report.want_blobs.len() {
                // 有在等的：让这一轮末尾的 `fetch_attachments` 去看它们。
                shared.wants.store(true, Ordering::Relaxed);
            }
            let mut fetched = 0usize;
            for blob in &untried {
                let download =
                    attach::download(ctx, live.transport(), &live.keys, blob, s.attachment_max);
                match download.await {
                    Ok(_) => fetched += 1,
                    Err(error) if error.is_not_found() => {
                        // 存储上还没有：向别的设备要。
                        let (device, blob) = (s.device.clone(), blob.clone());
                        let missing = on_pool(ctx, move |ctx| {
                            db::write(ctx.db(), |tx| {
                                attach::on_missing(tx, &device, &blob, false, now_ms())
                            })
                        })
                        .await?;
                        if let attach::Missing::Waiting { asked, .. } = missing {
                            // 刚排进队列的 `blob.want` 下一轮封段上传。
                            out.again |= asked;
                            shared.wants.store(true, Ordering::Relaxed);
                        }
                    }
                    Err(error) => tracing::debug!("取回附件 {blob} 失败: {error}"),
                }
            }
            if fetched == 0 {
                break;
            }
        }
        if retry && self.files_retry < FILES_RETRY_MAX {
            self.files_retry += 1;
            out.retry_in(FILES_RETRY_AFTER);
        } else if !retry {
            self.files_retry = 0;
        }
        Ok(())
    }

    /// 排队取回附件：小图自动取（`scan_images`，附件同步开着时），向别的设备要过的隔
    /// 一阵再试。真正的下载在下载任务里，不占这一轮。
    async fn fetch_attachments(
        &mut self,
        shared: &Shared,
        s: &Settings,
        scan_images: bool,
    ) -> Result<()> {
        let ctx = &shared.ctx;
        let scan_images = scan_images && s.scopes.attachments;
        let (images, waiting) = on_pool(ctx, move |ctx| {
            db::read(ctx.db(), |conn| {
                let mut images: Vec<String> = Vec::new();
                if scan_images {
                    // 向别的设备要过的不在这里取（下面隔一阵去取）；已经收了场的不再自动
                    // 取（§8.7），也不让它们占着这几百个名额。
                    let mut stmt = conn.prepare_cached(
                        "SELECT blob_id, rel_path FROM sync_blob WHERE state = ?1 AND size <= ?2 \
                         AND blob_id NOT IN \
                             (SELECT key FROM sync_state WHERE scope IN (?3, ?4)) \
                         ORDER BY rowid DESC LIMIT 400",
                    )?;
                    let rows = stmt.query_map(
                        params![
                            BLOB_REMOTE,
                            AUTO_FETCH_MAX as i64,
                            scope::BLOB_WANT,
                            scope::BLOB_LOST
                        ],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )?;
                    for row in rows {
                        let (blob, rel_path) = row?;
                        let lower = rel_path.to_ascii_lowercase();
                        if IMAGE_SUFFIXES.iter().any(|suffix| lower.ends_with(suffix)) {
                            images.push(blob);
                        }
                    }
                }
                Ok((images, attach::waiting(conn)?))
            })
        })
        .await?;
        let attach::Waiting {
            blobs: wanted,
            stale,
        } = waiting;
        if !stale.is_empty() {
            // 到手了、或者登记已经没了的：不用再记着要过它。
            on_pool(ctx, move |ctx| {
                db::write(ctx.db(), |tx| attach::forget_stale(tx, &stale))
            })
            .await?;
        }
        let mut queued = 0usize;
        for blob in images {
            if queued >= AUTO_FETCH_PER_ROUND {
                break;
            }
            if self.auto_fetched.insert(blob.clone()) {
                shared.request_fetch(blob, false);
                queued += 1;
            }
        }
        // 要过的：对方上传之后没有通知，只能隔一阵去看一眼。等到头了的也在这里去取
        // 最后一次——还是没有，就由下载那边收场（[`attach::on_missing`]），下一轮它就
        // 不在这份清单里了。
        {
            let still: HashSet<&str> = wanted.iter().map(|(blob, _)| blob.as_str()).collect();
            self.want_tries
                .retain(|blob, _| still.contains(blob.as_str()));
        }
        let now = Instant::now();
        let mut tried = 0usize;
        for (blob, gone) in wanted {
            match self.want_tries.get(&blob).copied() {
                None => {
                    self.want_tries.insert(blob, (0, now));
                }
                Some((tries, last)) => {
                    let step = match gone {
                        true => tries.min(WANT_BACKOFF_GONE_MAX),
                        false => tries.min(WANT_BACKOFF.len() - 1),
                    };
                    if tried < WANTS_PER_ROUND && last.elapsed() >= WANT_BACKOFF[step] {
                        tried += 1;
                        self.want_tries.insert(blob.clone(), (tries + 1, now));
                        shared.request_fetch(blob, false);
                    }
                }
            }
        }
        Ok(())
    }

    /// 全量快照（§9.5）：该写就开始写，每轮写一段。出错只放弃这个快照。`start` 为假时
    /// 不开始写新的（已经在写的照常接着写）。
    async fn write_snapshot(
        &mut self,
        shared: &Shared,
        s: &Settings,
        fresh: bool,
        start: bool,
        out: &mut Outcome,
    ) {
        let ctx = &shared.ctx;
        let live = self.live.clone();
        let (transport, keys) = (live.transport(), &live.keys);
        if start && self.writer.is_none() && s.scopes.sessions && !shared.low_memory {
            let due = {
                let s = s.clone();
                on_pool(ctx, move |ctx| {
                    db::read(ctx.db(), |conn| snapshot::due(conn, &s, now_ms()))
                })
                .await
            };
            if matches!(due, Ok(true)) {
                match snapshot::Writer::begin(ctx, transport, s, keys, fresh).await {
                    Ok(writer) => {
                        // `begin` 自己清理过上次没写完的。
                        self.cleaned = true;
                        self.writer = writer;
                    }
                    Err(error) => tracing::debug!("开始写全量快照失败（以后再试）: {error}"),
                }
            }
        }
        let Some(mut writer) = self.writer.take() else {
            return;
        };
        if now_ms() - writer.started_ms() > snapshot::WRITER_MAX_MS {
            tracing::warn!("全量快照写了太久，放弃这一个");
            writer.abort(ctx, transport).await;
            return;
        }
        shared.set_state("syncing", None);
        match writer.step(ctx, transport, SNAP_STEP).await {
            // 写完了：快照指针要随 head 写出去。
            Ok(Some(_)) => out.again = true,
            Ok(None) => {
                self.writer = Some(writer);
                out.retry_in(SNAP_CONTINUE);
            }
            Err(error) => {
                tracing::warn!("写全量快照失败，放弃这一个: {error}");
                writer.abort(ctx, transport).await;
            }
        }
    }

    // -- 两轮之间 -----------------------------------------------------------

    /// 本机变更检查（每 5 秒一次，不发请求，§7.1）。该来一轮完整的时返回「多久之后」：
    /// 运行提示变了马上来；有新消息行等着发布、它所在的会话又空下来了，去抖之后来。
    pub(crate) async fn tick(
        &mut self,
        shared: &Shared,
        config: &SyncConfig,
    ) -> Result<Option<Duration>> {
        let ctx = &shared.ctx;
        let mut due = None;
        let s = self.settings(shared, config);
        if s.scopes.sessions {
            let (seen, recheck) = (self.seen_max, self.busy_pending);
            let check =
                on_pool(ctx, move |ctx| publish::check_local(ctx, &s, seen, recheck)).await?;
            self.seen_max = check.max_id;
            self.busy_pending = check.busy;
            if check.ready {
                due = Some(PUBLISH_DEBOUNCE);
            }
        }
        let mut running = ctx.gate.running_sessions();
        running.sort();
        running.dedup();
        if running != self.running_seen {
            self.running_seen = running;
            due = Some(Duration::ZERO);
        }
        Ok(due)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    const NOW: i64 = 1_763_438_400_000;
    const PEER: &str = "00000000000000b1";

    fn put_peer(ctx: &Ctx, peer: &db::PeerState) {
        db::write(ctx.db(), |tx| db::put_peer_state(tx, PEER, peer)).unwrap();
    }

    fn ready(ctx: &Ctx, creator: bool, more: bool, waited_long: bool) -> bool {
        db::read(ctx.db(), |conn| {
            may_prime(conn, creator, more, waited_long, NOW)
        })
        .unwrap()
    }

    #[test]
    fn a_joining_device_publishes_its_stock_only_after_the_logs_are_read() {
        let (_dir, ctx) = testing::ctx();
        testing::settings(&ctx);
        // 一台别的设备都没见到：等够了才放行。本机建的空间不用等。
        assert!(!ready(&ctx, false, false, false));
        assert!(ready(&ctx, false, false, true));
        assert!(ready(&ctx, true, true, false));

        // 取回的还没处理完。
        let mut peer = db::PeerState {
            fetched: 9,
            processed: 4,
            ..Default::default()
        };
        put_peer(&ctx, &peer);
        assert!(!ready(&ctx, false, false, true));
        // 那份日志停住了：不等它。
        peer.halted = Some("不认识的 op".into());
        put_peer(&ctx, &peer);
        assert!(ready(&ctx, false, false, false));
        // 都处理完了；这一轮的拉取还有剩的就再等一轮。
        peer.halted = None;
        peer.processed = 9;
        put_peer(&ctx, &peer);
        assert!(ready(&ctx, false, false, false));
        assert!(!ready(&ctx, false, true, false));
        // 读不下来：等一阵，一直读不到就不等了。
        peer.error_since_ms = NOW - 1000;
        put_peer(&ctx, &peer);
        assert!(!ready(&ctx, false, false, true));
        peer.error_since_ms = NOW - PRIME_PEER_WAIT_MS;
        put_peer(&ctx, &peer);
        assert!(ready(&ctx, false, false, false));
    }

    #[test]
    fn stock_is_not_published_while_the_stale_check_is_pending() {
        let (_dir, ctx) = testing::ctx();
        testing::settings(&ctx);
        put_peer(
            &ctx,
            &db::PeerState {
                fetched: 3,
                processed: 3,
                ..Default::default()
            },
        );
        assert!(ready(&ctx, false, false, false));
        // 吃过快照、换过身份之后还没清点：加入方、建空间的一方都不放行。
        db::write(ctx.db(), |tx| {
            db::kv_put_json(tx, db::KV_STALE_CHECK, &true)
        })
        .unwrap();
        assert!(!ready(&ctx, false, false, true));
        assert!(!ready(&ctx, true, false, true));
        // 清点完了。
        db::write(ctx.db(), |tx| Ok(sync_db::kv_del(tx, db::KV_STALE_CHECK)?)).unwrap();
        assert!(ready(&ctx, false, false, false));
        assert!(ready(&ctx, true, false, false));
    }
}
