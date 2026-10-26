use super::*;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct AgentTaskSpec {
    pub(super) name: String,
    pub(super) role: String,
    pub(super) task: String,
    pub(super) context: String,
    pub(super) team_name: Option<String>,
    pub(super) subagent_type: Option<String>,
    pub(super) split_reason: Option<String>,
    // 后台/可恢复子任务必须持久化来源通道，否则恢复后只能看到共享媒体沙箱，
    // 无法判断应生成哪个外部 channel 的 marker。default 兼容旧执行快照。
    #[serde(default)]
    pub(super) source_channel: String,
    /// 后台任务的发起会话（chat_id）：完成唤醒消息的路由目标（ZCode 移植 · Layer 4）。
    /// 与 source_channel 一样由运行时注入（`_rclaw_source_chat`），不暴露在工具 schema。
    #[serde(default)]
    pub(super) origin_chat_id: String,
    /// 子代理独立模型（ZCode 移植 · Layer 2b）：`[[llm.providers]]` 里的 provider 名。
    /// 运行期经 model_factory 解析；None / 嵌入方未注入 factory 时回落主 provider
    /// （名字合法性已在工具入参校验期把关）。
    #[serde(default)]
    pub(super) model: Option<String>,
    /// 计划步骤上下文（Layer 3）：由 PlanRun 构造步骤时注入，escalate 用它定位 journal。
    #[serde(default)]
    pub(super) plan_id: Option<String>,
    #[serde(default)]
    pub(super) step_id: Option<String>,
    pub(super) isolation: String,
    pub(super) max_tool_iter: usize,
    pub(super) max_output_chars: usize,
}

#[derive(Clone)]
pub(super) struct AgentTaskResult {
    pub(super) name: String,
    pub(super) role: String,
    pub(super) is_error: bool,
    pub(super) cancelled: bool,
    pub(super) completed_steps: usize,
    pub(super) handoff: StructuredHandoff,
    pub(super) content: String,
    pub(super) validated_media_markers: Vec<String>,
    pub(super) media_publish_attempted: bool,
    /// 子代理仍未解决的副作用工具错误的运行时披露块（空串 = 无）。独立于 content：
    /// 结构化 handoff 渲染不输出 content，max_output_chars 截断可能丢尾部——渲染层
    /// 在两者之后重新附加，保证父代理一定看到（codex 第 2 轮 #1）。
    pub(super) unresolved_disclosure: String,
}

pub(super) fn invalidate_result_for_cancellation(result: &mut AgentTaskResult) {
    let reason = "任务在成功结果提交前收到取消请求，结果和媒体交付均未提交".to_string();
    result.is_error = false;
    result.cancelled = true;
    result.content = reason.clone();
    result.handoff.summary = reason;
    result.handoff.evidence.clear();
    result.handoff.artifacts.clear();
    result.handoff.validation.clear();
    result.handoff.open_risks = vec!["任务在完成提交前被取消".to_string()];
    result.media_publish_attempted |= !result.validated_media_markers.is_empty();
    result.validated_media_markers.clear();
    // 与 marker 同口径：取消终态不得携带成功路径的披露（取消方须重新发起任务）。
    result.unresolved_disclosure.clear();
}

pub(super) enum RunTaskFailure {
    Cancelled {
        reason: String,
        completed_steps: usize,
        media_publish_attempted: bool,
    },
    Failed {
        error: String,
        completed_steps: usize,
        media_publish_attempted: bool,
    },
}

pub(super) fn parse_agent_task(input: &Value, idx: usize) -> Result<AgentTaskSpec, String> {
    let task = get_str(input, "task")
        .or_else(|| get_str(input, "prompt"))
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| format!("tasks[{idx}].task/prompt 缺失或为空"))?
        .to_string();
    let description = get_str(input, "description").unwrap_or_default();
    let coding_material = format!(
        "{task}\n{description}\n{}\n{}",
        get_str(input, "role").unwrap_or_default(),
        get_str(input, "subagent_type").unwrap_or_default()
    );
    let isolation = if looks_like_complex_coding_task(&coding_material) {
        "candidate_patch".to_string()
    } else {
        "shared_cas".to_string()
    };
    Ok(AgentTaskSpec {
        name: get_str(input, "name")
            .or_else(|| {
                if description.trim().is_empty() {
                    None
                } else {
                    Some(description)
                }
            })
            .map(str::to_string)
            .unwrap_or_else(|| format!("agent-{idx}")),
        role: get_str(input, "role")
            .or_else(|| get_str(input, "subagent_type"))
            .unwrap_or_default()
            .to_string(),
        task,
        context: get_str(input, "context").unwrap_or_default().to_string(),
        team_name: get_str(input, "team_name").map(str::to_string),
        subagent_type: get_str(input, "subagent_type").map(str::to_string),
        split_reason: get_str(input, "split_reason").map(str::to_string),
        source_channel: get_str(input, "_rclaw_source_channel")
            .unwrap_or_default()
            .to_string(),
        origin_chat_id: get_str(input, "_rclaw_source_chat")
            .unwrap_or_default()
            .to_string(),
        model: get_str(input, "model")
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string),
        plan_id: None,
        step_id: None,
        isolation,
        max_tool_iter: get_usize(input, "max_tool_iter", DEFAULT_MAX_TOOL_ITER, MAX_TOOL_ITER)?,
        max_output_chars: get_usize(
            input,
            "max_output_chars",
            DEFAULT_MAX_OUTPUT_CHARS,
            MAX_OUTPUT_CHARS,
        )?,
    })
}

pub(super) fn get_str<'a>(input: &'a Value, key: &str) -> Option<&'a str> {
    input.get(key).and_then(|v| v.as_str())
}

pub(super) fn get_bool(input: &Value, key: &str) -> bool {
    input.get(key).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub(super) fn message_to_string(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

pub(super) fn get_usize(
    input: &Value,
    key: &str,
    default: usize,
    max: usize,
) -> Result<usize, String> {
    let Some(v) = input.get(key) else {
        return Ok(default);
    };
    let Some(n) = v.as_u64() else {
        return Err(format!("{key} 必须是正整数"));
    };
    if n == 0 {
        return Err(format!("{key} 必须大于 0"));
    }
    let n = usize::try_from(n).map_err(|_| format!("{key} 过大"))?;
    Ok(n.min(max))
}

pub(super) fn filtered_tool_schemas(
    schemas: &[ToolSchema],
    candidate_patch_only: bool,
) -> Vec<ToolSchema> {
    schemas
        .iter()
        .filter(|schema| {
            !is_subagent_blocked_tool(&schema.name)
                && (!candidate_patch_only || is_candidate_patch_tool_allowed(&schema.name))
        })
        .cloned()
        .collect()
}

pub(super) fn looks_like_complex_coding_task(task: &str) -> bool {
    let lower = task.to_ascii_lowercase();
    [
        "refactor",
        "fix bug",
        "add feature",
        "modify code",
        "change code",
        "update code",
        "debug",
        "unit test",
        "integration test",
        "code change",
        "write code",
        "coder",
        "implement function",
        "implement class",
        "implement module",
        "implement command",
        "implement cli",
        "implement api",
        "修改代码",
        "实现功能",
        "实现函数",
        "实现接口",
        "实现模块",
        "实现命令",
        "重构",
        "修复代码",
        "新增功能",
        "添加测试",
        "单元测试",
        "修复 bug",
        "修复bug",
        "编写代码",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub(super) fn build_assistant_message(
    outcome: &rclaw_core::ChatOutcome,
) -> Result<Message, String> {
    let mut blocks = Vec::new();
    if !outcome.text.is_empty() {
        blocks.push(ContentBlock::text(outcome.text.clone()));
    }
    if let Some(r) = &outcome.reasoning {
        if !r.is_empty() {
            blocks.push(ContentBlock::reasoning(r));
        }
    }
    for tc in &outcome.tool_calls {
        let input: Value = serde_json::from_str(&tc.input)
            .map_err(|e| format!("工具 {} 入参不是合法 JSON: {e}", tc.name))?;
        blocks.push(ContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.name.clone(),
            input,
        });
    }
    Ok(Message {
        id: 0,
        role: Role::Assistant,
        content: blocks,
        tool_call_id: None,
        ts: 0,
    })
}

pub(super) struct SubagentToolContext<'a> {
    pub(super) registry: &'a ToolRegistry,
    pub(super) supports_multimodal: bool,
    pub(super) source_channel: &'a str,
    pub(super) layout: &'a Layout,
    pub(super) candidate_patch_only: bool,
    pub(super) channels: &'a rclaw_core::RuntimeChannelInfo,
    pub(super) state: &'a mut TurnState,
    pub(super) catalog: &'a SkillCatalog,
    /// 工具结果落盘配置（来自 AgentToolRuntime 的 AgentOptions；子代理与前台同一套阈值）。
    pub(super) opts: &'a crate::options::AgentOptions,
}

pub(super) async fn execute_subagent_tools(
    outcome: &rclaw_core::ChatOutcome,
    context: SubagentToolContext<'_>,
) -> crate::turn::ToolExecutionMessages {
    execute_tool_batch(
        outcome,
        ToolExecutionContext {
            registry: context.registry,
            layout: context.layout,
            channels: context.channels,
            state: context.state,
            catalog: context.catalog,
            policy: ToolExecutionPolicy::Subagent {
                source_channel: context.source_channel,
                candidate_patch_only: context.candidate_patch_only,
                supports_multimodal: context.supports_multimodal,
            },
            spill: crate::turn::artifact::ToolResultSpill::from_options(context.opts),
        },
    )
    .await
}

pub(super) fn parse_structured_handoff(
    text: &str,
    spec: &AgentTaskSpec,
    max_chars: usize,
) -> StructuredHandoff {
    let value = text
        .find('{')
        .zip(text.rfind('}'))
        .and_then(|(start, end)| (start <= end).then(|| &text[start..=end]))
        .and_then(|candidate| serde_json::from_str::<Value>(candidate).ok());
    let string_list = |key: &str| {
        value
            .as_ref()
            .and_then(|value| value.get(key))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    let summary = value
        .as_ref()
        .and_then(|value| value.get("summary"))
        .and_then(Value::as_str)
        .filter(|summary| !summary.trim().is_empty())
        .unwrap_or(text)
        .trim()
        .to_string();
    bound_structured_handoff(
        StructuredHandoff {
            goal: spec.task.clone(),
            summary,
            evidence: string_list("evidence"),
            artifacts: string_list("artifacts"),
            open_risks: string_list("open_risks"),
            validation: string_list("validation"),
        },
        max_chars,
    )
}

pub(super) fn bound_structured_handoff(
    mut handoff: StructuredHandoff,
    max_chars: usize,
) -> StructuredHandoff {
    let budget = max_chars.max(1_000);
    handoff.goal = truncate_chars(&handoff.goal, (budget / 8).clamp(200, 2_000));
    handoff.summary = truncate_chars(&handoff.summary, (budget / 2).max(500));
    let mut remaining = budget
        .saturating_sub(handoff.goal.chars().count())
        .saturating_sub(handoff.summary.chars().count());
    for items in [
        &mut handoff.evidence,
        &mut handoff.artifacts,
        &mut handoff.open_risks,
        &mut handoff.validation,
    ] {
        items.truncate(32);
        let mut bounded = Vec::new();
        for item in items.drain(..) {
            if remaining == 0 {
                break;
            }
            let item = truncate_chars(&item, remaining.min(2_000));
            remaining = remaining.saturating_sub(item.chars().count());
            bounded.push(item);
        }
        *items = bounded;
    }
    handoff
}

pub(super) fn strip_untrusted_media_control_lines(text: &str) -> String {
    text.lines()
        .filter(|line| !is_media_control_line(line))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn append_validated_media_state(
    output: &mut String,
    markers: &[String],
    media_publish_attempted: bool,
) {
    for marker in markers {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("RCLAW_VALIDATED_MEDIA_MARKER=");
        output.push_str(marker);
        output.push('\n');
    }
    if media_publish_attempted {
        if !output.ends_with('\n') {
            output.push('\n');
        }
        output.push_str("RCLAW_MEDIA_PUBLISH_ATTEMPTED=1\n");
    }
}

pub(super) fn render_execution_media_payload(payload: Value) -> String {
    let markers = payload
        .pointer("/execution/validated_media_markers")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let attempted = payload
        .pointer("/execution/media_publish_attempted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut output = payload.to_string();
    append_validated_media_state(&mut output, &markers, attempted);
    // 运行时披露最后附加：父代理经 AgentStatus／AgentWait 轮询无团队后台任务时，
    // 快照 JSON 里的披露字段可能被截断显示或淹没——渲染层保证可见（codex 第 3 轮 P1）。
    if let Some(disclosure) = payload
        .pointer("/execution/unresolved_disclosure")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        output.push_str("\n\n");
        output.push_str(disclosure);
    }
    output
}

pub(super) fn render_single_result(result: &AgentTaskResult) -> String {
    render_single_result_inner(result, true)
}

fn render_single_result_inner(result: &AgentTaskResult, with_disclosure: bool) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# {}\n\n",
        if result.name.trim().is_empty() {
            "sub-agent"
        } else {
            result.name.trim()
        }
    ));
    if !result.role.trim().is_empty() {
        out.push_str(&format!("role: {}\n\n", result.role.trim()));
    }
    if result.cancelled {
        out.push_str("[cancelled]\n");
    } else if result.is_error {
        out.push_str("[error]\n");
    }
    out.push_str(&format!(
        "goal: {}\n\nsummary:\n{}\n",
        result.handoff.goal.trim(),
        result.handoff.summary.trim()
    ));
    for (title, items) in [
        ("evidence", &result.handoff.evidence),
        ("artifacts", &result.handoff.artifacts),
        ("open risks", &result.handoff.open_risks),
        ("validation", &result.handoff.validation),
    ] {
        out.push_str(&format!("\n{title}:\n"));
        if items.is_empty() {
            out.push_str("- none\n");
        } else {
            for item in items {
                out.push_str(&format!("- {}\n", item.trim()));
            }
        }
    }
    let mut out = strip_untrusted_media_control_lines(&out);
    append_validated_media_state(
        &mut out,
        &result.validated_media_markers,
        result.media_publish_attempted,
    );
    // 运行时披露最后附加（截断、结构化渲染、控制行剥离之后）：子代理仍未解决的
    // 副作用错误必须越过 handoff 渲染到达父代理——open_risks 由模型自填，不构成
    // 运行时保证（codex 第 2 轮 #1）。
    if with_disclosure && !result.unresolved_disclosure.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&result.unresolved_disclosure);
    }
    out
}

pub(super) fn render_foreground_result(result: &AgentTaskResult) -> String {
    // 披露不在 handoff 渲染段附加，而是放在【整份文本的最末尾】：前台 full output
    // 可能触发统一 spill（头尾 4KiB 预览、中段省略），披露夹在中段会被预览裁掉
    // （codex 第 6 轮 P1）。
    let mut out = render_single_result_inner(result, false);
    let sanitized_content = strip_untrusted_media_control_lines(&result.content);
    let content = sanitized_content.trim();
    if !content.is_empty() && content != result.handoff.summary.trim() {
        out.push_str("\nfull output:\n");
        out.push_str(content);
        out.push('\n');
    }
    if !result.unresolved_disclosure.is_empty() {
        if !out.trim().is_empty() {
            out.push_str("\n\n");
        }
        out.push_str(&result.unresolved_disclosure);
    }
    out
}

/// `truncate_chars` 截断后追加的标记：下游（plan verdict 解析）据此判断输出是否完整。
pub(super) const OUTPUT_TRUNCATED_MARKER: &str = "\n\n[已按 max_output_chars 截断]";

pub(super) fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = s.chars().take(max_chars).collect::<String>();
    out.push_str(OUTPUT_TRUNCATED_MARKER);
    out
}

/// 非模型输出的文本（任务文本、依赖摘要、提问、诊断、报告摘录）被裁短时的后缀。
/// 不复用 `OUTPUT_TRUNCATED_MARKER`：那个标记专指子代理【输出】按 max_output_chars 被截，
/// plan 的 verdict 解析靠它判定验证者输出是否完整——一段被裁短的依赖摘要带上它，读起来
/// 就成了「输出被截断」。
pub(super) const TEXT_TRUNCATED_SUFFIX: &str = "…[截断]";

/// 按字符数裁短非输出文本，超长时追加 [`TEXT_TRUNCATED_SUFFIX`]。
pub(super) fn truncate_text(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out = s.chars().take(max_chars).collect::<String>();
    out.push_str(TEXT_TRUNCATED_SUFFIX);
    out
}
