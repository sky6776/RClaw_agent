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

/// agent 根的绝对形态：CLI 的 base_dir 默认是相对路径（"agent"），句柄里的落盘
/// 路径若保持相对，read / shell 都会按【各自的工作目录】再解析一次——shell 在
/// agent 根里执行，"agent/artifacts/..." 会变成 agent/agent/... 而扑空。相对 base
/// 按进程 CWD 展开；已是绝对路径原样返回。
fn absolute_base(base: &Path) -> PathBuf {
    if base.is_absolute() {
        base.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(base))
            .unwrap_or_else(|_| base.to_path_buf())
    }
}

/// 落盘行宽（字节）：read 的 offset/limit 是【行式】分页，单行超长内容（压缩 JSON、
/// 长正则串）靠它永远取不到中段，而 grep 会截断长行、受限子代理（candidate_patch
/// 的只读小循环）没有 shell 不能按字节取段——只把超过行宽的【超长行】折行。
const SPILL_LINE_BYTES: usize = 2048;

/// 只折叠超长行：原始行结构原样保留（短行即原文），仅超过 [`SPILL_LINE_BYTES`]
/// 字节的行在字符边界处折行（插入 '\n'）。多行内容（源码 / 日志 / diff）落盘后
/// 与原文逐字节一致；单行超长内容（压缩 JSON）折行后行式分页可用——此时拼接
/// 各行即原文（原文本就没有换行）。
fn fold_long_lines(content: &str) -> String {
    let mut out = String::with_capacity(content.len() + 64);
    let mut first_line = true;
    for line in content.split('\n') {
        if !first_line {
            out.push('\n');
        }
        first_line = false;
        let mut line_bytes = 0usize;
        for ch in line.chars() {
            if line_bytes >= SPILL_LINE_BYTES {
                out.push('\n');
                line_bytes = 0;
            }
            out.push(ch);
            line_bytes += ch.len_utf8();
        }
    }
    out
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

/// 这次调用是不是在【取回 artifact】（`read` 工具、path 落在本 agent 根的 artifacts/ 下）。
/// 取回结果不再二次落盘：否则模型按句柄分页读中段，一页超过预算就又生成一个 artifact
/// 套一个句柄——取回永远到不了内容本身。`read` 自身的 256KB 输出上限兜住单页体积。
fn reads_artifact(base: &Path, tool: &str, tool_input: &str) -> bool {
    if tool != "read" {
        return false;
    }
    let Ok(input) = serde_json::from_str::<serde_json::Value>(tool_input) else {
        return false;
    };
    let Some(path) = input.get("path").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let normalized = path.replace('\\', "/");
    let mut root = absolute_base(base)
        .join("artifacts")
        .to_string_lossy()
        .replace('\\', "/");
    if !root.ends_with('/') {
        root.push('/');
    }
    // 句柄里的路径由本模块生成，模型照抄；Windows 路径大小写不敏感，宽松比较。模型也可能
    // 写成相对 agent 根的 `artifacts/<key>/…`（`read` 同样接受，拼到 base 下）。
    has_dir_prefix(&normalized, &root) || has_dir_prefix(&normalized, "artifacts/")
}

/// `path` 是否以目录前缀 `prefix`（以 `/` 结尾）开头、且其后还有内容。按【字节】比较：
/// `prefix.len()` 不一定落在 `path` 的字符边界上（前缀之外的中文路径分量），按字符串
/// 切片会 panic，而这里跑在 turn 任务里——一次 `read` 大文件就能把整条 turn 打崩。
fn has_dir_prefix(path: &str, prefix: &str) -> bool {
    let (path, prefix) = (path.as_bytes(), prefix.as_bytes());
    path.len() > prefix.len() && path[..prefix.len()].eq_ignore_ascii_case(prefix)
}

/// 超预算时把 `content` 替换为「预览 + 路径」引用句柄；未超限 / 关闭 / 落盘失败 /
/// 本身就是在取回 artifact（见 [`reads_artifact`]）则原样保留。
///
/// `tool_input` 为该次调用的入参 JSON（只用于识别 artifact 取回，解析失败按普通调用）。
/// 返回落盘路径（仅在成功落盘并替换时为 `Some`，供测试与日志断言）。
pub(crate) fn spill_if_over_budget(
    spill: &ToolResultSpill,
    base: &Path,
    channel: &str,
    chat_id: &str,
    tool: &str,
    tool_input: &str,
    content: &mut String,
) -> Option<PathBuf> {
    if !spill.enabled || content.len() <= spill.threshold_bytes {
        return None;
    }
    if reads_artifact(base, tool, tool_input) {
        return None;
    }
    let dir = artifacts_dir(&absolute_base(base), &sanitize_chat_key(channel, chat_id));
    let path = dir.join(artifact_file_name(tool));
    // atomic_write 自带 ensure_parent_dir；失败静默回退（artifact 只是上下文优化）。
    // 落盘的是折叠形态（见 fold_long_lines）：原始行结构保留，仅超长行折行。
    if let Err(error) = rclaw_store::fs::atomic_write(&path, fold_long_lines(content).as_bytes()) {
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
    // 运行时披露随预览保留（codex 第 7 轮 P1）：披露块落在被省略的中段时，父代理
    // 就看不到子代理的未解决副作用错误。聚合输出（TaskList 排空邮箱等）把多条结果
    // 拼在一起，仅靠「披露放尾部」保证不了——从原文提取全部披露块，附在预览之后；
    // 尾窗已完整保留的块去重跳过。
    let disclosure_blocks = extract_runtime_disclosure_blocks(content);
    let disclosure_suffix = disclosure_blocks
        .iter()
        .filter(|block| !preview.text.contains(block.as_str()))
        .cloned()
        .collect::<Vec<_>>()
        .join("\n\n");
    // 路径统一正斜杠：跨平台一致（Windows 侧 API 与 read 工具都接受），
    // 句柄进 transcript 后也不因平台不同而形态漂移。
    let display_path = path.to_string_lossy().replace('\\', "/");
    // 每页行数建议：按行宽上限估算一页不超过预算（短行内容可以更多；取回 artifact 的
    // read 结果不会再次落盘，页取大了只是多占上下文）。
    let page_lines = (spill.threshold_bytes / SPILL_LINE_BYTES).max(1);
    *content = format!(
        "[工具输出超出预算已落盘：原始 {total} 字节（预算 {} 字节）]\n\
         完整内容已存至: {display_path}\n\
         （多行内容与原文逐字节一致；仅超过 ~2KB 的超长行被折行——若原文是单行长\n\
         内容，拼接各行即原文）\n\
         取回中段：用 read 工具按 offset/limit 分页读取该文件（建议每页 limit ≤ {page_lines} 行，\n\
         超长行已折行，任意段都能按行取回；读取该文件的结果不会再次落盘）。\n\n\
         预览（头尾合计 {} 字节，中段已省略）：\n{}",
        spill.threshold_bytes, PREVIEW_BYTES, preview.text,
    );
    if !disclosure_suffix.is_empty() {
        content.push_str(
            "\n\n未解决工具错误的运行时披露（原文位于被省略的中段，随预览保留）：\n\n",
        );
        content.push_str(&disclosure_suffix);
    }
    Some(path)
}

/// 从工具结果文本提取全部运行时披露块，去重保序。两种形态：
///
/// 1. 【行首明文块】披露标题在行首（允许空格缩进）；
/// 2. 【JSON 字符串字段内的转义块】标题紧随已知披露字段（`unresolved_disclosure`
///    / `unresolved_disclosures` 数组）的序列化锚点，取到该字符串的收束引号为止、
///    `\n` 还原为换行。披露内容不含引号（工具名已净化、回执号/类别是安全字符），
///    解析安全；任意其它非行首标题（普通 read 输出正文等）一律忽略。
///
/// 两形态都经 [`disclosure_block_from_text`] 只保留「标题 + 连续 `- ` 条目（含
/// 「（回执 」标记）」的运行时格式前缀——转义字符串里条目之后的大段普通正文、
/// 计划报告相邻的普通 `- ` 行都不会被并入或回填，spill 预算不被打破
/// （codex 第 8-12 轮 P1）。
fn extract_runtime_disclosure_blocks(content: &str) -> Vec<String> {
    let header = super::run::RUNTIME_DISCLOSURE_HEADER;
    let mut blocks: Vec<String> = Vec::new();
    for (index, _) in content.match_indices(header) {
        let line_start = content[..index].rfind('\n').map(|i| i + 1).unwrap_or(0);
        let at_line_start = content[line_start..index].chars().all(|ch| ch == ' ');
        let block = if at_line_start {
            disclosure_block_from_text(&content[line_start..])
        } else {
            let before = &content[..index];
            // 已知字段的两种序列化形态：对象字符串字段（ask 信封/执行快照/计划事件）
            // 与披露数组（run_completed 事件/GetWorkflowRun 投影）。数组后续元素
            // 前面是 `","`——必须落在未闭合的已知数组内才算数，任意 `","` 不认
            // （codex 第 11 轮 P1）。
            let object_field = "\"unresolved_disclosure\":\"";
            let array_field = "\"unresolved_disclosures\":[\"";
            let known = if before.ends_with(object_field) || before.ends_with(array_field) {
                true
            } else if before.ends_with("\",\"") {
                match before.rfind(array_field) {
                    Some(pos) => {
                        // `[` 在锚点内倒数第二字符：从它到标题之间不得出现数组的
                        // 收括号或任何对象边界（数组仍开启、内容只有字符串与逗号）。
                        let open = pos + array_field.len() - 2;
                        content[open..index]
                            .chars()
                            .all(|ch| !matches!(ch, ']' | '{' | '}'))
                    }
                    None => false,
                }
            } else {
                false
            };
            if !known {
                continue;
            }
            let rest = &content[index..];
            // 转义字符串必须有收束引号：真实序列化必有；未闭合 = 普通正文噪声，
            // 拒绝提取，否则无引号大段正会被吞到 EOF（codex 第 11 轮 P1 遗留）。
            let Some(end_rel) = rest[header.len()..].find('"') else {
                continue;
            };
            let block = rest[..end_rel + header.len()].replace("\\n", "\n");
            disclosure_block_from_text(&block)
        };
        if let Some(block) = block {
            if !blocks.contains(&block) {
                blocks.push(block);
            }
        }
    }
    blocks
}

/// 从以披露标题开头（允许行首空白）的文本提取「标题 + 连续 `- ` 条目」块：条目行
/// 必须含「（回执 」标记（渲染不变量），遇首个其它行即停——转义字符串里条目之后
/// 的大段普通正文不会被并入；不足一条条目返回 None（标题孤立 = 文本噪声）。
fn disclosure_block_from_text(text: &str) -> Option<String> {
    let mut kept_lines = 1usize;
    for line in text.lines().skip(1) {
        if line.starts_with("- ") && line.contains("（回执 ") {
            kept_lines += 1;
        } else {
            break;
        }
    }
    if kept_lines < 2 {
        return None;
    }
    Some(text.lines().take(kept_lines).collect::<Vec<_>>().join("\n"))
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
    fn disclosure_extraction_ignores_json_string_field_but_keeps_real_block() {
        // codex 第 8/9 轮 P1：单行 JSON 输出（AgentStatus/GetWorkflowRun）里的披露
        // 标题在字符串字段内部——按转义形态安全解析为披露块（预算不被超长 JSON
        // 破坏），行首的真实块照常提取。
        let header = super::super::run::RUNTIME_DISCLOSURE_HEADER;
        let json_blob = format!(
            "{{\"execution\":{{\"unresolved_disclosure\":\"{}{}- echo（回执 r1-1-d70708db，类别 tool_execution）\"}}}}",
            header, "\\n"
        );
        let real = format!(
            "{}{}- write（回执 r0-1-11111111，类别 tool_execution）",
            header,
            "\n"
        );
        let content = format!("{}\n\n{}\n\n{}", "x".repeat(4096), json_blob, real);
        let blocks = extract_runtime_disclosure_blocks(&content);
        assert_eq!(blocks.len(), 2, "{blocks:?}");
        assert!(blocks.contains(&real), "行首真实块应被提取: {blocks:?}");
        // JSON 字符串字段按转义形态安全解析——块是披露明文而非整条 JSON。
        let json_block = format!(
            "{}{}- echo（回执 r1-1-d70708db，类别 tool_execution）",
            header,
            "\n"
        );
        assert!(blocks.contains(&json_block), "{blocks:?}");
        assert!(
            !blocks.iter().any(|block| block.contains("{\"execution\"")),
            "整条 JSON 不得被当作披露块回填: {blocks:?}"
        );
        // 只有标题行没有「（回执 」条目的匹配视为文本噪声跳过。
        let content_header_only = format!("前缀 {} 无条目", header);
        assert!(extract_runtime_disclosure_blocks(&content_header_only).is_empty());
        // 行首标题 + EOF（无条目）同样跳过；相邻的普通 `- ` 报告行（无「（回执 」）
        // 不会被并入披露块（计划报告行噪声，codex 第 9 轮 nice-to-have）。
        let content_report = format!(
            "{}{}- write（回执 r0-1-11111111，类别 timeout）{}- impl [failed]: 摘要行",
            header,
            "\n",
            "\n"
        );
        let blocks = extract_runtime_disclosure_blocks(&content_report);
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert!(!blocks[0].contains("impl [failed]"), "{blocks:?}");
        // codex 第 10 轮反例：普通正文（非 JSON 字段）里的非行首标题 + 条目 + 大段
        // 无引号正文——不是已知披露字段的序列化，整段忽略（spill 预算不被打破）。
        let plain = format!(
            "说明 {}{}- write（回执 r0-1-11111111，类别 timeout）{}{}",
            header,
            "\n",
            "\n",
            "正文".repeat(50_000)
        );
        // codex 第 13 轮防退化断言：闭合 JSON 字符串内「有效条目 + 换行 + 30KB 正文」
        // ——只保留标题与连续条目，正文不回填。
        let blob = format!("\"unresolved_disclosure\":\"{}\\n- write（回执 r0-1-1，类别 timeout）\\n{}\"", header, "body".repeat(30_000));
        let blocks = extract_runtime_disclosure_blocks(&blob);
        assert_eq!(blocks.len(), 1, "{blocks:?}");
        assert!(blocks[0].ends_with("- write（回执 r0-1-1，类别 timeout）"), "{blocks:?}");
        assert!(!blocks[0].contains("bodybody"), "{blocks:?}");
        // 行首仅标题 + EOF（无条目）：直接断言为空。
        assert!(extract_runtime_disclosure_blocks(&format!(
            "{}{}- 孤立标题行", header, "\n"
        )).is_empty());

        // codex 第 11 轮：披露数组的多元素（后续元素前面是 ","）全部提取。
        let block_a = format!("{}{}- memory（回执 r1-1-027e4bd5，类别 tool_execution）", header, "\n");
        let block_b = format!("{}{}- shell（回执 r1-2-33334444，类别 timeout）", header, "\n");
        let array_json = format!(
            "{{\"run_id\":\"r\",\"unresolved_disclosures\":[\"{}\",\"{}\"]}}",
            block_a.replace("\n", "\\n"),
            block_b.replace("\n", "\\n"),
        );
        let blocks = extract_runtime_disclosure_blocks(&array_json);
        assert_eq!(blocks, vec![block_a, block_b], "{blocks:?}");
        // 转义字符串未闭合（普通正文噪声）：拒绝提取，不吞到 EOF。
        let unclosed = format!(
            "{{\"unresolved_disclosure\":\"{}{}- write（回执 r0-1-1，类别 timeout）{}大段无引号正文",
            header,
            "\n",
            "\n",
        );
        assert!(extract_runtime_disclosure_blocks(&unclosed).is_empty());
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
        assert_ne!(
            sanitize_chat_key("desktop", &long_a),
            sanitize_chat_key("desktop", &long_b)
        );
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
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", "{}", &mut content)
                .is_none()
        );
        assert_eq!(content, "short");
        // 关闭时超预算也不动。
        let off = ToolResultSpill {
            enabled: false,
            threshold_bytes: 4,
        };
        let mut big = "x".repeat(100);
        assert!(
            spill_if_over_budget(&off, dir.path(), "cli", "c", "read", "{}", &mut big).is_none()
        );
        assert_eq!(big.len(), 100);
    }

    #[test]
    fn reading_an_artifact_back_is_not_spilled_again() {
        // 回归：按句柄分页读 artifact，一页超过预算曾被再次落盘成新 artifact + 新句柄，
        // 取回永远到不了内容本身。
        let dir = tempfile::tempdir().unwrap();
        let spill = spill_on(64);
        let mut first = "line\n".repeat(100);
        let path = spill_if_over_budget(&spill, dir.path(), "cli", "c", "grep", "{}", &mut first)
            .expect("首次超预算应落盘");
        assert!(first.contains("建议每页 limit"), "{first}");
        // 模型照抄句柄路径（正斜杠形态）去 read：结果原样保留。
        let display_path = path.to_string_lossy().replace('\\', "/");
        let input = serde_json::json!({"path": display_path, "offset": 0, "limit": 50}).to_string();
        let mut page = "line\n".repeat(50);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &input, &mut page)
                .is_none()
        );
        assert_eq!(page, "line\n".repeat(50));
        // 反斜杠形态 / 大小写差异同样识别。
        let windows_style = display_path.replace('/', "\\").to_ascii_uppercase();
        let input = serde_json::json!({"path": windows_style}).to_string();
        let mut page = "line\n".repeat(50);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &input, &mut page)
                .is_none()
        );
        // 读别的大文件照常落盘；非 read 工具带 artifacts 路径也照常落盘。
        let other = serde_json::json!({"path": dir.path().join("src/big.rs").to_string_lossy()})
            .to_string();
        let mut big = "x".repeat(500);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &other, &mut big)
                .is_some()
        );
        let shell = serde_json::json!({"command": format!("cat {display_path}")}).to_string();
        let mut out = "x".repeat(500);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "shell", &shell, &mut out)
                .is_some()
        );
        // 入参不是 JSON（或缺 path）按普通调用处理。
        let mut odd = "x".repeat(500);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", "not json", &mut odd)
                .is_some()
        );
        // 回归：前缀长度落在中文分量内部时字符串切片会 panic（整条 turn 被打崩）——
        // 非 artifact 的中文路径大文件照常落盘、不 panic。agent 根 + "docs/" 恰好与
        // 根 + "artifacts/" 同长，让 prefix.len() 切进「说明.md」的多字节字符里。
        let cjk = serde_json::json!({"path": dir.path().join("docs/说明说明说明说明.md").to_string_lossy()}).to_string();
        let mut big = "x".repeat(500);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &cjk, &mut big).is_some()
        );
        let short = serde_json::json!({"path": "说明"}).to_string();
        let mut big = "x".repeat(500);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &short, &mut big)
                .is_some()
        );
        // 相对 agent 根的句柄写法（read 同样拼到 base 下）也认作取回。
        let relative = serde_json::json!({"path": format!("artifacts/{}", path.file_name().unwrap().to_string_lossy())}).to_string();
        let mut page = "line\n".repeat(50);
        assert!(
            spill_if_over_budget(&spill, dir.path(), "cli", "c", "read", &relative, &mut page)
                .is_none()
        );
    }

    #[test]
    fn fold_keeps_multiline_content_byte_identical() {
        // 多行内容（源码 / 日志 / diff）全部行都在行宽内：落盘与原文逐字节一致——
        // 句柄绝不能让模型把原始行拼接起来（那会毁掉源码 / 补丁）。
        let source = "fn main() {\n    println!(\"hi\");\n}\n";
        assert_eq!(fold_long_lines(source), source);
        // 单行超长内容被折行，拼接各行即原文（原文本就没有换行）。
        let single = "y".repeat(5000);
        let folded = fold_long_lines(&single);
        assert!(folded.contains('\n'));
        assert_eq!(folded.replace('\n', ""), single);
        // 混合：短行原样、超长行折行。
        let mixed = format!("short line\n{}\nlast line", "z".repeat(5000));
        let folded = fold_long_lines(&mixed);
        assert!(folded.starts_with("short line\n"));
        assert!(folded.ends_with("\nlast line"));
        assert_eq!(folded.replace('\n', ""), mixed.replace('\n', ""));
    }

    #[test]
    fn over_budget_content_is_replaced_with_preview_and_path() {
        let dir = tempfile::tempdir().unwrap();
        let spill = spill_on(1024);
        let head = "HEAD LINE\n".repeat(20); // 200 字节
        let middle = "m".repeat(8192);
        let tail = "\nTAIL LINE\n".repeat(20);
        let mut content = format!("{head}{middle}{tail}");
        let original = content.clone();
        let path = spill_if_over_budget(
            &spill,
            dir.path(),
            "cli",
            "console",
            "grep",
            "{}",
            &mut content,
        )
        .expect("超预算应落盘");
        // 落盘的是【全文】（折叠形态：原始行结构保留，仅超长行折行）。
        let disk = std::fs::read_to_string(&path).unwrap();
        assert!(disk.starts_with("HEAD LINE\n"));
        assert!(
            disk.replace('\n', "").contains(&middle),
            "折行后的文件去掉补入的换行必须能拼回中段"
        );
        assert!(disk.ends_with("TAIL LINE\n"));
        // 落盘按 ~2KB 折行：read 的行式分页才能取回任意段（受限子代理没有 shell）。
        for line in disk.lines() {
            assert!(
                line.len() <= SPILL_LINE_BYTES + 4,
                "每行不超过行宽上限: {}",
                line.len()
            );
        }
        // 原始短行原样保留（fold 只动超长行，任何内容字节都不改）。
        assert!(disk.contains("HEAD LINE\nHEAD LINE\n"));
        assert_eq!(
            disk.replace('\n', ""),
            original.replace('\n', ""),
            "fold 不得改动任何内容字节"
        );
        assert!(disk.ends_with("TAIL LINE\n"));
        // 模型收到的是小句柄：路径 + 头尾预览 + 中段省略标记。
        let display_path = path.to_string_lossy().replace('\\', "/");
        assert!(content.contains(&display_path), "{content}");
        assert!(content.contains("HEAD LINE"));
        assert!(content.contains("TAIL LINE"));
        assert!(content.contains("bytes omitted"), "{content}");
        assert!(!content.contains(&middle), "中段不得整段进入句柄");
        assert!(content.len() < PREVIEW_BYTES + 900, "{}", content.len());
        // 句柄必须给出读取指引（read 行分页；文件已分行，单行内容也能取回）。
        assert!(content.contains("offset/limit"), "{content}");
    }

    #[test]
    fn artifacts_are_bucketed_per_chat_key() {
        let dir = tempfile::tempdir().unwrap();
        let spill = spill_on(16);
        let mut a = "a".repeat(100);
        let mut b = "b".repeat(100);
        let pa = spill_if_over_budget(&spill, dir.path(), "cli", "chat-a", "read", "{}", &mut a)
            .unwrap();
        let pb = spill_if_over_budget(&spill, dir.path(), "cli", "chat-b", "read", "{}", &mut b)
            .unwrap();
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
