//! 工具调用的结构化观察事件（桌面 / 安卓会话面板「工具调用可展开观察」的直播通道）。
//!
//! 与持久化的关系：事件只负责「正在发生」的直播；事实源是落库的 assistant(tool_calls)
//! 行 + tool_result 载体 + `tool_call_meta` 侧表（`SessionStore::load_tool_chain_detail`）。
//! [`ToolBatchFinished::assistant_msg_id`] 把直播条目盖章成持久化条目——重启 / 重装后
//! 面板从库里重建的是同一个条目，宿主无需再维护独立的事件日志。
//!
//! 载荷按「列表级」裁剪：入参里的长字符串截到 [`BRIEF_STRING_LIMIT`]、容器元素截到
//! [`BRIEF_ITEMS_LIMIT`]，结果正文截到 [`RESULT_PREVIEW_LIMIT`]；完整内容在展开面板
//! 时按行 id 懒加载。桌面历史 DTO 复用同一裁剪（[`brief_input`]），直播与回放同形。

use serde::Serialize;
use serde_json::Value;

use rclaw_core::ContentBlock;
use rclaw_store::ToolCallMetaRecord;

/// 列表级入参里单个字符串的上限（字符数）。write 的整文件内容 / edit 的大段新旧文本
/// 超过它就截断，展开时懒加载全文。
pub const BRIEF_STRING_LIMIT: usize = 8 * 1024;
/// 列表级入参里数组 / 对象的元素上限。
pub const BRIEF_ITEMS_LIMIT: usize = 64;
/// 直播结果预览的上限（字符数）。
pub const RESULT_PREVIEW_LIMIT: usize = 8 * 1024;

/// 一次工具调用开始。
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallBegun {
    /// `ToolUse.id`（同一批内唯一；跨批会重复，宿主以批次 / 行 id 区分）。
    pub id: String,
    pub name: String,
    /// 列表级入参（已按 [`brief_input`] 裁剪）。
    pub input: Value,
    /// 入参被裁剪过：展开面板需要懒加载全文。
    pub input_truncated: bool,
    /// 调用开始即可知的 Skill 归属（Manifest 专用工具 → 所属 skill）；`read` 触发的
    /// Skill 激活在结束事件里补齐。
    pub skill: Option<String>,
}

/// 一批工具调用开始（= 一次 ReAct 迭代返回的全部 tool_calls）。
#[derive(Debug, Clone, Serialize)]
pub struct ToolBatchBegun {
    /// 从 1 起的迭代号（与「正在调用工具(N)」文案同口径）。
    pub iter: u32,
    pub calls: Vec<ToolCallBegun>,
}

/// 一次工具调用结束。
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallFinished {
    pub id: String,
    pub is_error: bool,
    /// 调用被 /cancel 打断、没有拿到结果（结果块是 aborted 占位）：面板显示「已中断」
    /// 而不是「失败」。
    pub aborted: bool,
    /// 结果正文预览（按 [`RESULT_PREVIEW_LIMIT`] 截断）。
    pub preview: String,
    pub truncated: bool,
    pub duration_ms: Option<u64>,
    pub skill: Option<String>,
}

/// 一批工具调用结束并（前台路径）已落库。取消清理把中断链闭合落库后同样上报一次
/// （结果全为 aborted 占位）——直播条目由此拿到行 id，展开时能懒加载被打断前已完成
/// 调用的入参 / 写入前快照。
#[derive(Debug, Clone, Serialize)]
pub struct ToolBatchFinished {
    pub iter: u32,
    /// 已落库的 assistant(tool_calls) 行 id —— 持久化条目的身份，展开面板据此懒加载
    /// 全量详情；cron 等不持久化消息的路径为 `None`。
    pub assistant_msg_id: Option<i64>,
    pub results: Vec<ToolCallFinished>,
}

impl ToolBatchFinished {
    /// 由结果载体的 `ToolResult` 块与侧表记录组装。块顺序 = 调用顺序；侧表记录与结果块
    /// 同源同序（工具批执行 / `from_result_blocks` / 取消清理都按结果顺序产出），长度一致
    /// 时按位置配对——模型在同一批里重复 tool_use_id 时按 id 找会把首条的耗时 / skill
    /// 配给后面的调用；长度不一致（熔断合成结果、旧行）退回按 id，缺失时耗时 / skill 为空。
    pub fn from_results(
        iter: u32,
        assistant_msg_id: Option<i64>,
        result_blocks: &[ContentBlock],
        metas: &[ToolCallMetaRecord],
    ) -> Self {
        let positional = metas.len() == result_blocks.len();
        let results = result_blocks
            .iter()
            .enumerate()
            .filter_map(|(index, block)| match block {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let meta = positional
                        .then(|| metas.get(index))
                        .flatten()
                        .filter(|m| &m.tool_use_id == tool_use_id)
                        .or_else(|| metas.iter().find(|m| &m.tool_use_id == tool_use_id));
                    let (preview, truncated) = preview_text(content, RESULT_PREVIEW_LIMIT);
                    Some(ToolCallFinished {
                        id: tool_use_id.clone(),
                        is_error: *is_error,
                        aborted: meta.is_some_and(|m| m.aborted),
                        preview,
                        truncated,
                        duration_ms: meta.and_then(|m| m.duration_ms),
                        skill: meta.and_then(|m| m.skill.clone()),
                    })
                }
                _ => None,
            })
            .collect();
        ToolBatchFinished {
            iter,
            assistant_msg_id,
            results,
        }
    }
}

/// 列表级入参裁剪：长字符串截到 [`BRIEF_STRING_LIMIT`] 字符并加省略标记，数组 / 对象只
/// 保留前 [`BRIEF_ITEMS_LIMIT`] 个元素；返回 `(裁剪后的值, 是否裁剪过)`。标量与短
/// 字符串原样返回。
pub fn brief_input(input: &Value) -> (Value, bool) {
    let mut truncated = false;
    let value = brief_value(input, &mut truncated);
    (value, truncated)
}

fn brief_value(value: &Value, truncated: &mut bool) -> Value {
    match value {
        Value::String(s) => {
            let (text, cut) = preview_text(s, BRIEF_STRING_LIMIT);
            if cut {
                *truncated = true;
                Value::String(format!("{text}…（已截断，展开查看全文）"))
            } else {
                value.clone()
            }
        }
        Value::Array(items) => {
            if items.len() > BRIEF_ITEMS_LIMIT {
                *truncated = true;
            }
            Value::Array(
                items
                    .iter()
                    .take(BRIEF_ITEMS_LIMIT)
                    .map(|item| brief_value(item, truncated))
                    .collect(),
            )
        }
        Value::Object(map) => {
            if map.len() > BRIEF_ITEMS_LIMIT {
                *truncated = true;
            }
            Value::Object(
                map.iter()
                    .take(BRIEF_ITEMS_LIMIT)
                    .map(|(k, v)| (k.clone(), brief_value(v, truncated)))
                    .collect(),
            )
        }
        _ => value.clone(),
    }
}

/// 字符安全截断：取前 `limit` 个字符；返回 `(文本, 是否截断)`。
pub fn preview_text(content: &str, limit: usize) -> (String, bool) {
    let mut chars = content.char_indices();
    match chars.nth(limit) {
        Some((cut, _)) => (content[..cut].to_string(), true),
        None => (content.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_inputs_pass_through_untouched() {
        let input = json!({"path": "a.rs", "lines": [1, 2, 3], "n": 7, "ok": true});
        let (out, truncated) = brief_input(&input);
        assert_eq!(out, input);
        assert!(!truncated);
    }

    #[test]
    fn long_strings_are_cut_and_flagged() {
        let long = "字".repeat(BRIEF_STRING_LIMIT + 5);
        let (out, truncated) = brief_input(&json!({"content": long}));
        assert!(truncated);
        let text = out["content"].as_str().unwrap();
        assert!(text.starts_with(&"字".repeat(BRIEF_STRING_LIMIT)));
        assert!(text.ends_with("展开查看全文）"));
    }

    #[test]
    fn oversized_arrays_are_trimmed() {
        let items: Vec<Value> = (0..(BRIEF_ITEMS_LIMIT + 1)).map(|i| json!(i)).collect();
        let (out, truncated) = brief_input(&Value::Array(items));
        assert!(truncated);
        assert_eq!(out.as_array().unwrap().len(), BRIEF_ITEMS_LIMIT);
    }

    #[test]
    fn preview_text_is_char_safe() {
        let (text, cut) = preview_text("héllo", 2);
        assert_eq!(text, "hé");
        assert!(cut);
        let (text, cut) = preview_text("hi", 2);
        assert_eq!(text, "hi");
        assert!(!cut);
    }

    #[test]
    fn finished_batch_pairs_metas_by_id() {
        let blocks = vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_0".into(),
                content: "ok".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: "boom".into(),
                is_error: true,
            },
        ];
        let metas = vec![ToolCallMetaRecord {
            tool_use_id: "call_1".into(),
            skill: Some("pdf".into()),
            started_ms: Some(1),
            duration_ms: Some(42),
            is_error: true,
            aborted: false,
        }];
        let batch = ToolBatchFinished::from_results(3, Some(99), &blocks, &metas);
        assert_eq!(batch.iter, 3);
        assert_eq!(batch.assistant_msg_id, Some(99));
        assert_eq!(batch.results.len(), 2);
        assert_eq!(batch.results[0].duration_ms, None);
        assert_eq!(batch.results[1].duration_ms, Some(42));
        assert_eq!(batch.results[1].skill.as_deref(), Some("pdf"));
        assert!(batch.results[1].is_error);
        assert!(!batch.results[1].aborted);
    }

    #[test]
    fn finished_batch_pairs_duplicate_ids_by_position() {
        // 模型在同一批里重复 tool_use_id：侧表与结果块同序同长 → 按位置各配各的。
        let blocks = vec![
            ContentBlock::ToolResult {
                tool_use_id: "call_0".into(),
                content: "first".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "call_0".into(),
                content: "second".into(),
                is_error: false,
            },
        ];
        let meta = |duration: u64, skill: Option<&str>| ToolCallMetaRecord {
            tool_use_id: "call_0".into(),
            skill: skill.map(str::to_string),
            started_ms: Some(1),
            duration_ms: Some(duration),
            is_error: false,
            aborted: false,
        };
        let metas = vec![meta(10, None), meta(20, Some("pdf"))];
        let batch = ToolBatchFinished::from_results(1, Some(5), &blocks, &metas);
        assert_eq!(batch.results[0].duration_ms, Some(10));
        assert_eq!(batch.results[0].skill, None);
        assert_eq!(batch.results[1].duration_ms, Some(20));
        assert_eq!(batch.results[1].skill.as_deref(), Some("pdf"));
    }

    #[test]
    fn aborted_metas_mark_results_interrupted() {
        // 取消清理：占位结果 + aborted 侧表 → 结束事件逐条带 aborted，面板显示「已中断」。
        let ids = vec!["call_0".to_string(), "call_1".to_string()];
        let carrier = rclaw_store::synthesize_aborted_tool_results(&ids);
        let metas = ids
            .iter()
            .map(|id| ToolCallMetaRecord::aborted(id.clone()))
            .collect::<Vec<_>>();
        let batch = ToolBatchFinished::from_results(2, Some(7), &carrier.content, &metas);
        assert_eq!(batch.results.len(), 2);
        assert!(batch.results.iter().all(|r| r.aborted && r.is_error));
        assert_eq!(batch.results[1].id, "call_1");
    }
}
