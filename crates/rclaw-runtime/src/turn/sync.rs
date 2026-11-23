//! 多端同步与 turn 的接缝（`docs/multi-device-sync.md` §7.3、§7.5、§7.8）。
//!
//! runtime 不依赖同步引擎，两边只在这里碰面：
//! - 引擎经 [`crate::SessionLifecycle`] 占住一个会话做合入（合入登记）；撞上合入的新消息
//!   稍后重投，不回「请稍后重试」；
//! - runtime 经宿主装进来的 [`SyncControl`] 执行 `/sync`、带范围的 `/clear`，并在 turn
//!   结束时告诉引擎「可以发布了」。
//!
//! 没装 [`SyncControl`]、也没有人调合入登记时，这里的代码一行都不会改变既有行为。

use std::sync::MutexGuard;
use std::time::{Duration, Instant};

use super::*;

/// 启动不了 turn 的消息（撞上合入，或者静默门被生命周期操作拿着）多久重投一次、每条最多
/// 等多久。合入是毫秒级的，拿着静默门的操作（同步装技能、分叉、技能整理）多半也很短：
/// 等过上限才回复忙。
const MERGE_RETRY_STEP: Duration = Duration::from_millis(50);
const MERGE_RETRY_MAX: Duration = Duration::from_secs(2);
/// 一个会话最多停这么多条等着重投的消息（与插队队列的容量一致），再多就丢弃并告警。
const PARKED_MAX: usize = 32;
/// `/clear confirm local` 最多等同步引擎这么久去退出同步空间。等的时候路由任务拿着静默门：
/// 别的会话的消息都排着，所以不能没有上限。
const LEAVE_WAIT: Duration = Duration::from_secs(5);

/// 合入登记，以及等着重投的消息。
///
/// 加锁顺序：先 `active` 后本结构，反过来的路径不存在。「确认没有活动 turn → 登记合入」
/// 与「确认没有合入登记 → 登记 turn」各自在同一次 `active` 持锁里完成，所以两边不可能
/// 同时通过检查。只碰本结构的操作（注销、排队、重投）不需要 `active` 锁。
#[derive(Default)]
pub(super) struct MergeState {
    /// 正在被合入的会话 `(channel, chat_id)`。
    held: HashSet<(String, String)>,
    /// 启动不了 turn 的消息（撞上合入的、撞上静默门的），按到达顺序。有条目 = 有一个重投
    /// 任务在替这个会话等。
    parked: HashMap<(String, String), VecDeque<Parked>>,
}

/// 一条等着重投的消息。
struct Parked {
    msg: BusMessage,
    /// 它是什么时候停下来的：每条消息各算各的等待上限。
    since: Instant,
}

impl Parked {
    fn new(msg: BusMessage) -> Parked {
        Parked {
            msg,
            since: Instant::now(),
        }
    }
}

/// 合入登记的守卫：Drop 时注销登记并释放静默门槽位。
pub struct MergeGuard {
    manager: Arc<TurnManager>,
    chat: (String, String),
}

impl Drop for MergeGuard {
    fn drop(&mut self) {
        self.manager.merges().held.remove(&self.chat);
        self.manager.gate.end_turn();
    }
}

/// 多端同步的控制入口。宿主把同步引擎包成它，用
/// [`crate::AgentLoop::set_sync_control`] 装进来；实现里的阻塞方法由 runtime 放到阻塞线程
/// 上调用。
#[async_trait::async_trait]
pub trait SyncControl: Send + Sync {
    /// 这台设备（这个数据目录）是否在一个同步空间里：创建或加入过，还没有退出。同步此刻
    /// 关着、或者由用同一个数据目录的另一个进程在做，也算在——清空本机时同样要问范围。
    /// **阻塞**（可能要读一次磁盘上的配对状态）。
    fn in_space(&self) -> Result<bool, String>;

    /// `/sync status` 的正文。**阻塞**（读库）。
    fn status_text(&self) -> String;

    /// 立即同步一轮，做完才返回。
    async fn sync_now(&self) -> Result<(), String>;

    /// 暂停 / 恢复同步。
    async fn set_paused(&self, paused: bool) -> Result<(), String>;

    /// 一条待处理事项的详情（`/sync show <编号>`）；`item` 是状态里列出的编号。**阻塞**。
    fn pending_text(&self, item: &str) -> Result<String, String>;

    /// 处理一条待处理事项（`/sync accept|reject|retry|discard|dismiss <编号>`）：等本机确认
    /// 的人格 / 技能 / bot 改动、被隔离的会话、看过的提示。成功返回给用户的一句结果。
    async fn resolve_pending(&self, item: &str, action: &str) -> Result<String, String>;

    /// 退出同步空间（`/clear confirm local`）。其他设备上的数据不动。
    async fn leave_space(&self) -> Result<(), String>;

    /// `/clear confirm all` 的会话部分：把参与同步的会话逐个按「删除」处理——本机回收站
    /// 留一份，删除传到其他设备。返回登记删除的会话数。**阻塞**。调用方持静默门：期间
    /// 没有 turn 在跑，也不会有合入，实现不要再去占会话。
    fn delete_published_sessions(&self) -> Result<usize, String>;

    /// `/clear confirm all` 的记忆部分前一步：把本机 `MEMORY.md` 现有的条目留一份副本，
    /// 返回它们的标识。**没有任何可发布的副作用**；删除的登记由调用方在本地清空成功后
    /// 经 [`SyncControl::commit_memory_clear`] 提交。**阻塞**。
    fn stash_memory_for_clear(&self) -> Result<Vec<String>, String>;

    /// `/clear confirm all` 的记忆部分后一步：本地清空已成功，把清空登记成发给所有设备
    /// 的删除。`ids` 是 [`SyncControl::stash_memory_for_clear`] 返回的那些。**阻塞**。
    fn commit_memory_clear(&self, ids: &[String]) -> Result<usize, String>;

    /// 本机数据变了，尽快发布：`turn_ended` 为真是一个 turn 刚结束（它写下的消息可以发了），
    /// 为假是别的改动（清空等）。不能阻塞。
    fn local_changed(&self, turn_ended: bool);
}

/// 这台设备在同步空间里时 `/clear confirm` 的范围。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClearScope {
    /// 只清本机，并退出同步空间。
    Local,
    /// 清本机，并把会话删除与记忆清空传到所有设备。
    All,
}

/// `/clear confirm <范围>` 的第三个词。
pub(super) fn clear_scope(content: &str) -> Option<ClearScope> {
    let word = content.split_whitespace().nth(2)?.to_ascii_lowercase();
    match word.as_str() {
        "local" => Some(ClearScope::Local),
        "all" => Some(ClearScope::All),
        _ => None,
    }
}

/// 这台设备在同步空间里时，确认 `/clear` 的两种写法。
pub(super) const CLEAR_SCOPE_HINT: &str = "这台设备在一个多端同步空间里，确认时要指明范围：\n\
     - /clear confirm local：只清空这台设备，并让它退出同步空间；其他设备上的会话与记忆不受影响\n\
     - /clear confirm all：清空这台设备，并把参与同步的会话与记忆（MEMORY.md）的删除同步出去。\
     各设备按自己的回收站保留设置保存副本。每日笔记（memory/daily/）只同步追加、不同步\
     删除：其他设备上的每日笔记不会被清掉，要清请到那台设备上再执行一次";

const SYNC_USAGE: &str =
    "用法：/sync [status|now|pause|resume|show <编号>|accept <编号>|reject <编号>]\n\
     - status：查看多端同步的状态与待处理事项（默认）\n\
     - now：立即同步一轮\n\
     - pause / resume：暂停 / 恢复同步\n\
     - show <编号>：查看一条待处理事项的详情\n\
     - accept / reject <编号>：接受 / 拒绝一项等本机确认的改动；retry、discard、dismiss 同理\
     （状态里写明了每一项能用的操作）\n\
     \n配对与存储设置不走命令：在设置面板的「多端同步」里操作，命令行版用启动参数 \
     rclaw --sync-join 加入同步空间。";

/// `/sync <操作> <编号>` 里处理待处理事项的操作。
const PENDING_ACTIONS: &[&str] = &["accept", "reject", "retry", "discard", "dismiss"];

/// 用户把配对码发进对话时的回复。
const PAIRING_CODE_NOTICE: &str =
    "⚠️ 这条消息里有多端同步的配对码，没有处理：没有保存到会话，也没有交给模型。\n\
     配对码等同于全部同步数据的钥匙，请不要发到对话里。要加入同步空间：在设置面板的「多端同步」\
     里粘贴或扫码；命令行版运行 rclaw --sync-join。\n\
     如果这条消息是经聊天软件（微信、QQ 等）发过来的，配对码已经经过了它们的服务器：建议在一台\
     已加入的设备上重置同步空间，换一个新的配对码。";

async fn status_text(sync: Arc<dyn SyncControl>) -> String {
    tokio::task::spawn_blocking(move || sync.status_text())
        .await
        .unwrap_or_else(|error| format!("读取同步状态失败：{error}"))
}

impl TurnManager {
    fn merges(&self) -> MutexGuard<'_, MergeState> {
        // 临界区里只有集合的增删，panic 留不下半截状态：中毒也照常用。
        self.merges
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    // ---- 合入登记（§7.3）----

    /// 同步引擎占住一个会话做合入，办法同 `/compact`：先占一个静默门槽位，再在 active 锁内
    /// 确认这个会话上没有会改历史的活动 turn，然后登记。登记期间 [`Self::spawn_turn`] 不在
    /// 这个会话上启动会改历史的 turn，撞上的消息稍后重投。守卫释放即注销。
    ///
    /// 返回 `None` 的情况：静默门被生命周期操作持有；会话上有会改历史的活动 turn（含
    /// `/compact`）；已经有一个合入登记；有消息正等着合入结束（让它们先走，合入下一轮再来）。
    /// 与 `/compact` 不同，登记不进 active map：`/cancel`、宿主的忙碌显示都看不到它。
    pub fn begin_merge(self: &Arc<Self>, channel: &str, chat_id: &str) -> Option<MergeGuard> {
        if !self.gate.try_begin_turn() {
            return None;
        }
        let chat = (channel.to_string(), chat_id.to_string());
        let registered = {
            let active = self.active.lock().expect("active map 锁未被毒化");
            let busy = active.iter().any(|(key, turn)| {
                key.channel == channel
                    && key.chat_id == chat_id
                    && turn.touches_history
                    && !turn.event_tx.is_closed()
            });
            let mut merges = self.merges();
            !busy && !merges.parked.contains_key(&chat) && merges.held.insert(chat.clone())
        };
        if !registered {
            self.gate.end_turn();
            return None;
        }
        Some(MergeGuard {
            manager: self.clone(),
            chat,
        })
    }

    /// 同步：这个会话上是否有合入登记。[`Self::spawn_turn`] 在持 active 锁时调用。
    pub(super) fn merge_registered(&self, channel: &str, chat_id: &str) -> bool {
        let merges = self.merges();
        !merges.held.is_empty()
            && merges
                .held
                .contains(&(channel.to_string(), chat_id.to_string()))
    }

    /// 同步：这个会话有消息在等着重投时，把 `msg` 排到它们后面（后到的不能抢先），返回
    /// 真。只管会改历史的消息——定时任务的轮次不读写会话历史，不受合入影响，照常去试
    /// （静默门关着的话它自己也会停下来排队）。
    pub(super) fn queue_behind_parked(&self, msg: &BusMessage) -> bool {
        if msg.is_cron_task() {
            return false;
        }
        let mut merges = self.merges();
        if merges.parked.is_empty() {
            return false;
        }
        let Some(queue) = merges
            .parked
            .get_mut(&(msg.channel.clone(), msg.chat_id.clone()))
        else {
            return false;
        };
        push_parked(queue, msg.clone());
        true
    }

    /// 把现在启动不了 turn 的消息（撞上合入、静默门关着）停下来，由重投任务稍后再交
    /// （每个会话至多一个任务）。不占路由循环。
    pub(super) fn park(self: &Arc<Self>, msg: BusMessage) {
        let chat = (msg.channel.clone(), msg.chat_id.clone());
        {
            let mut merges = self.merges();
            if let Some(queue) = merges.parked.get_mut(&chat) {
                // 另一条路由路径（turn 外壳的残留重路由）先一步停下了消息：排在后面。
                push_parked(queue, msg);
                return;
            }
            merges
                .parked
                .insert(chat.clone(), VecDeque::from([Parked::new(msg)]));
        }
        let manager = self.clone();
        tokio::spawn(async move { manager.redeliver_parked(chat).await });
    }

    /// 重投任务：每隔 [`MERGE_RETRY_STEP`] 把排在最前面的消息再交一次，能启动了就按到达
    /// 顺序把排着的消息都交出去。哪一条自己等过了 [`MERGE_RETRY_MAX`] 还是启动不了，才给它
    /// 回复「请稍后重试」（非用户来源的到这时才丢）。
    async fn redeliver_parked(self: Arc<Self>, chat: (String, String)) {
        loop {
            tokio::time::sleep(MERGE_RETRY_STEP).await;
            loop {
                let Some((msg, since)) = self.parked_front(&chat) else {
                    return;
                };
                match self.offer(&TurnKey::from_msg(&msg), &msg) {
                    Spawned::Yes => {}
                    Spawned::Merging | Spawned::GateHeld if since.elapsed() < MERGE_RETRY_MAX => {
                        break;
                    }
                    Spawned::Merging | Spawned::GateHeld => self.reject_busy(msg).await,
                }
                self.parked_pop(&chat);
            }
        }
    }

    /// 同步：排在最前面的消息（克隆）和它停下来的时刻。队列空了就把条目移除并返回
    /// `None`——这一步与 [`Self::queue_behind_parked`] 在同一把锁下，不会有消息排进一个
    /// 没人管的队列。
    fn parked_front(&self, chat: &(String, String)) -> Option<(BusMessage, Instant)> {
        let mut merges = self.merges();
        let front = merges
            .parked
            .get(chat)
            .and_then(|queue| queue.front())
            .map(|parked| (parked.msg.clone(), parked.since));
        if front.is_none() {
            merges.parked.remove(chat);
        }
        front
    }

    fn parked_pop(&self, chat: &(String, String)) {
        if let Some(queue) = self.merges().parked.get_mut(chat) {
            queue.pop_front();
        }
    }

    // ---- 同步引擎要问的几件事 ----

    /// 同步：该会话上是否有会改历史的活动 turn（含 `/compact`；cron 轮次与僵尸条目不算，
    /// 口径同 [`Self::has_other_active_turn`]）。正忙的会话不发布、不合入。
    pub fn is_history_busy(&self, channel: &str, chat_id: &str) -> bool {
        self.active
            .lock()
            .expect("active map 锁未被毒化")
            .iter()
            .any(|(key, turn)| {
                key.channel == channel
                    && key.chat_id == chat_id
                    && turn.touches_history
                    && !turn.event_tx.is_closed()
            })
    }

    /// 同步：会改历史的活动 turn 已经运行了至少 `at_least` 的会话（去重）。同步把它们写进
    /// 运行提示，别的设备打开同一会话时提醒用户。
    pub fn long_running_sessions(&self, at_least: Duration) -> Vec<(String, String)> {
        let mut sessions: Vec<(String, String)> = self
            .active
            .lock()
            .expect("active map 锁未被毒化")
            .iter()
            .filter(|(_, turn)| {
                turn.touches_history
                    && !turn.event_tx.is_closed()
                    && turn.started.elapsed() >= at_least
            })
            .map(|(key, _)| (key.channel.clone(), key.chat_id.clone()))
            .collect();
        sessions.sort();
        sessions.dedup();
        sessions
    }

    /// 在没有任何 turn、后台 review 运行时执行 `f`：持静默门，执行期间也不会有新的启动
    /// （与 skill 生命周期操作同一道门）。现在不空闲就不执行，返回 false。`f` 里只做很快
    /// 的本地操作。
    pub fn when_quiet(&self, f: &mut dyn FnMut()) -> bool {
        let Some(_quiescent) = self.gate.try_acquire() else {
            return false;
        };
        f();
        true
    }

    // ---- SyncControl ----

    /// 装入多端同步的控制入口。每实例只接受一次，重复安装被忽略并返回 false。
    pub fn set_sync_control(&self, sync: Arc<dyn SyncControl>) -> bool {
        self.sync.set(sync).is_ok()
    }

    pub(super) fn sync_control(&self) -> Option<Arc<dyn SyncControl>> {
        self.sync.get().cloned()
    }

    /// 告诉同步引擎本机数据变了。引擎是宿主插件：它的 panic 不能打断 turn 的收尾。
    pub(super) fn notify_sync(&self, turn_ended: bool) {
        let Some(sync) = self.sync.get() else {
            return;
        };
        let notified = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sync.local_changed(turn_ended)
        }));
        if notified.is_err() {
            tracing::error!("多端同步的 local_changed 回调 panic，已忽略");
        }
    }

    /// 消息里带着多端同步的配对码时把它拦下来（§11.2）：配对码等同于全部同步数据的钥匙，
    /// 不进日志、会话和模型。返回 `None` = 这条消息到此为止。[`Self::route`] 一进来就调它，
    /// 在记「用户原始输入」那条日志之前；同步开没开都拦——配对码在别的设备上照样管用。
    ///
    /// 用户发来的整条不处理，回一句该到哪里输入。系统内部来源的（定时任务、心跳、工作流
    /// 通知、bot 交接）只把配对码遮掉、照常处理：整条丢掉的话，等它结果的那一方会一直等
    /// 下去。
    pub(super) async fn intercept_pairing_code(&self, mut msg: BusMessage) -> Option<BusMessage> {
        use rclaw_store::redact::{contains_pairing_code, redact_pairing_codes};
        let found = [&msg.content, &msg.display_content, &msg.model_content]
            .into_iter()
            .any(|text| contains_pairing_code(text));
        if !found {
            return Some(msg);
        }
        if msg.source() != MessageSource::User {
            tracing::warn!(
                channel = %msg.channel,
                chat_id = %msg.chat_id,
                source = ?msg.source(),
                "内部消息里带着多端同步的配对码，已遮掉"
            );
            for text in [
                &mut msg.content,
                &mut msg.display_content,
                &mut msg.model_content,
            ] {
                let masked = redact_pairing_codes(text).into_owned();
                *text = masked;
            }
            return Some(msg);
        }
        tracing::warn!(
            channel = %msg.channel,
            chat_id = %msg.chat_id,
            "用户消息里带着多端同步的配对码：没有保存，也没有交给模型"
        );
        let _ = self
            .outbound_tx
            .send(BusMessage::new(
                msg.channel,
                msg.chat_id,
                PAIRING_CODE_NOTICE,
            ))
            .await;
        None
    }

    /// `/sync`：查看状态、立即同步、暂停、恢复，以及处理待处理事项（没有设置面板的宿主靠
    /// 它做「本机确认」；命令只认真实用户在特权会话里发的，模型和定时任务发不出来）。配对
    /// 与存储设置不走命令——配对码不能出现在会话文本里。
    pub(super) async fn handle_sync(&self, content: &str) -> String {
        let Some(sync) = self.sync_control() else {
            return "当前运行环境没有接入多端同步。".to_string();
        };
        let mut words = content.split_whitespace().skip(1);
        let sub = words
            .next()
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        let item = words.next().unwrap_or_default().to_string();
        match sub.as_str() {
            "" | "status" => status_text(sync).await,
            "show" if !item.is_empty() => {
                tokio::task::spawn_blocking(move || sync.pending_text(&item))
                    .await
                    .unwrap_or_else(|error| Err(format!("读取失败：{error}")))
                    .unwrap_or_else(|error| format!("⚠️ {error}"))
            }
            action if PENDING_ACTIONS.contains(&action) && !item.is_empty() => {
                match sync.resolve_pending(&item, action).await {
                    Ok(done) => format!("✅ {done}"),
                    Err(error) => format!("⚠️ {error}"),
                }
            }
            "now" => match sync.sync_now().await {
                Ok(()) => format!("✅ 已同步一轮。\n\n{}", status_text(sync).await),
                Err(error) => format!("⚠️ 同步没有完成：{error}"),
            },
            "pause" => match sync.set_paused(true).await {
                Ok(()) => "⏸ 多端同步已暂停。恢复请输入：/sync resume".to_string(),
                Err(error) => format!("⚠️ 暂停失败：{error}"),
            },
            "resume" => match sync.set_paused(false).await {
                Ok(()) => "▶️ 多端同步已恢复。".to_string(),
                Err(error) => format!("⚠️ 恢复失败：{error}"),
            },
            _ => SYNC_USAGE.to_string(),
        }
    }

    /// `/clear confirm local|all` 在清空本机之前要做的同步侧动作（§7.5）。调用方持静默门，
    /// 并且已经先过了本地安全预检（记忆目录安全闸）。成功返回附在清空摘要后面的一段说明
    /// 和「记忆清空待提交」的条目标识（`all` 才有）；失败返回给用户的整条回复，调用方
    /// 不再清空。
    pub(super) async fn prepare_clear(
        &self,
        sync: Arc<dyn SyncControl>,
        scope: ClearScope,
    ) -> Result<(String, Vec<String>), String> {
        match scope {
            ClearScope::Local => {
                // 同步引擎在两轮同步之间才办这件事。一轮跑得久（首次同步、网络卡住）时，
                // 路由任务不能拿着静默门一直等：到点就放弃，引擎排到它时看到没人等了就
                // 不退（恰好已经在退的话，重试时本机已不在空间里，直接清空）。
                match tokio::time::timeout(LEAVE_WAIT, sync.leave_space()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        return Err(format!(
                            "⚠️ 退出同步空间失败，没有清空任何内容：{error}\n\n\
                             可以重试：/clear confirm local"
                        ));
                    }
                    Err(_) => {
                        return Err(format!(
                            "⏳ 多端同步正忙（一轮同步还没跑完），{} 秒内没能退出同步空间，\
                             没有清空任何内容。\n\n请稍后重试：/clear confirm local",
                            LEAVE_WAIT.as_secs()
                        ));
                    }
                }
                Ok((
                    "\n\n多端同步：这台设备已退出同步空间；其他设备上的会话与记忆没有动。"
                        .to_string(),
                    Vec::new(),
                ))
            }
            ClearScope::All => {
                let sessions = sync.clone();
                let deleted = tokio::task::spawn_blocking(move || sessions.delete_published_sessions())
                    .await
                    .map_err(|error| format!("⚠️ 清空任务执行异常: {error}"))?
                    .map_err(|error| {
                        format!(
                            "⚠️ 把会话删除同步给其他设备时出错，/clear 已中止：{error}\n\n\
                             已登记的会话删除可能会同步出去。可以重试：\
                             /clear confirm all"
                        )
                    })?;
                // 记忆的条目先留副本（还没有任何可发布的副作用）；本地清空成功后才提交。
                let ids = {
                    tokio::task::spawn_blocking(move || sync.stash_memory_for_clear())
                        .await
                        .map_err(|error| format!("⚠️ 清空任务执行异常: {error}"))?
                        .map_err(|error| {
                            format!(
                                "⚠️ 清空之前给记忆留副本时出错，/clear 已中止：{error}\n\n\
                                 已登记的会话删除可能会同步出去。可以重试：\
                                 /clear confirm all"
                            )
                        })?
                };
                Ok((
                    format!(
                        "\n\n多端同步已把 {deleted} 个已发布会话的删除传给其他设备。\
                         记忆的删除等本机清空成功后再登记。\n\
                         每日笔记只同步追加、不同步删除：这次只清了这台设备上的，其他设备上的还在\
                         （以后新加入的设备也会收到）。要清掉它们，请到每台设备上各执行一次 \
                         /clear confirm all；要让它们从同步空间里也消失，各端清空之后在多端同步的\
                         设置里重置同步空间。"
                    ),
                    ids,
                ))
            }
        }
    }
}

fn push_parked(queue: &mut VecDeque<Parked>, msg: BusMessage) {
    if queue.len() >= PARKED_MAX {
        tracing::warn!(
            "等着重投的消息太多，丢弃新到的一条 ({}:{})",
            msg.channel,
            msg.chat_id
        );
        return;
    }
    queue.push_back(Parked::new(msg));
}

#[cfg(test)]
mod tests;
