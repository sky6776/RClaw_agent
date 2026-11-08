//! SOUL 人格库：内置人格物化、选择与导入。
//!
//! 设计要点：
//! - **内置人格编译期嵌入**（`include_str!`，源在 `crates/rclaw-store/assets/souls/`）
//!   ——APK/桌面/CLI 三宿主同一份来源，不依赖 APK assets 解包链路；缺文件直接
//!   编译失败，防止「打包时漏了人格」。
//!   刻意**不**读 `agent/config/*.md`：那是运行时可编辑/可被本模块覆盖的目录，
//!   若作为构建输入，本地切一次人格就会改变下次构建的「默认人格」。
//! - **运行时只读 `config/SOUL.md` 一个路径**（`Layout::soul_file`，见
//!   `rclaw-runtime::context`）；本模块负责把库里选中的那份**复制**成它，
//!   并保留库文件本身供用户继续编辑。
//! - **永不静默覆盖用户编辑**：库文件已存在且内容与内置不同 → 保留；切换生效
//!   人格时若 `SOUL.md` 已改过 → 先要求确认（`NeedsConfirm`），确认后自动备份
//!   旧内容再写。
//! - 导入的文件名与内容在**后端**做硬校验（前端不可绕过）：大小、UTF-8、NUL、
//!   字符集、路径逃逸、重名去重。

use std::path::{Path, PathBuf};

use crate::{atomic_write, Layout, Result, StoreError};

/// `init_layout` 写出的占位内容（trim 后比较）。它不算「用户编辑过」。
const PLACEHOLDER: &str = "# Personality";

/// 单个人格文件导入上限（人格是纯文本提示词，256 KiB 足够宽松）。
pub const MAX_IMPORT_BYTES: usize = 256 * 1024;
/// 单次批量导入的文件数上限。
pub const MAX_IMPORT_FILES: usize = 20;
/// 保存到库里的文件名长度上限（字符数，含扩展名）。
const MAX_FILE_NAME_CHARS: usize = 80;

/// 一个人格项。
#[derive(Debug, Clone, Copy)]
pub struct SoulPersona {
    /// 稳定 id（API/前端标识；与文件名解耦，重命名库文件不影响它）。
    pub id: &'static str,
    /// 展示名（配置页列表）。
    pub label: &'static str,
    /// 落到 `config/souls/` 下的文件名。
    pub file_name: &'static str,
    /// 人格正文（编译期嵌入）。
    pub content: &'static str,
}

/// 内置人格库。第一项是**默认人格**（全新安装时播种为生效人格）。
///
/// 来源是仓库 `agent/config/`；`include_str!` 让「漏文件」在编译期暴露。
pub const BUILTIN_SOULS: [SoulPersona; 4] = [
    SoulPersona {
        id: "wechat",
        label: "微信真人口吻（默认）",
        file_name: "SOUL-微信真人口吻.md",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/souls/SOUL-微信真人口吻.md"
        )),
    },
    SoulPersona {
        id: "sushi",
        label: "苏轼（重文言）",
        file_name: "SOUL-苏轼-重文言.md",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/souls/SOUL-苏轼-重文言.md"
        )),
    },
    SoulPersona {
        id: "dongbei",
        label: "东北老铁",
        file_name: "SOUL-东北老铁.md",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/souls/SOUL-东北老铁.md"
        )),
    },
    SoulPersona {
        id: "likui",
        label: "李逵",
        file_name: "SOUL-李逵.md",
        content: include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/souls/SOUL-李逵.md"
        )),
    },
];

/// 默认人格（全新安装播种用）。
pub fn default_soul() -> &'static SoulPersona {
    &BUILTIN_SOULS[0]
}

/// 人格库目录：`config/souls/`。
pub fn souls_dir(layout: &Layout) -> PathBuf {
    layout.config_dir.join("souls")
}

/// 启动期准备结果（供 boot log）。
#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    /// 新写入库的内置人格文件名。
    pub written: Vec<String>,
    /// 已存在且内容与内置不同 → 保留（用户编辑过）。
    pub preserved: Vec<String>,
    /// 是否播种了默认生效人格。
    pub seeded_default: bool,
}

/// 启动期准备：物化四份内置人格 + 播种默认人格。
///
/// 三宿主（CLI/桌面/APK）在 `prepare_storage` 里统一调用；幂等，可每次启动都跑。
pub fn prepare_souls(layout: &Layout) -> Result<ImportReport> {
    let _guard = soul_lock();
    let dir = souls_dir(layout);
    std::fs::create_dir_all(&dir)
        .map_err(|e| StoreError::Other(format!("创建人格库目录失败（{}）: {e}", dir.display())))?;
    let mut report = ImportReport::default();
    for persona in BUILTIN_SOULS.iter() {
        let path = dir.join(persona.file_name);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == persona.content => {}
            Ok(_) => report.preserved.push(persona.file_name.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                atomic_write(&path, persona.content.as_bytes())?;
                report.written.push(persona.file_name.to_string());
            }
            Err(e) => {
                return Err(StoreError::Other(format!(
                    "读取人格库文件失败（{}）: {e}",
                    path.display()
                )))
            }
        }
    }
    report.seeded_default = seed_default_soul_locked(layout)?;
    Ok(report)
}

/// 生效人格缺失或是 `init_layout` 占位时，写入默认人格。返回是否发生写入。
pub fn seed_default_soul(layout: &Layout) -> Result<bool> {
    let _guard = soul_lock();
    seed_default_soul_locked(layout)
}

/// [`seed_default_soul`] 的已持锁版本（`prepare_souls` 内部调用，避免重入死锁）。
fn seed_default_soul_locked(layout: &Layout) -> Result<bool> {
    let existing = active_soul_bytes(layout)?;
    let needs_seed = match existing.as_deref() {
        None => true,
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(content) => content.trim().is_empty() || is_placeholder(content),
            // 非 UTF-8 视为用户内容：不播种（避免吞掉无法解码的旧文件）。
            Err(_) => false,
        },
    };
    if !needs_seed {
        return Ok(false);
    }
    atomic_write(&layout.soul_file, default_soul().content.as_bytes())?;
    // 标记失败不阻断播种：内容已写入，列表退化为按内容匹配/当前未知。
    let _ = record_active_marker(layout, default_soul().file_name);
    Ok(true)
}

fn is_placeholder(content: &str) -> bool {
    content.trim() == PLACEHOLDER
}

/// 人格库条目（配置页列表用）。
#[derive(Debug, Clone)]
pub struct PersonaEntry {
    pub file_name: String,
    pub bytes: u64,
    pub active: bool,
}

/// 列出人格库里可用的 `.md` 文件（按文件名排序），标记当前生效的那份。
///
/// 生效判定按**内容**而不是文件名：用户把库文件复制成 `SOUL.md` 后改了两行，
/// 就不算同一份。
pub fn list_personas(layout: &Layout) -> Result<Vec<PersonaEntry>> {
    // 直接复用 personas_state：单次持锁 + 单次扫描，且 `active_empty`（空/占位
    // 生效人格不标任何库文件）等判定只在一处，公开入口不会与配置页口径不一致。
    Ok(personas_state(layout)?.entries)
}

/// 扫描人格库一次，返回 `(文件名, 字节数, 可解码内容)`，按文件名排序。
fn collect_personas(layout: &Layout) -> Result<Vec<(String, u64, Option<String>)>> {
    let dir = souls_dir(layout);
    let mut out: Vec<(String, u64, Option<String>)> = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => {
            return Err(StoreError::Other(format!(
                "读取人格库目录失败（{}）: {e}",
                dir.display()
            )))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|e| StoreError::Other(format!("遍历人格库失败: {e}")))?;
        let path = entry.path();
        if !path.is_file() || !is_persona_extension(&path) {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push((
            file_name.to_string(),
            bytes,
            std::fs::read_to_string(&path).ok(),
        ));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn count_matches(collected: &[(String, u64, Option<String>)], active: Option<&str>) -> usize {
    match active {
        None => 0,
        Some(active) => collected
            .iter()
            .filter(|(_, _, content)| content.as_deref() == Some(active))
            .count(),
    }
}

/// 由一次扫描结果构造列表项并标记「当前」。
fn build_entries(
    collected: Vec<(String, u64, Option<String>)>,
    active_content: Option<&str>,
    marked: Option<&str>,
    matches: usize,
) -> Vec<PersonaEntry> {
    // marker 只有在「指向库内真实文件且该文件内容等于生效人格」时才算数；
    // 文件被删/改名/内容变了就当作没有 marker，退回内容匹配。
    let marked_valid = marked.filter(|name| {
        active_content.is_some_and(|active| {
            collected.iter().any(|(file_name, _, content)| {
                file_name == name && content.as_deref() == Some(active)
            })
        })
    });
    collected
        .into_iter()
        .map(|(file_name, bytes, content)| {
            let same_content = match (active_content, content.as_deref()) {
                (Some(active), Some(content)) => active == content,
                _ => false,
            };
            let active = same_content
                && match marked_valid {
                    Some(name) => name == file_name,
                    // 没有有效 marker：只有唯一匹配时才敢标；多份同内容又不确定
                    // 是哪份时一律不标（宁可显示「当前未知」，也不标错）。
                    None => matches == 1,
                };
            PersonaEntry {
                file_name,
                bytes,
                active,
            }
        })
        .collect()
}

/// 配置页需要的完整状态：**一次持锁、一次库扫描**算出，保证各字段同源。
#[derive(Debug, Clone, Default)]
pub struct SoulState {
    pub entries: Vec<PersonaEntry>,
    /// 生效人格有内容、不等于任何库文件（用户改过）。
    pub active_edited: bool,
    /// 生效人格缺失/为空/仍是占位（还没播种或被清空）。
    pub active_empty: bool,
    /// 生效人格有内容、库内有多份同内容、且没有有效选择标记——无法唯一确定是哪份。
    pub active_ambiguous: bool,
}

/// 一次持锁取完整状态。
///
/// 配置页各字段必须同源：分多次调用、或在同一函数里多次扫描库，都会在中间插入
/// FilePanel 保存/apply/import 时给出自相矛盾的快照（列表标 A、edited 按 B 算）。
/// 这里只读一次 `SOUL.md`、只扫一次 `souls/`。
pub fn personas_state(layout: &Layout) -> Result<SoulState> {
    let _guard = soul_lock();
    let marked = read_active_marker(layout);
    let collected = collect_personas(layout)?;
    let bytes = active_soul_bytes(layout)?;
    let (active_content, active_edited) = match bytes.as_deref() {
        None => (None, false),
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(content) => {
                let edited = is_edited_content_in(&collected, content);
                (Some(content.to_string()), edited)
            }
            // 读不出的旧文件：算「已编辑」（覆盖前必须确认 + 备份原始字节），
            // 但无法与库内容比对，列表一个都不标。
            Err(_) => (None, true),
        },
    };
    let active_empty = match active_content.as_deref() {
        None => !active_edited,
        Some(content) => content.trim().is_empty() || is_placeholder(content),
    };
    // 空/占位状态下不参与「当前」比对：库内可能恰好也有一份空白/占位人格文件，
    // 按内容相等把它标成「当前」会和 active_empty 的提示自相矛盾。
    let comparable = if active_empty {
        None
    } else {
        active_content.as_deref()
    };
    let matches = count_matches(&collected, comparable);
    // 歧义只在「有实际生效内容 + 多份同内容 + 无有效标记」时成立。空/占位/缺失
    // 属于 active_empty，UI 不能用「同内容多份」的文案去解释它。
    let marked_valid = marked.as_deref().is_some_and(|name| {
        comparable.is_some_and(|active| {
            collected.iter().any(|(file_name, _, content)| {
                file_name == name && content.as_deref() == Some(active)
            })
        })
    });
    let active_ambiguous = !active_edited && !active_empty && matches > 1 && !marked_valid;
    let entries = build_entries(collected, comparable, marked.as_deref(), matches);
    Ok(SoulState {
        entries,
        active_edited,
        active_empty,
        active_ambiguous,
    })
}

/// 生效人格是否被用户编辑过。
///
/// 判据：`config/SOUL.md` 存在、非空、不是占位，且内容不等于人格库里的任何一份。
/// 缺失/空/占位都视为「没东西可丢」，不算编辑。
pub fn is_active_edited(layout: &Layout) -> bool {
    match active_soul_bytes(layout) {
        Ok(None) => false,
        // 文件存在但读不出来/不是 UTF-8：当作「有内容且未知」——宁可多问一次，
        // 也不能当成「没有旧内容」直接覆盖（否则一次编码/权限异常就丢数据）。
        Ok(Some(bytes)) => match std::str::from_utf8(&bytes) {
            Ok(content) => {
                is_edited_content_in(&collect_personas(layout).unwrap_or_default(), content)
            }
            Err(_) => true,
        },
        Err(_) => true,
    }
}

/// 单份内容的「是否被编辑过」判据：非空、非占位、且不等于库内任何一份。
fn is_edited_content_in(collected: &[(String, u64, Option<String>)], content: &str) -> bool {
    if content.trim().is_empty() || is_placeholder(content) {
        return false;
    }
    !collected
        .iter()
        .any(|(_, _, candidate)| candidate.as_deref() == Some(content))
}

/// 读生效人格的原始字节。`Ok(None)` 仅表示真正的 NotFound；其他 I/O 错误上抛，
/// 由调用方决定「不动文件」。
fn active_soul_bytes(layout: &Layout) -> Result<Option<Vec<u8>>> {
    match std::fs::read(&layout.soul_file) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(StoreError::Other(format!(
            "读取生效人格失败（{}）: {e}",
            layout.soul_file.display()
        ))),
    }
}

/// 应用人格的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    /// 已写入生效人格。`backup` = 覆盖前的旧文件备份路径（无旧文件/无需备份时为 None）。
    Applied {
        persona: String,
        backup: Option<PathBuf>,
    },
    /// 生效人格被编辑过，需要用户确认后才覆盖。
    NeedsConfirm,
    /// 目标内容已经是生效人格。
    AlreadyActive,
    /// 人格库里没有这个文件。
    NotFound,
}

/// 把库里的某个人格应用为生效人格。
///
/// `force=false` 时，若生效人格被编辑过则返回 [`ApplyOutcome::NeedsConfirm`]；
/// `force=true` 时先备份旧文件（`SOUL.md.bak.<YYYYMMDD-HHmmss>`）再覆盖。
pub fn apply_persona(layout: &Layout, file_name: &str, force: bool) -> Result<ApplyOutcome> {
    // 整个 read→backup→write 序列串行化：避免与并发的应用/导入/文件面板保存交错。
    let _guard = soul_lock();
    let dir = souls_dir(layout);
    let Some(safe_name) = validate_existing_file_name(file_name) else {
        return Ok(ApplyOutcome::NotFound);
    };
    let source = dir.join(&safe_name);
    // 不跟随符号链接：库里的 .md 若是指向库外的链接，拒绝应用（读取会泄露库外内容，
    // 也可能把库外文件复制成人格）。
    match std::fs::symlink_metadata(&source) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(StoreError::Other(format!(
                "人格文件是符号链接，拒绝应用：{safe_name}"
            )))
        }
        Ok(meta) if !meta.is_file() => return Ok(ApplyOutcome::NotFound),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ApplyOutcome::NotFound),
        Err(e) => {
            return Err(StoreError::Other(format!(
                "读取人格文件属性失败（{}）: {e}",
                source.display()
            )))
        }
    }
    if let Ok(meta) = std::fs::metadata(&source) {
        if meta.len() > MAX_IMPORT_BYTES as u64 {
            return Err(StoreError::Other(format!(
                "人格文件过大（{} 字节，上限 {MAX_IMPORT_BYTES}），拒绝应用",
                meta.len()
            )));
        }
    }
    let source_bytes = std::fs::read(&source)
        .map_err(|e| StoreError::Other(format!("读取人格失败（{}）: {e}", source.display())))?;
    // 与导入同一套内容校验：库文件可能是外部/文件面板直接放进去的二进制。
    validate_soul_content(&source_bytes)
        .map_err(|e| StoreError::Other(format!("人格文件不可用（{safe_name}）：{e}")))?;
    let current = active_soul_bytes(layout)?;
    if current.as_deref() == Some(source_bytes.as_slice()) {
        record_active_marker(layout, &safe_name)?;
        return Ok(ApplyOutcome::AlreadyActive);
    }
    // 读不出/非 UTF-8 的旧文件一律按「已编辑」处理：必须先确认并备份原始字节。
    let edited = match current.as_deref() {
        None => false,
        Some(bytes) => match std::str::from_utf8(bytes) {
            Ok(content) => is_edited_content_in(&collect_personas(layout)?, content),
            Err(_) => true,
        },
    };
    if edited && !force {
        return Ok(ApplyOutcome::NeedsConfirm);
    }
    let backup = if edited {
        backup_active_soul(layout)?
    } else {
        None
    };
    atomic_write(&layout.soul_file, &source_bytes)?;
    // 内容已写入，但标记写失败会让列表标错「当前」——作为错误上报，用户重试
    // 即可（重试走到 AlreadyActive 分支，会再写一次标记）。
    record_active_marker(layout, &safe_name)?;
    Ok(ApplyOutcome::Applied {
        persona: safe_name,
        backup,
    })
}

fn backup_active_soul(layout: &Layout) -> Result<Option<PathBuf>> {
    let Some(bytes) = active_soul_bytes(layout)? else {
        return Ok(None);
    };
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    for attempt in 0..100 {
        let name = if attempt == 0 {
            format!("SOUL.md.bak.{stamp}")
        } else {
            format!("SOUL.md.bak.{stamp}-{attempt}")
        };
        let candidate = layout.config_dir.join(name);
        // `create_new` 独占创建：并发的两次备份不可能落到同一个文件（exists()+copy
        // 的窗口里，后一次会把前一次的备份覆盖掉）。
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                use std::io::Write as _;
                if let Err(e) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
                    // 写失败的备份不能留下：否则 config/ 里会有个看着能恢复、
                    // 实际是 0 字节/半截的 .bak，出事时才发现救不回来。
                    drop(file);
                    return Err(StoreError::Other(match std::fs::remove_file(&candidate) {
                        Ok(()) => format!("写入备份失败（{}）: {e}", candidate.display()),
                        Err(cleanup) => format!(
                            "写入备份失败（{}）: {e}；且清理残留备份失败（{cleanup}），该 .bak 不完整、不可用于恢复",
                            candidate.display()
                        ),
                    }));
                }
                return Ok(Some(candidate));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => {
                return Err(StoreError::Other(format!(
                    "创建备份失败（{}）: {e}",
                    candidate.display()
                )))
            }
        }
    }
    Err(StoreError::Other(
        "备份旧人格失败：同名备份文件过多".to_string(),
    ))
}

/// 单条导入项。
#[derive(Debug, Clone)]
pub struct ImportItem {
    /// 用户设备上的文件名（只取 basename，其余丢弃）。
    pub file_name: String,
    /// 原始字节（前端 base64 解码后传入；UTF-8 校验在后端做）。
    pub content: Vec<u8>,
}

/// 单条导入结果。
#[derive(Debug, Clone)]
pub struct ImportResult {
    /// 用户提交的原始文件名（回显用）。
    pub file_name: String,
    /// 成功时保存到库里的文件名。
    pub saved_as: Option<String>,
    /// 失败原因（成功时为 None）。
    pub error: Option<String>,
}

/// 批量导入人格文件。
///
/// 逐项独立：一个文件失败不影响其他文件；返回值顺序与入参一致。
/// 超过 [`MAX_IMPORT_FILES`] 时整批拒绝（避免一次塞爆目录）。
pub fn import_from_files(layout: &Layout, items: &[ImportItem]) -> Result<Vec<ImportResult>> {
    // 整批持锁：选名（create_new）与写入必须与 apply/seed 互斥，否则「使用」
    // 可能在导入写到一半时把半截文件复制成生效人格。
    let _guard = soul_lock();
    if items.len() > MAX_IMPORT_FILES {
        return Err(StoreError::Other(format!(
            "一次最多导入 {MAX_IMPORT_FILES} 个文件（收到 {} 个）",
            items.len()
        )));
    }
    let dir = souls_dir(layout);
    std::fs::create_dir_all(&dir)
        .map_err(|e| StoreError::Other(format!("创建人格库目录失败（{}）: {e}", dir.display())))?;
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let mut result = ImportResult {
            file_name: item.file_name.clone(),
            saved_as: None,
            error: None,
        };
        match validate_import(item) {
            Ok(()) => match unique_target(&dir, &item.file_name) {
                Ok((target, mut file)) => {
                    use std::io::Write as _;
                    let saved_as = target
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or_default()
                        .to_string();
                    let written = file.write_all(&item.content).and_then(|_| file.sync_all());
                    if let Err(e) = written {
                        // 占位文件已创建：删除它，避免留下 0 字节人格。
                        drop(file);
                        result.error = Some(match std::fs::remove_file(&target) {
                            Ok(()) => format!("写入失败：{e}"),
                            Err(cleanup) => format!(
                                "写入失败：{e}；且清理残留文件失败（{}: {cleanup}），该文件不可用，请手动删除",
                                target.display()
                            ),
                        });
                    } else {
                        result.saved_as = Some(saved_as);
                    }
                }
                Err(e) => result.error = Some(e),
            },
            Err(e) => result.error = Some(e),
        }
        out.push(result);
    }
    Ok(out)
}

fn validate_import(item: &ImportItem) -> std::result::Result<(), String> {
    validate_soul_content(&item.content)?;
    if sanitize_file_name(&item.file_name).is_none() {
        return Err("文件名不合法（只允许 .md/.markdown/.txt，字符限中英文/数字/._-）".to_string());
    }
    Ok(())
}

/// 在目标目录里独占创建一个不冲突的文件名：`name.md`、`name-2.md`、`name-3.md`…
///
/// 返回打开的文件句柄（已 `create_new` 占位），调用方直接写入——`exists()` 检查
/// 在并发导入下会被另一个请求抢走同名目标，`create_new` 由文件系统仲裁。
fn unique_target(
    dir: &Path,
    raw_name: &str,
) -> std::result::Result<(PathBuf, std::fs::File), String> {
    let name = sanitize_file_name(raw_name).ok_or_else(|| "文件名不合法".to_string())?;
    let (stem, ext) = split_stem_ext(&name);
    for attempt in 0..100 {
        let suffix = if attempt == 0 {
            String::new()
        } else {
            format!("-{}", attempt + 1)
        };
        // 去重后缀必须计入长度上限，否则生成的名字会因为超长而无法应用。
        let budget =
            MAX_FILE_NAME_CHARS.saturating_sub(suffix.chars().count() + ext.chars().count() + 1);
        if budget == 0 {
            return Err("文件名过长，无法生成去重名".to_string());
        }
        let trimmed: String = stem.chars().take(budget).collect();
        let candidate_name = format!("{trimmed}{suffix}.{ext}");
        let candidate = dir.join(&candidate_name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("创建文件失败：{e}")),
        }
    }
    Err("同名文件过多，无法生成唯一文件名".to_string())
}

fn split_stem_ext(name: &str) -> (String, String) {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem.to_string(), ext.to_string()),
        _ => (name.to_string(), "md".to_string()),
    }
}

fn is_persona_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase),
        Some(ext) if ext == "md" || ext == "markdown" || ext == "txt"
    )
}

/// 应用人格时的文件名校验：**不改写**，只确认是库内的一级文件名。
///
/// 导入侧的 `sanitize_file_name` 不是幂等的（`a--.md` → `a-.md` → `a.md`），
/// 拿它校验已有文件名会「选错人格」——这里必须精确使用列表返回的名字。
fn validate_existing_file_name(name: &str) -> Option<String> {
    // 不做 trim：库里若真有 `" a.md"`，按 `"a.md"` 去匹配会命中另一份人格。
    let trimmed = name;
    if trimmed.is_empty()
        || trimmed.starts_with('.')
        || trimmed.contains('/')
        || trimmed.contains('\\')
        || trimmed.contains("..")
    {
        return None;
    }
    if !is_persona_extension(Path::new(trimmed)) {
        return None;
    }
    if trimmed.chars().count() > MAX_FILE_NAME_CHARS {
        return None;
    }
    Some(trimmed.to_string())
}

/// 人格库/生效文件的进程内互斥：应用、导入、播种都改同一批文件，串行化避免
/// read→check→write 之间被另一个请求插入。
fn soul_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
}

/// 「当前人格文件名」记录：放在人格库内（`.active` 不是 .md，不会被列表收录），
/// 用于两份内容相同的人格之间消歧。
fn active_marker_path(layout: &Layout) -> PathBuf {
    souls_dir(layout).join(".active")
}

fn write_active_marker(layout: &Layout, file_name: &str) -> Result<()> {
    atomic_write(active_marker_path(layout), file_name.as_bytes())
}

fn read_active_marker(layout: &Layout) -> Option<String> {
    // 精确读取（不 trim）：marker 记录的就是原文件名。只容忍文件末尾换行
    // （手工编辑可能加），中间/前导空白一律保留。
    let name = std::fs::read_to_string(active_marker_path(layout)).ok()?;
    // 只容忍文件末尾换行（含 Windows CRLF）：文件名本身一个字符都不能动。
    let name = name
        .strip_suffix("\r\n")
        .or_else(|| name.strip_suffix('\n'))
        .unwrap_or(&name)
        .to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// marker 写入失败不改变「已应用」的事实，但必须留日志：下次列表会退回内容匹配，
/// 同内容多份时可能标错，排查时需要这条线索。
fn record_active_marker(layout: &Layout, file_name: &str) -> Result<()> {
    if let Err(error) = write_active_marker(layout, file_name) {
        // 旧 marker 若还在，会指向「上一份」人格：内容相同的多份文件时列表会
        // 静默标错。先尽力删掉它，让列表退回「无法唯一确定」。
        let marker = active_marker_path(layout);
        let cleanup = std::fs::remove_file(&marker);
        tracing::warn!(
            "写入当前人格标记失败（{}）：{error}；清理旧标记：{:?}",
            file_name,
            cleanup
        );
        // 旧 marker 本来就不存在（NotFound）等于「已经清理干净」，不算失败。
        let cleanup_ok = match &cleanup {
            Ok(()) => true,
            Err(e) => e.kind() == std::io::ErrorKind::NotFound,
        };
        return Err(StoreError::Other(if cleanup_ok {
            format!("写入当前人格标记失败（{error}）；已确保无旧标记，列表会显示「当前未知」")
        } else {
            format!(
                "写入当前人格标记失败（{error}）；且旧标记无法清除（{}），列表可能标错「当前」",
                marker.display()
            )
        }));
    }
    Ok(())
}

/// 人格内容校验（导入与「应用库内已有文件」共用）：非空、非纯空白、≤上限、
/// 无 NUL、合法 UTF-8。
fn validate_soul_content(bytes: &[u8]) -> std::result::Result<&str, String> {
    if bytes.is_empty() {
        return Err("文件为空".to_string());
    }
    if bytes.len() > MAX_IMPORT_BYTES {
        return Err(format!(
            "文件过大（{} 字节，上限 {MAX_IMPORT_BYTES}）",
            bytes.len()
        ));
    }
    if bytes.contains(&0) {
        return Err("内容含 NUL 字节（疑似二进制文件）".to_string());
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "不是合法的 UTF-8 文本".to_string())?;
    if text.trim().is_empty() {
        return Err("内容为空或只有空白".to_string());
    }
    Ok(text)
}

/// 文件名清洗：只取 basename、拒绝前导点/路径分隔、字符集白名单、补扩展名。
///
/// 允许中英文、数字、`.`、`_`、`-`；其他字符（空格、括号等）替换为 `-`。
/// 扩展名不在 `.md/.markdown/.txt` 时补 `.md`（如 `foo`、`foo.doc` → `foo.doc.md`）。
fn sanitize_file_name(raw: &str) -> Option<String> {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base.starts_with('.') || base == ".." {
        return None;
    }
    let mut cleaned = String::with_capacity(base.len());
    let mut last_dash = false;
    for ch in base.chars() {
        let keep = ch.is_ascii_alphanumeric()
            || matches!(ch, '.' | '_' | '-')
            || ('\u{4e00}'..='\u{9fff}').contains(&ch);
        if keep {
            // `-` 紧跟 `.` 是空格/括号被替换后的痕迹（如「人格 (1).md」），折叠掉。
            if ch == '.' && cleaned.ends_with('-') {
                cleaned.pop();
            }
            cleaned.push(ch);
            last_dash = false;
        } else if !last_dash && !cleaned.is_empty() {
            cleaned.push('-');
            last_dash = true;
        }
    }
    let cleaned = cleaned.trim_end_matches(['.', '-', '_']).to_string();
    if cleaned.is_empty() {
        return None;
    }
    // 用 Path::extension 判定真实扩展名（不能用 split_stem_ext 的默认值，否则
    // 无扩展名的 `foo` 会被误判成已有 `.md` 并原样保存）。
    let has_known_ext = matches!(
        std::path::Path::new(&cleaned)
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("md") | Some("markdown") | Some("txt")
    );
    let final_name = if has_known_ext {
        cleaned
    } else {
        format!("{cleaned}.md")
    };
    if final_name.chars().count() > MAX_FILE_NAME_CHARS {
        return None;
    }
    // 最后一道：文件名本身不能逃出目录（`..` 已被前导点规则挡住，这里再确认）。
    if final_name.contains('/') || final_name.contains('\\') || final_name.contains("..") {
        return None;
    }
    Some(final_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout_with_tempdir() -> (tempfile::TempDir, Layout) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path().join("agent"));
        crate::fs::init_layout(&layout).unwrap();
        (dir, layout)
    }

    #[test]
    fn prepare_souls_materializes_all_builtins_and_seeds_default() {
        let (_dir, layout) = layout_with_tempdir();
        let report = prepare_souls(&layout).unwrap();
        assert_eq!(report.written.len(), BUILTIN_SOULS.len());
        assert!(report.seeded_default);
        // 占位被替换成默认人格。
        let active = std::fs::read_to_string(&layout.soul_file).unwrap();
        assert_eq!(active, default_soul().content);
        for persona in BUILTIN_SOULS.iter() {
            let path = souls_dir(&layout).join(persona.file_name);
            assert_eq!(std::fs::read_to_string(path).unwrap(), persona.content);
        }
    }

    #[test]
    fn prepare_souls_is_idempotent_and_preserves_user_edits() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 用户改库文件与生效人格。
        let edited_lib = souls_dir(&layout).join(BUILTIN_SOULS[1].file_name);
        std::fs::write(&edited_lib, "我改过的人格\n").unwrap();
        std::fs::write(&layout.soul_file, "我自己的生效人格\n").unwrap();

        let report = prepare_souls(&layout).unwrap();
        assert!(report.written.is_empty(), "第二次不应再写任何库文件");
        assert_eq!(
            report.preserved,
            vec![BUILTIN_SOULS[1].file_name.to_string()]
        );
        assert!(!report.seeded_default, "生效人格非占位，不应被播种覆盖");
        assert_eq!(
            std::fs::read_to_string(&edited_lib).unwrap(),
            "我改过的人格\n"
        );
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            "我自己的生效人格\n"
        );
    }

    #[test]
    fn seed_does_not_touch_non_placeholder_content() {
        let (_dir, layout) = layout_with_tempdir();
        std::fs::write(&layout.soul_file, "自定义\n").unwrap();
        assert!(!seed_default_soul(&layout).unwrap());
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            "自定义\n"
        );
    }

    #[test]
    fn seed_replaces_empty_file_and_placeholder() {
        let (_dir, layout) = layout_with_tempdir();
        std::fs::write(&layout.soul_file, "   \n").unwrap();
        assert!(seed_default_soul(&layout).unwrap());
        std::fs::write(&layout.soul_file, "# Personality\n\n").unwrap();
        assert!(seed_default_soul(&layout).unwrap());
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            default_soul().content
        );
    }

    #[test]
    fn active_edited_states() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 默认人格（等于库文件）→ 未编辑。
        assert!(!is_active_edited(&layout));
        // 缺失 → 不算编辑。
        std::fs::remove_file(&layout.soul_file).unwrap();
        assert!(!is_active_edited(&layout));
        // 空 → 不算编辑。
        std::fs::write(&layout.soul_file, "  \n").unwrap();
        assert!(!is_active_edited(&layout));
        // 占位 → 不算编辑。
        std::fs::write(&layout.soul_file, "# Personality\n\n").unwrap();
        assert!(!is_active_edited(&layout));
        // 改过 → 编辑。
        std::fs::write(&layout.soul_file, "我自己写的人格\n").unwrap();
        assert!(is_active_edited(&layout));
    }

    #[test]
    fn apply_moves_persona_and_reports_already_active() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let target = BUILTIN_SOULS[1].file_name;
        let outcome = apply_persona(&layout, target, false).unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Applied {
                persona: target.to_string(),
                backup: None
            }
        );
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            BUILTIN_SOULS[1].content
        );
        // 再应用一次 → 已是当前。
        assert_eq!(
            apply_persona(&layout, target, false).unwrap(),
            ApplyOutcome::AlreadyActive
        );
        // 不存在的人格。
        assert_eq!(
            apply_persona(&layout, "不存在.md", false).unwrap(),
            ApplyOutcome::NotFound
        );
        // 路径逃逸被当作不存在。
        assert_eq!(
            apply_persona(&layout, "../SOUL.md", false).unwrap(),
            ApplyOutcome::NotFound
        );
    }

    #[test]
    fn apply_requires_confirm_and_backs_up_when_forced() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        std::fs::write(&layout.soul_file, "我手写的旧人格\n").unwrap();
        let target = BUILTIN_SOULS[2].file_name;

        assert_eq!(
            apply_persona(&layout, target, false).unwrap(),
            ApplyOutcome::NeedsConfirm
        );
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            "我手写的旧人格\n",
            "未确认时绝不能动生效人格"
        );

        let outcome = apply_persona(&layout, target, true).unwrap();
        let ApplyOutcome::Applied { backup, .. } = outcome else {
            panic!("force 应直接应用，实际 {outcome:?}");
        };
        let backup = backup.expect("覆盖用户编辑必须产生备份");
        assert_eq!(
            std::fs::read_to_string(&backup).unwrap(),
            "我手写的旧人格\n"
        );
        assert!(backup
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("SOUL.md.bak."));
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            BUILTIN_SOULS[2].content
        );
    }

    #[test]
    fn bulk_import_dedups_within_batch_and_against_library() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let items = vec![
            ImportItem {
                file_name: "自定义.md".to_string(),
                content: "人格 A\n".into(),
            },
            ImportItem {
                file_name: "自定义.md".to_string(),
                content: "人格 B\n".into(),
            },
            // 与内置库重名（SOUL-李逵.md 已存在）。
            ImportItem {
                file_name: BUILTIN_SOULS[3].file_name.to_string(),
                content: "人格 C\n".into(),
            },
        ];
        let results = import_from_files(&layout, &items).unwrap();
        assert_eq!(results[0].saved_as.as_deref(), Some("自定义.md"));
        assert_eq!(results[1].saved_as.as_deref(), Some("自定义-2.md"));
        assert_eq!(
            results[2].saved_as.as_deref(),
            Some("SOUL-李逵-2.md"),
            "库内重名要递增去重，不能覆盖内置人格"
        );
        let dir = souls_dir(&layout);
        assert_eq!(
            std::fs::read_to_string(dir.join("自定义-2.md")).unwrap(),
            "人格 B\n"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(BUILTIN_SOULS[3].file_name)).unwrap(),
            BUILTIN_SOULS[3].content,
            "内置人格内容不得被导入覆盖"
        );
    }

    #[test]
    fn bulk_import_rejects_bad_items_but_keeps_good_ones() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let items = vec![
            ImportItem {
                file_name: "好.md".to_string(),
                content: "正常人格\n".into(),
            },
            ImportItem {
                file_name: "空.md".to_string(),
                content: Vec::new(),
            },
            ImportItem {
                file_name: "二进制.md".to_string(),
                content: vec![0x00, 0x01, 0x02],
            },
            ImportItem {
                file_name: "太大.md".to_string(),
                content: vec![b'a'; MAX_IMPORT_BYTES + 1],
            },
            ImportItem {
                file_name: "非法UTF8.md".to_string(),
                content: vec![0xff, 0xfe, 0xfd],
            },
            ImportItem {
                file_name: "../逃逸.md".to_string(),
                content: "x\n".into(),
            },
        ];
        let results = import_from_files(&layout, &items).unwrap();
        assert!(results[0].saved_as.is_some(), "好文件应成功: {results:?}");
        // 1..=4 是空/二进制/超大/非法 UTF-8，必须逐项失败。
        for bad in &results[1..5] {
            assert!(bad.saved_as.is_none(), "坏文件不应落盘: {bad:?}");
            assert!(bad.error.is_some());
        }
        // 路径逃逸按设计只取 basename：`../逃逸.md` → 库内 `逃逸.md`，
        // 绝不可能写出 souls 目录之外（下面显式验证）。
        assert_eq!(results[5].saved_as.as_deref(), Some("逃逸.md"));
        assert!(!layout.config_dir.parent().unwrap().join("逃逸.md").exists());
    }

    #[test]
    fn bulk_import_limits_batch_size() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let items: Vec<ImportItem> = (0..=MAX_IMPORT_FILES)
            .map(|i| ImportItem {
                file_name: format!("p{i}.md"),
                content: "x\n".into(),
            })
            .collect();
        assert!(import_from_files(&layout, &items).is_err());
    }

    #[test]
    fn file_name_sanitizer_rules() {
        assert_eq!(sanitize_file_name("人格.md").as_deref(), Some("人格.md"));
        assert_eq!(
            sanitize_file_name("C:\\x\\人格 (1).md").as_deref(),
            Some("人格-1.md")
        );
        assert_eq!(
            sanitize_file_name("无扩展名").as_deref(),
            Some("无扩展名.md")
        );
        assert_eq!(sanitize_file_name("a.txt").as_deref(), Some("a.txt"));
        assert_eq!(sanitize_file_name(".隐藏.md"), None);
        assert_eq!(sanitize_file_name(".."), None);
        assert_eq!(sanitize_file_name(""), None);
        assert_eq!(sanitize_file_name("///"), None);
        let long = "字".repeat(MAX_FILE_NAME_CHARS + 1);
        assert_eq!(sanitize_file_name(&long), None);
    }

    #[test]
    fn non_utf8_active_file_requires_confirm_and_backups_raw_bytes() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 非 UTF-8 的旧人格：不能被当作「没有旧内容」直接覆盖。
        let raw = vec![0xff_u8, 0xfe, 0x00, 0x41];
        std::fs::write(&layout.soul_file, &raw).unwrap();
        assert!(is_active_edited(&layout), "读不出的旧文件必须算已编辑");

        let target = BUILTIN_SOULS[1].file_name;
        assert_eq!(
            apply_persona(&layout, target, false).unwrap(),
            ApplyOutcome::NeedsConfirm
        );
        assert_eq!(
            std::fs::read(&layout.soul_file).unwrap(),
            raw,
            "未确认不得动文件"
        );

        let ApplyOutcome::Applied { backup, .. } = apply_persona(&layout, target, true).unwrap()
        else {
            panic!("force 应直接应用");
        };
        let backup = backup.expect("非 UTF-8 旧文件必须备份");
        assert_eq!(std::fs::read(&backup).unwrap(), raw, "备份必须是原始字节");
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            BUILTIN_SOULS[1].content
        );
    }

    #[test]
    fn import_rejects_blank_content() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let items = vec![ImportItem {
            file_name: "空白.md".to_string(),
            content: "  \n\t\n".as_bytes().to_vec(),
        }];
        let results = import_from_files(&layout, &items).unwrap();
        assert!(results[0].saved_as.is_none());
        assert!(results[0].error.as_deref().unwrap().contains("空白"));
    }

    #[test]
    fn apply_uses_exact_library_name_without_resanitizing() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 库内直接放一个「清洗不幂等」风格的文件名（外部/文件面板可创建）。
        let dir = souls_dir(&layout);
        std::fs::write(dir.join("a--.md"), "连字符人格\n").unwrap();
        std::fs::write(dir.join("a.md"), "短名字人格\n").unwrap();

        let outcome = apply_persona(&layout, "a--.md", false).unwrap();
        assert_eq!(
            outcome,
            ApplyOutcome::Applied {
                persona: "a--.md".to_string(),
                backup: None
            },
            "应用必须精确命中列表里的文件名，不能二次清洗成 a.md"
        );
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            "连字符人格\n"
        );
    }

    #[test]
    fn dedup_suffix_respects_name_length_limit() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let long_stem = "字".repeat(76); // + ".md" = 79 字符
        let first = format!("{long_stem}.md");
        let items = vec![
            ImportItem {
                file_name: first.clone(),
                content: "第一份\n".into(),
            },
            ImportItem {
                file_name: first.clone(),
                content: "第二份\n".into(),
            },
        ];
        let results = import_from_files(&layout, &items).unwrap();
        let second = results[1].saved_as.clone().expect("去重后仍应成功");
        assert!(
            second.chars().count() <= MAX_FILE_NAME_CHARS,
            "去重名超长将无法应用：{second}"
        );
        // 生成的名字必须能被 apply 接受。
        assert_eq!(
            apply_persona(&layout, &second, false).unwrap(),
            ApplyOutcome::Applied {
                persona: second,
                backup: None
            }
        );
    }

    #[test]
    fn duplicate_content_marks_only_marker_file_active() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 复制一份内容相同、文件名不同的人格。
        let dir = souls_dir(&layout);
        let src = dir.join(BUILTIN_SOULS[1].file_name);
        let dup = dir.join("苏轼副本.md");
        std::fs::copy(&src, &dup).unwrap();

        // 应用其中一份 → 只有它标 active（sidecar 记录文件名）。
        let outcome = apply_persona(&layout, "苏轼副本.md", false).unwrap();
        assert!(matches!(outcome, ApplyOutcome::Applied { .. }));
        let entries = list_personas(&layout).unwrap();
        let active: Vec<_> = entries.iter().filter(|e| e.active).collect();
        assert_eq!(active.len(), 1, "同内容双份只能有一份算当前: {entries:?}");
        assert_eq!(active[0].file_name, "苏轼副本.md");
    }

    #[test]
    fn apply_rejects_symlinked_persona_file() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let dir = souls_dir(&layout);
        let outside = layout.config_dir.join("outside.md");
        std::fs::write(&outside, "库外内容\n").unwrap();
        let link = dir.join("链接人格.md");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&outside, &link).is_ok();
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_file(&outside, &link).is_ok();
        if !made {
            // 无权限创建符号链接（Windows 非开发者模式）：跳过该断言。
            return;
        }
        let err = apply_persona(&layout, "链接人格.md", true).unwrap_err();
        assert!(err.to_string().contains("符号链接"), "{err}");
        assert_ne!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            "库外内容\n",
            "绝不能把库外内容复制成生效人格"
        );
    }

    #[test]
    fn apply_rejects_binary_library_file() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 文件面板/外部可以在库里塞一个二进制 .md：apply 必须拒绝，不能把
        // 读不出的人格复制成 SOUL.md（否则「已应用」但提示词里根本没有）。
        let dir = souls_dir(&layout);
        std::fs::write(dir.join("二进制.md"), [0u8, 1, 2, 3]).unwrap();
        let err = apply_persona(&layout, "二进制.md", true).unwrap_err();
        assert!(
            err.to_string().contains("NUL") || err.to_string().contains("UTF-8"),
            "{err}"
        );
        assert_eq!(
            std::fs::read_to_string(&layout.soul_file).unwrap(),
            default_soul().content
        );
    }

    #[test]
    fn stale_active_marker_falls_back_to_content_match() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // marker 指向一个已被删除的文件：列表必须仍能按内容标出唯一「当前」。
        std::fs::write(souls_dir(&layout).join(".active"), "已删除的人格.md").unwrap();
        let entries = list_personas(&layout).unwrap();
        let active: Vec<_> = entries.iter().filter(|e| e.active).collect();
        assert_eq!(active.len(), 1, "marker 失效时应退回内容匹配: {entries:?}");
        assert_eq!(active[0].file_name, default_soul().file_name);
    }

    #[test]
    fn marker_selects_exact_file_among_identical_content() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let dir = souls_dir(&layout);
        std::fs::copy(dir.join(BUILTIN_SOULS[1].file_name), dir.join("副本B.md")).unwrap();
        // 先应用到 副本B → marker 记录它；同内容的原文件不能被误标为当前。
        assert!(matches!(
            apply_persona(&layout, "副本B.md", false).unwrap(),
            ApplyOutcome::Applied { .. }
        ));
        let entries = list_personas(&layout).unwrap();
        let active: Vec<_> = entries.iter().filter(|e| e.active).collect();
        assert_eq!(active.len(), 1, "{entries:?}");
        assert_eq!(active[0].file_name, "副本B.md");
    }

    #[test]
    fn marker_with_crlf_still_selects_exact_file() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let dir = souls_dir(&layout);
        std::fs::copy(dir.join(BUILTIN_SOULS[1].file_name), dir.join("副本C.md")).unwrap();
        assert!(matches!(
            apply_persona(&layout, "副本C.md", false).unwrap(),
            ApplyOutcome::Applied { .. }
        ));
        // 手工编辑 marker 留下 CRLF：仍应精确命中，不能退回「未知/第一份」。
        let marker = dir.join(".active");
        let name = std::fs::read_to_string(&marker).unwrap();
        std::fs::write(
            &marker,
            format!(
                "{name}
"
            ),
        )
        .unwrap();
        let entries = list_personas(&layout).unwrap();
        let active: Vec<_> = entries.iter().filter(|e| e.active).collect();
        assert_eq!(active.len(), 1, "{entries:?}");
        assert_eq!(active[0].file_name, "副本C.md");
    }

    #[test]
    fn ambiguous_duplicates_without_marker_mark_nothing_active() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let dir = souls_dir(&layout);
        // 两份同内容 + 生效文件等于该内容 + 无 marker → 无法确定是哪一份，不标。
        std::fs::copy(dir.join(BUILTIN_SOULS[1].file_name), dir.join("副本D.md")).unwrap();
        std::fs::write(&layout.soul_file, BUILTIN_SOULS[1].content).unwrap();
        let _ = std::fs::remove_file(dir.join(".active"));
        let entries = list_personas(&layout).unwrap();
        assert!(
            entries.iter().all(|e| !e.active),
            "有歧义时宁可显示「当前未知」也不能标错: {entries:?}"
        );
    }

    #[test]
    fn blank_active_never_marks_library_entry_current() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        // 库里放一份空白/占位人格：生效文件也是占位时，它不能被标成「当前」。
        let dir = souls_dir(&layout);
        std::fs::write(
            dir.join("空白人格.md"),
            "   
",
        )
        .unwrap();
        std::fs::write(
            &layout.soul_file,
            "# Personality
",
        )
        .unwrap();
        let state = personas_state(&layout).unwrap();
        assert!(state.active_empty, "占位应报 active_empty");
        assert!(
            state.entries.iter().all(|e| !e.active),
            "空/占位状态不得标记任何库文件为当前: {:?}",
            state.entries
        );
    }

    #[test]
    fn ambiguous_state_true_when_duplicates_without_valid_marker() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let dir = souls_dir(&layout);
        std::fs::copy(dir.join(BUILTIN_SOULS[1].file_name), dir.join("副本E.md")).unwrap();
        std::fs::write(&layout.soul_file, BUILTIN_SOULS[1].content).unwrap();
        let _ = std::fs::remove_file(dir.join(".active"));
        let state = personas_state(&layout).unwrap();
        assert!(state.active_ambiguous, "同内容多份且无 marker 应报歧义");
        assert!(!state.active_empty);
        assert!(state.entries.iter().all(|e| !e.active));
    }

    #[test]
    fn public_list_also_ignores_blank_active() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let dir = souls_dir(&layout);
        // 公开入口必须与 personas_state 同口径：占位生效 + 库内空白人格 → 不标。
        std::fs::write(
            dir.join("空白人格2.md"),
            "# Personality
",
        )
        .unwrap();
        std::fs::write(
            &layout.soul_file,
            "# Personality
",
        )
        .unwrap();
        assert!(
            list_personas(&layout).unwrap().iter().all(|e| !e.active),
            "list_personas 不能把占位内容标成当前"
        );
    }

    #[test]
    fn list_marks_active_by_content() {
        let (_dir, layout) = layout_with_tempdir();
        prepare_souls(&layout).unwrap();
        let entries = list_personas(&layout).unwrap();
        assert_eq!(entries.len(), BUILTIN_SOULS.len());
        let active: Vec<_> = entries.iter().filter(|e| e.active).collect();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].file_name, default_soul().file_name);
        // 生效文件被改一行后，不再标记任何库文件为当前。
        let mut edited = std::fs::read_to_string(&layout.soul_file).unwrap();
        edited.push_str("\n额外一行\n");
        std::fs::write(&layout.soul_file, edited).unwrap();
        assert!(list_personas(&layout).unwrap().iter().all(|e| !e.active));
    }
}
