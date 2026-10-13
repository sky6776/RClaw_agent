//! Tauri command 层：前端与核心之间的唯一通道。
//!
//! 命令面刻意保持最小——对话消息（含斜杠命令）统一走 inbound 总线，
//! 配置类操作（/llmodel 等）复用既有 slash 管道，不另开配置命令。
//! 多会话以 chat_id 区分（同一 desktop 通道，TurnManager 按 (channel, chat_id)
//! 各自维护活动 turn）；归档 = `state.sqlite` 里的 `archived_at` 标记（消息不动）。

use rclaw_tools::files::is_dos_device_name;
use serde::Serialize;
use tauri::{AppHandle, Manager, State};

use crate::agent::{self, is_valid_chat_id, CHANNEL_DESKTOP};
use crate::home;
use crate::state::AppState;
use crate::wizard::{self, WizardParams};


// ---------------------------------------------------------------------------
// 检查更新：GitHub releases（桌面与 Android 同一逻辑——提示用户去下载页，
// 不做自动下载安装；Android 侧载分发 = 重装 APK，桌面 = 下载安装包）。
// ---------------------------------------------------------------------------

/// 仓库 coordinates：releases 以 GitHub Release 发布（tag 形如 v2.5.7）。
/// 注意 /releases/latest 只返回正式 release（不含 prerelease）——CI 每次推送
/// 发布的 nightly 是 prerelease，不参与更新检查；发正式版走「升版本号 + 打
/// 正式 tag」（见 .github/workflows/release.yml 注释）。
const UPDATE_REPO_API: &str = "https://api.github.com/repos/sky6776/RClaw_agent/releases/latest";

/// 检查更新结果。
#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    /// 当前运行版本。
    pub current: String,
    /// 最新发布版本（查询失败为 None）。
    pub latest: Option<String>,
    /// 最新版发布页 URL。
    pub url: Option<String>,
    /// 发布说明（release body，截断到 2000 字符）。
    pub notes: Option<String>,
    /// 查询失败原因（latest 为 None 时给出）。
    pub error: Option<String>,
}

/// `v?x.y.z` → (x, y, z) 数值段（解析失败的段按 0 处理）。
fn parse_version(text: &str) -> Vec<u64> {
    text.trim()
        .trim_start_matches('v')
        .trim_start_matches('V')
        .split('.')
        .map(|part| part.trim().parse().unwrap_or(0))
        .collect()
}

/// 段落数值比较：left > right 返回 Greater。
fn version_cmp(left: &[u64], right: &[u64]) -> std::cmp::Ordering {
    let len = left.len().max(right.len());
    for i in 0..len {
        let l = left.get(i).copied().unwrap_or(0);
        let r = right.get(i).copied().unwrap_or(0);
        if l != r {
            return l.cmp(&r);
        }
    }
    std::cmp::Ordering::Equal
}

/// 查询 GitHub 最新 release 并与当前版本比较。网络失败不报错——返回
/// `error` 字段由前端展示（检查更新是用户主动动作，失败要可见而非静默）。
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<UpdateInfo, String> {
    let current = app.package_info().version.to_string();
    let client = reqwest::Client::builder()
        .user_agent(concat!("RClaw/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| format!("HTTP 客户端构建失败: {e}"))?;
    let response = client
        .get(UPDATE_REPO_API)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await;
    let response = match response {
        Ok(response) => response,
        Err(error) => {
            return Ok(UpdateInfo {
                current,
                latest: None,
                url: None,
                notes: None,
                error: Some(format!("网络请求失败: {error}")),
            });
        }
    };
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        // releases/latest 在仓库没有任何 Release 时返回 404——这是正常初期状态。
        return Ok(UpdateInfo {
            current,
            latest: None,
            url: None,
            notes: None,
            error: Some("仓库还没有发布过版本".into()),
        });
    }
    if !response.status().is_success() {
        return Ok(UpdateInfo {
            current,
            latest: None,
            url: None,
            notes: None,
            error: Some(format!("GitHub 返回 {}", response.status())),
        });
    }
    #[derive(serde::Deserialize)]
    struct Release {
        tag_name: String,
        html_url: String,
        #[serde(default)]
        body: Option<String>,
    }
    let release: Release = response
        .json()
        .await
        .map_err(|e| format!("解析 release JSON 失败: {e}"))?;
    let latest = release.tag_name.trim_start_matches('v').to_string();
    let has_newer =
        version_cmp(&parse_version(&latest), &parse_version(&current)) == std::cmp::Ordering::Greater;
    let mut notes = release.body;
    if let Some(body) = notes.as_mut() {
        body.truncate(2000);
    }
    Ok(UpdateInfo {
        current,
        latest: if has_newer { Some(latest) } else { None },
        url: Some(release.html_url),
        notes,
        error: None,
    })
}

/// 用系统方式打开外部链接：Android 经 JNI intent（应用内注册的 context），
/// 桌面 Windows 走 rundll32 url.dll,FileProtocolHandler。
#[tauri::command]
pub async fn open_url(url: String) -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        rclaw_android::open_url(&url).map_err(|e| format!("打开链接失败: {e}"))
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        // 不经 cmd /c start：cmd 把参数里的 & 当命令分隔符（URL 来自模型输出，
        // 可构造注入）；rundll32 由 CreateProcess 直接收单参数，天然免疫。
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", &url])
            .spawn()
            .map_err(|e| format!("打开链接失败: {e}"))?;
        Ok(())
    }
}

/// `get_status` 返回值。
#[derive(Debug, Serialize)]
pub struct StatusDto {
    /// 是否需要首运行向导（rclaw.toml 不存在）。
    pub wizard_needed: bool,
    /// agent 是否已就绪。
    pub agent_ready: bool,
    /// 选中的 provider 名（未启动为 null）。
    pub provider: Option<String>,
    /// 选中的模型名（未启动为 null）。
    pub model: Option<String>,
    /// 用户数据目录（诊断 / 关于页展示）。
    pub home: String,
    /// 最近一次 agent 启动失败的错误文案（前端轮询兜底：error 事件被错过时
    /// 据此从「启动中」转入错误态；None = 无失败记录）。
    pub agent_error: Option<String>,
}

#[tauri::command]
pub async fn get_status(state: State<'_, AppState>) -> Result<StatusDto, String> {
    let (provider, model) = match state.provider_summary() {
        Some((provider, model)) => (Some(provider), model),
        None => (None, None),
    };
    Ok(StatusDto {
        wizard_needed: !home::has_config(&state.home),
        agent_ready: state.provider_summary().is_some(),
        provider,
        model,
        home: state.home.to_string_lossy().replace('\\', "/"),
        agent_error: state
            .start_error
            .read()
            .expect("AppState.start_error 锁未被毒化")
            .clone(),
    })
}

/// 复制用户数据目录路径到系统剪贴板（状态条单击；复制原始反斜杠路径）。
#[tauri::command]
pub async fn copy_home_path(state: State<'_, AppState>) -> Result<(), String> {
    let text = state.home.to_string_lossy().to_string();
    tauri::async_runtime::spawn_blocking(move || copy_to_clipboard(&text))
        .await
        .map_err(|e| format!("剪贴板任务失败: {e}"))?
}

/// 在系统文件管理器中打开用户数据目录（状态条双击）。
#[tauri::command]
pub async fn open_home_dir(state: State<'_, AppState>) -> Result<(), String> {
    let path = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || open_in_file_manager(&path))
        .await
        .map_err(|e| format!("打开目录任务失败: {e}"))?
}

/// 写系统剪贴板。不为单一入口引入剪贴板插件依赖：
/// - Windows：Win32 剪贴板 API 直放 UTF-16 文本——管道走 clip.exe 会按控制台
///   代码页（中文系统是 GBK）解码字节，非 ASCII 用户名的路径会复制成乱码；
/// - macOS / Linux：复用系统自带 pbcopy / xclip 走管道。
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        copy_to_clipboard_win(text)
    }
    #[cfg(not(windows))]
    {
        copy_to_clipboard_pipe(text)
    }
}

/// Win32 剪贴板写入（`CF_UNICODETEXT`）。
///
/// 所有权规则：`SetClipboardData` 成功后内存移交系统、不得再释放；任何失败
/// 路径都必须 `GlobalFree` + `CloseClipboard`，不留悬空占用。
#[cfg(windows)]
fn copy_to_clipboard_win(text: &str) -> Result<(), String> {
    use windows_sys::Win32::Foundation::GlobalFree;
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::System::Ole::CF_UNICODETEXT;

    // UTF-16 含终止 NUL；SetClipboardData 要求 GMEM_MOVEABLE（无零填充）。
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("打开剪贴板失败（可能被其它程序占用）".into());
        }
        let outcome = (|| -> Result<(), String> {
            let handle = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if handle.is_null() {
                return Err("分配剪贴板内存失败".into());
            }
            let dst = GlobalLock(handle);
            if dst.is_null() {
                let _ = GlobalFree(handle);
                return Err("锁定剪贴板内存失败".into());
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), dst.cast::<u16>(), wide.len());
            let _ = GlobalUnlock(handle);
            if EmptyClipboard() == 0 {
                let _ = GlobalFree(handle);
                return Err("清空剪贴板失败".into());
            }
            match SetClipboardData(CF_UNICODETEXT as u32, handle.cast()) {
                ok if !ok.is_null() => Ok(()), // 所有权已移交系统
                _ => {
                    let _ = GlobalFree(handle);
                    Err("写入剪贴板失败".into())
                }
            }
        })();
        let _ = CloseClipboard();
        outcome
    }
}

/// 非 Windows 的管道剪贴板写入（macOS pbcopy / Linux xclip）。
#[cfg(not(windows))]
fn copy_to_clipboard_pipe(text: &str) -> Result<(), String> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[cfg(target_os = "macos")]
    let (program, args): (&str, &[&str]) = ("pbcopy", &[]);
    #[cfg(not(target_os = "macos"))]
    let (program, args): (&str, &[&str]) = ("xclip", &["-selection", "clipboard"]);

    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 {program} 失败: {e}"))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|e| format!("写入剪贴板失败: {e}"))?;
    }
    drop(child.stdin.take()); // 关闭 stdin，工具读到 EOF 后落剪贴板。
    child
        .wait()
        .map(|_| ())
        .map_err(|e| format!("{program} 执行失败: {e}"))
}

/// 用系统文件管理器打开目录（Windows explorer / macOS open / Linux xdg-open）。
/// 不等待退出：explorer 即使成功也返回非零退出码，状态不可靠。
fn open_in_file_manager(path: &std::path::Path) -> Result<(), String> {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";

    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {program} 失败: {e}"))
}

/// 保存向导配置并启动 agent。配置已存在时拒绝（向导只跑一次）。
#[tauri::command]
#[allow(clippy::too_many_arguments)] // Tauri 命令参数与前端 invoke 参数一一平铺
pub async fn wizard_save(
    app: AppHandle,
    state: State<'_, AppState>,
    provider: String,
    base_url: String,
    model: String,
    api_key: String,
    context_length: u64,
    auto_evolve: bool,
) -> Result<StatusDto, String> {
    let params = WizardParams {
        provider,
        base_url,
        model,
        api_key,
        context_length,
        auto_evolve,
    };
    wizard::write_config(&state.home, &params)?;
    agent::start_agent(app, state.home.clone()).await?;
    get_status(state).await
}

/// 发送一条对话消息到指定会话（以 `/` 开头即斜杠命令，复用既有 slash 管道）。
#[tauri::command]
pub async fn send_message(app: AppHandle, chat_id: String, content: String) -> Result<(), String> {
    agent::send(&app, chat_id, content).await
}

/// 取消指定会话的 running turn（复用 /cancel 语义：路由层立即回执）。
#[tauri::command]
pub async fn cancel(app: AppHandle, chat_id: String) -> Result<(), String> {
    agent::send(&app, chat_id, "/cancel".to_string()).await
}

// ---------------------------------------------------------------------------
// 多会话：列表 / 历史 / 归档 / 恢复
// ---------------------------------------------------------------------------

/// 会话列表条目。
#[derive(Debug, Serialize)]
pub struct ChatDto {
    pub chat_id: String,
    /// 用户自定义名（desktop_prefs.json 的 chat_titles，经 /chat_rename 写入）优先；
    /// 无自定义名时 = 首条用户消息截断。
    pub title: String,
    /// 最后修改时间（unix 毫秒；每次追加消息时刷新）。
    pub updated_at: u64,
    pub archived: bool,
}

/// 会话相关命令都是同步 SQLite 读写（`SessionStore` 单连接 + 互斥锁：正在跑的
/// turn 写事务期间读会排队）。放到阻塞线程池执行，不占 Tauri 异步运行时的工作线程。
async fn run_blocking<T, F>(label: &str, f: F) -> Result<T, String>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, String> + Send + 'static,
{
    tauri::async_runtime::spawn_blocking(f)
        .await
        .map_err(|e| format!("{label}任务失败: {e}"))?
}

#[tauri::command]
pub async fn chat_list(state: State<'_, AppState>) -> Result<Vec<ChatDto>, String> {
    let session = state.session().ok_or("agent 尚未启动")?;
    let layout = state.layout();
    let home = state.home.clone();
    run_blocking("读取会话列表", move || {
        // 用户自定义会话名（desktop_prefs.json）优先于派生标题。
        let custom_titles = agent::read_chat_titles(&home);
        let summaries = session
            .list_sessions(Some(CHANNEL_DESKTOP))
            .map_err(|e| format!("读取会话列表失败: {e}"))?;
        let mut out = Vec::new();
        for summary in summaries {
            let chat_id = summary.chat_id;
            // Bot Mode：群成员会话（grp-…）是编排内部会话，不进用户会话列表。
            if chat_id.starts_with("grp-") {
                continue;
            }
            let mut derived = extract_title(&session, &chat_id);
            // Bot Mode：bot 收件箱的首条「用户」消息往往是队友交接信封
            // （Message from 🤖 …），原文当标题不可读；改用 bot 显示名。
            if let Some(name) = chat_id.strip_prefix("bot-") {
                if let Some(def) = layout
                    .as_ref()
                    .and_then(|layout| rclaw_store::BotStore::new(&layout.base).get(name).ok())
                {
                    derived = format!("🤖 {}", def.title);
                }
            }
            let title = custom_titles.get(&chat_id).cloned().unwrap_or(derived);
            out.push(ChatDto {
                chat_id,
                title,
                updated_at: summary.updated_at.max(0) as u64,
                archived: summary.archived,
            });
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
        Ok(out)
    })
    .await
}

/// 重命名会话（双击 / 右键菜单触发行内编辑）。标题去空白后为空 = 恢复自动派生标题。
/// 仅写 desktop_prefs.json 的 chat_titles 映射，不动 state.sqlite 里的会话记录。
#[tauri::command]
pub async fn chat_rename(
    state: State<'_, AppState>,
    chat_id: String,
    title: String,
) -> Result<(), String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    if title.trim().chars().count() > 120 {
        return Err("会话名过长（上限 120 字符）".to_string());
    }
    // desktop_prefs.json 的读-改-写是同步文件 IO，不占异步运行时线程。
    let home = state.home.clone();
    run_blocking("重命名会话", move || {
        agent::write_chat_title(&home, &chat_id, &title)
    })
    .await
}

/// 导出完整聊天记录为 Markdown 文本（不做条数裁剪，前端触发下载）。
/// 读完整 transcript（含被压缩进摘要的消息），不是 provider 可见的压缩视图。
#[tauri::command]
pub async fn chat_export(state: State<'_, AppState>, chat_id: String) -> Result<String, String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    let transcript = run_blocking("读取会话", {
        let chat_id = chat_id.clone();
        move || {
            session
                .load_transcript(CHANNEL_DESKTOP, &chat_id)
                .map_err(|e| format!("读取会话失败: {e}"))
        }
    })
    .await?;
    if transcript.is_empty() {
        return Err(format!("会话 {chat_id} 不存在或没有记录"));
    }
    let stamp = |ts: i64| -> String {
        if ts <= 0 {
            return String::new();
        }
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(ts, 0)
            .single()
            .map(|t| format!("（{}）", t.format("%Y-%m-%d %H:%M")))
            .unwrap_or_default()
    };
    let mut out = format!(
        "# 聊天记录：{chat_id}\n\n- 导出时间：{}\n\n---\n\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    for msg in transcript {
        if msg.is_tool_result_carrier() {
            continue;
        }
        match msg.role {
            rclaw_core::Role::User => {
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                let t = text.trim();
                if t.is_empty() || t.starts_with("[CONTEXT COMPACTION") {
                    continue;
                }
                out.push_str(&format!("## 用户{}\n\n{t}\n\n", stamp(msg.ts)));
            }
            rclaw_core::Role::Assistant => {
                let tools: Vec<&str> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        rclaw_core::ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                        _ => None,
                    })
                    .collect();
                if !tools.is_empty() {
                    out.push_str(&format!("> 🔧 调用工具：{}\n\n", tools.join("、")));
                }
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                if !text.trim().is_empty() {
                    out.push_str(&format!("## 助手{}\n\n{}\n\n", stamp(msg.ts), text.trim()));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// 从会话最早的几条用户消息提取标题：首条非压缩摘要的用户文本，截 60 字符。
fn extract_title(session: &rclaw_store::session::SessionStore, chat_id: &str) -> String {
    const CONTEXT_SUMMARY_PREFIX: &str = "[CONTEXT COMPACTION";
    let Ok(messages) = session.first_user_messages(CHANNEL_DESKTOP, chat_id, 8) else {
        return String::new();
    };
    for msg in messages {
        if msg.role != rclaw_core::Role::User || msg.is_tool_result_carrier() {
            continue;
        }
        for block in &msg.content {
            if let rclaw_core::ContentBlock::Text { text } = block {
                let t = text.trim();
                if t.is_empty() || t.starts_with(CONTEXT_SUMMARY_PREFIX) {
                    continue;
                }
                return t.chars().take(60).collect();
            }
        }
    }
    String::new()
}

/// 会话分叉结果。
#[derive(Debug, Serialize)]
pub struct ChatForkDto {
    pub chat_id: String,
    /// 复制进新会话的消息条数。
    pub copied: usize,
}

/// 从任意消息锚点分叉出新会话（ZCode forkStableConversationAtMessage 同源语义，
/// 第十二轮）：消息下方的「分叉」按钮直达这里。原会话不动；新会话出现在会话列表。
#[tauri::command]
pub async fn chat_fork(
    state: State<'_, AppState>,
    chat_id: String,
    anchor_message_id: i64,
    full: Option<bool>,
) -> Result<ChatForkDto, String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    // 新 chat_id：`<orig>-fork<hex>`（白名单字符集，基名收敛到 40 字符）；
    // 撞名（同秒连点两次）追加计数重试。
    let base: String = chat_id.chars().take(40).collect();
    let mut target = format!("{base}-fork{:x}", chrono::Utc::now().timestamp());
    for n in 1..=64 {
        let candidate = if n == 1 {
            target.clone()
        } else {
            format!("{target}-{n}")
        };
        let exists = run_blocking("检查分叉会话 id", {
            let session = session.clone();
            let chat_id = candidate.clone();
            move || Ok(session.session_exists(CHANNEL_DESKTOP, &chat_id).unwrap_or(false))
        })
        .await?;
        if !exists {
            target = candidate;
            break;
        }
        if n == 64 {
            return Err("无法为分叉会话分配新 id，请稍后重试".to_string());
        }
    }
    let fork_target = target.clone();
    let full_transcript = full.unwrap_or(false);
    let fork = run_blocking("分叉会话", move || {
        session
            .fork_session(
                CHANNEL_DESKTOP,
                &chat_id,
                anchor_message_id,
                &fork_target,
                full_transcript,
            )
            .map_err(|e| e.to_string())
    })
    .await?;
    Ok(ChatForkDto {
        chat_id: fork.new_chat_id,
        copied: fork.copied,
    })
}

/// 计划（PlanRun）的一个步骤状态。
#[derive(Debug, Serialize)]
pub struct PlanStepDto {
    pub id: String,
    /// pending | running | verifying | succeeded | failed | skipped。
    pub status: String,
    /// 步骤任务文本（plan_started 里截断到 80 字符）。
    pub task: String,
    /// 最近一次收尾摘要（截断展示）。
    pub summary: String,
}

/// 一个计划的状态投影（供会话面板 / 消息流的流程步骤卡片轮询）。
#[derive(Debug, Serialize)]
pub struct PlanDto {
    pub plan_id: String,
    pub name: String,
    /// 计划是否已整体收尾（plan_completed）。
    pub completed: bool,
    /// 用户暂停中（最后的 plan_paused / plan_resumed|plan_unpaused 事件决定）。
    pub paused: bool,
    /// plan_completed 携带的成功步骤数。
    pub succeeded: Option<u64>,
    /// 最后一条事件的 ts（unix 毫秒），列表按它倒序。
    pub updated_at: i64,
    pub steps: Vec<PlanStepDto>,
}

/// 把一条 plan journal 流的事件投影进 PlanDto（纯函数，可单测）。
///
/// 事件语义：`plan_started` 定步骤全集与名字（兼容旧版 id 数组 / 新版 {id, task}），
/// 同一流上的第二次 started = resume 新一代执行——整体收尾态复位，上一代 succeeded /
/// verifying 的步骤按 resume 语义继承（执行器不重跑也不重报它们）；带
/// `awaiting_verdict` 的 step_finished 投影成 verifying（终局由 `verdict_passed` /
/// `verdict_failed` 判定——直接 ✓ 会把验证否决的目标亮成成功，`verdict_failed` 未耗尽
/// 轮次时目标回 pending 重跑）；`step_skipped` 定 skipped，`step_unverified` 定 failed
/// （验证者不可用时目标终局就是 Failed，执行器侧传染口径一致）；
/// `plan_paused` / `plan_resumed|plan_unpaused` 按事件序翻转暂停态（**必须是独立
/// 分支**——合并进 plan_started 的写法会吞掉 resumed，暂停投影翻不回 false，clippy
/// unreachable_patterns 曾揪出此 bug）；`plan_completed` 定整体收尾（收尾后暂停无意义，
/// 强制清除）。
fn project_plan_events(plan: &mut PlanDto, events: &[(u64, serde_json::Value)]) {
    for (_seq, event) in events {
        // journal 的 ts 是 RFC3339 字符串（plan_event_record 形态）；毫秒 int 只在
        // 手拼事件里出现，一并兼容。解析失败保持上一条的 updated_at。
        let ts = event
            .get("ts")
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|text| {
                        chrono::DateTime::parse_from_rfc3339(text)
                            .ok()
                            .map(|time| time.timestamp_millis())
                    })
                    .or_else(|| value.as_i64())
            })
            .unwrap_or(0);
        if ts > 0 {
            plan.updated_at = ts;
        }
        match event.get("event").and_then(serde_json::Value::as_str) {
            Some("plan_started") => {
                if let Some(name) = event.get("name").and_then(serde_json::Value::as_str) {
                    plan.name = name.to_string();
                }
                // resume 复用同一 journal 流：新一代执行开始必须复位上一代的整体收尾态。
                plan.completed = false;
                plan.succeeded = None;
                plan.paused = false;
                // 兼容两种形态：旧版 steps 为 id 数组，新版为 {id, task} 数组。
                if let Some(steps) = event.get("steps").and_then(serde_json::Value::as_array) {
                    // resume 语义的投影对位：上一代 succeeded 的步骤执行器直接继承
                    // （不重跑、不重发 step_finished），全量归零会让它永远停在 pending；
                    // 等验证者的目标（verifying）同样保留——resume 只补跑验证者。
                    let inherited: std::collections::HashMap<&str, &PlanStepDto> = plan
                        .steps
                        .iter()
                        .filter(|s| s.status == "succeeded" || s.status == "verifying")
                        .map(|s| (s.id.as_str(), s))
                        .collect();
                    plan.steps = steps
                        .iter()
                        .filter_map(|entry| {
                            let mut step = match entry {
                                serde_json::Value::String(id) => PlanStepDto {
                                    id: id.clone(),
                                    status: "pending".into(),
                                    task: String::new(),
                                    summary: String::new(),
                                },
                                serde_json::Value::Object(_) => PlanStepDto {
                                    id: entry
                                        .get("id")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    status: "pending".into(),
                                    task: entry
                                        .get("task")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default()
                                        .to_string(),
                                    summary: String::new(),
                                },
                                _ => return None,
                            };
                            if let Some(prev) = inherited.get(step.id.as_str()) {
                                step.status = prev.status.clone();
                                step.summary = prev.summary.clone();
                            }
                            Some(step)
                        })
                        .collect();
                }
            }
            // resume / 波次边界恢复都不带 steps（清单在 plan_started 里，已装载）；
            // 事件流顺序处理：paused → resumed 按最后一条事件决定投影态。波次边界的
            // 挂起/恢复事件是 plan_unpaused（plan_resumed 是 resume 的代际锚点，
            // open_escalations 据它判定提问归属，二者不可混用）。
            Some("plan_resumed") | Some("plan_unpaused") => {
                if let Some(name) = event.get("name").and_then(serde_json::Value::as_str) {
                    plan.name = name.to_string();
                }
                plan.paused = false;
            }
            Some("plan_completed") => {
                plan.completed = true;
                plan.succeeded = event.get("succeeded").and_then(serde_json::Value::as_u64);
            }
            Some("plan_paused") => plan.paused = true,
            // step_started：步骤级「运行中」（终态事件在其后覆盖；重跑重记按序生效）。
            Some("step_started") => {
                if let Some(id) = event.get("step").and_then(serde_json::Value::as_str) {
                    if let Some(step) = plan.steps.iter_mut().find(|s| s.id == id) {
                        step.status = "running".into();
                    }
                }
            }
            Some("step_finished") => {
                let id = event
                    .get("step")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let outcome = event
                    .get("outcome")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let summary = event
                    .get("summary")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if let Some(step) = plan.steps.iter_mut().find(|s| s.id == id) {
                    // 被验证步骤「执行成功、等待 verdict」→ verifying（非终态）。
                    let awaiting = event
                        .get("awaiting_verdict")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true);
                    step.status = if awaiting { "verifying".into() } else { outcome.to_string() };
                    step.summary = summary.chars().take(160).collect();
                }
            }
            // 对抗环 verdict 落到目标头上：pass → succeeded；fail 且轮次耗尽 → failed
            //（验证者自己的 step_finished 只覆盖验证者）；fail 未耗尽 → 目标带着失败
            // 原因回到待跑（执行器会重跑它）。
            Some("verdict_passed") => {
                let id = event
                    .get("target")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if let Some(step) = plan.steps.iter_mut().find(|s| s.id == id) {
                    step.status = "succeeded".into();
                }
            }
            Some("verdict_failed") => {
                let id = event
                    .get("target")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                let exhausted =
                    event.get("exhausted").and_then(serde_json::Value::as_bool) == Some(true);
                if let Some(step) = plan.steps.iter_mut().find(|s| s.id == id) {
                    step.status = if exhausted { "failed".into() } else { "pending".into() };
                    let reasons = event
                        .get("reasons")
                        .and_then(serde_json::Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(serde_json::Value::as_str)
                                .collect::<Vec<_>>()
                                .join("; ")
                        })
                        .unwrap_or_default();
                    if !reasons.is_empty() {
                        step.summary = reasons.chars().take(160).collect();
                    }
                }
            }
            Some("step_skipped") => {
                let id = event
                    .get("step")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if let Some(step) = plan.steps.iter_mut().find(|s| s.id == id) {
                    step.status = "skipped".into();
                    step.summary = event
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
            }
            // 验证者不可用 / 未给出 verdict 的终局：目标按 Failed 收口（与执行器传染
            // 口径一致），投影成 failed 而不是 skipped。
            Some("step_unverified") => {
                let id = event
                    .get("step")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if let Some(step) = plan.steps.iter_mut().find(|s| s.id == id) {
                    step.status = "failed".into();
                    step.summary = event
                        .get("reason")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                }
            }
            _ => {}
        }
    }
    if plan.completed {
        plan.paused = false;
    }
}

/// 最近若干个 PlanRun 计划的状态（journal `plan/<plan_id>` 流的只读投影）。
///
/// 步骤状态由事件流推导：`step_finished{outcome}` / `step_skipped{reason}` 定终态，
/// plan_started 的步骤清单定全集，`plan_completed` 定整体收尾。没有 step_started
/// 事件，未定终态的步骤一律显示 pending（执行中的细节由任务面板的子代理列表承担）。
#[tauri::command]
pub async fn plan_status(state: State<'_, AppState>) -> Result<Vec<PlanDto>, String> {
    let session = state.session().ok_or("agent 尚未启动")?;
    run_blocking("读取计划状态", move || {
        let journal = session.journal();
        let streams = journal
            .streams(rclaw_store::JOURNAL_PLAN)
            .map_err(|e| e.to_string())?;
        let mut plans: Vec<PlanDto> = Vec::new();
        for stream in streams {
            let events = journal
                .load(rclaw_store::JOURNAL_PLAN, &stream)
                .map_err(|e| e.to_string())?;
            if events.is_empty() {
                continue;
            }
            let mut plan = PlanDto {
                plan_id: stream.clone(),
                name: stream.clone(),
                completed: false,
                paused: false,
                succeeded: None,
                updated_at: 0,
                steps: Vec::new(),
            };
            project_plan_events(&mut plan, &events);
            plans.push(plan);
        }
        plans.sort_by_key(|plan| std::cmp::Reverse(plan.updated_at));
        plans.truncate(6);
        Ok(plans)
    })
    .await
}

/// 暂停 / 继续一个 PlanRun 计划（ZCode 任务暂停/继续，第十二轮）：写 kv 暂停标志
/// + 追加 `plan_paused` / `plan_unpaused` 事件。执行器在【波次边界】协作式读取——
/// 当前在飞的步骤跑完、后续波次等待恢复；不影响取消语义（/cancel 照常终止）。
#[tauri::command]
pub async fn plan_pause(
    state: State<'_, AppState>,
    plan_id: String,
    paused: bool,
) -> Result<(), String> {
    let session = state.session().ok_or("agent 尚未启动")?;
    let key = format!("plan-pause/{plan_id}");
    run_blocking("计划暂停/继续", move || {
        let journal = session.journal();
        // kv 里流必须已存在（暂停一个不存在的计划没有意义，明确报错）。
        if !journal
            .exists(rclaw_store::JOURNAL_PLAN, &plan_id)
            .map_err(|e| e.to_string())?
        {
            return Err(format!("计划 '{plan_id}' 不存在（见任务面板 / 步骤卡片里的 plan_id）"));
        }
        let (event, write) = if paused {
            (
                "plan_paused",
                journal.kv_set(&key, &serde_json::json!({"at": chrono::Utc::now().timestamp_millis()})),
            )
        } else {
            // 恢复事件是 plan_unpaused——不是 plan_resumed：后者是 resume 的代际锚点
            // （open_escalations 据它判定提问归属），波次边界的恢复不换代，混用会把
            // 挂起前未消费的升级答案误判成「上一代已送达」。
            ("plan_unpaused", journal.kv_delete(&key))
        };
        write.map_err(|e| e.to_string())?;
        journal
            .append(
                rclaw_store::JOURNAL_PLAN,
                &plan_id,
                &serde_json::json!({
                    "ts": chrono::Utc::now().to_rfc3339(),
                    "event": event,
                    "plan_id": plan_id,
                    "source": "ui",
                }),
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    })
    .await
}

/// 历史消息条目（恢复转录用）。
#[derive(Debug, Serialize)]
pub struct HistoryMsgDto {
    /// 消息行 id（state.sqlite 的 message.id；消息下方「分叉到新会话」的锚点）。
    pub id: i64,
    /// user | assistant | status（工具调用轮）。
    pub role: String,
    pub content: String,
}

/// 读取指定会话的持久化历史（完整 transcript 里最近 200 条可回放行 / 4 MB），映射为转录条目。
///
/// 与 `chat_export` 同源读 transcript，而不是 provider 可见的压缩视图：压缩是非破坏性的，
/// 回放若按压缩视图渲染，压缩点处会无提示地缺一段中间对话（boundary 行只是摘要，还被
/// 过滤掉）。boundary 行与工具结果载体由 store 在 SQL 层筛掉、不占 200 条的额度（否则
/// 工具密集的会话首屏只剩几条对话）；下面的过滤只是兜底。
#[tauri::command]
pub async fn chat_history(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<Vec<HistoryMsgDto>, String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    // 只读，不改库（目标会话可能正有 turn 在追加：进程内共用一条连接、按互斥锁串行，
    // 这里只是排在写事务之后读到一致快照，不与之交错）。
    let msgs = run_blocking("读取会话历史", move || {
        session
            .load_transcript_tail(CHANNEL_DESKTOP, &chat_id, 200, 4 * 1024 * 1024)
            .map_err(|e| e.to_string())
    })
    .await?;
    let mut out = Vec::new();
    for msg in msgs {
        if msg.is_tool_result_carrier() {
            continue;
        }
        match msg.role {
            rclaw_core::Role::User => {
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                let t = text.trim();
                if t.is_empty() || t.starts_with("[CONTEXT COMPACTION") {
                    continue;
                }
                out.push(HistoryMsgDto {
                    id: msg.id,
                    role: "user".into(),
                    content: t.to_string(),
                });
            }
            rclaw_core::Role::Assistant => {
                let tools: Vec<&str> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        rclaw_core::ContentBlock::ToolUse { name, .. } => Some(name.as_str()),
                        _ => None,
                    })
                    .collect();
                if !tools.is_empty() {
                    out.push(HistoryMsgDto {
                        id: msg.id,
                        role: "status".into(),
                        content: format!("🔧 调用工具: {}", tools.join(", ")),
                    });
                }
                let text: String = msg
                    .content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .collect::<Vec<_>>()
                    .join("");
                if !text.trim().is_empty() {
                    out.push(HistoryMsgDto {
                        id: msg.id,
                        role: "assistant".into(),
                        content: text,
                    });
                }
            }
            _ => {}
        }
    }
    // 只保留最近 80 条，避免长会话首屏过载。
    if out.len() > 80 {
        out.drain(..out.len() - 80);
    }
    Ok(out)
}

/// 归档会话：打 `archived_at` 标记（恢复 = 清除标记）；消息、chat_id 都不动。
#[tauri::command]
pub async fn chat_archive(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    set_chat_archived(&state, chat_id, true).await
}

/// 恢复归档会话。
#[tauri::command]
pub async fn chat_restore(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    set_chat_archived(&state, chat_id, false).await
}

async fn set_chat_archived(state: &AppState, chat_id: String, archive: bool) -> Result<(), String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    run_blocking("更新会话归档状态", move || {
        set_session_archived(&session, &chat_id, archive)
    })
    .await
}

/// 归档 / 恢复的核心：只改会话标记，与 Tauri 状态解耦便于测试。
///
/// /clear 会在 UI 列表刷新前删掉会话；会话已不在 = 无事可做，视为成功，
/// 让前端刷新列表清掉幽灵条目，而不是报错把条目卡死。
fn set_session_archived(
    session: &rclaw_store::session::SessionStore,
    chat_id: &str,
    archive: bool,
) -> Result<(), String> {
    session
        .set_archived(CHANNEL_DESKTOP, chat_id, archive)
        .map(|_existed| ())
        .map_err(|e| format!("更新会话归档状态失败: {e}"))
}

/// 删除会话：删掉会话记录（消息、文件 checkpoint、用量锚点一并级联）并移除自定义标题，
/// 再清掉 agent loop 里该会话的运行期状态（内存 usage 锚点、压缩防抖 / 冷却 / 地板）——
/// 库与内存一起归零，不给同名会话留下旧历史形态的判断。
/// 正在工作的会话由前端拦截（turn 收尾还会向该会话追加）；会话已不在 = 幂等成功。
#[tauri::command]
pub async fn chat_delete(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    let lifecycle = state.session_lifecycle().ok_or("agent 尚未启动")?;
    let home = state.home.clone();
    run_blocking("删除会话", move || {
        // 正在跑的会话不删：turn 收尾会把消息 / 检查点写回刚清空的 chat，变成一个只有
        // 半截历史的「幽灵会话」。后端兜底，不依赖前端是否按运行态禁用了删除按钮。
        // 残余窗口：判定只看 active map，一条已入 inbound 队列、尚未 spawn 成 turn 的消息
        // 看不见，它随后仍会在被删会话上开 turn（新会话、只有本轮历史）。正常 UI 流程里
        // 发送与删除同一会话不会并发；这里不为它持锁跨越整段删除（锁也挡不住队列里的
        // 消息），接受这一窗口。
        if lifecycle.is_session_busy(CHANNEL_DESKTOP, &chat_id) {
            return Err("该会话还有正在进行的回复，请先 /cancel 或等它结束再删除".into());
        }
        delete_session_record(&session, &chat_id)?;
        lifecycle.forget_session(CHANNEL_DESKTOP, &chat_id);
        // 自定义标题随会话一起清掉（空标题 = 删除该键）。
        agent::write_chat_title(&home, &chat_id, "")
    })
    .await
}

/// 删除的核心：只做会话记录删除（活动 / 归档同一张表），与 Tauri 状态解耦便于测试。
fn delete_session_record(
    session: &rclaw_store::session::SessionStore,
    chat_id: &str,
) -> Result<(), String> {
    session
        .delete_session(CHANNEL_DESKTOP, chat_id)
        .map(|_existed| ())
        .map_err(|e| format!("删除会话失败: {e}"))
}

// ---------------------------------------------------------------------------
// 多 agent：后台执行快照
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn agents_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let runtime = state.agent_runtime().ok_or("agent 尚未启动")?;
    Ok(runtime.executions_payload().await)
}

// ---------------------------------------------------------------------------
// 状态面板：上下文窗口占用 + 进程内存
// ---------------------------------------------------------------------------

/// 上下文窗口与内存占用快照（右侧任务面板底部「状态」区轮询）。
#[derive(Debug, Serialize)]
pub struct UsageDto {
    /// 会话历史 token 估算（不含 system prompt / 工具 schema）。
    pub history_used: u64,
    /// 请求压力估算（历史 + system prompt + 动态上下文 + 静态工具面）。与自动压缩触发用
    /// 同一个估算器与同一份工具面，但是纯启发式——没有 turn 内记下的真实 usage 锚点
    /// （见 `rclaw_runtime::ContextUsageSnapshot::request_used`），长会话上可能与 `/context`
    /// 的锚定值有出入。
    pub request_used: u64,
    /// 上下文窗口（token）。
    pub context_length: u64,
    /// 进程常驻内存（字节；无法获取的平台为 0）。
    pub rss_bytes: u64,
}

#[tauri::command]
pub async fn usage_snapshot(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<UsageDto, String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let tools = state.tools().ok_or("agent 尚未启动")?;
    let (memory, opts, channels, web_search) = state.usage_inputs().ok_or("agent 尚未启动")?;
    // 估算会扫技能目录 / 读记忆文件（与 insights 同策略放阻塞线程，不卡 UI 桥）。
    let usage = tauri::async_runtime::spawn_blocking(move || {
        rclaw_runtime::estimate_context_usage(
            &session,
            &memory,
            &layout,
            &tools,
            &opts,
            &channels,
            web_search.as_deref(),
            CHANNEL_DESKTOP,
            &chat_id,
        )
    })
    .await
    .map_err(|e| format!("上下文估算任务失败: {e}"))??;
    Ok(UsageDto {
        history_used: usage.history_used,
        request_used: usage.request_used,
        context_length: usage.context_length,
        rss_bytes: process_rss_bytes(),
    })
}

/// 当前进程常驻内存。Windows 用 GetProcessMemoryInfo；Linux 读 /proc/self/statm；
/// 其余平台返回 0（面板显示为不可用）。
pub(crate) fn process_rss_bytes() -> u64 {
    #[cfg(windows)]
    {
        // SAFETY: 全部为无副作用的进程信息查询；GetCurrentProcess 返回伪句柄，无需关闭。
        unsafe {
            use windows_sys::Win32::System::ProcessStatus::{
                GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
            };
            use windows_sys::Win32::System::Threading::GetCurrentProcess;
            let mut counters = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                PageFaultCount: 0,
                PeakWorkingSetSize: 0,
                WorkingSetSize: 0,
                QuotaPeakPagedPoolUsage: 0,
                QuotaPagedPoolUsage: 0,
                QuotaPeakNonPagedPoolUsage: 0,
                QuotaNonPagedPoolUsage: 0,
                PagefileUsage: 0,
                PeakPagefileUsage: 0,
            };
            let ok = GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            );
            if ok != 0 {
                counters.WorkingSetSize as u64
            } else {
                0
            }
        }
    }
    // Android 同属 Linux 内核：/proc/self/statm 同样可读（此前缺 android cfg
    // 导致移动端状态块内存恒为 0/「—」）。
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // statm 字段：size resident shared …（页为单位）；取第 2 个 resident。
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|text| {
                text.split_whitespace()
                    .nth(1)
                    .and_then(|pages| pages.parse::<u64>().ok())
            })
            .map(|pages| pages * 4096)
            .unwrap_or(0)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "android")))]
    {
        0
    }
}

// ---------------------------------------------------------------------------
// 工具面板：列表 + 使能/禁能（禁用立即拦截执行并移出下一 turn 的工具面）
// ---------------------------------------------------------------------------

/// 工具条目。
#[derive(Debug, Serialize)]
pub struct ToolDto {
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

#[tauri::command]
pub async fn tools_list(state: State<'_, AppState>) -> Result<Vec<ToolDto>, String> {
    let tools = state.tools().ok_or("agent 尚未启动")?;
    Ok(tools
        .schemas()
        .iter()
        .map(|s| ToolDto {
            name: s.name.clone(),
            description: s.description.clone(),
            enabled: tools.is_enabled(&s.name),
        })
        .collect())
}

#[tauri::command]
pub async fn set_tool_enabled(
    state: State<'_, AppState>,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    let tools = state.tools().ok_or("agent 尚未启动")?;
    if !tools.set_enabled(&name, enabled) {
        return Err(format!("未知工具: {name}"));
    }
    // 持久化偏好（跨重启保留）。以注册表当前状态为准，避免并发开关互相覆盖。
    let disabled: Vec<String> = tools
        .schemas()
        .iter()
        .filter(|s| !tools.is_enabled(&s.name))
        .map(|s| s.name.clone())
        .collect();
    agent::write_disabled_tools(&state.home, &disabled)
}

// ---------------------------------------------------------------------------
// 技能面板：列表（含归档态）+ 禁能/启用（复用 skill_manage 的 archive/restore）
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn skills_list(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let tools = state.tools().ok_or("agent 尚未启动")?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let result = tools.execute("skills_list", serde_json::json!({})).await;
    if result.is_error {
        return Err(result.content);
    }
    let mut value: serde_json::Value =
        serde_json::from_str(&result.content).map_err(|e| format!("解析技能清单失败: {e}"))?;
    merge_archived_skills(&mut value, &layout.skills_dir);
    Ok(value)
}

/// 把 lifecycle 元数据里 state=Archived 的技能合并进 `{"skills":[...]}` 清单。
///
/// 工具面只扫 `skills/` 根（归档即移出扫描范围），不合并的话面板上看不到归档技能、
/// 永远无法再启用（restore 走 lifecycle 的 archive_path 元数据，无需目录在位）。
fn merge_archived_skills(value: &mut serde_json::Value, skills_dir: &std::path::Path) {
    let usage = rclaw_tools::SkillUsageStore::new(skills_dir.to_path_buf());
    let Ok(records) = usage.records() else {
        return;
    };
    let Some(arr) = value.get_mut("skills").and_then(|s| s.as_array_mut()) else {
        return;
    };
    for (name, record) in records {
        if record.state != rclaw_tools::SkillState::Archived {
            continue;
        }
        if arr
            .iter()
            .any(|s| s.get("name").and_then(|n| n.as_str()) == Some(&name))
        {
            continue;
        }
        arr.push(serde_json::json!({
            "name": name,
            "title": name,
            "description": "（已归档，启用后恢复）",
            "path": record.archive_path,
            "owner": record.owner.map(|o| o.to_string()).unwrap_or_else(|| "user".into()),
            "state": record.state.to_string(),
            "pinned": record.pinned,
        }));
    }
}

#[tauri::command]
pub async fn skill_set_enabled(
    state: State<'_, AppState>,
    name: String,
    enabled: bool,
) -> Result<(), String> {
    let tools = state.tools().ok_or("agent 尚未启动")?;
    let input = serde_json::json!({
        "action": if enabled { "restore" } else { "archive" },
        "name": name,
    });
    let result = tools.execute("skill_manage", input).await;
    if result.is_error {
        return Err(result.content);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 自进化面板：开关状态 + 记忆文件 + 技能统计
// ---------------------------------------------------------------------------

/// 自进化信息。
#[derive(Debug, Serialize)]
pub struct EvolutionInfo {
    pub auto_memory: bool,
    pub auto_evolve: bool,
    pub skill_curator: bool,
    pub session_compression: bool,
    /// MEMORY.md 正文（不存在为 null）。
    pub memory_md: Option<String>,
    /// USER.md 正文（不存在为 null）。
    pub user_md: Option<String>,
    /// 技能库路径。
    pub skills_dir: String,
}

#[tauri::command]
pub async fn evolution_info(state: State<'_, AppState>) -> Result<EvolutionInfo, String> {
    // 开关来自 rclaw.toml（向导显式写值）；记忆文件来自存储布局。
    let cfg = rclaw_cli::config::Config::load_from_dir(&state.home)
        .map_err(|e| format!("加载配置失败: {e}"))?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let read_opt = |path: &std::path::Path| -> Option<String> {
        std::fs::read_to_string(path)
            .ok()
            .map(|text| text.chars().take(24_000).collect())
    };
    Ok(EvolutionInfo {
        auto_memory: cfg.agent.auto_memory,
        auto_evolve: cfg.agent.auto_evolve,
        skill_curator: cfg.agent.skill_curator,
        session_compression: cfg.agent.session_compression,
        memory_md: read_opt(&layout.memory_file),
        user_md: read_opt(&layout.user_file),
        skills_dir: layout.skills_dir.to_string_lossy().replace('\\', "/"),
    })
}

// ---------------------------------------------------------------------------
// B6 学习洞察：会话 / 工具 / 技能使用聚合 + 最近技能变更（自进化面板卡片）
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn insights(state: State<'_, AppState>) -> Result<crate::insights::InsightsDto, String> {
    let session = state.session().ok_or("agent 尚未启动")?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    // 记忆占用上限必须与 bootstrap 的正式实例同源（rclaw.toml [agent] 限额），
    // 否则用户改过限额后面板百分比与模型侧 [memory usage] 头不一致。
    let cfg = rclaw_cli::config::Config::load_from_dir(&state.home)
        .map_err(|e| format!("加载配置失败: {e}"))?;
    let memory = rclaw_store::MemoryStore::new(layout.clone())
        .with_char_limits(cfg.agent.memory_char_limit, cfg.agent.user_char_limit);
    // 全库消息扫描放到阻塞线程池，不卡 async 运行时。
    tauri::async_runtime::spawn_blocking(move || {
        crate::insights::compute(&session, &layout.skills_dir, &memory)
    })
    .await
    .map_err(|e| format!("insights 计算失败: {e}"))
}

// ---------------------------------------------------------------------------
// 配置面板：rclaw.toml 全量项的可视化编辑（文档来自 rclaw.toml.example）
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct ConfigStateDto {
    /// rclaw.toml 绝对路径。
    pub path: String,
    /// 当前文件原文（只读展示 / 高级编辑参考）。
    pub raw: String,
    /// 按示例解析的段/键文档模型（含每键当前值）。
    pub doc: rclaw_store::config_doc::ConfigDoc,
}

#[derive(Debug, serde::Deserialize)]
pub struct ConfigEdit {
    /// 点分路径（`agent.max_concurrent_turns` / `llm.providers.zhipu.model`）。
    pub path: String,
    /// bool / int / float / str / str_array。
    pub kind: String,
    /// UI 输入的字符串值（按 kind 解析；str_array 为逗号分隔）。
    pub value: String,
}

pub fn config_state_for(home: &std::path::Path) -> Result<ConfigStateDto, String> {
    let path = home.join("rclaw.toml");
    let raw = std::fs::read_to_string(&path).unwrap_or_default();
    let mut doc = rclaw_store::config_doc::parse_example(rclaw_store::config_doc::EXAMPLE);
    let user: toml_edit::DocumentMut = raw
        .parse()
        .map_err(|e| format!("rclaw.toml 解析失败: {e}"))?;
    for section in &mut doc.sections {
        for item in &mut section.items {
            item.current = rclaw_store::config_doc::resolve_current_display(&user, &item.path);
        }
    }
    fill_effective_llm_model_default(&user, &mut doc);
    Ok(ConfigStateDto {
        path: path.display().to_string(),
        raw,
        doc,
    })
}

/// `[llm].model` 未显式配置时，生效模型 = `[llm].provider` 指向条目的 `model`
/// （Config 的取值顺序：llm.model > provider.model > 内置兜底）。把这个真实生效值
/// 回填为该项的「默认」展示——仍保持「未配置」状态——避免显示示例占位模型
/// （glm-5.3），让用户误以为生效的不是 provider 里的模型。
fn fill_effective_llm_model_default(
    user: &toml_edit::DocumentMut,
    doc: &mut rclaw_store::config_doc::ConfigDoc,
) {
    let Some(provider) = rclaw_store::config_doc::resolve_current_display(user, "llm.provider")
    else {
        return;
    };
    let Some(model) = rclaw_store::config_doc::resolve_current_display(
        user,
        &format!("llm.providers.{provider}.model"),
    ) else {
        return;
    };
    for section in &mut doc.sections {
        for item in &mut section.items {
            if item.path == "llm.model" && item.current.is_none() {
                item.default_raw = format!("\"{model}\"");
            }
        }
    }
}

/// 应用一组面板编辑：注释/键序保真的 toml_edit 手术 → 结构校验 → 原子落盘。
/// 校验失败绝不落盘（改坏配置的尝试在保存前被拦下）。
pub fn config_save_edits(home: &std::path::Path, edits: &[ConfigEdit]) -> Result<(), String> {
    if edits.is_empty() {
        return Ok(());
    }
    let toml_file = rclaw_store::RclawToml::new(home.join("rclaw.toml"));
    let mut parsed = Vec::with_capacity(edits.len());
    for e in edits {
        parsed.push((e.path.clone(), parse_edit_value(&e.kind, &e.value)?));
    }
    let preview = toml_file.apply_edits_preview(&parsed)?;
    rclaw_cli::config::Config::parse_str(&preview)
        .map_err(|e| format!("保存被校验拒绝（未写入文件）: {e}"))?;
    validate_provider_names(&preview, parsed.iter().map(|(p, _)| p.as_str()))?;
    toml_file.write_text(&preview)
}

/// `llm.provider` / `llm.review_provider` 指向不存在的 `[[llm.providers]]` 名会让
/// agent 重启即失败——本批改过这两个键、或改过任何 provider 条目的 `name`（改名
/// 同样可能断开引用）时，对预览文本做全量名字一致性校验（不追咎既有配置的历史
/// 问题；`review_provider` 空串 = 未配置，放行）。
fn validate_provider_names<'a>(
    preview: &str,
    edited_paths: impl Iterator<Item = &'a str>,
) -> Result<(), String> {
    let edited: std::collections::HashSet<&str> = edited_paths.collect();
    let touches_names = edited
        .iter()
        .any(|p| p.starts_with("llm.providers.") && p.ends_with(".name"));
    if !touches_names && !edited.contains("llm.provider") && !edited.contains("llm.review_provider")
    {
        return Ok(());
    }
    let doc: toml_edit::DocumentMut = preview.parse().map_err(|e| format!("预览解析失败: {e}"))?;
    let names: Vec<&str> = doc
        .get("llm")
        .and_then(|llm| llm.get("providers"))
        .and_then(|p| p.as_array_of_tables())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let llm_table = doc.get("llm").and_then(|llm| llm.as_table());
    for (path, key, allow_empty) in [
        ("llm.provider", "provider", false),
        ("llm.review_provider", "review_provider", true),
    ] {
        let Some(value) = llm_table.and_then(|t| t.get(key)).and_then(|i| i.as_str()) else {
            continue;
        };
        if (allow_empty && value.is_empty()) || names.contains(&value) {
            continue;
        }
        return Err(format!(
            "保存被拒绝（未写入文件）：{path} = \"{value}\" 不在 [[llm.providers]] 的 name 列表（{names:?}）里，保存后 agent 将无法启动"
        ));
    }
    Ok(())
}

fn parse_edit_value(kind: &str, value: &str) -> Result<rclaw_store::EditValue, String> {
    let value = value.trim();
    match kind {
        "bool" => value
            .parse::<bool>()
            .map(rclaw_store::EditValue::Bool)
            .map_err(|_| format!("布尔值只接受 true/false: {value}")),
        "int" => value
            .parse::<i64>()
            .map(rclaw_store::EditValue::Int)
            .map_err(|_| format!("整数解析失败: {value}")),
        "float" => value
            .parse::<f64>()
            .map(rclaw_store::EditValue::Float)
            .map_err(|_| format!("数值解析失败: {value}")),
        "str" => Ok(rclaw_store::EditValue::Str(value.to_string())),
        "str_array" => Ok(rclaw_store::EditValue::StrArray(
            value
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        )),
        other => Err(format!("未知配置值类型: {other}")),
    }
}

#[tauri::command]
pub async fn config_state(state: State<'_, AppState>) -> Result<ConfigStateDto, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || config_state_for(&home))
        .await
        .map_err(|e| format!("读取配置失败: {e}"))?
}

#[tauri::command]
pub async fn config_save(state: State<'_, AppState>, edits: Vec<ConfigEdit>) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || config_save_edits(&home, &edits))
        .await
        .map_err(|e| format!("保存配置失败: {e}"))?
}

/// 导入整份 rclaw.toml（GUI「从文件导入」）：解析 + Config 校验 + provider 引用
/// 一致性检查 → 原子替换。校验不过不落盘，现有配置保持不动。
pub fn config_import_raw_for(home: &std::path::Path, content: &str) -> Result<(), String> {
    if content.trim().is_empty() {
        return Err("导入内容为空".into());
    }
    content
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("rclaw.toml 解析失败: {e}"))?;
    rclaw_cli::config::Config::parse_str(content)
        .map_err(|e| format!("导入被校验拒绝（未写入文件）: {e}"))?;
    validate_provider_names(
        content,
        ["llm.provider", "llm.review_provider"].into_iter(),
    )?;
    rclaw_store::RclawToml::new(home.join("rclaw.toml")).write_text(content)
}

#[tauri::command]
pub async fn config_import_raw(
    app: AppHandle,
    state: State<'_, AppState>,
    content: String,
) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || config_import_raw_for(&home, &content))
        .await
        .map_err(|e| format!("导入配置失败: {e}"))??;
    // 导入即完成配置落盘：与 wizard_save 同路径启动 agent。此前只写文件不
    // 启动——App 启动时无配置则 agent 不会被拉起，导入后 UI 永久卡「正在启动」。
    agent::start_agent(app, state.home.clone()).await
}

// ---------------------------------------------------------------------------
// 标题栏循环标语：界面偏好（agent/desktop_prefs.json），不写入 rclaw.toml
// ---------------------------------------------------------------------------

/// 标语轮播偏好（标语 + 轮播周期秒数）。
#[derive(Debug, serde::Serialize)]
pub struct SloganPrefsDto {
    pub slogans: Vec<String>,
    pub interval_secs: u64,
}

#[tauri::command]
pub async fn slogans_get(state: State<'_, AppState>) -> Result<SloganPrefsDto, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let prefs = agent::read_slogan_prefs(&home);
        SloganPrefsDto {
            slogans: prefs.slogans,
            interval_secs: prefs.interval_secs,
        }
    })
    .await
    .map_err(|e| format!("读取标语失败: {e}"))
}

#[tauri::command]
pub async fn slogans_set(
    state: State<'_, AppState>,
    slogans: Vec<String>,
    interval_secs: u64,
) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        agent::write_slogan_prefs(&home, &slogans, interval_secs)
    })
    .await
    .map_err(|e| format!("保存标语失败: {e}"))?
}

/// 把窗口标题设为当前轮播标语（任务栏 / Alt+Tab 处显示；窗口内顶部是自绘
/// 标题栏，由前端用同一文案渲染）。空标语由前端回退应用名。
#[tauri::command]
pub fn set_window_title(app: AppHandle, title: String) -> Result<(), String> {
    app.get_webview_window("main")
        .ok_or_else(|| "主窗口不存在".to_string())?
        .set_title(&title)
        .map_err(|e| format!("设置标题失败: {e}"))
}

// ---------------------------------------------------------------------------
// 窗口控制与界面主题：自绘标题栏（decorations: false）配套——最小化 / 最大化
// 切换 / 关闭（隐藏到托盘）/ 拖拽移动 / 最大化状态查询；主题为界面偏好
// （agent/desktop_prefs.json），不写入 rclaw.toml。
// ---------------------------------------------------------------------------

/// 取主窗口句柄；缺失时报错文案。
fn main_window(app: &AppHandle) -> Result<tauri::WebviewWindow, String> {
    app.get_webview_window("main")
        .ok_or_else(|| "主窗口不存在".to_string())
}

#[tauri::command]
pub fn window_minimize(app: AppHandle) -> Result<(), String> {
    // Android：无最小化语义（固定全屏），协议保持成功。
    #[cfg(target_os = "android")]
    {
        let _ = &app;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        main_window(&app)?
            .minimize()
            .map_err(|e| format!("最小化失败: {e}"))
    }
}

/// 切换最大化 / 还原，返回切换后的最大化状态（标题栏按钮图标据此切换）。
#[tauri::command]
pub fn window_toggle_maximize(app: AppHandle) -> Result<bool, String> {
    // Android：固定全屏，恒为最大化状态。
    #[cfg(target_os = "android")]
    {
        let _ = &app;
        Ok(false)
    }
    #[cfg(not(target_os = "android"))]
    {
        let window = main_window(&app)?;
        if window.is_maximized().map_err(|e| e.to_string())? {
            window.unmaximize().map_err(|e| format!("还原失败: {e}"))?;
            Ok(false)
        } else {
            window.maximize().map_err(|e| format!("最大化失败: {e}"))?;
            Ok(true)
        }
    }
}

/// 当前最大化状态（前端在启动与窗口 resize 后查询）。
#[tauri::command]
pub fn window_maximized(app: AppHandle) -> Result<bool, String> {
    // Android：固定全屏。
    #[cfg(target_os = "android")]
    {
        let _ = &app;
        Ok(true)
    }
    #[cfg(not(target_os = "android"))]
    {
        main_window(&app)?.is_maximized().map_err(|e| e.to_string())
    }
}

/// 关闭窗口：走 CloseRequested 管道 = 隐藏到托盘，保持常驻。
#[tauri::command]
pub fn window_close(app: AppHandle) -> Result<(), String> {
    main_window(&app)?
        .close()
        .map_err(|e| format!("关闭失败: {e}"))
}

/// 退出应用（安卓返回键在侧边栏上再按一次时的「退出」语义）：结束 GUI 进程。
/// 注意前台服务是 START_STICKY 且 should_run 仍为 true，系统稍后会以无头方式
/// 拉起常驻运行时（Watchdog/should_run 的设计本意——GUI 退出不等于停 agent）；
/// 要连运行时一起停需先走 ACTION_STOP 清 should_run（目前无 GUI 入口）。
#[tauri::command]
pub fn app_exit(app: AppHandle) -> Result<(), String> {
    #[cfg(not(target_os = "android"))]
    {
        app.cleanup_before_exit();
        app.exit(0);
        Ok(())
    }
    #[cfg(target_os = "android")]
    {
        let _ = &app;
        // cleanup_before_exit 在安卓真机会触发原生崩溃（Skipping native
        // crash dialog），进程级直接退出。exit 返回 !，按 Result 收尾。
        std::process::exit(0)
    }
}

/// 标题栏拖拽移动（在 mousedown 里调用，进入系统原生移动循环，贴边分屏照常生效）。
#[tauri::command]
pub fn window_start_drag(app: AppHandle) -> Result<(), String> {
    // Android：系统级全屏窗口，无自由拖拽。
    #[cfg(target_os = "android")]
    {
        let _ = &app;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        main_window(&app)?
            .start_dragging()
            .map_err(|e| format!("拖拽失败: {e}"))
    }
}

#[tauri::command]
pub async fn theme_get(state: State<'_, AppState>) -> Result<String, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || agent::read_theme(&home))
        .await
        .map_err(|e| format!("读取主题失败: {e}"))
}

#[tauri::command]
pub async fn theme_set(state: State<'_, AppState>, theme: String) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || agent::write_theme(&home, &theme))
        .await
        .map_err(|e| format!("保存主题失败: {e}"))?
}

// ---------------------------------------------------------------------------
// 工具权限：模式查询/切换 + 工作区外访问的弹窗应答
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn permission_mode_get(state: State<'_, AppState>) -> Result<String, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || agent::read_permission_mode(&home))
        .await
        .map_err(|e| format!("读取权限模式失败: {e}"))
}

#[tauri::command]
pub async fn permission_mode_set(state: State<'_, AppState>, mode: String) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || agent::write_permission_mode(&home, &mode))
        .await
        .map_err(|e| format!("保存权限模式失败: {e}"))?
}

/// 回填一次工作区外访问的授权结果（弹窗的允许/拒绝按钮调用）。
#[tauri::command]
pub fn permission_respond(id: String, allowed: bool) -> Result<(), String> {
    crate::permissions::respond(&id, allowed)
}

// ---------------------------------------------------------------------------
// 文件上传：webview 选择的文件落入 agent 工作区（agent/uploads/）
// ---------------------------------------------------------------------------

/// 单个上传文件的最大字节数（base64 解码后）。覆盖常见文档 / 图片 / 音视频，
/// 同时拦截误选的超大文件把内存打爆。
const UPLOAD_MAX_BYTES: usize = 50 * 1024 * 1024;

/// 上传结果：工作区相对路径（供对话里引用）与绝对路径（前端 title 展示）。
#[derive(Debug, Serialize)]
pub struct UploadDto {
    pub rel_path: String,
    pub abs_path: String,
    pub size: u64,
}

/// 把前端（`<input type="file">` 读取的 base64）上传的文件写入 agent 工作区的
/// `uploads/` 子目录：文件名取 basename 并清洗非法字符，加时间戳 + 随机后缀防
/// 覆盖。返回的相对路径相对 agent 存储根，模型在工作区内可直接 read。
#[tauri::command]
pub async fn upload_file(
    state: State<'_, AppState>,
    name: String,
    data_base64: String,
) -> Result<UploadDto, String> {
    use base64::Engine;
    // 解码前先按 base64 长度预检：装不下目标字节数的 payload 直接拒绝，
    // 避免超大请求白白解码占内存（base64 膨胀率 4/3，留 4 字符 padding 余量）。
    if data_base64.len() > UPLOAD_MAX_BYTES / 3 * 4 + 4 {
        return Err(format!("文件过大（超过 {UPLOAD_MAX_BYTES} 字节上限）"));
    }
    let home = state.home.clone();
    // 解码 + 落盘都是纯阻塞操作，整体放阻塞线程池，不卡 async 运行时。
    tauri::async_runtime::spawn_blocking(move || {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_base64.as_bytes())
            .map_err(|e| format!("上传数据解码失败: {e}"))?;
        if bytes.len() > UPLOAD_MAX_BYTES {
            return Err(format!(
                "文件过大（{} 字节 > 上限 {UPLOAD_MAX_BYTES}）",
                bytes.len()
            ));
        }
        // 文件名清洗：只保留 basename；ASCII 里的路径分隔符 / Windows 保留字符
        // 替换为下划线，非 ASCII（中文等）原样保留。
        let raw_name = std::path::Path::new(&name)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let safe: String = raw_name
            .chars()
            .map(|c| {
                if c.is_ascii() && !(c.is_ascii_alphanumeric() || "._- ()".contains(c)) {
                    '_'
                } else {
                    c
                }
            })
            .collect();
        let safe = safe.trim();
        let safe = if safe.is_empty() { "file" } else { safe };
        let dir = home.join("agent").join("uploads");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建上传目录失败: {e}"))?;
        // 文件名可预测（时间戳 + 随机后缀），工作区里能事先摆一条同名的悬空链接指向外部：
        // 用不跟随链接的 CREATE_NEW 打开，撞上已有条目就换个序号重试，绝不顺着链接写出去。
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
        let nonce = rand::random::<u16>();
        let path = write_fresh_file(
            &dir,
            "写入上传文件失败",
            |n| {
                if n == 1 {
                    format!("{stamp}-{nonce:04x}_{safe}")
                } else {
                    format!("{stamp}-{nonce:04x}-{n}_{safe}")
                }
            },
            |file| std::io::Write::write_all(file, &bytes),
        )?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(UploadDto {
            rel_path: format!("uploads/{file_name}"),
            abs_path: path.to_string_lossy().replace('\\', "/"),
            size: bytes.len() as u64,
        })
    })
    .await
    .map_err(|e| format!("上传任务失败: {e}"))?
}

// ---------------------------------------------------------------------------
// cron（原有）
// ---------------------------------------------------------------------------

/// cron 任务条目（列表展示用）。
#[derive(Debug, Serialize)]
pub struct CronEntryDto {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// 调度类型（snake_case，如 every / daily / weekly）。
    pub kind: String,
    /// 人读的调度描述。
    pub schedule: String,
    /// 触发时注入的消息正文。
    pub message: String,
}

#[tauri::command]
pub async fn cron_list(state: State<'_, AppState>) -> Result<Vec<CronEntryDto>, String> {
    let cron = state.cron().ok_or("agent 尚未启动")?;
    Ok(cron
        .list_snapshot()
        .into_iter()
        .map(|job| {
            let (kind, schedule) = describe_cron(&job);
            CronEntryDto {
                id: job.id,
                name: job.name,
                enabled: job.enabled,
                kind,
                schedule,
                message: job.message,
            }
        })
        .collect())
}

#[tauri::command]
pub async fn cron_remove(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    let cron = state.cron().ok_or("agent 尚未启动")?;
    cron.remove(&id).map_err(|e| e.to_string())
}

/// 把 CronJob 的调度字段渲染成（类型名, 人读描述）。
fn describe_cron(job: &rclaw_store::CronJob) -> (String, String) {
    use rclaw_store::CronKind;
    let clock = || job.at_clock.clone().unwrap_or_default();
    let weekday_name = |w: Option<u8>| {
        ["一", "二", "三", "四", "五", "六", "日"][w.unwrap_or(1).clamp(1, 7) as usize - 1]
    };
    match job.kind {
        CronKind::Every => (
            "every".into(),
            format!("每 {} 秒", job.interval_s.unwrap_or(0)),
        ),
        CronKind::At => (
            "at".into(),
            format!("一次性（unix {}）", job.at_epoch.unwrap_or(0)),
        ),
        CronKind::Daily => ("daily".into(), format!("每天 {}", clock())),
        CronKind::Weekly => (
            "weekly".into(),
            format!("每周{} {}", weekday_name(job.weekday), clock()),
        ),
        CronKind::Monthly => (
            "monthly".into(),
            format!("每月 {} 日 {}", job.day_of_month.unwrap_or(1), clock()),
        ),
        CronKind::LunarMonthly => (
            "lunar_monthly".into(),
            format!("农历每月 {} 日 {}", job.day_of_month.unwrap_or(1), clock()),
        ),
        CronKind::Yearly => (
            "yearly".into(),
            format!(
                "每年 {} 月 {} 日 {}",
                job.month.unwrap_or(1),
                job.day.unwrap_or(1),
                clock()
            ),
        ),
        CronKind::LunarYearly => (
            "lunar_yearly".into(),
            format!(
                "农历每年 {} 月 {} 日 {}",
                job.month.unwrap_or(1),
                job.day.unwrap_or(1),
                clock()
            ),
        ),
        CronKind::YearlyRange => (
            "yearly_range".into(),
            format!(
                "每年 {} 月 {} 日至 {} 月 {} 日",
                job.month.unwrap_or(1),
                job.day.unwrap_or(1),
                job.end_month.unwrap_or(1),
                job.end_day.unwrap_or(1)
            ),
        ),
        CronKind::LunarYearlyRange => (
            "lunar_yearly_range".into(),
            format!(
                "农历每年 {} 月 {} 日至 {} 月 {} 日",
                job.month.unwrap_or(1),
                job.day.unwrap_or(1),
                job.end_month.unwrap_or(1),
                job.end_day.unwrap_or(1)
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::merge_archived_skills;
    use rclaw_tools::{SkillOwner, SkillUsageStore};

    fn test_session(dir: &std::path::Path) -> rclaw_store::session::SessionStore {
        rclaw_store::session::SessionStore::new(rclaw_store::fs::Layout::new(dir.join("agent")))
    }

    fn archived_flag(session: &rclaw_store::session::SessionStore, chat_id: &str) -> Option<bool> {
        session
            .session_summary(super::CHANNEL_DESKTOP, chat_id)
            .unwrap()
            .map(|summary| summary.archived)
    }

    #[test]
    fn set_session_archived_tolerates_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        // /clear 删除会话后 UI 仍有幽灵条目：归档/恢复都应幂等成功，
        // 让前端刷新列表清掉条目，而不是报「会话不存在」。
        assert!(super::set_session_archived(&session, "main", true).is_ok());
        assert!(super::set_session_archived(&session, "main", false).is_ok());
        assert_eq!(archived_flag(&session, "main"), None, "不应凭空创建会话");
    }

    #[test]
    fn set_session_archived_flags_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        session
            .append(
                super::CHANNEL_DESKTOP,
                "main",
                &rclaw_core::Message::plain(rclaw_core::Role::User, "hello"),
            )
            .unwrap();

        super::set_session_archived(&session, "main", true).unwrap();
        assert_eq!(archived_flag(&session, "main"), Some(true));
        // 再归档同一会话 → 幂等成功。
        assert!(super::set_session_archived(&session, "main", true).is_ok());
        assert_eq!(archived_flag(&session, "main"), Some(true));

        super::set_session_archived(&session, "main", false).unwrap();
        assert_eq!(archived_flag(&session, "main"), Some(false));
        // 归档不动消息。
        assert_eq!(
            session
                .load_all(super::CHANNEL_DESKTOP, "main")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn delete_session_record_removes_active_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        for chat_id in ["main", "old"] {
            session
                .append(
                    super::CHANNEL_DESKTOP,
                    chat_id,
                    &rclaw_core::Message::plain(rclaw_core::Role::User, "hello"),
                )
                .unwrap();
        }
        session
            .set_archived(super::CHANNEL_DESKTOP, "old", true)
            .unwrap();

        // 活动与归档会话都能删；再删幂等成功。
        assert!(super::delete_session_record(&session, "main").is_ok());
        assert!(super::delete_session_record(&session, "old").is_ok());
        assert_eq!(archived_flag(&session, "main"), None);
        assert_eq!(archived_flag(&session, "old"), None);
        assert!(super::delete_session_record(&session, "main").is_ok());
    }

    #[test]
    fn extract_title_skips_compaction_summary_and_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path());
        assert_eq!(super::extract_title(&session, "none"), "");
        let long = "标题".repeat(50);
        for text in [
            "[CONTEXT COMPACTION — REFERENCE ONLY] summary",
            long.as_str(),
        ] {
            session
                .append(
                    super::CHANNEL_DESKTOP,
                    "main",
                    &rclaw_core::Message::plain(rclaw_core::Role::User, text),
                )
                .unwrap();
        }
        let title = super::extract_title(&session, "main");
        assert_eq!(title.chars().count(), 60);
        assert!(title.starts_with("标题"));
    }

    #[test]
    fn resolve_workspace_entry_accepts_real_and_rejects_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent");
        std::fs::create_dir_all(root.join("media")).unwrap();
        std::fs::write(root.join("media/a.txt"), b"x").unwrap();
        // 命中真实文件：canonical 与 base 同源（copy/delete/rename 共用此校验）。
        let (base, canon) = super::resolve_workspace_entry(&root, "media/a.txt").unwrap();
        assert!(canon.starts_with(&base));
        // 叶子就是条目本身（父目录 canonical + 原名），供 delete/rename 直接操作。
        assert_eq!(canon, base.join("media").join("a.txt"));
        // 穿越 / 反斜杠 / 绝对路径 / 空路径 / `.` / 空分量 / 不存在一律拒绝。
        for bad in ["../a.txt", "a\\b", "/etc/passwd", "", ".", "media/", "media//a.txt", "nope.txt"] {
            assert!(super::resolve_workspace_entry(&root, bad).is_err(), "{bad:?}");
        }
        #[cfg(windows)]
        for bad in ["C:evil", "media/a.txt:ads", "media/a.txt.", "media /a.txt"] {
            assert!(super::resolve_workspace_entry(&root, bad).is_err(), "{bad:?}");
        }
        #[cfg(unix)]
        {
            // 叶子链接不跟随：返回的是链接自身，删除 / 重命名作用在链接上。
            std::os::unix::fs::symlink(root.join("media/a.txt"), root.join("media/link")).unwrap();
            let (base, entry) = super::resolve_workspace_entry(&root, "media/link").unwrap();
            assert_eq!(entry, base.join("media").join("link"));
            assert!(std::fs::symlink_metadata(&entry).unwrap().file_type().is_symlink());
            // 复制要读内容：跟随后仍在工作区内的普通文件才放行。
            assert_eq!(
                super::resolve_workspace_file_content(&base, &entry).unwrap(),
                base.join("media").join("a.txt")
            );
            // 指向工作区外 / 指向运行时数据库的链接：条目可删可改名，但内容不可复制。
            std::os::unix::fs::symlink("/etc/passwd", root.join("media/escape")).unwrap();
            let (base, entry) = super::resolve_workspace_entry(&root, "media/escape").unwrap();
            assert!(super::resolve_workspace_file_content(&base, &entry).is_err());
            std::fs::write(root.join("state.sqlite"), b"db").unwrap();
            std::os::unix::fs::symlink(root.join("state.sqlite"), root.join("media/dblink")).unwrap();
            let (base, entry) = super::resolve_workspace_entry(&root, "media/dblink").unwrap();
            assert_eq!(
                super::resolve_workspace_file_content(&base, &entry).unwrap_err(),
                super::STATE_DB_PROTECTED
            );
            // 数据库本体也不可复制。
            let (base, entry) = super::resolve_workspace_entry(&root, "state.sqlite").unwrap();
            assert_eq!(
                super::resolve_workspace_file_content(&base, &entry).unwrap_err(),
                super::STATE_DB_PROTECTED
            );
        }
    }

    #[test]
    fn agent_and_upload_paths_resolve_inside_uploads_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let root = home.join("agent");
        std::fs::create_dir_all(root.join("uploads").join("sub")).unwrap();
        std::fs::write(root.join("uploads/photo.jpg"), b"jpg").unwrap();
        std::fs::write(root.join("uploads/sub/clip.mp4"), b"mp4").unwrap();
        std::fs::write(root.join("notes.md"), b"n").unwrap();
        std::fs::write(root.join("state.sqlite"), b"db").unwrap();
        let canonical_root = root.canonicalize().unwrap();
        // 只读解析：工作区内任意已存在文件；数据库 / 越区 / 不存在 / 非法拼写拒绝。
        assert_eq!(
            super::resolve_agent_path(home, "notes.md").unwrap(),
            canonical_root.join("notes.md")
        );
        assert_eq!(
            super::resolve_agent_path(home, "uploads/sub/clip.mp4").unwrap(),
            canonical_root.join("uploads").join("sub").join("clip.mp4")
        );
        assert_eq!(super::resolve_agent_path(home, "state.sqlite").unwrap_err(), super::STATE_DB_PROTECTED);
        for bad in ["../agent/notes.md", "uploads/../../x", "/etc/passwd", "missing.md", "", "a\\b"] {
            assert!(super::resolve_agent_path(home, bad).is_err(), "{bad:?}");
        }
        // 采集产物入口：必须落在 uploads/ 之内；uploads 本身、工作区别处、数据库都不行。
        assert_eq!(
            super::resolve_upload_path(home, "uploads/photo.jpg").unwrap(),
            canonical_root.join("uploads").join("photo.jpg")
        );
        for bad in ["uploads", "uploads/", "notes.md", "state.sqlite", "uploads/../notes.md", "uploads/nope.jpg"] {
            assert!(super::resolve_upload_path(home, bad).is_err(), "{bad:?}");
        }
        #[cfg(unix)]
        {
            // uploads/ 里指向数据库 / 工作区外的链接：按真实落点判，一律拒绝；指向 uploads/
            // 内文件的链接放行（落点仍在 uploads/ 内）。
            std::os::unix::fs::symlink(root.join("state.sqlite"), root.join("uploads/dblink")).unwrap();
            std::os::unix::fs::symlink("/etc/passwd", root.join("uploads/escape")).unwrap();
            std::os::unix::fs::symlink(root.join("notes.md"), root.join("uploads/outside")).unwrap();
            std::os::unix::fs::symlink(root.join("uploads/photo.jpg"), root.join("uploads/inside")).unwrap();
            assert_eq!(super::resolve_upload_path(home, "uploads/dblink").unwrap_err(), super::STATE_DB_PROTECTED);
            assert!(super::resolve_upload_path(home, "uploads/escape").is_err());
            assert!(super::resolve_upload_path(home, "uploads/outside").is_err());
            assert_eq!(
                super::resolve_upload_path(home, "uploads/inside").unwrap(),
                canonical_root.join("uploads").join("photo.jpg")
            );
        }
    }

    #[test]
    fn workspace_rel_paths_are_plain_segments() {
        for ok in ["a.md", "dir/sub/x.txt", "名字.md", "a..b.md", "state.sqlite"] {
            assert!(super::validate_workspace_rel_path(ok).is_ok(), "{ok}");
        }
        for bad in [
            "", ".", "..", "../x", "a/../b", "a/./b/..", "/abs", "a\\b", "a//b", "a/", "tab\tx",
            "\\\\?\\C:\\x",
        ] {
            assert!(super::validate_workspace_rel_path(bad).is_err(), "{bad:?}");
        }
        #[cfg(windows)]
        for bad in ["C:evil", "a:b", "a/b.", "a /b", "a<b", "a|b", "a?b", "a*b", "a\"b"] {
            assert!(super::validate_workspace_rel_path(bad).is_err(), "{bad:?}");
        }
        // Windows DOS 设备名：分量主干（第一个点之前）命中即拒，不分大小写；类 Unix 是普通名。
        #[cfg(windows)]
        {
            for bad in ["con", "CON", "nul.md", "dir/Com1/x.txt", "lpt9.log", "aux.tar.gz", "prn", "COM¹.md", "dir/lpt³/x.txt"] {
                assert!(super::validate_workspace_rel_path(bad).is_err(), "{bad:?}");
            }
            // 上标数字 ¹²³ 也是设备名尾字符（按字符数而非字节数判断）。
            for bad in ["con", "CON", "nul.md", "Com1", "lpt9.log", "aux.tar.gz", "prn", "COM¹", "com²", "LPT³.txt"] {
                assert!(super::validate_entry_name(bad).is_err(), "{bad:?}");
            }
            for ok in ["console.md", "com10.txt", "com0", "nulls", "lpt", "aux-notes.md", "prnt", "com⁴", "lpt¹x"] {
                assert!(super::validate_workspace_rel_path(ok).is_ok(), "{ok}");
                assert!(super::validate_entry_name(ok).is_ok(), "{ok}");
            }
        }
        #[cfg(not(windows))]
        for ok in ["con", "nul.md", "dir/Com1/x.txt"] {
            assert!(super::validate_workspace_rel_path(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn state_db_names_and_entries_are_recognized_case_insensitively() {
        for name in ["state.sqlite", "STATE.SQLITE", "State.SQLite-wal", "state.sqlite-shm", "state.sqlite-journal"] {
            assert!(super::is_state_db_name(name), "{name}");
        }
        // Unicode 大小写折叠后同形（长 s `ſ` 的大写是 `S`）：大小写不敏感的文件系统把它们
        // 当同一个名字，`eq_ignore_ascii_case` 看不见。
        for name in ["ſtate.sqlite", "ſtate.ſqlite-wal", "STATE.SQLITE-JOURNAL"] {
            assert!(super::is_state_db_name(name), "{name}");
        }
        for name in ["state.sqlite-backup", "state.sqlite2", "my-state.sqlite", "rclaw.toml", ""] {
            assert!(!super::is_state_db_name(name), "{name}");
        }
        // Win32 会静默剥掉尾随点 / 空格：落盘名就是保留名。
        #[cfg(windows)]
        for name in ["state.sqlite-journal.", "state.sqlite ", "state.sqlite-wal.."] {
            assert!(super::is_state_db_name(name), "{name:?}");
        }
        #[cfg(not(windows))]
        assert!(!super::is_state_db_name("state.sqlite-journal."));

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("agent");
        std::fs::create_dir_all(root.join("backups")).unwrap();
        let base = root.canonicalize().unwrap();
        // 只有根下直接子项算；子目录里的同名文件、根本身都不算。
        assert!(super::is_state_db_entry(&base, &base.join("state.sqlite")));
        assert!(super::is_state_db_entry(&base, &base.join("STATE.SQLITE-WAL")));
        assert!(!super::is_state_db_entry(&base, &base.join("backups").join("state.sqlite")));
        assert!(!super::is_state_db_entry(&base, &base));
        assert!(!super::is_state_db_entry(&base, &base.join("notes.md")));
    }

    #[test]
    fn copy_workspace_file_never_follows_a_planted_destination_link() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let root = home.join("agent");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/notes.md"), b"hello").unwrap();
        std::fs::write(root.join("state.sqlite"), b"db").unwrap();

        // 普通复制：同目录「-副本」命名，冲突递增。
        assert_eq!(
            super::copy_workspace_file(&home, "docs/notes.md").unwrap(),
            "docs/notes-副本.md"
        );
        assert_eq!(std::fs::read(root.join("docs/notes-副本.md")).unwrap(), b"hello");
        assert_eq!(
            super::copy_workspace_file(&home, "docs/notes.md").unwrap(),
            "docs/notes-副本2.md"
        );
        // 运行时数据库不可复制。
        assert_eq!(
            super::copy_workspace_file(&home, "state.sqlite").unwrap_err(),
            super::STATE_DB_PROTECTED
        );
        assert!(!root.join("state-副本.sqlite").exists());

        #[cfg(unix)]
        {
            // 事先摆好的悬空链接 `plan-副本.md -> <工作区外>`：exists() 看它是空位，fs::copy
            // 会顺着链接把内容写到工作区外；现在它算「已占用」，副本落到下一个候选名，
            // 链接目标不被创建。
            std::fs::write(root.join("docs/plan.md"), b"plan").unwrap();
            let outside = dir.path().join("outside.md");
            std::os::unix::fs::symlink(&outside, root.join("docs/plan-副本.md")).unwrap();
            assert_eq!(
                super::copy_workspace_file(&home, "docs/plan.md").unwrap(),
                "docs/plan-副本2.md"
            );
            assert!(!outside.exists(), "不得顺着悬空链接在工作区外创建文件");
            assert!(std::fs::symlink_metadata(root.join("docs/plan-副本.md"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read(root.join("docs/plan-副本2.md")).unwrap(), b"plan");
            // 指向库文件的链接：内容不可复制。
            std::os::unix::fs::symlink(root.join("state.sqlite"), root.join("docs/dblink.md")).unwrap();
            assert_eq!(
                super::copy_workspace_file(&home, "docs/dblink.md").unwrap_err(),
                super::STATE_DB_PROTECTED
            );
        }
    }

    /// 覆盖写的打开路径：悬空链接在打开处就失败，链接目标【不被创建】（`O_CREAT` 单独
    /// 会先把它建出来）；普通文件正常打开，`create_new` 撞上已有链接报 AlreadyExists。
    #[cfg(unix)]
    #[test]
    fn open_for_write_nofollow_never_creates_a_dangling_link_target() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside.txt");
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(super::open_for_write_nofollow(&link, false).is_err());
        assert!(!outside.exists(), "不得顺着悬空链接创建目标文件");
        assert_eq!(
            super::open_for_write_nofollow(&link, true).unwrap_err().kind(),
            std::io::ErrorKind::AlreadyExists
        );
        assert!(!outside.exists());
        // 指向已有文件的链接同样拒绝，目标内容不动。
        let real = dir.path().join("real.txt");
        std::fs::write(&real, b"keep").unwrap();
        let link2 = dir.path().join("link2.txt");
        std::os::unix::fs::symlink(&real, &link2).unwrap();
        assert!(super::open_for_write_nofollow(&link2, false).is_err());
        assert_eq!(std::fs::read(&real).unwrap(), b"keep");
        // 普通文件：覆盖写与新建都正常。
        let mut file = super::open_for_write_nofollow(&real, false).unwrap();
        file.set_len(0).unwrap();
        std::io::Write::write_all(&mut file, b"new").unwrap();
        drop(file);
        assert_eq!(std::fs::read(&real).unwrap(), b"new");
        let fresh = dir.path().join("fresh.txt");
        drop(super::open_for_write_nofollow(&fresh, true).unwrap());
        assert!(fresh.is_file());
    }

    #[test]
    fn delete_workspace_entry_can_remove_stray_reserved_directory_but_not_the_db() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let root = home.join("agent");
        std::fs::create_dir_all(root.join("state.sqlite-journal").join("sub")).unwrap();
        std::fs::write(root.join("state.sqlite-journal/sub/x.txt"), b"x").unwrap();
        std::fs::write(root.join("state.sqlite"), b"db").unwrap();
        std::fs::write(root.join("note.md"), b"note").unwrap();

        // 库文件本身拒绝；叫保留名的【真实目录】可删（它只会让 SQLite 开不了库）。
        assert_eq!(
            super::delete_workspace_entry(&home, "state.sqlite").unwrap_err(),
            super::STATE_DB_PROTECTED
        );
        super::delete_workspace_entry(&home, "state.sqlite-journal").unwrap();
        assert!(!root.join("state.sqlite-journal").exists());
        assert_eq!(std::fs::read(root.join("state.sqlite")).unwrap(), b"db");
        // 普通条目照删；根本身 / 非法路径拒绝。
        super::delete_workspace_entry(&home, "note.md").unwrap();
        assert!(!root.join("note.md").exists());
        assert!(super::delete_workspace_entry(&home, "").is_err());
        assert!(super::delete_workspace_entry(&home, "../agent").is_err());

        #[cfg(unix)]
        {
            // 叫保留名的链接不是真实目录：仍拒绝。普通目录链接删的是链接本身。
            std::os::unix::fs::symlink(root.join("state.sqlite"), root.join("state.sqlite-wal")).unwrap();
            assert_eq!(
                super::delete_workspace_entry(&home, "state.sqlite-wal").unwrap_err(),
                super::STATE_DB_PROTECTED
            );
            std::fs::create_dir_all(root.join("docs")).unwrap();
            std::fs::write(root.join("docs/keep.md"), b"keep").unwrap();
            std::os::unix::fs::symlink(root.join("docs"), root.join("docs-link")).unwrap();
            super::delete_workspace_entry(&home, "docs-link").unwrap();
            assert!(std::fs::symlink_metadata(root.join("docs-link")).is_err());
            assert_eq!(std::fs::read(root.join("docs/keep.md")).unwrap(), b"keep");
        }
    }

    #[test]
    fn rename_workspace_entry_guards_db_and_renames_leaf_only() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let root = home.join("agent");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.md"), b"a").unwrap();
        std::fs::write(root.join("b.md"), b"b").unwrap();
        std::fs::write(root.join("state.sqlite"), b"db").unwrap();

        // 普通改名；目标已存在 / 非法名 / 根 / 不存在的条目拒绝。
        super::rename_workspace_entry(&home, "docs/a.md", " a2.md ").unwrap();
        assert_eq!(std::fs::read(root.join("docs/a2.md")).unwrap(), b"a");
        std::fs::write(root.join("docs/taken.md"), b"t").unwrap();
        assert!(super::rename_workspace_entry(&home, "docs/a2.md", "taken.md").is_err());
        assert!(super::rename_workspace_entry(&home, "docs/a2.md", "x/y.md").is_err());
        assert!(super::rename_workspace_entry(&home, "", "x.md").is_err());
        assert!(super::rename_workspace_entry(&home, "nope.md", "x.md").is_err());
        // 库文件不能被改名；根下别的文件也不能改成保留名（伴生名此刻不存在同样拦）。
        assert_eq!(
            super::rename_workspace_entry(&home, "state.sqlite", "backup.sqlite").unwrap_err(),
            super::STATE_DB_PROTECTED
        );
        for reserved in ["state.sqlite-wal", "STATE.SQLITE-JOURNAL"] {
            assert_eq!(
                super::rename_workspace_entry(&home, "b.md", reserved).unwrap_err(),
                super::STATE_DB_PROTECTED,
                "{reserved}"
            );
        }
        // 子目录里叫保留名无妨。
        super::rename_workspace_entry(&home, "docs/taken.md", "state.sqlite-wal").unwrap();
        assert!(root.join("docs/state.sqlite-wal").exists());
        assert_eq!(std::fs::read(root.join("state.sqlite")).unwrap(), b"db");
        // 根下叫保留名的【真实目录】与 delete 同口径可改名（它不是库文件，改走名字同样
        // 能让 SQLite 重新打开库）。
        std::fs::create_dir(root.join("state.sqlite-journal")).unwrap();
        super::rename_workspace_entry(&home, "state.sqlite-journal", "stray-dir").unwrap();
        assert!(root.join("stray-dir").is_dir());
        assert!(!root.join("state.sqlite-journal").exists());

        #[cfg(unix)]
        {
            // 链接改名改的是链接本身；指向库文件的链接与库同口径拒绝。
            std::os::unix::fs::symlink(root.join("docs"), root.join("docs-link")).unwrap();
            super::rename_workspace_entry(&home, "docs-link", "docs-link2").unwrap();
            assert!(std::fs::symlink_metadata(root.join("docs-link2"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert!(root.join("docs/a2.md").exists());
            // 悬空链接也占着名字：不能被 rename 静默换掉。
            std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();
            assert!(super::rename_workspace_entry(&home, "b.md", "dangling").is_err());
            assert!(std::fs::symlink_metadata(root.join("dangling"))
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(std::fs::read(root.join("b.md")).unwrap(), b"b");
            std::os::unix::fs::symlink(root.join("state.sqlite"), root.join("dblink")).unwrap();
            assert_eq!(
                super::rename_workspace_entry(&home, "dblink", "renamed").unwrap_err(),
                super::STATE_DB_PROTECTED
            );
            assert_eq!(
                super::delete_workspace_entry(&home, "dblink").unwrap_err(),
                super::STATE_DB_PROTECTED
            );
        }
    }

    /// Windows 8.3 短名是同一文件的另一个合法拼写：`STATE~1.SQL` 过得了名字校验、字面又不是
    /// 保留名，删除 / 重命名必须按真实落点识别出它就是库。卷上关了 8.3 生成时拿不到短名，
    /// 直接跳过。
    #[cfg(windows)]
    #[test]
    fn delete_and_rename_refuse_windows_short_name_alias_of_the_db() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let root = home.join("agent");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("state.sqlite"), b"db").unwrap();
        let wide: Vec<u16> = root
            .join("state.sqlite")
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut buf = vec![0u16; 1024];
        let len = unsafe {
            windows_sys::Win32::Storage::FileSystem::GetShortPathNameW(
                wide.as_ptr(),
                buf.as_mut_ptr(),
                buf.len() as u32,
            )
        };
        assert!(len > 0 && (len as usize) < buf.len(), "GetShortPathNameW 失败");
        let short = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buf[..len as usize]));
        let Some(alias) = short.file_name().and_then(|n| n.to_str()).map(str::to_string) else {
            panic!("短名不可解析: {short:?}");
        };
        if alias.eq_ignore_ascii_case("state.sqlite") {
            eprintln!("卷未启用 8.3 短名生成，跳过");
            return;
        }
        assert!(!super::is_state_db_name(&alias), "{alias} 字面不是保留名，才是本测试要挡的别名");
        assert_eq!(
            super::delete_workspace_entry(&home, &alias).unwrap_err(),
            super::STATE_DB_PROTECTED
        );
        assert_eq!(
            super::rename_workspace_entry(&home, &alias, "gone.sqlite").unwrap_err(),
            super::STATE_DB_PROTECTED
        );
        assert_eq!(std::fs::read(root.join("state.sqlite")).unwrap(), b"db");
    }

    #[test]
    fn admit_pasted_file_moves_from_staging_and_refuses_reserved_names() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let root = home.join("agent");
        std::fs::create_dir_all(&root).unwrap();
        let base = root.canonicalize().unwrap();
        let staging = home.join(super::PASTE_STAGING_DIR);
        std::fs::create_dir_all(&staging).unwrap();

        // 普通文件：搬进目标目录，暂存清空，返回 ok:<名>。
        std::fs::write(staging.join("notes.txt"), b"hello").unwrap();
        assert_eq!(
            super::admit_pasted_file(&home, &base, true, &staging, "ok:notes.txt").unwrap(),
            "ok:notes.txt"
        );
        assert_eq!(std::fs::read(base.join("notes.txt")).unwrap(), b"hello");
        assert!(!staging.join("notes.txt").exists());
        // 同名已存在：按「名(n).扩展」递增，不覆盖。
        std::fs::write(staging.join("notes.txt"), b"second").unwrap();
        assert_eq!(
            super::admit_pasted_file(&home, &base, true, &staging, "ok:notes.txt").unwrap(),
            "ok:notes(1).txt"
        );
        assert_eq!(std::fs::read(base.join("notes.txt")).unwrap(), b"hello");
        assert_eq!(std::fs::read(base.join("notes(1).txt")).unwrap(), b"second");
        // 根下的运行时数据库保留名：暂存文件删掉、拒绝，工作区根从未出现过它。
        std::fs::write(staging.join("state.sqlite-journal"), b"fake").unwrap();
        assert_eq!(
            super::admit_pasted_file(&home, &base, true, &staging, "ok:state.sqlite-journal")
                .unwrap_err(),
            super::STATE_DB_PROTECTED
        );
        assert!(!staging.join("state.sqlite-journal").exists());
        assert!(!base.join("state.sqlite-journal").exists());
        // 子目录不是根：同名放行。
        let sub = base.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(staging.join("state.sqlite-journal"), b"fake").unwrap();
        assert_eq!(
            super::admit_pasted_file(&home, &sub, false, &staging, "ok:state.sqlite-journal")
                .unwrap(),
            "ok:state.sqlite-journal"
        );
        // 桥接返回异常 / 非法名：不碰文件系统。
        assert!(super::admit_pasted_file(&home, &base, true, &staging, "error:x").is_err());
        assert!(super::admit_pasted_file(&home, &base, true, &staging, "ok:").is_err());
        assert!(super::admit_pasted_file(&home, &base, true, &staging, "ok:a/b.txt").is_err());
        assert!(super::admit_pasted_file(&home, &base, true, &staging, "ok:..").is_err());
    }

    #[test]
    fn entry_names_must_be_single_plain_components() {
        for ok in ["笔记.md", "a-b_c.txt", "no-ext", "state.sqlite-backup", "x.y.z", "a..b.md"] {
            assert!(super::validate_entry_name(ok).is_ok(), "{ok}");
            // 单分量名 join 后父目录不变（rename / new_folder 的二次核对依赖此性质）。
            let joined = std::path::Path::new("parent").join(ok);
            assert_eq!(joined.parent(), Some(std::path::Path::new("parent")), "{ok}");
        }
        for bad in [
            "", ".", "..", "a/b", "a\\b", "../x", "C:evil", "C:\\evil", "state.sqlite:ads",
            "a<b", "a>b", "a|b", "a?b", "a*b", "a\"b", "tab\tname", "trailing.", "trailing ",
            "\\\\?\\C:\\x",
        ] {
            assert!(super::validate_entry_name(bad).is_err(), "{bad:?}");
        }
        let long = "名".repeat(121);
        assert!(super::validate_entry_name(&long).is_err());
    }

    #[test]
    fn merge_archived_skills_appends_archived_entries_only() {
        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        std::fs::create_dir_all(&skills_dir).unwrap();

        let usage = SkillUsageStore::new(skills_dir.clone());
        usage.record_created("demo", SkillOwner::Agent).unwrap();
        // mark_archived 要求归档路径在（规范化后的）skills_dir 内，与 do_archive 传入的
        // canonical dest 一致。
        let canon = skills_dir.canonicalize().unwrap();
        usage
            .mark_archived("demo", &canon.join(".archive/demo-20260101-000000"))
            .unwrap();

        let mut value = serde_json::json!({
            "skills": [
                {"name": "live", "state": "active"},
                {"name": "demo", "state": "active"},
            ]
        });
        merge_archived_skills(&mut value, &skills_dir);

        let skills = value["skills"].as_array().unwrap();
        // 在位技能不被重复追加。
        assert_eq!(skills.len(), 2);
        // lifecycle 里 demo 已归档但清单已有同名条目 → 不重复。

        // 清单不含 demo 时（真实归档后的场景：目录已移出扫描范围）→ 合并追加。
        let mut value = serde_json::json!({ "skills": [{"name": "live", "state": "active"}] });
        merge_archived_skills(&mut value, &skills_dir);
        let skills = value["skills"].as_array().unwrap();
        assert_eq!(skills.len(), 2);
        let archived = skills.last().unwrap();
        assert_eq!(archived["name"], "demo");
        assert_eq!(archived["state"], "archived");
    }

    // ---- 主配置虚拟条目（工作区暴露 rclaw.toml）----

    #[test]
    fn config_virtual_entry_rules() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join("agent")).unwrap();
        // 无同名真实文件：虚拟条目生效；仅根级精确名命中。
        assert!(super::config_virtual_active(&home.join("agent")));
        assert!(super::is_config_virtual(&home.join("agent"), "rclaw.toml"));
        assert!(!super::is_config_virtual(&home.join("agent"), "sub/rclaw.toml"));
        assert!(!super::is_config_virtual(&home.join("agent"), "other.toml"));
        let body = "[agent]\nbase_dir = \"agent\"\n";
        std::fs::write(home.join("rclaw.toml"), body).unwrap();
        let dto = super::config_entry_dto(home);
        assert!(dto.is_config);
        assert_eq!(dto.name, "rclaw.toml");
        assert_eq!(dto.size, body.len() as u64);
        // agent 根出现同名真实文件 → 虚拟停用（真实文件优先，避免同名并列）。
        std::fs::write(home.join("agent/rclaw.toml"), "x").unwrap();
        assert!(!super::config_virtual_active(&home.join("agent")));
        assert!(!super::is_config_virtual(&home.join("agent"), "rclaw.toml"));
    }

    fn config_home() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        // 与向导模板同构的最小可解析配置。
        std::fs::write(
            dir.path().join("rclaw.toml"),
            "# 手写注释\n[agent]\nbase_dir = \"agent\" # 根目录\nmax_concurrent_turns = 6\nsession_compression = true\n\n[log]\nfile = true\nfile_level = \"info\"\nconsole = false\n\n[llm]\nprovider = \"zhipu\"\n\n[[llm.providers]]\nname = \"zhipu\"\nbase_url = \"https://open.bigmodel.cn/api/paas/v4\"\nmodel = \"glm-5.3\"\napi_key = \"sk-live\"\ncontext_length = 131072\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn config_import_replaces_file_when_valid() {
        let dir = config_home();
        let imported = "# 导入的配置\n[agent]\nbase_dir = \"agent\"\n\n[log]\nfile = true\n\n[llm]\nprovider = \"zhipu\"\n\n[[llm.providers]]\nname = \"zhipu\"\nbase_url = \"https://example.com\"\nmodel = \"glm-5.3\"\napi_key = \"sk-new\"\ncontext_length = 65536\n";
        super::config_import_raw_for(dir.path(), imported).unwrap();
        let on_disk = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(on_disk, imported);
    }

    #[test]
    fn config_import_rejects_invalid_without_writing() {
        let dir = config_home();
        let before = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        // 结构解析失败。
        super::config_import_raw_for(dir.path(), "not [ valid toml").unwrap_err();
        // Config 校验失败（provider 引用断开）。
        super::config_import_raw_for(
            dir.path(),
            "[llm]\nprovider = \"missing\"\n\n[[llm.providers]]\nname = \"other\"\nbase_url = \"https://x\"\nmodel = \"m\"\napi_key = \"k\"\n",
        )
        .unwrap_err();
        // 空内容拒绝。
        super::config_import_raw_for(dir.path(), "  \n").unwrap_err();
        let after = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(before, after, "校验失败时原文件必须原样保留");
    }

    #[test]
    fn config_state_merges_current_values_into_doc() {
        let dir = config_home();
        let state = super::config_state_for(dir.path()).unwrap();
        let agent = state
            .doc
            .sections
            .iter()
            .find(|s| s.path == "agent")
            .unwrap();
        let base = agent.items.iter().find(|i| i.key == "base_dir").unwrap();
        assert_eq!(base.current.as_deref(), Some("agent"));
        // 未配置的默认键 current = None（如 auto_review_tooled）。
        let tooled = agent
            .items
            .iter()
            .find(|i| i.key == "auto_review_tooled")
            .unwrap();
        assert!(tooled.current.is_none());
        // provider 数组段按 name 寻址取当前值。
        let zhipu = state
            .doc
            .sections
            .iter()
            .find(|s| s.path == "llm.providers.zhipu")
            .unwrap();
        let model = zhipu.items.iter().find(|i| i.key == "model").unwrap();
        assert_eq!(model.current.as_deref(), Some("glm-5.3"));
        // [llm].model 未显式配置：「默认」展示回填为选中 provider 的真实生效模型，
        // 而不是示例占位（glm-5.3）。
        let llm = state
            .doc
            .sections
            .iter()
            .find(|s| s.path == "llm")
            .unwrap();
        let llm_model = llm.items.iter().find(|i| i.key == "model").unwrap();
        assert!(llm_model.current.is_none());
        assert_eq!(llm_model.default_raw, "\"glm-5.3\"");
    }

    #[test]
    fn config_save_roundtrip_preserves_comments_and_validates() {
        let dir = config_home();
        let edits = vec![
            super::ConfigEdit {
                path: "agent.max_concurrent_turns".into(),
                kind: "int".into(),
                value: "9".into(),
            },
            super::ConfigEdit {
                path: "llm.providers.zhipu.model".into(),
                kind: "str".into(),
                value: "glm-5.3-flash".into(),
            },
            super::ConfigEdit {
                path: "agent.auto_memory".into(),
                kind: "bool".into(),
                value: "true".into(),
            },
        ];
        super::config_save_edits(dir.path(), &edits).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("# 手写注释"), "注释必须保真");
        assert!(text.contains("max_concurrent_turns = 9"));
        assert!(text.contains("model = \"glm-5.3-flash\""));
        assert!(text.contains("auto_memory = true"));
        // 保存后仍可被配置面板读取（文件结构与文档一致）。
        assert!(super::config_state_for(dir.path()).is_ok());
    }

    #[test]
    fn config_save_rejects_bad_value_without_writing() {
        let dir = config_home();
        let before = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        let bad = vec![super::ConfigEdit {
            path: "agent.max_concurrent_turns".into(),
            kind: "int".into(),
            value: "not-a-number".into(),
        }];
        let err = super::config_save_edits(dir.path(), &bad).unwrap_err();
        assert!(err.contains("整数"), "{err}");
        // 校验失败不得落盘。
        let after = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn config_save_rejects_unknown_provider_name() {
        let dir = config_home();
        let before = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        // provider 切到不存在的名字：会导致重启失败，必须保存前拦下。
        let bad = vec![super::ConfigEdit {
            path: "llm.provider".into(),
            kind: "str".into(),
            value: "nonexistent".into(),
        }];
        let err = super::config_save_edits(dir.path(), &bad).unwrap_err();
        assert!(err.contains("llm.providers"), "{err}");
        let after = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert_eq!(before, after, "校验失败不得落盘");

        // 已配置的名字可保存；review_provider 空串 = 未配置，放行。
        let ok = vec![
            super::ConfigEdit {
                path: "llm.provider".into(),
                kind: "str".into(),
                value: "zhipu".into(),
            },
            super::ConfigEdit {
                path: "llm.review_provider".into(),
                kind: "str".into(),
                value: String::new(),
            },
        ];
        super::config_save_edits(dir.path(), &ok).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("review_provider = \"\""));
    }

    #[test]
    fn config_save_rejects_provider_rename_breaking_reference() {
        let dir = config_home();
        // 只改 provider 条目的 name（引用仍指旧名）→ 拒绝。
        let rename = vec![super::ConfigEdit {
            path: "llm.providers.zhipu.name".into(),
            kind: "str".into(),
            value: "renamed".into(),
        }];
        let err = super::config_save_edits(dir.path(), &rename).unwrap_err();
        assert!(err.contains("llm.provider"), "{err}");
        // 同步改 name + provider 引用 → 通过。
        let both = vec![
            super::ConfigEdit {
                path: "llm.providers.zhipu.name".into(),
                kind: "str".into(),
                value: "renamed".into(),
            },
            super::ConfigEdit {
                path: "llm.provider".into(),
                kind: "str".into(),
                value: "renamed".into(),
            },
        ];
        super::config_save_edits(dir.path(), &both).unwrap();
        let text = std::fs::read_to_string(dir.path().join("rclaw.toml")).unwrap();
        assert!(text.contains("name = \"renamed\""));
        assert!(text.contains("provider = \"renamed\""));
    }
}

// ---------------------------------------------------------------------------
// Bot Mode（ADR-0006）：bot 定义 CRUD 与群聊房间
// ---------------------------------------------------------------------------

/// bot 定义 DTO（前端编辑器与花名册共用）。
#[derive(Debug, Clone, Serialize)]
pub struct BotDto {
    pub name: String,
    pub title: String,
    pub description: String,
    pub persona: String,
    pub provider: Option<String>,
    pub denied_tools: Vec<String>,
}

/// 群聊房间 DTO（含完整日志与活动流；列表用轻量版）。
#[derive(Debug, Clone, Serialize)]
pub struct GroupDto {
    pub id: String,
    pub name: String,
    pub members: Vec<String>,
    pub running: bool,
    pub created_ts: i64,
    pub lines: Vec<rclaw_store::GroupLine>,
    pub activities: Vec<rclaw_store::GroupActivity>,
}

impl From<rclaw_store::GroupRoom> for GroupDto {
    fn from(r: rclaw_store::GroupRoom) -> Self {
        GroupDto {
            id: r.id,
            name: r.name,
            members: r.members,
            running: r.running,
            created_ts: r.created_ts,
            lines: r.lines,
            activities: r.activities,
        }
    }
}

fn bot_components(
    state: &State<'_, AppState>,
) -> Result<
    (
        std::sync::Arc<rclaw_runtime::BotRegistry>,
        std::sync::Arc<rclaw_runtime::BotRouter>,
    ),
    String,
> {
    state
        .bot_components()
        .map(|(b, r, _)| (b, r))
        .ok_or_else(|| "agent 尚未启动".to_string())
}

/// bot 花名册（含 persona 全文，供编辑器回填）。
#[tauri::command]
pub async fn bots_list(state: State<'_, AppState>) -> Result<Vec<BotDto>, String> {
    let (registry, _) = bot_components(&state)?;
    Ok(registry
        .all()
        .into_iter()
        .map(|p| BotDto {
            name: p.name.clone(),
            title: p.def.title.clone(),
            description: p.def.description.clone(),
            persona: p.def.persona.clone(),
            provider: p.def.provider.clone(),
            denied_tools: p.def.denied_tools.clone(),
        })
        .collect())
}

/// 可选 provider 名列表（bot 编辑器「固定模型」下拉）。
#[tauri::command]
pub async fn bot_providers(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let (_, _, providers) = state
        .bot_components()
        .ok_or_else(|| "agent 尚未启动".to_string())?;
    Ok(providers)
}

/// 创建 / 覆盖保存 bot 定义并热重载注册表（下一个 turn 生效）。
#[tauri::command]
pub async fn bot_save(
    state: State<'_, AppState>,
    name: String,
    title: String,
    description: String,
    persona: String,
    provider: Option<String>,
    denied_tools: Vec<String>,
) -> Result<(), String> {
    let (registry, _) = bot_components(&state)?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let store = rclaw_store::BotStore::new(&layout.base);
    let def = rclaw_store::BotDef {
        title,
        description,
        persona,
        provider: provider.filter(|p| !p.trim().is_empty()),
        denied_tools,
    };
    store.save(&name, def).map_err(|e| e.to_string())?;
    registry.reload(state.web_search().as_deref());
    Ok(())
}

/// 删除 bot 定义与其数据目录（会话记录仍留在 state.sqlite，可在会话列表里逐个删除）。
#[tauri::command]
pub async fn bot_delete(state: State<'_, AppState>, name: String) -> Result<(), String> {
    let (registry, _) = bot_components(&state)?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let store = rclaw_store::BotStore::new(&layout.base);
    if !store.delete(&name).map_err(|e| e.to_string())? {
        return Err(format!("bot 不存在: {name}"));
    }
    registry.reload(state.web_search().as_deref());
    Ok(())
}

/// 群聊列表（轻量：不含日志与活动流）。
#[tauri::command]
pub async fn groups_list(state: State<'_, AppState>) -> Result<Vec<GroupDto>, String> {
    let layout = state.layout().ok_or("agent 尚未启动")?;
    Ok(rclaw_store::GroupStore::new(&layout.base)
        .list()
        .into_iter()
        .map(GroupDto::from)
        .collect())
}

/// 创建群聊（成员须为已注册 bot；2..=6 人）。
#[tauri::command]
pub async fn group_create(
    state: State<'_, AppState>,
    name: String,
    members: Vec<String>,
) -> Result<GroupDto, String> {
    let (registry, _) = bot_components(&state)?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let store = rclaw_store::GroupStore::new(&layout.base);
    // 8 位十六进制随机 id：碰撞（≈2^-32）时重抽，不做静默覆盖。
    let mut id = String::new();
    for _ in 0..8 {
        let candidate: String = (0..8)
            .map(|_| {
                use rand::Rng;
                format!("{:x}", rand::thread_rng().gen::<u8>() & 0xf)
            })
            .collect();
        if store.get(&candidate).is_err() {
            id = candidate;
            break;
        }
    }
    let room = rclaw_store::GroupRoom::new(&id, &name, members, chrono::Utc::now().timestamp())
        .map_err(|e| e.to_string())?;
    for member in &room.members {
        if registry.get(member).is_none() {
            return Err(format!("成员 {member} 不是已注册的 bot"));
        }
    }
    store.save(&room).map_err(|e| e.to_string())?;
    Ok(GroupDto::from(room))
}

/// 删除群聊（进行中的编排先拒绝）。以进程内登记为权威：崩溃残留的文件
/// running 标志不阻塞删除。
#[tauri::command]
pub async fn group_delete(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let layout = state.layout().ok_or("agent 尚未启动")?;
    // try_begin/end 包住删除：删除期间新编排无法启动，杜绝「删除后又被
    // 并发编排的收尾落盘复活」的竞态窗口。
    let Some(_guard) = state.group_runs.try_delete_guard(&id) else {
        return Err("该群聊有进行中的编排，请先取消或等待收敛".into());
    };
    let store = rclaw_store::GroupStore::new(&layout.base);
    store.delete(&id).map_err(|e| e.to_string())?;
    Ok(())
}

/// 群聊完整历史（转录 + 活动流）。
#[tauri::command]
pub async fn group_history(state: State<'_, AppState>, id: String) -> Result<GroupDto, String> {
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let room = rclaw_store::GroupStore::new(&layout.base)
        .get(&id)
        .map_err(|e| e.to_string())?;
    Ok(GroupDto::from(room))
}

/// 向群聊发送一条用户消息并触发一轮编排（结果经 `group` 事件推送）。
#[tauri::command]
pub async fn group_send(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
    text: String,
) -> Result<(), String> {
    let (registry, router) = bot_components(&state)?;
    let layout = state.layout().ok_or("agent 尚未启动")?;
    let groups = rclaw_store::GroupStore::new(&layout.base);
    crate::groups::group_send(
        app,
        &state.group_runs,
        &registry,
        &router,
        &groups,
        &id,
        &text,
    )
}

/// 取消进行中的群聊编排：置取消令牌（轮间检查）+ 向正在执行的成员轮会话
/// 投递 /cancel（立即打断长 turn，不必等到 600s 超时）。
#[tauri::command]
pub async fn group_cancel(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), String> {
    let cancelled = state.group_runs.cancel(&id);
    if let Some(chat) = state.group_runs.current_chat(&id) {
        let _ = crate::agent::send(&app, chat, "/cancel".to_string()).await;
    }
    if cancelled {
        Ok(())
    } else {
        Err("该群聊当前没有进行中的编排".into())
    }
}


// ---------------------------------------------------------------------------
// 技能运行面：会话内嵌媒体读取、脚本审批、技能配置编辑。
// ---------------------------------------------------------------------------

/// 会话内嵌图片载荷（前端以 data URL 渲染）。
#[derive(Debug, Serialize)]
pub struct MediaPayload {
    pub mime: String,
    pub base64: String,
}

const MEDIA_MAX_BYTES: u64 = 25 * 1024 * 1024;
const MEDIA_EXTENSIONS: [&str; 5] = ["png", "jpg", "jpeg", "webp", "gif"];

/// 预览图长边上限（2K 屏的 1x 显示足够；放大看原图由前端按需加载全图）。
const MEDIA_PREVIEW_MAX_EDGE: u32 = 1440;
/// 预览缓存目录（相对 agent/；预览是派生物，进 media/cache 不污染用户文件）。
const MEDIA_PREVIEW_CACHE_DIR: &str = "media/cache/preview";

/// 生成（带磁盘缓存）媒体的屏幕级预览图：大图解码后按长边缩到上限再编码
/// JPEG，避免前端解码 12MP 原图（真机实测 4032×3024 全图解码 + 光栅 2~3s，
/// 是会话里点开图片卡顿的根因）。缓存键 = 路径 + 文件大小 + mtime 的哈希，
/// 文件更新自动失效；GIF（动画丢帧）与小图（长边不超上限）原样返回不重编码。
#[tauri::command]
pub async fn media_preview(
    state: State<'_, AppState>,
    path: String,
) -> Result<MediaPayload, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || media_preview_blocking(&home, &path))
        .await
        .map_err(|e| format!("生成预览任务失败: {e}"))?
}

fn media_preview_blocking(home: &std::path::Path, path: &str) -> Result<MediaPayload, String> {
    if path.contains("..") || path.contains('\\') || path.starts_with('/') {
        return Err("非法媒体路径".into());
    }
    let extension = path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !MEDIA_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!("不支持的媒体类型: {extension}"));
    }
    let root = home.join("agent");
    let full = root
        .join(path)
        .canonicalize()
        .map_err(|e| format!("媒体文件不存在: {e}"))?;
    let root_canon = root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    if !full.starts_with(&root_canon) {
        return Err("媒体路径越出工作区".into());
    }
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取媒体失败: {e}"))?;
    if meta.len() > MEDIA_MAX_BYTES {
        return Err(format!(
            "媒体超过大小上限（{} MB）",
            MEDIA_MAX_BYTES / 1024 / 1024
        ));
    }
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/gif",
    };
    let original = std::fs::read(&full).map_err(|e| format!("读取媒体失败: {e}"))?;

    // GIF 帧序列会被单帧解码丢掉动画：原样返回。
    if mime == "image/gif" {
        return Ok(MediaPayload {
            mime: mime.into(),
            base64: encode_base64(&original),
        });
    }

    // 缓存命中（键含 mtime+大小，文件更新自动失效）。
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key: String = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(path.as_bytes());
        hasher.update(meta.len().to_le_bytes());
        hasher.update(mtime.to_le_bytes());
        hasher
            .finalize()[..12]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    let cache_dir = root.join(MEDIA_PREVIEW_CACHE_DIR);
    // 无 alpha → .jpg，带 alpha → .png（JPEG 无透明）：读缓存两种后缀都试，
    // 否则透明图永远不命中、每次打开都重新解码整张原图。
    let cache_base = cache_dir.join(&key);
    for (ext, cache_mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
        if let Ok(cached) = std::fs::read(cache_base.with_extension(ext)) {
            return Ok(MediaPayload {
                mime: cache_mime.into(),
                base64: encode_base64(&cached),
            });
        }
    }

    let img = image::load_from_memory(&original).map_err(|e| format!("解码图片失败: {e}"))?;
    // 长边本就不超上限：原图已足够小，原样返回（不重编码损质量）。
    if img.width() <= MEDIA_PREVIEW_MAX_EDGE && img.height() <= MEDIA_PREVIEW_MAX_EDGE {
        return Ok(MediaPayload {
            mime: mime.into(),
            base64: encode_base64(&original),
        });
    }
    let thumb = img.thumbnail(MEDIA_PREVIEW_MAX_EDGE, MEDIA_PREVIEW_MAX_EDGE);
    // 带 alpha 的缩略图编码 PNG（JPEG 无透明，会衬黑底）；缓存后缀同步跟随。
    let has_alpha = thumb.color().has_alpha();
    let mut encoded = Vec::new();
    if has_alpha {
        let encoder = image::codecs::png::PngEncoder::new(&mut encoded);
        thumb
            .write_with_encoder(encoder)
            .map_err(|e| format!("编码预览失败: {e}"))?;
    } else {
        let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, 82);
        thumb
            .write_with_encoder(encoder)
            .map_err(|e| format!("编码预览失败: {e}"))?;
    }
    let cache_path = cache_base.with_extension(if has_alpha { "png" } else { "jpg" });
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::write(&cache_path, &encoded); // 缓存写失败只影响下次，不挡返回
    Ok(MediaPayload {
        mime: if has_alpha { "image/png".into() } else { "image/jpeg".into() },
        base64: encode_base64(&encoded),
    })
}

/// 读取 agent 工作区内的媒体文件（相对 `agent/` 的路径，如 `media/x.png`），
/// 供会话流内嵌预览。路径校验：归一后必须仍位于 `agent/` 之内。
#[tauri::command]
pub async fn media_base64(state: State<'_, AppState>, path: String) -> Result<MediaPayload, String> {
    // 最大 25MB 的同步读 + 编码挪到阻塞线程池（与同文件其他命令一致），
    // 避免卡住 Tauri 命令线程（低端安卓设备上明显）。
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || media_base64_blocking(&home, &path))
        .await
        .map_err(|e| format!("读取媒体任务失败: {e}"))?
}

fn media_base64_blocking(home: &std::path::Path, path: &str) -> Result<MediaPayload, String> {
    if path.contains("..") || path.contains('\\') || path.starts_with('/') {
        return Err("非法媒体路径".into());
    }
    let extension = path
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !MEDIA_EXTENSIONS.contains(&extension.as_str()) {
        return Err(format!("不支持的媒体类型: {extension}"));
    }
    let root = home.join("agent");
    let full = root.join(path);
    // 归一后必须在 agent/ 之内（防符号链接/相对段逃逸）。
    let full = full
        .canonicalize()
        .map_err(|e| format!("媒体文件不存在: {e}"))?;
    let root_canon = root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    if !full.starts_with(&root_canon) {
        return Err("媒体路径越出工作区".into());
    }
    let meta = std::fs::metadata(&full).map_err(|e| format!("读取媒体失败: {e}"))?;
    if meta.len() > MEDIA_MAX_BYTES {
        return Err(format!(
            "媒体超过大小上限（{} MB）",
            MEDIA_MAX_BYTES / 1024 / 1024
        ));
    }
    let mime = match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/gif",
    };
    let bytes = std::fs::read(&full).map_err(|e| format!("读取媒体失败: {e}"))?;
    Ok(MediaPayload {
        mime: mime.into(),
        base64: encode_base64(&bytes),
    })
}

/// 标准库没有 base64：手写编码（3 字节 → 4 字符）。
fn encode_base64(data: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

// 仅在非安卓宿主构建里被技能审批/配置命令的兜底分支调用；安卓（apk-host）
// 构建里这些分支整体消失，helper 若不随 cfg 消失就是 dead_code。
#[cfg(not(all(target_os = "android", feature = "apk-host")))]
fn reject_non_android<T>() -> Result<T, String> {
    Err("技能审批/配置编辑仅安卓 APK 内可用".into())
}

/// 待审批脚本条目。
#[derive(Debug, Serialize)]
pub struct SkillApprovalDto {
    pub script: String,
    pub has_baseline: bool,
}

/// 列出未获批准（无基线且无用户批准，或内容已偏离）的技能脚本。
#[tauri::command]
pub async fn skill_approvals_pending() -> Result<Vec<SkillApprovalDto>, String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        let gate = rclaw_android::approval_gate().ok_or("技能闸门尚未就绪")?;
        let pending = tauri::async_runtime::spawn_blocking(move || {
            gate.store.pending_approvals(&gate.skills_dir)
        })
        .await
        .map_err(|e| format!("审批查询任务失败: {e}"))?;
        Ok(pending
            .into_iter()
            .map(|p| SkillApprovalDto {
                script: p.script,
                has_baseline: p.has_baseline,
            })
            .collect())
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        reject_non_android()
    }
}

/// 校验前端传来的相对脚本路径（防越界）。
#[cfg(all(target_os = "android", feature = "apk-host"))]
fn validate_relative_script(script: &str) -> Result<(), String> {
    if script.contains("..") || script.contains('\\') || script.starts_with('/') {
        return Err("非法脚本路径".into());
    }
    Ok(())
}

/// 批准脚本当前内容（写入用户批准位；无基线的新脚本同样适用）。
#[tauri::command]
pub async fn skill_approvals_approve(script: String) -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        validate_relative_script(&script)?;
        let gate = rclaw_android::approval_gate().ok_or("技能闸门尚未就绪")?;
        let path = gate.skills_dir.join(&script);
        tauri::async_runtime::spawn_blocking(move || {
            gate.store.approve_user_version(&gate.skills_dir, &path)
        })
        .await
        .map_err(|e| format!("审批任务失败: {e}"))?
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = script;
        reject_non_android()
    }
}

/// 撤销用户批准（回退到 APK 内置基线判定）。
#[tauri::command]
pub async fn skill_approvals_revoke(script: String) -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        validate_relative_script(&script)?;
        let gate = rclaw_android::approval_gate().ok_or("技能闸门尚未就绪")?;
        let path = gate.skills_dir.join(&script);
        tauri::async_runtime::spawn_blocking(move || {
            gate.store.revoke_user_approval(&path);
        })
        .await
        .map_err(|e| format!("撤销任务失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = script;
        reject_non_android()
    }
}

/// 技能配置文件条目（相对 `agent/skills/` 的路径 + 大小）。
#[derive(Debug, Serialize)]
pub struct SkillConfigEntry {
    pub path: String,
    pub size: u64,
}

/// 枚举技能配置（`skills/*/config.toml` 与 `skills/*.toml` 顶层共享配置）。
#[tauri::command]
pub async fn skill_config_list() -> Result<Vec<SkillConfigEntry>, String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        let gate = rclaw_android::approval_gate().ok_or("技能闸门尚未就绪")?;
        let scan = tauri::async_runtime::spawn_blocking(move || -> std::io::Result<Vec<SkillConfigEntry>> {
            let mut entries: Vec<SkillConfigEntry> = Vec::new();
            for entry in std::fs::read_dir(&gate.skills_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                if entry.file_type()?.is_dir() {
                    for inner in std::fs::read_dir(entry.path())? {
                        let inner = inner?;
                        if inner.file_name().to_string_lossy().ends_with(".toml") {
                            entries.push(SkillConfigEntry {
                                path: format!("{name}/{}", inner.file_name().to_string_lossy()),
                                size: inner.metadata().map(|m| m.len()).unwrap_or(0),
                            });
                        }
                    }
                } else if name.ends_with(".toml") {
                    entries.push(SkillConfigEntry {
                        path: name,
                        size: entry.metadata().map(|m| m.len()).unwrap_or(0),
                    });
                }
            }
            entries.sort_by(|a, b| a.path.cmp(&b.path));
            Ok(entries)
        })
        .await
        .map_err(|e| format!("配置扫描任务失败: {e}"))?;
        scan.map_err(|e| format!("扫描技能配置失败: {e}"))
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        reject_non_android()
    }
}

/// 读取技能配置内容。
#[tauri::command]
pub async fn skill_config_read(path: String) -> Result<String, String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        if path.contains("..") || path.contains('\\') || path.starts_with('/') {
            return Err("非法配置路径".into());
        }
        let gate = rclaw_android::approval_gate().ok_or("技能闸门尚未就绪")?;
        let full = gate.skills_dir.join(&path);
        tauri::async_runtime::spawn_blocking(move || {
            std::fs::read_to_string(&full).map_err(|e| format!("读取配置失败: {e}"))
        })
        .await
        .map_err(|e| format!("配置读取任务失败: {e}"))?
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = path;
        reject_non_android()
    }
}

/// 保存技能配置内容（直写文件、不经过技能脚本变更钩子；改动脚本后的安全
/// 由执行闸门的脚本哈希比对兜底——哈希不符即要求重新批准）。
#[tauri::command]
pub async fn skill_config_write(path: String, content: String) -> Result<(), String> {
    #[cfg(all(target_os = "android", feature = "apk-host"))]
    {
        if path.contains("..") || path.contains('\\') || path.starts_with('/') {
            return Err("非法配置路径".into());
        }
        let gate = rclaw_android::approval_gate().ok_or("技能闸门尚未就绪")?;
        let full = gate.skills_dir.join(&path);
        tauri::async_runtime::spawn_blocking(move || {
            std::fs::write(&full, content).map_err(|e| format!("保存配置失败: {e}"))
        })
        .await
        .map_err(|e| format!("配置保存任务失败: {e}"))?
    }
    #[cfg(not(all(target_os = "android", feature = "apk-host")))]
    {
        let _ = (path, content);
        reject_non_android()
    }
}

/// 校验并解析工作区相对媒体路径 → 绝对路径（open/save/share 共用：拒穿越与
/// 反斜杠，canonicalize 后必须仍落在 agent/ 工作区内且文件存在）。
fn resolve_media_file(home: &std::path::Path, rel: &str) -> Result<std::path::PathBuf, String> {
    if rel.contains("..") || rel.contains('\\') || rel.starts_with('/') {
        return Err("非法媒体路径".into());
    }
    let root = home.join("agent");
    let full = root
        .join(rel)
        .canonicalize()
        .map_err(|e| format!("媒体文件不存在: {e}"))?;
    let root_canon = root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    if !full.starts_with(&root_canon) {
        return Err("媒体路径越出工作区".into());
    }
    Ok(full)
}

/// 扩展名 → 保存类型（photo/video/audio 进相册；其余按文档进下载目录）。
fn media_kind_of(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "gif" => "photo",
        "mp4" | "webm" | "m4v" | "mov" | "3gp" => "video",
        "m4a" | "aac" | "wav" | "mp3" | "ogg" | "opus" | "flac" => "audio",
        _ => "file",
    }
}

/// 会话内点开工作区媒体：安卓经 FileProvider 拉起系统播放器/查看器，
/// 桌面用系统默认程序打开（路径校验同 media_base64）。
#[tauri::command]
pub async fn open_media(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let full = resolve_media_file(&state.home, &path)?;

    #[cfg(target_os = "android")]
    {
        let _ = &full;
        // FileProvider 的 URI 需要调用方包名上下文，转交 Java 侧执行。
        rclaw_android::open_media(&path).map_err(|e| format!("打开媒体失败: {e}"))
    }
    #[cfg(not(target_os = "android"))]
    {
        // 同 open_url：文件名（来自模型/技能输出）可能含 & 等字符，不走 cmd。
        let shell_path = full.to_string_lossy().to_string();
        std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", &shell_path])
            .spawn()
            .map_err(|e| format!("打开媒体失败: {e}"))?;
        Ok(())
    }
}

/// 会话媒体保存到本地：图片/视频/音频走 save_to_gallery（安卓 MediaStore 相册 /
/// 桌面 Pictures/RClaw）；文档（pdf/md/xlsx…）安卓写入系统下载目录
/// （MediaStore.Downloads）、桌面拷贝到「下载/RClaw」。返回 "ok" / "permission_requested"。
#[tauri::command]
pub async fn save_media(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let kind = media_kind_of(&path);
    if kind != "file" {
        return save_to_gallery(state.clone(), path, kind.to_string()).await;
    }
    // 主配置虚拟条目：导出 home 根的实体（配置备份到下载目录）。
    let abs = if is_config_virtual(&state.home.join("agent"), &path) {
        state.home.join(CONFIG_NAME)
    } else {
        resolve_media_file(&state.home, &path)?
    };
    #[cfg(target_os = "android")]
    {
        let result = rclaw_android::save_to_downloads(&abs.to_string_lossy())
            .map_err(|e| format!("保存到下载目录失败: {e}"))?;
        if let Some(reason) = result.strip_prefix("error:") {
            return Err(reason.to_string());
        }
        Ok(result)
    }
    #[cfg(not(target_os = "android"))]
    {
        let base = std::env::var("USERPROFILE")
            .map(|p| std::path::PathBuf::from(p).join("Downloads"))
            .unwrap_or_else(|_| state.home.clone());
        let dir = base.join("RClaw");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
        let name = abs.file_name().ok_or("非法文件名")?;
        std::fs::copy(&abs, dir.join(name)).map_err(|e| format!("保存失败: {e}"))?;
        Ok("ok".into())
    }
}

/// 转发工作区媒体到其它应用（安卓系统分享面板：微信/QQ / 保存到文件等）。
#[tauri::command]
pub async fn share_media(state: State<'_, AppState>, path: String) -> Result<(), String> {
    // 路径校验同 open_media（canonicalize + 工作区前缀），防越区分享。
    let _abs = resolve_media_file(&state.home, &path)?;
    #[cfg(target_os = "android")]
    {
        rclaw_android::share_file(&path).map_err(|e| format!("转发失败: {e}"))
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("转发到其它应用仅安卓支持".into())
    }
}

/// 把纯文本分享到其它应用（安卓 ACTION_SEND text/plain：微信/QQ 收为聊天
/// 文字）。会话「选择导出到微信/QQ」的文本通道。
#[tauri::command]
pub async fn share_text(title: String, text: String) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        rclaw_android::share_text(&title, &text).map_err(|e| format!("分享失败: {e}"))
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (title, text);
        Err("分享文本仅安卓支持（桌面可导出为文档后手动发送）".into())
    }
}

// ---------------------------------------------------------------------------
// 消息选择导出：选中消息打包成 Markdown 文本（.md）或内嵌图片的 DOCX。
// 产物落 output/exports/，前端再经 save_media / share_media 决定去向。
// ---------------------------------------------------------------------------

/// 待导出消息（前端按会话时间顺序传入）。
#[derive(Debug, serde::Deserialize)]
pub struct ExportMessageDto {
    /// user | assistant | thinking | status。
    pub role: String,
    /// 消息原文（媒体路径引用保持原样，docx 据此内嵌图片）。
    pub content: String,
}

/// 导出选中消息为文档。fmt: "md" | "docx"。返回 output/exports/ 相对路径。
#[tauri::command]
pub async fn export_messages(
    state: State<'_, AppState>,
    title: String,
    fmt: String,
    messages: Vec<ExportMessageDto>,
) -> Result<String, String> {
    if messages.is_empty() {
        return Err("未选择任何消息".into());
    }
    if !matches!(fmt.as_str(), "md" | "docx") {
        return Err(format!("不支持的导出格式: {fmt}"));
    }
    let title = if title.trim().is_empty() {
        "会话导出".to_string()
    } else {
        title.trim().chars().take(60).collect()
    };
    let home = state.home.clone();
    let parsed: Vec<crate::export::ExportMessage> = messages
        .into_iter()
        .filter(|m| {
            matches!(
                m.role.as_str(),
                "user" | "assistant" | "thinking" | "status"
            )
        })
        .map(|m| crate::export::ExportMessage {
            role: m.role,
            content: m.content,
        })
        .collect();
    if parsed.is_empty() {
        return Err("未包含可导出的消息".into());
    }
    let stamp = chrono::Local::now();
    tauri::async_runtime::spawn_blocking(move || {
        let agent_root = home.join("agent");
        let rel = crate::export::export_rel_path(
            &title,
            &stamp.format("%Y%m%d-%H%M%S").to_string(),
            &fmt,
        );
        let bytes = if fmt == "docx" {
            crate::export::render_docx(&title, &stamp.format("%Y-%m-%d %H:%M").to_string(), &parsed, &agent_root)?
        } else {
            crate::export::render_markdown(
                &title,
                &stamp.format("%Y-%m-%d %H:%M").to_string(),
                &parsed,
            )
            .into_bytes()
        };
        let abs = crate::export::export_abs_path(&agent_root, &rel);
        if let Some(parent) = abs.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建导出目录失败: {e}"))?;
        }
        std::fs::write(&abs, bytes).map_err(|e| format!("写入导出文件失败: {e}"))?;
        Ok(rel)
    })
    .await
    .map_err(|e| format!("导出任务失败: {e}"))?
}

// ---------------------------------------------------------------------------
// 关于页版本信息：各模块版本在编译期捕获（子 crate 的 VERSION 常量与
// build.rs 注入的环境）。应用版本与 tauri.properties（安卓 versionName）
// 同一来源，改一处即可。
// ---------------------------------------------------------------------------

/// 关于页所需的模块版本。
#[derive(Debug, serde::Serialize)]
pub struct AppVersionsDto {
    /// 应用版本（与安卓 versionName 同源）。
    pub app: String,
    /// Agent 内核（turn 循环 / 工具批次 / 完成闸门）。
    pub agent_runtime: String,
    /// 核心类型（消息 / 会话 / 总线）。
    pub agent_core: String,
    /// LLM 接入（provider / 流式协议）。
    pub agent_llm: String,
    /// 工具集。
    pub agent_tools: String,
    /// 会话与记忆存储。
    pub agent_store: String,
    /// CLI 装配（config / bootstrap）。
    pub agent_cli: String,
    /// 通道接入（仅 APK 宿主链接）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_channels: Option<String>,
    /// Tauri 壳框架。
    pub tauri: String,
    /// rustc 编译器版本。
    pub rustc: String,
    /// 构建时的 git 短提交。
    pub git: String,
    /// 构建时间（UTC epoch 秒）。
    pub build_time: i64,
}

/// 返回关于页的模块版本信息（纯编译期常量，无 IO）。
#[tauri::command]
pub async fn app_versions() -> AppVersionsDto {
    AppVersionsDto {
        app: env!("RCLAW_APP_VERSION").to_string(),
        agent_runtime: rclaw_runtime::VERSION.to_string(),
        agent_core: rclaw_core::VERSION.to_string(),
        agent_llm: rclaw_llm::VERSION.to_string(),
        agent_tools: rclaw_tools::VERSION.to_string(),
        agent_store: rclaw_store::VERSION.to_string(),
        agent_cli: rclaw_cli::VERSION.to_string(),
        agent_channels: {
            // rclaw-channels 挂在 android 目标依赖表（apk-host 才链入）：
            // host/desktop 构建无此 crate，通道行留空。
            #[cfg(all(feature = "apk-host", target_os = "android"))]
            {
                Some(rclaw_channels::VERSION.to_string())
            }
            #[cfg(not(all(feature = "apk-host", target_os = "android")))]
            {
                None
            }
        },
        tauri: tauri::VERSION.to_string(),
        rustc: env!("RCLAW_RUSTC").to_string(),
        git: env!("RCLAW_GIT").to_string(),
        build_time: env!("RCLAW_BUILD_TIME").parse::<i64>().unwrap_or(0),
    }
}

/// 视频/音频的本地流式 URL：仅回环地址 + 随机 token 的 HTTP 服务（支持
/// Range/206 分段），WebView `<video>` 据此边下边播并显示首帧——data URL
/// 整包注入对视频不可行。路径守卫与 media_base64 相同。
#[tauri::command]
pub async fn media_stream_url(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let agent_root = state.home.join("agent");
    match crate::media_stream::stream_url_with_root(agent_root, &path) {
        Some(url) => Ok(url),
        None => Err("媒体文件不存在或路径越出工作区".into()),
    }
}

// ---------------------------------------------------------------------------
// 工作区文件面板：安卓上 agent 目录（应用私有存储）文件管理器无法访问，
// 侧栏「文件」栏目经这三条命令浏览 / 查看 / 编辑工作区内的文本文件。
// ---------------------------------------------------------------------------

/// 工作区目录条目。
#[derive(Debug, Serialize)]
pub struct WorkspaceEntryDto {
    pub name: String,
    pub is_dir: bool,
    /// 字节数（目录为 0）。
    pub size: u64,
    /// 最后修改（unix 毫秒）。
    pub modified_ms: u64,
    /// 主配置虚拟条目（rclaw.toml，实体在 home 根）：前端用于图标/操作门控。
    #[serde(default)]
    pub is_config: bool,
}

// ---------------------------------------------------------------------------
// 主配置虚拟条目：rclaw.toml 实体在工作区根（home/agent）的上一级，文件面板
// 看不到；这里以虚拟条目形式暴露在工作区列表顶部，read/write/copy 路由到
// home/rclaw.toml。agent 根存在同名真实文件时不启用（真实文件优先，避免歧义）。
// ---------------------------------------------------------------------------

const CONFIG_NAME: &str = "rclaw.toml";

/// 虚拟条目是否生效（agent 根无同名真实文件）。
fn config_virtual_active(agent_root: &std::path::Path) -> bool {
    !agent_root.join(CONFIG_NAME).exists()
}

/// 工作区相对路径是否指向主配置虚拟条目（仅根级精确名）。
fn is_config_virtual(agent_root: &std::path::Path, path: &str) -> bool {
    path == CONFIG_NAME && config_virtual_active(agent_root)
}

/// 虚拟条目 DTO（size/mtime 取自 home/rclaw.toml）。
fn config_entry_dto(home: &std::path::Path) -> WorkspaceEntryDto {
    let meta = std::fs::metadata(home.join(CONFIG_NAME)).ok();
    let (size, modified_ms) = meta
        .map(|m| {
            let modified = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            (m.len(), modified)
        })
        .unwrap_or((0, 0));
    WorkspaceEntryDto {
        name: CONFIG_NAME.to_string(),
        is_dir: false,
        size,
        modified_ms,
        is_config: true,
    }
}

/// 运行时数据库文件名（`agent/state.sqlite` 及 SQLite 的 WAL / SHM / 回滚日志伴生文件）。
/// 进程持有打开的连接：文件面板删除 / 重命名 / 以此为名新建都会撕裂正在写入的库，
/// 一律拒绝。读写本来就被文本扩展名白名单挡住，这里补齐剩余入口。
const STATE_DB_NAMES: &[&str] = &[
    "state.sqlite",
    "state.sqlite-wal",
    "state.sqlite-shm",
    "state.sqlite-journal",
];

/// 条目名是否是运行时数据库保留名。Windows 另剥掉 Win32 会静默丢弃的尾随点/空格
/// （`state.sqlite-journal.` 落盘后就是 `state.sqlite-journal`，一个假 hot journal 足以
/// 让下次打开时读坏库）。除 ASCII 大小写外还按 Unicode 大小写折叠比一次：NTFS / APFS
/// 这类大小写不敏感文件系统把 `ſtate.sqlite`（长 s）这类折叠后同形的名字当同一条目。
fn is_state_db_name(name: &str) -> bool {
    #[cfg(windows)]
    let name = name.trim_end_matches(['.', ' ']);
    STATE_DB_NAMES.iter().any(|db| {
        name.eq_ignore_ascii_case(db)
            || name.to_lowercase() == *db
            || name.to_uppercase() == db.to_uppercase()
    })
}

/// 已解析（canonical）的工作区条目是否就是根下的运行时数据库文件。
/// 按真实路径判断而不是入参字面——`./state.sqlite`、尾随点等拼写差异不影响结论。
/// 叶子未 canonicalize 的调用方（删除 / 重命名）要用 [`is_state_db_target`]。
fn is_state_db_entry(base: &std::path::Path, canonical: &std::path::Path) -> bool {
    canonical.parent() == Some(base)
        && canonical
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_state_db_name)
}

/// 叶子【未跟随】解析出的条目是否落在运行时数据库文件上：先按条目自己的名字判，再按
/// `canonicalize` 后的真实落点判。后者挡住两类别名：Windows 8.3 短名——`STATE~1.SQL` 是
/// `state.sqlite` 的另一个合法拼写，`symlink_metadata` 能探到、字面却不是保留名；以及指向
/// 库文件的符号链接（删 / 改名链接本身无害，但与 rclaw-tools 同口径一律拒绝，省得为一个
/// 没人需要的操作留判断分支）。真实目录不可能是库文件，由调用方按 `symlink_metadata`
/// 先行放行。
fn is_state_db_target(base: &std::path::Path, entry: &std::path::Path) -> bool {
    is_state_db_entry(base, entry)
        || entry
            .canonicalize()
            .is_ok_and(|real| is_state_db_entry(base, &real))
}

/// 以「不跟随叶子链接」的方式打开待写入的文件（`create_new` = 必须新建，否则覆盖或新建）。
///
/// 先 `symlink_metadata` 再写仍留下检查与打开之间的窗口：一条此刻被摆进来的悬空链接
/// `x -> /别处/新文件` 会让 `fs::write` / Windows 的 `CREATE_NEW` 顺着链接把内容写到工作区外
/// （Windows 的 CreateFile 先穿越 reparse point 再应用 CREATE_NEW；POSIX 的 `O_EXCL` 不跟随，
/// 但覆盖写用的 `O_CREAT` 跟随——它会先把链接目标【创建】出来）。所以打开本身就不跟随，
/// 再用句柄核对，核对通过前不截断、不写：
/// - Windows：`FILE_FLAG_OPEN_REPARSE_POINT` 让 CreateFile 停在 reparse point 本身，`CREATE_NEW`
///   撞上已有链接报 AlreadyExists；覆盖写时看句柄的 `is_symlink()`。不是链接 / junction 的
///   reparse point（OneDrive 云占位、WOF 压缩、去重文件）也拒绝：绕过其过滤驱动裸写会损坏它，
///   而正常打开又会丢掉不跟随链接的保证。
/// - Unix：`O_NOFOLLOW` 让 open 在链接处直接失败（ELOOP），不会创建链接目标；`O_NONBLOCK`
///   让换成 FIFO 的路径不把阻塞线程挂死。再比较 `lstat(path)` 与 `fstat(fd)` 的 (dev, ino)
///   兜底：路径此刻已被换成别的文件时二者不一致。
fn open_for_write_nofollow(
    path: &std::path::Path,
    create_new: bool,
) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(
            windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    let handle_meta = file.metadata()?;
    let not_plain_file = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "目标是符号链接或非普通文件，不跟随链接写入",
        )
    };
    if handle_meta.file_type().is_symlink() || !handle_meta.is_file() {
        return Err(not_plain_file());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if handle_meta.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "目标是云占位 / 压缩等特殊文件（reparse point），文件面板不直接写入；请先在资源管理器里\
                 将其设为「始终保留在此设备上」或取消压缩",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let path_meta = std::fs::symlink_metadata(path)?;
        if path_meta.file_type().is_symlink()
            || (path_meta.dev(), path_meta.ino()) != (handle_meta.dev(), handle_meta.ino())
        {
            return Err(not_plain_file());
        }
    }
    Ok(file)
}

const STATE_DB_PROTECTED: &str =
    "state.sqlite 是 RClaw 运行时数据库（会话 / 回滚点 / journal），不能在文件面板里删除、重命名、复制或覆盖；要导出会话请在聊天里用 /export";

/// 校验前端传来的工作区相对路径（`a/b/c.md`；空串是否代表根由各命令自行决定）。
///
/// 只过滤 `..` / `\` / 前导 `/` 不够：Windows 上 `base.join("C:evil")` 会整个替换成盘符
/// 相对路径，`name:stream` 打开的是备用数据流。逐段要求是普通分量（非空、非 `.` / `..`、
/// 无控制字符），Windows 再加 `:<>"|?*` 与尾随点 / 空格；最后用 `components()` 兜底，
/// 任何 Prefix / RootDir / ParentDir 都拒绝。面板列出的现有条目名来自 `read_dir`，
/// 类 Unix 允许 `:` 等字符出现在文件名里，所以那组字符只在 Windows 上拦。
fn validate_workspace_rel_path(path: &str) -> Result<(), String> {
    if path.contains('\\') || path.starts_with('/') {
        return Err("非法路径".into());
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err("非法路径".into());
        }
        if segment.chars().any(char::is_control) {
            return Err("路径不能包含控制字符".into());
        }
        #[cfg(windows)]
        if segment
            .chars()
            .any(|c| matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*'))
            || segment.ends_with('.')
            || segment.ends_with(' ')
        {
            return Err("路径不能包含 : < > \" | ? * 或以点 / 空格结尾".into());
        }
        if is_dos_device_name(segment) {
            return Err("路径分量不能是 Windows 设备名（CON / NUL / COM1 / LPT1 …）".into());
        }
    }
    if !std::path::Path::new(path)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)))
    {
        return Err("非法路径".into());
    }
    Ok(())
}

/// 校验「新建 / 重命名」给出的条目名：必须是单个普通路径分量。
///
/// 只过滤 `/`、`\`、`..` 不够：Windows 上 `parent.join("C:evil")` 会整个替换成盘符相对路径
/// `C:evil`（落到 C: 当前目录），`name:stream` 打开的是备用数据流。统一在所有平台拒绝
/// `:<>"|?*` 与控制字符、尾随点 / 空格（Win32 会静默剥掉，落盘名与显示名不一致），并要求
/// `Path::new(name)` 恰好解析为一个 `Normal` 分量且原样等于 name（挡住 `.`、`..`、
/// 盘符、根、`\\?\` 前缀等一切非普通分量）。
fn validate_entry_name(name: &str) -> Result<(), String> {
    // 按字符数限长（len() 是字节数，中文名会被压到 40 字）。
    if name.is_empty() || name.chars().count() > 120 {
        return Err("名称不能为空且不超过 120 字符".into());
    }
    // `..` 本身由下面的「恰好一个 Normal 分量」检查拒绝；`a..b.md` 这种含连续点的普通名
    // 与工作区路径校验口径一致地放行（否则面板能打开却不能新建 / 改成这个名字）。
    if name.contains('/') || name.contains('\\') {
        return Err("名称不能包含路径分隔符".into());
    }
    if name
        .chars()
        .any(|c| matches!(c, ':' | '<' | '>' | '"' | '|' | '?' | '*') || c.is_control())
    {
        return Err("名称不能包含 : < > \" | ? * 或控制字符".into());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("名称不能以点或空格结尾".into());
    }
    if is_dos_device_name(name) {
        return Err("名称不能是 Windows 设备名（CON / NUL / COM1 / LPT1 …）".into());
    }
    let mut components = std::path::Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(std::path::Component::Normal(only)), None) if only == name => Ok(()),
        _ => Err("名称必须是单个文件名，不能是路径".into()),
    }
}

/// 解析工作区相对目录（空 = agent 根）：拒绝越区，必须真实存在且是目录。
fn resolve_workspace_dir(root: &std::path::Path, rel: &str) -> Result<std::path::PathBuf, String> {
    if !rel.is_empty() {
        validate_workspace_rel_path(rel).map_err(|_| "非法目录路径".to_string())?;
    }
    let base = root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    let target = if rel.is_empty() {
        base.clone()
    } else {
        base.join(rel)
    };
    let canonical = target
        .canonicalize()
        .map_err(|e| format!("目录不存在: {e}"))?;
    if !canonical.starts_with(&base) {
        return Err("目录越出工作区".into());
    }
    if !canonical.is_dir() {
        return Err("不是目录".into());
    }
    Ok(canonical)
}

/// 列出工作区目录（目录在前、按名排序；空 path = agent 根）。
#[tauri::command]
pub async fn workspace_list(
    state: State<'_, AppState>,
    path: String,
) -> Result<Vec<WorkspaceEntryDto>, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let dir = resolve_workspace_dir(&home.join("agent"), &path)?;
        let mut out = Vec::new();
        for entry in std::fs::read_dir(&dir).map_err(|e| format!("读取目录失败: {e}"))? {
            let entry = entry.map_err(|e| format!("读取目录项失败: {e}"))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            let modified_ms = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            out.push(WorkspaceEntryDto {
                name,
                is_dir: entry.file_type().map(|t| t.is_dir()).unwrap_or(false),
                size: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                modified_ms,
                is_config: false,
            });
        }
        out.sort_by(|a, b| {
            b.is_dir
                .cmp(&a.is_dir)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        // 根目录置顶注入主配置虚拟条目（实体在 home 根且已存在——wizard
        // 完成前没有 rclaw.toml，不注入空壳；空 path = agent 根）。
        if path.is_empty()
            && config_virtual_active(&dir)
            && home.join(CONFIG_NAME).is_file()
        {
            out.insert(0, config_entry_dto(&home));
        }
        Ok(out)
    })
    .await
    .map_err(|e| format!("列目录任务失败: {e}"))?
}

/// 文本编辑上限：1MB（大日志 / 导出文件只读浏览，编辑走外部导出）。
const WORKSPACE_TEXT_MAX_BYTES: u64 = 1024 * 1024;

/// 可编辑文本的扩展名白名单（无扩展名文件经 NUL 探测放行，如 LICENSE）。
fn is_workspace_editable_text(name: &str) -> bool {
    const TEXT_EXTS: &[&str] = &[
        "md", "txt", "json", "jsonl", "toml", "yaml", "yml", "py", "rs", "ts", "tsx", "js",
        "jsx", "css", "html", "xml", "csv", "log", "sh", "bat", "ps1", "ini", "cfg", "conf",
        "env", "gitignore", "prompt", "skill", "lock",
    ];
    let lower = name.to_lowercase();
    match lower.rsplit_once('.') {
        Some((_, ext)) => TEXT_EXTS.contains(&ext),
        None => true,
    }
}

/// 读取工作区文本文件（仅白名单扩展名且 ≤1MB；返回原文）。
#[tauri::command]
pub async fn workspace_read(state: State<'_, AppState>, path: String) -> Result<String, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        validate_workspace_rel_path(&path).map_err(|_| "非法文件路径".to_string())?;
        let root = home.join("agent");
        // 主配置虚拟条目：读 home 根的实体。
        if is_config_virtual(&root, &path) {
            let target = home.join(CONFIG_NAME);
            let bytes = std::fs::read(&target).map_err(|e| format!("读取主配置失败: {e}"))?;
            return String::from_utf8(bytes)
                .map_err(|_| "主配置不是 UTF-8 文本".into());
        }
        let base = root
            .canonicalize()
            .map_err(|e| format!("工作区不可用: {e}"))?;
        let canonical = base
            .join(&path)
            .canonicalize()
            .map_err(|e| format!("文件不存在: {e}"))?;
        if !canonical.starts_with(&base) {
            return Err("文件越出工作区".into());
        }
        let name = canonical
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !is_workspace_editable_text(name) {
            return Err("仅支持编辑文本类文件（媒体/压缩包请在会话里打开或导出）".into());
        }
        let meta = canonical
            .metadata()
            .map_err(|e| format!("读取文件信息失败: {e}"))?;
        if !meta.is_file() {
            return Err("不是文件".into());
        }
        if meta.len() > WORKSPACE_TEXT_MAX_BYTES {
            return Err("文件超过 1MB，超出应用内编辑上限".into());
        }
        let bytes = std::fs::read(&canonical).map_err(|e| format!("读取文件失败: {e}"))?;
        // 无扩展名文件可能仍是二进制：前 8KB 含 NUL 视为二进制拒绝。
        if !name.contains('.') && bytes.iter().take(8192).any(|b| *b == 0) {
            return Err("二进制文件，不支持应用内编辑".into());
        }
        String::from_utf8(bytes).map_err(|_| "文件不是 UTF-8 文本".into())
    })
    .await
    .map_err(|e| format!("读取任务失败: {e}"))?
}

/// 保存工作区文本文件（覆盖或新建；同名约束同 read）。
#[tauri::command]
pub async fn workspace_write(
    state: State<'_, AppState>,
    path: String,
    content: String,
) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        validate_workspace_rel_path(&path).map_err(|_| "非法文件路径".to_string())?;
        let root = home.join("agent");
        // 主配置虚拟条目：完整校验（TOML 解析 + Config 校验 + provider 引用
        // 一致性）通过才落盘，与配置栏目「从文件导入」同一通道（错误文案
        // 自带「未写入文件」语义，直接透传）。
        if is_config_virtual(&root, &path) {
            if content.trim().is_empty() {
                return Err("内容为空：主配置不能保存为空文件".into());
            }
            return config_import_raw_for(&home, &content);
        }
        let base = root
            .canonicalize()
            .map_err(|e| format!("工作区不可用: {e}"))?;
        let target = base.join(&path);
        // 目标必须仍落在工作区内（父目录可能尚不存在，用词法前缀 + 物理校验双保险）。
        if let Some(parent) = target.parent() {
            let canonical_parent = parent
                .canonicalize()
                .map_err(|e| format!("目标目录不存在: {e}"))?;
            if !canonical_parent.starts_with(&base) {
                return Err("目标目录越出工作区".into());
            }
        }
        let name = target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        if !is_workspace_editable_text(name) {
            return Err("仅支持保存文本类文件".into());
        }
        if content.len() as u64 > WORKSPACE_TEXT_MAX_BYTES {
            return Err("内容超过 1MB 上限".into());
        }
        // 叶子不跟随符号链接：`notes.md -> state.sqlite`（或指向工作区外的文件）能过扩展名
        // 白名单，fs::write 却会顺着链接写到目标上。已存在的目标必须是普通文件。
        match std::fs::symlink_metadata(&target) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err("目标是符号链接，应用内编辑不跟随链接写入".into());
            }
            Ok(meta) if !meta.is_file() => return Err("目标不是文件".into()),
            _ => {}
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        // 上面的探测与这里的打开之间仍有窗口（悬空链接被摆进来）：打开后按句柄核对，
        // 核对通过才截断、写入。
        let mut file = open_for_write_nofollow(&target, false)
            .map_err(|e| format!("保存失败: {e}"))?;
        file.set_len(0)
            .and_then(|()| std::io::Write::write_all(&mut file, content.as_bytes()))
            .map_err(|e| format!("保存失败: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("保存任务失败: {e}"))?
}

/// 解析工作区内已存在的文件/目录（copy/delete/rename 共用）：返回 (base, entry)。
///
/// 只 canonicalize 父目录，叶子用 `symlink_metadata` 探存在性、**不跟随**：删除 / 重命名
/// 的对象是条目本身——`link -> notes/` 删的该是链接而不是 notes/ 里的内容，改名改的也是
/// 链接。需要读内容的调用方（复制）自己再对叶子 canonicalize 一次校验真实落点。
fn resolve_workspace_entry(
    root: &std::path::Path,
    path: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    if path.is_empty() {
        return Err("路径不能为空".into());
    }
    validate_workspace_rel_path(path).map_err(|_| "非法路径".to_string())?;
    let base = root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    let joined = base.join(path);
    let (parent, name) = match (joined.parent(), joined.file_name()) {
        (Some(parent), Some(name)) => (parent.to_path_buf(), name.to_os_string()),
        _ => return Err("非法路径".into()),
    };
    let canonical_parent = parent
        .canonicalize()
        .map_err(|e| format!("不存在: {e}"))?;
    if !canonical_parent.starts_with(&base) {
        return Err("路径越出工作区".into());
    }
    let entry = canonical_parent.join(name);
    std::fs::symlink_metadata(&entry).map_err(|e| format!("不存在: {e}"))?;
    Ok((base, entry))
}

/// 删除符号链接本身（不碰目标）。Windows 的目录链接 / junction 要用 `remove_dir`，
/// 文件链接用 `remove_file`：按条目自己的类型选删法，两种都试、报与类型相符的那个错误
/// （文件链接因共享冲突删不掉时，不能用 `remove_dir` 的「目录名无效」盖掉真因）。
fn remove_symlink(entry: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        type RemoveFn = fn(&std::path::Path) -> std::io::Result<()>;
        let is_dir_link = std::fs::symlink_metadata(entry)
            .is_ok_and(|meta| meta.file_type().is_symlink_dir());
        let (first, second): (RemoveFn, RemoveFn) = if is_dir_link {
            (|p| std::fs::remove_dir(p), |p| std::fs::remove_file(p))
        } else {
            (|p| std::fs::remove_file(p), |p| std::fs::remove_dir(p))
        };
        match first(entry) {
            Ok(()) => Ok(()),
            Err(e) => second(entry).map_err(|_| e),
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(entry)
    }
}

/// 复制类命令要读条目内容，跟随叶子链接解析到真实文件：落点仍须在工作区内，且不能是
/// 运行时数据库（`copyme -> state.sqlite` 这类链接名字无害、内容却是正在写的库）。
fn resolve_workspace_file_content(
    base: &std::path::Path,
    entry: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let target = entry
        .canonicalize()
        .map_err(|e| format!("不存在: {e}"))?;
    if !target.starts_with(base) {
        return Err("链接指向工作区外，拒绝复制".into());
    }
    if is_state_db_entry(base, entry) || is_state_db_entry(base, &target) {
        return Err(STATE_DB_PROTECTED.into());
    }
    let meta = target
        .metadata()
        .map_err(|e| format!("读取文件信息失败: {e}"))?;
    if !meta.is_file() {
        return Err("仅支持复制文件（目录请进入后逐个复制）".into());
    }
    Ok(target)
}

/// 复制工作区文件为同目录副本（自动命名「原名-副本.扩展」，冲突递增）。
#[tauri::command]
pub async fn workspace_copy_file(
    state: State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || copy_workspace_file(&home, &path))
        .await
        .map_err(|e| format!("复制任务失败: {e}"))?
}

/// 在 `dir` 下找一个尚不存在的副本名并把 `write` 的内容写进去（最多 999 个候选）。
///
/// 「尚不存在」用 `symlink_metadata` 判（`exists()` 会跟随链接：一条事先摆好的悬空链接
/// `x-副本.md -> /别处/新文件` 会被当成空位，`fs::copy` / `fs::write` 顺着链接把内容写到
/// 工作区外）；创建用 [`open_for_write_nofollow`]（POSIX `O_EXCL` 不跟随；Windows 加
/// `FILE_FLAG_OPEN_REPARSE_POINT` 才不跟随）兜住检查与创建之间的窗口。
fn write_fresh_copy(
    dir: &std::path::Path,
    candidate_name: impl Fn(u32) -> String,
    write: impl Fn(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<std::path::PathBuf, String> {
    write_fresh_file(dir, "复制失败", candidate_name, write)
}

/// [`write_fresh_copy`] 的通用形式：`failure` 是错误文案前缀（复制 / 上传 / 保存各自措辞）。
/// 上传、照片编辑保存这类落到 `uploads/` 下、名字可预测（时间戳）的新文件也走这里：
/// 事先摆好的同名悬空链接不会被 `fs::write` 顺着写到工作区外。
fn write_fresh_file(
    dir: &std::path::Path,
    failure: &str,
    candidate_name: impl Fn(u32) -> String,
    write: impl Fn(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<std::path::PathBuf, String> {
    for n in 1..=999u32 {
        let dest = dir.join(candidate_name(n));
        if std::fs::symlink_metadata(&dest).is_ok() {
            continue;
        }
        let mut file = match open_for_write_nofollow(&dest, true) {
            Ok(file) => file,
            // 刚被别人占了名字：换下一个候选。
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("{failure}: {e}")),
        };
        if let Err(e) = write(&mut file) {
            drop(file);
            let _ = std::fs::remove_file(&dest);
            return Err(format!("{failure}: {e}"));
        }
        return Ok(dest);
    }
    Err(format!("{failure}: 同名文件过多，请手动整理"))
}

/// [`workspace_copy_file`] 的核心（与 Tauri 状态解耦便于测试）：返回相对工作区根的副本路径。
fn copy_workspace_file(home: &std::path::Path, path: &str) -> Result<String, String> {
    let root = home.join("agent");
    // 主配置虚拟条目：复制为 agent 根下的真实副本（改动主配置前的手动备份路径）。
    if is_config_virtual(&root, path) {
        let bytes =
            std::fs::read(home.join(CONFIG_NAME)).map_err(|e| format!("读取主配置失败: {e}"))?;
        let base = root
            .canonicalize()
            .map_err(|e| format!("工作区不可用: {e}"))?;
        let dest = write_fresh_copy(
            &base,
            |n| {
                if n == 1 {
                    "rclaw-副本.toml".to_string()
                } else {
                    format!("rclaw-副本{n}.toml")
                }
            },
            |file| std::io::Write::write_all(file, &bytes),
        )?;
        return Ok(dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default());
    }
    let (base, entry) = resolve_workspace_entry(&root, path)?;
    // 副本放在条目所在目录、以条目名命名；内容取跟随链接后的真实文件（已校验落点与保留名）。
    let source = resolve_workspace_file_content(&base, &entry)?;
    let src_name = entry
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("文件名不可解析")?;
    let (stem, ext) = match src_name.rsplit_once('.') {
        Some((s, e)) => (s, Some(e)),
        None => (src_name, None),
    };
    let parent = entry.parent().ok_or("父目录不可解析")?;
    // 副本名一定含「-副本」，不可能撞上根目录保留名（state.sqlite*），无需再核。
    let candidate_name = |n: u32| match (n, ext) {
        (1, Some(e)) => format!("{stem}-副本.{e}"),
        (1, None) => format!("{stem}-副本"),
        (i, Some(e)) => format!("{stem}-副本{i}.{e}"),
        (i, None) => format!("{stem}-副本{i}"),
    };
    let dest = write_fresh_copy(parent, candidate_name, |file| {
        let mut from = std::fs::File::open(&source)?;
        std::io::copy(&mut from, file).map(|_| ())
    })?;
    // 相对工作区根的返回路径（供前端刷新定位；base 已 canonicalize，剥前缀得到相对分量）。
    Ok(dest
        .strip_prefix(&base)
        .map_err(|_| "结果路径异常".to_string())?
        .to_string_lossy()
        .replace('\\', "/"))
}

/// 删除工作区文件或目录（目录递归删除；拒绝删除 agent 根自身）。
#[tauri::command]
pub async fn workspace_delete(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if is_config_virtual(&home.join("agent"), &path) {
            return Err("主配置 rclaw.toml 是虚拟条目，不能删除（可清空内容或复制副本）".into());
        }
        delete_workspace_entry(&home, &path)
    })
    .await
    .map_err(|e| format!("删除任务失败: {e}"))?
}

/// [`workspace_delete`] 的核心（与 Tauri 状态解耦便于测试）。
fn delete_workspace_entry(home: &std::path::Path, path: &str) -> Result<(), String> {
    let (base, entry) = resolve_workspace_entry(&home.join("agent"), path)?;
    // 路径校验已保证非空且全是普通分量，解析不到根本身；留作兜底。
    if entry == base {
        return Err("不能删除工作区根目录".into());
    }
    // 叶子不跟随链接：删的是链接本身，不是它指向的目录 / 文件。
    let meta = std::fs::symlink_metadata(&entry).map_err(|e| format!("读取信息失败: {e}"))?;
    // 运行时数据库守卫只拦库文件本身与叫保留名的链接：一个叫 `state.sqlite-journal/` 的
    // 【真实目录】不可能是库文件，却会让 SQLite 下次打开失败——安卓上文件面板是应用私有
    // 目录唯一的文件管理器，不放行它用户就只剩清空应用数据一条路。
    // 字面之外还按真实落点判（Windows 8.3 短名 `STATE~1.SQL`、指向库文件的链接）。
    if !meta.is_dir() && is_state_db_target(&base, &entry) {
        return Err(STATE_DB_PROTECTED.into());
    }
    if meta.file_type().is_symlink() {
        remove_symlink(&entry).map_err(|e| format!("删除链接失败: {e}"))?;
    } else if meta.is_dir() {
        std::fs::remove_dir_all(&entry).map_err(|e| format!("删除目录失败: {e}"))?;
    } else {
        std::fs::remove_file(&entry).map_err(|e| format!("删除文件失败: {e}"))?;
    }
    Ok(())
}

/// 重命名文件/目录（仅名字，不换目录）：新名不含分隔符/..，目标不得已存在。
#[tauri::command]
pub async fn workspace_rename(
    state: State<'_, AppState>,
    path: String,
    new_name: String,
) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || rename_workspace_entry(&home, &path, &new_name))
        .await
        .map_err(|e| format!("重命名任务失败: {e}"))?
}

/// [`workspace_rename`] 的核心（与 Tauri 状态解耦便于测试）。
fn rename_workspace_entry(home: &std::path::Path, path: &str, new_name: &str) -> Result<(), String> {
    let name = new_name.trim();
    validate_entry_name(name)?;
    let root = home.join("agent");
    if is_config_virtual(&root, path) {
        return Err("主配置 rclaw.toml 不能重命名".into());
    }
    // 叶子不跟随链接：改名改的是条目本身（链接就改链接，不动它指向的文件）。
    let (base, entry) = resolve_workspace_entry(&root, path)?;
    // 路径校验已保证解析不到根本身（改名根 = 把整个工作区连库搬走）；留作兜底。
    if entry == base {
        return Err("不能重命名工作区根目录".into());
    }
    let parent = entry.parent().ok_or("父目录不可解析")?;
    let at_root = parent == base;
    // 运行时数据库：既不能被改名（含 8.3 短名 / 指向它的链接，见 is_state_db_target），
    // 也不能让别的文件改成它的名字（伴生文件可能此刻不存在，一个假 WAL 足以让 SQLite
    // 读坏库）。叫保留名的【真实目录】与 delete 同口径放行：它不是库文件，改走名字
    // 同样能让 SQLite 重新打开库。
    let meta = std::fs::symlink_metadata(&entry).map_err(|e| format!("读取信息失败: {e}"))?;
    if (!meta.is_dir() && is_state_db_target(&base, &entry)) || (at_root && is_state_db_name(name))
    {
        return Err(STATE_DB_PROTECTED.into());
    }
    let target = parent.join(name);
    // 名字已校验为单个普通分量，join 不可能换目录；再核一遍以免校验与 join 语义脱节。
    if target.parent() != Some(parent) {
        return Err("名称必须是单个文件名，不能是路径".into());
    }
    // 占用判定不跟随链接：悬空链接也算占着这个名字（与 write_fresh_copy 同口径；
    // `exists()` 会把它当成空位，rename 随后静默换掉链接条目）。
    if std::fs::symlink_metadata(&target).is_ok() {
        return Err("同名文件/目录已存在".into());
    }
    // 根目录的 rclaw.toml 保留给主配置虚拟条目（目录里探不到虚拟名）。
    if at_root && config_virtual_active(&root) && name.eq_ignore_ascii_case(CONFIG_NAME) {
        return Err("rclaw.toml 是主配置保留名（在根目录直接编辑即改主配置）".into());
    }
    std::fs::rename(&entry, &target).map_err(|e| format!("重命名失败: {e}"))
}

/// 在工作区目录下新建文件夹（dir 为当前目录，空 = 根）。
#[tauri::command]
pub async fn workspace_new_folder(
    state: State<'_, AppState>,
    dir: String,
    name: String,
) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let clean = name.trim();
        validate_entry_name(clean)?;
        let root = home
            .join("agent")
            .canonicalize()
            .map_err(|e| format!("工作区不可用: {e}"))?;
        let base = resolve_workspace_dir(&home.join("agent"), &dir)?;
        // 「位于根」按解析后的目录判断（空 dir = 根；指向根的目录链接解析后也是根）。
        let at_root = base == root;
        let target = base.join(clean);
        if target.parent() != Some(base.as_path()) {
            return Err("名称必须是单个文件名，不能是路径".into());
        }
        // 占用判定不跟随链接（悬空链接也占名字，与 rename / write_fresh_copy 同口径）。
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err("同名文件/目录已存在".into());
        }
        // 根目录的 rclaw.toml 保留给主配置虚拟条目（目录里探不到虚拟名）。
        if at_root && config_virtual_active(&root) && clean.eq_ignore_ascii_case(CONFIG_NAME) {
            return Err("rclaw.toml 是主配置保留名（在根目录直接编辑即改主配置）".into());
        }
        // 运行时数据库伴生文件名同样保留（此刻可能不存在，一个同名目录会让 SQLite 开不了 WAL）。
        if at_root && is_state_db_name(clean) {
            return Err(STATE_DB_PROTECTED.into());
        }
        std::fs::create_dir(&target).map_err(|e| format!("创建文件夹失败: {e}"))
    })
    .await
    .map_err(|e| format!("新建文件夹任务失败: {e}"))?
}

/// 系统剪贴板粘贴的暂存目录（home 下、工作区 `agent/` 之外，文件面板看不到）。
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
const PASTE_STAGING_DIR: &str = ".clipboard-staging";

/// 把 Java 侧落在暂存目录里的剪贴板文件搬进目标目录（[`workspace_paste_from_clipboard`]
/// 的收尾；与 Tauri / JNI 解耦便于在桌面上测试）。`result` 是桥接返回的 `ok:<文件名>`。
///
/// 名字先过关再搬：根目录下的主配置保留名、运行时数据库保留名一律删掉暂存文件并拒绝
/// （`uniqueClipFile` 只对已存在的名字加后缀，此刻不存在的伴生文件名会原名落地）。搬运用
/// [`write_fresh_copy`]：目标目录已有同名条目时按 Java 侧同一口径「名(n).扩展」递增，
/// 且永不跟随目标位置上的链接。返回搬运后的 `ok:<最终文件名>`。
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn admit_pasted_file(
    home: &std::path::Path,
    base: &std::path::Path,
    at_root: bool,
    staging: &std::path::Path,
    result: &str,
) -> Result<String, String> {
    let Some(landed) = result.strip_prefix("ok:") else {
        return Err(format!("剪贴板桥接返回异常: {result}"));
    };
    // 桥接只返回文件名；防御性地要求它确实是单个普通分量，否则不碰文件系统。
    let is_single_component = {
        let mut components = std::path::Path::new(landed).components();
        matches!(
            (components.next(), components.next()),
            (Some(std::path::Component::Normal(first)), None) if first == landed
        )
    };
    if landed.is_empty() || !is_single_component {
        return Err(format!("剪贴板桥接返回了非法文件名: {landed}"));
    }
    let staged = staging.join(landed);
    if at_root {
        if landed.eq_ignore_ascii_case(CONFIG_NAME) && config_virtual_active(&home.join("agent")) {
            let _ = std::fs::remove_file(&staged);
            return Err("rclaw.toml 是主配置保留名（在根目录直接编辑即改主配置）".into());
        }
        if is_state_db_name(landed) {
            let _ = std::fs::remove_file(&staged);
            return Err(STATE_DB_PROTECTED.into());
        }
    }
    let (stem, ext) = match landed.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, Some(ext)),
        _ => (landed, None),
    };
    let candidate_name = |n: u32| match (n, ext) {
        (1, _) => landed.to_string(),
        (i, Some(e)) => format!("{stem}({}).{e}", i - 1),
        (i, None) => format!("{stem}({})", i - 1),
    };
    let moved = write_fresh_copy(base, candidate_name, |file| {
        let mut from = std::fs::File::open(&staged)?;
        std::io::copy(&mut from, file).map(|_| ())
    });
    let _ = std::fs::remove_file(&staged);
    let dest = moved.map_err(|e| e.replace("复制失败", "粘贴失败"))?;
    Ok(format!(
        "ok:{}",
        dest.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
}

/// 读系统剪贴板落盘到工作区目录（安卓）：文本 →「剪贴板-时间.txt」，
/// content:// URI（文件管理器的「复制」）→ 原名文件。微信/QQ 不写系统
/// 文件剪贴板，从它们导入请用「分享到 RClaw」通道。
#[tauri::command]
pub async fn workspace_paste_from_clipboard(
    state: State<'_, AppState>,
    dir: String,
) -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        let home = state.home.clone();
        tauri::async_runtime::spawn_blocking(move || {
            let base = resolve_workspace_dir(&home.join("agent"), &dir)?;
            // Java 先落到工作区【外】的暂存目录，Rust 校验名字后再搬进目标目录：保留名
            // （state.sqlite-journal 这类假 hot journal）从不在工作区根出现，哪怕一瞬间。
            let staging = home.join(PASTE_STAGING_DIR);
            std::fs::create_dir_all(&staging).map_err(|e| format!("创建暂存目录失败: {e}"))?;
            let result = rclaw_android::read_clipboard_to_file(&staging.to_string_lossy())
                .map_err(|e| format!("读取剪贴板失败: {e}"))?;
            if result == "empty" {
                return Err("剪贴板没有文本或文件（微信/QQ 内容请用「分享到 RClaw」导入）".into());
            }
            if let Some(reason) = result.strip_prefix("error:") {
                return Err(reason.to_string());
            }
            let at_root = home
                .join("agent")
                .canonicalize()
                .map(|root| root == base)
                .unwrap_or(dir.is_empty());
            admit_pasted_file(&home, &base, at_root, &staging, &result)
        })
        .await
        .map_err(|e| format!("粘贴任务失败: {e}"))?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (state, dir);
        Err("粘贴系统剪贴板仅安卓支持（桌面请用「分享到 RClaw」或直接上传）".into())
    }
}

/// 把工作区文件以 FileProvider URI 写入系统剪贴板（安卓）：文件管理器长按
/// 「粘贴」可取出；微信/QQ 不接收系统文件剪贴板，请用「转发到其它应用」。
#[tauri::command]
pub async fn workspace_copy_to_clipboard(
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        let home = state.home.clone();
        tauri::async_runtime::spawn_blocking(move || {
            // 主配置虚拟条目：以 home 根实体的绝对路径进 FileProvider。
            let abs = if is_config_virtual(&home.join("agent"), &path) {
                home.join(CONFIG_NAME)
            } else {
                // 跟随链接取真实文件；落点越区 / 运行时数据库 / 目录都拒绝。
                let (base, entry) = resolve_workspace_entry(&home.join("agent"), &path)?;
                resolve_workspace_file_content(&base, &entry)?
            };
            rclaw_android::copy_file_to_clipboard(&abs.to_string_lossy())
                .map_err(|e| format!("复制到系统剪贴板失败: {e}"))
        })
        .await
        .map_err(|e| format!("复制任务失败: {e}"))?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (state, path);
        Err("复制到系统剪贴板仅安卓支持".into())
    }
}

// ---------------------------------------------------------------------------
// 系统分享接收：Java 侧（ShareReceiver）把分享内容落盘并写 files/pending_share.json，
// 这里消费（读后即删）：GUI 启动/就绪时取一次，开新会话预填附件引用。
// ---------------------------------------------------------------------------

/// 待消费的系统分享。
#[derive(Debug, Serialize)]
pub struct PendingShareDto {
    /// 相对 agent 存储根的已落盘文件（uploads/...）。
    pub files: Vec<String>,
    /// 分享附带的文本（微信「发送给朋友」的留言等）。
    pub text: Option<String>,
}

#[tauri::command]
pub async fn consume_pending_share(state: State<'_, AppState>) -> Result<Option<PendingShareDto>, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = home.join("pending_share.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        // 读到即删：分享只消费一次（解析失败也删，防坏文件永久卡消费）。
        let _ = std::fs::remove_file(&path);
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("pending_share.json 解析失败: {e}"))?;
        let files: Vec<String> = value
            .get("files")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();
        let text = value
            .get("text")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .filter(|s| !s.trim().is_empty());
        if files.is_empty() && text.is_none() {
            return Ok(None);
        }
        Ok(Some(PendingShareDto { files, text }))
    })
    .await
    .map_err(|e| format!("消费分享任务失败: {e}"))?
}

// ---------------------------------------------------------------------------
// 会话采集（拍照/拍视频/录音）与 whisper 语音转写。
// 采集结果由 Java 侧（CaptureBridge）写 files/capture_result.json（单消费），
// GUI 在采集期间轮询 consume_capture_result 取结果。
// ---------------------------------------------------------------------------

/// 待消费的采集结果。
#[derive(Debug, Serialize)]
pub struct CaptureResultDto {
    /// photo | video。
    pub kind: String,
    /// 相对 agent/ 的落盘文件（取消时为 None）。
    pub file: Option<String>,
}

#[tauri::command]
pub async fn capture_start(state: State<'_, AppState>, kind: String) -> Result<String, String> {
    if kind != "photo" && kind != "video" {
        return Err("不支持的采集类型".into());
    }
    // 清掉上一次残留结果，防止旧结果被本轮误消费。
    let _ = std::fs::remove_file(state.home.join("capture_result.json"));
    #[cfg(target_os = "android")]
    {
        // "ok" / "permission_requested"（授权后自动续跑）/ "error:<原因>"。
        let status = rclaw_android::capture(&kind).map_err(|e| format!("无法启动相机: {e}"))?;
        if let Some(reason) = status.strip_prefix("error:") {
            return Err(format!("相机不可用: {reason}"));
        }
        Ok(status)
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("拍照/录像仅 Android 应用支持".into())
    }
}

#[tauri::command]
pub async fn consume_capture_result(
    state: State<'_, AppState>,
) -> Result<Option<CaptureResultDto>, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let path = home.join("capture_result.json");
        let Ok(raw) = std::fs::read_to_string(&path) else {
            return Ok(None);
        };
        // 读到即删：结果只消费一次。
        let _ = std::fs::remove_file(&path);
        let value: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| format!("capture_result.json 解析失败: {e}"))?;
        let kind = value
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if value.get("cancelled").and_then(|v| v.as_bool()).unwrap_or(false) {
            return Ok(Some(CaptureResultDto { kind, file: None }));
        }
        let file = value
            .get("file")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Ok(Some(CaptureResultDto { kind, file }))
    })
    .await
    .map_err(|e| format!("消费采集结果任务失败: {e}"))?
}

#[tauri::command]
pub async fn record_start() -> Result<String, String> {
    #[cfg(target_os = "android")]
    {
        let status = rclaw_android::record_start().map_err(|e| format!("录音启动失败: {e}"))?;
        Ok(status)
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("录音仅 Android 应用支持".into())
    }
}

#[tauri::command]
pub async fn record_stop() -> Result<Option<String>, String> {
    #[cfg(target_os = "android")]
    {
        let rel = rclaw_android::record_stop().map_err(|e| format!("录音停止失败: {e}"))?;
        if rel.is_empty() {
            return Ok(None);
        }
        Ok(Some(rel))
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("录音仅 Android 应用支持".into())
    }
}

/// 原生侧是否正在录音（GUI 授权轮询用：授权后原生自动开录，GUI 据此亮出录音条）。
#[tauri::command]
pub async fn record_active() -> Result<bool, String> {
    #[cfg(target_os = "android")]
    {
        rclaw_android::record_active().map_err(|e| format!("查询录音状态失败: {e}"))
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(false)
    }
}

#[tauri::command]
pub async fn record_cancel() -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        rclaw_android::record_cancel().map_err(|e| format!("录音取消失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        Err("录音仅 Android 应用支持".into())
    }
}

/// 工作区相对路径校验 + 绝对路径解析（只读用途：保存到相册 / 转写等，目标须已存在
/// 且落在 agent 根内）。会删文件的入口用 [`resolve_upload_path`]。
///
/// 与文件面板同一套路径校验（[`validate_workspace_rel_path`]）；运行时数据库不经任何
/// 宿主命令读取 / 复制——哪怕只是「保存到相册」，没有 -wal 的快照也是一份不一致的库。
fn resolve_agent_path(
    home: &std::path::Path,
    rel: &str,
) -> Result<std::path::PathBuf, String> {
    validate_workspace_rel_path(rel).map_err(|_| "非法文件路径".to_string())?;
    let root = home.join("agent").canonicalize().map_err(|e| format!("工作区不可用: {e}"))?;
    let canonical = root
        .join(rel)
        .canonicalize()
        .map_err(|e| format!("文件不存在: {e}"))?;
    if !canonical.starts_with(&root) {
        return Err("路径越出工作区".into());
    }
    if is_state_db_entry(&root, &canonical) {
        return Err(STATE_DB_PROTECTED.into());
    }
    Ok(canonical)
}

/// 把采集/编辑产物保存到系统相册（安卓 MediaStore）。返回 "ok" /
/// "permission_requested"（引导用户授权后重试）。
#[tauri::command]
pub async fn save_to_gallery(
    state: State<'_, AppState>,
    rel: String,
    kind: String,
) -> Result<String, String> {
    if !matches!(kind.as_str(), "photo" | "video" | "audio") {
        return Err("不支持的媒体类型".into());
    }
    let home = state.home.clone();
    // 路径解析（canonicalize）、MediaStore 写入 / 桌面文件拷贝都是同步 IO，放阻塞线程池。
    run_blocking("保存到相册", move || {
        // 采集 / 编辑产物都落在 uploads/（上传、拍摄、photo-edited-*），与「放弃采集」同一
        // 口径只认 uploads/ 下的文件：工作区别处的文件（更不用说根下的 state.sqlite）不经
        // 此入口复制到相册。
        let abs = resolve_upload_path(&home, &rel)?;
        let abs = abs.to_string_lossy().to_string();
        #[cfg(target_os = "android")]
        {
            let result = rclaw_android::save_to_gallery(&abs, &kind)
                .map_err(|e| format!("保存到相册失败: {e}"))?;
            if let Some(reason) = result.strip_prefix("error:") {
                return Err(reason.to_string());
            }
            Ok(result)
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = kind;
            // 桌面兜底：拷贝到用户图片目录的 RClaw 子目录。
            let base = std::env::var("USERPROFILE")
                .map(|p| std::path::PathBuf::from(p).join("Pictures"))
                .unwrap_or_else(|_| home.clone());
            let dir = base.join("RClaw");
            std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
            std::fs::copy(
                &abs,
                dir.join(std::path::Path::new(&abs).file_name().unwrap_or_default()),
            )
            .map_err(|e| format!("保存失败: {e}"))?;
            Ok("ok".into())
        }
    })
    .await
}

/// 采集产物路径：在 [`resolve_agent_path`] 之上再要求落在 `uploads/` 内。放弃采集是
/// 一个删除入口、保存到相册是一个复制出去的入口，都只允许碰这里的临时文件——工作区
/// 别处的文件（包括根下的 state.sqlite）不经这两个入口删除 / 复制；按解析后的真实路径
/// 判断，`./uploads/..` 等拼写不影响。
fn resolve_upload_path(
    home: &std::path::Path,
    rel: &str,
) -> Result<std::path::PathBuf, String> {
    let canonical = resolve_agent_path(home, rel)?;
    let uploads = home
        .join("agent")
        .join("uploads")
        .canonicalize()
        .map_err(|e| format!("uploads 目录不可用: {e}"))?;
    if canonical == uploads || !canonical.starts_with(&uploads) {
        return Err("只能操作 uploads/ 下的采集产物".into());
    }
    Ok(canonical)
}

/// 放弃采集产物：删除 uploads/ 里的临时文件（用户点了取消）。
#[tauri::command]
pub async fn discard_capture(state: State<'_, AppState>, rel: String) -> Result<(), String> {
    let home = state.home.clone();
    // 三次 canonicalize + 删除都是同步文件系统调用，与其它文件命令一样下放阻塞线程。
    run_blocking("放弃采集产物", move || {
        let abs = resolve_upload_path(&home, &rel)?;
        match std::fs::remove_file(&abs) {
            Ok(()) => Ok(()),
            // 解析后到删除之间被别处清掉：目标状态已达成，不算失败。
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("删除采集产物失败: {e}")),
        }
    })
    .await
}

/// 编辑后的照片落盘（前端画布导出的 JPEG base64）。返回相对 agent/ 的路径。
#[tauri::command]
pub async fn save_edited_photo(
    state: State<'_, AppState>,
    data_base64: String,
) -> Result<String, String> {
    use base64::Engine;
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_base64.as_bytes())
            .map_err(|e| format!("照片数据解码失败: {e}"))?;
        if bytes.is_empty() {
            return Err("照片数据为空".into());
        }
        let dir = home.join("agent").join("uploads");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
        // 文件名按毫秒时间戳可预测：事先摆好的同名悬空链接不能让 `fs::write` 顺着写到
        // 工作区外——用不跟随链接的 CREATE_NEW 创建，撞上已有条目换序号重试。
        let stamp = chrono_like_ts();
        let path = write_fresh_file(
            &dir,
            "写盘失败",
            |n| {
                if n == 1 {
                    format!("photo-edited-{stamp}.jpg")
                } else {
                    format!("photo-edited-{stamp}-{n}.jpg")
                }
            },
            |file| std::io::Write::write_all(file, &bytes),
        )?;
        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(format!("uploads/{file_name}"))
    })
    .await
    .map_err(|e| format!("保存编辑照片任务失败: {e}"))?
}

/// 毫秒时间戳（本地小工具，避免为命名文件引入时间库依赖）。
fn chrono_like_ts() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 单调序号：毫秒时间戳在同一毫秒内并发会撞名，用它兜底区分。
#[cfg(target_os = "android")]
fn temp_file_seq() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// whisper 语音转写：模型 ggml-base-q5_1（约 57MB，中文可用），首次使用时下载。
// 链路：m4a/aac →（Java MediaCodec）→ 16k WAV →（librclaw_whisper.so）→ 文本。
// ---------------------------------------------------------------------------

const WHISPER_MODEL_FILE: &str = "ggml-base-q5_1.bin";
/// 国内镜像优先，官方源兜底（模型在 ggerganov/whisper.cpp HF 仓库）。
const WHISPER_MODEL_URLS: [&str; 2] = [
    "https://hf-mirror.com/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin",
    "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base-q5_1.bin",
];

#[derive(Debug, Serialize)]
pub struct WhisperModelStatus {
    pub installed: bool,
    pub size: u64,
    pub downloading: bool,
    pub received: u64,
    pub total: u64,
}

#[tauri::command]
pub async fn whisper_model_status(
    state: State<'_, AppState>,
) -> Result<WhisperModelStatus, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let model = home.join("models").join(WHISPER_MODEL_FILE);
        let (installed, size) = match std::fs::metadata(&model) {
            Ok(meta) if meta.len() > 1024 * 1024 => (true, meta.len()),
            _ => (false, 0),
        };
        let mut downloading = false;
        let mut received = 0u64;
        let mut total = 0u64;
        if let Ok(raw) = std::fs::read_to_string(home.join("models/download_status.json")) {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                downloading = matches!(
                    value.get("state").and_then(|v| v.as_str()),
                    Some("downloading") | Some("copying")
                );
                received = value.get("received").and_then(|v| v.as_u64()).unwrap_or(0);
                total = value.get("total").and_then(|v| v.as_u64()).unwrap_or(0);
            }
        }
        Ok(WhisperModelStatus { installed, size, downloading, received, total })
    })
    .await
    .map_err(|e| format!("读取模型状态任务失败: {e}"))?
}

#[tauri::command]
pub async fn whisper_model_download(state: State<'_, AppState>) -> Result<(), String> {
    // 已装好或已有下载在跑就不再触发（downloadModel 幂等，UI 侧也轮询状态）。
    let model = state.home.join("models").join(WHISPER_MODEL_FILE);
    if model.is_file() {
        return Ok(());
    }
    let status_path = state.home.join("models/download_status.json");
    if let Ok(raw) = std::fs::read_to_string(&status_path) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
            // copying = 从 APK assets 复制 57MB 模型中（也是"进行中"）：不拦的话
            // 双击下载会起两个线程写同一 .part，rename 后得到损坏模型。
            // 状态超龄（复制/下载中途进程被杀会永久残留）视为失效放行重下，
            // 否则之后每次下载都被守卫静默吞掉、模型永远装不上。
            let in_progress = matches!(
                value.get("state").and_then(|v| v.as_str()),
                Some("downloading") | Some("copying")
            );
            let stale = std::fs::metadata(&status_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(600));
            if in_progress && !stale {
                return Ok(());
            }
        }
    }
    let urls: Vec<&str> = WHISPER_MODEL_URLS.to_vec();
    #[cfg(target_os = "android")]
    {
        rclaw_android::whisper_model_download(&urls, WHISPER_MODEL_FILE)
            .map_err(|e| format!("启动模型下载失败: {e}"))?;
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = urls;
        Err("模型下载仅 Android 应用支持".into())
    }
}

/// 转写工作区音频文件（uploads/...），返回识别文本。阻塞推理在阻塞线程池跑。
#[tauri::command]
pub async fn whisper_transcribe(
    state: State<'_, AppState>,
    rel: String,
    language: Option<String>,
) -> Result<String, String> {
    let home = state.home.clone();
    let audio = resolve_agent_path(&home, &rel)?;
    let model = home.join("models").join(WHISPER_MODEL_FILE);
    if !model.is_file() {
        return Err("whisper 模型未下载（先在录音面板下载模型）".into());
    }
    let lang = language.filter(|s| !s.trim().is_empty()).unwrap_or_else(|| "auto".into());
    #[cfg(target_os = "android")]
    {
        tauri::async_runtime::spawn_blocking(move || {
            let wav = home.join(format!("whisper-tmp-{}-{}.wav", chrono_like_ts(), temp_file_seq()));
            let result = (|| -> Result<String, String> {
                let decode = rclaw_android::decode_to_wav(
                    &audio.to_string_lossy(),
                    &wav.to_string_lossy(),
                )
                .map_err(|e| format!("音频解码调用失败: {e}"))?;
                if decode != "ok" {
                    return Err(decode.trim_start_matches("error:").to_string());
                }
                if !rclaw_android::load_whisper_lib()
                    .map_err(|e| format!("加载 whisper 库失败: {e}"))?
                {
                    return Err("whisper 推理库未打包（librclaw_whisper.so 缺失）".into());
                }
                let raw = rclaw_android::whisper_transcribe(
                    &model.to_string_lossy(),
                    &wav.to_string_lossy(),
                    &lang,
                )
                .map_err(|e| format!("转写调用失败: {e}"))?;
                if let Some(reason) = raw.strip_prefix("error:") {
                    return Err(reason.to_string());
                }
                Ok(raw)
            })();
            // 所有路径（含失败）都清掉临时 WAV，不再残留在 filesDir 根。
            let _ = std::fs::remove_file(&wav);
            result
        })
        .await
        .map_err(|e| format!("转写任务失败: {e}"))?
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (audio, model, lang);
        Err("whisper 转写仅 Android 应用支持".into())
    }
}

#[cfg(test)]
mod plan_projection_tests {
    use super::*;

    fn event(kind: &str, fields: serde_json::Value) -> (u64, serde_json::Value) {
        let mut value = serde_json::json!({ "event": kind });
        if let (Some(obj), Some(extra)) = (value.as_object_mut(), fields.as_object()) {
            for (key, field) in extra {
                obj.insert(key.clone(), field.clone());
            }
        }
        (1, value)
    }

    fn blank_plan() -> PlanDto {
        PlanDto {
            plan_id: "p".into(),
            name: "p".into(),
            completed: false,
            paused: false,
            succeeded: None,
            updated_at: 0,
            steps: Vec::new(),
        }
    }

    #[test]
    fn projection_covers_step_lifecycle_and_legacy_step_shapes() {
        let mut plan = blank_plan();
        let events = vec![
            event(
                "plan_started",
                serde_json::json!({
                    "name": "巡检",
                    "steps": [
                        {"id": "a", "task": "收集"},
                        {"id": "b", "task": "汇总"}
                    ]
                }),
            ),
            event(
                "step_finished",
                serde_json::json!({"step": "a", "outcome": "succeeded", "summary": "完成"}),
            ),
            event("step_skipped", serde_json::json!({"step": "b", "reason": "依赖未成功"})),
            event("plan_completed", serde_json::json!({"succeeded": 1})),
        ];
        project_plan_events(&mut plan, &events);
        assert_eq!(plan.name, "巡检");
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[0].status, "succeeded");
        assert_eq!(plan.steps[0].summary, "完成");
        assert_eq!(plan.steps[1].status, "skipped");
        assert!(plan.completed);
        assert_eq!(plan.succeeded, Some(1));

        // 旧版 journal 的 steps 为 id 数组。
        let mut legacy = blank_plan();
        project_plan_events(
            &mut legacy,
            &[event(
                "plan_started",
                serde_json::json!({"steps": ["x", "y"]}),
            )],
        );
        assert_eq!(legacy.steps.len(), 2);
        assert_eq!(legacy.steps[0].id, "x");
        assert_eq!(legacy.steps[0].status, "pending");
    }

    #[test]
    fn projection_marks_started_steps_running_until_terminal() {
        let mut plan = blank_plan();
        project_plan_events(
            &mut plan,
            &[
                (
                    1,
                    serde_json::json!({
                        "ts": 1000,
                        "event": "plan_started",
                        "steps": [{"id": "a", "task": "t"}, {"id": "b", "task": "u"}]
                    }),
                ),
                (2, serde_json::json!({"ts": 1100, "event": "step_started", "step": "a"})),
                (3, serde_json::json!({"ts": 1200, "event": "step_started", "step": "b"})),
            ],
        );
        assert_eq!(plan.steps[0].status, "running");
        assert_eq!(plan.steps[1].status, "running");
        // 终态覆盖运行中；重跑（started 在 finished 后）重新置 running。
        project_plan_events(
            &mut plan,
            &[(4, serde_json::json!({"ts": 1300, "event": "step_finished", "step": "a", "outcome": "succeeded"}))],
        );
        assert_eq!(plan.steps[0].status, "succeeded");
        project_plan_events(
            &mut plan,
            &[(5, serde_json::json!({"ts": 1400, "event": "step_started", "step": "a"}))],
        );
        assert_eq!(plan.steps[0].status, "running");
    }

    #[test]
    fn projection_reads_rfc3339_and_millisecond_timestamps() {
        let mut plan = blank_plan();
        project_plan_events(
            &mut plan,
            &[
                (
                    1,
                    serde_json::json!({
                        "ts": "2026-10-13T13:44:37.113778700+00:00",
                        "event": "plan_started",
                        "steps": [{"id": "a", "task": "t"}]
                    }),
                ),
                (
                    2,
                    serde_json::json!({"ts": 1791899277000i64, "event": "plan_paused"}),
                ),
            ],
        );
        // RFC3339（执行器形态）与毫秒 int（手拼形态）都应推进 updated_at；
        // 后到的毫秒值更大，排序取最新。
        assert_eq!(plan.updated_at, 1_791_899_277_000);

        // 无法解析的 ts 不清零已有值。
        project_plan_events(
            &mut plan,
            &[(3, serde_json::json!({"ts": "not-a-time", "event": "plan_resumed"}))],
        );
        assert_eq!(plan.updated_at, 1_791_899_277_000);
        assert!(!plan.paused);
    }

    #[test]
    fn projection_flips_paused_and_resumed_in_event_order() {
        // 回归：plan_resumed 曾被 plan_started 的合并分支吞掉（clippy unreachable_patterns
        // 揪出）——暂停后恢复必须翻得回 false。
        let mut plan = blank_plan();
        let events = vec![
            event("plan_started", serde_json::json!({"name": "n", "steps": [{"id": "a", "task": "t"}]})),
            event("plan_paused", serde_json::json!({})),
            event("plan_resumed", serde_json::json!({})),
        ];
        project_plan_events(&mut plan, &events);
        assert!(!plan.paused, "恢复事件必须清除暂停态");

        // 波次边界恢复（UI / 执行器写 plan_unpaused）：同样翻回非暂停。
        let mut held = blank_plan();
        project_plan_events(
            &mut held,
            &[
                event("plan_started", serde_json::json!({"steps": [{"id": "a", "task": "t"}]})),
                event("plan_paused", serde_json::json!({})),
                event("plan_unpaused", serde_json::json!({})),
            ],
        );
        assert!(!held.paused, "plan_unpaused 必须清除暂停态");
        assert_eq!(held.steps[0].status, "pending");

        // 最后一条是暂停 → paused；收尾事件强制清除暂停（收尾后暂停无意义）。
        let mut paused = blank_plan();
        project_plan_events(&mut paused, &[event("plan_paused", serde_json::json!({}))]);
        assert!(paused.paused);
        project_plan_events(
            &mut paused,
            &[event("plan_completed", serde_json::json!({"succeeded": 0}))],
        );
        assert!(!paused.paused && paused.completed);
    }

    #[test]
    fn projection_tracks_verification_lifecycle() {
        // 目标执行成功但等待 verdict → verifying（不是 ✓）；verdict 判定终局。
        let mut plan = blank_plan();
        project_plan_events(
            &mut plan,
            &[
                event("plan_started", serde_json::json!({"steps": [{"id": "t", "task": "干活"}, {"id": "v", "task": "验证"}]})),
                event("step_finished", serde_json::json!({"step": "t", "outcome": "succeeded", "awaiting_verdict": true, "summary": "产出"})),
            ],
        );
        assert_eq!(plan.steps[0].status, "verifying", "awaiting_verdict 不是终态成功");

        // 未耗尽：目标带失败原因回 pending（执行器重跑）。
        project_plan_events(
            &mut plan,
            &[event("verdict_failed", serde_json::json!({"verifier": "v", "target": "t", "round": 1, "reasons": ["缺测试", "断言错"]}))],
        );
        assert_eq!(plan.steps[0].status, "pending");
        assert!(plan.steps[0].summary.contains("缺测试"), "{}", plan.steps[0].summary);

        // 重跑 → verifying → 轮次耗尽：目标 failed（验证者自己的 step_finished 不覆盖目标）。
        project_plan_events(
            &mut plan,
            &[
                event("step_started", serde_json::json!({"step": "t"})),
                event("step_finished", serde_json::json!({"step": "t", "outcome": "succeeded", "awaiting_verdict": true})),
                event("verdict_failed", serde_json::json!({"verifier": "v", "target": "t", "round": 2, "exhausted": true, "reasons": ["仍缺测试"]})),
            ],
        );
        assert_eq!(plan.steps[0].status, "failed", "轮次耗尽必须落 failed");

        // pass → 目标 succeeded（verdict_passed 是目标的终局事实源）。
        project_plan_events(
            &mut plan,
            &[
                event("step_started", serde_json::json!({"step": "t"})),
                event("step_finished", serde_json::json!({"step": "t", "outcome": "succeeded", "awaiting_verdict": true})),
                event("verdict_passed", serde_json::json!({"verifier": "v", "target": "t", "round": 3})),
            ],
        );
        assert_eq!(plan.steps[0].status, "succeeded");

        // 验证者不可用（step_unverified）：目标终局 failed，不是 skipped。
        let mut unverified = blank_plan();
        project_plan_events(
            &mut unverified,
            &[
                event("plan_started", serde_json::json!({"steps": [{"id": "t", "task": "t"}]})),
                event("step_finished", serde_json::json!({"step": "t", "outcome": "succeeded", "awaiting_verdict": true})),
                event("step_unverified", serde_json::json!({"step": "t", "verifier": "v", "reason": "验证者不可用"})),
            ],
        );
        assert_eq!(unverified.steps[0].status, "failed");
    }

    #[test]
    fn projection_resumes_generation_resets_completion_and_inherits_success() {
        // 第一代：a 成功、b 失败、整体收尾。
        let mut plan = blank_plan();
        project_plan_events(
            &mut plan,
            &[
                event("plan_started", serde_json::json!({"name": "巡检", "steps": [{"id": "a", "task": "收集"}, {"id": "b", "task": "汇总"}]})),
                event("step_finished", serde_json::json!({"step": "a", "outcome": "succeeded", "summary": "数据就绪"})),
                event("step_finished", serde_json::json!({"step": "b", "outcome": "failed", "summary": "源挂了"})),
                event("plan_completed", serde_json::json!({"succeeded": 1})),
            ],
        );
        assert!(plan.completed);

        // resume（同一 journal 流上的第二个 plan_started + plan_resumed）：
        // 整体收尾态复位（否则恢复中的计划永远「已完成」，todo 与暂停按钮全消失）；
        // a 按 resume 语义继承 succeeded（执行器不重跑也不重报），b 回 pending。
        project_plan_events(
            &mut plan,
            &[
                event("plan_started", serde_json::json!({"name": "巡检", "steps": [{"id": "a", "task": "收集"}, {"id": "b", "task": "汇总改"}]})),
                event("plan_resumed", serde_json::json!({})),
            ],
        );
        assert!(!plan.completed, "resume 必须复位 completed");
        assert_eq!(plan.succeeded, None);
        assert!(!plan.paused);
        assert_eq!(plan.steps[0].status, "succeeded", "上代成功步骤继承");
        assert_eq!(plan.steps[0].summary, "数据就绪");
        assert_eq!(plan.steps[0].task, "收集");
        assert_eq!(plan.steps[1].status, "pending", "失败步骤重跑");
        assert_eq!(plan.steps[1].task, "汇总改");

        // verifying 的目标同样跨代保留（resume 只补跑验证者）。
        let mut verifying = blank_plan();
        project_plan_events(
            &mut verifying,
            &[
                event("plan_started", serde_json::json!({"steps": [{"id": "t", "task": "t"}, {"id": "v", "task": "v"}]})),
                event("step_finished", serde_json::json!({"step": "t", "outcome": "succeeded", "awaiting_verdict": true})),
                event("plan_started", serde_json::json!({"steps": [{"id": "t", "task": "t"}, {"id": "v", "task": "v"}]})),
            ],
        );
        assert_eq!(verifying.steps[0].status, "verifying", "等验证者的目标跨代保留");
        assert_eq!(verifying.steps[1].status, "pending");
    }
}
