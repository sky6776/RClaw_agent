use super::*;

// ---- 校验辅助 ----

pub(super) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("skill name is required".into());
    }
    if name.len() > MAX_NAME_LENGTH {
        return Err(format!("skill name exceeds {MAX_NAME_LENGTH} characters"));
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return Err("skill name must start with a lowercase letter or digit".into()),
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
    {
        return Err("skill name must be lowercase letters/digits and - _ . only".into());
    }
    Ok(())
}

/// 支撑文件路径校验：相对、无 `..`、无绝对/盘符。
///
/// 技能根仅允许三个裸文件：`SKILL.md`（说明）、`skill.toml`（Manifest）与
/// `EXPERIENCE.md`（运行期自动注入的热经验层，见 turn/tool_execution 的加载逻辑）；
/// 其余支撑文件必须落在 [`ALLOWED_SUBDIRS`] 之一（references/templates/scripts/assets）下。
/// 旧实现允许任意裸文件名落在技能根，会在 background review 时于 skill 根目录留下杂文件。
pub(super) fn validate_file_path(file_path: &str) -> Result<(), String> {
    if file_path.is_empty() {
        return Err("file_path is required".into());
    }
    if file_path.contains('\\')
        || file_path.starts_with('/')
        || file_path.contains("..")
        || file_path.contains(':')
    {
        return Err(
            "file_path must be a relative path under the skill (no .. / abs / drive)".into(),
        );
    }
    let fname = file_path.rsplit('/').next().unwrap_or("");
    if fname.is_empty() || fname.starts_with('.') {
        return Err("file_path must end with a non-hidden file name".into());
    }
    // 技能根的裸文件必须与加载器契约的大小写完全一致（EXPERIENCE.md 是热经验层，
    // 读取端按精确文件名匹配，大小写漂移会让经验静默失效）。
    if !file_path.contains('/') {
        if matches!(fname, "SKILL.md" | "skill.toml" | "EXPERIENCE.md") {
            return Ok(());
        }
        if fname.eq_ignore_ascii_case("SKILL.md")
            || fname.eq_ignore_ascii_case("skill.toml")
            || fname.eq_ignore_ascii_case("EXPERIENCE.md")
        {
            return Err(
                "skill root contract file must be named exactly SKILL.md, skill.toml, or EXPERIENCE.md"
                    .into(),
            );
        }
    }
    // 其余支撑文件必须在允许的子目录下（形如 `references/x.md`）。
    let first = file_path.split('/').next().unwrap_or("");
    if !file_path.contains('/') || !ALLOWED_SUBDIRS.contains(&first) {
        return Err(format!(
            "file_path must be under an allowed subdirectory ({}); \
             only SKILL.md, skill.toml, and EXPERIENCE.md are allowed at the skill root",
            ALLOWED_SUBDIRS.join("/")
        ));
    }
    Ok(())
}

pub(super) fn validate_skill_md(content: &str) -> Result<(), String> {
    if content.chars().count() > MAX_SKILL_CONTENT_CHARS {
        return Err(format!("content exceeds {MAX_SKILL_CONTENT_CHARS} chars"));
    }
    if !skill_md_sane(content) {
        return Err("SKILL.md must start with '# ' title or '---' frontmatter".into());
    }
    Ok(())
}

pub(super) fn validate_target_content(target: &Path, content: &str) -> Result<(), String> {
    if content.chars().count() > MAX_SKILL_CONTENT_CHARS {
        return Err(format!("content exceeds {MAX_SKILL_CONTENT_CHARS} chars"));
    }
    if target.file_name().is_some_and(|n| n == "SKILL.md") && !skill_md_sane(content) {
        return Err(
            "patch would break SKILL.md structure (must start with '# ' title or '---' frontmatter)"
                .into(),
        );
    }
    if target
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("skill.toml"))
    {
        let skill_dir = target
            .parent()
            .ok_or_else(|| "skill.toml 缺少 Skill 父目录".to_string())?;
        let base = skill_dir
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| "skill.toml 无法定位 agent 根目录".to_string())?;
        crate::skill_entrypoint::validate_skill_manifest_source(base, skill_dir, content)?;
    }
    Ok(())
}

pub(super) fn skill_md_sane(content: &str) -> bool {
    let t = content.trim_start();
    t.starts_with("# ") || t.starts_with("---")
}

/// 删除安全：
/// 拒绝符号链接/junction、拒绝删 skills 根自身、拒绝跳出 skills_dir。
pub(super) fn validate_delete_target(skills_dir: &Path, skill_dir: &Path) -> Result<(), String> {
    if is_path_redirect(skill_dir) {
        return Err(format!(
            "Refusing to delete '{}': the skill directory is a symlink/junction.",
            skill_dir.display()
        ));
    }
    let canon_root = skills_dir
        .canonicalize()
        .map_err(|e| format!("skills_dir 不可读: {e}"))?;
    let resolved = skill_dir
        .canonicalize()
        .map_err(|e| format!("could not resolve skill dir ({e})"))?;
    if resolved == canon_root {
        return Err(format!(
            "Refusing to delete '{}': resolves to the skills root itself.",
            skill_dir.display()
        ));
    }
    match resolved.strip_prefix(&canon_root) {
        Ok(rel) if !rel.as_os_str().is_empty() => Ok(()),
        _ => Err(format!(
            "Refusing to delete '{}': path does not resolve inside the skills root.",
            skill_dir.display()
        )),
    }
}

/// 路径是否为「重定向」（symlink，或 Windows 上的 junction）。
///
/// 拒绝让一次
/// `rmtree` 被中毒的 skills 树引向 skills 根之外。symlink 跨平台由 std 直接判定；Windows
/// junction（挂载点重解析点）没有跨平台的 `is_junction`，故在 Windows 上用
/// [`windows_path_is_reparse_point`] 查 `FILE_ATTRIBUTE_REPARSE_POINT` 位。外层
/// `resolve_within_base` 的 canonical 沙箱已能挡住
/// 绝大多数逃逸，这里再加一道「skill dir 本身是 junction」的显式拒绝。
pub(crate) fn is_path_redirect(path: &Path) -> bool {
    // 1) 符号链接：跨平台，std 直接支持（Windows 上 std 的 is_symlink 识别 reparse tag = symlink）。
    if path.is_symlink() {
        return true;
    }
    // 2) Windows junction（挂载点重解析）：is_symlink 不识别，读取原生文件属性位。
    #[cfg(windows)]
    if windows_path_is_reparse_point(path) {
        return true;
    }
    false
}

/// Windows：路径是否为重解析点（reparse point，含 junction）。
///
/// `is_symlink` 只认 symlink reparse tag，不认 junction 的 mount-point tag；Windows
/// [`std::os::windows::fs::MetadataExt`] 则直接暴露文件属性。使用原生属性既不依赖外部
/// `fsutil` 命令是否存在/有权限，也不会在检测失败时把指向 skills 内部的 junction 放行。
#[cfg(windows)]
pub(super) fn windows_path_is_reparse_point(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    std::fs::symlink_metadata(path)
        .is_ok_and(|metadata| metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// 把技能目录【归档】到 `skills_dir/.archive/<name>-<ts>/`（可恢复），而非永久删除。
///
/// background curator 的删是「最大破坏性动作」，
/// 必须可经 restore 还原（#29912）。归档目录名带时间戳避免二次归档覆盖。`.archive/` 不会被
/// `build_summaries` 枚举（它只看 `skills/<name>/SKILL.md` / `skills/*.md` /
/// `skills/<pkg>/skills/*.md`），故归档技能不会重新进入系统提示；技能名也不得以 `.` 开头，
/// 与 `.archive` 无命名冲突。
pub(crate) fn archive_skill(
    skills_dir: &Path,
    skill_dir: &Path,
    name: &str,
) -> Result<PathBuf, String> {
    let canon_skills = skills_dir
        .canonicalize()
        .map_err(|e| format!("skills_dir 不可读: {e}"))?;
    let archive_root = canon_skills.join(".archive");
    std::fs::create_dir_all(&archive_root).map_err(|e| format!("创建 .archive 失败: {e}"))?;
    if is_path_redirect(&archive_root) {
        return Err(".archive is a symlink/junction; refusing redirected archive storage".into());
    }
    // 归档目标也走 under_skills 同一套沙箱：若 `.archive` 是预置的、指向 skills 根外的
    // symlink/junction，create_dir_all 会顺着它把内容落到外部目录。这里在 create 之后用
    // resolve_within_base 复核 archive_root（及 dest）的真实落点仍在本 skills 根下——
    // 与读/写/patch/remove 的沙箱语义一致，否则拒绝归档。
    if !rclaw_store::resolve_within_base(&canon_skills, &archive_root) {
        return Err(".archive resolves outside skills_dir (symlink/junction)".into());
    }
    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let dest = archive_root.join(format!("{name}-{ts}"));
    if !rclaw_store::resolve_within_base(&canon_skills, &dest) {
        return Err(format!(
            "archive destination resolves outside skills_dir: {}",
            dest.display()
        ));
    }
    // canon_skills 与 .archive 同处一个文件系统（后者在前者之下），rename 原子且廉价；
    // 不存在跨卷问题，故无需 copy+remove 回退。
    std::fs::rename(skill_dir, &dest).map_err(|e| format!("归档移动失败: {e}"))?;
    Ok(dest)
}

pub(super) fn file_label(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string()
}

/// 写前留 sidecar `.bak`，再用 rclaw-store 的原子写落盘。
/// 写成功且目标是 skills 树下的 Python 脚本时通知宿主钩子（APK 审批摘除；
/// 桌面/Termux 无钩子 = no-op）。
pub(super) fn write_with_backup(target: &Path, content: &str) -> std::io::Result<()> {
    let fname = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("skill");
    let bak = target.with_file_name(format!(".{fname}.patch.bak"));
    if target.exists() {
        let _ = std::fs::copy(target, &bak);
    }
    rclaw_store::fs::atomic_write(target, content.as_bytes())
        .map(|()| crate::skill_entrypoint::notify_skill_script_changed(target))
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// 把路径规范化为可在 tracker 中比较的键（存在则 canonicalize，否则拼 parent 规范路径）。
pub(super) fn canonicalize_for_tracking(target: &Path) -> PathBuf {
    if let Ok(c) = target.canonicalize() {
        return c;
    }
    // 目标可能尚不存在（新建支撑文件）——用 parent 规范路径 + 文件名构造稳定键。
    let parent = target.parent().and_then(|p| p.canonicalize().ok());
    match (parent, target.file_name()) {
        (Some(p), Some(f)) => p.join(f),
        _ => target.to_path_buf(),
    }
}

/// 记录一次读入 tracker（供 review 的读工具调用）。无 tracker 时为 no-op。
pub fn note_read(tracker: &Option<ReviewReadTracker>, path: &Path, content: &[u8]) {
    if let Some(t) = tracker {
        let key = canonicalize_for_tracking(path);
        let digest: [u8; 32] = Sha256::digest(content).into();
        t.lock().expect("read_tracker 锁未毒化").insert(key, digest);
    }
}
