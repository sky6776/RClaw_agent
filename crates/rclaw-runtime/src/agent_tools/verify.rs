//! candidate_patch 子代理的可选交叉验证（只读 verify pass）。
//!
//! RClaw 的三层完成门禁是【自检】——声明与回执由同一个模型闭环核对。这里补一条
//! 【交叉】路径：编码类候选补丁完成后，由一个独立的小型只读循环（read/list/grep）
//! 去真实文件系统核对结构化交接（引用文件是否存在、diff 与真实代码是否对得上、
//! validation 声明是否可信），结论附在 handoff 之后交给父代理裁决。
//!
//! 约束（与 JiuwenSwarm reviewer 角色对齐，但保持 RClaw 的确定性哲学）：
//! - 只读：工具面白名单 read/list/grep，其它一律拒绝；
//! - 有界：最多 [`VERIFY_MAX_ITERS`] 次推理、单次 [`VERIFY_TIMEOUT_SECS`] 超时；
//! - 不阻塞交付：验证超时 / 模型失败 / 未收敛一律返回 `None`，不扣留子代理结果；
//! - 复用子代理的推理错峰（429 保护）与工具沙箱（task-local fail-safe Sandboxed）。

use super::*;

/// verify pass 的最大推理轮数。
const VERIFY_MAX_ITERS: usize = 4;
/// 单次推理超时；整个 pass 的实际上限 ≈ 轮数 × 该值。
const VERIFY_TIMEOUT_SECS: u64 = 120;
/// 验证结论的字符上限（附在 handoff 之后，不挤占父代理上下文）。
const VERIFY_VERDICT_MAX_CHARS: usize = 2000;

/// 运行一次交叉验证，返回结论文本；`None` = 无法得出结论（不附言）。
pub(super) async fn verify_candidate_patch(
    runtime: &AgentToolRuntime,
    spec: &AgentTaskSpec,
    registry: &ToolRegistry,
    handoff_text: &str,
) -> Option<String> {
    let mut schemas: Vec<ToolSchema> = registry
        .request_schemas()
        .iter()
        .filter(|schema| matches!(schema.name.as_str(), "read" | "list" | "grep"))
        .cloned()
        .collect();
    schemas.sort_by(|a, b| a.name.cmp(&b.name));
    if schemas.len() != 3 {
        // 注册表缺只读工具（被禁用 / 嵌入方裁剪）：交叉验证不可用，静默跳过。
        tracing::debug!("交叉验证跳过：只读工具面不完整");
        return None;
    }
    let system = "You are RClaw's read-only cross-verification reviewer for a candidate patch \
         produced by a coding sub-agent. You may ONLY use read / list / grep. Verify the \
         handoff's claims against the real files: do referenced paths exist, do quoted code \
         fragments match the actual sources, are the stated validation steps plausible for \
         this codebase? Be strict about facts, but do not run anything or propose new work. \
         Finish with exactly one line starting with VERIFIED: or ISSUES: followed by concise \
         bullet points (in the language of the task)."
        .to_string();
    let user = format!(
        "## Original task\n{}\n\n## Candidate handoff to verify\n{}\n\n\
         Verify the handoff claims now with read/list/grep, then output the verdict line.",
        spec.task.trim(),
        handoff_text.trim(),
    );
    let mut messages = vec![Message::plain(Role::User, user)];
    for iter in 0..VERIFY_MAX_ITERS {
        if runtime
            .wait_for_inference_slot(None, iter, false)
            .await
            .is_err()
        {
            return None;
        }
        let req = ChatRequest {
            system: Some(system.clone()),
            messages: messages.clone(),
            tools: schemas.clone(),
            max_tokens: runtime.provider.max_tokens(),
            stream: false,
        };
        let provider = runtime.provider.clone();
        let outcome = match tokio::time::timeout(
            std::time::Duration::from_secs(VERIFY_TIMEOUT_SECS),
            provider.chat(req),
        )
        .await
        {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(error)) => {
                tracing::debug!("交叉验证推理失败（不阻塞交付）: {error}");
                return None;
            }
            Err(_) => {
                tracing::debug!("交叉验证推理超时（不阻塞交付）");
                return None;
            }
        };
        if !outcome.wants_tools() {
            let verdict = outcome.text.trim();
            if verdict.is_empty() || !verdict.contains("VERIFIED") && !verdict.contains("ISSUES") {
                return None;
            }
            return Some(truncate_verdict(verdict));
        }
        let assistant = match build_assistant_message(&outcome) {
            Ok(message) => message,
            Err(_) => return None,
        };
        let mut blocks = Vec::with_capacity(outcome.tool_calls.len());
        for tc in &outcome.tool_calls {
            let result = if matches!(tc.name.as_str(), "read" | "list" | "grep") {
                let input: Value = serde_json::from_str(&tc.input).unwrap_or(Value::Null);
                registry.execute(&tc.name, input).await
            } else {
                ToolResult::err(format!(
                    "verification is read-only; tool `{}` is not allowed",
                    tc.name
                ))
            };
            blocks.push(ContentBlock::ToolResult {
                tool_use_id: tc.id.clone(),
                content: rclaw_store::redact_sensitive_text(&result.content),
                is_error: result.is_error,
            });
        }
        messages.push(assistant);
        messages.push(Message {
            role: Role::User,
            content: blocks,
            tool_call_id: None,
            ts: 0,
        });
    }
    // 未在轮数内收敛：验证不成立，但不阻塞交付。
    tracing::debug!("交叉验证未在 {VERIFY_MAX_ITERS} 轮内收敛（不附言）");
    None
}

fn truncate_verdict(verdict: &str) -> String {
    if verdict.chars().count() <= VERIFY_VERDICT_MAX_CHARS {
        return verdict.to_string();
    }
    let mut out: String = verdict.chars().take(VERIFY_VERDICT_MAX_CHARS).collect();
    out.push_str("\n…（验证结论已截断）");
    out
}
