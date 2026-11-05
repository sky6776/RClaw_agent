//! APK 宿主胶水（阶段 4）：APK 专属的环境/通道/gbrain 准备步骤。
//! 唯一装配路径：`rclaw-desktop` 的 `agent.rs` `assemble_apk_agent`（v2，
//! GUI 与前台服务共享单 tokio runtime，`RunningAgent` 进全局槽由 tauri
//! command 访问；v0.2.0 的独立 host 线程 start/run 已删除）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use rclaw_channels::{Channel, NapCat, Weixin, Wxbot};
use rclaw_core::ChannelCaps;
use rclaw_core::RuntimeChannelInfo;
use rclaw_store::fs::{init_layout, Layout};

use crate::bridge_executor::APPROVALS_FILE_NAME;
use crate::bridge_executor::{BridgePythonExecutor, SkillApprovalStore};
use crate::{
    apply_skill_configs, install_bundled_skills, reject_symlink, reject_symlink_ancestors,
    remove_managed_node, ResolvedAndroidConfig,
};

/// 技能闸门句柄（进程级）：审批存储 + 其枚举根，供 GUI 审批命令访问。
#[derive(Clone)]
pub struct ApprovalGateHandle {
    pub store: Arc<SkillApprovalStore>,
    pub skills_dir: PathBuf,
}

static APPROVAL_GATE: OnceLock<Mutex<Option<ApprovalGateHandle>>> = OnceLock::new();

/// 注册/更新进程级闸门句柄（每次装配调用；同进程仅一条）。
fn register_approval_gate(store: Arc<SkillApprovalStore>, skills_dir: PathBuf) {
    let slot = APPROVAL_GATE.get_or_init(|| Mutex::new(None));
    *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
        Some(ApprovalGateHandle { store, skills_dir });
}

/// GUI 审批命令读取；装配完成前返回 None。
pub fn approval_gate() -> Option<ApprovalGateHandle> {
    APPROVAL_GATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// APK 宿主环境准备结果。
pub struct PreparedEnvironment {
    pub layout: Layout,
    /// 媒体沙箱目录（通道与媒体 marker 共用）。
    pub media_dir: PathBuf,
    /// gbrain 允许读取/上传的工作区根。
    pub workspace_dir: PathBuf,
}

/// v0.2.0 `run()` 的目录/环境/skill 基线准备段（原样抽出）：
/// 目录防穿越校验、skill 配置落位、APK 宿主标记、内置 skill 安装、TMPDIR 与
/// 旧版残留清理、共用 `prepare_storage`。
pub fn prepare_environment(resolved: &mut ResolvedAndroidConfig) -> Result<PreparedEnvironment> {
    let files_dir = resolved.files_dir.clone();
    let workspace_dir = resolved.workspace_dir.clone();
    reject_symlink_ancestors(&files_dir, &workspace_dir, "workspace 目录")?;
    std::fs::create_dir_all(&workspace_dir)
        .with_context(|| format!("创建 APK workspace 目录失败: {}", workspace_dir.display()))?;

    let layout = Layout::new(&resolved.config.agent.base_dir);
    reject_symlink_ancestors(&files_dir, &layout.base, "agent 目录")?;
    reject_symlink_ancestors(&files_dir, &layout.skills_dir, "skill 目录")?;
    init_layout(&layout)
        .with_context(|| format!("初始化 agent 目录布局失败: {}", layout.base.display()))?;
    let app_data_dir = files_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("files_dir 缺少 app 数据目录父级"))?;
    reject_symlink_ancestors(
        app_data_dir,
        &resolved.skill_configs_dir,
        "skill 配置根目录",
    )?;
    reject_symlink(&resolved.skill_configs_dir, "skill 配置根目录")?;
    // 只有宿主真的下发了配置才整树重建：GUI 装配抢跑时走的是 `AndroidConfig::minimal`
    //（skill_configs 为空表），若此时也清空目录，随后前台服务带完整配置的 nativeStart
    // 会因槽已占用而直接复用运行时、不再应用配置——全部 skill 凭据在本进程生命周期内
    // 丢失，依赖配置的技能整体失效。空表时保留既有文件（最多残留旧配置，下次非空下发
    // 时仍会整树重建）。
    if !resolved.skill_configs.is_empty() {
        if resolved.skill_configs_dir.exists() {
            remove_managed_node(&resolved.skill_configs_dir, "skill 配置根目录")?;
        }
        std::fs::create_dir_all(&resolved.skill_configs_dir)
            .context("创建 APK skill 配置根目录失败")?;
        apply_skill_configs(&resolved.skill_configs, &resolved.skill_configs_dir)?;
    } else {
        std::fs::create_dir_all(&resolved.skill_configs_dir)
            .context("创建 APK skill 配置根目录失败")?;
    }
    // APK 宿主标记（§4.1 三态判定）：与 Termux 同为 android target，运行期 APK
    // 专属行为一律以该标记为条件，而非 cfg!(target_os = "android")。
    std::env::set_var(rclaw_runtime::HostKind::APK_MARKER_ENV, "1");
    std::env::set_var(
        "RCLAW_SKILL_CONFIG_ROOT",
        resolved.skill_configs_dir.to_string_lossy().as_ref(),
    );
    std::env::set_current_dir(&layout.base).context("切换到 RClaw app 私有 agent 目录失败")?;
    let skill_baseline_dir = files_dir.join("runtime/skill-baseline");
    // 首启竞速兜底：GUI 装配（前端 get_status 触发）可能与前台服务的内置
    // skill 解包并发——服务在 nativeStart 前同步解包，但 GUI 不等它。曾出现
    // 恢复数据后的首启装配抢在解包前失败且错误滞留到重启。有界等待解包
    // 完成标记（目录树先拷、版本标记最后写），就绪时零等待。
    //
    // 版本握手：覆盖安装后 `files/` 数据保留，旧 marker（旧 lastUpdateTime）仍
    // 在——只查存在性的等待会秒过，而 Java 正因版本不符在原地重解包同一棵树
    // （逐文件 tmp+delete+rename+末尾清理），Rust 遍历撞上刚被 rename/delete 的
    // 条目即裸 ENOENT（= 「APK 环境准备失败: No such file or directory」事故）。
    // 因此 marker 必须与 expected 版本一致才算就绪；超时一律**跳过安装**沿用
    // agent/skills 里上一次的完整副本（覆盖安装场景旧副本完整可用；下次启动再
    // 补装），绝不与进行中的解包抢树。
    // 路径安全校验与是否安装无关：skip 分支后续的 install_skill_gate 仍会读
    // bundled_skills_dir（refresh_baseline），必须在两条分支前统一做。
    // 目标不存在时 reject_symlink_ancestors 遇 NotFound 直接 break，安全。
    reject_symlink_ancestors(
        &files_dir,
        &resolved.bundled_skills_dir,
        "APK 内置 skill 目录",
    )?;
    reject_symlink_ancestors(&files_dir, &skill_baseline_dir, "skill 基线目录")?;
    // 超时分级（避免无谓的 3 分钟启动画面）：
    // - marker 不存在（全新安装首解包）：慢盘/模拟器可能久，等满 180s；
    // - marker 存在但版本不符（覆盖安装原地重解包）：内置技能仅约 6MB，正常
    //   几秒完成，30s 未完成说明解包已失败/极慢——按 skip 继续启动更快。
    let marker_present = resolved
        .bundled_skills_dir
        .with_file_name("bundled-skills.version")
        .is_file();
    let handshake_timeout = if marker_present {
        Duration::from_secs(30)
    } else {
        Duration::from_secs(180)
    };
    let bundled_ready = wait_for_bundled_skills_ready(
        &resolved.bundled_skills_dir,
        resolved.bundled_skills_version.as_deref(),
        handshake_timeout,
        Duration::from_millis(400),
    );
    if bundled_ready {
        install_bundled_skills(
            &resolved.bundled_skills_dir,
            &layout.skills_dir,
            &skill_baseline_dir,
        )?;
    } else {
        // 超时/未就绪：沿用 agent/skills 既有副本（可能与版本不符但完整），下次
        // 启动重试握手。agent/skills 为空的全新安装会以零技能启动（boot log 可查）。
        let marker = resolved
            .bundled_skills_dir
            .with_file_name("bundled-skills.version");
        let marker_now = read_marker_prefix(&marker)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let agent_skills_present = std::fs::read_dir(&layout.skills_dir)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        crate::log_boot(&format!(
            "内置 skill 解包未就绪（expected={:?} marker={:?} source_dir={} agent_skills_present={}），本次沿用既有技能副本跳过安装，下次启动重试",
            resolved.bundled_skills_version,
            marker_now,
            resolved.bundled_skills_dir.is_dir(),
            agent_skills_present,
        ));
    }
    // Never place the app-managed TOML under `layout.base`: the model-facing
    // read tool is intentionally rooted there and the configuration contains API
    // keys. Skill credentials live in their independent app-managed config files.
    let exposed_toml = layout.base.join("rclaw.toml");
    if exposed_toml.is_file() {
        std::fs::remove_file(&exposed_toml)
            .context("清理旧版暴露在 agent 目录中的 rclaw.toml 失败")?;
    }
    let media_dir = layout.base.join("media");
    std::fs::create_dir_all(&media_dir)
        .with_context(|| format!("创建 APK media 目录失败: {}", media_dir.display()))?;
    let tmp_dir = layout.base.join("tmp");
    std::env::set_var("TMPDIR", tmp_dir.to_string_lossy().as_ref());
    std::fs::create_dir_all(&tmp_dir)
        .with_context(|| format!("创建 APK tmp 目录失败: {}", tmp_dir.display()))?;

    // Older APK builds copied the current RClaw provider into this runtime file.
    // Remove it during upgrade: skills now read only their own config files.
    std::env::remove_var("RCLAW_PROVIDER_CONFIG");
    let obsolete_provider_runtime_path = files_dir.join("runtime/skill-provider.json");
    reject_symlink(&obsolete_provider_runtime_path, "旧版 skill provider 配置")?;
    if obsolete_provider_runtime_path.is_file() {
        std::fs::remove_file(&obsolete_provider_runtime_path)
            .context("清理旧版 skill provider 配置失败")?;
    }

    // 与 CLI / 桌面版同源的存储准备（目录 + 技能仓库 Manifest 安装）。
    let layout = rclaw_cli::bootstrap::prepare_storage(&resolved.config)?;
    let media_dir = layout.base.join("media");
    Ok(PreparedEnvironment {
        layout,
        media_dir,
        workspace_dir,
    })
}

/// 等 APK 内置 skill 解包完成（版本握手）。
///
/// 就绪判定：source 是目录 + 同级 `bundled-skills.version` 标记是文件 + 标记内容
/// （trim 后）等于 `expected`。`expected=None` 时退化为旧语义（只查存在）——仅剩
/// 「极旧启动 JSON / Java setter 失败」的边角路径，resolve 侧已有 boot log 提示。
/// Java 侧 RuntimeAssets 先拷目录树（逐文件 tmp+rename）、**最后**写标记，标记即
/// 该版本的完成信号；marker 读取失败按「未就绪」处理（可能正在被替换），不传播
/// 错误——返回 bool 让调用方决定 skip，不在装配路径抛裸 io。
///
/// 返回 true = 可安全安装；false = 超时未就绪（调用方跳过安装，沿用既有副本）。
/// poll 的 sleep 用 `min(remaining)` 截断，短超时（测试）不超 sleep 一拍。
fn bundled_skills_ready(source: &Path, expected: Option<&str>) -> bool {
    let marker = source.with_file_name("bundled-skills.version");
    if !(source.is_dir() && marker.is_file()) {
        return false;
    }
    match expected {
        // 旧语义：版本未知时 marker 在即视为就绪。
        None => true,
        Some(want) => read_marker_prefix(&marker).is_some_and(|got| got.trim() == want),
    }
}

/// 读 marker 的前缀（版本号是 lastUpdateTime 数字串，正常十几字节）。
/// 用 `take` 保证短读也不误判（单次 `read` 在 POSIX 上允许只返回部分字节）；
/// 反向风险（marker 超过 128 字节且前 128 恰等于期望）按异常数据处理：直接判
/// 未就绪，避免伪造 marker 让握手误通过。
pub fn read_marker_prefix(marker: &Path) -> Option<String> {
    use std::io::Read as _;
    let metadata = std::fs::metadata(marker).ok()?;
    if metadata.len() > 128 {
        return None;
    }
    let mut out = String::new();
    std::fs::File::open(marker)
        .ok()?
        .take(128)
        .read_to_string(&mut out)
        .ok()?;
    Some(out)
}

fn wait_for_bundled_skills_ready(
    source: &Path,
    expected: Option<&str>,
    timeout: Duration,
    poll: Duration,
) -> bool {
    if bundled_skills_ready(source, expected) {
        return true;
    }
    // poll=0 会忙等（sleep(0) 立即返回）：生产 400ms、测试 5ms，这里兜底成 1ms。
    let poll = poll.max(Duration::from_millis(1));
    let deadline = std::time::Instant::now() + timeout;
    // 此刻 tracing 订阅器尚未初始化（init_tracing 在 prepare_environment 之后），
    // 走 boot_debug.log 保证可观测。
    crate::log_boot(&format!(
        "等待 APK 内置 skill 解包（版本握手，expected={expected:?}，上限 {}s）",
        timeout.as_secs()
    ));
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(poll.min(remaining));
        if bundled_skills_ready(source, expected) {
            crate::log_boot("内置 skill 解包完成（版本匹配），继续装配");
            return true;
        }
    }
    crate::log_boot("等待内置 skill 解包超时，本次沿用既有技能副本");
    false
}

/// 通道构造（能力声明在接线期采集；只有真实构造成功的通道才进入 RuntimeChannelInfo）。
pub struct ApkChannels {
    pub weixin: Option<Weixin>,
    pub napcat: Option<NapCat>,
    pub wxbot: Option<Wxbot>,
    pub info: RuntimeChannelInfo,
}

pub fn build_channels(resolved: &ResolvedAndroidConfig, media_dir: &Path) -> ApkChannels {
    let mut weixin_cfg = resolved.config.weixin_channel_config();
    weixin_cfg.media_dir = media_dir.to_path_buf();
    let weixin = if weixin_cfg.enabled {
        Weixin::new(weixin_cfg)
    } else {
        None
    };
    let mut napcat_cfg = resolved.config.napcat_channel_config();
    napcat_cfg.media_dir = media_dir.to_path_buf();
    let napcat = if napcat_cfg.enabled {
        NapCat::new(napcat_cfg)
    } else {
        None
    };
    let mut wxbot_cfg = resolved.config.wxbot_channel_config();
    wxbot_cfg.media_dir = media_dir.to_path_buf();
    let wxbot = if wxbot_cfg.enabled {
        Wxbot::new(wxbot_cfg)
    } else {
        None
    };
    let mut channel_caps: Vec<ChannelCaps> = Vec::new();
    if let Some(wx) = &weixin {
        channel_caps.push(wx.capabilities());
    }
    if let Some(nc) = &napcat {
        channel_caps.push(nc.capabilities());
    }
    if let Some(wb) = &wxbot {
        channel_caps.push(wb.capabilities());
    }
    let info = RuntimeChannelInfo {
        media_dir: Some(media_dir.to_string_lossy().replace('\\', "/")),
        channels: channel_caps,
    };
    ApkChannels {
        weixin,
        napcat,
        wxbot,
        info,
    }
}

/// 技能执行闸门：审批表装载/刷新 + 桥执行器与审批摘除钩子注入（进程级槽位）。
/// GUI 与服务的装配路径都要在 `assemble_core_with` 之前调用。
pub fn install_skill_gate(
    layout: &Layout,
    resolved: &ResolvedAndroidConfig,
) -> Result<Arc<SkillApprovalStore>> {
    let approvals_path = resolved
        .skill_configs_dir
        .parent()
        .map(|root| root.join(APPROVALS_FILE_NAME))
        .unwrap_or_else(|| resolved.files_dir.join(APPROVALS_FILE_NAME));
    let approvals = Arc::new(SkillApprovalStore::load(approvals_path));
    approvals.refresh_baseline(&resolved.bundled_skills_dir);
    approvals.seed_user_approvals(&resolved.approved_skill_scripts);
    {
        let approvals_for_hook = approvals.clone();
        rclaw_tools::set_skill_script_change_hook(Arc::new(move |path| {
            approvals_for_hook.revoke_user_approval(path);
        }));
    }
    rclaw_tools::set_python_script_executor(Arc::new(BridgePythonExecutor::new(
        layout.base.clone(),
        layout.skills_dir.clone(),
        approvals.clone(),
    )));
    register_approval_gate(approvals.clone(), layout.skills_dir.clone());
    Ok(approvals)
}

/// 进程内 gbrain 准备：告警外部 MCP、打开 gbrain、摘除 MCP server 表
/// （APK 不 spawn 任何 stdio 子进程），返回 (句柄, 调用超时)。
///
/// gbrain 环境变量按进程级持续注入（守卫显式 forget——变量需在 gbrain 存活的
/// 全程保持，进程退出时随进程消亡）。
pub fn prepare_gbrain(
    resolved: &mut ResolvedAndroidConfig,
) -> Result<(
    Option<rclaw_gbrain::GBrainTools>,
    Option<std::time::Duration>,
)> {
    let gbrain_call_timeout = crate::configured_gbrain_call_timeout(resolved);
    let _guard = crate::GBrainEnvGuardForProcess;
    for (name, server) in &resolved.config.mcp.servers {
        if name != "gbrain"
            && server.enabled
            && server.auto_start.unwrap_or(resolved.config.mcp.auto_start)
        {
            tracing::warn!(server = %name, "APK 不启动外部 MCP 子进程；已忽略该 server");
        }
    }
    let workspace_dir = resolved.workspace_dir.clone();
    let gbrain = crate::open_gbrain(resolved, &workspace_dir)?;
    resolved.config.mcp.servers.clear();
    std::mem::forget(_guard);
    Ok((gbrain, gbrain_call_timeout))
}

#[cfg(test)]
mod tests {
    use super::{bundled_skills_ready, wait_for_bundled_skills_ready};
    use std::time::Duration;

    fn seed(source: &std::path::Path, marker_content: Option<&str>) {
        std::fs::create_dir_all(source).unwrap();
        if let Some(content) = marker_content {
            let marker = source.with_file_name("bundled-skills.version");
            std::fs::write(&marker, content).unwrap();
        }
    }

    #[test]
    fn ready_requires_dir_and_marker() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundled-skills");
        // 目录都没有 → 未就绪
        assert!(!bundled_skills_ready(&source, None));
        seed(&source, None);
        // 目录在、marker 不在 → 未就绪
        assert!(!bundled_skills_ready(&source, None));
        seed(&source, Some("100"));
        // expected=None：marker 在即就绪（旧语义）
        assert!(bundled_skills_ready(&source, None));
        // expected=Some：内容匹配才就绪
        assert!(bundled_skills_ready(&source, Some("100")));
        assert!(!bundled_skills_ready(&source, Some("200")));
        // marker 尾随换行被 trim（Java writeMarker 写裸字节，可能带换行）
        std::fs::write(source.with_file_name("bundled-skills.version"), "100\n").unwrap();
        assert!(bundled_skills_ready(&source, Some("100")));
    }

    #[test]
    fn marker_as_directory_is_not_ready() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundled-skills");
        seed(&source, None);
        // marker 路径被目录占用：is_file 为假 → 未就绪（不 panic、不抛错）
        std::fs::create_dir_all(source.with_file_name("bundled-skills.version")).unwrap();
        assert!(!bundled_skills_ready(&source, Some("100")));
        assert!(!wait_for_bundled_skills_ready(
            &source,
            Some("100"),
            Duration::from_millis(20),
            Duration::from_millis(5),
        ));
    }

    /// 握手四组合（只覆盖 wait 的 ready 输入；prepare_environment 里
    /// `if bundled_ready { install } else { skip }` 的接线由 code review 保证）。
    /// ready 才安装：覆盖安装期间 Java 正在原地重解包，遍历即可能 ENOENT。
    #[test]
    fn handshake_ready_combinations() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundled-skills");
        let short = Duration::from_millis(20);
        let poll = Duration::from_millis(5);

        // 组合 1：marker 版本匹配 → 就绪 → 安装
        seed(&source, Some("new"));
        assert!(wait_for_bundled_skills_ready(
            &source,
            Some("new"),
            short,
            poll
        ));

        // 组合 2：marker 是旧版本（覆盖安装典型）→ 超时 → skip
        std::fs::write(source.with_file_name("bundled-skills.version"), "old").unwrap();
        assert!(!wait_for_bundled_skills_ready(
            &source,
            Some("new"),
            short,
            poll
        ));

        // 组合 3：marker 缺失（全新安装未解包完）→ 超时 → skip
        std::fs::remove_file(source.with_file_name("bundled-skills.version")).unwrap();
        assert!(!wait_for_bundled_skills_ready(
            &source,
            Some("new"),
            short,
            poll
        ));

        // 组合 4：expected=None 且 marker 缺失 → 退旧语义也要等到 marker 出现，超时 skip
        assert!(!wait_for_bundled_skills_ready(&source, None, short, poll));
    }

    #[test]
    fn wait_returns_true_immediately_when_version_matches() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundled-skills");
        seed(&source, Some("42"));
        assert!(wait_for_bundled_skills_ready(
            &source,
            Some("42"),
            Duration::from_millis(50),
            Duration::from_millis(5),
        ));
    }

    #[test]
    fn wait_times_out_on_version_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundled-skills");
        seed(&source, Some("old"));
        // 旧版本 marker 不会被当成新版本就绪：50ms 内超时返回 false
        assert!(!wait_for_bundled_skills_ready(
            &source,
            Some("new"),
            Duration::from_millis(50),
            Duration::from_millis(5),
        ));
    }

    #[test]
    fn wait_becomes_ready_once_marker_updates_midway() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("bundled-skills");
        seed(&source, Some("old"));
        // 另一线程模拟解包完成：30ms 后把 marker 换成新版本（解包最后一步写 marker）
        let writer = std::thread::spawn({
            let marker = source
                .with_file_name("bundled-skills.version")
                .to_path_buf();
            move || {
                std::thread::sleep(Duration::from_millis(30));
                std::fs::write(&marker, "new").unwrap();
            }
        });
        assert!(wait_for_bundled_skills_ready(
            &source,
            Some("new"),
            Duration::from_secs(2),
            Duration::from_millis(5),
        ));
        writer.join().unwrap();
    }
}
