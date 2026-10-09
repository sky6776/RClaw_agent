//! Skill 激活状态（[`DynamicToolSet`]）与技能目录缓存（[`SkillCatalog`]）。
//!
//! [`DynamicToolSet`] 拥有当前 turn 内激活的 Manifest 专用工具：schema、执行表与
//! 逐 skill 激活状态只有一个所有者；调用方构造 ChatRequest 时拼接静态与动态 schema，
//! 不再各自维护两个靠约定同步的容器。
//!
//! [`SkillCatalog`] 缓存每 turn 一次的 skills 目录扫描，供激活检测与动态上下文共享；
//! skill 文件变更时由调用方显式失效（skill_manage 成功、执行前重校验失败）。

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard},
};

use rclaw_core::{ToolCall, ToolSchema};
use rclaw_store::{
    skills::{build_summaries, summaries_to_text_with_stats, SkillStatSamples, SkillSummary},
    Layout,
};
use rclaw_tools::{load_skill_entrypoints, SkillEntrypointTool, SkillUsageStore, Tool};

use crate::turn::{
    is_potential_skill_entry_path, normalize_absolute_lexical_path, normalize_agent_tool_path,
    skill_package_label, TRUSTED_MEDIA_PUBLISHER_SCRIPTS,
};

/// 汇总技能结果遥测为索引标注样本（键 = 顶层技能目录名，值 = (成功数, 总数)）。
///
/// 读取失败（无 .usage.json、文件损坏）按空样本处理——索引退化为无标注版本，
/// 与压缩等其它「增强失败不阻断主路径」的错误策略一致。
fn outcome_samples(layout: &Layout) -> SkillStatSamples {
    let mut stats = SkillStatSamples::new();
    let Ok(records) = SkillUsageStore::new(layout.skills_dir.clone()).records() else {
        return stats;
    };
    for (name, record) in records {
        let total = record.success_count.saturating_add(record.failure_count);
        if total > 0 {
            stats.insert(name, (record.success_count, total));
        }
    }
    stats
}

/// 单个 skill 在当前 turn 内的激活状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ActivationState {
    /// 已读取入口，其 Manifest 专用工具在 schema 与执行表中可调用。
    Active,
    /// 曾激活后被失效（skill 编辑、定义漂移或全量失效），重新读取才能再次激活。
    Invalidated,
}

/// 一次 skill 入口读取的识别结果（路径已规范化，去重后）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillInvocation {
    pub path: PathBuf,
    pub name: String,
    pub package: Option<String>,
}

/// 当前 turn 的 Manifest 专用工具集合：schema、执行表与逐 skill 激活状态。
///
/// 每轮循环新建即完成「每 turn 重置」；失效后旧 schema 与执行表同时消失，
/// 模型必须重新读取对应 Skill 才能再次拿到专用工具。
pub(crate) struct DynamicToolSet {
    /// 静态注册表的工具名，作为动态工具的重名冲突底线。
    static_names: HashSet<String>,
    /// 工具名 → 执行表（含脚本路径与定义指纹）。
    tools: HashMap<String, Arc<SkillEntrypointTool>>,
    /// 动态 schema，按激活顺序排列，构造 ChatRequest 时与静态 schema 拼接。
    schemas: Vec<ToolSchema>,
    /// 逐 skill 激活记录，键为规范化的 SKILL.md / 入口文件路径。
    activations: HashMap<PathBuf, ActivationRecord>,
}

struct ActivationRecord {
    state: ActivationState,
    /// 该 skill 本次激活贡献的全部工具名（含与既有定义一致而跳过插入的）。
    tool_names: Vec<String>,
}

impl DynamicToolSet {
    /// 以静态注册表的 schema 为冲突底线构造空集合；每 turn 新建即完成重置。
    pub(crate) fn new(static_schemas: &[ToolSchema]) -> Self {
        Self {
            static_names: static_schemas
                .iter()
                .map(|schema| schema.name.clone())
                .collect(),
            tools: HashMap::new(),
            schemas: Vec::new(),
            activations: HashMap::new(),
        }
    }

    /// 把刚读取 Skill 的 Manifest 入口加入当前 turn，返回新激活的工具名。
    ///
    /// 失效语义与拆分前的 `activate_skill_entrypoints` 一致：
    /// - 重读时 skill.toml 缺失：仅失效该 skill 的旧工具；
    /// - manifest 解析失败 / 同名动态工具定义漂移 / 与静态工具重名：全量失效。
    pub(crate) fn activate(
        &mut self,
        base: &Path,
        skill_path: &Path,
    ) -> Result<Vec<String>, String> {
        let tools = match load_skill_entrypoints(base, skill_path) {
            Ok(Some(tools)) => tools,
            Ok(None) => {
                // skill.toml 已不存在：按路径失效该 skill 先前激活的旧工具。
                if self.invalidate_path(skill_path) {
                    return Err(
                        "该 Skill 先前已激活 Manifest 专用工具，但当前重读时 skill.toml 已不存在；旧工具已失效"
                            .to_string(),
                    );
                }
                return Ok(Vec::new());
            }
            Err(error) => {
                self.invalidate_all();
                return Err(format!(
                    "{error}；当前 turn 先前激活的 Manifest 专用工具已全部失效"
                ));
            }
        };
        // 先做全量冲突检查：任何一处冲突都让本 turn 先前的动态工具整体失效，
        // 避免模型在「半新半旧」的定义集合里继续调用。
        for tool in &tools {
            let name = tool.name();
            if let Some(existing) = self.tools.get(name) {
                if !existing.has_same_definition(tool) {
                    let error = format!(
                        "同名专用工具 `{name}` 的定义发生冲突（已加载 Skill：{}；新 Skill：{}）",
                        existing.skill_name(),
                        tool.skill_name()
                    );
                    self.invalidate_all();
                    return Err(format!(
                        "{error}；当前 turn 先前激活的 Manifest 专用工具已全部失效"
                    ));
                }
                continue;
            }
            if self.static_names.contains(name) {
                let error = format!("专用工具名与现有工具冲突: {name}");
                self.invalidate_all();
                return Err(format!(
                    "{error}；当前 turn 先前激活的 Manifest 专用工具已全部失效"
                ));
            }
        }

        let mut activated = Vec::new();
        let mut tool_names = Vec::new();
        for tool in tools {
            let name = tool.name().to_string();
            // 定义一致的重复激活不重复插入，但计入该 skill 的记录以便后续按路径失效。
            if !self.tools.contains_key(&name) {
                self.schemas.push(tool.tool_schema());
                self.tools.insert(name.clone(), tool);
                activated.push(name.clone());
            }
            tool_names.push(name);
        }
        // 记录（或刷新）激活状态：Invalidated → Active 的再激活也经由此处。
        // 迁移轨迹落 debug 日志——skill 生命周期是自进化调试热点。
        if let Some(previous) = self.activation_state(skill_path) {
            tracing::debug!(
                skill_path = %skill_path.display(),
                from = ?previous,
                to = ?ActivationState::Active,
                "skill 激活状态迁移"
            );
        }
        self.activations.insert(
            skill_path.to_path_buf(),
            ActivationRecord {
                state: ActivationState::Active,
                tool_names,
            },
        );
        Ok(activated)
    }

    /// 移除全部动态工具，返回先前是否存在任何激活。
    pub(crate) fn invalidate_all(&mut self) -> bool {
        let had_any = !self.tools.is_empty();
        self.tools.clear();
        self.schemas.clear();
        // 保留激活记录并把状态标记为 Invalidated：状态查询仍能回答
        // 「该 skill 本 turn 曾激活过、需要重新读取」。
        for record in self.activations.values_mut() {
            record.state = ActivationState::Invalidated;
        }
        had_any
    }

    /// 当前可随 ChatRequest 发送的动态 schema（按激活顺序）。
    pub(crate) fn active_schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    /// 按名取执行表中的工具。
    pub(crate) fn get(&self, name: &str) -> Option<&Arc<SkillEntrypointTool>> {
        self.tools.get(name)
    }

    /// 是否存在该名字的动态工具。
    pub(crate) fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// 当前激活的工具名（供批次起点快照与 AgentStatus 汇报）。
    pub(crate) fn active_names(&self) -> impl Iterator<Item = &String> {
        self.tools.keys()
    }

    /// 该 skill 路径的激活状态；本 turn 未激活过返回 None。
    pub(crate) fn activation_state(&self, skill_path: &Path) -> Option<ActivationState> {
        self.activations.get(skill_path).map(|record| record.state)
    }

    /// 该动态工具是否为受信任的媒体发布入口。
    ///
    /// Manifest 位于可演进的 Skill 工作区，单独声明 publishes_media 不能把任意脚本
    /// 提升为外发回执信任根；仍只接受既有媒体门禁认可的安装路径。
    pub(crate) fn is_media_publisher(&self, name: &str, skills_dir: &Path) -> bool {
        let Some(tool) = self.tools.get(name).filter(|tool| tool.publishes_media()) else {
            return false;
        };
        TRUSTED_MEDIA_PUBLISHER_SCRIPTS.iter().any(|relative| {
            skills_dir
                .join(relative)
                .canonicalize()
                .is_ok_and(|expected| expected.as_path() == tool.script_path())
        })
    }

    /// 已加载 Manifest 后，拒绝再由 shell 猜测 Skill 脚本路径和参数协议。
    pub(crate) fn legacy_shell_error(&self, command: &str) -> Option<String> {
        let mut matched_tools = self
            .tools
            .values()
            .filter(|tool| tool.matches_legacy_shell_command(command))
            .map(|tool| tool.name().to_string())
            .collect::<Vec<_>>();
        matched_tools.sort();
        matched_tools.dedup();
        if matched_tools.is_empty() {
            return None;
        }
        Some(format!(
            "SKILL_ENTRYPOINT_REQUIRED: 当前 Skill 已启用 Manifest 专用工具，请调用 {}；不要在 shell 中手写、猜测或改写 Skill 脚本路径与参数。",
            matched_tools.join(", ")
        ))
    }

    /// 从执行表与 schema 中移除属于该 skill 的工具，返回是否移除了任何工具。
    ///
    /// 归属判定以工具自身的 [`SkillEntrypointTool::owns_skill_path`] 为准，
    /// 与拆分前 `invalidate_skill_entrypoints_for_path` 的语义一致。
    fn invalidate_path(&mut self, skill_path: &Path) -> bool {
        let stale_names = self
            .tools
            .iter()
            .filter(|(_, tool)| tool.owns_skill_path(skill_path))
            .map(|(name, _)| name.clone())
            .collect::<HashSet<_>>();
        if stale_names.is_empty() {
            return false;
        }
        self.schemas
            .retain(|schema| !stale_names.contains(&schema.name));
        self.tools.retain(|name, _| !stale_names.contains(name));
        for record in self.activations.values_mut() {
            if record
                .tool_names
                .iter()
                .any(|name| stale_names.contains(name))
            {
                record.state = ActivationState::Invalidated;
            }
        }
        true
    }
}

/// skills 目录的每 turn 缓存：摘要列表、渲染文本与读取识别共用一次扫描。
///
/// 现状（拆分前）每个工具批次与每次推理重试都全量重扫 skills 目录；
/// 缓存把扫描收敛为每 turn 一次，skill 文件变更经 [`SkillCatalog::invalidate`]
/// 显式失效后重新可见。
///
/// 缓存用 [`Mutex`] 而非 `RefCell`：catalog 会随 turn / 子任务 future 跨 `await`
/// 存活，必须保持 `Send`；所有方法取锁后立即释放，不在持锁期间 await。
pub(crate) struct SkillCatalog {
    layout: Layout,
    cache: Mutex<CatalogCache>,
}

#[derive(Default)]
struct CatalogCache {
    summaries: Option<Arc<Vec<SkillSummary>>>,
    text: Option<Arc<String>>,
}

impl SkillCatalog {
    /// 以给定布局构造缓存；扫描惰性执行，每 turn 失效一次。
    pub(crate) fn new(layout: &Layout) -> Self {
        Self {
            layout: layout.clone(),
            cache: Mutex::new(CatalogCache::default()),
        }
    }

    /// 取缓存锁。锁中毒说明另一线程 panic 时持锁，缓存数据本身仍完整可用。
    fn lock_cache(&self) -> MutexGuard<'_, CatalogCache> {
        self.cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// skills 摘要列表（缓存；首次调用触发扫描）。
    pub(crate) fn summaries(&self) -> Arc<Vec<SkillSummary>> {
        if let Some(summaries) = self.lock_cache().summaries.clone() {
            return summaries;
        }
        let summaries = Arc::new(build_summaries(&self.layout));
        self.lock_cache().summaries = Some(summaries.clone());
        summaries
    }

    /// 「可用技能」渲染文本（缓存；供动态上下文的 Available Skills 段使用）。
    ///
    /// 结果遥测（.usage.json）样本充足的技能行附带可靠性标注——读取失败只影响
    /// 标注（降级为无标注索引），不阻断技能列表本身。
    pub(crate) fn skills_text(&self) -> Arc<String> {
        if let Some(text) = self.lock_cache().text.clone() {
            return text;
        }
        let stats = outcome_samples(&self.layout);
        let text = Arc::new(summaries_to_text_with_stats(&self.summaries(), &stats));
        self.lock_cache().text = Some(text.clone());
        text
    }

    /// skill 文件已变更：丢弃缓存，下次访问重新扫描。
    pub(crate) fn invalidate(&self) {
        *self.lock_cache() = CatalogCache::default();
    }

    /// 从一组工具调用中识别对已注册 skill 主入口的 `read`。
    ///
    /// 只匹配存储层实际暴露给模型的入口文件，不把同目录 references/scripts
    /// 或普通文件读取误报成 skill 调用；同一批次内重复读取同一入口只计一次。
    pub(crate) fn detect_skill_invocations(&self, tool_calls: &[ToolCall]) -> Vec<SkillInvocation> {
        let current_dir = std::env::current_dir().ok();
        let Some(base) = normalize_absolute_lexical_path(&self.layout.base, current_dir.as_deref())
        else {
            return Vec::new();
        };
        let Some(skills_root) =
            normalize_absolute_lexical_path(&self.layout.skills_dir, current_dir.as_deref())
        else {
            return Vec::new();
        };
        let read_targets = tool_calls
            .iter()
            .filter(|tc| tc.name == "read")
            .filter_map(|tc| serde_json::from_str::<serde_json::Value>(&tc.input).ok())
            .filter_map(|input| input.get("path")?.as_str().map(str::to_string))
            .filter_map(|path| normalize_agent_tool_path(Path::new(&path), &base))
            .filter(|path| is_potential_skill_entry_path(&skills_root, path))
            .collect::<Vec<_>>();
        if read_targets.is_empty() {
            return Vec::new();
        }

        // 摘要走每 turn 缓存；这里按规范化路径建索引供读取目标反查名称与技能包。
        let summaries = self.summaries();
        let catalog = summaries
            .iter()
            .filter_map(|skill| {
                let name = skill
                    .title
                    .chars()
                    .map(|ch| if ch.is_control() { ' ' } else { ch })
                    .collect::<String>()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if name.is_empty() {
                    return None;
                }
                let package = skill_package_label(&self.layout, &skill.path, &name);
                let path = normalize_absolute_lexical_path(
                    Path::new(&skill.path),
                    current_dir.as_deref(),
                )?;
                Some((path, (name, package)))
            })
            .collect::<HashMap<_, _>>();

        let mut seen = HashSet::new();
        read_targets
            .into_iter()
            .filter_map(|path| {
                let (name, package) = catalog.get(&path)?.clone();
                seen.insert(path.clone()).then_some(SkillInvocation {
                    path,
                    name,
                    package,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests;
