//! 多端同步的命令：「多端同步」面板、会话包、配对码与扫码、附件占位、别的设备上的定时
//! 任务（`docs/multi-device-sync.md` §6、§7.9、§8.6–§8.8、§10–§11）。
//!
//! 引擎的操作分三种，这里按它们各自的要求调用：
//! - 经引擎线程的（创建 / 加入空间、立即同步……）是 async，直接 await；
//! - 直接读写本机数据库和文件的（状态、角标、回收站、会话包……）是阻塞的，放到阻塞线程池；
//! - 通知（窗口获得焦点、打开会话）不阻塞。
//!
//! 配对码等同于全部同步数据的钥匙：只在面板上显示、只从面板上输入。这里不把它写进日志，
//! 报错里也不带它。拍下来的二维码照片同样是钥匙：照片的路径不交给界面，由这里读进内存后
//! 立刻删掉，识别成不成都一样。

use std::io::Read;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "android", test))]
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use rclaw_sync::engine::{
    AttachmentInfo, Backlog, MemoryRestored, MemoryTrashItem, RemoteJob, SelfTestStep, SessionMark,
    TrashItem, Trigger,
};
use rclaw_sync::{ImportOptions, PairingCode, SyncConfig, SyncEngine, SyncStatus};
use serde::Serialize;
use tauri::State;

use super::{run_blocking, MediaPayload};
use crate::agent::{is_valid_chat_id, CHANNEL_DESKTOP};
use crate::home;
use crate::state::AppState;

fn handle(state: &AppState) -> Result<SyncEngine, String> {
    state.sync().ok_or_else(|| "agent 尚未启动".to_string())
}

fn fail(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn check_chat_id(chat_id: &str) -> Result<(), String> {
    if is_valid_chat_id(chat_id) {
        Ok(())
    } else {
        Err(format!("非法会话 id: {chat_id}"))
    }
}

// ---------------------------------------------------------------------------
// 设置：`[sync]` 段在 rclaw.toml 里，密码与空间密钥在 sync/secrets.json 里
// ---------------------------------------------------------------------------

/// `rclaw.toml` 里现在的 `[sync]`。
fn disk_config(home: &Path) -> Result<SyncConfig, String> {
    rclaw_cli::config::Config::load_from_dir(home)
        .map(|config| config.sync)
        .map_err(|e| format!("读取 rclaw.toml 失败: {e:#}"))
}

/// 把 `[sync]` 写回 `rclaw.toml`：只动变了的键，其余内容与注释保真。
fn persist_from(home: &Path, before: &SyncConfig, after: &SyncConfig) -> Result<(), String> {
    rclaw_cli::sync::save_sync_config(&home::config_path(home), before, after)
        .map_err(|e| format!("保存同步设置失败: {e:#}"))
}

/// 以磁盘上的最新一份为底写回 [`persist`]。
fn persist(home: &Path, after: &SyncConfig) -> Result<(), String> {
    let before = disk_config(home)?;
    persist_from(home, &before, after)
}

/// 先落盘再交给引擎：文件没写成的设置不生效。
///
/// 存储设置只由保存存储的那条命令改（它在同一把锁里写 TOML）：这份表单草稿里的存储段
/// 可能是旧的，落盘时以磁盘上的当前值为准，别把别处刚保存的存储设置回退。
async fn save_and_apply(
    state: &AppState,
    engine: &SyncEngine,
    mut config: SyncConfig,
) -> Result<(), String> {
    let home = state.home.clone();
    let paths = rclaw_sync::SyncPaths::new(&home);
    let lock_paths = paths.clone();
    let _lock = run_blocking("保存同步设置", move || {
        rclaw_sync::StorageLock::acquire(&lock_paths)
            .map_err(|error| format!("保存同步设置失败: {error}"))
    })
    .await?;
    let disk_home = home.clone();
    let disk = run_blocking("读取同步设置", move || disk_config(&disk_home)).await?;
    config.transport = disk.transport;
    config.webdav = disk.webdav;
    config.file = disk.file;
    config.s3 = disk.s3;
    let saved = config.clone();
    let write_home = home.clone();
    run_blocking("保存同步设置", move || persist(&write_home, &saved)).await?;
    engine.apply_config(config);
    crate::sync_host::emit_status();
    Ok(())
}

/// 同 [`save_and_apply`]，但配置与刚填的密码在同一条命令里交给引擎（[`SyncEngine::
/// save_storage_with`]）：密码按这条命令里的配置绑定存储账户，两步之间插进来的别的配置
/// 变更不会把它绑到另一处存储上。创建 / 加入 / 修改存储设置都走这一条。
///
/// 意图、`rclaw.toml`、secrets 三者的提交由引擎在一把跨进程锁里串行完成：这里只把
/// 「写回文件」这一步作为回调交进去。中途失败留下意图，重启时宿主与引擎收敛到同一套。
async fn save_with_secret(
    state: &AppState,
    engine: &SyncEngine,
    config: SyncConfig,
    secret: Option<String>,
    // 创建/加入向导会收集设备名：`Some(向导打开时的设备名)` 表示「和它不同就采用表单
    // 里的新名字，相同就保留锁内读到的（别处可能刚改过）」；普通存储表单传 `None`，
    // 设备名以磁盘为准。
    device_name_baseline: Option<String>,
) -> Result<(), String> {
    let home = state.home.clone();
    let merge_home = home.clone();
    let write_home = home.clone();
    engine
        .save_storage_with(
            config,
            secret.filter(|secret| !secret.is_empty()),
            move |proposed| {
                // 锁内以磁盘最新一份为底，只采用这次表单的存储段与开关：表单里可能带
                // 着过期的范围/通道（填表之后别处刚改过），不能整份写回。
                let before = disk_config(&merge_home)?;
                let mut merged = before.clone();
                merged.apply_storage_from(proposed);
                merged.enabled = proposed.enabled;
                if device_name_baseline
                    .as_deref()
                    .is_some_and(|baseline| proposed.device_name != baseline)
                {
                    merged.device_name = proposed.device_name.clone();
                }
                Ok(merged)
            },
            move |merged| persist(&write_home, merged),
        )
        .await
        .map_err(fail)?;
    crate::sync_host::emit_status();
    Ok(())
}

/// `rclaw.toml` 被别的入口改写之后（配置面板保存了 `sync.*`、导入了整份配置）：把文件里
/// 的 `[sync]` 重新交给引擎。读不出来只记日志——文件本身的毛病由那个入口自己报。
pub(super) async fn reload_config(state: &AppState) {
    let Some(engine) = state.sync() else {
        return;
    };
    let home = state.home.clone();
    match run_blocking("读取同步设置", move || disk_config(&home)).await {
        Ok(config) => {
            engine.apply_config(config);
            crate::sync_host::emit_status();
        }
        Err(error) => tracing::warn!("重新读取 [sync] 设置失败: {error}"),
    }
}

/// 面板的全部状态：引擎的状态 + 当前生效的设置（表单的初值）。
#[derive(Debug, Serialize)]
pub struct SyncStateDto {
    pub status: SyncStatus,
    pub config: SyncConfig,
}

#[tauri::command]
pub async fn sync_status(state: State<'_, AppState>) -> Result<SyncStateDto, String> {
    let engine = handle(&state)?;
    run_blocking("读取同步状态", move || {
        Ok(SyncStateDto {
            status: engine.status(),
            config: engine.config(),
        })
    })
    .await
}

/// 保存同步设置（开关、设备名、范围、存储地址……）并立即生效。
#[tauri::command]
pub async fn sync_save_config(
    state: State<'_, AppState>,
    config: SyncConfig,
) -> Result<(), String> {
    let engine = handle(&state)?;
    save_and_apply(&state, &engine, config).await
}

/// 只换存储的密码（WebDAV 的密码 / 对象存储的 Secret Key）。
#[tauri::command]
pub async fn sync_set_secret(state: State<'_, AppState>, secret: String) -> Result<(), String> {
    let engine = handle(&state)?;
    if secret.is_empty() {
        return Err("密码不能为空".into());
    }
    engine.set_storage_secret(&secret).await.map_err(fail)
}

/// 保存存储的设置（地址、账号、目录……）与刚填的密码，一起生效。
///
/// 面板上「先保存配置、再单独保存密码」两条命令之间，别的入口（配置面板保存 `sync.*`、
/// 导入整份配置）可能把配置换成另一份；worker 用收到命令时的配置决定密码绑定的账户，
/// 密码就会绑到后一份配置上，并可能在重连时发到那个地址去。这里把两者放进同一条命令，
/// 由引擎线程一次办完。
#[tauri::command]
pub async fn sync_save_storage(
    state: State<'_, AppState>,
    config: SyncConfig,
    secret: Option<String>,
) -> Result<(), String> {
    let engine = handle(&state)?;
    // 只带走存储这几段：别处（范围、设备名）在这期间改过的不被这份草稿盖回去。
    let current = engine.config();
    let mut next = current.clone();
    next.transport = config.transport;
    next.webdav = config.webdav;
    next.file = config.file;
    next.s3 = config.s3;
    next.validate().map_err(fail)?;
    save_with_secret(&state, &engine, next, secret, None).await
}

/// 「测试连接」：用表单上正在填的设置（不必先保存）把存储要用到的操作逐个走一遍。
/// `secret` 不给就用本机存着的。
#[tauri::command]
pub async fn sync_test_connection(
    state: State<'_, AppState>,
    config: SyncConfig,
    secret: Option<String>,
) -> Result<Vec<SelfTestStep>, String> {
    let engine = handle(&state)?;
    config.validate().map_err(fail)?;
    let secret = secret.filter(|secret| !secret.is_empty());
    Ok(engine.test_connection(&config, secret.as_deref()).await)
}

/// 取一个 HTTPS 地址出示的证书指纹（自签证书的「信任这张证书」）。
#[tauri::command]
pub async fn sync_probe_certificate(url: String) -> Result<String, String> {
    SyncEngine::probe_certificate(url.trim())
        .await
        .map_err(fail)
}

// ---------------------------------------------------------------------------
// 同步空间：创建、加入、退出、重置；配对码
// ---------------------------------------------------------------------------

/// 创建同步空间：保存表单上的设置（顺带开启同步）与密码，然后在存储上建出空间。设置与
/// 密码在同一条命令里交给引擎（见 [`save_with_secret`]）。
#[tauri::command]
pub async fn sync_create_space(
    state: State<'_, AppState>,
    mut config: SyncConfig,
    secret: Option<String>,
) -> Result<(), String> {
    let engine = handle(&state)?;
    config.enabled = true;
    let baseline = engine.config().device_name;
    save_with_secret(&state, &engine, config.clone(), secret, Some(baseline)).await?;
    engine.create_space().await.map_err(fail)
}

/// 配对码里有什么（加入之前给用户核对）。不含密钥与密码。
#[derive(Debug, Serialize)]
pub struct PairingInfoDto {
    /// 配对码自带存储的地址与凭据：加入时不用再填。
    pub has_storage: bool,
    /// 自带的存储是哪一种（`webdav` / `s3`）。
    pub transport: Option<String>,
    /// 自带的存储地址。数据会传到这里，要让用户看一眼是不是自己的。
    pub endpoint: Option<String>,
    /// 配对码没带存储设置、本机配置的存储又需要密码而没存着：加入之前要先问用户。
    pub needs_secret: bool,
}

/// 检查一段配对码，并说出它带了什么。`config` 是表单上正在填的设置（不给就用当前生效的）。
#[tauri::command]
pub async fn sync_pairing_info(
    state: State<'_, AppState>,
    code: String,
    config: Option<SyncConfig>,
) -> Result<PairingInfoDto, String> {
    let engine = handle(&state)?;
    let config = config.unwrap_or_else(|| engine.config());
    let home = state.home.clone();
    run_blocking("检查配对码", move || {
        let parsed = PairingCode::parse(&code).map_err(fail)?;
        let needs_secret =
            rclaw_sync::engine::join_needs_secret(&home, &config, &code).map_err(fail)?;
        let (transport, endpoint) = match &parsed.storage {
            Some(rclaw_sync::pairing::PairedStorage::Webdav { url, .. }) => {
                (Some("webdav".to_string()), Some(url.clone()))
            }
            Some(rclaw_sync::pairing::PairedStorage::S3 {
                endpoint, bucket, ..
            }) => (
                Some("s3".to_string()),
                Some(format!("{endpoint}（存储桶 {bucket}）")),
            ),
            None => (None, None),
        };
        Ok(PairingInfoDto {
            has_storage: parsed.storage.is_some(),
            transport,
            endpoint,
            needs_secret,
        })
    })
    .await
}

/// 用配对码加入同步空间。`config` 是表单上正在填的设置（配对码没带存储设置时用它）；
/// `secret` 是刚填的存储密码。配对码带了存储设置的话以它为准，加入之后写回 `rclaw.toml`。
#[tauri::command]
pub async fn sync_join_space(
    state: State<'_, AppState>,
    code: String,
    config: Option<SyncConfig>,
    secret: Option<String>,
) -> Result<(), String> {
    let engine = handle(&state)?;
    let baseline = engine.config().device_name;
    let mut base = config.unwrap_or_else(|| engine.config());
    base.enabled = true;
    // 配对码可能带着自己的存储设置：加入之前先把最终目标写回文件。只在加入之后才写的话，
    // 「密钥已经存好、设置还没写回」的窗口里重启会按旧设置连到另一处存储；写入失败也
    // 不会动远端的空间和本机密钥。重复加入同一空间是幂等的（见 `SyncEngine::join_space`）。
    let preview = rclaw_sync::pairing_config(&base, &code).map_err(fail)?;
    save_with_secret(&state, &engine, base, secret, Some(baseline)).await?;
    if preview != engine.config() {
        let home = state.home.clone();
        let target = preview.clone();
        let lock_home = home.clone();
        // 预写也走协议锁 + 磁盘最新一份：配对码带来的只有存储段，范围/通道/开关
        // 以磁盘当前值为准，别把填表之后别处改过的设置回退。
        run_blocking("保存同步设置", move || {
            let paths = rclaw_sync::SyncPaths::new(&lock_home);
            let _lock = rclaw_sync::StorageLock::acquire(&paths)
                .map_err(|error| format!("取同步存储锁失败: {error}"))?;
            let before = disk_config(&home)?;
            let mut merged = before.clone();
            merged.apply_storage_from(&target);
            persist_from(&home, &before, &merged)
        })
        .await
        .map_err(|error| format!("保存同步设置失败（还没加入同步空间）：{error}"))?;
    }
    let (joined, join_seq) = engine.join_space_with_seq(&code).await.map_err(|error| {
        format!("{error}（配对码带来的存储设置已经写回 rclaw.toml；修好后可以重试同一配对码）")
    })?;
    // 加入之后以引擎实际采用的设置为准再写一次。即使结果与预写相同也要核对磁盘：
    // 预写与加入之间可能完成过另一次存储保存（TOML 已是 C），而这次加入才是最后一次
    // 安装（引擎在 B）——不写就会 TOML 与引擎分叉。只有这次加入仍是引擎最近一次
    // 安装时才写：更新的提交负责写盘，迟到的回写不能覆盖它。
    {
        let home = state.home.clone();
        let target = joined.clone();
        let seq_check = engine.clone();
        let lock_home = home.clone();
        let written = run_blocking("保存同步设置", move || {
            let paths = rclaw_sync::SyncPaths::new(&lock_home);
            let _lock = rclaw_sync::StorageLock::acquire(&paths)
                .map_err(|error| format!("取同步存储锁失败: {error}"))?;
            // 序号复核必须在协议锁内：保存存储在这把锁内一直持有到 worker 安装与
            // 宿主发布结束，锁内看到的序号就是最终顺序；锁外检查后再取锁，期间
            // 完成的保存会被这次旧回写盖掉。
            if seq_check.config_seq() != join_seq {
                tracing::info!("加入空间的回写迟到：引擎已安装更新的配置，跳过写盘");
                return Ok(());
            }
            let before = disk_config(&home)?;
            let mut merged = before.clone();
            merged.apply_storage_from(&target);
            persist_from(&home, &before, &merged)
        })
        .await;
        crate::sync_host::emit_status();
        written.map_err(|error| {
            format!(
                "已经加入同步空间，但设置没能写回 rclaw.toml（{error}）。\
                 请在同步设置里再保存一次，否则下次启动后连不上"
            )
        })
    }
}

/// 退出同步空间：本机的密钥与存储密码清掉，会话、记忆等数据原样留着。
#[tauri::command]
pub async fn sync_leave_space(state: State<'_, AppState>) -> Result<(), String> {
    handle(&state)?.leave_space().await.map_err(fail)
}

/// 重置同步空间（设备丢失之后用）：换新空间、新密钥，其余设备要拿新的配对码重新加入。
#[tauri::command]
pub async fn sync_reset_space(state: State<'_, AppState>) -> Result<(), String> {
    handle(&state)?.reset_space().await.map_err(fail)
}

/// 「添加设备」要显示的配对码。不派生 `Debug`：它不该出现在任何日志里。
#[derive(Serialize)]
pub struct PairingCodeDto {
    pub code: String,
    /// 二维码（PNG 的 data URL）。配对码太长画不出来时为空，原因在 `qr_error` 里。
    pub qr: Option<String>,
    pub qr_error: Option<String>,
}

/// 本机所在空间的配对码与它的二维码。`include_storage`：带上存储的地址与凭据，新设备
/// 扫一下就能用。
#[tauri::command]
pub async fn sync_pairing_code(
    state: State<'_, AppState>,
    include_storage: bool,
) -> Result<PairingCodeDto, String> {
    let engine = handle(&state)?;
    let code = engine.pairing_code(include_storage).await.map_err(fail)?;
    run_blocking("生成二维码", move || {
        let (qr, qr_error) = match qr_png(&code) {
            Ok(png) => (
                Some(format!(
                    "data:image/png;base64,{}",
                    super::encode_base64(&png)
                )),
                None,
            ),
            Err(error) => (None, Some(error)),
        };
        Ok(PairingCodeDto { code, qr, qr_error })
    })
    .await
}

// ---------------------------------------------------------------------------
// 日常操作：立即同步、暂停、待处理事项、设备
// ---------------------------------------------------------------------------

/// 「立即同步」：完整跑一轮，返回这一轮成没成。
#[tauri::command]
pub async fn sync_now(state: State<'_, AppState>) -> Result<(), String> {
    handle(&state)?.sync_now().await.map_err(fail)
}

#[tauri::command]
pub async fn sync_set_paused(state: State<'_, AppState>, paused: bool) -> Result<(), String> {
    handle(&state)?.set_paused(paused).await.map_err(fail)
}

/// 一条待处理事项的详情（等确认的改动的新旧内容，界面做对比）。
#[tauri::command]
pub async fn sync_pending_detail(
    state: State<'_, AppState>,
    id: String,
) -> Result<serde_json::Value, String> {
    let engine = handle(&state)?;
    run_blocking("读取待处理事项", move || {
        engine.pending_detail(&id).map_err(fail)
    })
    .await
}

/// 处理一条待处理事项：`accept` / `reject` / `retry` / `discard` / `dismiss`。
///
/// `reviewed` 是界面上显示的那一版（事项 `payload.hlc`，详情里的 `hlc`）。等确认的改动
/// 必须带着它：这期间来了更新的一版的话引擎不办，让用户看过新内容再决定。
#[tauri::command]
pub async fn sync_resolve(
    state: State<'_, AppState>,
    id: String,
    action: String,
    reviewed: Option<String>,
) -> Result<(), String> {
    handle(&state)?
        .resolve_pending(&id, &action, reviewed.as_deref())
        .await
        .map_err(fail)
}

/// 把一台设备标成「已丢失」（它的定时任务可以被强制接管），或取消。
#[tauri::command]
pub async fn sync_mark_lost(
    state: State<'_, AppState>,
    device: String,
    lost: bool,
) -> Result<(), String> {
    let engine = handle(&state)?;
    run_blocking("标记设备", move || {
        engine.mark_lost(&device, lost).map_err(fail)
    })
    .await
}

/// 同意在按流量计费的网络下做首次同步。
#[tauri::command]
pub async fn sync_confirm_metered(state: State<'_, AppState>) -> Result<(), String> {
    handle(&state)?.confirm_metered().await.map_err(fail)
}

/// 开启附件同步之后，已有的附件传哪些：`"all"`、`{"days": N}`、`"none"`。
#[tauri::command]
pub async fn sync_set_backlog(state: State<'_, AppState>, choice: Backlog) -> Result<(), String> {
    handle(&state)?.set_backlog(choice).await.map_err(fail)
}

/// 界面告诉引擎发生了什么：`focus`（窗口获得焦点）、`open_session`（打开了一个会话）。
/// 引擎自己决定要不要拉取。不阻塞；agent 没启动、同步没开时什么都不做。
#[tauri::command]
pub async fn sync_notify(
    state: State<'_, AppState>,
    event: String,
    chat_id: Option<String>,
) -> Result<(), String> {
    let Some(engine) = state.sync() else {
        return Ok(());
    };
    match event.as_str() {
        "focus" => engine.notify(Trigger::Focus),
        "open_session" => {
            if let Some(chat_id) = chat_id.filter(|chat_id| is_valid_chat_id(chat_id)) {
                engine.notify(Trigger::OpenSession {
                    channel: CHANNEL_DESKTOP.to_string(),
                    chat_id,
                });
            }
        }
        other => return Err(format!("不认识的同步通知: {other}")),
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 会话：角标、仅本机、回收站
// ---------------------------------------------------------------------------

/// 会话列表的角标（仅本机、已同步、自动分出的分支、换了键落地、正在别的设备上运行）。
/// 只列桌面通道里至少有一项不是默认值的会话。
#[tauri::command]
pub async fn sync_session_marks(state: State<'_, AppState>) -> Result<Vec<SessionMark>, String> {
    let engine = handle(&state)?;
    run_blocking("读取会话的同步情况", move || {
        let marks = engine.session_marks().map_err(fail)?;
        Ok(marks
            .into_iter()
            .filter(|mark| mark.channel == CHANNEL_DESKTOP)
            .collect())
    })
    .await
}

/// 标记 / 取消「仅本机」：标记之后这个会话不再同步，其他设备上已有的那一份会被彻底
/// 删除（不进它们的回收站，§7.9）。会话还没有落库时也可以标（新建会话时就能选）。
#[tauri::command]
pub async fn sync_set_local_only(
    state: State<'_, AppState>,
    chat_id: String,
    on: bool,
) -> Result<(), String> {
    check_chat_id(&chat_id)?;
    let engine = handle(&state)?;
    run_blocking("设置仅本机", move || {
        engine
            .set_local_only(CHANNEL_DESKTOP, &chat_id, on)
            .map_err(fail)
    })
    .await
}

/// 回收站里的东西：会话，和因为同步而从本机的记忆文件里删掉的条目。
#[derive(Debug, Serialize)]
pub struct TrashDto {
    pub sessions: Vec<TrashItem>,
    pub memory: Vec<MemoryTrashItem>,
}

/// 回收站不看同步开没开、在不在同步空间里：退出同步空间之后照样读得出来。
#[tauri::command]
pub async fn sync_trash_list(state: State<'_, AppState>) -> Result<TrashDto, String> {
    let engine = handle(&state)?;
    run_blocking("读取回收站", move || {
        // 先列会话：它顺带把过了保留期的条目（两种都算）清掉。
        let sessions = engine.trash_list();
        Ok(TrashDto {
            sessions,
            memory: engine.trash_memory_list(),
        })
    })
    .await
}

/// 导入 / 恢复出来的会话。
#[derive(Debug, Serialize)]
pub struct ImportedDto {
    pub channel: String,
    pub chat_id: String,
    pub messages: usize,
}

impl From<rclaw_sync::Imported> for ImportedDto {
    fn from(imported: rclaw_sync::Imported) -> Self {
        ImportedDto {
            channel: imported.channel,
            chat_id: imported.chat_id,
            messages: imported.messages,
        }
    }
}

/// 从回收站恢复一个会话：作为新会话回到本机（同步开着的话随后发布给其他设备）。
#[tauri::command]
pub async fn sync_trash_restore(
    state: State<'_, AppState>,
    id: String,
) -> Result<ImportedDto, String> {
    let engine = handle(&state)?;
    run_blocking("恢复会话", move || {
        engine
            .trash_restore(&id)
            .map(ImportedDto::from)
            .map_err(|e| format!("恢复会话失败: {e}"))
    })
    .await
}

#[tauri::command]
pub async fn sync_trash_remove(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let engine = handle(&state)?;
    run_blocking("清理回收站", move || {
        engine.trash_remove(&id).map_err(fail)
    })
    .await
}

/// 把回收站里的一份记忆条目放回它原来所在的记忆文件（同步开着的话随后发给其他设备）。
/// 有条目没通过本机的检查时不算失败：结果里说明是哪几条，副本留在回收站里。
#[tauri::command]
pub async fn sync_trash_memory_restore(
    state: State<'_, AppState>,
    id: String,
) -> Result<MemoryRestored, String> {
    let engine = handle(&state)?;
    run_blocking("放回记忆", move || {
        engine
            .trash_memory_restore(&id)
            .map_err(|e| format!("放回记忆失败: {e}"))
    })
    .await
}

#[tauri::command]
pub async fn sync_trash_memory_remove(
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let engine = handle(&state)?;
    run_blocking("清理回收站", move || {
        engine.trash_memory_remove(&id).map_err(fail)
    })
    .await
}

// ---------------------------------------------------------------------------
// 别的设备上的定时任务（只读镜像与接管）
// ---------------------------------------------------------------------------

/// 别的设备上的一个定时任务（列表展示用，与本机任务的 `CronEntryDto` 同一套字段）。
#[derive(Debug, Serialize)]
pub struct RemoteCronDto {
    /// 归属设备的标识与展示名。
    pub owner: String,
    pub owner_name: String,
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// 人读的调度描述；本机版本不认识这种调度方式时为空。
    pub schedule: String,
    /// 触发时注入的消息正文。
    pub message: String,
    /// 这份定义的版本。接管时原样交回（[`sync_cron_takeover`] 的 `reviewed`）：用户看到的
    /// 和要接管的得是同一版。
    pub version: String,
    /// 不能移到本机执行的原因；空 = 可以。
    pub blocked: Option<String>,
    /// 本机已经请求接手，在等那台设备确认。
    pub pending: bool,
    /// 那台设备很久没出现、或被标为已丢失：可以不等它确认，强制接管。
    pub can_force: bool,
}

impl From<RemoteJob> for RemoteCronDto {
    fn from(remote: RemoteJob) -> Self {
        let text = |key: &str| {
            remote
                .job
                .get(key)
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string()
        };
        // 别的设备可能比本机新，用了本机还不认识的调度方式：名字和正文照样显示。
        let (name, enabled, schedule, message) =
            match serde_json::from_value::<rclaw_store::CronJob>(remote.job.clone()) {
                Ok(job) => {
                    let (_, schedule) = super::describe_cron(&job);
                    (job.name, job.enabled, schedule, job.message)
                }
                Err(_) => (
                    text("name"),
                    remote
                        .job
                        .get("enabled")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(true),
                    String::new(),
                    text("message"),
                ),
            };
        RemoteCronDto {
            owner: remote.owner,
            owner_name: remote.owner_name,
            id: remote.id,
            name,
            enabled,
            schedule,
            message,
            version: remote.version,
            blocked: remote.blocked,
            pending: remote.pending,
            can_force: remote.can_force,
        }
    }
}

#[tauri::command]
pub async fn sync_remote_cron(state: State<'_, AppState>) -> Result<Vec<RemoteCronDto>, String> {
    let engine = handle(&state)?;
    run_blocking("读取其他设备的定时任务", move || {
        let jobs = engine.remote_cron().map_err(fail)?;
        Ok(jobs.into_iter().map(RemoteCronDto::from).collect())
    })
    .await
}

/// 把别的设备上的一个定时任务接管到本机。`force`：那台设备很久没出现、或已经标成
/// 「已丢失」，不等它确认。`reviewed` 是列表里显示的那份定义的版本
/// （[`RemoteCronDto::version`]）：那台设备在这之后改过任务的话不办，让用户先看新的。
#[tauri::command]
pub async fn sync_cron_takeover(
    state: State<'_, AppState>,
    owner: String,
    id: String,
    force: bool,
    reviewed: String,
) -> Result<(), String> {
    handle(&state)?
        .cron_takeover(&owner, &id, force, &reviewed)
        .await
        .map_err(fail)
}

// ---------------------------------------------------------------------------
// 附件：占位、下载、文件快照
// ---------------------------------------------------------------------------

/// 一个会话里各条消息引用的附件在本机的情况。界面先看这份清单，清单里没有的再按工作区
/// 路径找。
#[tauri::command]
pub async fn sync_attachments(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<Vec<AttachmentInfo>, String> {
    check_chat_id(&chat_id)?;
    let engine = handle(&state)?;
    run_blocking("读取附件清单", move || {
        engine.attachments(CHANNEL_DESKTOP, &chat_id).map_err(fail)
    })
    .await
}

/// 用户点了一个还没下载的附件：排到下载队列最前面。结果经 `sync` 事件
/// （`kind = "attachment"`）通知。
#[tauri::command]
pub async fn sync_fetch_attachment(state: State<'_, AppState>, blob: String) -> Result<(), String> {
    handle(&state)?.fetch_attachment(&blob).map_err(fail)
}

/// 在会话的附件清单里找到 `blob` 对应的那份文件快照，返回清单项与它在同步缓存里的真实
/// 路径。路径从引擎的登记里来，不信界面传的；再核对一遍它确实落在同步缓存目录里。
fn cached_snapshot(
    engine: &SyncEngine,
    home: &Path,
    chat_id: &str,
    blob: &str,
) -> Result<(AttachmentInfo, PathBuf), String> {
    let info = engine
        .attachments(CHANNEL_DESKTOP, chat_id)
        .map_err(fail)?
        .into_iter()
        // 文件与工具结果全文（artifact）都可能有缓存副本：后者落到缓存时，工作区里的
        // 原路径句柄指向的可能是另一个同名文件，用户从会话顶部的附件区打开的就是这份。
        // 会话、对象、允许的类型一起选：拿第一条缓存记录去核对另一个 blob 的摘要，
        // 预览和保存就会取错附件。
        .find(|item| {
            item.blob == blob
                && (item.kind == "file" || item.kind == "artifact")
                && item.state == "cached"
        })
        .ok_or_else(|| {
            "这个附件不在本机的同步缓存里（可能还没有下载，或者已经被清理）".to_string()
        })?;
    // 缓存的副本要还是登记的那个对象（摘要对得上）才给用：被原地改写的文件不当作
    // 它——用户重新下载会走缺附件的路径，由持有正确内容的设备重新传。
    if !engine.cached_attachment_intact(&info.blob) {
        return Err(
            "这份缓存副本的内容已经和登记的不一致（可能被本机的程序改写过）。\
             请重新下载这个附件。"
                .to_string(),
        );
    }
    let local = info
        .local_path
        .as_deref()
        .ok_or_else(|| "这个附件在本机没有文件".to_string())?;
    let cache = home
        .join("sync")
        .join("files")
        .canonicalize()
        .map_err(|e| format!("同步缓存目录不可用: {e}"))?;
    let full = Path::new(local)
        .canonicalize()
        .map_err(|e| format!("附件文件不存在: {e}"))?;
    if !full.starts_with(&cache) {
        return Err("附件不在同步缓存目录里".into());
    }
    Ok((info, full))
}

/// 同步缓存里一张图片快照的预览（与工作区图片的预览同一条管线）。
#[tauri::command]
pub async fn sync_attachment_preview(
    state: State<'_, AppState>,
    chat_id: String,
    blob: String,
) -> Result<MediaPayload, String> {
    check_chat_id(&chat_id)?;
    let engine = handle(&state)?;
    let home = state.home.clone();
    run_blocking("生成预览", move || {
        let (info, full) = cached_snapshot(&engine, &home, &chat_id, &blob)?;
        let extension = info
            .path
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !super::MEDIA_EXTENSIONS.contains(&extension.as_str()) {
            return Err(format!("不支持的媒体类型: {extension}"));
        }
        super::image_preview_payload(&home, &full, &format!("sync-blob:{blob}"), &extension)
    })
    .await
}

/// 把一份文件快照存进工作区（§8.7）。返回它在工作区里的相对路径。
#[tauri::command]
pub async fn sync_attachment_save(
    state: State<'_, AppState>,
    chat_id: String,
    blob: String,
) -> Result<String, String> {
    check_chat_id(&chat_id)?;
    let engine = handle(&state)?;
    let home = state.home.clone();
    run_blocking("存进工作区", move || {
        let (info, full) = cached_snapshot(&engine, &home, &chat_id, &blob)?;
        // 工具结果（artifact）在消息里只有一个文件名，没有 uploads/ 这类前缀：导出到
        // 专用的 output/ 路径（工作区只认 uploads/、media/、output/ 之下的落点）。
        let target = if info.kind == "artifact" {
            let name = info.path.rsplit('/').next().unwrap_or(&info.path);
            format!("output/{name}")
        } else {
            info.path.clone()
        };
        save_snapshot(&home, &target, &full)
    })
    .await
}

/// 附件在消息里的相对路径 → 本机上能用的工作区相对路径。只认 `uploads/`、`media/`、
/// `output/` 之下的（消息能引用的就是这三处）；别的系统上合法、本机上用不了的字符
/// （Windows 不许 `:`、`?` 等）换成下划线，各平台用同一套，存出来的名字到哪都能用。
fn workspace_rel_for(path: &str) -> Result<String, String> {
    let bad = || "这个附件的路径不合规，不能存进工作区".to_string();
    let mut segments = Vec::new();
    for segment in path.split('/') {
        let cleaned: String = segment
            .chars()
            .map(|c| {
                if c.is_control() || matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*' | '\\') {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let cleaned = cleaned.trim_end_matches(['.', ' ']);
        if cleaned.is_empty() {
            return Err(bad());
        }
        segments.push(cleaned.to_string());
    }
    if segments.len() < 2 || !matches!(segments[0].as_str(), "uploads" | "media" | "output") {
        return Err(bad());
    }
    let rel = segments.join("/");
    super::validate_workspace_rel_path(&rel).map_err(|_| bad())?;
    Ok(rel)
}

/// 读满缓冲区或读到文件尾，返回读到的字节数。
fn read_full(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// `target` 是一个普通文件（不是链接），内容与 `source` 完全相同。
fn same_content(target: &Path, source: &Path) -> bool {
    let (Ok(target_meta), Ok(source_meta)) =
        (std::fs::symlink_metadata(target), std::fs::metadata(source))
    else {
        return false;
    };
    if !target_meta.is_file() || target_meta.len() != source_meta.len() {
        return false;
    }
    let (Ok(mut a), Ok(mut b)) = (std::fs::File::open(target), std::fs::File::open(source)) else {
        return false;
    };
    let (mut left, mut right) = (vec![0u8; 64 * 1024], vec![0u8; 64 * 1024]);
    loop {
        let (Ok(n), Ok(m)) = (read_full(&mut a, &mut left), read_full(&mut b, &mut right)) else {
            return false;
        };
        if n != m || left[..n] != right[..m] {
            return false;
        }
        if n == 0 {
            return true;
        }
    }
}

/// [`sync_attachment_save`] 的核心：把 `cached` 的内容存到工作区里 `path` 的位置。
///
/// - 那个位置空着：就存在那里，消息里的引用从此直接指向它。
/// - 那里已经有一份一模一样的：什么都不做。
/// - 那里是别的内容：不覆盖，另存为「原名-快照」。
///
/// 路径上的目录逐级核对 / 新建，不跟随链接；文件用不跟随链接的「必须新建」方式写。
fn save_snapshot(home: &Path, path: &str, cached: &Path) -> Result<String, String> {
    let rel = workspace_rel_for(path)?;
    let (dir_rel, name) = rel
        .rsplit_once('/')
        .ok_or_else(|| "这个附件的路径不合规，不能存进工作区".to_string())?;
    let root = home
        .join("agent")
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    let mut dir = root.clone();
    for segment in dir_rel.split('/') {
        dir.push(segment);
        match std::fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => return Err(format!("工作区里的 {dir_rel} 不是普通目录，不能往里存")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
            }
            Err(e) => return Err(format!("创建目录失败: {e}")),
        }
    }
    let dir = dir
        .canonicalize()
        .map_err(|e| format!("创建目录失败: {e}"))?;
    if !dir.starts_with(&root) {
        return Err("附件的路径越出工作区".into());
    }
    if same_content(&dir.join(name), cached) {
        return Ok(rel);
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => (stem, Some(extension)),
        _ => (name, None),
    };
    let dest = super::write_fresh_file(
        &dir,
        "存进工作区失败",
        |n| match (n, extension) {
            (1, _) => name.to_string(),
            (2, Some(extension)) => format!("{stem}-快照.{extension}"),
            (2, None) => format!("{stem}-快照"),
            (n, Some(extension)) => format!("{stem}-快照{}.{extension}", n - 1),
            (n, None) => format!("{stem}-快照{}", n - 1),
        },
        |file| {
            let mut from = std::fs::File::open(cached)?;
            std::io::copy(&mut from, file).map(|_| ())
        },
    )?;
    let file_name = dest
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| "结果路径异常".to_string())?;
    Ok(format!("{dir_rel}/{file_name}"))
}

// ---------------------------------------------------------------------------
// 会话包：导出 / 导入（不需要开启同步）
// ---------------------------------------------------------------------------

/// 会话包文件的扩展名。
const BUNDLE_EXTENSION: &str = "rclaw-session.jsonl";

/// 把一个会话导出成会话包，落在 `output/exports/` 下，返回工作区相对路径（界面经
/// save_media / share_media 决定去向）。会话包是明文：里面是这个会话的全部内容。
#[tauri::command]
pub async fn session_export_bundle(
    state: State<'_, AppState>,
    chat_id: String,
    title: String,
) -> Result<String, String> {
    check_chat_id(&chat_id)?;
    let engine = handle(&state)?;
    let home = state.home.clone();
    run_blocking("导出会话包", move || {
        let text = engine
            .export_session(CHANNEL_DESKTOP, &chat_id)
            .map_err(|e| format!("导出会话包失败: {e}"))?;
        write_bundle_file(&home, &title, text.as_bytes())
    })
    .await
}

fn write_bundle_file(home: &Path, title: &str, bytes: &[u8]) -> Result<String, String> {
    if bytes.len() > super::UPLOAD_MAX_BYTES {
        return Err(format!(
            "会话包超过可导入的 {} MB 上限，无法导出",
            super::UPLOAD_MAX_BYTES / 1024 / 1024
        ));
    }
    let title: String = if title.trim().is_empty() {
        "会话".to_string()
    } else {
        title.trim().chars().take(60).collect()
    };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let agent_root = home.join("agent");
    let rel = crate::export::export_rel_path(&title, &stamp, BUNDLE_EXTENSION);
    let abs = crate::export::export_abs_path(&agent_root, &rel);
    if let Some(parent) = abs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建导出目录失败: {e}"))?;
    }
    // 先写临时文件再改名：中断不留半截的包。
    let tmp = abs.with_file_name(format!(
        "{}.tmp",
        abs.file_name().and_then(|n| n.to_str()).unwrap_or("bundle")
    ));
    let written = std::fs::write(&tmp, bytes).and_then(|()| std::fs::rename(&tmp, &abs));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("写入会话包失败: {e}"));
    }
    Ok(rel)
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// 与界面新建会话同一种形态的会话键（`c-<时间><随机>`）。
fn new_chat_id() -> String {
    format!(
        "c-{}{}",
        base36(super::chrono_like_ts() as u64),
        base36(u64::from(rand::random::<u16>() % 10_000))
    )
}

/// 会话包导入成哪个会话键。包里的键是桌面通道的普通会话键就沿用（被占用时引擎自动加
/// 后缀）；别的通道来的、或者落在桌面端有特殊含义的前缀上的（`bot-` 是 bot 收件箱，
/// `grp-` 是群成员会话，都不进会话列表），换一个新键。包头读不出来时不管，由导入去报错。
fn import_chat_id(text: &str) -> Option<String> {
    let header = text.lines().find(|line| !line.trim().is_empty())?;
    let header: serde_json::Value = serde_json::from_str(header).ok()?;
    let channel = header.get("channel")?.as_str()?;
    let chat_id = header.get("chat_id")?.as_str()?;
    let keep = channel == CHANNEL_DESKTOP
        && is_valid_chat_id(chat_id)
        && !chat_id.starts_with("bot-")
        && !chat_id.starts_with("grp-");
    (!keep).then(new_chat_id)
}

/// 导入一个会话包：总是新建一个会话，从不覆盖已有的。返回新会话的键。
#[tauri::command]
pub async fn session_import_bundle(
    state: State<'_, AppState>,
    text: String,
) -> Result<ImportedDto, String> {
    if text.len() > super::UPLOAD_MAX_BYTES {
        return Err(format!(
            "会话包太大（超过 {} MB）",
            super::UPLOAD_MAX_BYTES / 1024 / 1024
        ));
    }
    let engine = handle(&state)?;
    run_blocking("导入会话包", move || {
        let options = ImportOptions {
            channel: Some(CHANNEL_DESKTOP.to_string()),
            chat_id: import_chat_id(&text),
        };
        engine
            .import_session(&text, &options)
            .map(ImportedDto::from)
            .map_err(|e| format!("导入会话包失败: {e}"))
    })
    .await
}

// ---------------------------------------------------------------------------
// 配对码的二维码：生成与识别
// ---------------------------------------------------------------------------

/// 二维码四周留白的模块数（规范要求至少 4）。
const QR_QUIET_MODULES: usize = 4;
/// 二维码图片边长的目标像素数：每个模块的像素数由它除出来。
const QR_TARGET_PX: usize = 560;

/// 把配对码画成二维码（PNG）。纠错等级取最低：配对码已经很长，屏幕上显示也不会破损。
fn qr_png(text: &str) -> Result<Vec<u8>, String> {
    let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L)
        .map_err(|_| "配对码太长，画不成二维码；请复制文字".to_string())?;
    let modules = code.width();
    let colors = code.to_colors();
    let total = modules + 2 * QR_QUIET_MODULES;
    let scale = (QR_TARGET_PX / total).clamp(3, 12);
    let side = u32::try_from(total * scale).map_err(|_| "二维码尺寸异常".to_string())?;
    let mut image = image::GrayImage::from_pixel(side, side, image::Luma([255u8]));
    for (index, color) in colors.iter().enumerate() {
        if *color != qrcode::Color::Dark {
            continue;
        }
        let left = (index % modules + QR_QUIET_MODULES) * scale;
        let top = (index / modules + QR_QUIET_MODULES) * scale;
        for dy in 0..scale {
            for dx in 0..scale {
                image.put_pixel((left + dx) as u32, (top + dy) as u32, image::Luma([0u8]));
            }
        }
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageLuma8(image)
        .write_with_encoder(image::codecs::png::PngEncoder::new(&mut bytes))
        .map_err(|e| format!("生成二维码失败: {e}"))?;
    Ok(bytes)
}

/// 识别时依次试的长边像素数。手机拍的照片有上千万像素，二维码只占其中一块：先缩小了找
/// （快，也顺带抹掉噪点），找不到再用更大的图。
const QR_SCAN_EDGES: [u32; 4] = [1024, 1600, 2400, 3200];

/// 在一张灰度图里找配对码的二维码。
fn find_pairing_code(image: &image::GrayImage) -> Option<String> {
    let (width, height) = (image.width() as usize, image.height() as usize);
    let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(width, height, |x, y| {
        image.get_pixel(x as u32, y as u32).0[0]
    });
    prepared.detect_grids().into_iter().find_map(|grid| {
        let (_, text) = grid.decode().ok()?;
        let text = text.trim();
        text.starts_with(rclaw_sync::pairing::PAIRING_PREFIX)
            .then(|| text.to_string())
    })
}

/// 从一张图片（照片、截图）里读出配对码。
fn decode_pairing_qr(bytes: &[u8]) -> Result<String, String> {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(super::MEDIA_DECODE_MAX_EDGE);
    limits.max_image_height = Some(super::MEDIA_DECODE_MAX_EDGE);
    limits.max_alloc = Some(super::MEDIA_DECODE_MAX_ALLOC);
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| format!("识别图片格式失败: {e}"))?;
    reader.limits(limits);
    let grey = reader
        .decode()
        .map_err(|e| format!("这张图片打不开: {e}"))?
        .into_luma8();
    let long_edge = grey.width().max(grey.height());
    for target in QR_SCAN_EDGES {
        let found = if long_edge > target {
            let ratio = f64::from(target) / f64::from(long_edge);
            let width = ((f64::from(grey.width()) * ratio).round() as u32).max(1);
            let height = ((f64::from(grey.height()) * ratio).round() as u32).max(1);
            let scaled = image::imageops::resize(
                &grey,
                width,
                height,
                image::imageops::FilterType::Triangle,
            );
            find_pairing_code(&scaled)
        } else {
            find_pairing_code(&grey)
        };
        if let Some(code) = found {
            return Ok(code);
        }
        if long_edge <= target {
            break;
        }
    }
    Err(
        "没有在图片里找到配对码的二维码。请让二维码完整、清晰地出现在画面里再试一次，\
         或者改用复制粘贴"
            .into(),
    )
}

/// 「从图片识别」：用户选了一张二维码的图片（截图、照片）。图片只在内存里过一遍，不落盘。
#[tauri::command]
pub async fn sync_scan_qr_image(data_base64: String) -> Result<String, String> {
    use base64::Engine;
    let limit = super::MEDIA_MAX_BYTES as usize;
    if data_base64.len() > limit / 3 * 4 + 4 {
        return Err(format!("图片太大（超过 {} MB）", limit / 1024 / 1024));
    }
    run_blocking("识别二维码", move || {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_base64.as_bytes())
            .map_err(|e| format!("图片数据解码失败: {e}"))?;
        decode_pairing_qr(&bytes)
    })
    .await
}

// ---------------------------------------------------------------------------
// 应用内扫码（APK）：用系统相机拍下二维码，在本机识别
// ---------------------------------------------------------------------------
//
// 走的是会话里「拍照」的同一条采集通道（系统相机拍完，结果写进 capture_result.json），但
// 这张照片拍的是配对码，等同于钥匙，所以它有自己的采集类型 `scan`，和会话的照片分开：
// - 照片不进工作区，按 `sync/scan/qr-<requestId>.jpg` 落在固定目录里。同步目录是文件工具
//   的禁区，文件面板也看不到；文件名带本轮的请求标识，旧请求迟到的回调只会删自己那张。
// - 结果是 `{"kind":"scan","request_id":...}`，不带路径；会话那边的消费入口见到它不交给
//   会话，扫码这边也只认自己 requestId 的结果。
// - 扫码期间数据目录里有一个标记，内容是这一轮的 requestId。相机回来时标记已经没了或换成
//   了下一轮（用户取消了、被收拾掉了），Kotlin 侧（`CaptureBridge`）当场把照片删掉，不写
//   结果；用户拒绝相机权限时，Kotlin 侧立刻写下一份「取消」的终态。
// 这里读到结果之后把照片读进内存、删掉文件，再识别。没走完的那一次，在应用启动时、下一次
// 扫码或拍照之前被收拾掉；旧版本固定名的 `qr.jpg` 与内容为 `1` 的标记也在这时清掉。

/// 「正在扫配对码」的标记文件（数据目录下，与 capture_result.json 并列），内容是本轮
/// requestId；旧版本写的是 `1`，对不上任何请求，按「不是本轮」处理。
const SCAN_MARKER: &str = "capture_scan.pending";
const CAPTURE_RESULT: &str = "capture_result.json";
/// 扫码拍下的照片目录（相对数据目录）；照片按 `qr-<requestId>.jpg` 命名，与
/// `CaptureBridge.scanFile` 一致。
const SCAN_PHOTO_DIR: &str = "sync/scan";
/// 扫码在采集通道里的类型名；与 `NativeBridge.capture` 一致。
const SCAN_KIND: &str = "scan";

/// 扫码的文件操作串行化：开始 / 轮询 / 取消可能同时进来（用户在界面切换），而「检查标记
/// 再删文件」不是一步原子操作。
static SCAN_LOCK: Mutex<()> = Mutex::new(());

fn scan_lock() -> MutexGuard<'static, ()> {
    SCAN_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 本轮扫码的请求标识：时间戳 + 进程内序号。只含十六进制与 `-`，可以直接拼进文件名。
/// 桌面端不扫码，只有 Android 与测试用到。
#[cfg(any(target_os = "android", test))]
fn new_scan_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{:x}", NEXT.fetch_add(1, Ordering::Relaxed))
}

/// requestId 会拼进文件名（也会从界面传回来），只接受自己生成的形态。
fn valid_scan_request_id(request_id: &str) -> bool {
    !request_id.is_empty()
        && request_id.len() <= 64
        && request_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
}

/// 标记文件的内容（不管是不是某一轮）；没有标记时是 None。
fn read_scan_marker(home: &Path) -> Option<String> {
    std::fs::read_to_string(home.join(SCAN_MARKER))
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
}

/// 标记文件里记的是不是这一轮。标记没了、或者已经换成了别的请求，都算不是。
fn scan_requested(home: &Path, request_id: &str) -> bool {
    read_scan_marker(home).as_deref() == Some(request_id)
}

/// 有一次扫码还没走完（标记存在，不看内容：旧版本留下的固定名标记也算）。
pub(super) fn scan_pending(home: &Path) -> bool {
    home.join(SCAN_MARKER).exists()
}

/// 一份采集结果是不是扫码的（读不懂的不算：那是会话那边要报的错）。
pub(super) fn is_scan_result(raw: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|value| {
            value
                .get("kind")
                .and_then(|kind| kind.as_str())
                .map(|kind| kind == SCAN_KIND)
        })
        .unwrap_or(false)
}

/// 一轮扫码拍下的照片。位置按 requestId 分开：旧请求迟到的回调只会动自己那张。
fn scan_photo_path(home: &Path, request_id: &str) -> PathBuf {
    home.join(SCAN_PHOTO_DIR)
        .join(format!("qr-{request_id}.jpg"))
}

/// 删掉这一轮扫码拍下的照片。返回它是不是确实不在了。
fn remove_scan_photo(home: &Path, request_id: &str) -> bool {
    match std::fs::remove_file(scan_photo_path(home, request_id)) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            tracing::warn!("扫码拍下的照片没能删除: {e}");
            false
        }
    }
}

/// 删掉扫码目录里的全部照片（旧版本的固定名 `qr.jpg` 与按 requestId 命名的都在这里）。
/// 只在整轮收拾时用：拍照期间应用被杀，留下来的照片等同于配对码。
fn remove_all_scan_photos(home: &Path) {
    let Ok(entries) = std::fs::read_dir(home.join(SCAN_PHOTO_DIR)) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::warn!("扫码拍下的照片没能删除: {e}");
        }
    }
}

/// 一份采集结果里记的 requestId；不是扫码结果的返回 None。
fn scan_result_request(raw: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    if value.get("kind").and_then(|kind| kind.as_str()) != Some(SCAN_KIND) {
        return None;
    }
    value
        .get("request_id")
        .and_then(|request_id| request_id.as_str())
        .map(|request_id| request_id.to_string())
}

/// 扫码拍下的照片：已经读进内存，文件已经删掉。
struct ScanPhoto {
    bytes: Result<Vec<u8>, String>,
    /// 文件没删掉（要告诉用户：这张照片等同于配对码）。
    leftover: bool,
}

enum ScanCapture {
    /// 相机还没回来。
    Waiting,
    /// 用户放弃了拍照。
    Cancelled,
    Photo(ScanPhoto),
}

/// 把这一轮的照片读进内存，然后不管读没读成都删掉文件。
fn read_and_remove(home: &Path, request_id: &str) -> ScanPhoto {
    let abs = scan_photo_path(home, request_id);
    let bytes = match std::fs::metadata(&abs) {
        Ok(meta) if meta.len() > super::MEDIA_MAX_BYTES => Err("照片太大，识别不了".to_string()),
        _ => std::fs::read(&abs).map_err(|e| format!("读取照片失败: {e}")),
    };
    let leftover = !remove_scan_photo(home, request_id);
    ScanPhoto { bytes, leftover }
}

/// 取走这一轮扫码的采集结果。拿到结果（照片或「放弃」）之后，本轮的结果文件与标记一并
/// 清掉；照片先删、标记后清，中途被打断的话照片也已经不在了。结果里没有本轮的 requestId
/// （旧请求迟到的、旧版本写的）就不算数；标记已经换成了下一轮就作废，不碰任何文件。
fn take_scan_capture(home: &Path, request_id: &str) -> ScanCapture {
    let result_path = home.join(CAPTURE_RESULT);
    let Ok(raw) = std::fs::read_to_string(&result_path) else {
        return ScanCapture::Waiting;
    };
    // 结果文件正写到一半（原子改名失败时的退路）下次再读；会话拍照的结果不归这里管。
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return ScanCapture::Waiting;
    };
    if value.get("kind").and_then(|kind| kind.as_str()) != Some(SCAN_KIND) {
        return ScanCapture::Waiting;
    }
    if value.get("request_id").and_then(|id| id.as_str()) != Some(request_id) {
        return ScanCapture::Waiting;
    }
    // 标记已经被取消或者换成了下一轮：这份结果不作数，也不动新一轮的任何文件。
    if !scan_requested(home, request_id) {
        return ScanCapture::Cancelled;
    }
    let cancelled = value
        .get("cancelled")
        .and_then(|cancelled| cancelled.as_bool())
        .unwrap_or(false);
    let capture = if cancelled {
        remove_scan_photo(home, request_id);
        ScanCapture::Cancelled
    } else {
        ScanCapture::Photo(read_and_remove(home, request_id))
    };
    let _ = std::fs::remove_file(&result_path);
    if scan_requested(home, request_id) {
        let _ = std::fs::remove_file(home.join(SCAN_MARKER));
    }
    capture
}

/// 只收拾这一轮扫码留下的东西：本轮的照片、本轮写下的结果，以及本轮还挂着的标记。
/// 取消旧请求时新一轮可能已经开始，别人的文件一概不碰。
fn discard_scan_request(home: &Path, request_id: &str) {
    remove_scan_photo(home, request_id);
    let result_path = home.join(CAPTURE_RESULT);
    if let Ok(raw) = std::fs::read_to_string(&result_path) {
        if scan_result_request(&raw).as_deref() == Some(request_id) {
            let _ = std::fs::remove_file(&result_path);
        }
    }
    if scan_requested(home, request_id) {
        let _ = std::fs::remove_file(home.join(SCAN_MARKER));
    }
}

/// 收拾扫码留下的东西（界面被回收、应用被杀、用户点了取消、相机在取消之后才回来）：照片
/// 无条件删掉，扫码的结果与标记清掉。会话拍照的结果不归这里管。没有东西可收拾时只是
/// 几次落空的删除，所以应用启动时、每次扫码和拍照之前都调一遍。
pub(crate) fn discard_scan(home: &Path) {
    let _guard = scan_lock();
    discard_scan_files(home);
}

fn discard_scan_files(home: &Path) {
    remove_all_scan_photos(home);
    let result_path = home.join(CAPTURE_RESULT);
    if let Ok(raw) = std::fs::read_to_string(&result_path) {
        if is_scan_result(&raw) {
            let _ = std::fs::remove_file(&result_path);
        }
    }
    let _ = std::fs::remove_file(home.join(SCAN_MARKER));
}

/// 扫码的进展。不派生 `Debug`：`code` 是配对码。
#[derive(Serialize)]
pub struct ScanDto {
    /// `waiting`（相机还没回来）、`cancelled`（没有在扫码，或者用户放弃了拍照）、`done`
    /// （识别出来了，见 `code`）、`failed`（拍了但没识别出来，见 `error`）。
    pub state: &'static str,
    pub code: Option<String>,
    pub error: Option<String>,
    /// 照片没能删掉时的提醒。
    pub warning: Option<String>,
}

impl ScanDto {
    fn only(state: &'static str) -> ScanDto {
        ScanDto {
            state,
            code: None,
            error: None,
            warning: None,
        }
    }
}

/// 开始扫码的结果：本轮请求标识 + 相机状态（`ok` / `permission_requested`）。之后的轮询
/// 与取消都带着这个 requestId。
#[derive(Serialize)]
pub struct ScanStartDto {
    pub request_id: String,
    pub status: String,
}

/// 开始扫码：拉起系统相机。返回本轮 requestId 与 `ok` / `permission_requested`（系统正在
/// 问相机权限，用户同意后相机自动打开）。之后界面带着 requestId 轮询
/// [`sync_scan_qr_poll`]。
#[tauri::command]
pub async fn sync_scan_qr_start(state: State<'_, AppState>) -> Result<ScanStartDto, String> {
    #[cfg(target_os = "android")]
    {
        let home = state.home.clone();
        let request_id = new_scan_request_id();
        let id = request_id.clone();
        // 文件操作和拉起相机（JNI）都放到阻塞线程上。
        let status = run_blocking("准备扫码", move || {
            let _guard = scan_lock();
            // 上一轮（包括旧版本固定名的标记 / 照片）留下的东西先收拾干净。
            discard_scan_files(&home);
            // 会话拍照的结果不归扫码管：这一轮相机的扫描结果会把它覆盖。
            std::fs::write(home.join(SCAN_MARKER), &id)
                .map_err(|e| format!("准备扫码失败: {e}"))?;
            let started = match rclaw_android::capture(SCAN_KIND, &id) {
                Ok(status) => match status.strip_prefix("error:") {
                    Some(reason) => Err(format!("相机不可用: {reason}")),
                    None => Ok(status),
                },
                Err(e) => Err(format!("无法启动相机: {e}")),
            };
            if started.is_err() {
                discard_scan_request(&home, &id);
            }
            started
        })
        .await?;
        Ok(ScanStartDto { request_id, status })
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = state;
        Err("拍照扫码只在手机上可用；电脑上请粘贴配对码，或者选一张二维码的图片".into())
    }
}

/// 看扫码走到哪了。相机回来之后的第一次调用会取走照片、删掉它、识别出配对码。带
/// requestId：取消后重新扫码时，旧请求的轮询不能取走新一轮的结果。
#[tauri::command]
pub async fn sync_scan_qr_poll(
    state: State<'_, AppState>,
    request_id: String,
) -> Result<ScanDto, String> {
    let home = state.home.clone();
    run_blocking("识别二维码", move || {
        if !valid_scan_request_id(&request_id) {
            return Err("无效的扫码请求".to_string());
        }
        let _guard = scan_lock();
        if !scan_requested(&home, &request_id) {
            // 没有在扫这一轮（取消了，或者已经开始下一轮）：只收拾本轮自己的东西。
            if read_scan_marker(&home).is_some() {
                discard_scan_request(&home, &request_id);
            } else {
                // 标记也没了：整轮收拾（含旧版本留下的固定名文件），不碰别的请求。
                discard_scan_files(&home);
            }
            return Ok(ScanDto::only("cancelled"));
        }
        Ok(match take_scan_capture(&home, &request_id) {
            ScanCapture::Waiting => ScanDto::only("waiting"),
            ScanCapture::Cancelled => ScanDto::only("cancelled"),
            ScanCapture::Photo(photo) => {
                let warning = photo.leftover.then(|| {
                    "拍下的二维码照片没能立刻删除（它等同于配对码）。应用下次启动、\
                     下次扫码或拍照时会再删一次"
                        .to_string()
                });
                match photo.bytes.and_then(|bytes| decode_pairing_qr(&bytes)) {
                    Ok(code) => ScanDto {
                        state: "done",
                        code: Some(code),
                        error: None,
                        warning,
                    },
                    Err(error) => ScanDto {
                        state: "failed",
                        code: None,
                        error: Some(error),
                        warning,
                    },
                }
            }
        })
    })
    .await
}

/// 取消扫码：这一轮已经拍下的照片与结果删掉，标记清掉。相机还开着的话，它回来时标记已经
/// 不是这一轮了，Kotlin 侧当场把照片删掉、不写结果；权限弹窗迟到的授权回调同样作废。
/// 取消旧请求不会碰新一轮的标记、照片和结果。
#[tauri::command]
pub async fn sync_scan_qr_cancel(
    state: State<'_, AppState>,
    request_id: String,
) -> Result<(), String> {
    let home = state.home.clone();
    run_blocking("取消扫码", move || {
        if valid_scan_request_id(&request_id) {
            let _guard = scan_lock();
            discard_scan_request(&home, &request_id);
        }
        Ok(())
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairing_code() -> String {
        // 形态对就行（前缀 + 一段 base64url）；这里不验证它能不能加入空间。
        format!(
            "{}{}",
            rclaw_sync::pairing::PAIRING_PREFIX,
            "eyJzIjoiMDAxMTIyMzM0NDU1NjY3NyIsImsiOiJBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB"
                .repeat(4)
        )
    }

    #[test]
    fn a_pairing_code_survives_the_qr_round_trip() {
        let code = pairing_code();
        let png = qr_png(&code).unwrap();
        assert_eq!(decode_pairing_qr(&png).unwrap(), code);
        // 放进一张更大的「照片」里（四周是灰底），照样找得到。
        let qr = image::load_from_memory(&png).unwrap().into_luma8();
        let mut photo =
            image::GrayImage::from_pixel(qr.width() * 3, qr.height() * 2, image::Luma([200u8]));
        image::imageops::overlay(
            &mut photo,
            &qr,
            i64::from(qr.width()),
            i64::from(qr.height() / 2),
        );
        let mut bytes = Vec::new();
        image::DynamicImage::ImageLuma8(photo)
            .write_with_encoder(image::codecs::png::PngEncoder::new(&mut bytes))
            .unwrap();
        assert_eq!(decode_pairing_qr(&bytes).unwrap(), code);
    }

    #[test]
    fn images_without_a_pairing_code_are_refused() {
        // 别的内容的二维码不算。
        let other = qr_png("https://example.com/").unwrap();
        assert!(decode_pairing_qr(&other).is_err());
        // 不是图片。
        assert!(decode_pairing_qr(b"not an image").is_err());
        // 太长画不出来的配对码：报错而不是崩溃。
        assert!(qr_png(&"x".repeat(5000)).is_err());
    }

    #[test]
    fn snapshot_paths_stay_inside_the_three_reference_dirs() {
        assert_eq!(workspace_rel_for("media/a.png").unwrap(), "media/a.png");
        assert_eq!(
            workspace_rel_for("output/2026/报告 最终版.docx").unwrap(),
            "output/2026/报告 最终版.docx"
        );
        // 别的系统上合法、Windows 上用不了的字符换成下划线。
        assert_eq!(
            workspace_rel_for("media/2026/x:y?.jpg").unwrap(),
            "media/2026/x_y_.jpg"
        );
        for bad in [
            "SOUL.md",
            "skills/x/SKILL.md",
            "media",
            "media/",
            "media/../SOUL.md",
            "media/./a.png",
            "/media/a.png",
            "uploads//a.png",
            "media/...",
        ] {
            assert!(workspace_rel_for(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn saving_a_snapshot_never_overwrites_a_different_file() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("agent")).unwrap();
        let cached = home.join("cached.bin");
        std::fs::write(&cached, b"remote version").unwrap();

        // 位置空着：存在原来的路径上（目录一并建出来）。
        assert_eq!(
            save_snapshot(home, "output/报告/a.txt", &cached).unwrap(),
            "output/报告/a.txt"
        );
        let target = home.join("agent/output/报告/a.txt");
        assert_eq!(std::fs::read(&target).unwrap(), b"remote version");
        // 已经有一份一模一样的：不再另存。
        assert_eq!(
            save_snapshot(home, "output/报告/a.txt", &cached).unwrap(),
            "output/报告/a.txt"
        );
        assert_eq!(
            std::fs::read_dir(target.parent().unwrap()).unwrap().count(),
            1
        );
        // 本机同名文件内容不同：不覆盖，另存为「-快照」。
        std::fs::write(&target, b"local version").unwrap();
        assert_eq!(
            save_snapshot(home, "output/报告/a.txt", &cached).unwrap(),
            "output/报告/a-快照.txt"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"local version");
        assert_eq!(
            std::fs::read(home.join("agent/output/报告/a-快照.txt")).unwrap(),
            b"remote version"
        );
        // 路径上有个同名的文件挡着：报错，不往别处写。
        std::fs::write(home.join("agent/media"), b"not a dir").unwrap();
        assert!(save_snapshot(home, "media/b.png", &cached).is_err());
        // 不在三个引用目录下的路径一律拒绝。
        assert!(save_snapshot(home, "config/SOUL.md", &cached).is_err());
        assert!(!home.join("agent/config").exists());
    }

    #[test]
    fn bundles_from_elsewhere_get_a_fresh_desktop_chat_id() {
        let header = |channel: &str, chat_id: &str| {
            format!(
                "{}\n{{\"gid\":\"x\"}}\n",
                serde_json::json!({ "rclaw_bundle": 1, "channel": channel, "chat_id": chat_id })
            )
        };
        // 桌面通道的普通会话键沿用。
        assert_eq!(import_chat_id(&header("desktop", "c-abc123")), None);
        assert_eq!(import_chat_id(&header("desktop", "main")), None);
        // 桌面端有特殊含义的前缀、别的通道的键：换新键。
        for (channel, chat_id) in [
            ("desktop", "bot-researcher"),
            ("desktop", "grp-team-a"),
            ("weixin", "wxid_abc@chatroom"),
            ("desktop", "有 空格"),
        ] {
            let fresh = import_chat_id(&header(channel, chat_id)).unwrap();
            assert!(
                fresh.starts_with("c-") && is_valid_chat_id(&fresh),
                "{fresh}"
            );
        }
        // 读不出包头：不管，交给导入去报错。
        assert_eq!(import_chat_id("not json"), None);
        assert_eq!(import_chat_id(""), None);
    }

    #[test]
    fn a_scanned_photo_is_deleted_whether_or_not_it_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let uploads = home.join("agent/uploads");
        std::fs::create_dir_all(&uploads).unwrap();
        // 每一轮扫码有自己的 requestId：照片、结果、标记都按它认领。
        let request_id = "1a2b-3c4d";
        let other_id = "9f8e-7d6c";
        let photo_path = scan_photo_path(home, request_id);
        let other_photo = scan_photo_path(home, other_id);
        std::fs::create_dir_all(photo_path.parent().unwrap()).unwrap();
        let result = home.join(CAPTURE_RESULT);
        let marker = home.join(SCAN_MARKER);
        let taken = format!(r#"{{"kind":"scan","request_id":"{request_id}"}}"#);
        let cancelled =
            format!(r#"{{"kind":"scan","request_id":"{request_id}","cancelled":true}}"#);
        const CHAT: &str = r#"{"kind":"photo","file":"uploads/chat.jpg"}"#;

        // requestId 只接受自己生成的形态，且每次都不一样。
        let first = new_scan_request_id();
        let second = new_scan_request_id();
        assert_ne!(first, second);
        assert!(valid_scan_request_id(&first) && valid_scan_request_id(&second));
        assert!(!valid_scan_request_id("../a") && !valid_scan_request_id(""));

        // 会话拍照的结果和照片不归扫码管：收拾的时候不碰。
        std::fs::write(&result, CHAT).unwrap();
        std::fs::write(uploads.join("chat.jpg"), b"chat photo").unwrap();
        assert!(!is_scan_result(CHAT) && !is_scan_result("not json") && is_scan_result(&taken));
        discard_scan(home);
        assert!(result.exists() && uploads.join("chat.jpg").exists());

        // 扫码中、相机还没回来（结果文件里还是会话那一份）：等着，不动它。
        std::fs::write(&marker, request_id).unwrap();
        assert!(scan_pending(home));
        assert!(scan_requested(home, request_id));
        assert!(matches!(
            take_scan_capture(home, request_id),
            ScanCapture::Waiting
        ));
        assert!(scan_pending(home) && result.exists());
        std::fs::remove_file(&result).unwrap();
        assert!(matches!(
            take_scan_capture(home, request_id),
            ScanCapture::Waiting
        ));

        // 拍到了二维码：照片读进内存后就删，标记与结果文件清掉。
        let code = pairing_code();
        std::fs::write(&photo_path, qr_png(&code).unwrap()).unwrap();
        std::fs::write(&result, &taken).unwrap();
        let ScanCapture::Photo(photo) = take_scan_capture(home, request_id) else {
            panic!("应该拿到照片");
        };
        assert!(!photo.leftover);
        assert_eq!(decode_pairing_qr(&photo.bytes.unwrap()).unwrap(), code);
        assert!(!photo_path.exists() && !result.exists() && !scan_pending(home));

        // 拍的不是二维码：识别不出来，照片照样删。
        std::fs::write(&marker, request_id).unwrap();
        std::fs::write(&photo_path, b"not an image").unwrap();
        std::fs::write(&result, &taken).unwrap();
        let ScanCapture::Photo(photo) = take_scan_capture(home, request_id) else {
            panic!("应该拿到照片");
        };
        assert!(decode_pairing_qr(&photo.bytes.unwrap()).is_err());
        assert!(!photo_path.exists() && !result.exists() && !scan_pending(home));

        // 用户放弃了拍照：就算相机留下了半张照片也删掉。
        std::fs::write(&marker, request_id).unwrap();
        std::fs::write(&photo_path, b"half").unwrap();
        std::fs::write(&result, &cancelled).unwrap();
        assert!(matches!(
            take_scan_capture(home, request_id),
            ScanCapture::Cancelled
        ));
        assert!(!photo_path.exists() && !result.exists() && !scan_pending(home));

        // 结果说拍了、照片却不在：报错，不崩溃。
        std::fs::write(&marker, request_id).unwrap();
        std::fs::write(&result, &taken).unwrap();
        let ScanCapture::Photo(photo) = take_scan_capture(home, request_id) else {
            panic!("应该走到照片分支");
        };
        assert!(photo.bytes.is_err() && !photo.leftover);

        // 取消后重新扫码、旧相机回调迟到：标记已经是新一轮的，旧结果作废，谁也不碰。
        std::fs::write(&marker, other_id).unwrap();
        std::fs::write(&photo_path, b"old qr photo").unwrap();
        std::fs::write(&result, &taken).unwrap();
        assert!(matches!(
            take_scan_capture(home, request_id),
            ScanCapture::Cancelled
        ));
        assert!(photo_path.exists() && result.exists() && scan_requested(home, other_id));

        // 旧请求的取消只收拾自己的照片与结果，不碰新一轮的标记和照片。
        std::fs::write(&other_photo, b"new qr photo").unwrap();
        discard_scan_request(home, request_id);
        assert!(!photo_path.exists() && !result.exists());
        assert!(other_photo.exists() && scan_requested(home, other_id));

        // 整轮收拾（启动 / 开始扫码 / 会话拍照之前）：旧版本固定名与本轮的文件一起删掉，
        // 会话的照片不受影响。
        std::fs::write(&marker, b"1").unwrap();
        std::fs::write(home.join(SCAN_PHOTO_DIR).join("qr.jpg"), b"legacy").unwrap();
        std::fs::write(&photo_path, b"qr photo").unwrap();
        std::fs::write(&other_photo, b"qr photo").unwrap();
        std::fs::write(
            &result,
            format!(r#"{{"kind":"scan","request_id":"{other_id}"}}"#),
        )
        .unwrap();
        discard_scan(home);
        assert!(!photo_path.exists() && !other_photo.exists() && !result.exists());
        assert!(!home.join(SCAN_PHOTO_DIR).join("qr.jpg").exists() && !scan_pending(home));

        // 会话的照片自始至终不受影响。
        assert!(uploads.join("chat.jpg").exists());
    }
}
