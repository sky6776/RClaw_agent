//! 可移植化（§7.1）与远端消息的合入检查（§11.3）。
//!
//! 消息正文里会出现本机的绝对路径（agent 根、工具结果全文的 artifact 句柄）。发布前把
//! 它们换成标记，合入时再换成接收端自己的路径：
//!
//! - `<<RCLAW_ARTIFACTS>>/` ← `<agent 根>/artifacts/<会话键>/`（先替换，它比根更具体）；
//! - `<<RCLAW_AGENT_ROOT>>/` ← `<agent 根>/`（正斜杠形态，以及 Windows 反斜杠在 JSON
//!   里转义后的形态）。
//!
//! 正文里本来就有的 `<<RCLAW_` 字样先转义，往返之后原样还原。

use std::path::Path;

use rclaw_core::ContentBlock;

use crate::error::{Result, SyncError};
use crate::ids;
use crate::op::{Artifact, FileRef, WireMsg, ARTIFACT_INLINE_MAX, MESSAGE_DATA_MAX};

pub const MARK_ARTIFACTS: &str = "<<RCLAW_ARTIFACTS>>/";
pub const MARK_ROOT: &str = "<<RCLAW_AGENT_ROOT>>/";
const MARK_HEAD: &str = "<<RCLAW_";
const MARK_HEAD_ESCAPED: &str = "<<RCLAW__";

/// 字符串在 JSON 文本里的写法（不带两端引号）。
fn json_inner(text: &str) -> String {
    let quoted = serde_json::Value::String(text.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

/// 本机 agent 根在消息正文（JSON 文本）里可能出现的写法。
#[derive(Debug, Clone)]
pub struct LocalRoot {
    /// 绝对路径，正斜杠，带尾斜杠，已按 JSON 转义。
    fwd: String,
    /// 反斜杠形态（Windows），不带尾分隔符，已按 JSON 转义。
    backslash: Option<String>,
}

impl LocalRoot {
    /// 相对的根按进程当前目录展开（与 artifact 落盘、`artifacts_dir_prefix` 同一假设）。
    pub fn new(base: &Path) -> LocalRoot {
        let base = if base.is_absolute() {
            base.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(base))
                .unwrap_or_else(|_| base.to_path_buf())
        };
        let native = base.to_string_lossy().into_owned();
        let fwd = format!("{}/", native.replace('\\', "/").trim_end_matches('/'));
        let backslash = native
            .contains('\\')
            .then(|| json_inner(native.trim_end_matches(['\\', '/'])));
        LocalRoot {
            fwd: json_inner(&fwd),
            backslash,
        }
    }
}

/// 本机形态的 `message.data` → 可移植形态。`artifacts_prefix` 是该会话 artifact 目录的
/// 前缀（`SessionStore::artifacts_dir_prefix`）。
pub fn to_portable(data: &str, root: &LocalRoot, artifacts_prefix: &str) -> String {
    let mut out = if data.contains(MARK_HEAD) {
        data.replace(MARK_HEAD, MARK_HEAD_ESCAPED)
    } else {
        data.to_string()
    };
    let artifacts = json_inner(artifacts_prefix);
    if !artifacts.is_empty() && out.contains(&artifacts) {
        out = out.replace(&artifacts, MARK_ARTIFACTS);
    }
    if out.contains(&root.fwd) {
        out = out.replace(&root.fwd, MARK_ROOT);
    }
    if let Some(backslash) = &root.backslash {
        if out.contains(backslash) {
            out = replace_backslash_root(&out, backslash);
        }
    }
    out
}

/// 把反斜杠写法的根换成标记，并把紧跟其后的那段路径里的分隔符改成正斜杠。标记在接收端
/// 展开成它自己的正斜杠根，后半截还留着反斜杠的话，在别的平台上就不是一条路径了；本机
/// 找附件引用（`uploads/`、`output/`）时也认不出来。「那段路径」到第一个空白、引号或者
/// 文件名里不能有的字符为止：宁可少改，不动路径以外的文字。
///
/// `root` 不带尾分隔符；它后面要紧跟一个分隔符才算数（反斜杠，或者拼路径时用的正斜杠），
/// 否则只是名字以它开头的另一个目录。
fn replace_backslash_root(text: &str, root: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(root) {
        let after = &rest[at + root.len()..];
        let Some(tail) = after
            .strip_prefix("\\\\")
            .or_else(|| after.strip_prefix('/'))
        else {
            out.push_str(&rest[..at + root.len()]);
            rest = after;
            continue;
        };
        out.push_str(&rest[..at]);
        out.push_str(MARK_ROOT);
        rest = tail;
        loop {
            let name = rest
                .find(|c: char| {
                    c.is_whitespace() || matches!(c, '\\' | '"' | '<' | '>' | ':' | '|' | '?' | '*')
                })
                .unwrap_or(rest.len());
            out.push_str(&rest[..name]);
            rest = &rest[name..];
            // JSON 文本里一个反斜杠写成两个；单个反斜杠开头的是别的转义（`\"`、`\n`）。
            match rest.strip_prefix("\\\\") {
                Some(after) => {
                    out.push('/');
                    rest = after;
                }
                None => break,
            }
        }
    }
    out.push_str(rest);
    out
}

/// 可移植形态 → 本机形态（接收端自己的根与该会话自己的 artifact 目录）。
pub fn to_local(data: &str, root: &LocalRoot, artifacts_prefix: &str) -> String {
    if !data.contains(MARK_HEAD) {
        return data.to_string();
    }
    data.replace(MARK_ARTIFACTS, &json_inner(artifacts_prefix))
        .replace(MARK_ROOT, &root.fwd)
        .replace(MARK_HEAD_ESCAPED, MARK_HEAD)
}

fn is_artifact_name(name: &str) -> bool {
    (1..=200).contains(&name.len())
        && !name.starts_with('.')
        && rclaw_store::is_portable_file_name(name)
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// 可移植正文里引用到的 artifact 文件名（去重，保序）。
pub fn artifact_names(portable: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = portable;
    while let Some(at) = rest.find(MARK_ARTIFACTS) {
        rest = &rest[at + MARK_ARTIFACTS.len()..];
        let len = rest
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
            .count();
        let name = rest[..len].trim_end_matches('.');
        if is_artifact_name(name) && !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
    }
    names
}

/// 附件引用的路径是否合规：`uploads/`、`media/`、`output/` 之下的相对路径，不含 `..`。
pub fn is_ref_path(path: &str) -> bool {
    let under_known_dir = ["uploads/", "media/", "output/"]
        .iter()
        .any(|dir| path.starts_with(dir));
    under_known_dir
        && path.len() <= 512
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

const ALLOWED_ROLES: &[&str] = &["user", "assistant", "tool"];
const ALLOWED_DATA_KEYS: &[&str] = &["role", "content", "tool_call_id", "ts"];
const ALLOWED_BLOCK_TYPES: &[&str] = &["text", "media_url", "reasoning", "tool_use", "tool_result"];
/// 时间戳上限：2100-01-01。
const TS_MAX: i64 = 4_102_444_800;

/// 检查一段 `message.data`（JSON 文本）的形态：`Message` 的反序列化很宽松（content 可以是
/// 字符串、对象、null），所以对着原始 JSON 值检查，而不是「能反序列化就算过」。
pub fn check_message_data(data: &str, role: &str) -> Result<()> {
    let bad = |what: &str| SyncError::invalid(format!("消息内容不合规: {what}"));
    if data.len() > MESSAGE_DATA_MAX {
        return Err(bad("过大"));
    }
    let value: serde_json::Value = serde_json::from_str(data).map_err(|_| bad("不是 JSON"))?;
    let object = value.as_object().ok_or_else(|| bad("不是对象"))?;
    if !object
        .keys()
        .all(|key| ALLOWED_DATA_KEYS.contains(&key.as_str()))
    {
        return Err(bad("有不认识的字段"));
    }
    if object.get("role").and_then(|v| v.as_str()) != Some(role) {
        return Err(bad("role 不一致"));
    }
    if object.get("tool_call_id").is_some_and(|v| !v.is_string())
        || object.get("ts").is_some_and(|v| !v.is_i64())
    {
        return Err(bad("字段类型不对"));
    }
    let blocks = object
        .get("content")
        .and_then(|v| v.as_array())
        .ok_or_else(|| bad("content 不是数组"))?;
    for block in blocks {
        let kind = block
            .get("type")
            .and_then(|v| v.as_str())
            .ok_or_else(|| bad("内容块缺少 type"))?;
        if !ALLOWED_BLOCK_TYPES.contains(&kind)
            || serde_json::from_value::<ContentBlock>(block.clone()).is_err()
        {
            return Err(bad("内容块不合规"));
        }
    }
    Ok(())
}

/// 线上形态的 `meta` 只有两种：compaction boundary 与分叉告知，里面的 id 都是全局的。
pub fn check_wire_meta(meta: &serde_json::Value) -> Result<()> {
    let bad = || SyncError::invalid("消息的 meta 不合规");
    let object = meta.as_object().filter(|o| o.len() == 1).ok_or_else(bad)?;
    let gid_or_null = |v: Option<&serde_json::Value>| match v {
        None | Some(serde_json::Value::Null) => true,
        Some(v) => v.as_str().is_some_and(ids::is_gid),
    };
    if let Some(compaction) = object.get("compaction").and_then(|v| v.as_object()) {
        let keys_ok = compaction
            .keys()
            .all(|key| matches!(key.as_str(), "head_until" | "tail_from"));
        let tail_ok = compaction
            .get("tail_from")
            .and_then(|v| v.as_str())
            .is_some_and(ids::is_gid);
        if keys_ok && tail_ok && gid_or_null(compaction.get("head_until")) {
            return Ok(());
        }
    } else if let Some(notice) = object.get("fork_notice").and_then(|v| v.as_object()) {
        let keys_ok = notice
            .keys()
            .all(|key| matches!(key.as_str(), "parent" | "anchor"));
        let parent_ok = match notice.get("parent") {
            None | Some(serde_json::Value::Null) => true,
            Some(v) => v.as_str().is_some_and(ids::is_sid),
        };
        if keys_ok && parent_ok && gid_or_null(notice.get("anchor")) {
            return Ok(());
        }
    }
    Err(bad())
}

fn check_ref(file: &FileRef) -> Result<()> {
    if is_ref_path(&file.path) && ids::is_digest(&file.blob) {
        Ok(())
    } else {
        Err(SyncError::invalid("附件引用不合规"))
    }
}

fn check_artifact(artifact: &Artifact, inline_max: usize) -> Result<()> {
    let ok = is_artifact_name(&artifact.name)
        && artifact
            .text
            .as_ref()
            .is_none_or(|text| text.len() <= inline_max)
        && artifact.blob.as_deref().is_none_or(ids::is_digest);
    if ok {
        Ok(())
    } else {
        Err(SyncError::invalid("工具结果全文（artifact）不合规"))
    }
}

/// 合入前对一条远端消息的全部检查（§11.3）。不通过的消息不落库，所属线程从这条起隔离。
pub fn check_wire_msg(msg: &WireMsg) -> Result<()> {
    check_wire_msg_with(msg, ARTIFACT_INLINE_MAX)
}

/// 同 [`check_wire_msg`]，内联 artifact 全文的上限由调用方给出（手动导入的会话包是本机
/// 文件，全文都带着，上限比日志里的 op 宽）。
pub fn check_wire_msg_with(msg: &WireMsg, artifact_inline_max: usize) -> Result<()> {
    if !ids::is_gid(&msg.gid) || msg.parent.as_deref().is_some_and(|p| !ids::is_gid(p)) {
        return Err(SyncError::invalid("消息的身份不合规"));
    }
    if msg.parent.as_deref() == Some(msg.gid.as_str()) {
        return Err(SyncError::invalid("消息的 parent 指向自己"));
    }
    if !ALLOWED_ROLES.contains(&msg.role.as_str()) {
        return Err(SyncError::invalid("消息的 role 不合规"));
    }
    if !(0..TS_MAX).contains(&msg.ts) {
        return Err(SyncError::invalid("消息的时间戳不合规"));
    }
    check_message_data(&msg.data, &msg.role)?;
    if let Some(meta) = &msg.meta {
        check_wire_meta(meta)?;
    }
    if msg.tools.len() > 256 || msg.refs.len() > 256 || msg.artifacts.len() > 256 {
        return Err(SyncError::invalid("消息的附带项过多"));
    }
    msg.refs.iter().try_for_each(check_ref)?;
    msg.artifacts
        .iter()
        .try_for_each(|artifact| check_artifact(artifact, artifact_inline_max))?;
    Ok(())
}

/// 会话标题：去掉控制字符与首尾空白，限长。空的返回 `None`。
pub fn clean_title(title: &str) -> Option<String> {
    let cleaned: String = title
        .chars()
        .filter(|c| !c.is_control())
        .take(200)
        .collect();
    let cleaned = cleaned.trim();
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// 发布本机的一行时用：行内容形态合规就原样携带；不合规（理论上不会出现）则宽松解析后
/// 重新序列化；仍不行就换成一条占位文本——链不能断，后面的消息都以它为 `parent`。
pub fn publishable_data(data: &str, role: &str, ts: i64) -> String {
    if check_message_data(data, role).is_ok() {
        return data.to_string();
    }
    if let Ok(message) = serde_json::from_str::<rclaw_core::Message>(data) {
        if let Ok(text) = serde_json::to_string(&message) {
            if check_message_data(&text, role).is_ok() {
                return text;
            }
        }
    }
    serde_json::json!({
        "role": role,
        "content": [{ "type": "text", "text": "（这条消息的内容无法同步）" }],
        "ts": ts,
    })
    .to_string()
}

/// 消息正文里的全部文本（文本块、工具结果、媒体地址），用来找它引用了哪些工作区文件。
pub fn text_leaves(data: &str) -> Vec<String> {
    fn walk(value: &serde_json::Value, out: &mut Vec<String>) {
        match value {
            serde_json::Value::String(text) => out.push(text.clone()),
            serde_json::Value::Array(items) => items.iter().for_each(|item| walk(item, out)),
            serde_json::Value::Object(map) => map
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "type" | "id" | "tool_use_id" | "name"))
                .for_each(|(_, item)| walk(item, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(data) {
        if let Some(content) = value.get("content") {
            walk(content, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(path: &str) -> LocalRoot {
        LocalRoot::new(Path::new(path))
    }

    const GID: &str = "0123456789abcdef.0123456789abcdef";

    #[test]
    fn paths_round_trip_between_two_roots() {
        // 设备 A：Windows 风格的根（正文里两种写法都有）。
        let a = LocalRoot {
            fwd: "C:/Users/me/agent/".into(),
            backslash: Some(r"C:\\Users\\me\\agent".into()),
        };
        let a_artifacts = "C:/Users/me/agent/artifacts/desktop-main-0011223344556677/";
        let data = r#"{"content":[{"text":"完整内容已存至: C:/Users/me/agent/artifacts/desktop-main-0011223344556677/12-shell.txt 另见 C:/Users/me/agent/uploads/a.png 与 C:\\Users\\me\\agent\\output\\b.txt","type":"text"}],"role":"user"}"#;
        let portable = to_portable(data, &a, a_artifacts);
        assert!(portable.contains("<<RCLAW_ARTIFACTS>>/12-shell.txt"));
        assert!(portable.contains("<<RCLAW_AGENT_ROOT>>/uploads/a.png"));
        // 反斜杠写法的路径整段改成正斜杠，换一个平台还是一条路径。
        assert!(portable.contains("<<RCLAW_AGENT_ROOT>>/output/b.txt"));
        assert!(!portable.contains("Users"));
        assert_eq!(artifact_names(&portable), ["12-shell.txt"]);

        // 设备 B：另一套根。
        let b = root("/data/agent");
        let b_artifacts = "/data/agent/artifacts/desktop-main-0011223344556677/";
        let local = to_local(&portable, &b, b_artifacts);
        assert!(local.contains("/data/agent/artifacts/desktop-main-0011223344556677/12-shell.txt"));
        assert!(local.contains("/data/agent/uploads/a.png"));
        assert!(local.contains("/data/agent/output/b.txt"));
        // B 再发布出去，与 A 发布的逐字节相同。
        assert_eq!(to_portable(&local, &b, b_artifacts), portable);
    }

    #[test]
    fn only_the_path_after_a_backslash_root_is_rewritten() {
        let a = LocalRoot {
            fwd: "C:/Users/me/agent/".into(),
            backslash: Some(r"C:\\Users\\me\\agent".into()),
        };
        let prefix = "C:/Users/me/agent/artifacts/k/";
        // 多级目录、目录结尾、根后面接正斜杠；路径后面的文字（哪怕带反斜杠）、路径里空格
        // 之后的部分、名字以根开头的另一个目录都不动。
        let data = r#"{"content":[{"text":"见 C:\\Users\\me\\agent\\output\\a\\b.txt 里的 \\d+，目录 C:\\Users\\me\\agent\\uploads\\，还有 C:\\Users\\me\\agent\\output\\my dir\\c.txt\n下一行 \\n C:\\Users\\me\\agent/media/d.png 与 C:\\Users\\me\\agent2\\e.txt","type":"text"}],"role":"user"}"#;
        let portable = to_portable(data, &a, prefix);
        assert_eq!(
            portable,
            r#"{"content":[{"text":"见 <<RCLAW_AGENT_ROOT>>/output/a/b.txt 里的 \\d+，目录 <<RCLAW_AGENT_ROOT>>/uploads/，还有 <<RCLAW_AGENT_ROOT>>/output/my dir\\c.txt\n下一行 \\n <<RCLAW_AGENT_ROOT>>/media/d.png 与 C:\\Users\\me\\agent2\\e.txt","type":"text"}],"role":"user"}"#
        );
        // 还是合法的 JSON。
        assert!(serde_json::from_str::<serde_json::Value>(&portable).is_ok());
    }

    #[test]
    fn literal_marker_text_survives_a_round_trip() {
        let r = root("/data/agent");
        let prefix = "/data/agent/artifacts/k/";
        let data = r#"{"content":[{"text":"正文里写着 <<RCLAW_AGENT_ROOT>>/x 和 <<RCLAW__y","type":"text"}],"role":"user"}"#;
        let portable = to_portable(data, &r, prefix);
        assert!(!portable.contains("<<RCLAW_AGENT_ROOT>>/x"));
        assert_eq!(to_local(&portable, &r, prefix), data);
        // 换一台设备落地，字面文本也不会被当成标记展开。
        let other = root("/other/root");
        assert_eq!(
            to_local(&portable, &other, "/other/root/artifacts/k/"),
            data
        );
    }

    #[test]
    fn artifact_names_ignore_traversal() {
        let text = "<<RCLAW_ARTIFACTS>>/../../etc/passwd <<RCLAW_ARTIFACTS>>/ok-1.txt. <<RCLAW_ARTIFACTS>>/ok-1.txt";
        assert_eq!(artifact_names(text), ["ok-1.txt"]);
    }

    fn msg(data: &str) -> WireMsg {
        WireMsg {
            gid: GID.into(),
            parent: None,
            role: "user".into(),
            ts: 1_763_438_400,
            data: data.into(),
            meta: None,
            tools: Vec::new(),
            refs: Vec::new(),
            artifacts: Vec::new(),
        }
    }

    const GOOD: &str = r#"{"content":[{"text":"hi","type":"text"}],"role":"user","ts":1763438400}"#;

    #[test]
    fn accepts_every_block_type() {
        check_wire_msg(&msg(GOOD)).unwrap();
        let all = r#"{"role":"user","content":[
            {"type":"text","text":"a"},
            {"type":"media_url","kind":"image","url":"uploads/a.png"},
            {"type":"reasoning","reasoning_content":"r"},
            {"type":"tool_use","id":"c1","name":"shell","input":{"cmd":"ls"}},
            {"type":"tool_result","tool_use_id":"c1","content":"ok","is_error":true}
        ]}"#;
        check_wire_msg(&msg(all)).unwrap();
    }

    #[test]
    fn rejects_malformed_message_data() {
        for bad in [
            "not json",
            "[]",
            r#"{"role":"user","content":"plain string"}"#,
            r#"{"role":"user","content":null}"#,
            r#"{"role":"user","content":[{"type":"text"}]}"#,
            r#"{"role":"user","content":[{"type":"shell","cmd":"rm"}]}"#,
            r#"{"role":"user","content":[{"text":"no type"}]}"#,
            r#"{"role":"assistant","content":[]}"#,
            r#"{"role":"system","content":[]}"#,
            r#"{"role":"user","content":[],"permission_mode":"yolo"}"#,
            r#"{"role":"user","content":[],"ts":"now"}"#,
        ] {
            assert!(check_wire_msg(&msg(bad)).is_err(), "{bad}");
        }
        let mut system = msg(r#"{"role":"system","content":[]}"#);
        system.role = "system".into();
        assert!(check_wire_msg(&system).is_err());
    }

    #[test]
    fn rejects_bad_identity_meta_refs_and_artifacts() {
        let mut m = msg(GOOD);
        m.gid = "x".into();
        assert!(check_wire_msg(&m).is_err());

        let mut m = msg(GOOD);
        m.parent = Some(GID.into());
        assert!(check_wire_msg(&m).is_err());

        let mut m = msg(GOOD);
        m.ts = -1;
        assert!(check_wire_msg(&m).is_err());

        let other = "0123456789abcdef.fedcba9876543210";
        for (meta, ok) in [
            (
                serde_json::json!({"compaction": {"head_until": null, "tail_from": other}}),
                true,
            ),
            (
                serde_json::json!({"compaction": {"head_until": other, "tail_from": other}}),
                true,
            ),
            (
                serde_json::json!({"fork_notice": {"parent": null, "anchor": null}}),
                true,
            ),
            (
                serde_json::json!({"fork_notice": {"parent": "a".repeat(24), "anchor": other}}),
                true,
            ),
            (
                serde_json::json!({"compaction": {"head_until": 3, "tail_from": 5}}),
                false,
            ),
            (
                serde_json::json!({"compaction": {"tail_from": other, "extra": 1}}),
                false,
            ),
            (
                serde_json::json!({"fork_notice": {"parent": "main", "anchor": 12}}),
                false,
            ),
            (serde_json::json!({"other": {}}), false),
            (
                serde_json::json!({"compaction": {"tail_from": other}, "fork_notice": {}}),
                false,
            ),
            (serde_json::json!("x"), false),
        ] {
            let mut m = msg(GOOD);
            m.meta = Some(meta.clone());
            assert_eq!(check_wire_msg(&m).is_ok(), ok, "{meta}");
        }

        let digest = "a".repeat(64);
        for (path, ok) in [
            ("uploads/a.png", true),
            ("media/2026/x y.jpg", true),
            ("output/报告.docx", true),
            ("config/SOUL.md", false),
            ("uploads/../config/SOUL.md", false),
            ("/etc/passwd", false),
            ("uploads\\a.png", false),
            ("uploads//a.png", false),
            ("uploads/", false),
        ] {
            let mut m = msg(GOOD);
            m.refs = vec![FileRef {
                path: path.into(),
                blob: digest.clone(),
                size: 1,
            }];
            assert_eq!(check_wire_msg(&m).is_ok(), ok, "{path}");
        }

        for (name, ok) in [
            ("12-shell.txt", true),
            (".hidden", false),
            ("a/b.txt", false),
            ("..", false),
            ("", false),
        ] {
            let mut m = msg(GOOD);
            m.artifacts = vec![Artifact {
                name: name.into(),
                text: Some("x".into()),
                size: 1,
                blob: None,
            }];
            assert_eq!(check_wire_msg(&m).is_ok(), ok, "{name}");
        }
    }

    #[test]
    fn publishable_data_keeps_good_rows_and_repairs_bad_ones() {
        assert_eq!(publishable_data(GOOD, "user", 1), GOOD);
        // content 是字符串：宽松解析后重新序列化成数组。
        let repaired = publishable_data(r#"{"role":"user","content":"hello"}"#, "user", 1);
        check_message_data(&repaired, "user").unwrap();
        assert!(repaired.contains("hello"));
        // 完全不可用：占位文本，仍然合规。
        let placeholder = publishable_data("garbage", "assistant", 7);
        check_message_data(&placeholder, "assistant").unwrap();
    }

    #[test]
    fn text_leaves_collects_content_strings() {
        let data = r#"{"role":"user","content":[
            {"type":"text","text":"看 uploads/a.png"},
            {"type":"media_url","kind":"image","url":"media/b.jpg"},
            {"type":"tool_result","tool_use_id":"c1","content":"写到 output/c.txt"}
        ]}"#;
        let leaves = text_leaves(data);
        assert!(leaves.contains(&"看 uploads/a.png".to_string()));
        assert!(leaves.contains(&"media/b.jpg".to_string()));
        assert!(leaves.contains(&"写到 output/c.txt".to_string()));
        assert!(!leaves.iter().any(|l| l == "c1" || l == "text"));
    }
}
