//! APK 宿主胶水（阶段 4）：APK 专属的环境/通道/gbrain 准备步骤。
//! 唯一装配路径：`rclaw-desktop` 的 `agent.rs` `assemble_apk_agent`（v2，
//! GUI 与前台服务共享单 tokio runtime，`RunningAgent` 进全局槽由 tauri
//! command 访问；v0.2.0 的独立 host 线程 start/run 已删除）。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};
use rclaw_channels::{Channel, NapCat, Weixin};
use rclaw_core::ChannelCaps;
use rclaw_core::RuntimeChannelInfo;
use rclaw_store::fs::{init_layout, Layout};

use crate::bridge_executor::{BridgePythonExecutor, SkillApprovalStore};
use crate::bridge_executor::APPROVALS_FILE_NAME;
use crate::{apply_skill_configs, install_bundled_skills, reject_symlink, reject_symlink_ancestors, remove_managed_node, ResolvedAndroidConfig};

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
    std::fs::create_dir_all(&workspace_dir)?;

    let layout = Layout::new(&resolved.config.agent.base_dir);
    reject_symlink_ancestors(&files_dir, &layout.base, "agent 目录")?;
    reject_symlink_ancestors(&files_dir, &layout.skills_dir, "skill 目录")?;
    init_layout(&layout)?;
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
    wait_for_bundled_skills_ready(&resolved.bundled_skills_dir);
    reject_symlink_ancestors(&files_dir, &resolved.bundled_skills_dir, "APK 内置 skill 目录")?;
    reject_symlink_ancestors(&files_dir, &skill_baseline_dir, "skill 基线目录")?;
    install_bundled_skills(
        &resolved.bundled_skills_dir,
        &layout.skills_dir,
        &skill_baseline_dir,
    )?;
    // Never place the app-managed TOML under `layout.base`: the model-facing
    // read tool is intentionally rooted there and the configuration contains API
    // keys. Skill credentials live in their independent app-managed config files.
    let exposed_toml = layout.base.join("rclaw.toml");
    if exposed_toml.is_file() {
        std::fs::remove_file(&exposed_toml).context("清理旧版暴露在 agent 目录中的 rclaw.toml 失败")?;
    }
    let media_dir = layout.base.join("media");
    std::fs::create_dir_all(&media_dir)?;
    std::env::set_var("TMPDIR", layout.base.join("tmp").to_string_lossy().as_ref());
    std::fs::create_dir_all(layout.base.join("tmp"))?;

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

/// 等 APK 内置 skill 解包完成：目录存在 + 同级 `bundled-skills.version`
/// 标记（Java 侧 RuntimeAssets 先拷目录树、最后写标记，标记即完成信号）。
/// 服务路径 nativeStart 前已同步解包（零等待）；GUI 装配首启可能抢跑
/// （MainActivity 无条件拉起服务，解包必然在途，等待有终点）。上限 180s：
/// 真机闪存数秒即完，慢速存储（实测 x86 模拟器）40MB 可超 90s；超时后
/// 放行，让后续 install_bundled_skills 按原有错误路径报告真实原因。
fn wait_for_bundled_skills_ready(source: &Path) {
    let marker = source.with_file_name("bundled-skills.version");
    if source.is_dir() && marker.is_file() {
        return;
    }
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(180);
    // 此刻 tracing 订阅器尚未初始化（init_tracing 在 prepare_environment 之后），
    // 走 boot_debug.log 保证可观测。
    crate::log_boot("等待 APK 内置 skill 解包（GUI 装配抢跑兜底，上限 180s）");
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(400));
        if source.is_dir() && marker.is_file() {
            crate::log_boot("内置 skill 解包完成，继续装配");
            return;
        }
    }
    crate::log_boot("等待内置 skill 解包超时（180s），按现状继续装配");
}

/// 通道构造（能力声明在接线期采集；只有真实构造成功的通道才进入 RuntimeChannelInfo）。
pub struct ApkChannels {
    pub weixin: Option<Weixin>,
    pub napcat: Option<NapCat>,
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
    let mut channel_caps: Vec<ChannelCaps> = Vec::new();
    if let Some(wx) = &weixin {
        channel_caps.push(wx.capabilities());
    }
    if let Some(nc) = &napcat {
        channel_caps.push(nc.capabilities());
    }
    let info = RuntimeChannelInfo {
        media_dir: Some(media_dir.to_string_lossy().replace('\\', "/")),
        channels: channel_caps,
    };
    ApkChannels {
        weixin,
        napcat,
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
) -> Result<(Option<rclaw_gbrain::GBrainTools>, Option<std::time::Duration>)> {
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
