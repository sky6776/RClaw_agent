//! Tauri command 层：前端与核心之间的唯一通道。
//!
//! 命令面刻意保持最小——对话消息（含斜杠命令）统一走 inbound 总线，
//! 配置类操作（/llmodel 等）复用既有 slash 管道，不另开配置命令。
//! 多会话以 chat_id 区分（同一 desktop 通道，TurnManager 按 (channel, chat_id)
//! 各自维护活动 turn）；归档 = 移动会话文件到 `sessions/archived/`。

use std::time::UNIX_EPOCH;

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

/// 仓库 coordinates：releases 以 GitHub Release 发布（tag 形如 v0.2.0）。
const UPDATE_REPO_API: &str = "https://api.github.com/repos/sky6776/RClaw/releases/latest";

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
    /// 无自定义名时 = 首条用户消息截断（读取会话文件头部提取）。
    pub title: String,
    /// 最后修改时间（unix 毫秒）。
    pub updated_at: u64,
    pub archived: bool,
}

/// 归档目录名（挂在 sessions/ 之下；列表扫描天然跳过子目录）。
const ARCHIVED_DIR: &str = "archived";

#[tauri::command]
pub async fn chat_list(state: State<'_, AppState>) -> Result<Vec<ChatDto>, String> {
    let session = state.session().ok_or("agent 尚未启动")?;
    let dir = session.sessions_dir();
    // 用户自定义会话名（desktop_prefs.json）优先于派生标题。
    let custom_titles = agent::read_chat_titles(&state.home);
    let mut out = Vec::new();
    for (archived, scan_dir) in [(false, dir.clone()), (true, dir.join(ARCHIVED_DIR))] {
        let Ok(entries) = std::fs::read_dir(&scan_dir) else {
            continue;
        };
        let prefix = format!("{}_", rclaw_store::percent_encode_key(CHANNEL_DESKTOP));
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let Some(rest) = name
                .strip_prefix(&prefix)
                .and_then(|r| r.strip_suffix(".jsonl"))
            else {
                continue;
            };
            let chat_id = percent_encoding::percent_decode_str(rest)
                .decode_utf8_lossy()
                .to_string();
            // Bot Mode：群成员会话（grp-…）是编排内部会话，不进用户会话列表。
            if chat_id.starts_with("grp-") {
                continue;
            }
            let (title, updated_at) = {
                let meta = entry.metadata().ok();
                let updated_at = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0);
                let mut derived = extract_title(&entry.path());
                // Bot Mode：bot 收件箱的首条「用户」消息往往是队友交接信封
                // （Message from 🤖 …），原文当标题不可读；改用 bot 显示名。
                if let Some(name) = chat_id.strip_prefix("bot-") {
                    if let Some(def) = state
                        .layout()
                        .and_then(|layout| rclaw_store::BotStore::new(&layout.base).get(name).ok())
                    {
                        derived = format!("🤖 {}", def.title);
                    }
                }
                let title = custom_titles.get(&chat_id).cloned().unwrap_or(derived);
                (title, updated_at)
            };
            out.push(ChatDto {
                chat_id,
                title,
                updated_at,
                archived,
            });
        }
    }
    out.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    Ok(out)
}

/// 重命名会话（双击 / 右键菜单触发行内编辑）。标题去空白后为空 = 恢复自动派生标题。
/// 仅写 desktop_prefs.json 的 chat_titles 映射，不动会话文件。
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
    agent::write_chat_title(&state.home, &chat_id, &title)
}

/// 导出完整聊天记录为 Markdown 文本（不做条数裁剪，前端触发下载）。
/// 直接读会话 JSONL 原文，绕过 load_history_view 的 200 条窗口。
#[tauri::command]
pub async fn chat_export(state: State<'_, AppState>, chat_id: String) -> Result<String, String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    let path = session.session_path_pub(CHANNEL_DESKTOP, &chat_id);
    use std::io::BufRead;
    let file = std::fs::File::open(&path).map_err(|e| format!("读取会话失败: {e}"))?;
    let reader = std::io::BufReader::new(file);
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
    for line in reader.lines().map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<rclaw_core::Message>(&line) else {
            continue;
        };
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

/// 从会话文件头部提取标题：首条非压缩摘要的用户文本，截 60 字符。
fn extract_title(path: &std::path::Path) -> String {
    use std::io::BufRead;
    const CONTEXT_SUMMARY_PREFIX: &str = "[CONTEXT COMPACTION";
    let Ok(file) = std::fs::File::open(path) else {
        return String::new();
    };
    let reader = std::io::BufReader::new(file);
    for line in reader.lines().take(64).map_while(Result::ok) {
        let Ok(msg) = serde_json::from_str::<rclaw_core::Message>(&line) else {
            continue;
        };
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

/// 历史消息条目（恢复转录用）。
#[derive(Debug, Serialize)]
pub struct HistoryMsgDto {
    /// user | assistant | status（工具调用轮）。
    pub role: String,
    pub content: String,
}

/// 读取指定会话的持久化历史（最近 200 条，经 repair / 裁剪），映射为转录条目。
#[tauri::command]
pub async fn chat_history(
    state: State<'_, AppState>,
    chat_id: String,
) -> Result<Vec<HistoryMsgDto>, String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    // 只读视图：目标会话可能正有 turn 在追加，普通 load_history 的压缩写回会与之竞争。
    let msgs = session
        .load_history_view(CHANNEL_DESKTOP, &chat_id, 200, 4 * 1024 * 1024)
        .map_err(|e| e.to_string())?;
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

/// 归档会话：把会话文件移入 `sessions/archived/`（恢复 = 移回）。
#[tauri::command]
pub async fn chat_archive(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    move_chat(&state, &chat_id, true)
}

/// 恢复归档会话。
#[tauri::command]
pub async fn chat_restore(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    move_chat(&state, &chat_id, false)
}

fn move_chat(state: &AppState, chat_id: &str, archive: bool) -> Result<(), String> {
    if !is_valid_chat_id(chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    move_session_file(&session, chat_id, archive)
}

/// 归档 / 恢复的核心：只做会话文件移动，与 Tauri 状态解耦便于测试。
fn move_session_file(
    session: &rclaw_store::session::SessionStore,
    chat_id: &str,
    archive: bool,
) -> Result<(), String> {
    let dir = session.sessions_dir();
    let from = if archive {
        session.session_path_pub(CHANNEL_DESKTOP, chat_id)
    } else {
        dir.join(ARCHIVED_DIR).join(
            session
                .session_path_pub(CHANNEL_DESKTOP, chat_id)
                .file_name()
                .ok_or("非法文件名")?,
        )
    };
    let to_dir = if archive { dir.join(ARCHIVED_DIR) } else { dir };
    // /clear 会在 UI 列表刷新前删掉会话文件；文件已不在 = 无需移动，视为成功，
    // 让前端刷新列表清掉幽灵条目，而不是报错把条目卡死。
    if !from.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(&to_dir).map_err(|e| e.to_string())?;
    let file_name = from.file_name().ok_or("非法文件名")?;
    let to = to_dir.join(file_name);
    if to.exists() {
        return Err("目标位置已存在同名会话文件".into());
    }
    std::fs::rename(&from, &to).map_err(|e| format!("移动会话文件失败: {e}"))?;
    Ok(())
}

/// 删除会话：删掉会话文件（活动与已归档两处都清）并移除自定义标题。
/// 正在工作的会话由前端拦截（turn 收尾还会向原路径追加）；文件已不在 = 幂等成功。
#[tauri::command]
pub async fn chat_delete(state: State<'_, AppState>, chat_id: String) -> Result<(), String> {
    if !is_valid_chat_id(&chat_id) {
        return Err(format!("非法会话 id: {chat_id}"));
    }
    let session = state.session().ok_or("agent 尚未启动")?;
    delete_session_files(&session, &chat_id)?;
    // 自定义标题随会话一起清掉（空标题 = 删除该键）。
    agent::write_chat_title(&state.home, &chat_id, "")
}

/// 删除的核心：只做会话文件删除（活动 + 归档两处），与 Tauri 状态解耦便于测试。
fn delete_session_files(
    session: &rclaw_store::session::SessionStore,
    chat_id: &str,
) -> Result<(), String> {
    let dir = session.sessions_dir();
    let name = session
        .session_path_pub(CHANNEL_DESKTOP, chat_id)
        .file_name()
        .ok_or("非法文件名")?
        .to_owned();
    for path in [dir.join(&name), dir.join(ARCHIVED_DIR).join(&name)] {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| format!("删除会话文件失败: {e}"))?;
        }
    }
    Ok(())
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
    /// 请求压力估算（历史 + system prompt + 动态上下文 + 工具 schema），
    /// 与自动压缩触发同口径。
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
    // 有界扫描（≤200 文件 × 32MB）放到阻塞线程池，不卡 async 运行时。
    tauri::async_runtime::spawn_blocking(move || {
        crate::insights::compute(&session.sessions_dir(), &layout.skills_dir, &memory)
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
        let file_name = format!(
            "{}-{:04x}_{safe}",
            chrono::Local::now().format("%Y%m%d-%H%M%S"),
            rand::random::<u16>(),
        );
        let path = dir.join(&file_name);
        std::fs::write(&path, &bytes).map_err(|e| format!("写入上传文件失败: {e}"))?;
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

    #[test]
    fn move_session_file_tolerates_missing_session_file() {
        let dir = tempfile::tempdir().unwrap();
        let session = rclaw_store::session::SessionStore::new(rclaw_store::fs::Layout::new(
            dir.path().join("agent"),
        ));
        // /clear 删除会话文件后 UI 仍有幽灵条目：归档/恢复都应幂等成功，
        // 让前端刷新列表清掉条目，而不是报「会话文件不存在」。
        assert!(super::move_session_file(&session, "main", true).is_ok());
        assert!(super::move_session_file(&session, "main", false).is_ok());
    }

    #[test]
    fn move_session_file_archives_existing_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = rclaw_store::session::SessionStore::new(rclaw_store::fs::Layout::new(
            dir.path().join("agent"),
        ));
        let from = session.session_path_pub(super::CHANNEL_DESKTOP, "main");
        std::fs::create_dir_all(from.parent().unwrap()).unwrap();
        std::fs::write(&from, "{}\n").unwrap();

        super::move_session_file(&session, "main", true).unwrap();
        let archived = session
            .sessions_dir()
            .join(super::ARCHIVED_DIR)
            .join(from.file_name().unwrap());
        assert!(!from.exists());
        assert!(archived.exists());

        // 文件已移走后再归档同一会话 → 幂等成功。
        assert!(super::move_session_file(&session, "main", true).is_ok());

        super::move_session_file(&session, "main", false).unwrap();
        assert!(from.exists());
        assert!(!archived.exists());
    }

    #[test]
    fn delete_session_files_removes_active_and_archived() {
        let dir = tempfile::tempdir().unwrap();
        let session = rclaw_store::session::SessionStore::new(rclaw_store::fs::Layout::new(
            dir.path().join("agent"),
        ));
        // 活动与归档两处各放一份同名会话文件：一次删除全部清掉；再删幂等成功。
        let active = session.session_path_pub(super::CHANNEL_DESKTOP, "main");
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, "{}\n").unwrap();
        let archived = session
            .sessions_dir()
            .join(super::ARCHIVED_DIR)
            .join(active.file_name().unwrap());
        std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
        std::fs::write(&archived, "{}\n").unwrap();

        assert!(super::delete_session_files(&session, "main").is_ok());
        assert!(!active.exists());
        assert!(!archived.exists());
        assert!(super::delete_session_files(&session, "main").is_ok());
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
        // 穿越 / 反斜杠 / 绝对路径 / 空路径 / 不存在一律拒绝。
        assert!(super::resolve_workspace_entry(&root, "../a.txt").is_err());
        assert!(super::resolve_workspace_entry(&root, "a\\b").is_err());
        assert!(super::resolve_workspace_entry(&root, "/etc/passwd").is_err());
        assert!(super::resolve_workspace_entry(&root, "").is_err());
        assert!(super::resolve_workspace_entry(&root, "nope.txt").is_err());
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

/// 删除 bot 定义与其数据目录（会话文件保留，可手动清理）。
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

/// 解析工作区相对目录（空 = agent 根）：拒绝越区，必须真实存在且是目录。
fn resolve_workspace_dir(root: &std::path::Path, rel: &str) -> Result<std::path::PathBuf, String> {
    if rel.contains("..") || rel.contains('\\') || rel.starts_with('/') {
        return Err("非法目录路径".into());
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

/// 文本编辑上限：1MB（会话 jsonl / 大日志只读浏览，编辑走外部导出）。
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
        if path.contains("..") || path.contains('\\') || path.starts_with('/') {
            return Err("非法文件路径".into());
        }
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
        if path.contains("..") || path.contains('\\') || path.starts_with('/') {
            return Err("非法文件路径".into());
        }
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
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
        }
        std::fs::write(&target, content).map_err(|e| format!("保存失败: {e}"))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("保存任务失败: {e}"))?
}

/// 解析工作区内已存在的文件/目录（copy/delete 共用）：返回 (base, canonical)。
fn resolve_workspace_entry(
    root: &std::path::Path,
    path: &str,
) -> Result<(std::path::PathBuf, std::path::PathBuf), String> {
    if path.is_empty() {
        return Err("路径不能为空".into());
    }
    if path.contains("..") || path.contains('\\') || path.starts_with('/') {
        return Err("非法路径".into());
    }
    let base = root
        .canonicalize()
        .map_err(|e| format!("工作区不可用: {e}"))?;
    let canonical = base
        .join(path)
        .canonicalize()
        .map_err(|e| format!("不存在: {e}"))?;
    if !canonical.starts_with(&base) {
        return Err("路径越出工作区".into());
    }
    Ok((base, canonical))
}

/// 复制工作区文件为同目录副本（自动命名「原名-副本.扩展」，冲突递增）。
#[tauri::command]
pub async fn workspace_copy_file(
    state: State<'_, AppState>,
    path: String,
) -> Result<String, String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        // 主配置虚拟条目：复制为 agent 根下的真实副本（改动主配置前的手动备份路径）。
        if is_config_virtual(&home.join("agent"), &path) {
            let src = home.join(CONFIG_NAME);
            let bytes =
                std::fs::read(&src).map_err(|e| format!("读取主配置失败: {e}"))?;
            let base = home
                .join("agent")
                .canonicalize()
                .map_err(|e| format!("工作区不可用: {e}"))?;
            let mut n = 1u32;
            loop {
                let candidate = if n == 1 {
                    "rclaw-副本.toml".to_string()
                } else {
                    format!("rclaw-副本{n}.toml")
                };
                let dest = base.join(&candidate);
                if !dest.exists() {
                    std::fs::write(&dest, &bytes).map_err(|e| format!("复制失败: {e}"))?;
                    return Ok(candidate);
                }
                n += 1;
                if n > 999 {
                    return Err("同名副本过多，请手动整理".into());
                }
            }
        }
        let (base, canonical) = resolve_workspace_entry(&home.join("agent"), &path)?;
        let meta = canonical
            .metadata()
            .map_err(|e| format!("读取文件信息失败: {e}"))?;
        if !meta.is_file() {
            return Err("仅支持复制文件（目录请进入后逐个复制）".into());
        }
        let src_name = canonical
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("文件名不可解析")?;
        let (stem, ext) = match src_name.rsplit_once('.') {
            Some((s, e)) => (s, Some(e)),
            None => (src_name, None),
        };
        let parent = canonical.parent().ok_or("父目录不可解析")?;
        let mut n = 1u32;
        loop {
            let candidate = match (n, ext) {
                (1, Some(e)) => format!("{stem}-副本.{e}"),
                (1, None) => format!("{stem}-副本"),
                (i, Some(e)) => format!("{stem}-副本{i}.{e}"),
                (i, None) => format!("{stem}-副本{i}"),
            };
            let dest = parent.join(&candidate);
            if !dest.exists() {
                std::fs::copy(&canonical, &dest).map_err(|e| format!("复制失败: {e}"))?;
                // 相对工作区根的返回路径（供前端刷新定位；base 已 canonicalize，
                // 剥前缀得到相对分量）。
                let rel = dest
                    .strip_prefix(&base)
                    .map_err(|_| "结果路径异常".to_string())?
                    .to_string_lossy()
                    .replace('\\', "/");
                return Ok(rel);
            }
            n += 1;
            if n > 999 {
                return Err("同名副本过多，请手动整理".into());
            }
        }
    })
    .await
    .map_err(|e| format!("复制任务失败: {e}"))?
}

/// 删除工作区文件或目录（目录递归删除；拒绝删除 agent 根自身）。
#[tauri::command]
pub async fn workspace_delete(state: State<'_, AppState>, path: String) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        if is_config_virtual(&home.join("agent"), &path) {
            return Err("主配置 rclaw.toml 是虚拟条目，不能删除（可清空内容或复制副本）".into());
        }
        let (base, canonical) = resolve_workspace_entry(&home.join("agent"), &path)?;
        if canonical == base {
            return Err("不能删除工作区根目录".into());
        }
        let meta = canonical
            .metadata()
            .map_err(|e| format!("读取信息失败: {e}"))?;
        if meta.is_dir() {
            std::fs::remove_dir_all(&canonical).map_err(|e| format!("删除目录失败: {e}"))?;
        } else {
            std::fs::remove_file(&canonical).map_err(|e| format!("删除文件失败: {e}"))?;
        }
        Ok(())
    })
    .await
    .map_err(|e| format!("删除任务失败: {e}"))?
}

/// 重命名文件/目录（仅名字，不换目录）：新名不含分隔符/..，目标不得已存在。
#[tauri::command]
pub async fn workspace_rename(
    state: State<'_, AppState>,
    path: String,
    new_name: String,
) -> Result<(), String> {
    let home = state.home.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let name = new_name.trim();
        // 按字符数限长（len() 是字节数，中文名会被压到 40 字）。
        if name.is_empty() || name.chars().count() > 120 {
            return Err("名称不能为空且不超过 120 字符".into());
        }
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return Err("名称不能包含路径分隔符".into());
        }
        let root = home.join("agent");
        if is_config_virtual(&root, &path) {
            return Err("主配置 rclaw.toml 不能重命名".into());
        }
        let (_base, canonical) = resolve_workspace_entry(&root, &path)?;
        let parent = canonical.parent().ok_or("父目录不可解析")?;
        let target = parent.join(name);
        if target.exists() {
            return Err("同名文件/目录已存在".into());
        }
        // 根目录的 rclaw.toml 保留给主配置虚拟条目（target.exists() 探不到
        // 虚拟名；path 不含分隔符即位于根）。
        if !path.contains('/')
            && config_virtual_active(&root)
            && name.eq_ignore_ascii_case(CONFIG_NAME)
        {
            return Err("rclaw.toml 是主配置保留名（在根目录直接编辑即改主配置）".into());
        }
        std::fs::rename(&canonical, &target).map_err(|e| format!("重命名失败: {e}"))
    })
    .await
    .map_err(|e| format!("重命名任务失败: {e}"))?
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
        // 按字符数限长（同 rename；中文名不被字节数误伤）。
        if clean.is_empty() || clean.chars().count() > 120 {
            return Err("名称不能为空且不超过 120 字符".into());
        }
        if clean.contains('/') || clean.contains('\\') || clean.contains("..") {
            return Err("名称不能包含路径分隔符".into());
        }
        let base = resolve_workspace_dir(&home.join("agent"), &dir)?;
        let target = base.join(clean);
        if target.exists() {
            return Err("同名文件/目录已存在".into());
        }
        // 根目录的 rclaw.toml 保留给主配置虚拟条目（exists() 探不到虚拟名）。
        if dir.is_empty()
            && config_virtual_active(&home.join("agent"))
            && clean.eq_ignore_ascii_case(CONFIG_NAME)
        {
            return Err("rclaw.toml 是主配置保留名（在根目录直接编辑即改主配置）".into());
        }
        std::fs::create_dir(&target).map_err(|e| format!("创建文件夹失败: {e}"))
    })
    .await
    .map_err(|e| format!("新建文件夹任务失败: {e}"))?
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
            let result = rclaw_android::read_clipboard_to_file(
                &base.to_string_lossy(),
            )
            .map_err(|e| format!("读取剪贴板失败: {e}"))?;
            if result == "empty" {
                return Err("剪贴板没有文本或文件（微信/QQ 内容请用「分享到 RClaw」导入）".into());
            }
            if let Some(reason) = result.strip_prefix("error:") {
                return Err(reason.to_string());
            }
            // 文件 URI 粘贴的原名撞上根目录保留名：删除已落地文件并拒绝，
            // 免得虚拟条目与真实文件同名并列。
            if dir.is_empty() {
                if let Some(landed) = result.strip_prefix("ok:") {
                    if landed.eq_ignore_ascii_case(CONFIG_NAME)
                        && config_virtual_active(&home.join("agent"))
                    {
                        let _ = std::fs::remove_file(base.join(landed));
                        return Err(
                            "rclaw.toml 是主配置保留名（在根目录直接编辑即改主配置）".into(),
                        );
                    }
                }
            }
            Ok(result)
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
                let (_base, canonical) = resolve_workspace_entry(&home.join("agent"), &path)?;
                let meta = canonical
                    .metadata()
                    .map_err(|e| format!("读取文件信息失败: {e}"))?;
                if !meta.is_file() {
                    return Err("仅支持复制文件到系统剪贴板".into());
                }
                canonical
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

/// 工作区相对路径校验 + 绝对路径解析（uploads/ 下的采集产物）。
fn resolve_agent_path(
    home: &std::path::Path,
    rel: &str,
) -> Result<std::path::PathBuf, String> {
    if rel.is_empty() || rel.contains("..") || rel.contains('\\') || rel.starts_with('/') {
        return Err("非法文件路径".into());
    }
    let full = home.join("agent").join(rel);
    let canonical = full
        .canonicalize()
        .map_err(|e| format!("文件不存在: {e}"))?;
    let root = home.join("agent").canonicalize().map_err(|e| format!("工作区不可用: {e}"))?;
    if !canonical.starts_with(&root) {
        return Err("路径越出工作区".into());
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
    let abs = resolve_agent_path(&home, &rel)?;
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
        // 桌面兜底：拷贝到用户图片目录的 RClaw 子目录。
        let base = std::env::var("USERPROFILE")
            .map(|p| std::path::PathBuf::from(p).join("Pictures"))
            .unwrap_or_else(|_| home.clone());
        let dir = base.join("RClaw");
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败: {e}"))?;
        std::fs::copy(&abs, dir.join(std::path::Path::new(&abs).file_name().unwrap_or_default()))
            .map_err(|e| format!("保存失败: {e}"))?;
        Ok("ok".into())
    }
}

/// 放弃采集产物：删除工作区里的临时文件（用户点了取消）。
#[tauri::command]
pub async fn discard_capture(state: State<'_, AppState>, rel: String) -> Result<(), String> {
    let home = state.home.clone();
    let abs = resolve_agent_path(&home, &rel)?;
    let _ = std::fs::remove_file(abs);
    Ok(())
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
        let rel = format!("uploads/photo-edited-{}.jpg", chrono_like_ts());
        std::fs::write(dir.join(&rel["uploads/".len()..]), &bytes)
            .map_err(|e| format!("写盘失败: {e}"))?;
        Ok(rel)
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
