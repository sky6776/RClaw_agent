//! 工具结果落盘 + 引用句柄（ZCode artifact 策略，第十二轮移植）。
//!
//! **问题**：工具结果（大文件 read、grep 海量命中、shell 长输出）原样进入模型上下文，
//! 单条就能占掉几十 KB；microcompact 只在请求视图上【剪枝旧结果】，超预算结果在变旧
//! 之前就已经把当前迭代撑大。
//!
//! **机制**（对齐 ZCode `result-serialization.ts` 的 artifact 分支）：工具结果超过预算
//! 字节时，把【脱敏后的全文】写入 `<agent根>/artifacts/<会话键>/<回滚点id>-<工具名>.txt`，
//! 模型收到头尾预览（4KB，[`rclaw_tools::truncate::head_tail_truncate`]——诊断几乎都在
//! 尾部，与 shell 输出的既有截断哲学一致）+ 文件路径引用句柄；需要中段细节时用 `read`
//! 工具按 `offset/limit` 分页取回，不必重新执行工具。
//!
//! - **落盘失败静默回退原内容**：artifact 写入失败不能把一次成功的工具调用变成失败
//!   （ZCode 同款决策），此时退回旧行为（原样进上下文）。
//! - **写在脱敏之后**：磁盘副本同样不含敏感明文。
//! - **与 per-tool 截断的关系**：shell 等工具在更内层已有自己的头尾截断（64KB 流式
//!   预算），落盘存的是【工具返回的全部】——工具自己丢掉的中段（内存有界的设计取舍）
//!   不在本层恢复；本层保证的是模型上下文从第一刻起只见到小句柄。
//! - **会话键**：前台 turn 用 `channel-chat_id`；子代理没有源会话，用
//!   `subagent-<channel>` 归档（审计仍可按来源通道定位）。
//! - 已知边界：`agent/exports/`、`agent/artifacts/` 同为运行时产物目录，不清理、不
//!   入库；目录随时间增长，由用户按需清理（与 exports 同纪律）。

use std::path::{Path, PathBuf};

use rclaw_tools::truncate::head_tail_truncate;

use crate::options::AgentOptions;

/// 预览预算（头 + 尾合计，中段以省略标记替代）。
const PREVIEW_BYTES: usize = 4096;
/// 从 [`AgentOptions`] 派生的落盘配置（`Clone` 便宜，随执行上下文传递）。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ToolResultSpill {
    pub(crate) enabled: bool,
    pub(crate) threshold_bytes: usize,
}

impl ToolResultSpill {
    pub(crate) fn from_options(opts: &AgentOptions) -> Self {
        ToolResultSpill {
            enabled: opts.tool_result_artifact_enabled,
            threshold_bytes: opts.tool_result_artifact_bytes.max(1),
        }
    }
}

/// 会话键：规则单一事实源在 [`rclaw_store::session::artifact_chat_key`]（store 的
/// 会话删除级联清理用同一规则定位目录）。
pub(crate) fn sanitize_chat_key(channel: &str, chat_id: &str) -> String {
    rclaw_store::session::artifact_chat_key(channel, chat_id)
}

/// artifact 文件名：纳秒单调 id（复用 rewind 的分配器语义，进程内严格递增、可读时间）
/// + 工具名（工具名来自注册表，仍做白名单清洗防御未知来源）。
fn artifact_file_name(tool: &str) -> String {
    let safe_tool: String = tool
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(32)
        .collect();
    format!("{}-{safe_tool}.txt", super::rewind::next_rewind_id())
}

/// artifact 目录（按需创建）。
fn artifacts_dir(base: &Path, chat_key: &str) -> PathBuf {
    base.join("artifacts").join(chat_key)
}

/// 超预算时把 `content` 替换为「预览 + 路径」引用句柄；未超限 / 关闭 / 落盘失败则原样保留。
///
/// 返回落盘路径（仅在成功落盘并替换时为 `Some`，供测试与日志断言）。
pub(crate) fn spill_if_over_budget(
    spill: &ToolResultSpill,
    base: &Path,
    channel: &str,
    chat_id: &str,
    tool: &str,
    content: &mut String,
) -> Option<PathBuf> {
    if !spill.enabled || content.len() <= spill.threshold_bytes {
        return None;
    }
    let dir = artifacts_dir(base, &sanitize_chat_key(channel, chat_id));
    let path = dir.join(artifact_file_name(tool));
    // atomic_write 自带 ensure_parent_dir；失败静默回退（artifact 只是上下文优化）。
    if let Err(error) = rclaw_store::fs::atomic_write(&path, content.as_bytes()) {
        tracing::warn!(
            tool,
            path = %path.display(),
            %error,
            "工具结果落盘失败，按原内容继续（artifact 只是上下文优化，不改变工具结果）"
        );
        return None;
    }
    let preview = head_tail_truncate(content, PREVIEW_BYTES);
    let total = content.len();
    // 路径统一正斜杠：跨平台一致（Windows 侧 API 与 read 工具都接受），
    // 句柄进 transcript 后也不因平台不同而形态漂移。
    let display_path = path.to_string_lossy().replace('\\', "/");
    *content = format!(
        "[工具输出超出预算已落盘：原始 {total} 字节（预算 {} 字节）]\n\
         完整内容已存至: {display_path}\n\
         取回中段：多行内容用 read 工具按 offset/limit 分页读取该文件；单行长内容\n\
         （如压缩 JSON）read 按行分页无效，改用 shell 字节分页，例如\n\
         head -c 32768 \"{display_path}\" | tail -c 4096（每次取一小段，不要整读）。\n\n\
         预览（头尾合计 {} 字节，中段已省略）：\n{}",
        spill.threshold_bytes,
        PREVIEW_BYTES,
        preview.text,
    );
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spill_on(threshold: usize) -> ToolResultSpill {
        ToolResultSpill {
            enabled: true,
            threshold_bytes: threshold,
        }
    }

    #[test]
    fn chat_key_sanitizes_and_buckets_subagents() {
        // 键 = 可读前缀 + FNV 摘要（规则单一事实源在 store::artifact_chat_key；
        // 摘要防长 id 截断碰撞，具体形态测试在 store 侧）。
        assert!(sanitize_chat_key("cli", "console").starts_with("cli-console-"));
        assert!(sanitize_chat_key("napcat", "12345").starts_with("napcat-12345-"));
        // 路径分隔符与空白替换成下划线，不允许逃出目录。
        assert!(sanitize_chat_key("a/b", "c\\d e").starts_with("a_b-c_d_e-"));
        // 子代理（无源会话）按来源通道归档。
        assert!(sanitize_chat_key("desktop", "").starts_with("subagent-desktop-"));
        // 超长键截断（规则在 store::artifact_chat_key，总长 ≤ 63）。
        assert!(sanitize_chat_key("x", &"9".repeat(200)).chars().count() <= 63);
        // 只差尾字符的两个长 chat_id 不得共享目录（纯截断曾会）。
        let long_a = format!("{}A", "x".repeat(56));
        let long_b = format!("{}B", "x".repeat(56));
        assert_ne!(sanitize_chat_key("desktop", &long_a), sanitize_chat_key("desktop", &long_b));
    }

    #[test]
    fn artifact_name_is_monotonic_and_tool_is_sanitized() {
        let first = artifact_file_name("read");
        let second = artifact_file_name("weird/tool name");
        let first_ts: i64 = first
            .split('-')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("文件名以数字 id 开头");
        let second_ts: i64 = second
            .split('-')
            .next()
            .and_then(|n| n.parse().ok())
            .expect("文件名以数字 id 开头");
        assert!(second_ts > first_ts);
        assert!(second.contains("weird_tool_name.txt"), "{second}");
    }

    #[test]
    fn under_budget_content_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let spill = spill_on(1024);
        let mut content = "short".to_string();
        assert!(spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &mut content).is_none());
        assert_eq!(content, "short");
        // 关闭时超预算也不动。
        let off = ToolResultSpill {
            enabled: false,
            threshold_bytes: 4,
        };
        let mut big = "x".repeat(100);
        assert!(spill_if_over_budget(&off, dir.path(), "cli", "c", "read", &mut big).is_none());
        assert_eq!(big.len(), 100);
    }

    #[test]
    fn over_budget_content_is_replaced_with_preview_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let spill = spill_on(1024);
        let head = "HEAD LINE\n".repeat(20); // 200 字节
        let middle = "m".repeat(8192);
        let tail = "\nTAIL LINE\n".repeat(20);
        let mut content = format!("{head}{middle}{tail}");
        let path =
            spill_if_over_budget(&spill, dir.path(), "cli", "console", "grep", &mut content)
                .expect("超预算应落盘");
        // 落盘的是【全文】。
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(disk.starts_with("HEAD LINE\n"));
        assert!(disk.contains(&middle));
        assert!(disk.ends_with("TAIL LINE\n"));
        // 模型收到的是小句柄：路径 + 头尾预览 + 中段省略标记。
        let display_path = path.to_string_lossy().replace('\\', "/");
        assert!(content.contains(&display_path), "{content}");
        assert!(content.contains("HEAD LINE"));
        assert!(content.contains("TAIL LINE"));
        assert!(content.contains("bytes omitted"), "{content}");
        assert!(!content.contains(&middle), "中段不得整段进入句柄");
        assert!(content.len() < PREVIEW_BYTES + 900, "{}", content.len());
        // 句柄必须给出读取指引（read 行分页 + shell 字节分页兜底——单行长内容
        // offset/limit 取不到中段）。
        assert!(content.contains("offset/limit"), "{content}");
        assert!(content.contains("head -c"), "{content}");
    }

    #[test]
    fn artifacts_are_bucketed_per_chat_key() {
        let dir = tempfile::tempdir().unwrap();
        let spill = spill_on(16);
        let mut a = "a".repeat(100);
        let mut b = "b".repeat(100);
        let pa = spill_if_over_budget(&spill, dir.path(), "cli", "chat-a", "read", &mut a).unwrap();
        let pb = spill_if_over_budget(&spill, dir.path(), "cli", "chat-b", "read", &mut b).unwrap();
        let key_a = dir
            .path()
            .join("artifacts")
            .join(sanitize_chat_key("cli", "chat-a"));
        let key_b = dir
            .path()
            .join("artifacts")
            .join(sanitize_chat_key("cli", "chat-b"));
        assert!(pa.starts_with(&key_a), "{pa:?}");
        assert!(pb.starts_with(&key_b), "{pb:?}");
        assert_ne!(key_a, key_b, "不同会话不得共享目录");
    }
}
