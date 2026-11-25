//! 多端同步的装配（`docs/multi-device-sync.md` §12.1）。
//!
//! 同步引擎（`rclaw-sync`）与运行时（`rclaw-runtime`）互不依赖，在这里接到一起：
//! - 引擎经 [`rclaw_sync::SessionGate`] 占住会话做合入，背后是运行时的 `SessionLifecycle`；
//! - 运行时经 [`rclaw_runtime::SyncControl`] 执行 `/sync`、带范围的 `/clear`，并在 turn
//!   结束时通知引擎；
//! - 技能目录、定时任务调度器在工具层，各包一层交给引擎。
//!
//! 三个宿主（命令行、桌面、APK）走同一条路：bootstrap 备好 [`SyncSeed`]，宿主建出
//! `AgentLoop` 之后调 [`SyncSeed::attach`]。同步没开时引擎只是一个空句柄：不起线程、不建
//! 同步表、不碰磁盘。
//!
//! 另有两件宿主共用的事：把 `[sync]` 段写回 `rclaw.toml`（[`save_sync_config`]），和命令行
//! 的 `rclaw --sync-join`（[`join_from_stdin`]）。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context as _, Result};

use rclaw_runtime::{AgentLoop, BotRegistry, SessionLifecycle, SyncControl};
use rclaw_store::memory::MemoryStore;
use rclaw_store::session::SessionStore;
use rclaw_store::{CronJob, EditValue, RclawToml};
use rclaw_sync::engine::Trigger;
use rclaw_sync::status::{item_code, item_code_full};
use rclaw_sync::{
    CronHost, NoHost, PendingItem, Reload, SessionGate, SkillHost, SkillKind, SyncConfig, SyncDeps,
    SyncEngine, SyncEvent, SyncHost, SyncStatus,
};
use rclaw_tools::skill_sync::{self, SyncedSkill};
use rclaw_tools::{CronController, WebSearchControl};

use crate::config::Config;

/// turn 运行超过这么久，就把「这个会话正在这台设备上运行」写给其他设备看（§7.4）。
const RUNNING_HINT_AFTER: Duration = Duration::from_secs(10);
/// 进程退出之前的那次同步最多等这么久；等过 [`EXIT_SYNC_QUIET`] 还没完才告诉用户在等什么。
const EXIT_SYNC_WAIT: Duration = Duration::from_secs(3);
const EXIT_SYNC_QUIET: Duration = Duration::from_millis(700);
/// `/sync show` 里一段正文最多显示这么多字。
const SHOW_MAX_CHARS: usize = 4000;

/// `rclaw.toml` 所在的目录：同步的「数据根」，同步目录是它下面的 `sync/`。
pub fn data_root_of(config_path: &Path) -> PathBuf {
    let absolute = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
    match absolute.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// 装配同步引擎要用的东西。由 bootstrap 备好，宿主用 [`SyncSeed::attach`] 接上。
pub struct SyncSeed {
    config: SyncConfig,
    data_root: PathBuf,
    sessions: SessionStore,
    memory: MemoryStore,
    cron: Arc<CronController>,
    bots: Arc<BotRegistry>,
    web_search: Option<Arc<WebSearchControl>>,
    /// 宿主侧的重读入口：引擎打开连接前按磁盘上的 `rclaw.toml` 重读 `[sync]`，确认
    /// 启动配置没被同一数据目录里的另一个进程改掉。
    refresh_config: Option<rclaw_sync::ConfigRefresh>,
}

/// 宿主之间的差异。
pub struct SyncProfile {
    /// 宿主有会话列表、能打开同步来的会话（桌面、APK）。命令行没有：只同步文件类数据
    /// （记忆、人格、技能、bot、定时任务），库里不出现其他设备的会话（§13）。
    pub sessions: bool,
    /// 宿主环境：事件通知、网络是否按流量计费。
    pub host: Arc<dyn SyncHost>,
}

impl SyncProfile {
    /// 没有界面的宿主（命令行 / Termux）。
    pub fn headless() -> SyncProfile {
        SyncProfile {
            sessions: false,
            host: Arc::new(NoHost),
        }
    }
}

impl SyncSeed {
    pub(crate) fn new(
        config: SyncConfig,
        data_root: PathBuf,
        sessions: SessionStore,
        memory: MemoryStore,
        cron: Arc<CronController>,
        bots: Arc<BotRegistry>,
        web_search: Option<Arc<WebSearchControl>>,
        refresh_config: Option<rclaw_sync::ConfigRefresh>,
    ) -> SyncSeed {
        SyncSeed {
            config,
            data_root,
            sessions,
            memory,
            cron,
            bots,
            web_search,
            refresh_config,
        }
    }

    /// 把同步引擎接到 `agent` 上，配置里开着同步就启动它。必须在 `agent.run()` 之前调用。
    /// 返回的句柄宿主留着：设置面板、会话的删除与导入导出都经它。
    pub fn attach(self, agent: &AgentLoop, profile: SyncProfile) -> SyncEngine {
        let skills_dir = self.sessions.layout().skills_dir.clone();
        let engine = SyncEngine::new(
            SyncDeps {
                gate: Arc::new(RuntimeGate(agent.session_lifecycle())),
                host: Arc::new(HostBridge {
                    inner: profile.host,
                    bots: self.bots,
                    web_search: self.web_search,
                }),
                cron: Some(Arc::new(CronAdapter(self.cron))),
                skills: Some(Arc::new(SkillsAdapter(skills_dir))),
                sessions: self.sessions,
                data_root: self.data_root,
                memory: self.memory,
                platform: std::env::consts::OS.to_string(),
                version: crate::VERSION.to_string(),
                sessions_supported: profile.sessions,
                // 手机（APK 与 Termux）内存紧：一次读写的批量取小值。
                low_memory: cfg!(target_os = "android"),
                refresh_config: self.refresh_config,
            },
            self.config,
        );
        if !agent.set_sync_control(Arc::new(Control(engine.clone()))) {
            tracing::warn!("这个 agent loop 已经接过多端同步，忽略重复的接入");
        }
        engine.start();
        engine
    }
}

/// 进程退出之前调（命令行版）：把最后这段时间的改动（记忆、人格、技能、bot、定时任务）
/// 同步出去，最多等 [`EXIT_SYNC_WAIT`]。不做的话，退出前几秒里的改动要等到下次启动才到
/// 其他设备上。
///
/// 只在同步连着、上一轮也通的时候做：没开、没配对、暂停、离线、出错、同一个数据目录的
/// 另一个进程在做同步时直接返回，不让退出白等。没发出去的改动都在本机磁盘上，下次启动时
/// 补发；等到一半进程退出也没关系，引擎的每一步都经得起中断。
pub async fn sync_before_exit(engine: &SyncEngine) {
    if !engine.config().enabled {
        return;
    }
    let probe = engine.clone();
    let state = tokio::task::spawn_blocking(move || probe.status().state).await;
    if !matches!(state.as_deref(), Ok("idle" | "syncing")) {
        return;
    }
    let round = engine.sync_now();
    tokio::pin!(round);
    let done = match tokio::time::timeout(EXIT_SYNC_QUIET, &mut round).await {
        Ok(done) => Some(done),
        Err(_) => {
            eprintln!(
                "多端同步：正在把最后的改动同步出去（最多再等 {} 秒，Ctrl+C 立即退出）…",
                (EXIT_SYNC_WAIT - EXIT_SYNC_QUIET).as_secs().max(1)
            );
            tokio::time::timeout(EXIT_SYNC_WAIT - EXIT_SYNC_QUIET, &mut round)
                .await
                .ok()
        }
    };
    match done {
        Some(Ok(())) => tracing::info!("退出前已同步一轮"),
        Some(Err(error)) => tracing::info!("退出前的同步没有完成（下次启动时补上）: {error}"),
        None => tracing::info!("退出前的同步没有在时限内完成（下次启动时补上）"),
    }
}

// ---------------------------------------------------------------------------
// 引擎 → 运行时 / 工具层
// ---------------------------------------------------------------------------

/// 与运行中的 turn 互斥（§7.3）。
struct RuntimeGate(SessionLifecycle);

impl SessionGate for RuntimeGate {
    fn begin_merge(&self, channel: &str, chat_id: &str) -> Option<Box<dyn Send>> {
        self.0
            .begin_merge(channel, chat_id)
            .map(|guard| Box::new(guard) as Box<dyn Send>)
    }

    fn is_busy(&self, channel: &str, chat_id: &str) -> bool {
        self.0.is_history_busy(channel, chat_id)
    }

    fn running_sessions(&self) -> Vec<(String, String)> {
        self.0.long_running_sessions(RUNNING_HINT_AFTER)
    }

    fn session_changed(&self, channel: &str, chat_id: &str, _deleted: bool) {
        self.0.forget_session(channel, chat_id);
    }

    fn when_quiet(&self, f: &mut dyn FnMut()) -> bool {
        self.0.when_quiet(f)
    }
}

/// 定时任务调度器（§8.6）。调度服务与它共用同一张内存里的任务表，改完不用再通知谁。
struct CronAdapter(Arc<CronController>);

impl CronHost for CronAdapter {
    fn jobs(&self) -> Vec<CronJob> {
        self.0.list_snapshot()
    }

    fn add(&self, job: CronJob) -> std::result::Result<String, String> {
        self.0.add(job).map_err(|error| error.to_string())
    }

    fn add_disabled(&self, job: CronJob) -> std::result::Result<String, String> {
        self.0.add_disabled(job).map_err(|error| error.to_string())
    }

    fn set_enabled(&self, id: &str, enabled: bool) -> std::result::Result<(), String> {
        self.0
            .set_enabled(id, enabled)
            .map_err(|error| error.to_string())
    }

    fn remove(&self, id: &str) -> std::result::Result<bool, String> {
        self.0.remove(id).map_err(|error| error.to_string())
    }
}

/// 技能目录（§8.4）：归属、生命周期锁、归档都在工具层。
struct SkillsAdapter(PathBuf);

impl SkillHost for SkillsAdapter {
    fn syncable(&self) -> std::result::Result<Vec<String>, String> {
        skill_sync::syncable(&self.0)
    }

    fn kind(&self, name: &str) -> SkillKind {
        match skill_sync::kind(&self.0, name) {
            SyncedSkill::Builtin => SkillKind::Builtin,
            SyncedSkill::Parked => SkillKind::Archived,
            SyncedSkill::Live { pinned } => SkillKind::Live { pinned },
            SyncedSkill::Absent => SkillKind::Absent,
        }
    }

    fn in_use(&self) -> bool {
        skill_sync::in_use(&self.0)
    }

    fn staging_dir(&self) -> PathBuf {
        skill_sync::staging_dir(&self.0)
    }

    fn recover(&self) -> bool {
        skill_sync::recover(&self.0, &rclaw_sync::files::skills::is_local_only)
    }

    fn install(&self, name: &str, staged: &Path) -> std::result::Result<(), String> {
        skill_sync::install(
            &self.0,
            name,
            staged,
            &rclaw_sync::files::skills::is_local_only,
        )
    }

    fn remove(&self, name: &str) -> std::result::Result<bool, String> {
        skill_sync::remove(&self.0, name, &rclaw_sync::files::skills::is_local_only)
    }

    fn prune(&self, name: &str) {
        if let Err(error) = skill_sync::prune_replaced(&self.0, name) {
            tracing::debug!("清理技能「{name}」被同步换下来的旧版本失败: {error}");
        }
    }
}

/// 宿主环境。事件与网络状态交给宿主自己的实现；「重新加载」里运行时这一侧要做的事在
/// 这里先做掉：
/// - bot：注册表要重读定义、重建提示；
/// - 记忆、技能：运行时每个 turn 现读现用，不用做什么；
/// - 人格：生效人格的改动下次启动才生效（§8.3）；
/// - 定时任务：引擎是经调度器改的任务表，调度服务已经看到了。
struct HostBridge {
    inner: Arc<dyn SyncHost>,
    bots: Arc<BotRegistry>,
    web_search: Option<Arc<WebSearchControl>>,
}

impl SyncHost for HostBridge {
    fn is_metered(&self) -> bool {
        self.inner.is_metered()
    }

    fn emit(&self, event: SyncEvent) {
        self.inner.emit(event);
    }

    fn reload(&self, what: Reload) {
        if what != Reload::Bots {
            self.inner.reload(what);
            return;
        }
        // 重建 bot 的提示要读一批文件：不占引擎的线程。做完再告诉宿主刷新界面。
        let (inner, bots, search) = (
            self.inner.clone(),
            self.bots.clone(),
            self.web_search.clone(),
        );
        let spawned = std::thread::Builder::new()
            .name("rclaw-sync-reload".into())
            .spawn(move || {
                bots.reload(search.as_deref());
                inner.reload(what);
            });
        if let Err(error) = spawned {
            tracing::warn!("重新加载 bot 定义的线程没能启动: {error}");
            self.bots.reload(self.web_search.as_deref());
            self.inner.reload(what);
        }
    }
}

// ---------------------------------------------------------------------------
// 运行时 → 引擎
// ---------------------------------------------------------------------------

/// `/sync`、带范围的 `/clear`、turn 结束时的发布通知。
struct Control(SyncEngine);

impl Control {
    /// 按 `/sync` 里给的编号（或完整标识）找到那条待处理事项。阻塞（读库）。
    fn find(&self, key: &str) -> std::result::Result<PendingItem, String> {
        find_item(&self.0.status().pending, key).cloned()
    }
}

#[async_trait::async_trait]
impl SyncControl for Control {
    fn in_space(&self) -> std::result::Result<bool, String> {
        self.0.in_space().map_err(|error| error.to_string())
    }

    fn status_text(&self) -> String {
        render_status(&self.0.status())
    }

    async fn sync_now(&self) -> std::result::Result<(), String> {
        self.0.sync_now().await.map_err(|error| error.to_string())
    }

    async fn set_paused(&self, paused: bool) -> std::result::Result<(), String> {
        self.0
            .set_paused(paused)
            .await
            .map_err(|error| error.to_string())
    }

    fn pending_text(&self, item: &str) -> std::result::Result<String, String> {
        let item = self.find(item)?;
        let detail = self
            .0
            .pending_detail(&item.id)
            .map_err(|error| error.to_string())?;
        Ok(render_detail(&item, &detail))
    }

    async fn resolve_pending(
        &self,
        item: &str,
        action: &str,
    ) -> std::result::Result<String, String> {
        let (engine, key) = (self.0.clone(), item.to_string());
        let item = tokio::task::spawn_blocking(move || Control(engine).find(&key))
            .await
            .map_err(|error| format!("读取待处理事项失败: {error}"))??;
        if !item.actions.iter().any(|allowed| allowed == action) {
            return Err(format!(
                "这一项不支持 {action}。{}",
                actions_hint(&item)
                    .unwrap_or_else(|| "它不需要处理，情况消失后会自己撤掉。".into())
            ));
        }
        // 用户敲的编号对上的就是这一版（编号由标识和版本算出）：把它交给引擎，这期间又
        // 来了更新的一版的话，引擎不办。
        self.0
            .resolve_pending(&item.id, action, item.version())
            .await
            .map_err(|error| error.to_string())?;
        Ok(action_done(&item, action))
    }

    async fn leave_space(&self) -> std::result::Result<(), String> {
        self.0
            .leave_space()
            .await
            .map_err(|error| error.to_string())
    }

    fn delete_published_sessions(&self) -> std::result::Result<usize, String> {
        self.0
            .delete_published_sessions()
            .map_err(|error| error.to_string())
    }

    fn stash_memory_for_clear(&self) -> std::result::Result<(Vec<String>, String), String> {
        self.0
            .stash_memory_for_clear()
            .map_err(|error| error.to_string())
    }

    fn commit_memory_clear(&self, ids: &[String], op: &str) -> std::result::Result<usize, String> {
        self.0
            .commit_memory_clear(ids, op)
            .map_err(|error| error.to_string())
    }

    fn local_changed(&self, turn_ended: bool) {
        self.0.notify(if turn_ended {
            Trigger::TurnEnded
        } else {
            Trigger::LocalChange
        });
    }
}

/// 用户至少要敲这么多位编号（[`item_code`] 的长度）。
const CODE_MIN_LEN: usize = 6;

/// 按 `/sync` 里给的编号找事项。编号由事项的标识和版本一起算出：等确认的改动换了一版，
/// 旧编号就找不到了——用户照着旧编号 `accept`，不会接受到一份没看过的内容。所以带版本的
/// 事项只认编号（撞了可以多敲几位）；没有版本的事项也可以用完整标识。
fn find_item<'a>(
    items: &'a [PendingItem],
    key: &str,
) -> std::result::Result<&'a PendingItem, String> {
    let key = key
        .trim()
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    let matched: Vec<&PendingItem> = items
        .iter()
        .filter(|item| {
            (key.len() >= CODE_MIN_LEN && item_code_full(item).starts_with(&key))
                || (item.version().is_none() && item.id == key)
        })
        .collect();
    match matched.as_slice() {
        [item] => Ok(*item),
        [] => Err(format!(
            "没有编号为 {key} 的待处理事项（可能已经处理过了，或者它的内容更新之后换了编号）。\
             用 /sync status 看现在有哪些。"
        )),
        // 六位编号撞了（极少见）：让用户多敲几位。
        several => Err(format!(
            "编号 {key} 对应不止一项，请改用更长的编号：{}",
            several
                .iter()
                .map(|item| item_code_full(item))
                .collect::<Vec<_>>()
                .join("、")
        )),
    }
}

/// 「记忆文件里原有的条目都不见了」：它的「接受 / 拒绝」不是接受、拒绝一份远端改动，在
/// 命令行里把后果写明白。
const KIND_MEM_WIPE: &str = "mem-wipe";

/// 处理完一条事项之后回给用户的一句话。
fn action_done(item: &PendingItem, action: &str) -> String {
    let done = match (item.kind.as_str(), action) {
        (KIND_MEM_WIPE, "accept") => {
            return "已确认是有意删除：这些记忆条目的删除会同步到所有设备\
                    （其他设备各自在回收站里留一份）。"
                .to_string();
        }
        (KIND_MEM_WIPE, "reject") => {
            return "已取消：这些记忆条目不会从其他设备上删除，下一轮同步时恢复到这台设备上。"
                .to_string();
        }
        (_, "accept") => "已接受",
        (_, "reject") => "已拒绝",
        (_, "retry") => "已重试",
        (_, "discard") => "已丢弃",
        _ => "已关闭",
    };
    terminal_text(&format!("{done}：{}", item.title))
}

fn action_label(item: &PendingItem, action: &str) -> &'static str {
    match (item.kind.as_str(), action) {
        (KIND_MEM_WIPE, "accept") => "是有意删的，在所有设备上删除",
        (KIND_MEM_WIPE, "reject") => "不是有意的，恢复到这台设备",
        (_, "accept") => "接受",
        (_, "reject") => "拒绝",
        (_, "retry") => "重试",
        (_, "discard") => "丢弃",
        _ => "关闭",
    }
}

/// 「接受：/sync accept a1b2c3　拒绝：/sync reject a1b2c3」。没有可用的操作返回 `None`。
fn actions_hint(item: &PendingItem) -> Option<String> {
    if item.actions.is_empty() {
        return None;
    }
    let code = item_code(item);
    Some(
        item.actions
            .iter()
            .map(|action| format!("{}：/sync {action} {code}", action_label(item, action)))
            .collect::<Vec<_>>()
            .join("　"),
    )
}

fn state_label(state: &str) -> &'static str {
    match state {
        "off" => "未开启",
        "setup" => "已开启，还没有创建或加入同步空间",
        "standby" => "待命（另一个 RClaw 进程正在用这个数据目录同步）",
        "confirm" => "等待确认",
        "paused" => "已暂停",
        "syncing" => "同步中",
        "idle" => "空闲",
        "offline" => "离线（连不上存储）",
        "error" => "出错",
        _ => "未知",
    }
}

fn local_time(ms: i64) -> Option<String> {
    use chrono::TimeZone as _;
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|at| at.format("%m-%d %H:%M:%S").to_string())
}

fn terminal_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let control = ch.is_control() && !matches!(ch, '\n' | '\t');
        let directional = matches!(ch, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if control || directional {
            out.push_str(&format!("⟦U+{:04X}⟧", u32::from(ch)));
        } else {
            out.push(ch);
        }
    }
    out
}

/// `/sync status` 的正文。
pub fn render_status(status: &SyncStatus) -> String {
    let mut out = format!("多端同步：{}", state_label(&status.state));
    if let Some(message) = status.message.as_deref().filter(|m| !m.trim().is_empty()) {
        out.push_str(&format!("\n  {message}"));
    }
    if status.state == "off" {
        out.push_str(
            "\n在 rclaw.toml 的 [sync] 段里开启；命令行版用 rclaw --sync-join 粘贴配对码加入同步空间。",
        );
        return terminal_text(&out);
    }
    out.push_str(&format!("\n本机：{}", status.device_name));
    if let Some(space) = &status.space_id {
        let short: String = space.chars().take(8).collect();
        out.push_str(&format!("　同步空间：{short}…"));
    }
    let scopes = if status.scopes.is_empty() {
        "（无）".to_string()
    } else {
        status.scopes.join("、")
    };
    out.push_str(&format!("\n存储：{}　范围：{scopes}", status.transport));
    match local_time(status.last_sync_ms).filter(|_| status.last_sync_ms > 0) {
        Some(at) => out.push_str(&format!("\n上次同步：{at}")),
        None => out.push_str("\n上次同步：这次启动以来还没有完整跑完一轮"),
    }
    out.push_str(&format!(
        "\n待发出 {} 条，待合入 {} 条",
        status.outbox, status.inbox
    ));
    if status.uploads > 0 {
        out.push_str(&format!("，待上传附件 {} 个", status.uploads));
    }
    let others: Vec<_> = status.devices.iter().filter(|device| !device.me).collect();
    if !others.is_empty() {
        out.push_str(&format!("\n其他设备（{}）：", others.len()));
        for device in others {
            let mut notes = Vec::new();
            if !device.platform.is_empty() {
                notes.push(device.platform.clone());
            }
            if let Some(at) = local_time(device.seen_ms).filter(|_| device.seen_ms > 0) {
                notes.push(format!("最近出现 {at}"));
            }
            if device.lost {
                notes.push("已标记丢失".into());
            } else if device.stale {
                notes.push("很久没出现".into());
            }
            if device.behind > 0 {
                notes.push(format!("有 {} 条还没处理完", device.behind));
            }
            out.push_str(&format!("\n  - {}", device.name));
            if !notes.is_empty() {
                out.push_str(&format!("（{}）", notes.join("，")));
            }
        }
    }
    if !status.pending.is_empty() {
        out.push_str(&format!("\n待处理（{}）：", status.pending.len()));
        for item in &status.pending {
            let code = item_code(item);
            out.push_str(&format!("\n  [{code}] {}", item.title));
            if let Some(origin) = &item.origin {
                out.push_str(&format!("（来自「{origin}」）"));
            }
            match actions_hint(item) {
                Some(hint) => out.push_str(&format!("\n      查看：/sync show {code}　{hint}")),
                None => out.push_str(&format!("\n      查看：/sync show {code}")),
            }
        }
    }
    terminal_text(&out)
}

fn clip(text: &str) -> String {
    let total = text.chars().count();
    if total <= SHOW_MAX_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(SHOW_MAX_CHARS).collect();
    format!("{head}\n…（共 {total} 字，只显示了前 {SHOW_MAX_CHARS} 字）")
}

fn push_side(out: &mut String, title: &str, text: Option<&str>, missing: &str) {
    out.push_str(&format!("\n\n──── {title} ────\n"));
    match text {
        Some(text) if !text.is_empty() => out.push_str(&clip(text)),
        Some(_) => out.push_str("（空）"),
        None => out.push_str(missing),
    }
}

/// `/sync show <编号>` 的正文：这条事项说了什么、要确认的改动的新旧内容。
pub fn render_detail(item: &PendingItem, detail: &serde_json::Value) -> String {
    let mut out = format!("[{}] {}", item_code(item), item.title);
    if let Some(origin) = &item.origin {
        out.push_str(&format!("（来自「{origin}」）"));
    }
    if !item.detail.trim().is_empty() {
        out.push_str(&format!("\n{}", item.detail));
    }
    match item.kind.as_str() {
        "persona" | "bot" => {
            if let Some(path) = detail["path"].as_str() {
                out.push_str(&format!("\n文件：{path}"));
            }
            push_side(
                &mut out,
                "本机现在的内容",
                detail["local"].as_str(),
                "（本机没有）",
            );
            push_side(
                &mut out,
                "收到的内容",
                detail["remote"].as_str(),
                "（对方把它删除了）",
            );
        }
        "skill" => {
            let deleting = detail["op"].as_str() == Some("del");
            if deleting {
                out.push_str("\n对方把这个技能删除了；接受之后本机的版本会移入归档（可恢复）。");
            }
            for file in detail["files"].as_array().into_iter().flatten() {
                let status = match file["status"].as_str() {
                    Some("added") => "新增",
                    Some("modified") => "修改",
                    Some("removed") => "删除",
                    _ => "变化",
                };
                let exec = if file["exec"].as_bool() == Some(true) {
                    "　⚠ 可执行内容"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "\n  {status}　{}（{} 字节）{exec}",
                    file["path"].as_str().unwrap_or("?"),
                    file["size"].as_u64().unwrap_or(0)
                ));
            }
            if let Some(unchanged) = detail["unchanged"].as_u64().filter(|n| *n > 0) {
                out.push_str(&format!("\n  另有 {unchanged} 个文件没有变化"));
            }
            out.push_str("\n（文件的具体内容请在桌面版 / 手机版的「多端同步」面板里对比。）");
        }
        _ => {}
    }
    if let Some(hint) = actions_hint(item) {
        out.push_str(&format!("\n\n{hint}"));
    }
    terminal_text(&out)
}

// ---------------------------------------------------------------------------
// `[sync]` 段写回 rclaw.toml
// ---------------------------------------------------------------------------

fn int(value: u64) -> Result<EditValue> {
    Ok(EditValue::Int(
        i64::try_from(value).context("数值超出 TOML 整数范围")?,
    ))
}

/// `before` → `after` 之间变了的键，写成 `rclaw.toml` 的点路径赋值。
fn sync_edits(before: &SyncConfig, after: &SyncConfig) -> Result<Vec<(String, EditValue)>> {
    // 逐字段解构：`SyncConfig` 以后加了字段，这里编译不过，不会漏写。
    let SyncConfig {
        enabled,
        device_name,
        transport,
        scopes,
        channels,
        poll_interval_sec,
        background_interval_sec,
        presend_pull_wait_ms,
        attachment_max_mb,
        trash_days,
        webdav,
        file,
        s3,
    } = after;
    let rclaw_sync::config::WebdavSection {
        url,
        username,
        allow_http,
        pinned_cert_sha256,
    } = webdav;
    let rclaw_sync::config::FileSection { dir } = file;
    let rclaw_sync::config::S3Section {
        endpoint,
        region,
        bucket,
        prefix,
        access_key_id,
        path_style,
        oss_forbid_overwrite,
        allow_http: s3_allow_http,
    } = s3;

    let text = |value: &String| EditValue::Str(value.clone());
    let list = |value: &Vec<String>| EditValue::StrArray(value.clone());
    let all = [
        (
            "enabled",
            before.enabled != *enabled,
            EditValue::Bool(*enabled),
        ),
        (
            "device_name",
            before.device_name != *device_name,
            text(device_name),
        ),
        ("transport", before.transport != *transport, text(transport)),
        ("scopes", before.scopes != *scopes, list(scopes)),
        ("channels", before.channels != *channels, list(channels)),
        (
            "poll_interval_sec",
            before.poll_interval_sec != *poll_interval_sec,
            int(*poll_interval_sec)?,
        ),
        (
            "background_interval_sec",
            before.background_interval_sec != *background_interval_sec,
            int(*background_interval_sec)?,
        ),
        (
            "presend_pull_wait_ms",
            before.presend_pull_wait_ms != *presend_pull_wait_ms,
            int(*presend_pull_wait_ms)?,
        ),
        (
            "attachment_max_mb",
            before.attachment_max_mb != *attachment_max_mb,
            int(*attachment_max_mb)?,
        ),
        (
            "trash_days",
            before.trash_days != *trash_days,
            int(*trash_days)?,
        ),
        ("webdav.url", before.webdav.url != *url, text(url)),
        (
            "webdav.username",
            before.webdav.username != *username,
            text(username),
        ),
        (
            "webdav.allow_http",
            before.webdav.allow_http != *allow_http,
            EditValue::Bool(*allow_http),
        ),
        (
            "webdav.pinned_cert_sha256",
            before.webdav.pinned_cert_sha256 != *pinned_cert_sha256,
            text(pinned_cert_sha256),
        ),
        ("file.dir", before.file.dir != *dir, text(dir)),
        (
            "s3.endpoint",
            before.s3.endpoint != *endpoint,
            text(endpoint),
        ),
        ("s3.region", before.s3.region != *region, text(region)),
        ("s3.bucket", before.s3.bucket != *bucket, text(bucket)),
        ("s3.prefix", before.s3.prefix != *prefix, text(prefix)),
        (
            "s3.access_key_id",
            before.s3.access_key_id != *access_key_id,
            text(access_key_id),
        ),
        (
            "s3.path_style",
            before.s3.path_style != *path_style,
            EditValue::Bool(*path_style),
        ),
        (
            "s3.oss_forbid_overwrite",
            before.s3.oss_forbid_overwrite != *oss_forbid_overwrite,
            EditValue::Bool(*oss_forbid_overwrite),
        ),
        (
            "s3.allow_http",
            before.s3.allow_http != *s3_allow_http,
            EditValue::Bool(*s3_allow_http),
        ),
    ];
    Ok(all
        .into_iter()
        .filter(|(_, changed, _)| *changed)
        .map(|(key, _, value)| (format!("sync.{key}"), value))
        .collect())
}

/// 把 `[sync]` 的设置写回 `rclaw.toml`：只动 `before` → `after` 之间变了的键，注释与其余
/// 内容保真。改完的全文先过一遍配置校验，不合规就不落盘。文件不存在时新建。
///
/// 存储的密码、同步空间的密钥不经过这里：它们不在 `rclaw.toml` 里。
pub fn save_sync_config(config_path: &Path, before: &SyncConfig, after: &SyncConfig) -> Result<()> {
    after
        .validate()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let edits = sync_edits(before, after)?;
    if edits.is_empty() {
        return Ok(());
    }
    if !config_path.exists() {
        rclaw_store::atomic_write(config_path, b"")
            .with_context(|| format!("新建 {} 失败", config_path.display()))?;
    }
    let file = RclawToml::new(config_path);
    let preview = file
        .apply_edits_preview(&edits)
        .map_err(anyhow::Error::msg)?;
    Config::parse_str(&preview).context("改动没有通过配置校验，rclaw.toml 没有动")?;
    file.write_text(&preview).map_err(anyhow::Error::msg)
}

// ---------------------------------------------------------------------------
// 命令行：rclaw --sync-join
// ---------------------------------------------------------------------------

/// 从标准输入读一行，不回显：标准输入是终端时关掉回显；是管道时照常读。
fn read_hidden_line() -> Result<String> {
    use std::io::{BufRead as _, IsTerminal as _};
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return rpassword::read_password().context("读取输入失败");
    }
    let mut line = String::new();
    stdin.lock().read_line(&mut line).context("读取输入失败")?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// 读配对码，不回显。粘贴进来的配对码可能被折成了几行：凑不成完整的一个就接着读下一行
/// （`next_line` 每次给一行；读到头、读到空行、或者一看就不是配对码时停）。
///
/// 只读第一行的话，剩下的几行会留在终端的输入里——本程序一退出，shell 就把它们回显出来、
/// 当成命令执行，配对码的后半截就进了屏幕和命令历史。
///
/// 回显是一行一行关的（`rpassword`），两行之间有一瞬间是开着的。粘贴进来的几行是一次到
/// 的、到的时候回显关着，不受影响；只有恰好在那一瞬间敲进来的字符会显示出来。
fn read_pairing_code(mut next_line: impl FnMut() -> Result<String>) -> Result<String> {
    let mut code = next_line()?;
    let mut hinted = false;
    while rclaw_sync::PairingCode::is_incomplete(&code) {
        if !hinted {
            eprintln!("（配对码还不完整，接着读后面的行；已经粘贴完的话再按一次回车）");
            hinted = true;
        }
        // 后面的行读不出来（输入结束了）就按已经读到的算：下面的校验会说它不对。
        let more = next_line().unwrap_or_default();
        if more.trim().is_empty() {
            break;
        }
        code.push_str(more.trim());
    }
    Ok(code)
}

/// `rclaw --sync-join`（§11.2）：从标准输入读配对码（不回显），加入同步空间，把设置写回
/// `rclaw.toml`。下次正常启动时开始同步。
///
/// 配对码等同于全部同步数据的钥匙：这里不打印它，也不写日志（此时日志还没有初始化）。
pub async fn join_from_stdin(config_path: &Path) -> Result<()> {
    let data_root = data_root_of(config_path);
    let cfg = Config::load_from_dir(&data_root)?;
    eprintln!("在一台已经加入同步空间的设备上打开「多端同步」→「添加设备」，复制配对码。");
    eprintln!("粘贴配对码后回车（输入不会显示）：");
    let code = read_pairing_code(read_hidden_line)?;
    if code.trim().is_empty() {
        bail!("没有读到配对码");
    }
    let needs_secret = rclaw_sync::engine::join_needs_secret(&data_root, &cfg.sync, &code)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let secret = if needs_secret {
        eprintln!(
            "这个配对码没有带存储设置，将使用 rclaw.toml 的 [sync] 里配置的存储（{}）。",
            cfg.sync.transport
        );
        eprintln!("输入它的密码后回车（输入不会显示）：");
        let mut secret = read_hidden_line()?;
        if secret.is_empty() {
            // 粘贴配对码时多带进来的一个空行会落到这里：再问一次，不把它当成密码。
            eprintln!("（没有读到内容。请再输入一次；密码确实是空的就直接回车）");
            secret = read_hidden_line()?;
        }
        Some(secret)
    } else {
        None
    };
    let joined =
        rclaw_sync::engine::join_space_offline(&data_root, &cfg.sync, &code, secret.as_deref())
            .await
            .map_err(|error| anyhow::anyhow!("加入同步空间失败：{error}"))?;
    save_sync_config(config_path, &cfg.sync, &joined).with_context(|| {
        format!(
            "密钥已经存好，但设置没能写回 {}。修好这个文件之后用同一个配对码再执行一次 rclaw --sync-join",
            config_path.display()
        )
    })?;
    let scopes = joined.scope_set().without_sessions();
    let mut names = Vec::new();
    for (on, name) in [
        (scopes.memory, "记忆"),
        (scopes.persona, "人格"),
        (scopes.skills, "技能"),
        (scopes.bots, "bot"),
        (scopes.cron, "定时任务"),
    ] {
        if on {
            names.push(name);
        }
    }
    println!("已加入同步空间，设置已写入 {}。", config_path.display());
    if names.is_empty() {
        println!(
            "注意：[sync] scopes 里没有命令行版能同步的内容（它不同步会话）。可以加上 memory、persona、skills、bots、cron。"
        );
    } else {
        println!(
            "下次启动 rclaw 时开始同步：{}（命令行版不同步会话）。用 /sync status 查看状态。",
            names.join("、")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rclaw_sync::status::DeviceStatus;

    fn item(id: &str, kind: &str, actions: &[&str]) -> PendingItem {
        PendingItem {
            id: id.into(),
            kind: kind.into(),
            title: format!("标题 {id}"),
            detail: "说明".into(),
            origin: Some("手机".into()),
            at_ms: 1,
            actions: actions.iter().map(|action| action.to_string()).collect(),
            payload: serde_json::Value::Null,
        }
    }

    #[test]
    fn data_root_is_the_directory_of_the_config_file() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(data_root_of(Path::new("rclaw.toml")), cwd);
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(data_root_of(&dir.path().join("rclaw.toml")), dir.path());
    }

    /// 折成几行粘贴进来的配对码要整个读完，不把后半截留在终端的输入里。
    #[test]
    fn a_wrapped_pairing_code_is_read_to_its_end() {
        let whole = rclaw_sync::PairingCode {
            space_id: rclaw_sync::secrets::new_space_id(),
            key: rclaw_sync::crypto::SpaceKey::generate().to_base64(),
            storage: None,
        }
        .encode()
        .unwrap();
        // 喂给它的行；返回读到的配对码和没读走的行数。
        let read = |lines: &[&str]| {
            let mut rest = lines.iter();
            let code = read_pairing_code(|| match rest.next() {
                Some(line) => Ok(line.to_string()),
                None => anyhow::bail!("输入结束了"),
            })
            .unwrap();
            (code, rest.count())
        };
        // 一行给全：只读这一行。
        assert_eq!(read(&[&whole, "后面的命令"]), (whole.clone(), 1));
        // 折成三行、带缩进：都读走，拼回原样；再后面的不碰。
        let (a, rest) = whole.split_at(20);
        let (b, c) = rest.split_at(30);
        let indented = format!("  {b}");
        assert_eq!(read(&[a, &indented, c, "后面的命令"]), (whole.clone(), 1));
        // 一直凑不齐：读到空行为止，读到输入结束为止。
        assert_eq!(read(&[a, b, "", "后面的命令"]), (format!("{a}{b}"), 1));
        assert_eq!(read(&[a, b]), (format!("{a}{b}"), 0));
        // 一看就不是配对码：不等后面的行。
        assert_eq!(read(&["hello", "后面的命令"]), ("hello".to_string(), 1));
        assert_eq!(read(&["", "后面的命令"]), (String::new(), 1));
    }

    #[test]
    fn status_text_lists_devices_and_pending_items_with_their_commands() {
        let off = render_status(&SyncStatus {
            state: "off".into(),
            ..SyncStatus::default()
        });
        assert!(
            off.contains("未开启") && off.contains("--sync-join"),
            "{off}"
        );

        let persona = item("persona-0011223344556677", "persona", &["accept", "reject"]);
        let halted = item("upgrade-00112233445566ff", "upgrade", &[]);
        let code = item_code(&persona);
        let status = SyncStatus {
            state: "idle".into(),
            transport: "webdav".into(),
            space_id: Some("0123456789abcdef0123".into()),
            device_name: "终端".into(),
            scopes: vec!["memory".into(), "persona".into()],
            last_sync_ms: 0,
            outbox: 2,
            inbox: 1,
            devices: vec![
                DeviceStatus {
                    id: "aa".into(),
                    name: "终端".into(),
                    platform: "linux".into(),
                    version: "1".into(),
                    me: true,
                    seen_ms: 0,
                    stale: false,
                    lost: false,
                    behind: 0,
                },
                DeviceStatus {
                    id: "bb".into(),
                    name: "手机".into(),
                    platform: "android".into(),
                    version: "1".into(),
                    me: false,
                    seen_ms: 0,
                    stale: true,
                    lost: false,
                    behind: 3,
                },
            ],
            pending: vec![persona.clone(), halted.clone()],
            ..SyncStatus::default()
        };
        let text = render_status(&status);
        assert!(text.starts_with("多端同步：空闲"), "{text}");
        assert!(text.contains("同步空间：01234567…"), "{text}");
        assert!(
            !text.contains("0123456789abcdef0123"),
            "空间标识不整段显示: {text}"
        );
        assert!(text.contains("范围：memory、persona"), "{text}");
        assert!(text.contains("待发出 2 条，待合入 1 条"), "{text}");
        assert!(
            text.contains("其他设备（1）") && text.contains("手机（android，很久没出现"),
            "{text}"
        );
        assert!(text.contains(&format!("/sync show {code}")), "{text}");
        assert!(
            text.contains(&format!("接受：/sync accept {code}")),
            "{text}"
        );
        assert!(
            text.contains(&format!("拒绝：/sync reject {code}")),
            "{text}"
        );
        // 不用处理的事项只有「查看」。
        let halted_code = item_code(&halted);
        assert!(
            text.contains(&format!("/sync show {halted_code}")),
            "{text}"
        );
        assert!(!text.contains(&format!("accept {halted_code}")), "{text}");
    }

    #[test]
    fn items_are_found_by_code_and_only_unversioned_ones_by_id() {
        let items = vec![
            item("persona-0011223344556677", "persona", &["accept", "reject"]),
            item(
                "quarantine-00112233445566778899aabb",
                "quarantine",
                &["retry", "discard"],
            ),
        ];
        let code = item_code(&items[1]);
        assert_eq!(find_item(&items, &code).unwrap().id, items[1].id);
        assert_eq!(
            find_item(&items, &format!("[{}]", code.to_uppercase()))
                .unwrap()
                .id,
            items[1].id
        );
        assert_eq!(find_item(&items, &items[0].id).unwrap().id, items[0].id);
        assert!(find_item(&items, "ffffff").is_err());
        assert!(find_item(&items, "").is_err());
        // 编号太短不算数（空串是任何编号的前缀）。
        assert!(find_item(&items, &code[..3]).is_err());

        // 等确认的改动带着版本：只认编号（可以多敲几位），按标识找等于绕过「看过的是哪
        // 一版」；换了一版，旧编号就失效。
        let mut versioned = item("skill-0011223344556677", "skill", &["accept", "reject"]);
        versioned.payload = serde_json::json!({ "hlc": "1763438400000-0001-00000000000000aa" });
        let old_code = item_code(&versioned);
        let items = vec![versioned.clone()];
        assert_eq!(find_item(&items, &old_code).unwrap().id, versioned.id);
        assert_eq!(
            find_item(&items, &item_code_full(&versioned)).unwrap().id,
            versioned.id
        );
        assert!(find_item(&items, &versioned.id).is_err());
        versioned.payload = serde_json::json!({ "hlc": "1763438400999-0001-00000000000000aa" });
        assert!(find_item(&[versioned], &old_code).is_err());
    }

    #[test]
    fn detail_text_shows_both_sides_and_flags_executables() {
        let persona = item("persona-0011223344556677", "persona", &["accept", "reject"]);
        let text = render_detail(
            &persona,
            &serde_json::json!({ "path": "config/SOUL.md", "local": "旧人格", "remote": "新人格" }),
        );
        assert!(text.contains("文件：config/SOUL.md"), "{text}");
        let (local, remote) = (text.find("旧人格").unwrap(), text.find("新人格").unwrap());
        assert!(local < remote, "{text}");
        assert!(text.contains("/sync accept"), "{text}");
        // 对方删除、本机没有：各有说明，不留空白。
        let gone = render_detail(
            &persona,
            &serde_json::json!({ "path": "p", "local": null, "remote": null }),
        );
        assert!(
            gone.contains("（本机没有）") && gone.contains("（对方把它删除了）"),
            "{gone}"
        );
        // 长内容截断。
        let long = "字".repeat(SHOW_MAX_CHARS + 10);
        let clipped = render_detail(
            &persona,
            &serde_json::json!({ "local": long, "remote": "x" }),
        );
        assert!(
            clipped.contains(&format!("共 {} 字", SHOW_MAX_CHARS + 10)),
            "截断要说明"
        );

        let skill = item("skill-0011223344556677", "skill", &["accept", "reject"]);
        let text = render_detail(
            &skill,
            &serde_json::json!({
                "name": "demo", "op": "put", "unchanged": 2,
                "files": [
                    { "path": "run.py", "status": "added", "exec": true, "size": 12 },
                    { "path": "SKILL.md", "status": "modified", "exec": false, "size": 30 },
                ],
            }),
        );
        assert!(
            text.contains("新增　run.py（12 字节）　⚠ 可执行内容"),
            "{text}"
        );
        assert!(text.contains("修改　SKILL.md（30 字节）"), "{text}");
        assert!(text.contains("另有 2 个文件没有变化"), "{text}");
    }

    #[test]
    fn saving_writes_only_changed_keys_and_keeps_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclaw.toml");
        std::fs::write(
            &path,
            "# 顶部注释\n[agent]\nbase_dir = \"agent\" # 数据根\n\n[sync]\nenabled = false # 开关\ndevice_name = \"旧名\"\n",
        )
        .unwrap();
        let before = Config::load_from_dir(dir.path()).unwrap().sync;
        assert_eq!(before.device_name, "旧名");

        let mut after = before.clone();
        after.enabled = true;
        after.scopes = vec!["memory".into(), "persona".into(), "skills".into()];
        after.poll_interval_sec = 30;
        after.webdav.url = "https://dav.example.com/rclaw/".into();
        after.webdav.username = "me".into();
        after.s3.oss_forbid_overwrite = true;
        save_sync_config(&path, &before, &after).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("# 顶部注释") && text.contains("# 数据根"),
            "{text}"
        );
        assert!(text.contains("enabled = true # 开关"), "{text}");
        assert!(
            text.contains("device_name = \"旧名\""),
            "没变的键不动: {text}"
        );
        assert!(text.contains("[sync.webdav]"), "{text}");
        assert!(
            text.contains("oss_forbid_overwrite = true"),
            "OSS 开关要写回: {text}"
        );
        assert!(!text.contains("trash_days"), "没变的默认值不写进去: {text}");
        assert_eq!(Config::load_from_dir(dir.path()).unwrap().sync, after);

        // 没有变化：不重写文件。
        let stamp = std::fs::metadata(&path).unwrap().modified().unwrap();
        save_sync_config(&path, &after, &after).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);

        // 不合规的设置不落盘。
        let mut bad = after.clone();
        bad.transport = "ftp".into();
        assert!(save_sync_config(&path, &after, &bad).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn saving_creates_the_config_file_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rclaw.toml");
        let before = SyncConfig::default();
        let mut after = before.clone();
        after.enabled = true;
        after.transport = "file".into();
        after.file.dir = "/srv/sync".into();
        save_sync_config(&path, &before, &after).unwrap();
        assert_eq!(Config::load_from_dir(dir.path()).unwrap().sync, after);
    }
}
