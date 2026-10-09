use super::*;
use serde_json::json;

fn static_schemas() -> Vec<ToolSchema> {
    vec![ToolSchema {
        name: "shell".to_string(),
        description: "执行普通命令".to_string(),
        input_schema: json!({"type": "object"}),
    }]
}

#[test]
fn begin_batch_resets_scope_flags_but_keeps_sticky_state() {
    let mut state = TurnState::new(&static_schemas());
    state.skill_manage_succeeded = true;
    state.skill_scope_changed = true;
    state.media_scope_changed = true;
    state.accept_validated_marker("[:napcat-media id=1]");
    state.media_publish_attempted = true;

    state.begin_batch();

    // 批次标志复位。
    assert!(!state.skill_scope_changed);
    assert!(!state.media_scope_changed);
    // 粘性状态保持：skill_manage 成功、已验证 marker、发布尝试均不清空。
    assert!(state.skill_manage_succeeded);
    assert_eq!(state.validated_media_markers.len(), 1);
    assert!(state.media_publish_attempted);
}

#[test]
fn media_scope_change_clears_validated_but_keeps_seen() {
    let mut state = TurnState::new(&static_schemas());
    state.accept_validated_marker("[:napcat-media id=1]");
    state.accept_validated_marker("[:napcat-media id=2]");
    assert_eq!(state.validated_media_markers.len(), 2);

    state.handle_media_scope_change();

    // 旧的交付意图作废，但 seen 必须保留以拒绝旧快照回放。
    assert!(state.validated_media_markers.is_empty());
    assert_eq!(state.seen_media_markers.len(), 2);
    // 已 seen 过的 marker 不得经回放重新进入 validated。
    state.accept_validated_marker("[:napcat-media id=1]");
    assert!(state.validated_media_markers.is_empty());
}

#[test]
fn accept_validated_marker_dedups_via_seen() {
    let mut state = TurnState::new(&static_schemas());
    state.accept_validated_marker("[:weixin-media type=file path=a]");
    state.accept_validated_marker("[:weixin-media type=file path=a]");
    state.accept_validated_marker("[:weixin-media type=file path=b]");

    assert_eq!(state.validated_media_markers.len(), 2);
    assert_eq!(state.seen_media_markers.len(), 2);
}

#[test]
fn new_seeds_dynamic_toolset_with_static_conflict_baseline() {
    let mut statics = static_schemas();
    statics.push(ToolSchema {
        name: "skill__demo__analyze".to_string(),
        description: "静态工具".to_string(),
        input_schema: json!({"type": "object"}),
    });
    let state = TurnState::new(&statics);

    // 冲突底线随构造传入：动态集为空，且静态名在底线内（经 dynamic 的行为间接验证）。
    assert!(state.dynamic.active_schemas().is_empty());
    assert!(state.loaded_package_skills.is_empty());
    assert!(state.announced_skill_paths.is_empty());
    assert!(!state.media_publish_attempted);
    assert!(!state.skill_manage_succeeded);
}
