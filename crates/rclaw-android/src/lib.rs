//! RClaw Android 原生宿主。
//!
//! 装配由 `rclaw-desktop` 的 `assemble_apk_agent`（nativeStart 触发）完成，
//! 本 crate 提供配置解析 / 环境胶水 / 健康信号 / JNI 桥内核。Rust/Tokio 运行
//! 在应用进程内；gbrain 通过 `rclaw-gbrain` 直接注册工具，不创建 CLI 或 MCP
//! 子进程；skill Python 经 `bridge_executor::BridgePythonExecutor`（Chaquopy
//! 桥）执行。

mod bridge_executor;
#[cfg(target_os = "android")]
pub mod browser;
#[cfg(unix)]
pub mod cdp_browser;
mod host_glue;
pub mod keep_alive;

pub use bridge_executor::{BridgePythonExecutor, PendingApproval, SkillApprovalStore};
pub use host_glue::prepare_gbrain;
pub use host_glue::{
    approval_gate, build_channels, install_skill_gate, prepare_environment, ApkChannels,
    ApprovalGateHandle, PreparedEnvironment,
};

#[cfg(target_os = "android")]
pub use jni_bridge::{set_java_vm, set_python_bridge};

use anyhow::{anyhow, Context, Result};
use rclaw_cli::config::Config as RClawConfig;
use rclaw_gbrain::{GBrainConfig, GBrainTools};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const RUNTIME_HEARTBEAT_STALE_AFTER_MS: u64 = 150_000;
const SKILL_CONFIG_MAX_COUNT: usize = 128;
const SKILL_CONFIG_MAX_BYTES: usize = 1024 * 1024;
const SKILL_CONFIG_TOTAL_MAX_BYTES: usize = 8 * 1024 * 1024;
const APPROVED_SCRIPT_MAX_COUNT: usize = 512;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AndroidConfig {
    /// Android `Context.filesDir` 的绝对路径。
    pub files_dir: String,
    /// 允许 gbrain 读取和上传的 app 私有工作目录；缺省与 TOML 的 agent.base_dir 相同。
    pub workspace_dir: Option<String>,
    /// Complete app-managed `rclaw.toml`.
    pub toml_config: Option<String>,
    /// App-private directory populated from APK skill assets by the Java host.
    pub bundled_skills_dir: Option<String>,
    /// App-private, non-backed-up directory for APK-managed skill config files.
    pub skill_configs_dir: Option<String>,
    /// APK 管理的独立 skill 配置：相对 `agent/skills` 的路径 -> 完整文件内容。
    #[serde(default)]
    pub skill_configs: HashMap<String, String>,
    /// 用户通过 APK 文件编辑器明确保存过的 Python 脚本：相对 skills 路径 -> SHA-256。
    #[serde(default)]
    pub approved_skill_scripts: HashMap<String, String>,
}

impl AndroidConfig {
    /// 最小宿主上下文（nativeInit 阶段：只有 filesDir；GUI 早于服务装配时，
    /// `resolve_android_config` 从 `filesDir/rclaw.toml` 文件补全——见 resolve_android_config）。
    pub fn minimal(files_dir: &str) -> Self {
        Self {
            files_dir: files_dir.to_string(),
            ..Default::default()
        }
    }
}

/// 启动取证日志（Android；宿主/桌面共用 filesDir/boot_debug.log）。
pub fn log_boot(step: &str) {
    let Some(context) = host_context() else {
        tracing::warn!(step, "boot log: 宿主上下文未就绪");
        return;
    };
    let path = context.files_dir.join("boot_debug.log");
    // 诊断日志只增不减会无限增长：超过 1MB 时截断重开（保留最近一次启动从
    // from_toml 取证起的内容会随下次启动自然补齐，无需归档）。
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1024 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::write(&path, "");
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        use std::io::Write;
        let _ = writeln!(
            f,
            "[{}] {step}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        );
    }
}

pub struct ResolvedAndroidConfig {
    pub files_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub bundled_skills_dir: PathBuf,
    pub skill_configs_dir: PathBuf,
    pub skill_configs: HashMap<String, String>,
    pub approved_skill_scripts: HashMap<String, String>,
    pub config: RClawConfig,
}

/// v2（GUI 共享运行时）健康信号：`assemble_apk_agent` 成功后置位并周期喂心跳；
/// `is_healthy()` 只读它（v0.2.0 独立 host 线程路径已删除）。
static V2_RUNTIME_HEALTHY: AtomicBool = AtomicBool::new(false);
static V2_LAST_HEARTBEAT_MS: AtomicU64 = AtomicU64::new(0);
/// 装配在途标记：nativeStart 秒回后 Java 的 nativeStarted 立即为 true，而装配
/// （Python 导入、通道连接）在低端机上可超过 60 秒——这段窗口内健康检查必须
/// 判活，否则看门狗在装配完成前就杀进程（冷启动必死循环）。上限防装配死锁。
static ASSEMBLY_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
static ASSEMBLY_STARTED_AT_MS: AtomicU64 = AtomicU64::new(0);
const ASSEMBLY_GRACE_MS: u64 = 300_000;

/// v2 装配成功/停机时置位与清除。
pub fn set_runtime_healthy(healthy: bool) {
    V2_RUNTIME_HEALTHY.store(healthy, Ordering::Relaxed);
    if healthy {
        V2_LAST_HEARTBEAT_MS.store(unix_time_millis(), Ordering::Relaxed);
    }
}

/// 用系统浏览器打开外部链接（Android intent；Java 侧持有 app context）。
pub fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::open_url(url)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = url;
        Err(anyhow!(
            "open_url 仅在 Android 宿主可用（桌面走系统 shell）"
        ))
    }
}

/// 应用内更新：拉起系统安装器安装已下载的 APK。`apk_abs` 必须位于 FileProvider
/// 暴露的 files/ 之下（file_paths.xml 的 `<files-path path="."/>`；下载命令落在
/// files/update/）。Android 8+ 首次会先引导到「未知来源安装」授权页。
pub fn install_update(apk_abs: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::install_update(apk_abs)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = apk_abs;
        Err(anyhow!(
            "install_update 仅在 Android 宿主可用（桌面走自替换脚本）"
        ))
    }
}

/// CDP 浏览器会话就绪信息（Java start JSON 带回；pid 供拼 DevTools socket 名，
/// 尺寸为 WebView 离屏布局的真实 Display CSS 视口，指纹一致性记录用）。
#[derive(Debug, Clone, Copy, Serialize)]
pub struct BrowserSessionInfo {
    pub pid: u32,
    /// WebView 创建代际（Java 侧 createEpoch）：复用不变、重建递增。
    /// 触摸降级标志据此判定「WebView 是否真的换了」（pid 是应用进程号，
    /// 销毁重建也不变，不能当实例身份）。
    pub epoch: u32,
    pub width: i64,
    pub height: i64,
    pub dpr: f64,
}

/// CDP 浏览器会话：确保长驻调试 WebView 在跑（阻塞至就绪，至多 15s）。
pub fn browser_session_start() -> Result<BrowserSessionInfo> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_session_start()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// CDP 浏览器会话：销毁长驻 WebView（下次使用自动重建）。
pub fn browser_session_stop() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_session_stop()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// CDP 浏览器会话：离屏截图到 PNG（View.draw 软件位图，不走 CDP——离屏 WebView
/// 无合成器帧）。阻塞至绘制完成（至多 20s）。
pub fn browser_session_screenshot(abs_path: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_session_screenshot(abs_path)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = abs_path;
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// CDP 浏览器悬浮窗：展开面板并挂载 WebView（Google 风控页人工过码入口）。
/// 无悬浮窗权限时发引导通知并返回错误（park 不阻断）。
pub fn browser_overlay_expand() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_overlay_expand()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// CDP 浏览器悬浮窗：展开面板并导航到指定 http(s) URL（聊天链接默认应用内
/// 打开；仅 Android 宿主可用，桌面走系统浏览器）。
pub fn browser_overlay_open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_overlay_open_url(url)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = url;
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// CDP 浏览器悬浮窗：收起为气泡（WebView 摘回离屏，agent 不受影响）。
pub fn browser_overlay_collapse() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_overlay_collapse()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// CDP 浏览器悬浮窗：摘窗（WebView 回离屏，不销毁会话）。
pub fn browser_overlay_dismiss() -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_overlay_dismiss()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("CDP 浏览器仅在 Android 宿主可用"))
    }
}

/// WebView 当前是否挂在悬浮窗面板里（截图分流：attached 走 CDP 截图）。
/// 查询失败按离屏处理。
pub fn browser_overlay_attached() -> bool {
    #[cfg(target_os = "android")]
    {
        jni_bridge::browser_overlay_attached().unwrap_or(false)
    }
    #[cfg(not(target_os = "android"))]
    {
        false
    }
}

/// Google 风控页命中：高优通知 + 尽力自动展开悬浮窗。失败仅记日志不阻断
/// （park 照常等待；桌面无此能力为 no-op）。
pub fn notify_captcha_detected(url: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::notify_captcha_detected(url)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = url;
        Ok(())
    }
}

/// 会话内点开工作区媒体（相对 agent/ 的路径，如 media/skilltest_fish.mp4）：
/// 经 FileProvider 只读 URI 拉起系统播放器/查看器。
pub fn open_media(relative_path: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::open_media(relative_path)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = relative_path;
        Err(anyhow!(
            "open_media 仅在 Android 宿主可用（桌面走系统 shell）"
        ))
    }
}

/// 前台服务通知状态更新（任务运行中 busy=true：绿色 accent + 进度动画；
/// 空闲：上下文占用 + 内存文案）。桌面平台 no-op。
pub fn update_runtime_status(text: &str, busy: bool) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::update_runtime_status(text, busy)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (text, busy);
        Ok(())
    }
}

/// turn 收尾提醒通知（独立「任务完成」通道；调用方负责前台门闸过滤）。
/// 桌面平台 no-op。
pub fn notify_turn_done(text: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::notify_turn_done(text)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = text;
        Ok(())
    }
}

/// 应用 GUI 是否在前台（完成通知的门闸）。桌面恒 true。
pub fn app_foreground() -> bool {
    #[cfg(target_os = "android")]
    {
        jni_bridge::app_foreground().unwrap_or(true)
    }
    #[cfg(not(target_os = "android"))]
    {
        true
    }
}

/// 拍照/录视频（kind: photo|video）：起系统相机，结果走 Activity onActivityResult
/// → CaptureBridge 落盘 uploads/ + 写 capture_result.json。
pub fn capture(kind: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::capture(kind)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = kind;
        Err(anyhow!("capture 仅在 Android 宿主可用"))
    }
}

pub fn record_start() -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::record_start()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("录音仅 Android 宿主支持"))
    }
}

pub fn record_stop() -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::record_stop()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("录音仅 Android 宿主支持"))
    }
}

pub fn record_active() -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::record_active()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("录音仅 Android 宿主支持"))
    }
}

pub fn record_cancel() -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::record_cancel()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("录音仅 Android 宿主支持"))
    }
}

/// 保存到系统相册（MediaStore）。kind: photo|video|audio。
pub fn save_to_gallery(abs_path: &str, kind: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::save_to_gallery(abs_path, kind)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (abs_path, kind);
        Err(anyhow!("保存到相册仅 Android 宿主支持"))
    }
}

/// 转发工作区媒体到其它应用（ACTION_SEND 系统分享面板；相对 agent/ 的路径）。
pub fn share_file(relative_path: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::share_file(relative_path)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = relative_path;
        Err(anyhow!("转发仅 Android 宿主支持"))
    }
}

/// 把纯文本分享到其它应用（ACTION_SEND text/plain：微信/QQ 收为聊天文字）。
pub fn share_text(title: &str, text: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::share_text(title, text)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (title, text);
        Err(anyhow!("分享文本仅 Android 宿主支持"))
    }
}

/// 读系统剪贴板落盘到 dest_dir（文本 → .txt；content:// URI → 原名文件）。
/// 返回 "ok:<文件名>" / "empty" / "error:<原因>"。
pub fn read_clipboard_to_file(dest_dir: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::read_clipboard_to_file(dest_dir)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = dest_dir;
        Err(anyhow!("读取系统剪贴板仅 Android 宿主支持"))
    }
}

/// 把文件以 FileProvider URI 写入系统剪贴板（文件管理器可粘贴）。
pub fn copy_file_to_clipboard(abs_path: &str) -> Result<()> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::copy_file_to_clipboard(abs_path)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = abs_path;
        Err(anyhow!("复制到系统剪贴板仅 Android 宿主支持"))
    }
}

/// 文档保存到系统下载目录（MediaStore.Downloads）。
/// 返回 "ok" / "permission_requested" / "error:<原因>"。
pub fn save_to_downloads(abs_path: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::save_to_downloads(abs_path)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = abs_path;
        Err(anyhow!("保存到下载目录仅 Android 宿主支持"))
    }
}

/// 解码音频为 16kHz 单声道 WAV。返回 "ok" 或 "error:<原因>"。
pub fn decode_to_wav(audio_abs: &str, wav_abs: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::decode_to_wav(audio_abs, wav_abs)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (audio_abs, wav_abs);
        Err(anyhow!("音频解码仅 Android 宿主支持"))
    }
}

pub fn load_whisper_lib() -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::load_whisper_lib()
    }
    #[cfg(not(target_os = "android"))]
    {
        Err(anyhow!("whisper 仅 Android 宿主支持"))
    }
}

/// whisper 转写（阻塞，工作线程调用）。返回转写文本或 "error:<原因>"。
pub fn whisper_transcribe(model: &str, wav: &str, lang: &str) -> Result<String> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::whisper_transcribe(model, wav, lang)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (model, wav, lang);
        Err(anyhow!("whisper 仅 Android 宿主支持"))
    }
}

/// 后台线程下载 whisper 模型（多镜像，进度写状态文件）。
pub fn whisper_model_download(urls: &[&str], dest_name: &str) -> Result<bool> {
    #[cfg(target_os = "android")]
    {
        jni_bridge::whisper_model_download(urls, dest_name)
    }
    #[cfg(not(target_os = "android"))]
    {
        let _ = (urls, dest_name);
        Err(anyhow!("模型下载仅 Android 宿主支持"))
    }
}

/// v2 心跳：agent loop 存活期间每 30 秒调用（与 v0.2.0 run() 的节奏一致）。
pub fn mark_v2_heartbeat() {
    V2_LAST_HEARTBEAT_MS.store(unix_time_millis(), Ordering::Relaxed);
}

/// 装配开始/结束（`assemble_apk_agent` 进入时置位，Drop 守卫清除）。
pub fn begin_assembly() {
    ASSEMBLY_STARTED_AT_MS.store(unix_time_millis(), Ordering::Relaxed);
    ASSEMBLY_IN_FLIGHT.store(true, Ordering::Relaxed);
}

pub fn end_assembly() {
    ASSEMBLY_IN_FLIGHT.store(false, Ordering::Relaxed);
}

/// 宿主启动上下文（MainActivity/nativeStart 记录；桌面 GUI 装配与 home 解析读取）。
#[derive(Clone)]
pub struct AndroidHostContext {
    pub files_dir: PathBuf,
    pub config_json: String,
}

static HOST_CONTEXT: OnceLock<Mutex<Option<AndroidHostContext>>> = OnceLock::new();

pub fn record_host_context(files_dir: PathBuf, config_json: String) {
    *HOST_CONTEXT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(AndroidHostContext {
        files_dir,
        config_json,
    });
}

pub fn host_context() -> Option<AndroidHostContext> {
    HOST_CONTEXT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

static GBRAIN_ENV_ORIGINALS: OnceLock<Mutex<HashMap<String, Option<OsString>>>> = OnceLock::new();

pub(crate) struct GBrainEnvGuardForProcess;
impl Drop for GBrainEnvGuardForProcess {
    fn drop(&mut self) {
        let _ = replace_gbrain_env(&HashMap::new());
    }
}

/// 宿主是否健康：V2 共享运行时心跳新鲜即活；装配在途按宽限期判活。
pub fn is_healthy() -> bool {
    if V2_RUNTIME_HEALTHY.load(Ordering::Relaxed) {
        let now_ms = unix_time_millis();
        let last_ms = V2_LAST_HEARTBEAT_MS.load(Ordering::Relaxed);
        return heartbeat_is_fresh(now_ms, last_ms);
    }
    // 装配在途（V2 尚未置位）：进程活着、装配进行中，判活；超过宽限期仍
    // 未完成按故障放行给看门狗恢复。
    if ASSEMBLY_IN_FLIGHT.load(Ordering::Relaxed) {
        let started = ASSEMBLY_STARTED_AT_MS.load(Ordering::Relaxed);
        if started != 0 && unix_time_millis().saturating_sub(started) <= ASSEMBLY_GRACE_MS {
            return true;
        }
    }
    false
}

/// 心跳 / 装配计时用的单调毫秒计数（进程内起点，恒 ≥ 1，0 保留为「未置位」）。
/// 不能用墙钟：安卓开机后 NTP 校时、用户改时间都会让 `SystemTime` 跳变，看门狗
/// 单次探到「心跳过期 / 时间倒退」就直接杀进程重启，一次校时就是一次无故重启。
fn unix_time_millis() -> u64 {
    static ORIGIN: OnceLock<std::time::Instant> = OnceLock::new();
    let origin = ORIGIN.get_or_init(std::time::Instant::now);
    origin.elapsed().as_millis().min(u64::MAX as u128 - 1) as u64 + 1
}

fn heartbeat_is_fresh(now_ms: u64, last_ms: u64) -> bool {
    last_ms != 0 && now_ms >= last_ms && now_ms - last_ms <= RUNTIME_HEARTBEAT_STALE_AFTER_MS
}

/// 解析启动 TOML 并物理删除已退役的遗留配置节：内存剥离的同时写回
/// `files_dir/rclaw.toml`（AppConfig.toJson 从该文件读全文传给 Rust，文件是
/// 唯一真源；toml_edit 保真，注释与格式不动）。写回失败只落 boot 日志，
/// 本次仍按剥离结果加载，下次启动再试。deny_unknown_fields 会把遗留键当
/// 「拼错的键」拒绝（实例：v2.8.0 删除 [web_browser]，v2.7.x 配置在其上
/// 启动即「解析 rclaw.toml 失败」），所以解析前必须剥离。
fn from_toml_purging_legacy(toml: &str, files_dir: &Path) -> Result<RClawConfig> {
    let (stripped, removed) = rclaw_cli::config::strip_removed_legacy_sections(toml);
    if !removed.is_empty() {
        let path = files_dir.join("rclaw.toml");
        // 原子写：rclaw.toml 是唯一真源（含 API key），启动期被杀进程若落在
        // `fs::write` 的截断窗口内，整份配置归零。
        match rclaw_store::atomic_write(&path, stripped.as_bytes()) {
            Ok(()) => log_boot(&format!(
                "已从 rclaw.toml 物理移除已退役配置节: {}",
                removed.join(", ")
            )),
            Err(error) => log_boot(&format!(
                "rclaw.toml 遗留节写回失败: {error}（本次按剥离结果加载）"
            )),
        }
    }
    RClawConfig::from_toml(&stripped)
}

/// Resolve the effective app-private agent directory without requiring an LLM key.
/// The APK file editor uses this before the runtime has necessarily started.
pub fn resolve_agent_root(config_json: &str) -> Result<PathBuf> {
    let launch: AndroidConfig = serde_json::from_str(config_json)
        .map_err(|error| anyhow!("解析 Android 配置 JSON 失败: {error}"))?;
    if launch.files_dir.trim().is_empty() {
        return Err(anyhow!("files_dir 不能为空"));
    }
    let files_dir = PathBuf::from(launch.files_dir.trim());
    if !is_android_absolute(&files_dir) {
        return Err(anyhow!("files_dir 必须是 Android 应用私有目录的绝对路径"));
    }
    let toml = launch
        .toml_config
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| anyhow!("APK 完整 TOML 配置不能为空"))?;
    let config = from_toml_purging_legacy(toml, &files_dir)?;
    Ok(resolve_private_path(
        &files_dir,
        &files_dir,
        &config.agent.base_dir,
        &files_dir.join("rclaw"),
    ))
}

pub fn resolve_android_config(launch: AndroidConfig) -> Result<ResolvedAndroidConfig> {
    if launch.files_dir.trim().is_empty() {
        return Err(anyhow!("files_dir 不能为空"));
    }
    let files_dir = PathBuf::from(launch.files_dir.trim());
    if !is_android_absolute(&files_dir) {
        return Err(anyhow!("files_dir 必须是 Android 应用私有目录的绝对路径"));
    }

    // toml_config 缺省（GUI 早于服务装配的 nativeInit 最小上下文）时，按固定路径
    // 回读 filesDir/rclaw.toml（v1.7 review #2：agent 读固定路径；损坏/缺失由
    // from_toml 报错，绝不自动写默认值）。
    let toml_from_file;
    let toml = match launch
        .toml_config
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        Some(text) => text,
        None => {
            let path = PathBuf::from(files_dir.as_os_str()).join("rclaw.toml");
            toml_from_file = std::fs::read_to_string(&path).map_err(|error| {
                anyhow!(
                    "rclaw.toml 未提供且无法读取 {}: {error}（首次使用请完成配置向导）",
                    path.display()
                )
            })?;
            toml_from_file.as_str()
        }
    };
    let mut config = from_toml_purging_legacy(toml, &files_dir).map_err(|error| {
        // 真机取证（vivo logcat 被裁剪）：把输入指纹与完整错误链落到 boot log。
        let head: String = toml.chars().take(40).collect();
        let boot = format!(
            "from_toml 失败: {error:#}\n  toml_config 来源: {}\n  长度: {} 字节\n  开头 40 字符: {head:?}\n",
            if launch
                .toml_config
                .as_deref()
                .is_some_and(|t| !t.trim().is_empty())
            {
                "Java toml_config"
            } else {
                "filesDir/rclaw.toml 文件回读"
            },
            toml.len()
        );
        crate::log_boot(&boot);
        error
    })?;

    // 兜底目录必须与 GUI/Java 侧硬编码的工作区根（filesDir/agent）一致：
    // 双根会让媒体预览、上传、文件编辑各自指向不同的树。
    let default_base = files_dir.join("agent");
    let legacy_base = files_dir.join("rclaw");
    let base_dir = resolve_private_path(
        &files_dir,
        &files_dir,
        &config.agent.base_dir,
        &default_base,
    );
    config.agent.base_dir = base_dir.to_string_lossy().into_owned();
    // 存量迁移：旧版本兜底目录是 filesDir/rclaw——仅当本次解析结果就是默认
    // 根（未显式改 base_dir）且旧树存在、新树缺失时整体搬移（会话/审批表/
    // bot 定义/uploads），否则升级后老数据"消失"。显式配置指向旧路径的
    // 用户不受影响。
    if base_dir == default_base && legacy_base.is_dir() && !default_base.exists() {
        if let Err(error) = std::fs::rename(&legacy_base, &default_base) {
            tracing::warn!(
                error = %error,
                "旧工作区目录迁移失败（保留原位，配置可显式指向）"
            );
        }
    }

    let default_logs = base_dir.join("logs");
    let log_dir = resolve_private_path(&files_dir, &base_dir, &config.log.dir, &default_logs);
    config.log.dir = log_dir.to_string_lossy().into_owned();
    config.log.file_name = safe_file_name(&config.log.file_name, "rclaw.log");

    // Validate the selected provider before the background host thread starts.
    config.build_provider_config()?;

    // Keep gbrain file upload rooted at the same private directory used by
    // RClaw files and skill outputs. An embedding host may still narrow this
    // with an explicit workspace_dir.
    let default_workspace = base_dir.clone();
    let workspace_dir = launch
        .workspace_dir
        .as_deref()
        .map(|path| resolve_private_path(&files_dir, &files_dir, path, &default_workspace))
        .unwrap_or(default_workspace);
    let bundled_skills_dir = launch
        .bundled_skills_dir
        .as_deref()
        .map(|path| {
            resolve_private_path(
                &files_dir,
                &files_dir,
                path,
                &files_dir.join("runtime/bundled-skills"),
            )
        })
        .unwrap_or_else(|| files_dir.join("runtime/bundled-skills"));
    let app_data_dir = files_dir
        .parent()
        .ok_or_else(|| anyhow!("files_dir 缺少 app 数据目录父级"))?;
    let default_skill_configs_dir = app_data_dir.join("no_backup/rclaw-skill-configs");
    let skill_configs_dir = launch
        .skill_configs_dir
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or(default_skill_configs_dir);
    if !is_android_absolute(&skill_configs_dir)
        || skill_configs_dir
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
        || !skill_configs_dir.starts_with(app_data_dir)
        || skill_configs_dir.starts_with(&files_dir)
        || skill_configs_dir == app_data_dir
    {
        return Err(anyhow!(
            "skill_configs_dir 必须位于 app 私有数据目录内且处于 files_dir 之外"
        ));
    }
    validate_skill_configs(&launch.skill_configs)?;
    validate_approved_skill_scripts(&launch.approved_skill_scripts)?;

    Ok(ResolvedAndroidConfig {
        files_dir,
        workspace_dir,
        bundled_skills_dir,
        skill_configs_dir,
        skill_configs: launch.skill_configs,
        approved_skill_scripts: launch.approved_skill_scripts,
        config,
    })
}

fn validate_skill_configs(configs: &HashMap<String, String>) -> Result<()> {
    if configs.len() > SKILL_CONFIG_MAX_COUNT {
        return Err(anyhow!(
            "skill 配置文件数量超过上限 {SKILL_CONFIG_MAX_COUNT}"
        ));
    }
    let mut total = 0usize;
    for (relative, content) in configs {
        validate_skill_config_relative_path(relative)?;
        if content.len() > SKILL_CONFIG_MAX_BYTES {
            return Err(anyhow!("skill 配置文件超过 1 MiB: {relative}"));
        }
        total = total
            .checked_add(content.len())
            .ok_or_else(|| anyhow!("skill 配置总大小溢出"))?;
        if total > SKILL_CONFIG_TOTAL_MAX_BYTES {
            return Err(anyhow!("skill 配置总大小超过 8 MiB"));
        }
    }
    Ok(())
}

fn validate_skill_config_relative_path(relative: &str) -> Result<()> {
    if relative.is_empty()
        || relative.contains('\\')
        || !relative
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'.' | b'_' | b'-'))
    {
        return Err(anyhow!("skill 配置路径包含非法字符: {relative}"));
    }
    let path = Path::new(relative);
    let components: Vec<_> = path.components().collect();
    if components.len() < 2
        || components
            .iter()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(anyhow!(
            "skill 配置路径必须位于具体 skill 目录内: {relative}"
        ));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if !matches!(
        file_name,
        ".env" | "config.env" | "config.toml" | "config.json"
    ) {
        return Err(anyhow!("不支持的 skill 配置文件名: {relative}"));
    }
    Ok(())
}

fn validate_approved_skill_scripts(scripts: &HashMap<String, String>) -> Result<()> {
    if scripts.len() > APPROVED_SCRIPT_MAX_COUNT {
        return Err(anyhow!(
            "人工批准的 skill 脚本数量超过上限 {APPROVED_SCRIPT_MAX_COUNT}"
        ));
    }
    for (relative, digest) in scripts {
        let path = Path::new(relative);
        let components: Vec<_> = path.components().collect();
        if relative.is_empty()
            || relative.contains('\\')
            || components.len() < 2
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
            || path.extension().and_then(|value| value.to_str()) != Some("py")
        {
            return Err(anyhow!("人工批准的 skill 脚本路径无效: {relative}"));
        }
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(anyhow!("人工批准的 skill 脚本摘要无效: {relative}"));
        }
    }
    Ok(())
}

fn safe_file_name(configured: &str, fallback: &str) -> String {
    let path = Path::new(configured.trim());
    if configured.trim().is_empty()
        || !matches!(
            path.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        )
    {
        fallback.into()
    } else {
        configured.trim().into()
    }
}

fn looks_like_foreign_absolute(path: &str) -> bool {
    let bytes = path.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/'))
        || path.starts_with("\\\\")
}

fn is_android_absolute(path: &Path) -> bool {
    path.is_absolute() || path.as_os_str().to_string_lossy().starts_with('/')
}

fn resolve_private_path(
    root: &Path,
    relative_base: &Path,
    configured: &str,
    fallback: &Path,
) -> PathBuf {
    let configured = configured.trim();
    if configured.is_empty() || looks_like_foreign_absolute(configured) {
        return fallback.to_path_buf();
    }
    let path = Path::new(configured);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return fallback.to_path_buf();
    }
    let candidate = if is_android_absolute(path) {
        path.to_path_buf()
    } else {
        relative_base.join(path)
    };
    if candidate.starts_with(root) {
        candidate
    } else {
        fallback.to_path_buf()
    }
}

#[cfg(target_os = "android")]
fn call_python_bridge(
    script: &Path,
    args_json: &str,
    cwd: &Path,
    env_json: &str,
    timeout_ms: u64,
    limit_bytes: u64,
) -> Result<String> {
    jni_bridge::run_python(script, args_json, cwd, env_json, timeout_ms, limit_bytes)
}

#[cfg(not(target_os = "android"))]
fn call_python_bridge(
    _script: &Path,
    _args_json: &str,
    _cwd: &Path,
    _env_json: &str,
    _timeout_ms: u64,
    _limit_bytes: u64,
) -> Result<String> {
    Err(anyhow!("APK embedded Python is only available on Android"))
}

/// Rust 侧协作中断（路线 B）：经 Java/Chaquopy 调桥的 `interrupt_main()`，
/// 向主线程注入 KeyboardInterrupt。非 android 宿主为 no-op。
#[cfg(target_os = "android")]
fn interrupt_python_bridge() {
    jni_bridge::interrupt_python()
}

#[cfg(not(target_os = "android"))]
fn interrupt_python_bridge() {}

fn configured_gbrain_call_timeout(config: &ResolvedAndroidConfig) -> Option<Duration> {
    config
        .config
        .mcp
        .servers
        .get("gbrain")
        .filter(|server| {
            server.enabled && server.auto_start.unwrap_or(config.config.mcp.auto_start)
        })
        .map(|server| {
            let seconds = server
                .call_timeout_secs
                .unwrap_or(config.config.mcp.call_timeout_secs)
                .max(1);
            Duration::from_secs(seconds)
        })
}

fn install_bundled_skills(source: &Path, destination: &Path, baseline: &Path) -> Result<()> {
    reject_symlink(source, "APK 内置 skill 目录")?;
    if !source.is_dir() {
        return Err(anyhow!("APK 内置 skill 目录不存在: {}", source.display()));
    }
    prune_stale_bundled_tree(source, destination, baseline)?;
    merge_bundled_tree(source, destination, baseline)
}

fn apply_skill_configs(configs: &HashMap<String, String>, destination: &Path) -> Result<()> {
    validate_skill_configs(configs)?;
    for (relative, content) in configs {
        let target = destination.join(relative);
        reject_symlink_ancestors(destination, &target, "skill 配置路径")?;
        reject_symlink(&target, "skill 配置文件")?;
        let parent = target
            .parent()
            .ok_or_else(|| anyhow!("skill 配置缺少父目录: {relative}"))?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("创建 skill 配置目录失败: {relative}"))?;
        reject_symlink_ancestors(destination, parent, "skill 配置目录")?;
        let file_name = target
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("skill 配置文件名无效: {relative}"))?;
        let temporary = parent.join(format!(".{file_name}.rclaw.tmp"));
        reject_symlink(&temporary, "skill 配置临时文件")?;
        if temporary.exists() {
            if !temporary.is_file() {
                return Err(anyhow!(
                    "skill 配置临时路径不是普通文件: {}",
                    temporary.display()
                ));
            }
            std::fs::remove_file(&temporary)?;
        }
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .with_context(|| format!("创建 skill 配置临时文件失败: {relative}"))?;
        if let Err(error) = output
            .write_all(content.as_bytes())
            .and_then(|_| output.sync_all())
        {
            drop(output);
            let _ = std::fs::remove_file(&temporary);
            return Err(error).with_context(|| format!("写入 skill 配置失败: {relative}"));
        }
        drop(output);
        // Unix/Android rename atomically replaces an existing file. Windows'
        // std implementation rejects that case, so host-side tests use the
        // best available non-atomic fallback there.
        #[cfg(windows)]
        if target.is_file() {
            std::fs::remove_file(&target)?;
        }
        std::fs::rename(&temporary, &target)
            .with_context(|| format!("提交 skill 配置失败: {relative}"))?;
    }
    Ok(())
}

fn prune_stale_bundled_tree(source: &Path, destination: &Path, baseline: &Path) -> Result<()> {
    reject_symlink(destination, "skill 目标")?;
    reject_symlink(baseline, "skill 基线")?;
    if !baseline.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(baseline)? {
        let entry = entry?;
        let baseline_path = entry.path();
        let source_path = source.join(entry.file_name());
        let destination_path = destination.join(entry.file_name());
        reject_symlink(&source_path, "APK skill 资源")?;
        let metadata = std::fs::symlink_metadata(&baseline_path)?;
        if metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "APK skill 基线不允许包含符号链接: {}",
                baseline_path.display()
            ));
        }
        if metadata.is_dir() {
            if !source_path.is_dir() {
                prune_stale_bundled_tree(
                    &source_path.join(".removed"),
                    &destination_path,
                    &baseline_path,
                )?;
            } else {
                prune_stale_bundled_tree(&source_path, &destination_path, &baseline_path)?;
            }
            remove_if_empty(&baseline_path)?;
            if !source_path.is_dir() {
                remove_if_empty(&destination_path)?;
            }
        } else if metadata.is_file() && !source_path.is_file() {
            reject_symlink(&destination_path, "skill 目标")?;
            if destination_path.is_file() && files_equal(&destination_path, &baseline_path)? {
                std::fs::remove_file(&destination_path)?;
            }
            std::fs::remove_file(&baseline_path)?;
        }
    }
    Ok(())
}

fn reject_symlink(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(anyhow!("{label}不允许是符号链接: {}", path.display()))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn reject_symlink_ancestors(root: &Path, path: &Path, label: &str) -> Result<()> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| anyhow!("{label}超出 app 私有目录: {}", path.display()))?;
    reject_symlink(root, label)?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(anyhow!("{label}不允许包含符号链接: {}", current.display()));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn remove_if_empty(path: &Path) -> Result<()> {
    if path.is_dir() && std::fs::read_dir(path)?.next().is_none() {
        std::fs::remove_dir(path)?;
    }
    Ok(())
}

fn merge_bundled_tree(source: &Path, destination: &Path, baseline: &Path) -> Result<()> {
    reject_symlink(destination, "skill 目标")?;
    reject_symlink(baseline, "skill 基线")?;
    std::fs::create_dir_all(destination)?;
    if baseline.exists() && !baseline.is_dir() {
        remove_managed_node(baseline, "skill 基线")?;
    }
    std::fs::create_dir_all(baseline)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let baseline_path = baseline.join(entry.file_name());
        reject_symlink(&destination_path, "skill 目标")?;
        reject_symlink(&baseline_path, "skill 基线")?;
        let metadata = std::fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            return Err(anyhow!(
                "APK skill 资源不允许包含符号链接: {}",
                source_path.display()
            ));
        }
        if metadata.is_dir() {
            if destination_path.exists() && !destination_path.is_dir() {
                // A user-modified file may occupy a path which became a bundled
                // directory in a newer APK. Preserve the user node and advance
                // only the immutable baseline so future upgrades remain
                // comparable instead of failing create_dir_all at startup.
                replace_baseline_tree(&source_path, &baseline_path)?;
            } else {
                merge_bundled_tree(&source_path, &destination_path, &baseline_path)?;
            }
        } else if metadata.is_file() {
            let may_update = !destination_path.exists()
                || (baseline_path.is_file() && files_equal(&destination_path, &baseline_path)?);
            if may_update {
                std::fs::copy(&source_path, &destination_path).with_context(|| {
                    format!(
                        "安装 APK skill 资源失败: {} -> {}",
                        source_path.display(),
                        destination_path.display()
                    )
                })?;
            }
            if baseline_path.exists() && !baseline_path.is_file() {
                remove_managed_node(&baseline_path, "skill 基线")?;
            }
            std::fs::copy(&source_path, &baseline_path).with_context(|| {
                format!(
                    "更新 APK skill 基线失败: {} -> {}",
                    source_path.display(),
                    baseline_path.display()
                )
            })?;
        }
    }
    Ok(())
}

fn replace_baseline_tree(source: &Path, baseline: &Path) -> Result<()> {
    remove_managed_node(baseline, "skill 基线")?;
    copy_bundled_tree(source, baseline)
}

fn copy_bundled_tree(source: &Path, destination: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(source)?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!(
            "APK skill 资源不允许包含符号链接: {}",
            source.display()
        ));
    }
    if metadata.is_file() {
        std::fs::copy(source, destination)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(anyhow!("APK skill 资源类型不受支持: {}", source.display()));
    }

    std::fs::create_dir_all(destination)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        copy_bundled_tree(&source_path, &destination.join(entry.file_name()))?;
    }
    Ok(())
}

fn remove_managed_node(path: &Path, label: &str) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() {
        return Err(anyhow!("{label}不允许是符号链接: {}", path.display()));
    }
    if metadata.is_file() {
        std::fs::remove_file(path)?;
        return Ok(());
    }
    if !metadata.is_dir() {
        return Err(anyhow!("{label}类型不受支持: {}", path.display()));
    }
    for entry in std::fs::read_dir(path)? {
        remove_managed_node(&entry?.path(), label)?;
    }
    std::fs::remove_dir(path)?;
    Ok(())
}

fn files_equal(left: &Path, right: &Path) -> Result<bool> {
    let left_metadata = std::fs::metadata(left)?;
    let right_metadata = std::fs::metadata(right)?;
    if !left_metadata.is_file()
        || !right_metadata.is_file()
        || left_metadata.len() != right_metadata.len()
    {
        return Ok(false);
    }
    Ok(std::fs::read(left)? == std::fs::read(right)?)
}

pub(crate) fn open_gbrain(
    resolved: &mut ResolvedAndroidConfig,
    workspace_dir: &Path,
) -> Result<Option<GBrainTools>> {
    let Some(server) = resolved.config.mcp.servers.get("gbrain").cloned() else {
        replace_gbrain_env(&HashMap::new())?;
        return Ok(None);
    };
    let auto_start = server.auto_start.unwrap_or(resolved.config.mcp.auto_start);
    if !server.enabled || !auto_start {
        replace_gbrain_env(&HashMap::new())?;
        return Ok(None);
    }
    if server.command.is_some() || !server.args.is_empty() || server.cwd.is_some() {
        tracing::info!("APK 使用进程内 gbrain 模块；忽略 MCP command/args/cwd，不启动子进程");
    }

    let mut env = server.env;
    normalize_gbrain_paths(&resolved.files_dir, &mut env);
    for key in [
        "GBRAIN_DIR",
        "GBRAIN_DB_PATH",
        "GBRAIN_KB_STORAGE_DIR",
        "GBRAIN_ARTIFACT_STORAGE_DIR",
        "GBRAIN_LOG_FILE_PATH",
    ] {
        if let Some(path) = env.get(key) {
            reject_symlink_ancestors(
                &resolved.files_dir,
                Path::new(path),
                &format!("gbrain 路径 {key}"),
            )?;
        }
    }
    replace_gbrain_env(&env)?;
    let gbrain_config =
        GBrainConfig::load().map_err(|error| anyhow!("加载内嵌 gbrain 配置失败: {error}"))?;
    let database_path = gbrain_config
        .database_path
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| resolved.files_dir.join("gbrain/brain.db"));
    GBrainTools::open(database_path, workspace_dir, gbrain_config)
        .map(Some)
        .map_err(anyhow::Error::from)
}

fn normalize_gbrain_paths(files_dir: &Path, env: &mut HashMap<String, String>) {
    let default_dir = files_dir.join("gbrain");
    let gbrain_dir = env
        .get("GBRAIN_DIR")
        .map(|path| resolve_private_path(files_dir, files_dir, path, &default_dir))
        .unwrap_or(default_dir);
    env.insert(
        "GBRAIN_DIR".into(),
        gbrain_dir.to_string_lossy().into_owned(),
    );

    for (key, fallback) in [
        ("GBRAIN_DB_PATH", gbrain_dir.join("brain.db")),
        ("GBRAIN_KB_STORAGE_DIR", gbrain_dir.join("kb")),
        ("GBRAIN_ARTIFACT_STORAGE_DIR", gbrain_dir.join("artifacts")),
        ("GBRAIN_LOG_FILE_PATH", gbrain_dir.join("logs/gbrain.log")),
    ] {
        let path = env
            .get(key)
            .map(|configured| resolve_private_path(files_dir, &gbrain_dir, configured, &fallback))
            .unwrap_or(fallback);
        env.insert(key.into(), path.to_string_lossy().into_owned());
    }
}

fn replace_gbrain_env(values: &HashMap<String, String>) -> Result<()> {
    let mut originals = GBRAIN_ENV_ORIGINALS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .map_err(|_| anyhow!("gbrain 环境变量状态锁已中毒"))?;

    for (key, original) in originals.drain() {
        if let Some(value) = original {
            std::env::set_var(key, value);
        } else {
            std::env::remove_var(key);
        }
    }

    for (key, value) in values {
        if !is_gbrain_env_name(key) {
            tracing::warn!(name = %key, "内嵌 gbrain 已忽略非 GBRAIN_* 环境变量");
            continue;
        }
        if value.contains('\0') {
            return Err(anyhow!("gbrain 环境变量 {key} 包含 NUL 字符"));
        }
        originals.insert(key.clone(), std::env::var_os(key));
        std::env::set_var(key, value);
    }
    Ok(())
}

fn is_gbrain_env_name(name: &str) -> bool {
    name.starts_with("GBRAIN_")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
}

#[cfg(target_os = "android")]
pub mod jni_bridge {
    //! 单 .so 宿主面（阶段 4）：`Java_ai_rclaw_app_*` JNI 导出已上移到
    //! `rclaw-desktop` 的 `android_host` 模块；这里只保留桥执行所需的
    //! JavaVM / PythonRuntime 全局引用与 JNI 调用内核。

    use anyhow::{anyhow, Context, Result};
    use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
    use jni::{JNIEnv, JavaVM};
    use serde_json::Value;
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    static JAVA_VM: OnceLock<JavaVM> = OnceLock::new();
    static PYTHON_BRIDGE: OnceLock<Mutex<Option<GlobalRef>>> = OnceLock::new();
    /// NativeBridge 类的全局引用：attached 原生线程上 FindClass 走系统类加载器，
    /// 找不到应用类（真机 FATAL：ClassNotFoundException → 进程被杀）。类引用
    /// 必须在 Java 线程上下文（nativeStart 导出）缓存一次，之后所有 JNI 调用取缓存。
    static NATIVE_BRIDGE_CLASS: OnceLock<GlobalRef> = OnceLock::new();

    /// Java 线程上下文调用（nativeStart）：缓存 NativeBridge 类。
    pub fn cache_native_bridge_class(env: &mut JNIEnv<'_>) -> Result<()> {
        if NATIVE_BRIDGE_CLASS.get().is_some() {
            return Ok(());
        }
        let class = env
            .find_class("ai/rclaw/app/NativeBridge")
            .context("找不到 NativeBridge 类")?;
        let global = env
            .new_global_ref(&class)
            .context("缓存 NativeBridge 类引用失败")?;
        let _ = NATIVE_BRIDGE_CLASS.set(global);
        Ok(())
    }

    /// 取 NativeBridge 类：优先全局缓存；兜底 find_class（仅 Java 线程上下文可用，
    /// 失败时清掉挂起异常避免跨界 FATAL）。
    pub(super) fn native_bridge_class<'local>(env: &mut JNIEnv<'local>) -> Result<JClass<'local>> {
        if let Some(global) = NATIVE_BRIDGE_CLASS.get() {
            // 从全局引用建本地引用（JObject 无 Clone；NewLocalRef 一次 JNI 调用，
            // 相对系统调用可忽略）。
            let obj = env.new_local_ref(global.as_obj())?;
            return Ok(JClass::from(obj));
        }
        let class = env
            .find_class("ai/rclaw/app/NativeBridge")
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("NativeBridge 类不可达（缓存未建立且当前非 Java 线程上下文）: {error}")
            })?;
        Ok(class)
    }

    /// 记录 JavaVM（单 .so 下由 `android_host` 在 `nativeStart` 经
    /// `JNIEnv::get_java_vm` 取得并注入；不再依赖 `JNI_OnLoad`）。
    pub fn set_java_vm(vm: JavaVM) {
        let _ = JAVA_VM.set(vm);
    }

    pub fn set_python_bridge(env: &JNIEnv<'_>, bridge: JObject<'_>) -> Result<()> {
        let global = env
            .new_global_ref(bridge)
            .context("创建 PythonRuntime JNI 全局引用失败")?;
        *PYTHON_BRIDGE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| anyhow!("PythonRuntime JNI 状态锁已中毒"))? = Some(global);
        Ok(())
    }

    pub(super) fn run_python(
        script: &Path,
        args_json: &str,
        cwd: &Path,
        env_json: &str,
        timeout_ms: u64,
        limit_bytes: u64,
    ) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let bridge = PYTHON_BRIDGE
            .get_or_init(|| Mutex::new(None))
            .lock()
            .map_err(|_| anyhow!("PythonRuntime JNI 状态锁已中毒"))?
            .clone()
            .ok_or_else(|| anyhow!("PythonRuntime 尚未注册"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加 Python skill 线程到 JavaVM 失败")?;
        let script = env.new_string(script.to_string_lossy().as_ref())?;
        let args = env.new_string(args_json)?;
        let cwd = env.new_string(cwd.to_string_lossy().as_ref())?;
        let env_json = env.new_string(env_json)?;
        let script_object = JObject::from(script);
        let args_object = JObject::from(args);
        let cwd_object = JObject::from(cwd);
        let env_object = JObject::from(env_json);
        let value = match env.call_method(
            bridge.as_obj(),
            "runScript",
            "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;JJ)Ljava/lang/String;",
            &[
                JValue::Object(&script_object),
                JValue::Object(&args_object),
                JValue::Object(&cwd_object),
                JValue::Object(&env_object),
                JValue::Long(timeout_ms as i64),
                JValue::Long(limit_bytes as i64),
            ],
        ) {
            Ok(value) => value,
            Err(error) => {
                // Convert Java/Chaquopy startup failures into a normal tool
                // error. A pending JNI exception must be cleared before this
                // attached Rust worker can safely make another JNI call.
                let _ = env.exception_clear();
                return Err(anyhow!("调用 APK Python runtime 失败: {error}"));
            }
        }
        .l()?;
        let value = JString::from(value);
        let result = env.get_string(&value)?.to_string_lossy().into_owned();
        Ok(result)
    }

    /// 系统浏览器打开链接：调 NativeBridge.openUrl（Java 静态方法，内部用
    /// init 时持有的 application context 起 ACTION_VIEW intent）。
    pub(super) fn open_url(url: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加打开链接线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jurl = env.new_string(url)?;
        let result = env
            .call_static_method(
                class,
                "openUrl",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&JObject::from(jurl))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.openUrl 失败: {error}")
            })?;
        // Java 侧返回 false（无 context / Activity 启动失败）不能当成功吞掉，
        // 否则 GUI 表现为"点了没反应且无报错"。
        if !result.z().unwrap_or(false) {
            return Err(anyhow!("系统未受理打开链接请求（无可用浏览器或链接非法）"));
        }
        Ok(())
    }

    /// 应用内更新：调 NativeBridge.installUpdate 拉起系统安装器（FileProvider
    /// content URI + ACTION_VIEW，FLAG_NEW_TASK——从非 Activity 上下文启动必需）。
    pub(super) fn install_update(apk_abs: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加安装更新线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jpath = env.new_string(apk_abs)?;
        let result = env
            .call_static_method(
                class,
                "installUpdate",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&JObject::from(jpath))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.installUpdate 失败: {error}")
            })?;
        if !result.z().unwrap_or(false) {
            return Err(anyhow!(
                "系统未受理安装请求（文件缺失或需先授予「安装未知应用」权限）"
            ));
        }
        Ok(())
    }

    /// 前台服务通知状态更新（Rust android_status 推送 → NativeBridge。
    /// updateRuntimeStatus → RClawService.applyRuntimeStatus）。
    pub(super) fn update_runtime_status(text: &str, busy: bool) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加状态推送线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jtext = env.new_string(text)?;
        env.call_static_method(
            class,
            "updateRuntimeStatus",
            "(Ljava/lang/String;Z)V",
            &[
                JValue::Object(&JObject::from(jtext)),
                JValue::Bool(if busy { 1 } else { 0 }),
            ],
        )
        .map_err(|error| {
            let _ = env.exception_clear();
            anyhow!("调用 NativeBridge.updateRuntimeStatus 失败: {error}")
        })?;
        Ok(())
    }

    /// turn 收尾提醒通知（NativeBridge.notifyTurnDone，独立「任务完成」通道）。
    pub(super) fn notify_turn_done(text: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加完成通知线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jtext = env.new_string(text)?;
        env.call_static_method(
            class,
            "notifyTurnDone",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&JObject::from(jtext))],
        )
        .map_err(|error| {
            let _ = env.exception_clear();
            anyhow!("调用 NativeBridge.notifyTurnDone 失败: {error}")
        })?;
        Ok(())
    }

    /// CDP 浏览器会话：确保长驻调试 WebView 在跑，返回 pid + 真实视口（拼
    /// DevTools socket 名 webview_devtools_remote_<pid>）。阻塞至就绪（至多 15s）。
    pub(super) fn browser_session_start() -> Result<super::BrowserSessionInfo> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加 CDP 浏览器线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let result = env
            .call_static_method(class, "browserSessionStart", "()Ljava/lang/String;", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserSessionStart 失败: {error}")
            })?
            .l()?;
        let value = JString::from(result);
        let payload = env.get_string(&value)?.to_string_lossy().into_owned();
        parse_session_reply(&payload)
    }

    /// start/attach/detach 共用的 JSON 应答解析：ok=false 带错误串。
    fn parse_session_reply(payload: &str) -> Result<super::BrowserSessionInfo> {
        let reply: serde_json::Value = serde_json::from_str(payload)
            .map_err(|e| anyhow!("解析会话应答失败: {e}（原文: {payload}）"))?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(anyhow!(
                "CDP 浏览器会话操作失败: {}",
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ));
        }
        Ok(super::BrowserSessionInfo {
            pid: reply
                .get("pid")
                .and_then(Value::as_u64)
                .ok_or_else(|| anyhow!("会话应答缺少 pid"))? as u32,
            epoch: reply.get("epoch").and_then(Value::as_u64).unwrap_or(0) as u32,
            width: reply.get("width").and_then(Value::as_i64).unwrap_or(0),
            height: reply.get("height").and_then(Value::as_i64).unwrap_or(0),
            dpr: reply.get("dpr").and_then(Value::as_f64).unwrap_or(1.0),
        })
    }

    /// CDP 浏览器悬浮窗：展开面板并挂载 WebView（风控页人工过码入口）。
    pub(super) fn browser_overlay_expand() -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加悬浮窗线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let result = env
            .call_static_method(class, "browserOverlayExpand", "()Ljava/lang/String;", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserOverlayExpand 失败: {error}")
            })?
            .l()?;
        let value = JString::from(result);
        let payload = env.get_string(&value)?.to_string_lossy().into_owned();
        let reply: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| anyhow!("解析悬浮窗应答失败: {e}（原文: {payload}）"))?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(anyhow!(
                "悬浮窗展开失败: {}",
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ));
        }
        Ok(())
    }

    /// CDP 浏览器悬浮窗：展开面板并导航到指定 http(s) URL（聊天链接默认
    /// 应用内打开；复用地址栏同一导航通路）。
    pub(super) fn browser_overlay_open_url(url: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加悬浮窗线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jurl = env
            .new_string(url)
            .map_err(|e| anyhow!("创建 jstring 失败: {e}"))?;
        let result = env
            .call_static_method(
                class,
                "browserOverlayOpenUrl",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[(&jurl).into()],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserOverlayOpenUrl 失败: {error}")
            })?
            .l()?;
        let value = JString::from(result);
        let payload = env.get_string(&value)?.to_string_lossy().into_owned();
        let reply: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| anyhow!("解析悬浮窗应答失败: {e}（原文: {payload}）"))?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(anyhow!(
                "悬浮窗打开链接失败: {}",
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ));
        }
        Ok(())
    }

    /// CDP 浏览器悬浮窗：收起为气泡（WebView 摘回离屏）。
    pub(super) fn browser_overlay_collapse() -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加悬浮窗线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let result = env
            .call_static_method(class, "browserOverlayCollapse", "()Ljava/lang/String;", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserOverlayCollapse 失败: {error}")
            })?
            .l()?;
        let value = JString::from(result);
        let payload = env.get_string(&value)?.to_string_lossy().into_owned();
        let reply: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| anyhow!("解析悬浮窗应答失败: {e}（原文: {payload}）"))?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(anyhow!(
                "悬浮窗收起失败: {}",
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ));
        }
        Ok(())
    }

    /// CDP 浏览器悬浮窗：摘窗（WebView 回离屏，不销毁会话）。
    pub(super) fn browser_overlay_dismiss() -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加悬浮窗线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let result = env
            .call_static_method(class, "browserOverlayDismiss", "()Ljava/lang/String;", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserOverlayDismiss 失败: {error}")
            })?
            .l()?;
        let value = JString::from(result);
        let payload = env.get_string(&value)?.to_string_lossy().into_owned();
        let reply: serde_json::Value = serde_json::from_str(&payload)
            .map_err(|e| anyhow!("解析悬浮窗应答失败: {e}（原文: {payload}）"))?;
        if reply.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(anyhow!(
                "悬浮窗关闭失败: {}",
                reply
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("未知原因")
            ));
        }
        Ok(())
    }

    /// WebView 当前是否挂在悬浮窗面板里（截图分流查询）。
    pub(super) fn browser_overlay_attached() -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加悬浮窗查询线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let result = env
            .call_static_method(class, "browserOverlayAttached", "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserOverlayAttached 失败: {error}")
            })?;
        Ok(result.z().unwrap_or(false))
    }

    /// Google 风控页命中：高优通知 + 尽力自动展开悬浮窗（失败由 Java 侧吞掉记
    /// 日志，不阻断 park 等待）。
    pub(super) fn notify_captcha_detected(url: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加过码通知线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jurl = env.new_string(url)?;
        env.call_static_method(
            class,
            "notifyCaptchaDetected",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&JObject::from(jurl))],
        )
        .map_err(|error| {
            let _ = env.exception_clear();
            anyhow!("调用 NativeBridge.notifyCaptchaDetected 失败: {error}")
        })?;
        Ok(())
    }

    /// CDP 浏览器会话：销毁长驻 WebView（browser_close 工具）。
    pub(super) fn browser_session_stop() -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加 CDP 浏览器线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        env.call_static_method(class, "browserSessionStop", "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserSessionStop 失败: {error}")
            })?;
        Ok(())
    }

    /// CDP 浏览器会话：离屏截图到 PNG（View.draw 软件位图，不走 CDP——离屏
    /// WebView 无合成器帧）。阻塞至绘制完成（至多 20s）。
    pub(super) fn browser_session_screenshot(abs_path: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加 CDP 浏览器线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jpath = env.new_string(abs_path)?;
        let result = env
            .call_static_method(
                class,
                "browserSessionScreenshot",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&JObject::from(jpath))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.browserSessionScreenshot 失败: {error}")
            })?;
        if !result.z().unwrap_or(false) {
            return Err(anyhow!("截图失败（会话未启动或文件不可写）"));
        }
        Ok(())
    }

    /// 应用 GUI 是否在前台（MainActivity onResume/onPause 维护的标志）。
    pub(super) fn app_foreground() -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加前台查询线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let result = env
            .call_static_method(class, "isAppForeground", "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.isAppForeground 失败: {error}")
            })?;
        Ok(result.z().unwrap_or(true))
    }

    /// 会话内点开工作区媒体：调 NativeBridge.openMedia（FileProvider 只读 URI
    ///  → 系统播放器/查看器）。
    pub(super) fn open_media(relative_path: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加打开媒体线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jpath = env.new_string(relative_path)?;
        let result = env
            .call_static_method(
                class,
                "openMedia",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&JObject::from(jpath))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.openMedia 失败: {error}")
            })?;
        if !result.z().unwrap_or(false) {
            return Err(anyhow!("系统未受理打开媒体请求（文件不存在或无可用应用）"));
        }
        Ok(())
    }

    /// 转发媒体到其它应用：NativeBridge.shareFile（ACTION_SEND + FileProvider
    /// 只读 URI → 系统分享面板，微信/QQ 等接收）。
    pub(super) fn share_file(relative_path: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加转发线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jpath = env.new_string(relative_path)?;
        let result = env
            .call_static_method(
                class,
                "shareFile",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&JObject::from(jpath))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.shareFile 失败: {error}")
            })?;
        if !result.z().unwrap_or(false) {
            return Err(anyhow!("系统未受理转发请求（文件不存在或无可用应用）"));
        }
        Ok(())
    }

    /// 文档保存到系统下载目录（MediaStore.Downloads）。返回值约定同 saveToGallery。
    pub(super) fn save_to_downloads(abs_path: &str) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加保存线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let jpath = env.new_string(abs_path)?;
        let value = env
            .call_static_method(
                class,
                "saveToDownloads",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&JObject::from(jpath))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.saveToDownloads 失败: {error}")
            })?;
        let object = value.l()?;
        jstring_to_string(&mut env, object)
    }

    /// 纯文本分享到其它应用（微信/QQ 收为聊天文字）。
    pub(super) fn share_text(title: &str, text: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加分享线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jtitle = env.new_string(title)?;
        let jtext = env.new_string(text)?;
        let result = env
            .call_static_method(
                class,
                "shareText",
                "(Ljava/lang/String;Ljava/lang/String;)Z",
                &[
                    JValue::Object(&JObject::from(jtitle)),
                    JValue::Object(&JObject::from(jtext)),
                ],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.shareText 失败: {error}")
            })?;
        if !result.z().unwrap_or(false) {
            return Err(anyhow!("系统未受理分享请求（无可用分享目标）"));
        }
        Ok(())
    }

    /// 读系统剪贴板落盘到 dest_dir。返回 "ok:<文件名>" / "empty" / "error:<原因>"。
    pub(super) fn read_clipboard_to_file(dest_dir: &str) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加剪贴板线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jdir = env.new_string(dest_dir)?;
        let value = env
            .call_static_method(
                class,
                "readClipboardToFile",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&JObject::from(jdir))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.readClipboardToFile 失败: {error}")
            })?;
        let object = value.l()?;
        jstring_to_string(&mut env, object)
    }

    /// 把文件以 FileProvider URI 写入系统剪贴板（文件管理器可粘贴）。
    pub(super) fn copy_file_to_clipboard(abs_path: &str) -> Result<()> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加剪贴板线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let jpath = env.new_string(abs_path)?;
        let result = env
            .call_static_method(
                class,
                "copyFileToClipboard",
                "(Ljava/lang/String;)Z",
                &[JValue::Object(&JObject::from(jpath))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.copyFileToClipboard 失败: {error}")
            })?;
        if !result.z().unwrap_or(false) {
            return Err(anyhow!("写入系统剪贴板失败"));
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 采集（拍照/录视频/录音）与 whisper：NativeBridge 静态方法直调。
    // 错误消息在 Java 侧已字符串化（"error:<原因>"），这里透传给命令层。
    // ------------------------------------------------------------------

    /// JObject(JString) → Rust String（get_string 的借用期只在本函数内结束）。
    fn jstring_to_string(env: &mut JNIEnv, object: JObject) -> Result<String> {
        let string = JString::from(object);
        let java_str = env.get_string(&string)?;
        Ok(java_str.into())
    }

    fn call_static_string(name: &str, sig: &str, args: &[JValue]) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .with_context(|| format!("附加 {name} 线程到 JavaVM 失败"))?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let value = env
            .call_static_method(class, name, sig, args)
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.{name} 失败: {error}")
            })?;
        let object = value
            .l()
            .with_context(|| format!("NativeBridge.{name} 返回值转换失败"))?;
        jstring_to_string(&mut env, object)
    }

    pub(super) fn capture(kind: &str) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加采集线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let jkind = env.new_string(kind)?;
        let value = env
            .call_static_method(
                class,
                "capture",
                "(Ljava/lang/String;)Ljava/lang/String;",
                &[JValue::Object(&JObject::from(jkind))],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.capture 失败: {error}")
            })?;
        let object = value.l()?;
        jstring_to_string(&mut env, object)
    }

    pub(super) fn record_start() -> Result<String> {
        call_static_string("recordStart", "()Ljava/lang/String;", &[])
    }

    pub(super) fn record_stop() -> Result<String> {
        call_static_string("recordStop", "()Ljava/lang/String;", &[])
    }

    pub(super) fn record_active() -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let value = env
            .call_static_method(class, "recordActive", "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.recordActive 失败: {error}")
            })?;
        Ok(value.z()?)
    }

    pub(super) fn record_cancel() -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let value = env
            .call_static_method(class, "recordCancel", "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.recordCancel 失败: {error}")
            })?;
        Ok(value.z()?)
    }

    pub(super) fn save_to_gallery(abs_path: &str, kind: &str) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let jpath = env.new_string(abs_path)?;
        let jkind = env.new_string(kind)?;
        let value = env
            .call_static_method(
                class,
                "saveToGallery",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[
                    JValue::Object(&JObject::from(jpath)),
                    JValue::Object(&JObject::from(jkind)),
                ],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.saveToGallery 失败: {error}")
            })?;
        let object = value.l()?;
        jstring_to_string(&mut env, object)
    }

    pub(super) fn decode_to_wav(audio_abs: &str, wav_abs: &str) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let jaudio = env.new_string(audio_abs)?;
        let jwav = env.new_string(wav_abs)?;
        let value = env
            .call_static_method(
                class,
                "decodeToWav",
                "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[
                    JValue::Object(&JObject::from(jaudio)),
                    JValue::Object(&JObject::from(jwav)),
                ],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.decodeToWav 失败: {error}")
            })?;
        let object = value.l()?;
        jstring_to_string(&mut env, object)
    }

    pub(super) fn load_whisper_lib() -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let value = env
            .call_static_method(class, "loadWhisperLib", "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.loadWhisperLib 失败: {error}")
            })?;
        Ok(value.z()?)
    }

    pub(super) fn whisper_transcribe(model: &str, wav: &str, lang: &str) -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加 whisper 线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let jmodel = env.new_string(model)?;
        let jwav = env.new_string(wav)?;
        let jlang = env.new_string(lang)?;
        let value = env
            .call_static_method(
                class,
                "whisperTranscribe",
                "(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;",
                &[
                    JValue::Object(&JObject::from(jmodel)),
                    JValue::Object(&JObject::from(jwav)),
                    JValue::Object(&JObject::from(jlang)),
                ],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.whisperTranscribe 失败（库未加载或推理崩溃）: {error}")
            })?;
        let object = value.l()?;
        jstring_to_string(&mut env, object)
    }

    pub(super) fn whisper_model_download(urls: &[&str], dest_name: &str) -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加线程到 JavaVM 失败")?;
        let class = native_bridge_class(&mut env)?;
        let empty = env.new_string("")?;
        let array = env
            .new_object_array(urls.len() as i32, "java/lang/String", empty)
            .context("创建 URL 数组失败")?;
        for (i, url) in urls.iter().enumerate() {
            let jurl = env.new_string(url)?;
            env.set_object_array_element(&array, i as i32, jurl)?;
        }
        let jdest = env.new_string(dest_name)?;
        let value = env
            .call_static_method(
                class,
                "downloadWhisperModel",
                "([Ljava/lang/String;Ljava/lang/String;)Z",
                &[
                    JValue::Object(&JObject::from(array)),
                    JValue::Object(&JObject::from(jdest)),
                ],
            )
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.downloadWhisperModel 失败: {error}")
            })?;
        Ok(value.z()?)
    }

    /// 协作中断（超时处置层 2 的路线 B）：调桥模块的 `interrupt_main()`。
    pub(super) fn interrupt_python() {
        let run = || -> Result<()> {
            let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
            let bridge = PYTHON_BRIDGE
                .get_or_init(|| Mutex::new(None))
                .lock()
                .map_err(|_| anyhow!("PythonRuntime JNI 状态锁已中毒"))?
                .clone()
                .ok_or_else(|| anyhow!("PythonRuntime 尚未注册"))?;
            let mut env = vm
                .attach_current_thread()
                .context("附加中断线程到 JavaVM 失败")?;
            env.call_method(bridge.as_obj(), "interruptScript", "()Z", &[])
                .map_err(|error| {
                    let _ = env.exception_clear();
                    anyhow!("调用 APK Python 中断入口失败: {error}")
                })?;
            Ok(())
        };
        if let Err(error) = run() {
            tracing::warn!(%error, "向 APK Python 运行时注入中断失败（继续按超时处置）");
        }
    }

    // ------------------------------------------------------------------
    // 保活引导（NativeBridge.keepAlive* → KeepAliveGuide），见 crate::keep_alive。
    // ------------------------------------------------------------------

    fn call_static_bool(name: &str) -> Result<bool> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .with_context(|| format!("附加 {name} 线程到 JavaVM 失败"))?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let value = env
            .call_static_method(class, name, "()Z", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.{name} 失败: {error}")
            })?;
        Ok(value.z()?)
    }

    /// 保活状态 JSON（Java 无 application context 时返回 null → 报错而非空串）。
    pub(super) fn keep_alive_status() -> Result<String> {
        let vm = JAVA_VM.get().ok_or_else(|| anyhow!("JavaVM 尚未初始化"))?;
        let mut env = vm
            .attach_current_thread()
            .context("附加保活查询线程到 JavaVM 失败")?;
        let class =
            native_bridge_class(&mut env).map_err(|e| anyhow!("获取 NativeBridge 类失败: {e}"))?;
        let value = env
            .call_static_method(class, "keepAliveStatus", "()Ljava/lang/String;", &[])
            .map_err(|error| {
                let _ = env.exception_clear();
                anyhow!("调用 NativeBridge.keepAliveStatus 失败: {error}")
            })?;
        let object = value.l()?;
        if object.is_null() {
            return Err(anyhow!("应用未就绪（宿主上下文未初始化，稍后重试）"));
        }
        jstring_to_string(&mut env, object)
    }

    pub(super) fn keep_alive_request_battery_whitelist() -> Result<bool> {
        call_static_bool("keepAliveRequestBatteryWhitelist")
    }

    pub(super) fn keep_alive_open_oem_settings() -> Result<bool> {
        call_static_bool("keepAliveOpenOemSettings")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn runtime_heartbeat_expires_after_stale_threshold() {
        assert!(!heartbeat_is_fresh(1_000, 0));
        assert!(heartbeat_is_fresh(151_000, 1_000));
        assert!(!heartbeat_is_fresh(151_001, 1_000));
        assert!(!heartbeat_is_fresh(999, 1_000));
    }

    // 断言依赖 POSIX 绝对路径语义（/data/user/0/... 判定与 join 分隔符），
    // Windows 宿主跑不了——私有路径切换在真机（unix）上验证。
    #[test]
    #[cfg(unix)]
    fn complete_app_toml_maps_all_sections_and_switches_to_private_paths() {
        let toml = r#"
[agent]
base_dir = 'D:\\desktop\\rclaw'
session_compression = true
compression_protect_first = 2
compression_keep_recent = 6
compression_summary_max_tokens = 12000
auto_memory = true
auto_evolve = true
auto_review_tooled = false
auto_review_max_iters = 9
memory_review_nudge_interval = 7
skill_review_nudge_interval = 11

[llm]
provider = "zhipu"
model = "glm-4.5-flash"
stream = true
max_tokens = 8192
max_tool_calls = 24

[[llm.providers]]
name = "zhipu"
base_url = "https://open.bigmodel.cn/api/paas/v4"
api_key = "test-key"
model = "glm-4.5-flash"
auth = "bearer"
reasoning_field = "reasoning_content"
max_tokens_key = "max_tokens"
context_length = 131072
supports_tool_choice = false

[mcp]
auto_start = true
startup_timeout_secs = 19
call_timeout_secs = 31

[mcp.servers.gbrain]
enabled = false
auto_start = true
command = "desktop-gbrain"
args = ["--stdio"]
cwd = 'D:\\desktop\\gbrain'
startup_timeout_secs = 23
call_timeout_secs = 37

[mcp.servers.gbrain.env]
GBRAIN_DIR = 'D:\\desktop\\gbrain-data'
GBRAIN_DB_PATH = 'D:\\desktop\\gbrain-data\\brain.db'
GBRAIN_KB_ENABLED = "true"

[weixin]
enabled = false
token = "wx"
api_base = "https://ilinkai.weixin.qq.com"

[napcat]
enabled = false
ws_url = "ws://127.0.0.1:3001"
access_token = "napcat"
allow_from = "10001"
allow_groups = "20001"
block_from = "10002"
max_from = "123"
group_response_mode = "at_only"
heartbeat_interval_sec = 41
reconnect_delay_sec = 6

[web_search]
enabled = false
order = ["tavily", "zhipu"]
fallback = true
read_top = 2
total_timeout_secs = 47

[web_search.zhipu]
enabled = true
api_key = "web-key"

[web_search.tavily]
enabled = true
api_keys = ["tavily-key"]
max_results = 8
search_depth = "advanced"

[cron]
check_interval_sec = 17

[heartbeat]
interval_sec = 29

[log]
console = true
console_level = "debug"
file = true
file_level = "info"
dir = "custom-logs"
file_name = "agent.log"
"#;
        let launch = AndroidConfig {
            files_dir: "/data/user/0/ai.rclaw.app/files".into(),
            toml_config: Some(toml.into()),
            skill_configs: HashMap::from([(
                "glm-master-skill/config.toml".into(),
                "[zhipu]\napi_key = \"skill-key\"\n".into(),
            )]),
            ..AndroidConfig::default()
        };
        let resolved = resolve_android_config(launch).unwrap();
        assert!(resolved.config.agent.auto_evolve);
        assert_eq!(resolved.config.agent.auto_review_tooled, Some(false));
        assert_eq!(resolved.config.agent.auto_review_max_iters, 9);
        assert_eq!(resolved.config.agent.memory_review_nudge_interval, 7);
        assert_eq!(resolved.config.agent.skill_review_nudge_interval, 11);
        assert_eq!(resolved.config.llm.max_tool_calls, 24);
        assert!(!resolved.config.llm.providers[0].supports_tool_choice);
        assert_eq!(resolved.config.cron.check_interval_sec, 17);
        assert_eq!(resolved.config.heartbeat.interval_sec, 29);
        assert_eq!(resolved.config.web_search.read_top, 2);
        assert_eq!(resolved.config.web_search.total_timeout_secs, 47);
        assert_eq!(resolved.config.web_search.order, vec!["tavily", "zhipu"]);
        assert_eq!(resolved.config.napcat.allow_from.as_deref(), Some("10001"));
        assert_eq!(
            resolved.config.napcat.allow_groups.as_deref(),
            Some("20001")
        );
        assert_eq!(resolved.config.napcat.block_from.as_deref(), Some("10002"));
        assert_eq!(resolved.config.napcat.heartbeat_interval_sec, 41);
        assert_eq!(resolved.config.napcat.reconnect_delay_sec, 6);
        assert_eq!(
            Path::new(&resolved.config.agent.base_dir),
            Path::new("/data/user/0/ai.rclaw.app/files/rclaw")
        );
        assert_eq!(
            resolved.workspace_dir,
            Path::new(&resolved.config.agent.base_dir)
        );
        assert_eq!(
            Path::new(&resolved.config.log.dir),
            Path::new("/data/user/0/ai.rclaw.app/files/rclaw/custom-logs")
        );
        assert_eq!(resolved.config.log.file_name, "agent.log");
        assert_eq!(
            resolved.skill_configs_dir,
            Path::new("/data/user/0/ai.rclaw.app/no_backup/rclaw-skill-configs")
        );
        assert_eq!(resolved.skill_configs.len(), 1);
        let gbrain = resolved.config.mcp.servers.get("gbrain").unwrap();
        assert!(!gbrain.enabled);
        assert_eq!(
            gbrain.env.get("GBRAIN_KB_ENABLED").map(String::as_str),
            Some("true")
        );
        assert_eq!(configured_gbrain_call_timeout(&resolved), None);

        let mut enabled = resolved;
        enabled.config.mcp.auto_start = true;
        {
            let gbrain = enabled.config.mcp.servers.get_mut("gbrain").unwrap();
            gbrain.enabled = true;
            gbrain.auto_start = None;
        }
        // startup_timeout_secs 的映射直接断言（原 configured_startup_timeout
        // 随 v0.2.0 host 线程路径删除，v2 装配不再消费该值）。
        assert_eq!(
            enabled
                .config
                .mcp
                .servers
                .get("gbrain")
                .unwrap()
                .startup_timeout_secs,
            Some(23)
        );
        assert_eq!(
            configured_gbrain_call_timeout(&enabled),
            Some(Duration::from_secs(37))
        );
        enabled
            .config
            .mcp
            .servers
            .get_mut("gbrain")
            .unwrap()
            .auto_start = Some(false);
        assert_eq!(configured_gbrain_call_timeout(&enabled), None);
    }

    #[test]
    fn safe_file_name_accepts_only_one_normal_component() {
        assert_eq!(safe_file_name("agent.log", "rclaw.log"), "agent.log");
        assert_eq!(safe_file_name(".", "rclaw.log"), "rclaw.log");
        assert_eq!(safe_file_name("../agent.log", "rclaw.log"), "rclaw.log");
        assert_eq!(safe_file_name("logs/agent.log", "rclaw.log"), "rclaw.log");
    }

    #[test]
    fn skill_configs_accept_only_known_config_files_and_replace_installed_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let destination = temp.path().join("skills");
        fs::create_dir_all(destination.join("demo")).unwrap();
        fs::write(destination.join("demo/config.toml"), "api_key = \"\"\n").unwrap();
        let configs = HashMap::from([(
            "demo/config.toml".to_string(),
            "api_key = \"private\"\n".to_string(),
        )]);
        apply_skill_configs(&configs, &destination).unwrap();
        assert_eq!(
            fs::read_to_string(destination.join("demo/config.toml")).unwrap(),
            "api_key = \"private\"\n"
        );

        for invalid in [
            "config.toml",
            "../config.toml",
            "demo/script.py",
            "demo\\config.toml",
            "/demo/config.toml",
        ] {
            assert!(
                validate_skill_config_relative_path(invalid).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn bundled_skill_merge_updates_pristine_files_and_preserves_user_edits() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let baseline = temp.path().join("baseline");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("pristine.py"), "v1").unwrap();
        fs::write(source.join("custom.py"), "v1").unwrap();
        install_bundled_skills(&source, &destination, &baseline).unwrap();

        fs::write(destination.join("custom.py"), "user edit").unwrap();
        fs::write(source.join("pristine.py"), "v2").unwrap();
        fs::write(source.join("custom.py"), "v2").unwrap();
        install_bundled_skills(&source, &destination, &baseline).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("pristine.py")).unwrap(),
            "v2"
        );
        assert_eq!(
            fs::read_to_string(destination.join("custom.py")).unwrap(),
            "user edit"
        );
        assert_eq!(
            fs::read_to_string(baseline.join("custom.py")).unwrap(),
            "v2"
        );

        fs::remove_file(source.join("pristine.py")).unwrap();
        fs::remove_file(source.join("custom.py")).unwrap();
        install_bundled_skills(&source, &destination, &baseline).unwrap();
        assert!(!destination.join("pristine.py").exists());
        assert_eq!(
            fs::read_to_string(destination.join("custom.py")).unwrap(),
            "user edit"
        );
        assert!(!baseline.join("pristine.py").exists());
        assert!(!baseline.join("custom.py").exists());
    }

    #[test]
    fn bundled_skill_merge_handles_file_directory_shape_changes() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let destination = temp.path().join("destination");
        let baseline = temp.path().join("baseline");
        fs::create_dir_all(&source).unwrap();

        fs::write(source.join("pristine"), "v1").unwrap();
        fs::write(source.join("custom"), "v1").unwrap();
        install_bundled_skills(&source, &destination, &baseline).unwrap();
        fs::write(destination.join("custom"), "user edit").unwrap();

        fs::remove_file(source.join("pristine")).unwrap();
        fs::remove_file(source.join("custom")).unwrap();
        fs::create_dir_all(source.join("pristine")).unwrap();
        fs::create_dir_all(source.join("custom")).unwrap();
        fs::write(source.join("pristine/entry.py"), "v2").unwrap();
        fs::write(source.join("custom/entry.py"), "v2").unwrap();
        install_bundled_skills(&source, &destination, &baseline).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("pristine/entry.py")).unwrap(),
            "v2"
        );
        assert_eq!(
            fs::read_to_string(destination.join("custom")).unwrap(),
            "user edit"
        );
        assert_eq!(
            fs::read_to_string(baseline.join("custom/entry.py")).unwrap(),
            "v2"
        );

        fs::remove_dir_all(source.join("pristine")).unwrap();
        fs::remove_dir_all(source.join("custom")).unwrap();
        fs::write(source.join("pristine"), "v3").unwrap();
        fs::write(source.join("custom"), "v3").unwrap();
        install_bundled_skills(&source, &destination, &baseline).unwrap();

        assert_eq!(
            fs::read_to_string(destination.join("pristine")).unwrap(),
            "v3"
        );
        assert_eq!(
            fs::read_to_string(destination.join("custom")).unwrap(),
            "user edit"
        );
        assert_eq!(fs::read_to_string(baseline.join("custom")).unwrap(), "v3");
    }
}
