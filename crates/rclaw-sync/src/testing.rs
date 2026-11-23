//! 测试用的本机环境：临时数据根，下面是 `agent/` 与 `sync/`；以及不经存储、直接在两台
//! 测试设备之间搬运日志的小工具。

use std::cell::RefCell;
use std::sync::Arc;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rclaw_store::sync_db::INBOX_PENDING;
use rclaw_store::{Layout, MemoryStore, SessionStore};
use rusqlite::params;

use crate::codec::{self, HeadSeg, Segment};
use crate::config::{Scopes, SyncPaths};
use crate::crypto::{Keys, SpaceKey};
use crate::db;
use crate::merge::{self, Memo, MergeReport};
use crate::op::OpLine;
use crate::publish::{self, Settings};
use crate::pull;
use crate::seal::{self, SealOptions};
use crate::wire::Env;
use crate::{Ctx, NoGate, NoHost, SessionGate};

/// 建好设备身份，给出一份「全部范围都开着」的设置。
pub(crate) fn settings(ctx: &Ctx) -> Settings {
    let device = crate::db::write(ctx.db(), |tx| crate::db::ensure_device(tx)).expect("设备身份");
    Settings {
        device: device.clone(),
        my_ids: vec![device],
        scopes: Scopes {
            sessions: true,
            memory: true,
            persona: true,
            attachments: true,
            skills: true,
            bots: true,
            cron: true,
        },
        channels: vec!["desktop".into()],
        device_name: "测试机".into(),
        trash_days: 30,
        attachment_max: 1 << 20,
    }
}

pub(crate) fn ctx() -> (tempfile::TempDir, Ctx) {
    ctx_with(Arc::new(NoGate))
}

pub(crate) fn ctx_with(gate: Arc<dyn SessionGate>) -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().expect("临时目录");
    let layout = Layout::new(dir.path().join("agent"));
    rclaw_store::fs::init_layout(&layout).expect("初始化布局");
    let sessions = SessionStore::open(layout.clone()).expect("打开会话库");
    let paths = SyncPaths::new(dir.path());
    paths.ensure().expect("建同步目录");
    let ctx = Ctx {
        env: Env::new(&sessions, dir.path()),
        paths,
        memory: MemoryStore::new(layout.clone()),
        layout,
        gate,
        host: Arc::new(NoHost),
        cron: None,
        skills: None,
    };
    (dir, ctx)
}

/// 所有测试设备共用的空间密钥。
pub(crate) fn keys() -> Keys {
    let space_key = SpaceKey::from_base64(&URL_SAFE_NO_PAD.encode([7u8; 32])).expect("空间密钥");
    Keys::derive(&space_key)
}

/// 一台测试设备：自己的数据根、身份与设置。
pub(crate) struct Device {
    _dir: tempfile::TempDir,
    pub ctx: Ctx,
    pub settings: Settings,
    pub keys: Keys,
    memo: RefCell<Memo>,
    /// 已经封好、算作传到了存储上的段。
    shipped: RefCell<Vec<HeadSeg>>,
}

impl Device {
    pub fn new() -> Device {
        Device::with_gate(Arc::new(NoGate))
    }

    pub fn with_gate(gate: Arc<dyn SessionGate>) -> Device {
        let (dir, ctx) = ctx_with(gate);
        let settings = settings(&ctx);
        publish::publish_hello(&ctx, &settings, "test", "0.0.0", None).expect("device.hello");
        Device {
            _dir: dir,
            ctx,
            settings,
            keys: keys(),
            memo: RefCell::new(Memo::default()),
            shipped: RefCell::new(Vec::new()),
        }
    }

    pub fn sessions(&self) -> &SessionStore {
        &self.ctx.env.sessions
    }

    /// 合入一轮。
    pub fn merge(&self) -> MergeReport {
        merge::merge_round(
            &self.ctx,
            &self.settings,
            &mut self.memo.borrow_mut(),
            false,
        )
        .expect("合入")
    }

    /// 合入一轮，并把悬空区、墓碑全部复查一遍。
    pub fn merge_full(&self) -> MergeReport {
        merge::merge_round(&self.ctx, &self.settings, &mut self.memo.borrow_mut(), true)
            .expect("合入")
    }
}

/// 扫描本机变更并全部封段（算作已上传、已记账）。返回日志末尾的序号。
pub(crate) fn seal_all(device: &Device) -> u64 {
    publish::scan_sessions(&device.ctx, &device.settings, true).expect("发布扫描");
    loop {
        let sealed = seal::seal_next(
            &device.ctx,
            &SealOptions {
                keys: &device.keys,
                device: &device.settings.device,
                budget: 1 << 20,
                attachment_max: device.settings.attachment_max,
                upload_attachments: false,
            },
        )
        .expect("封段");
        let Some(seg) = sealed else {
            break;
        };
        db::write(device.ctx.db(), |tx| {
            let mut log = db::log_state(tx)?;
            log.pending = None;
            db::put_log_state(tx, &log)
        })
        .expect("记账");
        device.shipped.borrow_mut().push(seg);
    }
    device.shipped.borrow().last().map_or(0, |seg| seg.to)
}

fn open_segment(device: &Device, seg: &HeadSeg) -> Segment {
    let me = &device.settings.device;
    let object = std::fs::read(device.ctx.paths.outgoing.join(&seg.name)).expect("读段文件");
    let plain = codec::open_log_object(
        &device.keys,
        &codec::segment_path(me, &seg.name),
        &object,
        codec::SEGMENT_PLAIN_MAX,
    )
    .expect("解密段");
    codec::parse_segment(&plain, me).expect("解析段")
}

fn deliver(from: &Device, to: &Device, tamper: &dyn Fn(&str) -> String) {
    seal_all(from);
    let origin = &from.settings.device;
    for seg in from.shipped.borrow().iter() {
        let mut peer = db::read(to.ctx.db(), |conn| db::peer_state(conn, origin)).expect("进度");
        if seg.to <= peer.fetched {
            continue;
        }
        let mut segment = open_segment(from, seg);
        for line in &mut segment.lines {
            *line = tamper(line);
        }
        let outline = pull::outline(&segment).expect("解析段里的 op");
        db::write(to.ctx.db(), |tx| {
            pull::ingest(
                tx,
                origin,
                &mut peer,
                &seg.name,
                &seg.digest,
                &segment,
                &outline,
            )
        })
        .expect("写收件箱");
    }
}

/// 把 `from` 的新变更送进 `to` 的收件箱（还没合入）。
pub(crate) fn ship(from: &Device, to: &Device) {
    deliver(from, to, &|line| line.to_string());
}

/// 同 [`ship`]，但每行 op 先过一遍 `tamper`（模拟不合规或更新版本的 op）。
pub(crate) fn ship_tampered(from: &Device, to: &Device, tamper: impl Fn(&str) -> String) {
    deliver(from, to, &tamper);
}

/// 同 [`ship`]，但 `to` 拿不到 `from` 的日志里序号不大于 `lost` 的部分（模拟早先的日志
/// 已被回收、或还没传到）。
pub(crate) fn ship_after(from: &Device, to: &Device, lost: u64) {
    let origin = &from.settings.device;
    db::write(to.ctx.db(), |tx| {
        let mut peer = db::peer_state(tx, origin)?;
        peer.fetched = lost;
        peer.fetched_seg = None;
        peer.processed = lost;
        db::put_peer_state(tx, origin, &peer)
    })
    .expect("跳过");
    ship(from, to);
}

/// 把 `from` 的整条日志再送一遍（模拟分叉之后的重新取回、换身份之后的重新发布）。
pub(crate) fn replay(from: &Device, to: &Device) {
    seal_all(from);
    let origin = &from.settings.device;
    for seg in from.shipped.borrow().iter() {
        let segment = open_segment(from, seg);
        db::write(to.ctx.db(), |tx| {
            for (index, line) in segment.lines.iter().enumerate() {
                let parsed = OpLine::parse(line)?;
                tx.execute(
                    "INSERT INTO sync_inbox(origin, seg, seq, hlc, sid, op, state) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        origin,
                        seg.name,
                        (segment.header.from + index as u64) as i64,
                        parsed.hlc,
                        parsed.sid,
                        line,
                        INBOX_PENDING
                    ],
                )?;
            }
            Ok(())
        })
        .expect("重放");
    }
}

/// 待发队列里的 op（消息行引用也原样给出）。
pub(crate) fn outbox(device: &Device) -> Vec<serde_json::Value> {
    db::read(device.ctx.db(), |conn| {
        let mut stmt = conn.prepare("SELECT op FROM sync_outbox ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    })
    .expect("读待发队列")
}
