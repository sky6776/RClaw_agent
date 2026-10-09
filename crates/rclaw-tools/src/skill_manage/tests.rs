use super::*;
use crate::Tool;

fn write_skill(skills: &Path, rel: &str, content: &str) {
    let p = skills.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn fg(skills: PathBuf) -> SkillManageTool {
    SkillManageTool::new(skills)
}
fn bg(skills: PathBuf) -> (SkillManageTool, ReviewReadTracker) {
    let usage = SkillUsageStore::new(skills.clone());
    for entry in std::fs::read_dir(&skills).into_iter().flatten().flatten() {
        if entry.path().is_dir() && entry.path().join("SKILL.md").is_file() {
            usage
                .record_created(
                    entry.file_name().to_string_lossy().as_ref(),
                    SkillOwner::Agent,
                )
                .unwrap();
        }
    }
    let t: ReviewReadTracker = Arc::new(Mutex::new(HashMap::new()));
    (SkillManageTool::for_background_review(skills, t.clone()), t)
}

#[tokio::test]
async fn background_refuses_user_owned_skill_until_adopted() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "local/SKILL.md", "# Local\n\nbody\n");
    let tracker: ReviewReadTracker = Arc::new(Mutex::new(HashMap::new()));
    let read = SkillReadTool::new(skills.clone(), tracker.clone());
    let tool = SkillManageTool::for_background_review(skills.clone(), tracker);
    read.execute(json!({"path":"local/SKILL.md"})).await;
    let refused = tool
        .execute(json!({
            "action":"patch", "name":"local", "old_string":"body", "new_string":"changed"
        }))
        .await;
    assert!(refused.is_error);
    assert!(
        refused.content.contains("user-owned"),
        "{}",
        refused.content
    );
    assert!(std::fs::read_to_string(skills.join("local/SKILL.md"))
        .unwrap()
        .contains("body"));
}

#[tokio::test]
async fn background_refuses_to_overwrite_content_changed_after_read() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "learned/SKILL.md", "# Learned\n\noriginal\n");
    let (tool, tracker) = bg(skills.clone());
    let read = SkillReadTool::new(skills.clone(), tracker);
    let read_result = read.execute(json!({"path":"learned/SKILL.md"})).await;
    assert!(!read_result.is_error, "{}", read_result.content);

    write_skill(
        &skills,
        "learned/SKILL.md",
        "# Learned\n\nnew foreground content\n",
    );
    let refused = tool
        .execute(json!({
            "action":"edit",
            "name":"learned",
            "content":"# Learned\n\nstale background rewrite\n"
        }))
        .await;
    assert!(refused.is_error);
    assert!(refused.content.contains("changed after it was read"));
    assert_eq!(
        std::fs::read_to_string(skills.join("learned/SKILL.md")).unwrap(),
        "# Learned\n\nnew foreground content\n"
    );
}

#[tokio::test]
async fn background_create_cannot_replace_known_repository_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tracker: ReviewReadTracker = Arc::new(Mutex::new(HashMap::new()));
    let tool = SkillManageTool::for_background_review(skills.clone(), tracker);

    let refused = tool
        .execute(json!({
            "action":"create",
            "name":"glmv-stock-analyst",
            "content":"# Replacement\n"
        }))
        .await;
    assert!(refused.is_error);
    assert!(
        refused.content.contains("repository-owned"),
        "{}",
        refused.content
    );
    assert!(!skills.join("glmv-stock-analyst").exists());

    let created = tool
        .execute(json!({
            "action":"create",
            "name":"new-agent-skill",
            "content":"# New Agent Skill\n"
        }))
        .await;
    assert!(!created.is_error, "{}", created.content);
    assert_eq!(
        SkillUsageStore::new(skills)
            .owner("new-agent-skill")
            .unwrap(),
        SkillOwner::Agent
    );
}

#[tokio::test]
async fn create_patch_edit_delete_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());

    // create
    let r = tool
        .execute(json!({
            "action": "create",
            "name": "my-skill",
            "content": "# My Skill\n\nStep 1: do thing.\n"
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(skills.join("my-skill/SKILL.md").exists());

    // patch 唯一命中
    let r = tool
        .execute(json!({
            "action": "patch",
            "name": "my-skill",
            "old_string": "Step 1: do thing.",
            "new_string": "Step 1: do the right thing."
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);
    let after = std::fs::read_to_string(skills.join("my-skill/SKILL.md")).unwrap();
    assert!(after.contains("the right thing"));

    // edit 全量重写
    let r = tool
        .execute(json!({
            "action": "edit",
            "name": "my-skill",
            "content": "# My Skill v2\n\nBrand new body.\n"
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(std::fs::read_to_string(skills.join("my-skill/SKILL.md"))
        .unwrap()
        .contains("v2"));

    // delete
    let r = tool
        .execute(json!({"action":"delete","name":"my-skill"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(!skills.join("my-skill").exists());
}

#[tokio::test]
async fn recreating_repository_skill_preserves_repository_owner() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());
    let created = tool
        .execute(json!({
            "action":"create",
            "name":"glmv-stock-analyst",
            "content":"# Bundled\n\nRestored.\n"
        }))
        .await;
    assert!(!created.is_error, "{}", created.content);
    assert_eq!(
        SkillUsageStore::new(skills)
            .owner("glmv-stock-analyst")
            .unwrap(),
        SkillOwner::Repository
    );
}

#[tokio::test]
async fn write_file_and_remove_file_respect_subdir_allowlist() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());
    tool.execute(json!({"action":"create","name":"s","content":"# S\n\n"}))
        .await;

    // 合法支撑文件
    let r = tool
        .execute(json!({
            "action": "write_file",
            "name": "s",
            "file_path": "references/guide.md",
            "file_content": "guide body"
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(skills.join("s/references/guide.md").exists());

    // 非法子目录
    let bad = tool
        .execute(json!({
            "action": "write_file",
            "name": "s",
            "file_path": "evil/x.md",
            "file_content": "x"
        }))
        .await;
    assert!(bad.is_error);
    assert!(bad.content.contains("subdirectory"));

    // remove_file
    let r = tool
        .execute(json!({"action":"remove_file","name":"s","file_path":"references/guide.md"}))
        .await;
    assert!(!r.is_error, "{}", r.content);
    assert!(!skills.join("s/references/guide.md").exists());

    // remove_file 拒删 SKILL.md
    let guard = tool
        .execute(json!({"action":"remove_file","name":"s","file_path":"SKILL.md"}))
        .await;
    assert!(guard.is_error);
}

#[tokio::test]
async fn write_file_allows_only_skill_contract_files_at_root() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());
    tool.execute(json!({"action":"create","name":"s","content":"# S\n\n"}))
        .await;

    // 其它裸文件名必须被拒：支撑文件须落在允许的子目录，技能根只保留规范文件。
    let bad = tool
        .execute(json!({
            "action": "write_file",
            "name": "s",
            "file_path": "notes.md",
            "file_content": "x"
        }))
        .await;
    assert!(bad.is_error, "裸根文件名（非 SKILL.md）应被拒");
    assert!(bad.content.contains("subdirectory"));
    assert!(!skills.join("s/notes.md").exists());

    for wrong_case in ["skill.md", "Skill.toml"] {
        let rejected = tool
            .execute(json!({
                "action": "write_file",
                "name": "s",
                "file_path": wrong_case,
                "file_content": "# wrong case"
            }))
            .await;
        assert!(
            rejected.is_error,
            "技能根契约文件的错误大小写必须被拒: {wrong_case}"
        );
    }

    // SKILL.md 在技能根仍然允许（兼容既有用法）。
    let ok = tool
        .execute(json!({
            "action": "write_file",
            "name": "s",
            "file_path": "SKILL.md",
            "file_content": "# S\n\nbody"
        }))
        .await;
    assert!(!ok.is_error, "SKILL.md 在技能根应允许: {}", ok.content);

    std::fs::create_dir_all(skills.join("s/scripts")).unwrap();
    std::fs::write(skills.join("s/scripts/run.py"), "print('ok')\n").unwrap();
    let manifest = tool
        .execute(json!({
            "action": "write_file",
            "name": "s",
            "file_path": "skill.toml",
            "file_content": "manifest_version = 1\n[skill]\nname = \"s\"\n[[entrypoints]]\nname = \"run\"\ndescription = \"运行测试入口\"\nruntime = \"python\"\nscript = \"scripts/run.py\"\n"
        }))
        .await;
    assert!(
        !manifest.is_error,
        "skill.toml 在技能根应允许: {}",
        manifest.content
    );
    assert!(skills.join("s/skill.toml").is_file());

    let invalid = tool
        .execute(json!({
            "action": "write_file",
            "name": "s",
            "file_path": "skill.toml",
            "file_content": "manifest_version = 99"
        }))
        .await;
    assert!(invalid.is_error, "无效 Manifest 不得覆盖原文件");

    let protected = tool
        .execute(json!({"action":"remove_file","name":"s","file_path":"skill.toml"}))
        .await;
    assert!(protected.is_error, "Manifest 不得脱离 Skill 单独删除");

    let referenced_script = tool
        .execute(json!({"action":"remove_file","name":"s","file_path":"scripts/run.py"}))
        .await;
    assert!(
        referenced_script.is_error,
        "Manifest 仍引用的入口脚本不得单独删除"
    );
    assert!(skills.join("s/scripts/run.py").is_file());
}

#[tokio::test]
async fn remove_file_fails_closed_when_manifest_artifact_is_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    write_skill(&skills, "s/SKILL.md", "# S\n\n");
    write_skill(&skills, "s/scripts/helper.py", "print('ok')\n");
    std::fs::create_dir(skills.join("s/skill.toml")).unwrap();
    let tool = fg(skills.clone());

    let result = tool
        .execute(json!({"action":"remove_file","name":"s","file_path":"scripts/helper.py"}))
        .await;
    assert!(result.is_error, "异常 Manifest 存在时必须拒绝删除支撑文件");
    assert!(skills.join("s/scripts/helper.py").is_file());
}

#[tokio::test]
async fn patch_rejects_ambiguous_and_no_match_and_replace_all() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "d/SKILL.md", "# D\n\ndup\ndup\n");
    let tool = fg(skills.clone());

    let amb = tool
        .execute(json!({"action":"patch","name":"d","old_string":"dup","new_string":"x"}))
        .await;
    assert!(amb.is_error);
    assert!(amb.content.contains("matched 2"));

    let all = tool
        .execute(json!({
            "action":"patch","name":"d","old_string":"dup","new_string":"y","replace_all":true
        }))
        .await;
    assert!(!all.is_error, "{}", all.content);
    let after = std::fs::read_to_string(skills.join("d/SKILL.md")).unwrap();
    assert_eq!(after.matches("y").count(), 2);
}

#[tokio::test]
async fn protected_skill_blocks_bg_writes_but_allows_fg_patch() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "prot/SKILL.md", "# P\n\nbody\n");
    // 标记为 .protected（神圣不可碰：后台一切写皆拒）
    std::fs::write(skills.join("prot/.protected"), "").unwrap();

    // 前台可 patch（protected 仅挡 delete）
    let fg_tool = fg(skills.clone());
    let r = fg_tool
        .execute(json!({
            "action":"patch","name":"prot","old_string":"body","new_string":"new body"
        }))
        .await;
    assert!(!r.is_error, "{}", r.content);

    // 前台 delete 受保护 → 拒
    let del = fg_tool
        .execute(json!({"action":"delete","name":"prot"}))
        .await;
    assert!(del.is_error);
    assert!(del.content.contains("protected"));

    // 后台 review 一切写皆拒（guard 在 read-before-write 之前生效，无需 seed tracker）
    let (bg_tool, _t) = bg(skills.clone());
    let bg_patch = bg_tool
        .execute(json!({
            "action":"patch","name":"prot","old_string":"new body","new_string":"hack"
        }))
        .await;
    assert!(bg_patch.is_error);
    assert!(bg_patch.content.contains("protected skill"));
}

/// P2 拆分语义：`.pinned` 前台仅挡 delete；后台 review 是无人值守自主维护，
/// 对 pinned 技能一切写入都拒。
#[tokio::test]
async fn pinned_skill_blocks_bg_writes_but_allows_fg_patch() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "pinned/SKILL.md", "# P\n\nbody\n");
    std::fs::write(skills.join("pinned/.pinned"), "").unwrap();

    // 前台 patch 放行（pinned 仅挡 delete）。
    let fg_tool = fg(skills.clone());
    let fg_patch = fg_tool
        .execute(json!({
            "action":"patch","name":"pinned","old_string":"body","new_string":"fg body"
        }))
        .await;
    assert!(!fg_patch.is_error, "{}", fg_patch.content);

    // 前台 delete 被拒（pinned 挡 delete）。
    let fg_del = fg_tool
        .execute(json!({"action":"delete","name":"pinned"}))
        .await;
    assert!(fg_del.is_error);
    assert!(fg_del.content.contains("pinned"));

    // 后台 review：即使先 read，patch 仍拒（pinned 不允许无人值守自主修改）。
    let (bg_tool, tracker) = bg(skills.clone());
    let read_tool = SkillReadTool::new(skills.clone(), tracker.clone());
    let _ = read_tool.execute(json!({"path":"pinned/SKILL.md"})).await;
    let bg_patch = bg_tool
        .execute(json!({
            "action":"patch","name":"pinned","old_string":"fg body","new_string":"bg body"
        }))
        .await;
    assert!(bg_patch.is_error);
    assert!(bg_patch.content.contains("pinned skill"));
    let after = std::fs::read_to_string(skills.join("pinned/SKILL.md")).unwrap();
    assert!(after.contains("fg body"));
    assert!(!after.contains("bg body"));

    // 后台 review delete 被拒——即使声明
    // absorbed_into 也不行：guard_protected 在 guard_background_delete 之前短路。
    let bg_del = bg_tool
        .execute(json!({"action":"delete","name":"pinned","absorbed_into":"umbrella"}))
        .await;
    assert!(bg_del.is_error);
    assert!(bg_del.content.contains("pinned"));
    assert!(
        skills.join("pinned/SKILL.md").exists(),
        "pinned 不得被后台归档"
    );
}

#[tokio::test]
async fn background_review_requires_read_before_write() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "s/SKILL.md", "# S\n\nstep.\n");
    let (tool, tracker) = bg(skills.clone());

    // 未读先 patch → 拒（即便 old_string 正确）
    let refused = tool
        .execute(json!({
            "action":"patch","name":"s","old_string":"step.","new_string":"new step."
        }))
        .await;
    assert!(refused.is_error);
    assert!(refused.content.contains("has not been loaded"));

    // 模拟 review 读工具读过该文件
    let read_tool = SkillReadTool::new(skills.clone(), tracker.clone());
    let r = read_tool.execute(json!({"path":"s/SKILL.md"})).await;
    assert!(!r.is_error, "{}", r.content);

    // 再 patch → 放行
    let ok = tool
        .execute(json!({
            "action":"patch","name":"s","old_string":"step.","new_string":"new step."
        }))
        .await;
    assert!(!ok.is_error, "{}", ok.content);
    let after = std::fs::read_to_string(skills.join("s/SKILL.md")).unwrap();
    assert!(after.contains("new step"));
}

#[tokio::test]
async fn background_review_refuses_bare_delete_and_archives_consolidation() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "umbrella/SKILL.md", "# Umbrella\n\nu\n");
    write_skill(&skills, "dup/SKILL.md", "# Dup\n\nd\n");
    let (tool, tracker) = bg(skills.clone());

    // 裸删（无 absorbed_into）→ fail-closed 拒绝，技能仍存活。
    let refused = tool.execute(json!({"action":"delete","name":"dup"})).await;
    assert!(refused.is_error);
    assert!(refused.content.contains("absorbed_into"));
    assert!(skills.join("dup/SKILL.md").exists(), "fail-closed 不得动盘");

    // 声明 absorbed_into 但伞不存在 → 拒。
    let no_umbrella = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"ghost"}))
        .await;
    assert!(no_umbrella.is_error);
    assert!(no_umbrella.content.contains("does not exist"));

    // absorbed_into 指向自己 → 拒。
    let self_ref = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"dup"}))
        .await;
    assert!(self_ref.is_error);

    write_skill(&skills, "user-umbrella/SKILL.md", "# User Umbrella\n\nu\n");
    let wrong_owner = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"user-umbrella"}))
        .await;
    assert!(wrong_owner.is_error);
    assert!(wrong_owner
        .content
        .contains("agent-owned, unpinned, and unprotected"));

    std::fs::write(skills.join("umbrella/.protected"), "").unwrap();
    let protected_umbrella = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"umbrella"}))
        .await;
    assert!(protected_umbrella.is_error);
    assert!(protected_umbrella.content.contains("unprotected"));
    std::fs::remove_file(skills.join("umbrella/.protected")).unwrap();

    // 目标存在但源与伞都未在当前 review 读取 → 仍拒绝盲归档。
    let unread = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"umbrella"}))
        .await;
    assert!(unread.is_error);
    assert!(unread.content.contains("has not been loaded"));

    let read = SkillReadTool::new(skills.clone(), tracker);
    assert!(!read.execute(json!({"path":"dup/SKILL.md"})).await.is_error);
    assert!(
        !read
            .execute(json!({"path":"umbrella/SKILL.md"}))
            .await
            .is_error
    );

    // verified consolidation：伞存在 → 归档（可恢复），不永久删。
    let ok = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"umbrella"}))
        .await;
    assert!(!ok.is_error, "{}", ok.content);
    let data: serde_json::Value = serde_json::from_str(&ok.content).unwrap();
    assert_eq!(data["archived"], true);
    // 原目录已移走，伞仍在。
    assert!(!skills.join("dup").exists());
    assert!(skills.join("umbrella/SKILL.md").exists());
    // 归档落到 .archive/ 下，且只有一份。
    let archive = skills.join(".archive");
    let archived: Vec<_> = std::fs::read_dir(&archive)
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(archived.len(), 1);
    assert!(archived[0].path().to_string_lossy().contains("dup-"));
}

#[tokio::test]
async fn foreground_delete_is_permanent_without_absorbed_into() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "fg/SKILL.md", "# Fg\n\nf\n");
    let tool = fg(skills.clone());
    // 前台裸删 → 直接永久删除，无需 absorbed_into，也不归档（保留既有硬删语义）。
    let r = tool.execute(json!({"action":"delete","name":"fg"})).await;
    assert!(!r.is_error, "{}", r.content);
    assert!(!skills.join("fg").exists());
    assert!(!skills.join(".archive").exists());
}

/// skill_manage 路径沙箱必须解析已落地符号链接：skills/<name> -> 外部 时，
/// under_skills（经 resolve_within_base）与 SkillReadTool 都必须拒绝（P1 路径沙箱）。
#[cfg(unix)]
#[tokio::test]
async fn sandbox_rejects_symlinked_skill_dir() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "# Stolen\n\n").unwrap();
    // skills/link -> <outside>（指向 skills 之外）。
    symlink(&outside, skills.join("link")).unwrap();

    let tracker: ReviewReadTracker = Arc::new(Mutex::new(HashMap::new()));
    let read_tool = SkillReadTool::new(skills.clone(), tracker);
    // 经逃逸链接读 SKILL.md → 拒。
    let r = read_tool.execute(json!({"path":"link/SKILL.md"})).await;
    assert!(r.is_error, "经逃逸符号链接的读必须被拒绝");

    // 经逃逸链接 patch → under_skills 拒绝。
    let tool = fg(skills.clone());
    let p = tool
        .execute(json!({"action":"patch","name":"link","old_string":"Stolen","new_string":"x"}))
        .await;
    assert!(p.is_error);
}

#[cfg(unix)]
#[tokio::test]
async fn background_rejects_skill_alias_even_when_target_is_inside_skills() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "real/SKILL.md", "# Real\n\nbody\n");
    symlink(skills.join("real"), skills.join("alias")).unwrap();
    let usage = SkillUsageStore::new(skills.clone());
    usage.record_created("alias", SkillOwner::Agent).unwrap();
    let tracker: ReviewReadTracker = Arc::new(Mutex::new(HashMap::new()));
    let read = SkillReadTool::new(skills.clone(), tracker.clone());
    let read_result = read.execute(json!({"path":"alias/SKILL.md"})).await;
    assert!(read_result.is_error);
    assert!(read_result.content.contains("redirected"));

    let manage = SkillManageTool::for_background_review(skills.clone(), tracker);
    let patch = manage
        .execute(json!({
            "action":"patch",
            "name":"alias",
            "old_string":"body",
            "new_string":"changed"
        }))
        .await;
    assert!(patch.is_error);
    assert!(patch.content.contains("symlinks/junctions"));
    assert!(std::fs::read_to_string(skills.join("real/SKILL.md"))
        .unwrap()
        .contains("body"));

    let listed = SkillListTool::new(skills).execute(json!({})).await;
    assert!(!listed.content.contains("\"name\":\"alias\""));
}

#[tokio::test]
async fn skills_list_returns_addressable_skills_only() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    // 标题行紧接描述行（无空行），parse_one 才会把描述解析出来。
    write_skill(&skills, "alpha/SKILL.md", "# Alpha\nfirst skill here.\n");
    write_skill(&skills, "beta/SKILL.md", "# Beta\nsecond skill here.\n");
    // 包内嵌套子技能（<pkg>/skills/<sub>/SKILL.md）按内层名不可被 skill_manage 寻址 → 不应出现。
    write_skill(&skills, "watch-positions/skills/close-recap/SKILL.md", "# Close Recap\nnested here.\n");
    // loose .md 不带可寻址 dir 名 → 不应出现。
    std::fs::write(skills.join("loose.md"), "# Loose\n\n").unwrap();
    // .archive 目录里的归档技能也不应出现（build_summaries 不枚举 .archive）。
    std::fs::create_dir_all(skills.join(".archive/old-1")).unwrap();
    std::fs::write(skills.join(".archive/old-1/SKILL.md"), "# Old\n\n").unwrap();

    let tool = SkillListTool::new(skills.clone());
    let r = tool.execute(json!({})).await;
    assert!(!r.is_error, "{}", r.content);
    let data: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    let arr = data["skills"].as_array().unwrap();
    let names: Vec<String> = arr
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["alpha".to_string(), "beta".to_string()]);
    // title/description 被解析出来；name 可直接喂给 skill_manage。
    let alpha = &arr[0];
    assert_eq!(alpha["title"], "Alpha");
    assert!(alpha["description"]
        .as_str()
        .unwrap()
        .contains("first skill"));
}

/// skills_list 不得列入经符号链接逃逸到 skills 根外的技能（与 read/patch 沙箱一致）。
#[cfg(unix)]
#[tokio::test]
async fn skills_list_excludes_symlinked_skill() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "real/SKILL.md", "# Real\n\nr\n");
    // skills/link -> 外部目录（带 SKILL.md）→ 经符号链接逃逸。
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("SKILL.md"), "# Stolen\n\n").unwrap();
    symlink(&outside, skills.join("link")).unwrap();

    let tool = SkillListTool::new(skills.clone());
    let r = tool.execute(json!({})).await;
    let data: serde_json::Value = serde_json::from_str(&r.content).unwrap();
    let names: Vec<String> = data["skills"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, vec!["real".to_string()]);
    assert!(
        !names.contains(&"link".to_string()),
        "逃逸符号链接技能不得列入"
    );
}

/// 归档目标必须走 under_skills 同一套沙箱：若 `.archive` 预置为指向 skills 根外的
/// symlink/junction，归档应被拒、技能仍存活（P2 archive 沙箱）。
#[cfg(unix)]
#[tokio::test]
async fn archive_refuses_symlinked_archive_root() {
    use std::os::unix::fs::symlink;
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "umbrella/SKILL.md", "# Umbrella\n\nu\n");
    write_skill(&skills, "dup/SKILL.md", "# Dup\n\nd\n");
    // 预置一个指向 skills 根外的 .archive 符号链接。
    let outside = dir.path().join("outside-archive");
    std::fs::create_dir_all(&outside).unwrap();
    symlink(&outside, skills.join(".archive")).unwrap();

    let (tool, tracker) = bg(skills.clone());
    let read = SkillReadTool::new(skills.clone(), tracker);
    assert!(!read.execute(json!({"path":"dup/SKILL.md"})).await.is_error);
    assert!(
        !read
            .execute(json!({"path":"umbrella/SKILL.md"}))
            .await
            .is_error
    );
    // 即使声明 absorbed_into，归档也应因 .archive 逃逸而被拒；技能仍存活。
    let r = tool
        .execute(json!({"action":"delete","name":"dup","absorbed_into":"umbrella"}))
        .await;
    assert!(r.is_error, "逃逸的 .archive 必须拒绝归档");
    assert!(r.content.contains("outside skills_dir"));
    assert!(skills.join("dup/SKILL.md").exists(), "归档被拒时不得动盘");
}

#[tokio::test]
async fn traversal_and_structure_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());

    // 名字含 ..
    let esc = tool
        .execute(json!({
            "action":"patch","name":"../escape","old_string":"a","new_string":"b"
        }))
        .await;
    assert!(esc.is_error);

    // 非法名
    let bad_name = tool
        .execute(json!({"action":"create","name":"Bad Name","content":"# x\n"}))
        .await;
    assert!(bad_name.is_error);

    // create 内容破坏结构
    write_skill(&skills, "b/SKILL.md", "# B\n\nbody\n");
    let broken = tool
        .execute(json!({
            "action":"patch","name":"b","old_string":"# B\n\nbody","new_string":"no heading"
        }))
        .await;
    assert!(broken.is_error);
    assert!(broken.content.contains("SKILL.md structure"));
}

#[tokio::test]
async fn create_rejects_invisible_unicode_and_redacts_secrets() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());

    // 隐形字符（零宽空格）→ 拒绝。
    let invisible = tool
        .execute(json!({
            "action":"create","name":"evil",
            "content":"# Evil\u{200b}\n\nbody with hidden mark\n"
        }))
        .await;
    assert!(invisible.is_error, "隐形字符必须拒绝");
    assert!(
        invisible.content.contains("security gate"),
        "{}",
        invisible.content
    );
    assert!(!skills.join("evil").exists(), "拒绝时不得留下半成品目录");

    // 硬编码密钥 → 自动脱敏后放行（B2：先脱敏再落盘）。
    let with_key = tool
        .execute(json!({
            "action":"create","name":"with-key",
            "content":"# With Key\n\nexport OPENAI_API_KEY=sk-proj-abcdefghij0123456789abcdefghij0123456789\n"
        }))
        .await;
    assert!(!with_key.is_error, "{}", with_key.content);
    let on_disk = std::fs::read_to_string(skills.join("with-key").join("SKILL.md")).unwrap();
    assert!(
        !on_disk.contains("sk-proj-abcdefghij"),
        "密钥必须已脱敏: {on_disk}"
    );
}

#[tokio::test]
async fn ledger_records_and_rollbacks_changes() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "demo/SKILL.md", "# Demo\n\noriginal body\n");
    let tool = fg(skills.clone());

    // patch 一次。
    let patched = tool
        .execute(json!({
            "action":"patch","name":"demo",
            "old_string":"original body","new_string":"patched body"
        }))
        .await;
    assert!(!patched.is_error, "{}", patched.content);
    let on_disk = std::fs::read_to_string(skills.join("demo").join("SKILL.md")).unwrap();
    assert!(on_disk.contains("patched body"));

    // 找到台账条目 id。
    let ledger_raw = std::fs::read_to_string(skills.join(".ledger").join("ledger.jsonl")).unwrap();
    let id = ledger_raw
        .lines()
        .next()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .expect("台账应有记录");

    // 回滚 → 恢复 before 内容。
    let rolled = tool.execute(json!({"action":"rollback","id": id})).await;
    assert!(!rolled.is_error, "{}", rolled.content);
    let restored = std::fs::read_to_string(skills.join("demo").join("SKILL.md")).unwrap();
    assert!(
        restored.contains("original body"),
        "回滚应恢复 before 内容: {restored}"
    );
    assert!(!restored.contains("patched body"));

    // 台账再读 → 末尾应有 rollback 留痕；对 rollback 条目再回滚被拒。
    let ledger_after =
        std::fs::read_to_string(skills.join(".ledger").join("ledger.jsonl")).unwrap();
    assert!(ledger_after.contains("\"rollback\""));
    let nested = serde_json::Value::String(ledger_after.clone());
    let last_id = nested
        .as_str()
        .unwrap()
        .lines()
        .last()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .unwrap();
    let again = tool
        .execute(json!({"action":"rollback","id": last_id}))
        .await;
    assert!(again.is_error, "rollback 条目不可再回滚");
}

#[tokio::test]
async fn ledger_create_rollback_removes_file() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    let tool = fg(skills.clone());
    let created = tool
        .execute(json!({"action":"create","name":"fresh","content":"# Fresh\n\nnew\n"}))
        .await;
    assert!(!created.is_error, "{}", created.content);
    assert!(skills.join("fresh").join("SKILL.md").is_file());

    let ledger_raw = std::fs::read_to_string(skills.join(".ledger").join("ledger.jsonl")).unwrap();
    let id = ledger_raw
        .lines()
        .next()
        .and_then(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .and_then(|v| v.get("id").and_then(|i| i.as_str()).map(str::to_string))
        .unwrap();

    let rolled = tool.execute(json!({"action":"rollback","id": id})).await;
    assert!(!rolled.is_error, "{}", rolled.content);
    assert!(
        !skills.join("fresh").join("SKILL.md").exists(),
        "create 的回滚 = 删除新文件"
    );
}

/// 经 skill_manage execute 走 archive → restore 全程必须在有限时间内完成。
/// （曾因 execute 全程持有 lifecycle 锁、do_restore 再次 acquire 同一把
/// std Mutex 而死锁——面板开关与 agent 的 restore 永久挂起。）
#[tokio::test]
async fn archive_then_restore_via_execute_does_not_deadlock() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    std::fs::create_dir_all(&skills).unwrap();
    write_skill(&skills, "demo/SKILL.md", "# Demo\n\nbody\n");
    let tool = fg(skills.clone());

    let archived = tool.execute(json!({"action":"archive","name":"demo"})).await;
    assert!(!archived.is_error, "{}", archived.content);
    assert!(!skills.join("demo").exists(), "归档后目录应已移出 skills 根");

    let restored = tool.execute(json!({"action":"restore","name":"demo"})).await;
    assert!(!restored.is_error, "{}", restored.content);
    assert!(
        skills.join("demo").join("SKILL.md").is_file(),
        "恢复后 SKILL.md 应回到 skills/<name>/"
    );
}
