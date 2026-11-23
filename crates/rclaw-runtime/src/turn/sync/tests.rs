use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use async_trait::async_trait;
use rclaw_core::{ChatOutcome, FinishReason, RuntimeChannelInfo, CHANNEL_WEIXIN};
use rclaw_llm::OnText;

use super::*;

/// 立即回一句固定的话。
struct Echo;

fn outcome() -> ChatOutcome {
    ChatOutcome {
        text: "好的".into(),
        reasoning: None,
        tool_calls: vec![],
        stop: Some(FinishReason::Stop),
        usage: None,
    }
}

#[async_trait]
impl Provider for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    async fn chat(&self, _req: ChatRequest) -> rclaw_llm::Result<ChatOutcome> {
        Ok(outcome())
    }
    async fn chat_stream(
        &self,
        _req: ChatRequest,
        on_text: OnText,
    ) -> rclaw_llm::Result<ChatOutcome> {
        on_text("好的");
        Ok(outcome())
    }
}

struct Rig {
    mgr: Arc<TurnManager>,
    session: SessionStore,
    memory_file: PathBuf,
    outbound: mpsc::Receiver<BusMessage>,
    _dir: tempfile::TempDir,
}

fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let layout = Layout::new(dir.path().join("agent"));
    rclaw_store::fs::init_layout(&layout).unwrap();
    let session = SessionStore::new(layout.clone());
    let (outbound_tx, outbound) = mpsc::channel(64);
    let bots = Arc::new(crate::bots::BotRegistry::new(
        &Layout::new(dir.path().join("bots")),
        Arc::new(|_: &str| -> Option<Arc<dyn Provider>> { None }),
        None,
        2200,
        1375,
    ));
    let memory_file = layout.memory_dir.join("MEMORY.md");
    let mgr = Arc::new(TurnManager::new(
        Arc::new(Echo),
        None,
        session.clone(),
        MemoryStore::new(layout.clone()),
        layout,
        Arc::new(ToolRegistry::new()),
        AgentOptions {
            stream_enabled: false,
            send_working_status: false,
            ..AgentOptions::default()
        },
        outbound_tx,
        None,
        RuntimeChannelInfo::default(),
        bots,
        None,
    ));
    Rig {
        mgr,
        session,
        memory_file,
        outbound,
        _dir: dir,
    }
}

async fn recv(rx: &mut mpsc::Receiver<BusMessage>) -> BusMessage {
    tokio::time::timeout(Duration::from_secs(10), rx.recv())
        .await
        .expect("等待 outbound 消息超时")
        .expect("outbound 通道已关闭")
}

async fn idle(mgr: &TurnManager) {
    tokio::time::timeout(Duration::from_secs(10), mgr.wait_turns_idle())
        .await
        .expect("turn 没有在预期时间内结束");
}

fn active_turn(touches_history: bool) -> (ActiveTurn, mpsc::Receiver<Interjection>) {
    let (event_tx, event_rx) = mpsc::channel(1);
    (
        ActiveTurn {
            cancel: CancellationToken::new(),
            event_tx,
            touches_history,
            final_delivered: false,
            started: Instant::now(),
        },
        event_rx,
    )
}

fn key(chat_id: &str, cron_job_id: Option<&str>) -> TurnKey {
    TurnKey {
        channel: CHANNEL_WEIXIN.to_string(),
        chat_id: chat_id.to_string(),
        cron_job_id: cron_job_id.map(str::to_string),
    }
}

#[tokio::test]
async fn a_message_that_hits_a_merge_is_redelivered_once_the_merge_ends() {
    let mut rig = rig();
    let mgr = rig.mgr.clone();
    let guard = mgr
        .begin_merge(CHANNEL_WEIXIN, "u1")
        .expect("空闲的会话可以占住");
    // 同一会话不能重复登记；别的会话不受影响。
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u1").is_none());
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u2").is_some());
    // 合入登记不是 turn：宿主的忙碌判断、/cancel 都看不到它；但它占着一个静默门槽位。
    assert!(!mgr.is_session_busy(CHANNEL_WEIXIN, "u1"));
    assert!(!mgr.is_history_busy(CHANNEL_WEIXIN, "u1"));
    assert!(!mgr.is_idle());
    assert!(!mgr.when_quiet(&mut || panic!("有合入在进行时不该执行")));

    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u1", "第一条"))
        .await;
    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u1", "第二条"))
        .await;
    // 没有启动 turn，没有写库，也没有回「请稍后重试」；两条消息按到达顺序排着。
    assert!(!mgr.is_session_busy(CHANNEL_WEIXIN, "u1"));
    assert!(rig.outbound.try_recv().is_err());
    assert!(rig
        .session
        .load_all(CHANNEL_WEIXIN, "u1")
        .unwrap()
        .is_empty());
    let parked: Vec<String> = mgr.merges().parked[&(CHANNEL_WEIXIN.to_string(), "u1".to_string())]
        .iter()
        .map(|parked| parked.msg.content.clone())
        .collect();
    assert_eq!(parked, ["第一条", "第二条"]);
    // 别的会话照常开始。
    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u2", "不相干"))
        .await;
    assert_eq!(recv(&mut rig.outbound).await.chat_id, "u2");

    drop(guard);
    // 有消息在等：合入让它们先走，这时候占不到。
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u1").is_none());
    let reply = recv(&mut rig.outbound).await;
    assert_eq!(
        (reply.chat_id.as_str(), reply.content.as_str()),
        ("u1", "好的")
    );
    // 第二条要么插进了第一条的 turn，要么在它之后另起一个 turn：等两条都落库。
    let user_texts = || -> Vec<String> {
        rig.session
            .load_all(CHANNEL_WEIXIN, "u1")
            .unwrap()
            .iter()
            .filter(|message| message.role == Role::User)
            .filter_map(|message| message.content.first().and_then(|block| block.as_text()))
            .map(str::to_string)
            .collect()
    };
    for _ in 0..2000 {
        if user_texts().len() >= 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(user_texts(), ["第一条", "第二条"], "顺序不能乱");
    idle(&mgr).await;
    assert!(mgr.merges().parked.is_empty());
    // 都交出去之后又可以占了。
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u1").is_some());
    assert!(mgr.is_idle());
}

#[tokio::test]
async fn merges_wait_for_history_turns_but_not_for_cron_turns() {
    let rig = rig();
    let mgr = rig.mgr.clone();
    let (turn, _live) = active_turn(true);
    mgr.active.lock().unwrap().insert(key("u1", None), turn);
    let (cron, _cron_live) = active_turn(false);
    mgr.active
        .lock()
        .unwrap()
        .insert(key("u2", Some("nightly")), cron);

    assert!(mgr.is_history_busy(CHANNEL_WEIXIN, "u1"));
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u1").is_none());
    // 定时任务的轮次不读写会话历史：不算忙，不挡合入（宿主的删除检查仍然看得到它）。
    assert!(mgr.is_session_busy(CHANNEL_WEIXIN, "u2"));
    assert!(!mgr.is_history_busy(CHANNEL_WEIXIN, "u2"));
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u2").is_some());
    // 占不到的那次没有留下槽位。
    assert_eq!(mgr.gate.running().0, 0);

    assert_eq!(
        mgr.long_running_sessions(Duration::ZERO),
        [(CHANNEL_WEIXIN.to_string(), "u1".to_string())]
    );
    assert!(mgr
        .long_running_sessions(Duration::from_secs(3600))
        .is_empty());

    // 僵尸条目（turn 主体已经退出）不算忙。
    drop(_live);
    assert!(!mgr.is_history_busy(CHANNEL_WEIXIN, "u1"));
    assert!(mgr.begin_merge(CHANNEL_WEIXIN, "u1").is_some());
}

#[tokio::test]
async fn a_cron_turn_starts_even_while_its_session_is_being_merged() {
    let mut rig = rig();
    let mgr = rig.mgr.clone();
    let _guard = mgr.begin_merge(CHANNEL_WEIXIN, "u1").unwrap();
    mgr.route(BusMessage::new_cron(CHANNEL_WEIXIN, "u1", "到点了"))
        .await;
    assert!(recv(&mut rig.outbound).await.content.contains("好的"));
    assert!(mgr.merges().parked.is_empty());
}

#[tokio::test]
async fn a_message_gets_the_busy_reply_only_after_waiting_out_the_limit() {
    let mut rig = rig();
    let mgr = rig.mgr.clone();
    let _guard = mgr.begin_merge(CHANNEL_WEIXIN, "u1").unwrap();
    let started = Instant::now();
    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u1", "你好"))
        .await;
    let reply = recv(&mut rig.outbound).await;
    assert!(reply.content.contains("请稍后重试"), "{}", reply.content);
    assert!(started.elapsed() >= MERGE_RETRY_MAX);
    // 重投任务收了尾。
    tokio::time::sleep(MERGE_RETRY_STEP * 2).await;
    assert!(mgr.merges().parked.is_empty());
    assert!(rig
        .session
        .load_all(CHANNEL_WEIXIN, "u1")
        .unwrap()
        .is_empty());
}

/// 静默门被生命周期操作拿着（多端同步装技能、分叉……）的那一小会儿来的消息：不回「请稍后
/// 重试」，定时任务的消息也不丢，门一开就按到达顺序交出去。
#[tokio::test]
async fn messages_that_meet_a_closed_gate_wait_for_it_to_open() {
    let mut rig = rig();
    let mgr = rig.mgr.clone();
    let quiescent = mgr.gate.try_acquire().expect("空闲时拿得到静默门");
    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u1", "你好"))
        .await;
    mgr.route(BusMessage::new_cron(CHANNEL_WEIXIN, "u2", "到点了"))
        .await;
    tokio::time::sleep(MERGE_RETRY_STEP * 4).await;
    // 门关着：没有回复、没有 turn，两条都排着。
    assert!(rig.outbound.try_recv().is_err());
    assert!(mgr.is_idle());
    assert_eq!(mgr.merges().parked.len(), 2);

    drop(quiescent);
    let mut replied = vec![
        recv(&mut rig.outbound).await.chat_id,
        recv(&mut rig.outbound).await.chat_id,
    ];
    replied.sort();
    assert_eq!(replied, ["u1", "u2"]);
    idle(&mgr).await;
    tokio::time::sleep(MERGE_RETRY_STEP * 2).await;
    assert!(mgr.merges().parked.is_empty());

    // 门一直不开：每条消息各等各的上限，到点才回复忙。（turn 收尾时技能整理会短暂地拿一下
    // 门，所以这里拿不到就再试。）
    let _quiescent = loop {
        if let Some(guard) = mgr.gate.try_acquire() {
            break guard;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let started = Instant::now();
    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u3", "第一条"))
        .await;
    tokio::time::sleep(MERGE_RETRY_MAX / 2).await;
    mgr.route(BusMessage::new(CHANNEL_WEIXIN, "u3", "第二条"))
        .await;
    let first = recv(&mut rig.outbound).await;
    assert!(first.content.contains("请稍后重试"), "{}", first.content);
    assert!(started.elapsed() >= MERGE_RETRY_MAX);
    // 第二条晚到了半个上限：第一条被回绝时它还在等。
    assert!(rig.outbound.try_recv().is_err());
    let second = recv(&mut rig.outbound).await;
    assert!(second.content.contains("请稍后重试"), "{}", second.content);
    assert!(started.elapsed() >= MERGE_RETRY_MAX + MERGE_RETRY_MAX / 2);
    assert!(rig
        .session
        .load_all(CHANNEL_WEIXIN, "u3")
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn when_quiet_runs_only_while_nothing_is_running() {
    let rig = rig();
    let mgr = rig.mgr.clone();
    let mut ran = 0;
    assert!(mgr.when_quiet(&mut || ran += 1));
    assert_eq!(ran, 1);
    // 持门期间新的 turn、新的合入都起不来。
    let mut inside = None;
    assert!(mgr.when_quiet(&mut || inside = Some(mgr.begin_merge(CHANNEL_WEIXIN, "u1").is_none())));
    assert_eq!(inside, Some(true));
}

/// 记下被调用了什么的同步入口。
#[derive(Default)]
struct FakeSync {
    in_space: AtomicBool,
    left: AtomicUsize,
    purged: AtomicUsize,
    synced: AtomicUsize,
    changed: Mutex<Vec<bool>>,
    fail_leave: AtomicBool,
    /// 引擎一直忙着：退出同步空间的请求等不到回话。
    stall_leave: AtomicBool,
}

#[async_trait]
impl SyncControl for FakeSync {
    fn in_space(&self) -> Result<bool, String> {
        Ok(self.in_space.load(Ordering::SeqCst))
    }
    fn status_text(&self) -> String {
        "状态：空闲".to_string()
    }
    async fn sync_now(&self) -> Result<(), String> {
        self.synced.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn set_paused(&self, paused: bool) -> Result<(), String> {
        match paused {
            true => Ok(()),
            false => Err("没有暂停".to_string()),
        }
    }
    fn pending_text(&self, item: &str) -> Result<String, String> {
        match item {
            "a1" => Ok("人格文件的新旧内容".to_string()),
            _ => Err("没有这一项".to_string()),
        }
    }
    async fn resolve_pending(&self, item: &str, action: &str) -> Result<String, String> {
        match (item, action) {
            ("a1", "accept" | "reject") => Ok(format!("{action} {item}")),
            _ => Err("这一项不支持这个操作".to_string()),
        }
    }
    async fn leave_space(&self) -> Result<(), String> {
        if self.stall_leave.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.fail_leave.load(Ordering::SeqCst) {
            return Err("存储连不上".to_string());
        }
        self.left.fetch_add(1, Ordering::SeqCst);
        self.in_space.store(false, Ordering::SeqCst);
        Ok(())
    }
    fn delete_published_sessions(&self) -> Result<(usize, usize), String> {
        self.purged.fetch_add(1, Ordering::SeqCst);
        Ok((3, 0))
    }
    fn local_changed(&self, turn_ended: bool) {
        self.changed.lock().unwrap().push(turn_ended);
    }
}

fn seed(rig: &Rig) {
    rig.session
        .append(CHANNEL_WEIXIN, "u1", &Message::plain(Role::User, "旧消息"))
        .unwrap();
    std::fs::create_dir_all(rig.memory_file.parent().unwrap()).unwrap();
    std::fs::write(&rig.memory_file, "记忆").unwrap();
}

fn wiped(rig: &Rig) -> bool {
    rig.session
        .load_all(CHANNEL_WEIXIN, "u1")
        .unwrap()
        .is_empty()
        && !rig.memory_file.exists()
}

#[tokio::test]
async fn clear_is_unchanged_without_a_sync_space() {
    let rig = rig();
    seed(&rig);
    let prompt = rig.mgr.handle_clear("/clear").await;
    assert!(prompt.ends_with("确认请输入：/clear confirm"), "{prompt}");
    // 装了同步入口但不在空间里：同样不变，多写的范围词被忽略。
    let sync = Arc::new(FakeSync::default());
    assert!(rig.mgr.set_sync_control(sync.clone()));
    assert!(!rig.mgr.set_sync_control(sync.clone()), "只接受一次");
    let done = rig.mgr.handle_clear("/clear confirm all").await;
    assert!(done.starts_with("✅"), "{done}");
    assert!(!done.contains("多端同步"), "{done}");
    assert!(wiped(&rig));
    assert_eq!(sync.purged.load(Ordering::SeqCst), 0);
    assert_eq!(sync.left.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn clear_in_a_sync_space_needs_a_scope() {
    let rig = rig();
    seed(&rig);
    let sync = Arc::new(FakeSync::default());
    sync.in_space.store(true, Ordering::SeqCst);
    rig.mgr.set_sync_control(sync.clone());

    for content in ["/clear", "/clear confirm", "/clear confirm everything"] {
        let reply = rig.mgr.handle_clear(content).await;
        assert!(reply.contains("/clear confirm local"), "{content}: {reply}");
        assert!(reply.contains("/clear confirm all"), "{content}: {reply}");
        assert!(!wiped(&rig), "{content}");
    }

    // all：先把会话删除传给其他设备，再清本机；清完通知引擎发布。
    let done = rig.mgr.handle_clear("/clear confirm ALL").await;
    assert!(done.starts_with("✅"), "{done}");
    assert!(done.contains("已登记 3 个会话和 0 条记忆的删除"), "{done}");
    assert!(wiped(&rig));
    assert_eq!(sync.purged.load(Ordering::SeqCst), 1);
    assert_eq!(sync.left.load(Ordering::SeqCst), 0);
    assert_eq!(*sync.changed.lock().unwrap(), [false]);
}

#[tokio::test]
async fn clear_local_leaves_the_space_first_and_stops_if_that_fails() {
    let rig = rig();
    seed(&rig);
    let sync = Arc::new(FakeSync::default());
    sync.in_space.store(true, Ordering::SeqCst);
    sync.fail_leave.store(true, Ordering::SeqCst);
    rig.mgr.set_sync_control(sync.clone());

    let failed = rig.mgr.handle_clear("/clear confirm local").await;
    assert!(failed.contains("没有清空任何内容"), "{failed}");
    assert!(failed.contains("存储连不上"), "{failed}");
    assert!(!wiped(&rig));
    // 失败之后门是开着的。
    assert!(rig.mgr.when_quiet(&mut || {}));

    sync.fail_leave.store(false, Ordering::SeqCst);
    let done = rig.mgr.handle_clear("/clear confirm local").await;
    assert!(done.starts_with("✅"), "{done}");
    assert!(done.contains("已退出同步空间"), "{done}");
    assert!(wiped(&rig));
    assert_eq!(sync.left.load(Ordering::SeqCst), 1);
    assert_eq!(sync.purged.load(Ordering::SeqCst), 0);
}

/// 引擎一直忙（一轮同步没跑完）：`/clear confirm local` 不拿着静默门一直等，到点放弃，什么
/// 都不清。
#[tokio::test]
async fn clear_local_gives_up_when_the_engine_stays_busy() {
    let rig = rig();
    seed(&rig);
    let sync = Arc::new(FakeSync::default());
    sync.in_space.store(true, Ordering::SeqCst);
    sync.stall_leave.store(true, Ordering::SeqCst);
    rig.mgr.set_sync_control(sync.clone());

    let started = Instant::now();
    let busy = rig.mgr.handle_clear("/clear confirm local").await;
    assert!(busy.contains("多端同步正忙"), "{busy}");
    assert!(busy.contains("没有清空任何内容"), "{busy}");
    assert!(busy.contains("/clear confirm local"), "{busy}");
    assert!(started.elapsed() >= LEAVE_WAIT);
    assert!(started.elapsed() < LEAVE_WAIT * 3);
    assert!(!wiped(&rig));
    assert_eq!(sync.left.load(Ordering::SeqCst), 0);
    // 门放开了。
    assert!(rig.mgr.when_quiet(&mut || {}));

    // 引擎空下来之后重试就成了。
    sync.stall_leave.store(false, Ordering::SeqCst);
    let done = rig.mgr.handle_clear("/clear confirm local").await;
    assert!(done.starts_with("✅"), "{done}");
    assert!(wiped(&rig));
}

#[tokio::test]
async fn sync_command_reports_and_controls() {
    let mut rig = rig();
    let bare = rig.mgr.handle_sync("/sync").await;
    assert!(bare.contains("没有接入多端同步"), "{bare}");

    let sync = Arc::new(FakeSync::default());
    rig.mgr.set_sync_control(sync.clone());
    assert_eq!(rig.mgr.handle_sync("/sync").await, "状态：空闲");
    assert_eq!(rig.mgr.handle_sync("/sync STATUS").await, "状态：空闲");
    let now = rig.mgr.handle_sync("/sync now").await;
    assert!(
        now.starts_with("✅") && now.ends_with("状态：空闲"),
        "{now}"
    );
    assert_eq!(sync.synced.load(Ordering::SeqCst), 1);
    assert!(rig.mgr.handle_sync("/sync pause").await.contains("已暂停"));
    assert!(rig
        .mgr
        .handle_sync("/sync resume")
        .await
        .contains("没有暂停"));
    assert!(rig.mgr.handle_sync("/sync pair").await.starts_with("用法"));

    // 待处理事项：没有设置面板的宿主用命令做本机确认。
    assert_eq!(
        rig.mgr.handle_sync("/sync show a1").await,
        "人格文件的新旧内容"
    );
    assert_eq!(rig.mgr.handle_sync("/sync show zz").await, "⚠️ 没有这一项");
    assert_eq!(rig.mgr.handle_sync("/sync ACCEPT a1").await, "✅ accept a1");
    assert_eq!(
        rig.mgr.handle_sync("/sync retry a1").await,
        "⚠️ 这一项不支持这个操作"
    );
    for incomplete in ["/sync show", "/sync accept", "/sync dismiss"] {
        assert!(
            rig.mgr.handle_sync(incomplete).await.starts_with("用法"),
            "{incomplete}"
        );
    }

    // 外部通道的普通用户不能看同步状态，也不能控制它。
    rig.mgr
        .route(BusMessage::new(CHANNEL_WEIXIN, "u1", "/sync now"))
        .await;
    assert!(recv(&mut rig.outbound).await.content.contains("权限不足"));
    assert_eq!(sync.synced.load(Ordering::SeqCst), 1);
    // 本机通道可以。
    rig.mgr
        .route(BusMessage::new(CHANNEL_CLI, "console", "/sync"))
        .await;
    assert_eq!(recv(&mut rig.outbound).await.content, "状态：空闲");
}

/// 配对码打进对话里：用户发的整条不处理（不落会话、不起 turn），只回一句提示；内部来源的
/// 遮掉配对码照常处理。
#[tokio::test]
async fn a_pairing_code_typed_into_a_chat_goes_no_further() {
    let mut rig = rig();
    let body = "eyJzIjoiMDEyMzQ1Njc4OWFiY2RlZjAxMjMiLCJrIjoiQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBIn0";
    let code = format!("rclaw-pair-v1.{}\n{}", &body[..40], &body[40..]);

    rig.mgr
        .route(BusMessage::new(
            CHANNEL_WEIXIN,
            "u1",
            format!("帮我加入：{code}"),
        ))
        .await;
    let reply = recv(&mut rig.outbound).await;
    assert_eq!(reply.chat_id, "u1");
    assert!(
        reply.content.contains("没有保存到会话"),
        "{}",
        reply.content
    );
    assert!(reply.content.contains("--sync-join"), "{}", reply.content);
    assert!(!reply.content.contains(&body[..40]));
    assert!(rig.mgr.is_idle());
    assert!(rig
        .session
        .load_all(CHANNEL_WEIXIN, "u1")
        .unwrap()
        .is_empty());
    assert!(rig.outbound.try_recv().is_err());

    // 提到前缀的普通消息照常处理。
    rig.mgr
        .route(BusMessage::new(
            CHANNEL_WEIXIN,
            "u1",
            "配对码是 rclaw-pair-v1. 开头的吗",
        ))
        .await;
    assert_eq!(recv(&mut rig.outbound).await.content, "好的");
    idle(&rig.mgr).await;

    // 内部来源（这里是心跳）：遮掉之后照常走，落进会话的是遮过的文本。
    let masked = rig
        .mgr
        .intercept_pairing_code(BusMessage::new_heartbeat(
            CHANNEL_WEIXIN,
            "u2",
            format!("检查一下 {code} 之后"),
        ))
        .await
        .expect("内部消息不丢");
    assert_eq!(masked.content, "检查一下 [REDACTED] 之后");
    assert!(rig.outbound.try_recv().is_err());
}

#[tokio::test]
async fn the_engine_hears_when_a_history_turn_ends() {
    let mut rig = rig();
    let sync = Arc::new(FakeSync::default());
    rig.mgr.set_sync_control(sync.clone());
    rig.mgr
        .route(BusMessage::new(CHANNEL_WEIXIN, "u1", "你好"))
        .await;
    recv(&mut rig.outbound).await;
    idle(&rig.mgr).await;
    assert_eq!(*sync.changed.lock().unwrap(), [true]);
    // 定时任务的轮次不写会话历史，不通知。
    rig.mgr
        .route(BusMessage::new_cron(CHANNEL_WEIXIN, "u1", "到点了"))
        .await;
    recv(&mut rig.outbound).await;
    idle(&rig.mgr).await;
    assert_eq!(*sync.changed.lock().unwrap(), [true]);
}
