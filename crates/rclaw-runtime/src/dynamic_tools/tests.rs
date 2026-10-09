use super::*;
use serde_json::json;

fn layout_in(dir: &std::path::Path) -> Layout {
    let layout = Layout::new(dir.join("agent"));
    rclaw_store::fs::init_layout(&layout).unwrap();
    layout
}

fn static_schemas() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "shell".to_string(),
        description: "执行普通命令".to_string(),
        input_schema: json!({"type": "object"}),
    }]
}

/// 写入一个带 Manifest 入口 `analyze` 的 skill（runtime=python，脚本 scripts/analyze.py），
/// 返回 SKILL.md 路径。生成的专用工具名为 `skill__<snake(dir)>__analyze`。
fn write_manifest_skill(layout: &Layout, dir_name: &str) -> PathBuf {
    let skill_dir = layout.skills_dir.join(dir_name);
    std::fs::create_dir_all(skill_dir.join("scripts")).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {dir_name}\ndescription: 测试专用入口。\n---\n"),
    )
    .unwrap();
    std::fs::write(skill_dir.join("scripts/analyze.py"), "print('ok')\n").unwrap();
    std::fs::write(
        skill_dir.join("skill.toml"),
        format!(
            r#"
manifest_version = 1
[skill]
name = "{dir_name}"

[[entrypoints]]
name = "analyze"
description = "分析测试输入"
runtime = "python"
script = "scripts/analyze.py"
"#,
        ),
    )
    .unwrap();
    skill_dir.join("SKILL.md")
}

fn read_call(path: &str) -> ToolCall {
    ToolCall {
        id: "call_1".to_string(),
        name: "read".to_string(),
        input: json!({"path": path}).to_string(),
    }
}

#[test]
fn activate_registers_schemas_tools_and_active_state() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    let activated = set.activate(&layout.base, &skill_md).unwrap();

    assert_eq!(activated, vec!["skill__demo_skill__analyze"]);
    let schemas = set.active_schemas();
    assert_eq!(schemas.len(), 1);
    assert_eq!(schemas[0].name, "skill__demo_skill__analyze");
    assert!(set.get("skill__demo_skill__analyze").is_some());
    assert!(set.contains("skill__demo_skill__analyze"));
    assert_eq!(
        set.activation_state(&skill_md),
        Some(ActivationState::Active)
    );
    assert!(set
        .active_names()
        .any(|name| name == "skill__demo_skill__analyze"));
}

#[test]
fn repeated_activation_of_same_definitions_is_noop() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    set.activate(&layout.base, &skill_md).unwrap();
    let repeated = set.activate(&layout.base, &skill_md).unwrap();

    assert!(repeated.is_empty());
    assert_eq!(set.active_schemas().len(), 1);
    assert_eq!(
        set.activation_state(&skill_md),
        Some(ActivationState::Active)
    );
}

#[test]
fn conflicting_redefinition_invalidates_all_then_allows_reactivation() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    set.activate(&layout.base, &skill_md).unwrap();

    // 脚本内容变化 → 定义漂移，重读视为冲突，全部动态工具失效。
    std::fs::write(
        layout
            .skills_dir
            .join("demo-skill")
            .join("scripts/analyze.py"),
        "print('changed')\n",
    )
    .unwrap();
    let error = set.activate(&layout.base, &skill_md).unwrap_err();
    assert!(error.contains("定义发生冲突"));
    assert!(set.active_schemas().is_empty());
    assert!(!set.contains("skill__demo_skill__analyze"));
    assert_eq!(
        set.activation_state(&skill_md),
        Some(ActivationState::Invalidated)
    );

    // 重新读取（脚本恢复原定义）后可再次激活。
    std::fs::write(
        layout
            .skills_dir
            .join("demo-skill")
            .join("scripts/analyze.py"),
        "print('ok')\n",
    )
    .unwrap();
    let reactivated = set.activate(&layout.base, &skill_md).unwrap();
    assert_eq!(reactivated, vec!["skill__demo_skill__analyze"]);
    assert_eq!(
        set.activation_state(&skill_md),
        Some(ActivationState::Active)
    );
}

#[test]
fn dynamic_name_conflicting_with_static_tool_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut statics = static_schemas();
    statics.push(ToolSchema {
        name: "skill__demo_skill__analyze".to_string(),
        description: "静态工具".to_string(),
        input_schema: json!({"type": "object"}),
    });
    let mut set = DynamicToolSet::new(&statics);

    let error = set.activate(&layout.base, &skill_md).unwrap_err();
    assert!(error.contains("专用工具名与现有工具冲突"));
    assert!(set.active_schemas().is_empty());
    // 首次激活即在冲突检查处失败：该 skill 本 turn 从未成功激活，状态为 None。
    assert_eq!(set.activation_state(&skill_md), None);
}

#[test]
fn missing_manifest_on_reread_invalidates_only_that_skill() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let demo_md = write_manifest_skill(&layout, "demo-skill");
    let beta_md = write_manifest_skill(&layout, "beta-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    set.activate(&layout.base, &demo_md).unwrap();
    set.activate(&layout.base, &beta_md).unwrap();
    assert_eq!(set.active_schemas().len(), 2);

    std::fs::remove_file(layout.skills_dir.join("demo-skill").join("skill.toml")).unwrap();
    let error = set.activate(&layout.base, &demo_md).unwrap_err();
    assert!(error.contains("skill.toml 已不存在"));

    assert!(!set.contains("skill__demo_skill__analyze"));
    assert!(set.contains("skill__beta_skill__analyze"));
    assert_eq!(
        set.activation_state(&demo_md),
        Some(ActivationState::Invalidated)
    );
    assert_eq!(
        set.activation_state(&beta_md),
        Some(ActivationState::Active)
    );
}

#[test]
fn load_error_invalidates_all_dynamic_tools() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    set.activate(&layout.base, &skill_md).unwrap();

    std::fs::write(
        layout.skills_dir.join("demo-skill").join("skill.toml"),
        "not a toml [[[",
    )
    .unwrap();
    let error = set.activate(&layout.base, &skill_md).unwrap_err();
    assert!(error.contains("已全部失效"));
    assert!(set.active_schemas().is_empty());
    assert_eq!(
        set.activation_state(&skill_md),
        Some(ActivationState::Invalidated)
    );
}

#[test]
fn invalidate_all_reports_whether_anything_was_active() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    assert!(!set.invalidate_all());
    set.activate(&layout.base, &skill_md).unwrap();
    assert!(set.invalidate_all());
    assert!(!set.invalidate_all());
    assert_eq!(
        set.activation_state(&skill_md),
        Some(ActivationState::Invalidated)
    );
}

#[test]
fn legacy_shell_error_matches_skill_scripts_only() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    set.activate(&layout.base, &skill_md).unwrap();

    let error = set
        .legacy_shell_error("python skills/wrong-folder/scripts/analyze.py")
        .unwrap();
    assert!(error.contains("SKILL_ENTRYPOINT_REQUIRED"));
    assert!(error.contains("skill__demo_skill__analyze"));
    assert!(set
        .legacy_shell_error("rg analyze.py skills/demo-skill")
        .is_none());
    // 未激活任何 Manifest 工具时，不拦普通 shell 命令。
    set.invalidate_all();
    assert!(set
        .legacy_shell_error("python skills/wrong-folder/scripts/analyze.py")
        .is_none());
}

#[test]
fn is_media_publisher_requires_trusted_install_path() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let skill_md = write_manifest_skill(&layout, "demo-skill");

    let mut set = DynamicToolSet::new(&static_schemas());
    set.activate(&layout.base, &skill_md).unwrap();

    // 演示 skill 的脚本不在媒体门禁信任清单内，即便 manifest 声明也不提升为信任根。
    assert!(!set.is_media_publisher("skill__demo_skill__analyze", &layout.skills_dir));
    assert!(!set.is_media_publisher("unknown_tool", &layout.skills_dir));
}

#[test]
fn detect_maps_read_calls_to_catalog_entries() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    let alpha = layout.skills_dir.join("alpha-folder");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::write(
        alpha.join("SKILL.md"),
        "---\nname: alpha-skill\ndescription: 分析图片并生成摘要。\n---\n",
    )
    .unwrap();
    std::fs::write(
        layout.skills_dir.join("zeta.md"),
        "# zeta-skill\n\n生成项目报告。\n",
    )
    .unwrap();
    let package = layout.skills_dir.join("ai-berkshire").join("skills");
    std::fs::create_dir_all(&package).unwrap();
    std::fs::write(
        package.join("news-pulse.md"),
        "---\nname: news-pulse\ndescription: 分析公司新闻。\n---\n",
    )
    .unwrap();
    std::fs::create_dir_all(alpha.join("references")).unwrap();
    std::fs::write(alpha.join("references").join("alias.md"), "# 别名\n").unwrap();

    let catalog = SkillCatalog::new(&layout);
    let tool_calls = vec![
        read_call("skills/alpha-folder/SKILL.md"),
        read_call("skills/zeta.md"),
        read_call("skills/ai-berkshire/skills/news-pulse.md"),
        read_call("skills/alpha-folder/SKILL.md"),
        read_call("skills/alpha-folder/references/alias.md"),
        ToolCall {
            id: "call_2".to_string(),
            name: "shell".to_string(),
            input: json!({"command": "cat skills/zeta.md"}).to_string(),
        },
    ];
    let invocations = catalog.detect_skill_invocations(&tool_calls);

    assert_eq!(
        invocations.len(),
        3,
        "重复读取与 references、非 read 调用都不得计入"
    );
    let by_name = |name: &str| {
        invocations
            .iter()
            .find(|inv| inv.name == name)
            .unwrap_or_else(|| panic!("未识别出 {name}"))
    };
    let alpha_inv = by_name("alpha-skill");
    assert!(alpha_inv
        .path
        .ends_with(std::path::Path::new("alpha-folder/SKILL.md")));
    assert_eq!(alpha_inv.package, None);
    assert_eq!(by_name("zeta-skill").package, None);
    let pulse = by_name("news-pulse");
    assert_eq!(pulse.package.as_deref(), Some("ai-berkshire"));
}

#[test]
fn catalog_cache_reuses_scan_until_invalidated() {
    let dir = tempfile::tempdir().unwrap();
    let layout = layout_in(dir.path());
    std::fs::write(
        layout.skills_dir.join("zeta.md"),
        "# zeta-skill\n\n生成项目报告。\n",
    )
    .unwrap();

    let catalog = SkillCatalog::new(&layout);
    assert_eq!(catalog.summaries().len(), 1);
    assert!(catalog.skills_text().contains("zeta-skill"));

    // 缓存期内目录新增文件不可见。
    std::fs::write(
        layout.skills_dir.join("alpha.md"),
        "# alpha-skill\n\n新技能。\n",
    )
    .unwrap();
    assert_eq!(catalog.summaries().len(), 1);
    assert!(!catalog.skills_text().contains("alpha-skill"));

    // 显式失效后重新扫描可见。
    catalog.invalidate();
    assert_eq!(catalog.summaries().len(), 2);
    assert!(catalog.skills_text().contains("alpha-skill"));
}
