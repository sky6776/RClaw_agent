//! 引擎的端到端测试：两台「设备」各有自己的数据根，经同一个本机目录（`file` 后端）同步。
//! 引擎跑在自己的线程上，所以这里用真实的时钟，靠轮询等结果。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rclaw_core::{Message, Role};
use rclaw_store::{Layout, MemoryStore, SessionStore};

use super::*;
use crate::NoGate;

/// 把引擎发出的事件记下来。
#[derive(Default)]
struct Recorder(Mutex<Vec<SyncEvent>>);

impl SyncHost for Recorder {
    fn emit(&self, event: SyncEvent) {
        lock(&self.0).push(event);
    }
}

struct Rig {
    engine: SyncEngine,
    sessions: SessionStore,
    events: Arc<Recorder>,
    dir: tempfile::TempDir,
}

impl Rig {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn agent(&self) -> PathBuf {
        self.dir.path().join("agent")
    }
}

fn config_for(storage: &Path, name: &str, enabled: bool) -> SyncConfig {
    let mut config = SyncConfig {
        enabled,
        device_name: name.into(),
        transport: "file".into(),
        ..Default::default()
    };
    config.file.dir = storage.to_string_lossy().into_owned();
    config
}

fn engine_on(
    root: &Path,
    sessions: &SessionStore,
    events: Arc<Recorder>,
    config: SyncConfig,
) -> SyncEngine {
    SyncEngine::new(
        SyncDeps {
            sessions: sessions.clone(),
            data_root: root.to_path_buf(),
            memory: MemoryStore::new(sessions.layout().clone()),
            gate: Arc::new(NoGate),
            host: events,
            cron: None,
            skills: None,
            platform: "test".into(),
            version: "0.0.0".into(),
            sessions_supported: true,
            low_memory: false,
            refresh_config: None,
        },
        config,
    )
}

fn rig_with(config: SyncConfig) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path().join("agent"));
    rclaw_store::fs::init_layout(&layout).unwrap();
    let sessions = SessionStore::open(layout).unwrap();
    let events = Arc::new(Recorder::default());
    let engine = engine_on(dir.path(), &sessions, events.clone(), config);
    engine.start();
    Rig {
        engine,
        sessions,
        events,
        dir,
    }
}

fn rig(storage: &Path, name: &str) -> Rig {
    rig_with(config_for(storage, name, true))
}

fn say(rig: &Rig, chat_id: &str, role: Role, text: &str) {
    rig.sessions
        .append("desktop", chat_id, &Message::plain(role, text))
        .unwrap();
}

fn texts(rig: &Rig, chat_id: &str) -> Vec<String> {
    rig.sessions
        .load_all("desktop", chat_id)
        .unwrap_or_default()
        .iter()
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| block.as_text())
                .collect::<String>()
        })
        .collect()
}

/// 等到 `done` 成立（最多 `secs` 秒）。
async fn wait_for(what: &str, secs: u64, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !done() {
        assert!(Instant::now() < deadline, "等不到：{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 让这些引擎轮流同步，直到 `done` 成立。
async fn sync_until(what: &str, engines: &[&SyncEngine], done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        for engine in engines {
            engine.sync_now().await.unwrap();
        }
        if done() {
            return;
        }
        assert!(Instant::now() < deadline, "同步之后仍然等不到：{what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// 建一个空间、两台设备都加入。
async fn paired(storage: &Path) -> (Rig, Rig) {
    let a = rig(storage, "甲");
    let b = rig(storage, "乙");
    a.engine.create_space().await.unwrap();
    a.engine.sync_now().await.unwrap();
    let code = a.engine.pairing_code(false).await.unwrap();
    let joined = b.engine.join_space(&code).await.unwrap();
    assert!(joined.enabled);
    (a, b)
}

#[tokio::test]
async fn a_disabled_engine_starts_nothing_and_touches_nothing() {
    let storage = tempfile::tempdir().unwrap();
    let off = rig_with(config_for(storage.path(), "甲", false));
    assert_eq!(off.engine.status().state, "off");
    assert!(off.engine.sync_now().await.is_err());
    // 发送前的钩子马上返回。
    let started = Instant::now();
    off.engine.presend("desktop", "c1", "你好").await;
    assert!(started.elapsed() < Duration::from_millis(500));
    off.engine.notify(Trigger::Foreground);
    // 没有同步目录，库里也没有同步表。
    assert!(!off.root().join("sync").exists());
    let tables = db::read(off.sessions.db(), |conn| {
        Ok(sync_db::sync_tables_exist(conn)?)
    })
    .unwrap();
    assert!(!tables);
}

#[tokio::test]
async fn two_devices_exchange_and_continue_a_session() {
    let storage = tempfile::tempdir().unwrap();
    let (a, b) = paired(storage.path()).await;
    say(&a, "c1", Role::User, "一");
    say(&a, "c1", Role::Assistant, "二");
    sync_until("乙看到甲的会话", &[&a.engine, &b.engine], || {
        texts(&b, "c1") == ["一", "二"]
    })
    .await;
    // 乙续写，甲看到。
    say(&b, "c1", Role::User, "三");
    sync_until("甲看到乙的续写", &[&b.engine, &a.engine], || {
        texts(&a, "c1") == ["一", "二", "三"]
    })
    .await;

    let status = a.engine.status();
    assert!(
        matches!(status.state.as_str(), "idle" | "syncing"),
        "{}",
        status.state
    );
    assert!(status.space_id.is_some());
    assert_eq!(status.devices.len(), 2);
    assert_eq!(status.devices[0].name, "甲");
    assert_eq!(status.devices[1].name, "乙");
    assert!(status.last_sync_ms > 0);
    // 两边的会话列表都收到过「有会话变了」的事件。
    let got_sessions = |rig: &Rig| {
        lock(&rig.events.0)
            .iter()
            .any(|event| matches!(event, SyncEvent::Sessions { .. }))
    };
    assert!(got_sessions(&a) && got_sessions(&b));
    let marks = b.engine.session_marks().unwrap();
    assert!(marks.iter().any(|mark| mark.chat_id == "c1" && mark.synced));
}

#[tokio::test]
async fn deleting_a_session_reaches_the_other_device_and_its_trash() {
    let storage = tempfile::tempdir().unwrap();
    let (a, b) = paired(storage.path()).await;
    say(&a, "c1", Role::User, "要删的");
    say(&a, "keep", Role::User, "留着的");
    sync_until("乙收到两个会话", &[&a.engine, &b.engine], || {
        texts(&b, "c1").len() == 1 && texts(&b, "keep").len() == 1
    })
    .await;

    assert!(a.engine.delete_session("desktop", "c1", false).unwrap());
    sync_until("删除传到乙", &[&a.engine, &b.engine], || {
        texts(&b, "c1").is_empty()
    })
    .await;
    assert_eq!(texts(&b, "keep"), ["留着的"]);
    // 两边各留了一份在回收站，可以恢复。
    assert_eq!(a.engine.trash_list().len(), 1);
    let kept = b.engine.trash_list();
    assert_eq!(kept.len(), 1);
    let restored = b.engine.trash_restore(&kept[0].id).unwrap();
    assert_eq!(restored.messages, 1);
}

#[tokio::test]
async fn pausing_stops_rounds_until_resumed() {
    let storage = tempfile::tempdir().unwrap();
    let (a, b) = paired(storage.path()).await;
    b.engine.set_paused(true).await.unwrap();
    assert_eq!(b.engine.status().state, "paused");
    assert!(b.engine.sync_now().await.is_err());
    say(&a, "c1", Role::User, "暂停期间发的");
    a.engine.sync_now().await.unwrap();
    a.engine.sync_now().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(texts(&b, "c1").is_empty());

    b.engine.set_paused(false).await.unwrap();
    sync_until("恢复之后收到", &[&a.engine, &b.engine], || {
        texts(&b, "c1") == ["暂停期间发的"]
    })
    .await;
}

#[tokio::test]
async fn a_second_engine_on_the_same_data_root_stands_by_then_takes_over() {
    let storage = tempfile::tempdir().unwrap();
    let first = rig(storage.path(), "甲");
    wait_for("第一个引擎就绪", 10, || {
        first.engine.status().state == "setup"
    })
    .await;
    let second = engine_on(
        first.root(),
        &first.sessions,
        Arc::new(Recorder::default()),
        config_for(storage.path(), "甲", true),
    );
    second.start();
    wait_for("第二个引擎待命", 10, || {
        second.status().state == "standby"
    })
    .await;
    assert!(second.create_space().await.is_err());
    // 第一个停了，第二个接手。
    first.engine.stop();
    wait_for("第二个引擎接手", 45, || {
        second.status().state == "setup"
    })
    .await;
    second.create_space().await.unwrap();
    second.sync_now().await.unwrap();
}

/// 面板上的「重试」是把现有的配置再交一遍：没连上同步空间的引擎线程收到就再试一次；
/// 线程自己退出了的（里面出了没料到的错），换一条新的。用户点名要做的操作也会换。
#[tokio::test]
async fn retrying_reopens_and_replaces_an_engine_thread_that_died() {
    let storage = tempfile::tempdir().unwrap();
    let first = rig(storage.path(), "甲");
    wait_for("第一个引擎就绪", 10, || {
        first.engine.status().state == "setup"
    })
    .await;
    let config = config_for(storage.path(), "甲", true);
    let second = engine_on(
        first.root(),
        &first.sessions,
        Arc::new(Recorder::default()),
        config.clone(),
    );
    second.start();
    wait_for("第二个引擎待命", 10, || {
        second.status().state == "standby"
    })
    .await;
    // 等它过了「很快再试一次」的那一下：此后它自己要隔半分钟才再试。
    tokio::time::sleep(Duration::from_millis(2500)).await;
    assert_eq!(second.status().state, "standby");
    first.engine.stop();
    let deadline = Instant::now() + Duration::from_secs(10);
    while second.status().state != "setup" {
        assert!(Instant::now() < deadline, "重试没有让待命的引擎接手");
        match second.apply_config(config.clone()) {
            Ok(()) => {}
            // 待命的引擎接手时会持有存储协议锁读盘，正好撞上就是「正在保存」：
            // 下一次轮询重试即可，别的错误照样算失败。
            Err(error) if error.to_string().contains("正在保存") => {}
            Err(error) => panic!("重试发布失败: {error}"),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let crash = |engine: &SyncEngine| {
        let (commands, _) = engine.live(false).unwrap();
        commands.send(Cmd::Crash).unwrap();
    };
    let stopped = |engine: &SyncEngine| {
        let status = engine.status();
        status.state == "error"
            && status.space_id.is_none()
            && status
                .message
                .is_some_and(|message| message.contains("意外停止"))
    };
    crash(&second);
    wait_for("面板上说明引擎停了", 10, || stopped(&second)).await;
    // 不是用户点名的动静，不会马上把刚起来就出错的线程再拉起来。
    second.notify(Trigger::Focus);
    second.presend("desktop", "c1", "你好").await;
    assert!(stopped(&second));
    // 「重试」换上一条新的线程。
    second.apply_config(config.clone()).unwrap();
    wait_for("新的引擎线程就绪", 10, || {
        second.status().state == "setup"
    })
    .await;

    // 用户点名要做的操作也会换：这里直接创建空间。
    crash(&second);
    wait_for("面板上说明引擎停了", 10, || stopped(&second)).await;
    second.create_space().await.unwrap();
    second.sync_now().await.unwrap();
    assert!(second.status().space_id.is_some());
}

#[tokio::test]
async fn attachments_upload_after_the_backlog_choice_and_download_on_request() {
    let storage = tempfile::tempdir().unwrap();
    let with_attachments = |name: &str| {
        let mut config = config_for(storage.path(), name, true);
        config.scopes.push("attachments".into());
        config
    };
    let a = rig_with(with_attachments("甲"));
    let b = rig_with(with_attachments("乙"));
    // 开启同步之前就有的会话和附件：算存量。
    std::fs::create_dir_all(a.agent().join("uploads")).unwrap();
    std::fs::write(a.agent().join("uploads").join("a.txt"), b"hello").unwrap();
    say(&a, "c1", Role::User, "请看 uploads/a.txt");
    a.engine.create_space().await.unwrap();
    let code = a.engine.pairing_code(false).await.unwrap();
    b.engine.join_space(&code).await.unwrap();
    // 首次同步做完之后，面板上问存量附件传不传。
    sync_until("甲提示有存量附件", &[&a.engine], || {
        a.engine.status().backlog_bytes == 5
    })
    .await;
    a.engine.set_backlog(Backlog::All).await.unwrap();
    wait_for("甲传完附件", 20, || {
        let status = a.engine.status();
        status.uploads == 0 && status.backlog_bytes == 0
    })
    .await;

    sync_until(
        "乙看到会话和附件占位",
        &[&a.engine, &b.engine],
        || {
            b.engine
                .attachments("desktop", "c1")
                .is_ok_and(|list| list.len() == 1)
        },
    )
    .await;
    let listed = b.engine.attachments("desktop", "c1").unwrap();
    assert_eq!(listed[0].path, "uploads/a.txt");
    assert_eq!(listed[0].state, "remote");
    assert!(!listed[0].path_present);
    assert!(!b.agent().join("uploads").join("a.txt").exists());

    b.engine.fetch_attachment(&listed[0].blob).unwrap();
    wait_for("乙取回附件", 20, || {
        b.agent().join("uploads").join("a.txt").is_file()
    })
    .await;
    assert_eq!(
        std::fs::read(b.agent().join("uploads").join("a.txt")).unwrap(),
        b"hello"
    );
    let blob = listed[0].blob.clone();
    wait_for("乙收到取回完成的事件", 10, || {
        lock(&b.events.0).iter().any(|event| {
            matches!(
                event,
                SyncEvent::Attachment {
                    blob: got,
                    error: None,
                    placed: true,
                    ..
                } if *got == blob
            )
        })
    })
    .await;
    let landed = b.engine.attachments("desktop", "c1").unwrap();
    assert_eq!(landed[0].state, "local");
    // 取回到原位置之后，界面按工作区路径出的卡就能直接打开了。
    assert!(landed[0].path_present);

    // 就地把同名文件改成别的内容（长度不变）：内容已经不是登记的那份，不能再当本机有
    // 打开（否则看到的是另一个文件）；界面给「重新下载」，取回时落到同步缓存。
    std::fs::write(b.agent().join("uploads").join("a.txt"), b"HELLO").unwrap();
    let tampered = b.engine.attachments("desktop", "c1").unwrap();
    assert_eq!(tampered[0].state, "changed");
    assert!(!tampered[0].path_present);
}

#[tokio::test]
async fn joining_without_an_engine_saves_the_key_for_the_next_start() {
    let storage = tempfile::tempdir().unwrap();
    let a = rig(storage.path(), "甲");
    a.engine.create_space().await.unwrap();
    let code = a.engine.pairing_code(false).await.unwrap();

    // 命令行加入：没有引擎在跑。
    let dir = tempfile::tempdir().unwrap();
    let config = config_for(storage.path(), "终端", false);
    // 本机目录做存储不需要密码；WebDAV 填了用户名而本机没存密码、配对码里又没带，就得问。
    assert!(!join_needs_secret(dir.path(), &config, &code).unwrap());
    let mut dav = config.clone();
    dav.transport = "webdav".into();
    dav.webdav.username = "me".into();
    assert!(join_needs_secret(dir.path(), &dav, &code).unwrap());
    assert!(join_needs_secret(dir.path(), &dav, "不是配对码").is_err());

    let joined = join_space_offline(dir.path(), &config, &code, None)
        .await
        .unwrap();
    assert!(joined.enabled);
    let saved = Secrets::load(&SyncPaths::new(dir.path())).unwrap();
    assert!(saved.is_paired());
    // 用同一个配对码再来一次不算错（上次没能把配置写回去时用得上）；换一个空间才拒绝。
    assert!(join_space_offline(dir.path(), &config, &code, None)
        .await
        .is_ok());
    let other = PairingCode {
        space_id: crate::secrets::new_space_id(),
        key: saved.key.clone(),
        storage: None,
    }
    .encode()
    .unwrap();
    assert!(join_space_offline(dir.path(), &config, &other, None)
        .await
        .is_err());
    // 错的配对码进不来。
    assert!(
        join_space_offline(dir.path(), &config, "rclaw-pair-v1.abc", None)
            .await
            .is_err()
    );
    // 有引擎在跑的数据目录：让用户到那个进程里去加入。
    assert!(join_space_offline(a.root(), &config, &code, None)
        .await
        .is_err());
}
