//! Skill Manifest 驱动的结构化入口工具。
//!
//! Manifest 只描述固定脚本和参数映射。执行时直接构造参数数组启动进程，
//! 不经过 shell，也不允许模型拼接脚本路径或命令行。

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use rclaw_core::ToolSchema;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::{timeout, Instant};

use crate::{Tool, ToolResult};

const MANIFEST_FILE: &str = "skill.toml";
const MAX_ENTRYPOINTS: usize = 32;
const MAX_ARGUMENTS: usize = 32;
const MAX_ARRAY_ITEMS: usize = 128;
const MAX_FIXED_ARGUMENTS: usize = 16;
const MAX_FIXED_ARGUMENT_CHARS: usize = 256;
const MAX_ENUM_VALUE_CHARS: usize = 256;
const MAX_DESCRIPTION_CHARS: usize = 1_000;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_SCRIPT_BYTES: u64 = 8 * 1024 * 1024;
// 个别技能入口（如股票报告的 GLM 视觉生成）思考模式下健康一次就要 7~25 分钟，
// 加上重试与返工，清单作者需要声明 1 小时级的预算；2 小时只挡明显写错的值。
const MAX_TIMEOUT_MS: u64 = 7_200_000;
const DEFAULT_TIMEOUT_MS: u64 = 300_000;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// PythonScriptExecutor：宿主无关的 Python 脚本执行面（Android 移植阶段 2.1）
// ---------------------------------------------------------------------------

/// 解释器解析回调：每次执行前调用（venv 重建 / 系统 python 切换后下一次执行
/// 即生效，无需重启 agent）。解析逻辑保持在 `skill_entrypoint` 一处，避免双头。
pub type PythonResolver = Arc<dyn Fn() -> Result<PathBuf, String> + Send + Sync>;

/// 一次 Python 脚本执行的最小请求面。
pub struct PythonScriptRequest<'a> {
    pub script: &'a Path,
    pub argv: &'a [OsString],
    pub cwd: &'a Path,
    /// 额外注入子进程/桥环境的键值对（如 `RCLAW_SOURCE_CHANNEL`）。
    pub env: &'a [(String, String)],
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

/// 执行结果。`exit_code` 为 `None` 表示被信号终止（与桌面子进程语义一致）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptOutcome {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

/// 执行失败形态。调用方（`SkillEntrypointTool`）负责映射成与历史逐字节
/// 一致的工具错误文案，因此这里只分形态、不带工具名。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptFailure {
    /// 解释器/桥解析失败（文案原样透出，不包工具名）。
    Resolve(String),
    /// 进程/桥启动失败。
    Spawn(String),
    /// 等待执行结束失败。
    Wait(String),
    /// 执行超时。
    Timeout,
    /// 输出收集超时。
    OutputCollectTimeout,
    /// 单流输出收集失败（stream = "stdout" | "stderr"）。
    OutputCollect { stream: &'static str, error: String },
    /// 宿主策略拒绝执行（如 APK 脚本审批失配）。文案自描述（含脚本路径、
    /// 拒绝原因与批准入口指引），由调用方原样透出给模型。
    Rejected(String),
    /// 运行时不可用（APK 桥 poisoned 等宿主侧状态）。
    Unavailable(String),
}

/// Python 脚本执行器。桌面/Termux 缺省用 [`ProcessPythonExecutor`]（spawn 子进程，
/// 行为与拆分前逐项一致）；APK 宿主经 [`set_python_script_executor`] 注入
/// `BridgePythonExecutor`（JNI → Chaquopy 桥，见 rclaw-android）。
#[async_trait]
pub trait PythonScriptExecutor: Send + Sync {
    async fn execute(
        &self,
        request: PythonScriptRequest<'_>,
    ) -> Result<ScriptOutcome, ScriptFailure>;
}

/// 进程级注入槽（范式同 `access.rs::PATH_CONFIRM_HOOK`）：宿主启动时注入，
/// 未注入时走缺省进程执行器，桌面/Termux 完全不感知该抽象的存在（D7）。
static PYTHON_EXECUTOR_SLOT: OnceLock<RwLock<Option<Arc<dyn PythonScriptExecutor>>>> =
    OnceLock::new();
/// 进程级输出限额（`[skills] max_output_bytes`）：缺省 [`MAX_OUTPUT_BYTES`]。
static SKILL_OUTPUT_LIMIT: OnceLock<RwLock<Option<usize>>> = OnceLock::new();

/// 注入宿主专属 Python 执行器（APK 启动时调用；重复注入覆盖旧值）。
pub fn set_python_script_executor(executor: Arc<dyn PythonScriptExecutor>) {
    *PYTHON_EXECUTOR_SLOT
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("PYTHON_EXECUTOR_SLOT 锁未被毒化") = Some(executor);
}

/// 清除注入的执行器（宿主停机 / 测试还原用）。
pub fn clear_python_script_executor() {
    *PYTHON_EXECUTOR_SLOT
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("PYTHON_EXECUTOR_SLOT 锁未被毒化") = None;
}

/// 设置 skill stdout/stderr 单流字节限额（`[skills] max_output_bytes`）。
pub fn set_skill_output_limit(max_bytes: usize) {
    *SKILL_OUTPUT_LIMIT
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("SKILL_OUTPUT_LIMIT 锁未被毒化") = Some(max_bytes.max(1));
}

/// 还原默认限额（测试用）。
pub fn clear_skill_output_limit() {
    *SKILL_OUTPUT_LIMIT
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("SKILL_OUTPUT_LIMIT 锁未被毒化") = None;
}

fn skill_output_limit() -> usize {
    let guard = SKILL_OUTPUT_LIMIT
        .get_or_init(|| RwLock::new(None))
        .read()
        .expect("SKILL_OUTPUT_LIMIT 锁未被毒化");
    guard.as_ref().copied().unwrap_or(MAX_OUTPUT_BYTES)
}

/// 桌面缺省执行器：把现有 spawn 逻辑原样封装，解释器经回调每次执行前解析。
pub struct ProcessPythonExecutor {
    resolver: PythonResolver,
}

impl ProcessPythonExecutor {
    pub fn new(resolver: PythonResolver) -> Self {
        Self { resolver }
    }
}

#[async_trait]
impl PythonScriptExecutor for ProcessPythonExecutor {
    async fn execute(
        &self,
        request: PythonScriptRequest<'_>,
    ) -> Result<ScriptOutcome, ScriptFailure> {
        let program = (self.resolver)().map_err(ScriptFailure::Resolve)?;
        let spec = ProcessSpawnSpec {
            program: &program,
            preamble: &[],
            script: request.script,
            argv: request.argv,
            cwd: request.cwd,
            env: request.env,
            python_utf8_env: true,
            timeout: request.timeout,
            max_output_bytes: request.max_output_bytes,
        };
        spawn_and_collect(spec).await
    }
}

/// 取当前生效的 Python 执行器：宿主注入优先，否则即时构造进程执行器
/// （闭包捕获 base/venv 克隆，每次执行回调解析，语义与拆分前一致）。
fn current_python_executor(
    base: &Path,
    python_venv: &Option<PathBuf>,
) -> Arc<dyn PythonScriptExecutor> {
    if let Some(injected) = PYTHON_EXECUTOR_SLOT
        .get_or_init(|| RwLock::new(None))
        .read()
        .expect("PYTHON_EXECUTOR_SLOT 锁未被毒化")
        .clone()
    {
        return injected;
    }
    let base = base.to_path_buf();
    let venv = python_venv.clone();
    let resolver: PythonResolver = Arc::new(move || resolve_python_interpreter(&base, &venv));
    Arc::new(ProcessPythonExecutor::new(resolver))
}

// ---------------------------------------------------------------------------
// 技能脚本写路径变更通知（APK 审批摘除的宿主钩子，crate 边界 ①）
// ---------------------------------------------------------------------------

/// 技能脚本被任何写路径（write/edit/delete、skill_manage patch、自进化）修改时
/// 的通知回调。rclaw-tools 只提供槽位与调用点，不知道审批表的存在；APK 宿主
/// 注入回调实现「改码必须过人」（哈希失配即拒绝执行，见 rclaw-android）。
/// 桌面 / Termux 缺省无钩子 = no-op，行为零变化（D7）。
pub type SkillScriptChangeHook = Arc<dyn Fn(&Path) + Send + Sync>;

static SKILL_SCRIPT_CHANGE_HOOK: OnceLock<RwLock<Option<SkillScriptChangeHook>>> = OnceLock::new();

/// 注入技能脚本变更钩子（APK 宿主启动时；重复注入覆盖旧值）。
pub fn set_skill_script_change_hook(hook: SkillScriptChangeHook) {
    *SKILL_SCRIPT_CHANGE_HOOK
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("SKILL_SCRIPT_CHANGE_HOOK 锁未被毒化") = Some(hook);
}

/// 清除钩子（宿主停机 / 测试还原用）。
pub fn clear_skill_script_change_hook() {
    *SKILL_SCRIPT_CHANGE_HOOK
        .get_or_init(|| RwLock::new(None))
        .write()
        .expect("SKILL_SCRIPT_CHANGE_HOOK 锁未被毒化") = None;
}

/// 路径位于 skills/ 树下且为 Python 脚本时触发钩子；无钩子时为零开销 no-op。
pub(crate) fn notify_skill_script_changed(path: &Path) {
    let is_skill_python_script = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("py"))
        && path
            .components()
            .any(|component| component.as_os_str() == std::ffi::OsStr::new("skills"));
    if !is_skill_python_script {
        return;
    }
    if let Some(hook) = SKILL_SCRIPT_CHANGE_HOOK
        .get_or_init(|| RwLock::new(None))
        .read()
        .expect("SKILL_SCRIPT_CHANGE_HOOK 锁未被毒化")
        .clone()
    {
        hook(path);
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillManifest {
    manifest_version: u32,
    skill: ManifestSkill,
    entrypoints: Vec<ManifestEntrypoint>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestSkill {
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestEntrypoint {
    name: String,
    description: String,
    runtime: RuntimeKind,
    script: String,
    windows_script: Option<String>,
    #[serde(default = "default_timeout_ms")]
    timeout_ms: u64,
    #[serde(default)]
    publishes_media: bool,
    python_venv: Option<String>,
    #[serde(default)]
    fixed_args: Vec<String>,
    #[serde(default)]
    constraints: ManifestConstraints,
    #[serde(default)]
    arguments: Vec<ManifestArgument>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RuntimeKind {
    Python,
    Node,
    PlatformShell,
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestConstraints {
    #[serde(default)]
    at_least_one: Vec<Vec<String>>,
    #[serde(default)]
    exactly_one: Vec<Vec<String>>,
    #[serde(default)]
    at_most_one: Vec<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct ManifestArgument {
    name: String,
    #[serde(rename = "type")]
    kind: ArgumentKind,
    description: String,
    #[serde(default)]
    required: bool,
    position: Option<u16>,
    flag: Option<String>,
    #[serde(default)]
    values: Vec<String>,
    minimum: Option<i64>,
    maximum: Option<i64>,
    min_items: Option<usize>,
    max_items: Option<usize>,
    repeat_flag: Option<bool>,
    #[serde(default)]
    must_exist: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArgumentKind {
    String,
    Path,
    Directory,
    PathOrDirectory,
    UrlOrPath,
    StringArray,
    PathArray,
    UrlOrPathArray,
    Integer,
    IntegerArray,
    Number,
    NumberArray,
    Boolean,
    Enum,
}

/// 从已加载 Skill 所在目录或最近的包级祖先目录读取 Manifest 并构造动态专用工具。
///
/// 未配置 Manifest 的旧 Skill 返回 `Ok(None)`，以保持渐进迁移能力。
pub fn load_skill_entrypoints(
    base: &Path,
    skill_md_path: &Path,
) -> Result<Option<Vec<Arc<SkillEntrypointTool>>>, String> {
    let safe_skill_path =
        resolve_safe_path(base, skill_md_path).map_err(|e| format!("Skill 路径不安全: {e}"))?;
    let canonical_base = base
        .canonicalize()
        .map_err(|e| format!("无法解析工作目录 {}: {e}", base.display()))?;
    let skills_root = resolve_safe_path(&canonical_base, &canonical_base.join("skills"))
        .map_err(|e| format!("Skills 目录不安全: {e}"))?;
    if !safe_skill_path.starts_with(&skills_root) {
        return Err(format!(
            "Skill 必须位于 Skills 目录内: {}",
            safe_skill_path.display()
        ));
    }
    let mut skill_dir = safe_skill_path
        .parent()
        .ok_or_else(|| format!("Skill 路径缺少父目录: {}", safe_skill_path.display()))?;
    let (skill_dir, manifest_path) = loop {
        if skill_dir == skills_root {
            return Ok(None);
        }
        // 包格式中间层固定名为 skills/，清单只允许位于具体 Skill 或包根目录。
        if skill_dir.file_name() != Some(std::ffi::OsStr::new("skills")) {
            let candidate = skill_dir.join(MANIFEST_FILE);
            match std::fs::symlink_metadata(&candidate) {
                Ok(_) => break (skill_dir, candidate),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(format!(
                        "检查 Manifest 失败（{}）: {error}",
                        candidate.display()
                    ));
                }
            }
        }
        skill_dir = skill_dir
            .parent()
            .ok_or_else(|| format!("Skill 路径越过工作目录边界: {}", safe_skill_path.display()))?;
        if !skill_dir.starts_with(&skills_root) {
            return Err(format!(
                "Skill 路径越过 Skills 目录边界: {}",
                safe_skill_path.display()
            ));
        }
    };

    let safe_manifest =
        resolve_safe_path(base, &manifest_path).map_err(|e| format!("Manifest 路径不安全: {e}"))?;
    let source = read_skill_manifest_source(&safe_manifest)?;
    let tools = parse_manifest_tools(base, skill_dir, &source, &safe_manifest)?;
    Ok(Some(filter_tools_for_host(tools)))
}

/// APK 宿主（运行期宿主标记，§4.1 三态）只暴露 Python 入口：Node / platform_shell
/// 依赖外部命令行，APK 内不存在，注册了也只会在调用时报「未找到 Node.js/PowerShell」。
/// 桌面 / Termux 不设标记，完整返回（D7 零回归）。
fn filter_tools_for_host(tools: Vec<Arc<SkillEntrypointTool>>) -> Vec<Arc<SkillEntrypointTool>> {
    filter_tools_for_host_flag(tools, apk_host_marker())
}

fn filter_tools_for_host_flag(
    mut tools: Vec<Arc<SkillEntrypointTool>>,
    apk_host: bool,
) -> Vec<Arc<SkillEntrypointTool>> {
    if !apk_host {
        return tools;
    }
    tools.retain(|tool| {
        let keep = matches!(tool.runtime, RuntimeKind::Python);
        if !keep {
            tracing::warn!(
                tool = %tool.name,
                "APK 宿主跳过非 Python runtime 的 skill 入口（无外部命令行可承载）"
            );
        }
        keep
    });
    tools
}

/// APK 宿主标记：与 `rclaw_runtime::HostKind::APK_MARKER_ENV` 同值同语义；
/// rclaw-tools 不依赖 rclaw-runtime（依赖方向禁止），按同一约定读环境变量。
pub(crate) fn apk_host_marker() -> bool {
    std::env::var("RCLAW_ANDROID").is_ok_and(|value| value.trim() == "1")
}

pub(crate) fn read_skill_manifest_source(path: &Path) -> Result<String, String> {
    if !path.is_file() {
        return Err(format!("Manifest 不是普通文件: {}", path.display()));
    }
    let manifest_bytes = path
        .metadata()
        .map_err(|e| format!("读取 Manifest 元数据失败（{}）: {e}", path.display()))?
        .len();
    if manifest_bytes > MAX_MANIFEST_BYTES {
        return Err(format!(
            "Manifest 超过大小上限（{manifest_bytes} > {MAX_MANIFEST_BYTES} 字节）: {}",
            path.display()
        ));
    }
    let file = std::fs::File::open(path)
        .map_err(|e| format!("读取 Manifest 失败（{}）: {e}", path.display()))?;
    let mut bytes = Vec::with_capacity(manifest_bytes as usize);
    let mut limited = std::io::Read::take(file, MAX_MANIFEST_BYTES + 1);
    std::io::Read::read_to_end(&mut limited, &mut bytes)
        .map_err(|e| format!("读取 Manifest 失败（{}）: {e}", path.display()))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(format!(
            "Manifest 超过大小上限（{} > {MAX_MANIFEST_BYTES} 字节）: {}",
            bytes.len(),
            path.display()
        ));
    }
    String::from_utf8(bytes)
        .map_err(|e| format!("Manifest 不是有效 UTF-8（{}）: {e}", path.display()))
}

/// 在 Skill 管理工具写盘前复用完整 Manifest 契约校验。
pub(crate) fn validate_skill_manifest_source(
    base: &Path,
    skill_dir: &Path,
    source: &str,
) -> Result<(), String> {
    let display_path = skill_dir.join(MANIFEST_FILE);
    parse_manifest_tools(base, skill_dir, source, &display_path).map(|_| ())
}

/// 校验 Manifest 后判断目标文件是否是任一入口在任一平台声明的脚本。
pub(crate) fn skill_manifest_references_script(
    base: &Path,
    skill_dir: &Path,
    source: &str,
    target: &Path,
) -> Result<bool, String> {
    let display_path = skill_dir.join(MANIFEST_FILE);
    let tools = parse_manifest_tools(base, skill_dir, source, &display_path)?;
    let safe_target = resolve_safe_path(base, target)
        .map_err(|error| format!("待删除脚本路径不安全: {error}"))?;
    Ok(tools
        .iter()
        .any(|tool| tool.validated_scripts.contains(&safe_target)))
}

fn parse_manifest_tools(
    base: &Path,
    skill_dir: &Path,
    source: &str,
    display_path: &Path,
) -> Result<Vec<Arc<SkillEntrypointTool>>, String> {
    let manifest_sha256: [u8; 32] = Sha256::digest(source.as_bytes()).into();
    let manifest: SkillManifest = toml::from_str(source)
        .map_err(|e| format!("解析 Manifest 失败（{}）: {e}", display_path.display()))?;
    validate_manifest_header(&manifest, skill_dir)?;
    let mut tools = Vec::with_capacity(manifest.entrypoints.len());
    let mut tool_names = HashSet::new();
    let mut script_digests = HashMap::new();
    for entrypoint in manifest.entrypoints {
        let tool = SkillEntrypointTool::from_manifest(
            base,
            skill_dir,
            display_path,
            manifest_sha256,
            &manifest.skill.name,
            entrypoint,
            &mut script_digests,
        )?;
        if !tool_names.insert(tool.name.clone()) {
            return Err(format!("Manifest 生成了重复工具名: {}", tool.name));
        }
        tools.push(Arc::new(tool));
    }
    Ok(tools)
}

fn validate_manifest_header(manifest: &SkillManifest, skill_dir: &Path) -> Result<(), String> {
    if manifest.manifest_version != 1 {
        return Err(format!(
            "不支持的 Manifest 版本 {}，当前只支持版本 1",
            manifest.manifest_version
        ));
    }
    let directory_name = skill_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("Skill 目录名不是有效 UTF-8: {}", skill_dir.display()))?;
    validate_identifier("Skill 名", &manifest.skill.name)?;
    if directory_name != manifest.skill.name {
        return Err(format!(
            "Manifest Skill 名 `{}` 与目录名 `{directory_name}` 不一致",
            manifest.skill.name
        ));
    }
    if manifest.entrypoints.is_empty() || manifest.entrypoints.len() > MAX_ENTRYPOINTS {
        return Err(format!(
            "Manifest 入口数量必须在 1..={MAX_ENTRYPOINTS} 之间"
        ));
    }
    Ok(())
}

fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

/// 单个 Manifest 入口对应的运行时工具。
#[derive(Debug)]
pub struct SkillEntrypointTool {
    name: String,
    description: String,
    skill_name: String,
    skill_dir: PathBuf,
    manifest_path: PathBuf,
    manifest_sha256: [u8; 32],
    base: PathBuf,
    runtime: RuntimeKind,
    script: PathBuf,
    script_sha256: [u8; 32],
    validated_scripts: Vec<PathBuf>,
    declared_scripts: Vec<String>,
    python_venv: Option<PathBuf>,
    timeout_ms: u64,
    publishes_media: bool,
    fixed_args: Vec<String>,
    arguments: Vec<ManifestArgument>,
    constraints: ManifestConstraints,
    input_schema: Value,
}

/// turn 被取消或执行 future 被丢弃时，输出读取任务也必须随之停止。
struct AbortTaskOnDrop<T> {
    handle: JoinHandle<T>,
}

impl<T> AbortTaskOnDrop<T> {
    fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }

    fn abort(&self) {
        self.handle.abort();
    }

    async fn join(&mut self) -> Result<T, JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortTaskOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl SkillEntrypointTool {
    fn from_manifest(
        base: &Path,
        skill_dir: &Path,
        manifest_path: &Path,
        manifest_sha256: [u8; 32],
        skill_name: &str,
        entrypoint: ManifestEntrypoint,
        script_digests: &mut HashMap<PathBuf, [u8; 32]>,
    ) -> Result<Self, String> {
        validate_identifier("入口名", &entrypoint.name)?;
        if entrypoint.description.trim().is_empty()
            || entrypoint.description.chars().count() > MAX_DESCRIPTION_CHARS
        {
            return Err(format!(
                "入口 `{}` 的 description 必须为 1..={MAX_DESCRIPTION_CHARS} 个字符",
                entrypoint.name
            ));
        }
        if !(1..=MAX_TIMEOUT_MS).contains(&entrypoint.timeout_ms) {
            return Err(format!(
                "入口 `{}` 的 timeout_ms 必须在 1..={MAX_TIMEOUT_MS} 之间",
                entrypoint.name
            ));
        }
        if entrypoint.arguments.len() > MAX_ARGUMENTS {
            return Err(format!(
                "入口 `{}` 的参数数量不能超过 {MAX_ARGUMENTS}",
                entrypoint.name
            ));
        }
        validate_fixed_arguments(&entrypoint.name, &entrypoint.fixed_args)?;

        let safe_skill_dir =
            resolve_safe_path(base, skill_dir).map_err(|e| format!("Skill 目录不安全: {e}"))?;
        let script = validate_script(
            base,
            skill_dir,
            &safe_skill_dir,
            &entrypoint.name,
            "script",
            &entrypoint.script,
            match entrypoint.runtime {
                RuntimeKind::Python => &["py"],
                RuntimeKind::Node => &["js", "mjs"],
                RuntimeKind::PlatformShell => &["sh"],
            },
        )?;
        let windows_script = entrypoint
            .windows_script
            .as_deref()
            .map(|value| {
                validate_script(
                    base,
                    skill_dir,
                    &safe_skill_dir,
                    &entrypoint.name,
                    "windows_script",
                    value,
                    &["ps1"],
                )
            })
            .transpose()?;
        match entrypoint.runtime {
            RuntimeKind::PlatformShell if windows_script.is_none() => {
                return Err(format!(
                    "入口 `{}` 的 platform_shell 必须配置 windows_script",
                    entrypoint.name
                ));
            }
            RuntimeKind::Python | RuntimeKind::Node if windows_script.is_some() => {
                return Err(format!(
                    "入口 `{}` 只有 platform_shell 可以配置 windows_script",
                    entrypoint.name
                ));
            }
            _ => {}
        }

        let python_venv = entrypoint
            .python_venv
            .as_deref()
            .map(|value| validate_relative_path("python_venv", value))
            .transpose()?
            .map(|relative| base.join(relative));
        if !matches!(entrypoint.runtime, RuntimeKind::Python) && python_venv.is_some() {
            return Err(format!(
                "入口 `{}` 只有 Python runtime 可以配置 python_venv",
                entrypoint.name
            ));
        }

        validate_arguments(
            &entrypoint.name,
            &entrypoint.arguments,
            &entrypoint.constraints,
        )?;
        let input_schema = build_input_schema(&entrypoint.arguments, &entrypoint.constraints);
        let name = format!(
            "skill__{}__{}",
            normalize_tool_component(skill_name),
            normalize_tool_component(&entrypoint.name)
        );
        if name.len() > 64 {
            return Err(format!("动态工具名超过 64 个字符: {name}"));
        }

        let validated_scripts = std::iter::once(script.clone())
            .chain(windows_script.clone())
            .collect::<Vec<_>>();
        let selected_script = if cfg!(windows) {
            windows_script.clone().unwrap_or_else(|| script.clone())
        } else {
            script.clone()
        };
        let script_sha256 = match script_digests.get(&selected_script) {
            Some(digest) => *digest,
            None => {
                let digest = file_sha256(&selected_script).map_err(|error| {
                    format!(
                        "读取入口 `{}` 的脚本摘要失败（{}）: {error}",
                        entrypoint.name,
                        selected_script.display()
                    )
                })?;
                script_digests.insert(selected_script.clone(), digest);
                digest
            }
        };
        Ok(Self {
            name,
            description: entrypoint.description,
            skill_name: skill_name.to_string(),
            skill_dir: safe_skill_dir,
            manifest_path: manifest_path.to_path_buf(),
            manifest_sha256,
            base: base.to_path_buf(),
            runtime: entrypoint.runtime,
            script: selected_script,
            script_sha256,
            validated_scripts,
            declared_scripts: std::iter::once(entrypoint.script)
                .chain(entrypoint.windows_script)
                .collect(),
            python_venv,
            timeout_ms: entrypoint.timeout_ms,
            publishes_media: entrypoint.publishes_media,
            fixed_args: entrypoint.fixed_args,
            arguments: entrypoint.arguments,
            constraints: entrypoint.constraints,
            input_schema,
        })
    }

    /// 返回可直接追加到当前轮工具列表的 Schema。
    pub fn tool_schema(&self) -> ToolSchema {
        self.schema()
    }

    /// 返回 Manifest 中的 Skill 名，用于生成迁移提示。
    pub fn skill_name(&self) -> &str {
        &self.skill_name
    }

    /// 该入口是否由受信任的 Manifest 显式声明为媒体发布器。
    pub fn publishes_media(&self) -> bool {
        self.publishes_media
    }

    /// 返回加载时已完成边界与符号链接校验、当前平台实际执行的脚本路径。
    pub fn script_path(&self) -> &Path {
        &self.script
    }

    /// 判断一个已成功读取的 Skill 入口文件是否由该 Manifest 所属目录覆盖。
    pub fn owns_skill_path(&self, skill_path: &Path) -> bool {
        skill_path
            .canonicalize()
            .is_ok_and(|path| path.starts_with(&self.skill_dir))
    }

    /// 执行前重新解析当前 Manifest 并校验脚本内容，拒绝加载后发生的外部改写。
    pub fn revalidate_definition(&self) -> Result<(), String> {
        let current_manifest = resolve_safe_path(&self.base, &self.manifest_path)
            .map_err(|error| format!("Manifest 已失效: {error}"))?;
        if current_manifest != self.manifest_path {
            return Err(format!(
                "Manifest 在加载后被移动或重定向（当前解析为 {}）",
                current_manifest.display()
            ));
        }
        let source = read_skill_manifest_source(&current_manifest)?;
        let current_manifest_sha256: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        if current_manifest_sha256 != self.manifest_sha256 {
            return Err(format!(
                "Manifest 在加载后发生变化，专用工具 `{}` 的整组 Schema 已失效",
                self.name
            ));
        }
        let tools = parse_manifest_tools(&self.base, &self.skill_dir, &source, &current_manifest)?;
        let current = tools
            .iter()
            .find(|tool| tool.name == self.name)
            .ok_or_else(|| format!("Manifest 已移除专用工具 `{}`", self.name))?;
        if !self.has_same_definition(current) {
            return Err(format!("专用工具 `{}` 的定义或脚本内容已改变", self.name));
        }
        Ok(())
    }

    /// 只有完整执行定义一致时，重复加载才可视为同一个动态工具。
    pub fn has_same_definition(&self, other: &Self) -> bool {
        self.name == other.name
            && self.description == other.description
            && self.skill_name == other.skill_name
            && self.skill_dir == other.skill_dir
            && self.manifest_path == other.manifest_path
            && self.manifest_sha256 == other.manifest_sha256
            && self.base == other.base
            && self.runtime == other.runtime
            && self.script == other.script
            && self.script_sha256 == other.script_sha256
            && self.validated_scripts == other.validated_scripts
            && self.declared_scripts == other.declared_scripts
            && self.python_venv == other.python_venv
            && self.timeout_ms == other.timeout_ms
            && self.publishes_media == other.publishes_media
            && self.fixed_args == other.fixed_args
            && self.arguments == other.arguments
            && self.constraints == other.constraints
            && self.input_schema == other.input_schema
    }

    /// 判断 shell 命令是否在绕过当前入口直接运行 Skill 脚本。
    pub fn matches_legacy_shell_command(&self, command: &str) -> bool {
        let normalized = command.replace('\\', "/").to_ascii_lowercase();
        self.declared_scripts.iter().any(|declared| {
            let declared = declared.replace('\\', "/").to_ascii_lowercase();
            let directory_match = declared.rsplit_once('/').is_some_and(|(directory, _)| {
                let marker = format!(
                    "skills/{}/{directory}/",
                    self.skill_name.to_ascii_lowercase()
                );
                normalized
                    .match_indices(&marker)
                    .any(|(target_at, _)| shell_prefix_invokes_script(&normalized[..target_at]))
            });
            if directory_match {
                return true;
            }
            let script_name = Path::new(&declared)
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&declared);
            normalized.match_indices(script_name).any(|(target_at, _)| {
                script_name_has_token_boundaries(&normalized, target_at, script_name.len())
                    && shell_prefix_invokes_script(&normalized[..target_at])
            })
        })
    }

    fn validate_and_build_argv(&self, input: &Value) -> Result<Vec<OsString>, String> {
        let object = input
            .as_object()
            .ok_or_else(|| "专用工具输入必须是 JSON 对象".to_string())?;
        let known: HashSet<&str> = self.arguments.iter().map(|arg| arg.name.as_str()).collect();
        if let Some(unknown) = object.keys().find(|name| !known.contains(name.as_str())) {
            return Err(format!("未知参数: {unknown}"));
        }

        for arg in &self.arguments {
            if arg.required && !object.contains_key(&arg.name) {
                return Err(format!("缺少必填参数: {}", arg.name));
            }
        }
        validate_constraints(object, &self.constraints)?;

        let mut positional: Vec<(&ManifestArgument, &Value)> = self
            .arguments
            .iter()
            .filter_map(|arg| {
                arg.position
                    .and_then(|position| object.get(&arg.name).map(|value| (position, arg, value)))
            })
            .map(|(_, arg, value)| (arg, value))
            .collect();
        positional.sort_by_key(|(arg, _)| arg.position);

        let mut argv = self
            .fixed_args
            .iter()
            .map(OsString::from)
            .collect::<Vec<_>>();
        for (arg, value) in positional {
            self.append_argument(&mut argv, arg, value, false)?;
        }
        for arg in self.arguments.iter().filter(|arg| arg.flag.is_some()) {
            if let Some(value) = object.get(&arg.name) {
                self.append_argument(&mut argv, arg, value, true)?;
            }
        }
        Ok(argv)
    }

    fn append_argument(
        &self,
        argv: &mut Vec<OsString>,
        arg: &ManifestArgument,
        value: &Value,
        flagged: bool,
    ) -> Result<(), String> {
        match arg.kind {
            ArgumentKind::Boolean => {
                let enabled = value
                    .as_bool()
                    .ok_or_else(|| format!("参数 `{}` 必须是布尔值", arg.name))?;
                if enabled {
                    argv.push(OsString::from(
                        arg.flag
                            .as_deref()
                            .ok_or_else(|| format!("布尔参数 `{}` 缺少 flag", arg.name))?,
                    ));
                }
            }
            ArgumentKind::StringArray
            | ArgumentKind::PathArray
            | ArgumentKind::UrlOrPathArray
            | ArgumentKind::IntegerArray
            | ArgumentKind::NumberArray => {
                let values = value
                    .as_array()
                    .ok_or_else(|| format!("参数 `{}` 必须是数组", arg.name))?;
                if values.is_empty() {
                    return Err(format!("参数 `{}` 不能为空数组", arg.name));
                }
                if arg.min_items.is_some_and(|minimum| values.len() < minimum)
                    || arg.max_items.is_some_and(|maximum| values.len() > maximum)
                    || values.len() > MAX_ARRAY_ITEMS
                {
                    return Err(format!("参数 `{}` 的数组长度超出允许范围", arg.name));
                }
                for (index, item) in values.iter().enumerate() {
                    if flagged && (arg.repeat_flag.unwrap_or(true) || index == 0) {
                        argv.push(OsString::from(arg.flag.as_deref().unwrap_or_default()));
                    }
                    argv.push(self.scalar_to_os_string(arg, item)?);
                }
            }
            _ => {
                if flagged {
                    argv.push(OsString::from(arg.flag.as_deref().unwrap_or_default()));
                }
                argv.push(self.scalar_to_os_string(arg, value)?);
            }
        }
        Ok(())
    }

    fn scalar_to_os_string(
        &self,
        arg: &ManifestArgument,
        value: &Value,
    ) -> Result<OsString, String> {
        match arg.kind {
            ArgumentKind::String | ArgumentKind::StringArray => {
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("参数 `{}` 必须是字符串", arg.name))?;
                if raw.is_empty() {
                    return Err(format!("参数 `{}` 不能为空字符串", arg.name));
                }
                Ok(OsString::from(raw))
            }
            ArgumentKind::Enum => {
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("参数 `{}` 必须是字符串枚举", arg.name))?;
                if !arg.values.iter().any(|allowed| allowed == raw) {
                    return Err(format!(
                        "参数 `{}` 的值不在允许列表中: {}",
                        arg.name,
                        arg.values.join(", ")
                    ));
                }
                Ok(OsString::from(raw))
            }
            ArgumentKind::Path
            | ArgumentKind::Directory
            | ArgumentKind::PathOrDirectory
            | ArgumentKind::PathArray => {
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("参数 `{}` 必须是路径字符串", arg.name))?;
                let safe = self.resolve_input_path(raw)?;
                let expected_kind_matches = match arg.kind {
                    ArgumentKind::Directory => safe.is_dir(),
                    ArgumentKind::PathOrDirectory => safe.is_file() || safe.is_dir(),
                    _ => safe.is_file(),
                };
                if safe.exists() && !expected_kind_matches {
                    return Err(format!(
                        "参数 `{}` 指向的已有路径类型不符: {}",
                        arg.name,
                        safe.display()
                    ));
                }
                if arg.must_exist && !safe.exists() {
                    return Err(format!(
                        "参数 `{}` 指向的路径不存在或类型不符: {}",
                        arg.name,
                        safe.display()
                    ));
                }
                Ok(safe.into_os_string())
            }
            ArgumentKind::UrlOrPath | ArgumentKind::UrlOrPathArray => {
                let raw = value
                    .as_str()
                    .ok_or_else(|| format!("参数 `{}` 必须是 URL 或路径字符串", arg.name))?;
                let lower = raw.to_ascii_lowercase();
                if lower.starts_with("https://") || lower.starts_with("http://") {
                    return Ok(OsString::from(raw));
                }
                let safe = self.resolve_input_path(raw)?;
                if safe.exists() && !safe.is_file() {
                    return Err(format!(
                        "参数 `{}` 指向的已有本地路径不是普通文件: {}",
                        arg.name,
                        safe.display()
                    ));
                }
                if arg.must_exist && !safe.exists() {
                    return Err(format!(
                        "参数 `{}` 指向的本地文件不存在: {}",
                        arg.name,
                        safe.display()
                    ));
                }
                Ok(safe.into_os_string())
            }
            ArgumentKind::Integer | ArgumentKind::IntegerArray => {
                let number = value
                    .as_i64()
                    .ok_or_else(|| format!("参数 `{}` 必须是整数", arg.name))?;
                if arg.minimum.is_some_and(|minimum| number < minimum)
                    || arg.maximum.is_some_and(|maximum| number > maximum)
                {
                    return Err(format!("参数 `{}` 超出允许范围", arg.name));
                }
                Ok(OsString::from(number.to_string()))
            }
            ArgumentKind::Number | ArgumentKind::NumberArray => match value {
                Value::Number(number) => Ok(OsString::from(number.to_string())),
                _ => Err(format!("参数 `{}` 必须是数字", arg.name)),
            },
            ArgumentKind::Boolean => Err(format!("参数 `{}` 的类型映射错误", arg.name)),
        }
    }

    fn resolve_input_path(&self, raw: &str) -> Result<PathBuf, String> {
        let relative_or_absolute = Path::new(raw);
        if raw.trim().is_empty()
            || relative_or_absolute
                .components()
                .any(|part| matches!(part, Component::ParentDir))
        {
            return Err(format!("路径为空或包含父目录跳转: {raw}"));
        }
        let target = if relative_or_absolute.is_absolute() {
            relative_or_absolute.to_path_buf()
        } else {
            self.base.join(relative_or_absolute)
        };
        resolve_safe_path(&self.base, &target).map_err(|e| format!("路径不安全 `{raw}`: {e}"))
    }

    fn resolve_path_program(&self, names: &[&str], label: &str) -> Result<PathBuf, String> {
        let path =
            std::env::var_os("PATH").ok_or_else(|| format!("PATH 未配置，无法定位 {label}"))?;
        for directory in std::env::split_paths(&path) {
            for name in names {
                let candidate = directory.join(name);
                if candidate.is_file() {
                    return Ok(candidate);
                }
            }
        }
        Err(format!("未在 PATH 中找到 {label}"))
    }

    /// 使用运行时可信来源 channel 执行入口。该值不来自模型参数，并通过环境变量
    /// 传给子进程，供发布媒体等需要绑定真实出站通道的 Skill 使用。
    pub async fn execute_with_source_channel(
        &self,
        input: Value,
        source_channel: &str,
    ) -> ToolResult {
        self.execute_with_runtime_context(input, Some(source_channel))
            .await
    }

    async fn execute_direct(&self, input: Value) -> ToolResult {
        self.execute_with_runtime_context(input, None).await
    }

    async fn execute_with_runtime_context(
        &self,
        input: Value,
        source_channel: Option<&str>,
    ) -> ToolResult {
        let current_script = match resolve_safe_path(&self.base, &self.script) {
            Ok(path) if path == self.script && path.is_file() => path,
            Ok(path) => {
                return ToolResult::err(format!(
                    "专用工具脚本在加载后已被替换、移动或重定向（当前解析为 {}）；请重新读取 Skill",
                    path.display()
                ))
            }
            Err(error) => {
                return ToolResult::err(format!(
                    "专用工具脚本在加载后已失效: {error}；请重新读取 Skill"
                ))
            }
        };
        match file_sha256(&current_script) {
            Ok(digest) if digest == self.script_sha256 => {}
            Ok(_) => {
                return ToolResult::err(
                    "专用工具脚本内容在加载后已改变；请重新读取 Skill".to_string(),
                )
            }
            Err(error) => {
                return ToolResult::err(format!(
                    "重新读取专用工具脚本失败（{}）: {error}；请重新读取 Skill",
                    current_script.display()
                ))
            }
        }
        let argv = match self.validate_and_build_argv(&input) {
            Ok(argv) => argv,
            Err(error) => return ToolResult::err(format!("专用工具参数校验失败: {error}")),
        };
        // 只有 turn/sub-agent 运行时显式传入的 channel 才能成为 Skill 子进程的
        // 可信上下文；executor / spawn 内核会先移除继承来源再应用这里的值。
        let source_channel_env: Vec<(String, String)> = source_channel
            .filter(|value| !value.is_empty())
            .map(|value| ("RCLAW_SOURCE_CHANNEL".to_string(), value.to_string()))
            .into_iter()
            .collect();

        // Python：经 PythonScriptExecutor（宿主可注入桥执行器；缺省进程执行器）。
        if matches!(self.runtime, RuntimeKind::Python) {
            let executor = current_python_executor(&self.base, &self.python_venv);
            let request = PythonScriptRequest {
                script: &current_script,
                argv: &argv,
                cwd: &self.base,
                env: &source_channel_env,
                timeout: Duration::from_millis(self.timeout_ms),
                max_output_bytes: skill_output_limit(),
            };
            return match executor.execute(request).await {
                Ok(outcome) => self.render_outcome(&outcome),
                Err(failure) => ToolResult::err(self.render_failure(failure)),
            };
        }

        let program = match self.runtime {
            RuntimeKind::Python => unreachable!("Python 已在上方经执行器分发"),
            RuntimeKind::Node => match self.resolve_path_program(
                if cfg!(windows) {
                    &["node.exe", "node", "nodejs.exe", "nodejs"]
                } else {
                    &["node", "nodejs"]
                },
                "Node.js",
            ) {
                Ok(program) => program,
                Err(error) => return ToolResult::err(error),
            },
            RuntimeKind::PlatformShell => match self.resolve_path_program(
                if cfg!(windows) {
                    &["powershell.exe", "pwsh.exe", "powershell", "pwsh"]
                } else {
                    &["bash"]
                },
                if cfg!(windows) { "PowerShell" } else { "Bash" },
            ) {
                Ok(program) => program,
                Err(error) => return ToolResult::err(error),
            },
        };

        let preamble: &[&str] =
            if matches!(self.runtime, RuntimeKind::PlatformShell) && cfg!(windows) {
                &[
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                ]
            } else {
                &[]
            };
        let outcome = spawn_and_collect(ProcessSpawnSpec {
            program: &program,
            preamble,
            script: &current_script,
            argv: &argv,
            cwd: &self.base,
            env: &source_channel_env,
            python_utf8_env: false,
            timeout: Duration::from_millis(self.timeout_ms),
            max_output_bytes: skill_output_limit(),
        })
        .await;
        match outcome {
            Ok(outcome) => self.render_outcome(&outcome),
            Err(failure) => ToolResult::err(self.render_failure(failure)),
        }
    }

    /// 把执行结果渲染成与拆分前逐字节一致的工具输出。
    fn render_outcome(&self, outcome: &ScriptOutcome) -> ToolResult {
        let content = format!(
            "专用工具: {}\n退出码: {}\n输出截断: stdout={}, stderr={}\n[stdout]\n{}\n[stderr]\n{}",
            self.name,
            outcome
                .exit_code
                .map_or_else(|| "signal".to_string(), |code| code.to_string()),
            outcome.stdout_truncated,
            outcome.stderr_truncated,
            outcome.stdout,
            outcome.stderr
        );
        if outcome.exit_code == Some(0) {
            ToolResult::ok(content)
        } else {
            ToolResult::err(content)
        }
    }

    /// 执行失败 → 工具错误文案（与拆分前的直接 spawn 路径逐字节一致）。
    fn render_failure(&self, failure: ScriptFailure) -> String {
        match failure {
            ScriptFailure::Resolve(error) => error,
            ScriptFailure::Spawn(error) => format!("专用工具启动失败（{}）: {error}", self.name),
            ScriptFailure::Wait(error) => format!("等待专用工具结束失败: {error}"),
            ScriptFailure::Timeout => {
                format!("专用工具执行超时（{} ms）: {}", self.timeout_ms, self.name)
            }
            ScriptFailure::OutputCollectTimeout => format!(
                "专用工具输出收集超时（{} ms）: {}",
                self.timeout_ms, self.name
            ),
            ScriptFailure::OutputCollect { stream, error } => {
                format!("专用工具 {stream} 收集失败: {error}")
            }
            ScriptFailure::Rejected(message) => message,
            ScriptFailure::Unavailable(error) => {
                format!("专用工具不可用（{}）: {error}", self.name)
            }
        }
    }
}

/// 进程执行内核的输入面（Python / Node / PlatformShell 共用）。
struct ProcessSpawnSpec<'a> {
    program: &'a Path,
    /// Windows PowerShell 固定前置参数（其他运行时为空）。
    preamble: &'a [&'a str],
    script: &'a Path,
    argv: &'a [OsString],
    cwd: &'a Path,
    env: &'a [(String, String)],
    /// Python 子进程固定 UTF-8 编码环境（PYTHONIOENCODING/PYTHONUTF8）。
    python_utf8_env: bool,
    timeout: Duration,
    max_output_bytes: usize,
}

/// 原有 spawn 执行路径（Command 构造 → 限时读取 → 截断解码）原样搬入；
/// 错误形态化后由调用方渲染文案。
async fn spawn_and_collect(spec: ProcessSpawnSpec<'_>) -> Result<ScriptOutcome, ScriptFailure> {
    let mut command = Command::new(spec.program);
    if !spec.preamble.is_empty() {
        command.args(spec.preamble);
    }
    command
        .arg(spec.script)
        .args(spec.argv)
        .current_dir(spec.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if spec.python_utf8_env {
        // Windows 系统 Python 常默认使用本地代码页；动态工具结果、JSON
        // 与 MEDIA_MARKER 都按 UTF-8 解码，因此显式固定子进程编码。
        command
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUTF8", "1");
    }
    // 不继承宿主进程里可能残留的来源；只有显式传入的 channel 才能成为
    // Skill 子进程的可信上下文。
    command.env_remove("RCLAW_SOURCE_CHANNEL");
    for (key, value) in spec.env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|error| ScriptFailure::Spawn(error.to_string()))?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let limit = spec.max_output_bytes;
    let mut stdout_task =
        AbortTaskOnDrop::new(tokio::spawn(
            async move { read_limited(stdout, limit).await },
        ));
    let mut stderr_task =
        AbortTaskOnDrop::new(tokio::spawn(
            async move { read_limited(stderr, limit).await },
        ));

    let started_at = Instant::now();
    let status = match timeout(spec.timeout, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(error)) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.join().await;
            let _ = stderr_task.join().await;
            return Err(ScriptFailure::Wait(error.to_string()));
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            // 读取任务不应在超时返回后继续游离运行。
            stdout_task.abort();
            stderr_task.abort();
            let _ = stdout_task.join().await;
            let _ = stderr_task.join().await;
            return Err(ScriptFailure::Timeout);
        }
    };
    let remaining = spec.timeout.saturating_sub(started_at.elapsed());
    let collected = timeout(remaining, async {
        let (stdout, stderr) = tokio::join!(stdout_task.join(), stderr_task.join());
        (collect_output(stdout), collect_output(stderr))
    })
    .await;
    let (stdout_result, stderr_result) = match collected {
        Ok(output) => output,
        Err(_) => {
            stdout_task.abort();
            stderr_task.abort();
            return Err(ScriptFailure::OutputCollectTimeout);
        }
    };
    let (stdout_text, stdout_truncated) =
        stdout_result.map_err(|error| ScriptFailure::OutputCollect {
            stream: "stdout",
            error: error.to_string(),
        })?;
    let (stderr_text, stderr_truncated) =
        stderr_result.map_err(|error| ScriptFailure::OutputCollect {
            stream: "stderr",
            error: error.to_string(),
        })?;
    Ok(ScriptOutcome {
        exit_code: status.code(),
        stdout: stdout_text,
        stderr: stderr_text,
        stdout_truncated,
        stderr_truncated,
    })
}

/// venv 优先、否则 PATH 搜索的解释器解析（原 `SkillEntrypointTool::resolve_python`
/// 主体；每次执行前经 resolver 回调调用，重建/切换后即时生效）。
fn resolve_python_interpreter(
    base: &Path,
    python_venv: &Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(venv) = python_venv {
        let safe_venv =
            resolve_safe_path(base, venv).map_err(|e| format!("Python venv 路径不安全: {e}"))?;
        let candidates = if cfg!(windows) {
            vec![
                safe_venv.join("Scripts/python.exe"),
                safe_venv.join("bin/python"),
            ]
        } else {
            vec![
                safe_venv.join("bin/python"),
                safe_venv.join("Scripts/python.exe"),
            ]
        };
        return candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| {
                format!(
                    "Manifest 指定的 Python venv 中未找到解释器: {}",
                    safe_venv.display()
                )
            });
    }

    let names: &[&str] = if cfg!(windows) {
        &["python.exe", "python3.exe", "python", "py.exe", "py"]
    } else {
        &["python3", "python"]
    };
    let path = std::env::var_os("PATH")
        .ok_or_else(|| "PATH 未配置，无法定位 Python 解释器".to_string())?;
    for directory in std::env::split_paths(&path) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err("未在 PATH 中找到 Python 解释器".to_string())
}

fn script_name_has_token_boundaries(command: &str, start: usize, name_len: usize) -> bool {
    let before = command[..start].chars().next_back();
    let after = command[start + name_len..].chars().next();
    let before_ok =
        before.is_none_or(|ch| !(ch.is_alphanumeric() || matches!(ch, '_' | '-' | '.')));
    let after_ok = after.is_none_or(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '\'' | '"' | ';' | '|' | '&' | '<' | '>' | ')' | ']' | '}'
            )
    });
    before_ok && after_ok
}

fn shell_prefix_invokes_script(prefix: &str) -> bool {
    let segment = prefix
        .rsplit([';', '\n', '|', '&'])
        .next()
        .unwrap_or_default()
        .trim();
    let direct = segment.trim_matches([' ', '\'', '"']);
    if direct.is_empty()
        || direct == "."
        || direct == "./"
        || segment.starts_with(['\'', '"'])
        || (!direct.chars().any(char::is_whitespace)
            && (direct.ends_with('/') || direct.ends_with('\\')))
    {
        return true;
    }

    segment.split_whitespace().any(|token| {
        let token = token.trim_matches(['\'', '"']);
        let program = Path::new(token)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(token)
            .trim_end_matches(".exe");
        program.starts_with("python")
            || matches!(
                program,
                "py" | "node"
                    | "nodejs"
                    | "bash"
                    | "sh"
                    | "dash"
                    | "zsh"
                    | "ksh"
                    | "fish"
                    | "source"
                    | "pwsh"
                    | "powershell"
                    | "cmd"
                    | "start"
                    | "start-process"
                    | "invoke-item"
                    | "import-module"
                    | "ii"
                    | "uv"
                    | "bun"
                    | "deno"
                    | "tsx"
                    | "ts-node"
            )
    })
}

#[async_trait]
impl Tool for SkillEntrypointTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn input_schema(&self) -> Value {
        self.input_schema.clone()
    }

    async fn execute(&self, input: Value) -> ToolResult {
        self.execute_direct(input).await
    }
}

fn validate_identifier(label: &str, value: &str) -> Result<(), String> {
    let mut chars = value.chars();
    let valid_first = chars.next().is_some_and(|ch| ch.is_ascii_lowercase());
    let valid_rest =
        chars.all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-' || ch == '_');
    if !valid_first || !valid_rest || value.len() > 40 {
        return Err(format!(
            "{label} `{value}` 必须匹配 [a-z][a-z0-9_-]* 且不超过 40 个字符"
        ));
    }
    Ok(())
}

fn normalize_tool_component(value: &str) -> String {
    value.replace('-', "_")
}

fn validate_relative_path(label: &str, value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path.is_absolute()
        || path.components().any(|part| {
            matches!(
                part,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!("{label} 必须是无父目录跳转的相对路径: {value}"));
    }
    Ok(path.to_path_buf())
}

fn validate_script(
    base: &Path,
    skill_dir: &Path,
    safe_skill_dir: &Path,
    entrypoint_name: &str,
    field_name: &str,
    declared: &str,
    extensions: &[&str],
) -> Result<PathBuf, String> {
    let relative = validate_relative_path(field_name, declared)?;
    let extension = relative.extension().and_then(|value| value.to_str());
    if !extension.is_some_and(|value| extensions.contains(&value)) {
        return Err(format!(
            "入口 `{entrypoint_name}` 的 {field_name} 扩展名必须是: {}",
            extensions.join(", ")
        ));
    }
    let script = resolve_safe_path(base, &skill_dir.join(relative))
        .map_err(|error| format!("入口 `{entrypoint_name}` 的 {field_name} 路径不安全: {error}"))?;
    if !script.starts_with(safe_skill_dir) {
        return Err(format!(
            "入口 `{entrypoint_name}` 的 {field_name} 必须位于当前 Skill 目录内"
        ));
    }
    if !script.is_file() {
        return Err(format!(
            "入口 `{entrypoint_name}` 的 {field_name} 不存在或不是普通文件: {}",
            script.display()
        ));
    }
    Ok(script)
}

fn file_sha256(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let bytes = file.metadata()?.len();
    if bytes > MAX_SCRIPT_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("脚本超过大小上限（{bytes} > {MAX_SCRIPT_BYTES} 字节）"),
        ));
    }
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut total = 0_u64;
    loop {
        let read = std::io::Read::read(&mut file, &mut buffer)?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_SCRIPT_BYTES {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("脚本读取期间超过大小上限（{MAX_SCRIPT_BYTES} 字节）"),
            ));
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().into())
}

/// 解析最长已存在祖先中的符号链接，并返回 base 内的真实目标路径。
fn resolve_safe_path(base: &Path, target: &Path) -> Result<PathBuf, String> {
    let canonical_base = base
        .canonicalize()
        .map_err(|error| format!("无法解析工作目录 {}: {error}", base.display()))?;
    let mut existing = target.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match std::fs::symlink_metadata(&existing) {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match (existing.parent(), existing.file_name()) {
                    (Some(parent), Some(name)) if parent != existing => {
                        tail.push(name.to_os_string());
                        existing = parent.to_path_buf();
                    }
                    _ => return Err(format!("无法解析目标路径: {}", target.display())),
                }
            }
            Err(error) => return Err(format!("检查路径 {} 失败: {error}", existing.display())),
        }
    }
    let mut resolved = existing
        .canonicalize()
        .map_err(|error| format!("无法解析路径 {}: {error}", existing.display()))?;
    for name in tail.into_iter().rev() {
        resolved.push(name);
    }
    if !resolved.starts_with(&canonical_base) {
        return Err(format!("路径越过工作目录边界: {}", target.display()));
    }
    Ok(resolved)
}

fn validate_arguments(
    entrypoint_name: &str,
    arguments: &[ManifestArgument],
    constraints: &ManifestConstraints,
) -> Result<(), String> {
    let mut names = HashSet::new();
    let mut positions = HashSet::new();
    let mut flags = HashSet::new();
    for arg in arguments {
        validate_identifier("参数名", &arg.name)?;
        if !names.insert(arg.name.as_str()) {
            return Err(format!(
                "入口 `{entrypoint_name}` 存在重复参数: {}",
                arg.name
            ));
        }
        if arg.description.trim().is_empty()
            || arg.description.chars().count() > MAX_DESCRIPTION_CHARS
        {
            return Err(format!("参数 `{}` 的 description 不能为空或过长", arg.name));
        }
        if arg.position.is_some() == arg.flag.is_some() {
            return Err(format!(
                "参数 `{}` 必须且只能配置 position 或 flag 之一",
                arg.name
            ));
        }
        if let Some(position) = arg.position {
            if !positions.insert(position) {
                return Err(format!(
                    "入口 `{entrypoint_name}` 存在重复 position: {position}"
                ));
            }
        }
        if let Some(flag) = &arg.flag {
            if !flag.starts_with("--")
                || flag.len() < 3
                || !flag[2..]
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
            {
                return Err(format!("参数 `{}` 的 flag 格式无效: {flag}", arg.name));
            }
            if !flags.insert(flag.as_str()) {
                return Err(format!("入口 `{entrypoint_name}` 存在重复 flag: {flag}"));
            }
        }
        if arg.kind == ArgumentKind::Boolean && arg.flag.is_none() {
            return Err(format!("布尔参数 `{}` 必须使用 flag", arg.name));
        }
        if arg.kind == ArgumentKind::Enum && arg.values.is_empty() {
            return Err(format!("枚举参数 `{}` 必须配置 values", arg.name));
        }
        if arg.kind == ArgumentKind::Enum {
            let mut values = HashSet::new();
            for value in &arg.values {
                if value.is_empty()
                    || value.chars().count() > MAX_ENUM_VALUE_CHARS
                    || value.contains(['\0', '\r', '\n'])
                {
                    return Err(format!("枚举参数 `{}` 包含无效值", arg.name));
                }
                if !values.insert(value.as_str()) {
                    return Err(format!("枚举参数 `{}` 包含重复值: {value}", arg.name));
                }
            }
        }
        if arg.kind != ArgumentKind::Enum && !arg.values.is_empty() {
            return Err(format!("只有枚举参数可以配置 values: {}", arg.name));
        }
        if !matches!(arg.kind, ArgumentKind::Integer | ArgumentKind::IntegerArray)
            && (arg.minimum.is_some() || arg.maximum.is_some())
        {
            return Err(format!(
                "只有整数参数可以配置 minimum/maximum: {}",
                arg.name
            ));
        }
        if arg
            .minimum
            .zip(arg.maximum)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
        {
            return Err(format!("参数 `{}` 的 minimum 不能大于 maximum", arg.name));
        }
        if !matches!(
            arg.kind,
            ArgumentKind::Path
                | ArgumentKind::Directory
                | ArgumentKind::PathOrDirectory
                | ArgumentKind::UrlOrPath
                | ArgumentKind::PathArray
                | ArgumentKind::UrlOrPathArray
        ) && arg.must_exist
        {
            return Err(format!("只有路径参数可以配置 must_exist: {}", arg.name));
        }
        let is_array = matches!(
            arg.kind,
            ArgumentKind::StringArray
                | ArgumentKind::PathArray
                | ArgumentKind::UrlOrPathArray
                | ArgumentKind::IntegerArray
                | ArgumentKind::NumberArray
        );
        if !is_array
            && (arg.min_items.is_some() || arg.max_items.is_some() || arg.repeat_flag.is_some())
        {
            return Err(format!(
                "只有数组参数可以配置 min_items、max_items 或 repeat_flag: {}",
                arg.name
            ));
        }
        if arg.repeat_flag.is_some() && arg.flag.is_none() {
            return Err(format!("位置数组参数不能配置 repeat_flag: {}", arg.name));
        }
        if arg
            .min_items
            .zip(arg.max_items)
            .is_some_and(|(minimum, maximum)| minimum > maximum)
            || arg.min_items == Some(0)
            || arg.max_items == Some(0)
            || arg
                .min_items
                .is_some_and(|minimum| minimum > MAX_ARRAY_ITEMS)
            || arg
                .max_items
                .is_some_and(|maximum| maximum > MAX_ARRAY_ITEMS)
        {
            return Err(format!("参数 `{}` 的数组长度约束无效", arg.name));
        }
    }

    let mut positional = arguments
        .iter()
        .filter_map(|arg| arg.position.map(|position| (position, arg.required)))
        .collect::<Vec<_>>();
    positional.sort_by_key(|(position, _)| *position);
    for (expected, (position, required)) in positional.iter().copied().enumerate() {
        if usize::from(position) != expected {
            return Err(format!(
                "入口 `{entrypoint_name}` 的 position 必须从 0 开始连续递增"
            ));
        }
        if !required && expected + 1 < positional.len() {
            return Err(format!(
                "入口 `{entrypoint_name}` 最多只能有一个可选位置参数，且它必须位于末尾"
            ));
        }
    }

    let positional_arrays = arguments
        .iter()
        .filter(|arg| {
            matches!(
                arg.kind,
                ArgumentKind::StringArray
                    | ArgumentKind::PathArray
                    | ArgumentKind::UrlOrPathArray
                    | ArgumentKind::IntegerArray
                    | ArgumentKind::NumberArray
            )
        })
        .filter_map(|arg| arg.position)
        .collect::<Vec<_>>();
    if positional_arrays.len() > 1 {
        return Err("每个入口最多只能配置一个位置数组参数".to_string());
    }
    if let Some(array_position) = positional_arrays.first().copied() {
        let greatest = arguments.iter().filter_map(|arg| arg.position).max();
        if Some(array_position) != greatest {
            return Err("位置数组参数必须是最后一个位置参数".to_string());
        }
    }
    validate_constraint_declarations(constraints, &names)
}

fn validate_constraint_declarations(
    constraints: &ManifestConstraints,
    names: &HashSet<&str>,
) -> Result<(), String> {
    for (kind, groups) in [
        ("at_least_one", &constraints.at_least_one),
        ("exactly_one", &constraints.exactly_one),
        ("at_most_one", &constraints.at_most_one),
    ] {
        for group in groups {
            if group.is_empty() {
                return Err(format!("约束 `{kind}` 不能包含空参数组"));
            }
            let mut group_names = HashSet::new();
            for name in group {
                if !names.contains(name.as_str()) {
                    return Err(format!("约束 `{kind}` 引用了未知参数: {name}"));
                }
                if !group_names.insert(name) {
                    return Err(format!("约束 `{kind}` 重复引用参数: {name}"));
                }
            }
        }
    }
    Ok(())
}

fn build_input_schema(arguments: &[ManifestArgument], constraints: &ManifestConstraints) -> Value {
    let mut properties = Map::new();
    let mut required = Vec::new();
    for arg in arguments {
        let mut property = Map::new();
        property.insert("description".to_string(), json!(arg.description));
        match arg.kind {
            ArgumentKind::String
            | ArgumentKind::Path
            | ArgumentKind::Directory
            | ArgumentKind::PathOrDirectory
            | ArgumentKind::UrlOrPath => {
                property.insert("type".to_string(), json!("string"));
                property.insert("minLength".to_string(), json!(1));
            }
            ArgumentKind::StringArray
            | ArgumentKind::PathArray
            | ArgumentKind::UrlOrPathArray
            | ArgumentKind::IntegerArray
            | ArgumentKind::NumberArray => {
                property.insert("type".to_string(), json!("array"));
                let mut items = Map::new();
                if arg.kind == ArgumentKind::IntegerArray {
                    items.insert("type".to_string(), json!("integer"));
                    if let Some(minimum) = arg.minimum {
                        items.insert("minimum".to_string(), json!(minimum));
                    }
                    if let Some(maximum) = arg.maximum {
                        items.insert("maximum".to_string(), json!(maximum));
                    }
                } else if arg.kind == ArgumentKind::NumberArray {
                    items.insert("type".to_string(), json!("number"));
                } else {
                    items.insert("type".to_string(), json!("string"));
                    items.insert("minLength".to_string(), json!(1));
                }
                property.insert("items".to_string(), Value::Object(items));
                property.insert("minItems".to_string(), json!(arg.min_items.unwrap_or(1)));
                property.insert(
                    "maxItems".to_string(),
                    json!(arg.max_items.unwrap_or(MAX_ARRAY_ITEMS)),
                );
            }
            ArgumentKind::Integer => {
                property.insert("type".to_string(), json!("integer"));
                if let Some(minimum) = arg.minimum {
                    property.insert("minimum".to_string(), json!(minimum));
                }
                if let Some(maximum) = arg.maximum {
                    property.insert("maximum".to_string(), json!(maximum));
                }
            }
            ArgumentKind::Boolean => {
                property.insert("type".to_string(), json!("boolean"));
            }
            ArgumentKind::Number => {
                property.insert("type".to_string(), json!("number"));
            }
            ArgumentKind::Enum => {
                property.insert("type".to_string(), json!("string"));
                property.insert("enum".to_string(), json!(arg.values));
            }
        }
        if arg.required {
            required.push(arg.name.clone());
        }
        properties.insert(arg.name.clone(), Value::Object(property));
    }

    let mut schema = Map::new();
    schema.insert("type".to_string(), json!("object"));
    schema.insert("properties".to_string(), Value::Object(properties));
    schema.insert("additionalProperties".to_string(), json!(false));
    if !required.is_empty() {
        schema.insert("required".to_string(), json!(required));
    }
    let mut all_of = Vec::new();
    for group in &constraints.at_least_one {
        all_of.push(json!({
            "anyOf": group
                .iter()
                .map(|name| constraint_requirement_schema(name, arguments))
                .collect::<Vec<_>>()
        }));
    }
    for group in &constraints.exactly_one {
        all_of.push(json!({
            "oneOf": group.iter().map(|name| {
                let others = group
                    .iter()
                    .filter(|other| *other != name)
                    .map(|other| constraint_requirement_schema(other, arguments))
                    .collect::<Vec<_>>();
                json!({
                    "allOf": [
                        constraint_requirement_schema(name, arguments),
                        json!({"not": {"anyOf": others}})
                    ]
                })
            }).collect::<Vec<_>>()
        }));
    }
    for group in &constraints.at_most_one {
        for left in 0..group.len() {
            for right in (left + 1)..group.len() {
                all_of.push(json!({
                    "not": {
                        "allOf": [
                            constraint_requirement_schema(&group[left], arguments),
                            constraint_requirement_schema(&group[right], arguments)
                        ]
                    }
                }));
            }
        }
    }
    if !all_of.is_empty() {
        schema.insert("allOf".to_string(), Value::Array(all_of));
    }
    Value::Object(schema)
}

fn constraint_requirement_schema(name: &str, arguments: &[ManifestArgument]) -> Value {
    if arguments
        .iter()
        .any(|argument| argument.name == name && argument.kind == ArgumentKind::Boolean)
    {
        json!({
            "required": [name],
            "properties": {name: {"enum": [true]}}
        })
    } else {
        json!({"required": [name]})
    }
}

/// 固定参数只用于声明脚本子命令等不应由模型生成的常量 argv。
fn validate_fixed_arguments(entrypoint_name: &str, arguments: &[String]) -> Result<(), String> {
    if arguments.len() > MAX_FIXED_ARGUMENTS {
        return Err(format!(
            "入口 `{entrypoint_name}` 的 fixed_args 不能超过 {MAX_FIXED_ARGUMENTS} 项"
        ));
    }
    for argument in arguments {
        if argument.is_empty()
            || argument.chars().count() > MAX_FIXED_ARGUMENT_CHARS
            || argument.contains(['\0', '\r', '\n'])
        {
            return Err(format!("入口 `{entrypoint_name}` 包含无效的 fixed_args"));
        }
    }
    Ok(())
}

fn validate_constraints(
    object: &Map<String, Value>,
    constraints: &ManifestConstraints,
) -> Result<(), String> {
    for group in &constraints.at_least_one {
        if !group
            .iter()
            .any(|name| constraint_value_is_present(object.get(name)))
        {
            return Err(format!("至少需要提供以下参数之一: {}", group.join(", ")));
        }
    }
    for group in &constraints.exactly_one {
        let count = group
            .iter()
            .filter(|name| constraint_value_is_present(object.get(*name)))
            .count();
        if count != 1 {
            return Err(format!("以下参数必须且只能提供一个: {}", group.join(", ")));
        }
    }
    for group in &constraints.at_most_one {
        let count = group
            .iter()
            .filter(|name| constraint_value_is_present(object.get(*name)))
            .count();
        if count > 1 {
            return Err(format!("以下参数最多提供一个: {}", group.join(", ")));
        }
    }
    Ok(())
}

fn constraint_value_is_present(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(values)) => !values.is_empty(),
        Some(_) => true,
    }
}

async fn read_limited<R>(reader: Option<R>, limit: usize) -> std::io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok((Vec::new(), false));
    };
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8 * 1024];
    let mut truncated = false;
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Err(error) => return Err(error),
            Ok(read) => {
                let remaining = limit.saturating_sub(output.len());
                output.extend_from_slice(&chunk[..read.min(remaining)]);
                if read > remaining {
                    truncated = true;
                }
            }
        }
    }
    Ok((output, truncated))
}

fn collect_output(
    result: Result<std::io::Result<(Vec<u8>, bool)>, JoinError>,
) -> Result<(String, bool), String> {
    match result {
        Ok(Ok((bytes, truncated))) => Ok((String::from_utf8_lossy(&bytes).into_owned(), truncated)),
        Ok(Err(error)) => Err(format!("读取进程输出失败: {error}")),
        Err(error) => Err(format!("进程输出任务失败: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fixture(root: &Path, manifest: &str) -> PathBuf {
        let skill_dir = root.join("skills/demo-skill");
        std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "---\nname: demo-skill\n---\n").unwrap();
        std::fs::write(skill_dir.join("scripts/run.py"), "print('ok')\n").unwrap();
        std::fs::write(skill_dir.join("scripts/run.mjs"), "console.log('ok');\n").unwrap();
        std::fs::write(skill_dir.join("scripts/setup.sh"), "#!/usr/bin/env bash\n").unwrap();
        std::fs::write(skill_dir.join("scripts/setup.ps1"), "exit 0\n").unwrap();
        std::fs::write(skill_dir.join(MANIFEST_FILE), manifest).unwrap();
        skill_dir.join("SKILL.md")
    }

    #[test]
    fn loads_manifest_and_builds_schema() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1

[skill]
name = "demo-skill"

[[entrypoints]]
name = "analyze-file"
description = "分析一个本地文件"
runtime = "python"
script = "scripts/run.py"

[entrypoints.constraints]
exactly_one = [["file", "file_url"]]

[[entrypoints.arguments]]
name = "file"
type = "path"
description = "本地文件"
flag = "--file"
must_exist = true

[[entrypoints.arguments]]
name = "file_url"
type = "string"
description = "远程文件地址"
flag = "--file-url"
"#,
        );
        let tools = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name(), "skill__demo_skill__analyze_file");
        let schema = tools[0].input_schema();
        assert_eq!(schema["additionalProperties"], false);
        assert!(schema["allOf"][0]["oneOf"].is_array());
    }

    #[test]
    fn rejects_script_outside_skill() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "bad"
description = "无效入口"
runtime = "python"
script = "../outside.py"
"#,
        );
        let error = load_skill_entrypoints(temp.path(), &skill).unwrap_err();
        assert!(error.contains("相对路径"));
    }

    #[test]
    fn validates_constraints_and_normalizes_paths() {
        let temp = tempfile::tempdir().unwrap();
        let input_file = temp.path().join("holding.png");
        std::fs::write(&input_file, b"image").unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "analyze"
description = "分析输入"
runtime = "python"
script = "scripts/run.py"

[entrypoints.constraints]
at_least_one = [["images", "image_urls"]]
at_most_one = [["quick", "deep"]]

[[entrypoints.arguments]]
name = "images"
type = "url_or_path_array"
description = "本地或远程图片"
flag = "--image"
must_exist = true

[[entrypoints.arguments]]
name = "image_urls"
type = "string_array"
description = "远程图片"
flag = "--image-url"

[[entrypoints.arguments]]
name = "quick"
type = "boolean"
description = "快速模式"
flag = "--quick"

[[entrypoints.arguments]]
name = "deep"
type = "boolean"
description = "深度模式"
flag = "--deep"
"#,
        );
        let tool = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap()
            .remove(0);
        let missing = tool.validate_and_build_argv(&json!({})).unwrap_err();
        assert!(missing.contains("至少需要"));
        let conflicting = tool
            .validate_and_build_argv(&json!({
                "images": ["holding.png"],
                "quick": true,
                "deep": true
            }))
            .unwrap_err();
        assert!(conflicting.contains("最多提供一个"));
        let argv = tool
            .validate_and_build_argv(&json!({
                "images": ["holding.png", "HTTPS://example.com/image.png"]
            }))
            .unwrap();
        assert_eq!(argv[0], "--image");
        assert_eq!(PathBuf::from(&argv[1]), input_file.canonicalize().unwrap());
        assert_eq!(argv[2], "--image");
        assert_eq!(argv[3], "HTTPS://example.com/image.png");
    }

    #[test]
    fn detects_only_direct_script_execution() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "run"
description = "运行入口"
runtime = "python"
script = "scripts/run.py"
"#,
        );
        let tool = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap()
            .remove(0);
        assert!(tool.matches_legacy_shell_command("python skills/demo-skill/scripts/run.py"));
        assert!(tool.matches_legacy_shell_command("python skills/wrong-name/scripts/run.py"));
        assert!(!tool.matches_legacy_shell_command("rg run.py skills/demo-skill"));
        assert!(!tool.matches_legacy_shell_command("python skills/wrong-name/scripts/notrun.py"));
        assert!(!tool.matches_legacy_shell_command("python skills/wrong-name/scripts/run.py.bak"));
    }

    #[test]
    fn supports_node_platform_shell_and_structured_arrays() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "build-deck"
description = "生成演示文稿"
runtime = "node"
script = "scripts/run.mjs"

[[entrypoints.arguments]]
name = "box"
type = "integer_array"
description = "四个边界坐标"
required = true
flag = "--box"
repeat_flag = false
min_items = 4
max_items = 4
minimum = 0
maximum = 1000

[[entrypoints.arguments]]
name = "ratio"
type = "number"
description = "缩放比例"
flag = "--ratio"

[[entrypoints]]
name = "setup"
description = "初始化依赖"
runtime = "platform_shell"
script = "scripts/setup.sh"
windows_script = "scripts/setup.ps1"
"#,
        );
        let tools = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap();
        let argv = tools[0]
            .validate_and_build_argv(&json!({"box": [1, 2, 3, 4], "ratio": 0.5}))
            .unwrap();
        assert_eq!(
            argv,
            ["--box", "1", "2", "3", "4", "--ratio", "0.5"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert!(tools[1]
            .matches_legacy_shell_command("powershell -File skills/demo-skill/scripts/setup.ps1"));
    }

    #[test]
    fn path_or_directory_accepts_existing_files_and_directories() {
        let temp = tempfile::tempdir().unwrap();
        let output_dir = temp.path().join("output");
        std::fs::create_dir(&output_dir).unwrap();
        let output_file = temp.path().join("output.png");
        std::fs::write(&output_file, b"png").unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "render"
description = "渲染到文件或目录"
runtime = "python"
script = "scripts/run.py"

[[entrypoints.arguments]]
name = "output"
type = "path_or_directory"
description = "输出路径"
required = true
flag = "--output"
"#,
        );
        let tool = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap()
            .remove(0);
        for output in [&output_dir, &output_file] {
            let argv = tool
                .validate_and_build_argv(&json!({"output": output}))
                .unwrap();
            assert_eq!(PathBuf::from(&argv[1]), output.canonicalize().unwrap());
        }
    }

    #[test]
    fn rejects_non_contiguous_positions_and_invalid_array_bounds() {
        let temp = tempfile::tempdir().unwrap();
        let position_error = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"
[[entrypoints]]
name = "run"
description = "运行入口"
runtime = "python"
script = "scripts/run.py"
[[entrypoints.arguments]]
name = "value"
type = "string"
description = "值"
position = 1
"#,
        );
        assert!(load_skill_entrypoints(temp.path(), &position_error)
            .unwrap_err()
            .contains("position 必须从 0 开始连续递增"));

        let bounds_error = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"
[[entrypoints]]
name = "run"
description = "运行入口"
runtime = "python"
script = "scripts/run.py"
[[entrypoints.arguments]]
name = "values"
type = "string_array"
description = "值列表"
flag = "--value"
max_items = 0
"#,
        );
        assert!(load_skill_entrypoints(temp.path(), &bounds_error)
            .unwrap_err()
            .contains("数组长度约束无效"));
    }

    #[test]
    fn revalidation_rejects_script_content_changes() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"
[[entrypoints]]
name = "run"
description = "运行入口"
runtime = "python"
script = "scripts/run.py"
"#,
        );
        let tool = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap()
            .remove(0);
        std::fs::write(
            temp.path().join("skills/demo-skill/scripts/run.py"),
            "print('changed')\n",
        )
        .unwrap();
        assert!(tool
            .revalidate_definition()
            .unwrap_err()
            .contains("定义或脚本内容已改变"));
    }

    #[test]
    fn revalidation_rejects_unrelated_manifest_entry_changes() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"
[[entrypoints]]
name = "run"
description = "运行入口"
runtime = "python"
script = "scripts/run.py"
"#,
        );
        let tool = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap()
            .remove(0);
        std::fs::write(
            temp.path().join("skills/demo-skill/skill.toml"),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"
[[entrypoints]]
name = "run"
description = "运行入口"
runtime = "python"
script = "scripts/run.py"
[[entrypoints]]
name = "other"
description = "另一个入口"
runtime = "python"
script = "scripts/run.py"
"#,
        )
        .unwrap();
        assert!(tool
            .revalidate_definition()
            .unwrap_err()
            .contains("Manifest 在加载后发生变化"));
    }

    #[test]
    fn package_children_inherit_manifest_fixed_args_and_number_arrays() {
        let temp = tempfile::tempdir().unwrap();
        let package = temp.path().join("skills/market-package");
        std::fs::create_dir_all(package.join("skills")).unwrap();
        std::fs::create_dir_all(package.join("tools")).unwrap();
        let child = package.join("skills/research.md");
        std::fs::write(&child, "# 研究\n\n包内 Skill。\n").unwrap();
        std::fs::write(package.join("tools/calc.py"), "print('ok')\n").unwrap();
        std::fs::write(
            package.join(MANIFEST_FILE),
            r#"
manifest_version = 1

[skill]
name = "market-package"

[[entrypoints]]
name = "scenario"
description = "计算情景。"
runtime = "python"
script = "tools/calc.py"
fixed_args = ["scenario"]

[[entrypoints.arguments]]
name = "values"
type = "number_array"
description = "情景数值。"
required = true
flag = "--values"
repeat_flag = false
min_items = 2
max_items = 2
"#,
        )
        .unwrap();

        let tool = load_skill_entrypoints(temp.path(), &child)
            .unwrap()
            .unwrap()
            .remove(0);
        let argv = tool
            .validate_and_build_argv(&json!({"values": [1.5, -2]}))
            .unwrap();
        assert_eq!(
            argv,
            ["scenario", "--values", "1.5", "-2"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            tool.input_schema["properties"]["values"]["items"]["type"],
            "number"
        );
        assert!(tool.matches_legacy_shell_command("python skills/market-package/tools/guessed.py"));
    }

    #[test]
    fn loads_repository_skill_manifests() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let skills_dir = repository.join("skills");
        let mut loaded = 0_usize;
        for entry in std::fs::read_dir(&skills_dir).unwrap() {
            let skill_dir = entry.unwrap().path();
            if !skill_dir.join(MANIFEST_FILE).is_file() {
                continue;
            }
            let skill_name = skill_dir.file_name().unwrap().to_string_lossy();
            let skill_md = if skill_dir.join("SKILL.md").is_file() {
                skill_dir.join("SKILL.md")
            } else {
                std::fs::read_dir(skill_dir.join("skills"))
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.extension().and_then(|value| value.to_str()) == Some("md"))
                    .expect("带 Manifest 的包必须至少包含一个 Skill 入口")
            };
            let tools = load_skill_entrypoints(&repository, &skill_md)
                .unwrap()
                .unwrap();
            assert!(!tools.is_empty(), "{skill_name}");
            loaded += 1;
        }
        assert!(loaded >= 20, "仅加载了 {loaded} 个 Skill Manifest");
    }

    #[test]
    fn repository_skill_docs_do_not_execute_scripts_directly() {
        fn collect_markdown(directory: &Path, files: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    collect_markdown(&path, files);
                } else if path.extension().and_then(|value| value.to_str()) == Some("md") {
                    files.push(path);
                }
            }
        }

        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        // 已知债务：手机端原生兜底包（pick-funds / read-macro / watch-positions）按
        // 「文档教模型经 shell 直跑 scripts/*.py」设计，是 termux 侧实测可用的机制，
        // 尚未迁移为 Manifest 工具（迁移需逐子命令建入口并在手机端回归）。迁移前
        // 它们的文档豁免本守卫；其余包出现脚本路径引用仍然直接失败。
        const PENDING_MANIFEST_MIGRATION: &[&str] = &[
            "pick-funds",
            "read-macro",
            "watch-positions",
            // zcode 生产力包（docx/xlsx/pptx/pdf，2026-10 移植）：按「文档教模型经
            // shell 直跑 scripts/*」设计（QA/校验脚本十几个、含 Node 运行时），
            // Manifest 迁移待办；迁移前豁免本守卫。
            "docx",
            "pdf",
            "pptx",
            "xlsx",
        ];
        let executable_path =
            regex::Regex::new(r#"(?i)(?:scripts|tools)[/\\][^`"']+\.(?:py|js|mjs|sh|ps1)"#)
                .unwrap();
        let mut files = Vec::new();
        collect_markdown(&repository.join("skills"), &mut files);
        for path in files {
            if PENDING_MANIFEST_MIGRATION.iter().any(|pkg| {
                path.components()
                    .any(|c| c.as_os_str().to_string_lossy() == *pkg)
            }) {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for (index, line) in source.lines().enumerate() {
                if !executable_path.is_match(line) {
                    continue;
                }
                panic!(
                    "{}:{} 仍包含可执行 Skill 脚本路径: {}",
                    path.display(),
                    index + 1,
                    line
                );
            }
        }
    }

    #[tokio::test]
    #[ignore = "需要系统 Python 和 GLM Skill 依赖，作为显式端到端检查运行"]
    async fn repository_glm5v_entrypoint_dry_run() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let skill_md = repository.join("skills/glm5v-multimodal/SKILL.md");
        let tools = load_skill_entrypoints(&repository, &skill_md)
            .unwrap()
            .unwrap();
        let tool = tools
            .into_iter()
            .find(|tool| tool.name() == "skill__glm5v_multimodal__analyze_files")
            .unwrap();
        let result = tool
            .execute_direct(json!({
                "prompt": "只验证结构化动态入口，不调用模型 API。",
                "files": ["skills/glm5v-multimodal/SKILL.md"],
                "dry_run": true
            }))
            .await;
        assert!(!result.is_error, "{}", result.content);
        assert!(result.content.contains("\"mode\": \"file\""));
    }

    // —— PythonScriptExecutor 抽象（Android 移植阶段 2.1）——

    /// 注入槽是进程级全局：用例之间必须串行，避免 guard drop 清掉彼此的注入。
    static EXECUTOR_SLOT_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 注入槽守卫：drop 时还原缺省执行器/限额，避免污染同进程的其他测试。
    struct ExecutorSlotGuard;

    impl Drop for ExecutorSlotGuard {
        fn drop(&mut self) {
            clear_python_script_executor();
            clear_skill_output_limit();
        }
    }

    fn lock_executor_slot_for_test() -> std::sync::MutexGuard<'static, ()> {
        EXECUTOR_SLOT_TEST_MUTEX
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    type RecordedRequest = (
        PathBuf,
        Vec<OsString>,
        Vec<(String, String)>,
        Duration,
        usize,
    );

    struct RecordingExecutor {
        seen: std::sync::Mutex<Option<RecordedRequest>>,
        result: Result<ScriptOutcome, ScriptFailure>,
    }

    #[async_trait]
    impl PythonScriptExecutor for RecordingExecutor {
        async fn execute(
            &self,
            request: PythonScriptRequest<'_>,
        ) -> Result<ScriptOutcome, ScriptFailure> {
            *self.seen.lock().unwrap() = Some((
                request.script.to_path_buf(),
                request.argv.to_vec(),
                request.env.to_vec(),
                request.timeout,
                request.max_output_bytes,
            ));
            self.result.clone()
        }
    }

    fn python_entrypoint_fixture(temp: &Path) -> Arc<SkillEntrypointTool> {
        let skill = write_fixture(
            temp,
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "analyze-file"
description = "分析一个本地文件"
runtime = "python"
script = "scripts/run.py"

[[entrypoints.arguments]]
name = "file"
type = "string"
description = "参数"
flag = "--file"
"#,
        );
        let tools = load_skill_entrypoints(temp, &skill).unwrap().unwrap();
        tools.into_iter().next().unwrap()
    }

    #[tokio::test]
    // 执行器槽位是进程级单例：锁必须跨 await 持有以串行化并发测试。
    #[allow(clippy::await_holding_lock)]
    async fn injected_python_executor_receives_request_and_renders_outcome() {
        let _serialize = lock_executor_slot_for_test();
        let temp = tempfile::tempdir().unwrap();
        let tool = python_entrypoint_fixture(temp.path());
        let executor = Arc::new(RecordingExecutor {
            seen: std::sync::Mutex::new(None),
            result: Ok(ScriptOutcome {
                exit_code: Some(0),
                stdout: "桥输出".into(),
                stderr: String::new(),
                stdout_truncated: false,
                stderr_truncated: true,
            }),
        });
        set_python_script_executor(executor.clone());
        let _guard = ExecutorSlotGuard;

        let result = tool
            .execute_with_source_channel(json!({"file": "x.txt"}), "napcat")
            .await;
        assert!(!result.is_error);
        assert_eq!(
            result.content,
            "专用工具: skill__demo_skill__analyze_file\n退出码: 0\n输出截断: stdout=false, stderr=true\n[stdout]\n桥输出\n[stderr]\n"
        );
        let seen = executor.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.0, tool.script_path().to_path_buf());
        assert_eq!(
            seen.1,
            ["--file", "x.txt"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            seen.2,
            [("RCLAW_SOURCE_CHANNEL".to_string(), "napcat".to_string())]
        );
        assert_eq!(seen.3, Duration::from_millis(DEFAULT_TIMEOUT_MS));
        assert_eq!(seen.4, MAX_OUTPUT_BYTES);
    }

    #[tokio::test]
    // 执行器槽位是进程级单例：锁必须跨 await 持有以串行化并发测试。
    #[allow(clippy::await_holding_lock)]
    async fn injected_executor_failure_maps_to_legacy_error_text() {
        let _serialize = lock_executor_slot_for_test();
        let temp = tempfile::tempdir().unwrap();
        let tool = python_entrypoint_fixture(temp.path());
        set_python_script_executor(Arc::new(RecordingExecutor {
            seen: std::sync::Mutex::new(None),
            result: Err(ScriptFailure::Timeout),
        }));
        let _guard = ExecutorSlotGuard;

        let result = tool.execute_direct(json!({"file": "x.txt"})).await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            format!("专用工具执行超时（{DEFAULT_TIMEOUT_MS} ms）: skill__demo_skill__analyze_file")
        );

        // Unavailable：APK 桥 poisoned 的宿主侧状态映射。
        set_python_script_executor(Arc::new(RecordingExecutor {
            seen: std::sync::Mutex::new(None),
            result: Err(ScriptFailure::Unavailable("运行时已标记为不可用".into())),
        }));
        let result = tool.execute_direct(json!({"file": "x.txt"})).await;
        assert!(result.is_error);
        assert_eq!(
            result.content,
            "专用工具不可用（skill__demo_skill__analyze_file）: 运行时已标记为不可用"
        );
    }

    #[tokio::test]
    // 执行器槽位是进程级单例：锁必须跨 await 持有以串行化并发测试。
    #[allow(clippy::await_holding_lock)]
    async fn skill_output_limit_slot_feeds_executor_request() {
        let _serialize = lock_executor_slot_for_test();
        let temp = tempfile::tempdir().unwrap();
        let tool = python_entrypoint_fixture(temp.path());
        set_skill_output_limit(2048);
        let executor = Arc::new(RecordingExecutor {
            seen: std::sync::Mutex::new(None),
            result: Ok(ScriptOutcome {
                exit_code: Some(3),
                stdout: String::new(),
                stderr: "err".into(),
                stdout_truncated: false,
                stderr_truncated: false,
            }),
        });
        set_python_script_executor(executor.clone());
        let _guard = ExecutorSlotGuard;

        let result = tool.execute_direct(json!({"file": "x"})).await;
        assert!(result.is_error, "非零退出码必须渲染为错误结果");
        assert!(result.content.contains("退出码: 3"));
        let seen = executor.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.4, 2048, "限额槽位值必须透传到执行请求");
    }

    /// 缺省进程执行器：真实 python 上验证参数/编码 env/来源通道透传与字节截断。
    /// 宿主无 python 时跳过（与桌面门禁同一宽松度）。
    #[tokio::test]
    async fn process_executor_runs_system_python_with_utf8_env_and_limit() {
        let program = match resolve_python_interpreter(Path::new("."), &None) {
            Ok(program) => program,
            Err(_) => return,
        };
        let temp = tempfile::tempdir().unwrap();
        let script = temp.path().join("probe.py");
        std::fs::write(
            &script,
            "import os, sys\nprint(sys.argv[1])\nprint(os.environ['PYTHONUTF8'], os.environ.get('RCLAW_SOURCE_CHANNEL'))\n",
        )
        .unwrap();
        let resolver: PythonResolver = Arc::new(move || Ok(program.clone()));
        let executor = ProcessPythonExecutor::new(resolver);
        let argv = [OsString::from("值")];
        let env = [("RCLAW_SOURCE_CHANNEL".to_string(), "napcat".to_string())];

        let outcome = executor
            .execute(PythonScriptRequest {
                script: &script,
                argv: &argv,
                cwd: temp.path(),
                env: &env,
                timeout: Duration::from_secs(30),
                max_output_bytes: MAX_OUTPUT_BYTES,
            })
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0), "{}", outcome.stderr);
        assert!(outcome.stdout.contains("值"), "{}", outcome.stdout);
        assert!(outcome.stdout.contains("1 napcat"), "{}", outcome.stdout);
        assert!(!outcome.stdout_truncated);

        // 字节截断：多字节字符流按字节上限截断并标记 truncated；切在字符中间时
        // 残字节按桌面 from_utf8_lossy 语义解码为替换符（3 字节）。
        std::fs::write(&script, "print('漢' * 100)\n").unwrap();
        let outcome = executor
            .execute(PythonScriptRequest {
                script: &script,
                argv: &[],
                cwd: temp.path(),
                env: &[],
                timeout: Duration::from_secs(30),
                max_output_bytes: 10,
            })
            .await
            .unwrap();
        assert_eq!(outcome.exit_code, Some(0));
        assert!(outcome.stdout_truncated);
        assert_eq!(outcome.stdout, "漢漢漢\u{fffd}");
    }

    /// APK 宿主只保留 Python 入口；桌面/Termux（标记关闭）完整返回。
    #[test]
    fn apk_host_filters_non_python_entrypoints() {
        let temp = tempfile::tempdir().unwrap();
        let skill = write_fixture(
            temp.path(),
            r#"
manifest_version = 1
[skill]
name = "demo-skill"

[[entrypoints]]
name = "analyze"
description = "python 入口"
runtime = "python"
script = "scripts/run.py"

[[entrypoints]]
name = "render"
description = "node 入口"
runtime = "node"
script = "scripts/run.mjs"

[[entrypoints]]
name = "setup"
description = "shell 入口"
runtime = "platform_shell"
script = "scripts/setup.sh"
windows_script = "scripts/setup.ps1"
"#,
        );
        let tools = load_skill_entrypoints(temp.path(), &skill)
            .unwrap()
            .unwrap();
        assert_eq!(tools.len(), 3, "桌面不设标记时必须完整返回");
        let filtered = filter_tools_for_host_flag(tools, true);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name(), "skill__demo_skill__analyze");
    }
}
