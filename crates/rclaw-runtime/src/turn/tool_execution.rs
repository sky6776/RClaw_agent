use super::*;

/// 主 turn 与子 Agent 共用的工具执行策略。策略只描述两条路径确实不同的能力，
/// 解析、Skill 生命周期、动态工具重校验、媒体门禁和结果组装都由本模块统一拥有。
pub(crate) enum ToolExecutionPolicy<'a> {
    Foreground {
        message: &'a BusMessage,
        outbound: &'a mpsc::Sender<BusMessage>,
        send_working_status: bool,
    },
    Subagent {
        source_channel: &'a str,
        candidate_patch_only: bool,
        supports_multimodal: bool,
    },
}

impl ToolExecutionPolicy<'_> {
    fn source_channel(&self) -> &str {
        match self {
            Self::Foreground { message, .. } => &message.channel,
            Self::Subagent { source_channel, .. } => source_channel,
        }
    }

    /// 发起会话的 chat_id（后台完成唤醒的路由目标；Subagent 无后台任务能力，恒为空）。
    fn source_chat(&self) -> &str {
        match self {
            Self::Foreground { message, .. } => &message.chat_id,
            Self::Subagent { .. } => "",
        }
    }

    fn candidate_patch_only(&self) -> bool {
        matches!(
            self,
            Self::Subagent {
                candidate_patch_only: true,
                ..
            }
        )
    }

    fn supports_multimodal(&self) -> bool {
        match self {
            // 前台路径沿用既有行为；provider 在请求构造层负责其输入能力。
            Self::Foreground { .. } => true,
            Self::Subagent {
                supports_multimodal,
                ..
            } => *supports_multimodal,
        }
    }

    fn allow_skill_activation(&self) -> bool {
        !self.candidate_patch_only()
    }

    fn is_foreground(&self) -> bool {
        matches!(self, Self::Foreground { .. })
    }
}

pub(crate) struct ToolExecutionContext<'a> {
    pub(crate) registry: &'a ToolRegistry,
    pub(crate) layout: &'a Layout,
    pub(crate) channels: &'a rclaw_core::RuntimeChannelInfo,
    pub(crate) state: &'a mut TurnState,
    pub(crate) catalog: &'a SkillCatalog,
    pub(crate) policy: ToolExecutionPolicy<'a>,
}

/// 执行一整批工具调用，并生成下一次模型请求所需的规范消息。
pub(crate) async fn execute_tool_batch(
    outcome: &rclaw_core::ChatOutcome,
    context: ToolExecutionContext<'_>,
) -> ToolExecutionMessages {
    let ToolExecutionContext {
        registry,
        layout,
        channels,
        state,
        catalog,
        policy,
    } = context;
    let mut slots: Vec<Option<ContentBlock>> =
        (0..outcome.tool_calls.len()).map(|_| None).collect();
    let mut media_blocks = Vec::new();
    let mut package_skill_contexts = Vec::new();
    state.begin_batch();
    let mut validated_media_markers = Vec::new();
    let skill_manage_preflight = preflight_skill_manage_batch(&outcome.tool_calls);
    let callable_dynamic_tools = state
        .dynamic
        .active_names()
        .cloned()
        .collect::<HashSet<_>>();

    // ---- A4 并行门控：全部调用均 parallel_safe 时整批并发，否则串行 ----
    //
    // 门控规则（借鉴 Codex tools/parallel.rs）：一批调用【混入任一】不可并行的工具
    // （写文件、shell、cron、动态 Manifest 工具、被策略封锁的工具等）→ 整批退回
    // 串行路径；全部 parallel_safe（read / list / grep / web_search 家族）→ 并发执行。
    let batch_parallel_safe = outcome.tool_calls.len() > 1
        && skill_manage_preflight.is_empty()
        && outcome.tool_calls.iter().enumerate().all(|(index, tc)| {
            !is_parallel_agent_tool(&tc.name)
                && !callable_dynamic_tools.contains(&tc.name)
                && !state.dynamic.contains(&tc.name)
                && !is_policy_blocked_tool(&policy, &tc.name)
                && !skill_manage_preflight.contains_key(&index)
                && registry.is_parallel_safe(&tc.name)
        });

    if batch_parallel_safe {
        // 快速路径：顺序完成入参解析与 read 的 Skill 公告，然后并发执行、按槽位
        // 顺序回填并施加状态副作用（Skill 激活 / 附件 / 脱敏），与串行结果一致。
        let mut parsed_inputs: Vec<Option<Value>> =
            (0..outcome.tool_calls.len()).map(|_| None).collect();
        let mut invocations_per_slot: Vec<Vec<SkillInvocation>> =
            (0..outcome.tool_calls.len()).map(|_| Vec::new()).collect();
        for (index, tc) in outcome.tool_calls.iter().enumerate() {
            match serde_json::from_str(&tc.input) {
                Ok(value) => parsed_inputs[index] = Some(value),
                Err(_) => {
                    slots[index] = Some(rejected_call(tc, "工具入参不是合法 JSON，已拒绝执行"));
                }
            }
        }
        for (index, tc) in outcome.tool_calls.iter().enumerate() {
            if parsed_inputs[index].is_none() || tc.name != "read" {
                continue;
            }
            if !policy.allow_skill_activation() {
                continue;
            }
            invocations_per_slot[index] = announce_skill_invocations(
                &policy,
                channels,
                std::slice::from_ref(tc),
                catalog,
                &mut state.announced_skill_paths,
            )
            .await;
        }
        let futures = outcome
            .tool_calls
            .iter()
            .enumerate()
            .filter(|(index, _)| parsed_inputs[*index].is_some())
            .map(|(index, tc)| {
                let input = parsed_inputs[index].clone().expect("上方已过滤被拒槽位");
                async move {
                    tracing::debug!(tool = %tc.name, "并发执行 parallel_safe 工具");
                    let result = registry.execute(&tc.name, input).await;
                    tracing::debug!(
                        tool = %tc.name,
                        is_error = result.is_error,
                        "parallel_safe 工具返回"
                    );
                    (index, result)
                }
            });
        for (index, mut result) in join_all(futures).await {
            if !result.is_error {
                apply_skill_invocations(
                    &invocations_per_slot[index],
                    layout,
                    state,
                    &mut package_skill_contexts,
                    &mut result,
                );
            }
            media_blocks.extend(result.attachments);
            result.content = redact_sensitive_text(&result.content);
            let tc = &outcome.tool_calls[index];
            slots[index] = Some(ContentBlock::ToolResult {
                tool_use_id: tc.id.clone(),
                content: result.content,
                is_error: result.is_error,
            });
        }
    } else {
        execute_tool_batch_serial(
            outcome,
            registry,
            layout,
            channels,
            state,
            catalog,
            &policy,
            &skill_manage_preflight,
            &callable_dynamic_tools,
            &mut slots,
            &mut media_blocks,
            &mut package_skill_contexts,
            &mut validated_media_markers,
        )
        .await;
    }

    let tool_result = Message {
        role: Role::User,
        content: slots.into_iter().flatten().collect(),
        tool_call_id: None,
        ts: 0,
    };
    let media_message = if media_blocks.is_empty() {
        None
    } else if policy.supports_multimodal() {
        let mut content = Vec::with_capacity(media_blocks.len() + 1);
        content.push(ContentBlock::text(
            "工具读取到以下多模态附件，请结合附件内容继续完成任务。",
        ));
        content.extend(media_blocks);
        Some(Message {
            role: Role::User,
            content,
            tool_call_id: None,
            ts: 0,
        })
    } else {
        Some(multimodal_unsupported_message())
    };
    let skill_context_message = (!package_skill_contexts.is_empty())
        .then(|| Message::plain(Role::User, package_skill_contexts.join("\n\n")));
    ToolExecutionMessages {
        tool_result,
        skill_context_message,
        media_message,
        validated_media_markers,
    }
}

/// 串行执行路径（原 `execute_tool_batch` 主循环）：逐条处理，允许工具间的状态
/// 副作用（Skill 激活 / 媒体门禁 / 熔断检查）影响同批后续调用。
#[allow(clippy::too_many_arguments)]
async fn execute_tool_batch_serial(
    outcome: &rclaw_core::ChatOutcome,
    registry: &ToolRegistry,
    layout: &Layout,
    channels: &rclaw_core::RuntimeChannelInfo,
    state: &mut TurnState,
    catalog: &SkillCatalog,
    policy: &ToolExecutionPolicy<'_>,
    skill_manage_preflight: &HashMap<usize, String>,
    callable_dynamic_tools: &HashSet<String>,
    slots: &mut [Option<ContentBlock>],
    media_blocks: &mut Vec<rclaw_core::ContentBlock>,
    package_skill_contexts: &mut Vec<String>,
    validated_media_markers: &mut Vec<String>,
) {
    let mut index = 0;
    while index < outcome.tool_calls.len() {
        let tc = &outcome.tool_calls[index];
        if state.skill_scope_changed {
            slots[index] = Some(rejected_call(
                tc,
                "Skill 定义或加载状态已发生变化；该批次后续工具调用基于旧定义，已取消执行。请重新读取对应 Skill 后再决策。",
            ));
            index += 1;
            continue;
        }
        if state.media_scope_changed {
            slots[index] = Some(rejected_call(
                tc,
                "TaskList 收到新的团队消息；该批次后续工具调用基于旧上下文，已取消执行。请结合新消息重新决策。",
            ));
            index += 1;
            continue;
        }

        if policy.is_foreground() && is_parallel_agent_tool(&tc.name) {
            let start = index;
            while index < outcome.tool_calls.len()
                && is_parallel_agent_tool(&outcome.tool_calls[index].name)
            {
                index += 1;
            }
            let source_channel = policy.source_channel().to_string();
            let source_chat = policy.source_chat().to_string();
            let loaded_package_skills = &state.loaded_package_skills;
            let futures =
                outcome.tool_calls[start..index]
                    .iter()
                    .enumerate()
                    .map(|(offset, tc)| {
                        let source_channel = source_channel.clone();
                        let source_chat = source_chat.clone();
                        async move {
                            let slot_index = start + offset;
                            let mut input: Value = match serde_json::from_str(&tc.input) {
                                Ok(value) => value,
                                Err(_) => {
                                    return (
                                        slot_index,
                                        rejected_call(tc, "工具入参不是合法 JSON，已拒绝执行"),
                                        Vec::new(),
                                        Vec::new(),
                                        false,
                                    );
                                }
                            };
                            patch_runtime_source_channel(&mut input, &source_channel);
                            patch_runtime_source_chat(&mut input, &source_chat);
                            inject_package_skill_context(&mut input, loaded_package_skills);
                            tracing::debug!(tool = %tc.name, "并发执行 agent 工具");
                            let mut result = registry.execute(&tc.name, input).await;
                            tracing::debug!(
                                tool = %tc.name,
                                is_error = result.is_error,
                                "agent 工具返回"
                            );
                            let markers = if result.is_error {
                                Vec::new()
                            } else {
                                validated_media_markers_from_agent_result(
                                    &result.content,
                                    &source_channel,
                                    channels,
                                )
                            };
                            let attempted =
                                agent_result_reports_media_publish_attempt(&result.content);
                            result.content = redact_sensitive_text(&result.content);
                            (
                                slot_index,
                                ContentBlock::ToolResult {
                                    tool_use_id: tc.id.clone(),
                                    content: result.content,
                                    is_error: result.is_error,
                                },
                                result.attachments,
                                markers,
                                attempted,
                            )
                        }
                    });
            for (slot_index, block, attachments, markers, attempted) in join_all(futures).await {
                slots[slot_index] = Some(block);
                media_blocks.extend(attachments);
                validated_media_markers.extend(markers);
                state.media_publish_attempted |= attempted;
            }
            continue;
        }

        if let Some(error) = skill_manage_preflight.get(&index) {
            slots[index] = Some(rejected_call(tc, error));
            index += 1;
            continue;
        }
        if is_policy_blocked_tool(policy, &tc.name) {
            let content = if policy.candidate_patch_only()
                && !is_candidate_patch_tool_allowed(&tc.name)
            {
                format!(
                    "复杂编码子任务处于 candidate-patch 隔离模式，工具 {} 不在只读/协作白名单中；请返回候选补丁。",
                    tc.name
                )
            } else {
                format!("sub-agent 不允许调用工具 {}", tc.name)
            };
            slots[index] = Some(rejected_call(tc, &content));
            index += 1;
            continue;
        }
        let mut input: Value = match serde_json::from_str(&tc.input) {
            Ok(value) => value,
            Err(_) => {
                slots[index] = Some(rejected_call(tc, "工具入参不是合法 JSON，已拒绝执行"));
                index += 1;
                continue;
            }
        };
        patch_policy_input(policy, &tc.name, &mut input);

        let media_publish_attempt = tc.name == "shell"
            && input
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(shell_command_attempts_media_publisher);
        let trusted_media_publish_command = tc.name == "shell"
            && input
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(|command| {
                    shell_command_runs_media_publisher(
                        command,
                        &layout.skills_dir,
                        input.get("cwd").and_then(Value::as_str),
                    )
                });
        let trusted_dynamic_media_publisher = callable_dynamic_tools.contains(&tc.name)
            && state
                .dynamic
                .is_media_publisher(&tc.name, &layout.skills_dir);
        let media_publish_command_error = (tc.name == "shell")
            .then(|| input.get("command").and_then(Value::as_str))
            .flatten()
            .and_then(|command| {
                untrusted_publish_media_command_error(
                    command,
                    &layout.skills_dir,
                    input.get("cwd").and_then(Value::as_str),
                )
            });
        let skill_shell_command_error = (tc.name == "shell")
            .then(|| input.get("command").and_then(Value::as_str))
            .flatten()
            .and_then(|command| state.dynamic.legacy_shell_error(command));
        let media_agent_forward = matches!(
            tc.name.as_str(),
            "TaskList" | "AgentStatus" | "AgentWait" | "AgentResume"
        );
        if tc.name == "shell" {
            state.media_publish_attempted |= media_publish_attempt;
        }
        state.media_publish_attempted |= trusted_dynamic_media_publisher;
        if matches!(policy, ToolExecutionPolicy::Subagent { .. })
            && matches!(tc.name.as_str(), "write" | "edit")
            && input.get("expected_sha256").is_none()
        {
            slots[index] = Some(rejected_call(
                tc,
                "多 Agent 写入必须携带 expected_sha256：先用 read(include_metadata=true) 取得版本；新建文件使用 \"missing\"。",
            ));
            index += 1;
            continue;
        }

        let skill_invocations = if tc.name == "read" && policy.allow_skill_activation() {
            announce_skill_invocations(
                policy,
                channels,
                std::slice::from_ref(tc),
                catalog,
                &mut state.announced_skill_paths,
            )
            .await
        } else {
            Vec::new()
        };
        tracing::debug!(tool = %tc.name, "执行工具");
        // 写形状判定须在执行链 move input 之前完成。
        let memory_write_shape = tc.name == "memory" && is_memory_write_call(&input);
        // Layer 5 选点回滚：前台 write/edit/delete 在执行前捕获 before 内容
        // （input 随后会被执行链 move，先解析出需要的字段）。
        let rewind_capture = if policy.is_foreground() {
            rewind::capture_before(&tc.name, &input, &layout.base)
        } else {
            None
        };
        // memory 失败上限：同一 turn 内连续失败 ≥3 次后不再执行，返回终态拒绝，
        // 防止记忆触顶/反复未命中把整个回复循环卡到预算耗尽。
        let mut result = if tc.name == "memory" && state.memory_failures >= 3 {
            ToolResult::err(
                "memory tool is disabled for the rest of this turn after repeated failures \
                 (3 consecutive errors). Skip memory operations and continue with your reply \
                 to the user."
                    .to_string(),
            )
        } else if let Some(error) = skill_shell_command_error {
            ToolResult::err(error)
        } else if let Some(error) = media_publish_command_error {
            ToolResult::err(error)
        } else if callable_dynamic_tools.contains(&tc.name) {
            execute_dynamic_tool(tc, input, policy.source_channel(), state, catalog).await
        } else if state.dynamic.contains(&tc.name) {
            ToolResult::err(format!(
                "Manifest 专用工具 `{}` 在本批次中刚被激活，Schema 尚未进入模型上下文；请在下一轮调用",
                tc.name
            ))
        } else {
            registry.execute(&tc.name, input).await
        };
        tracing::debug!(tool = %tc.name, is_error = result.is_error, "工具返回");
        // Layer 5：执行成功且文件确有变化 → 入账待落盘（turn 循环持久化后 flush）。
        if let Some(mut checkpoint) = rewind_capture {
            if !result.is_error && rewind::finish_capture(&mut checkpoint) {
                state.pending_file_checkpoints.push(checkpoint);
            }
        }

        state.media_scope_changed |= tc.name == "TaskList"
            && !result.is_error
            && result.content.contains("<team-message from=");
        if !result.is_error {
            apply_skill_invocations(
                &skill_invocations,
                layout,
                state,
                package_skill_contexts,
                &mut result,
            );
        }
        if tc.name == "skill_manage" && !result.is_error {
            state.skill_manage_succeeded = true;
            state.skill_scope_changed = true;
            let invalidated = state.dynamic.invalidate_all();
            catalog.invalidate();
            state.loaded_package_skills.clear();
            if invalidated {
                result.content.push_str(
                    "\n\nSkill 文件已发生变更，本任务先前激活的 Manifest 专用工具已失效；继续使用前必须重新读取对应 Skill。",
                );
            }
        }
        // memory 记账：成功 → 置粘性标志（后台 memory 评审当轮跳过）并清失败计数；
        // 失败 → 累计（达到上限后本 turn 内转为终态拒绝）。写标志只认写形状的
        // 调用——`list` 是纯读（工具描述还鼓励 list-first），读成功不代表前台
        // 已写记忆，不能据此跳过后台 memory 评审。
        if tc.name == "memory" {
            if result.is_error {
                state.memory_failures = state.memory_failures.saturating_add(1);
            } else {
                if memory_write_shape {
                    state.memory_write_succeeded = true;
                }
                state.memory_failures = 0;
            }
        }
        if (trusted_media_publish_command || trusted_dynamic_media_publisher) && !result.is_error {
            validated_media_markers.extend(validated_media_markers_from_tool_result(
                &result.content,
                policy.source_channel(),
                channels,
            ));
        } else if media_agent_forward && !result.is_error {
            validated_media_markers.extend(validated_media_markers_from_agent_result(
                &result.content,
                policy.source_channel(),
                channels,
            ));
        }
        if media_agent_forward {
            state.media_publish_attempted |=
                agent_result_reports_media_publish_attempt(&result.content);
        }
        media_blocks.extend(result.attachments);
        result.content = redact_sensitive_text(&result.content);
        slots[index] = Some(ContentBlock::ToolResult {
            tool_use_id: tc.id.clone(),
            content: result.content,
            is_error: result.is_error,
        });
        index += 1;
    }
}

/// memory 调用是否为写形状：`operations` 批次，或单动作 add/replace/remove。
/// `list` 是纯读，且与工具内部分派同序优先于 `operations` 判定；
/// 无 action/operations 的入参在工具内必然报错，不会走到成功分支。
fn is_memory_write_call(input: &Value) -> bool {
    if input.get("action").and_then(Value::as_str) == Some("list") {
        return false;
    }
    if input.get("operations").is_some() {
        return true;
    }
    matches!(
        input.get("action").and_then(Value::as_str),
        Some("add") | Some("replace") | Some("remove")
    )
}

fn rejected_call(tc: &rclaw_core::ToolCall, content: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tc.id.clone(),
        content: content.to_string(),
        is_error: true,
    }
}

fn is_policy_blocked_tool(policy: &ToolExecutionPolicy<'_>, name: &str) -> bool {
    matches!(policy, ToolExecutionPolicy::Subagent { .. })
        && (is_subagent_blocked_tool(name)
            || (policy.candidate_patch_only() && !is_candidate_patch_tool_allowed(name)))
}

fn patch_policy_input(policy: &ToolExecutionPolicy<'_>, tool_name: &str, input: &mut Value) {
    match policy {
        ToolExecutionPolicy::Foreground { message, .. } => {
            if tool_name == "cron_add" {
                patch_cron_add_input(input, message);
            }
            if tool_name == "shell" {
                patch_shell_turn_context(input, message);
            }
        }
        ToolExecutionPolicy::Subagent { source_channel, .. } if tool_name == "shell" => {
            patch_runtime_source_channel(input, source_channel);
        }
        ToolExecutionPolicy::Subagent { .. } => {}
    }
}

async fn execute_dynamic_tool(
    tc: &rclaw_core::ToolCall,
    input: Value,
    source_channel: &str,
    state: &mut TurnState,
    catalog: &SkillCatalog,
) -> ToolResult {
    match state.dynamic.get(&tc.name).cloned() {
        Some(tool) => match tool.revalidate_definition() {
            Ok(()) => {
                tool.execute_with_source_channel(input, source_channel)
                    .await
            }
            Err(error) => {
                state.dynamic.invalidate_all();
                catalog.invalidate();
                state.loaded_package_skills.clear();
                state.skill_scope_changed = true;
                ToolResult::err(format!(
                    "Manifest 专用工具 `{}` 执行前重校验失败: {error}；当前任务的旧工具已全部失效，请重新读取对应 Skill",
                    tc.name
                ))
            }
        },
        None => ToolResult::err(format!(
            "Manifest 专用工具 `{}` 已失效；请重新读取对应 Skill",
            tc.name
        )),
    }
}

fn apply_skill_invocations(
    invocations: &[SkillInvocation],
    layout: &Layout,
    state: &mut TurnState,
    package_skill_contexts: &mut Vec<String>,
    result: &mut ToolResult,
) {
    for skill_path in invocations.iter().map(|invocation| &invocation.path) {
        if let Some(name) = top_level_skill_name(layout, skill_path) {
            if let Err(error) =
                SkillUsageStore::new(layout.skills_dir.clone()).record_view_and_use(&name)
            {
                tracing::warn!(skill = name, %error, "failed to record Skill use telemetry");
            }
        }
        match load_package_skill(layout, skill_path, &mut state.loaded_package_skills) {
            PackageSkillLoad::Loaded { path, content } => {
                package_skill_contexts.push(format!(
                    "以下包级 Skill 说明已按需加载，并与刚读取的具体 Skill 同时生效（来源：{}）：\n\n{}",
                    path.display(),
                    content,
                ));
            }
            PackageSkillLoad::Failed { path, error } => {
                result.is_error = true;
                result.content = format!(
                    "包级 Skill 说明加载失败（{}）：{error}。当前具体 Skill 依赖该说明，已停止加载；请报告安装或文件状态异常，不要绕过包级规则继续执行。",
                    path.display()
                );
            }
            PackageSkillLoad::NotApplicable | PackageSkillLoad::AlreadyHandled => {}
        }
        if !result.is_error {
            match state.dynamic.activate(&layout.base, skill_path) {
                Ok(activated) if !activated.is_empty() => {
                    result.content.push_str(&format!(
                        "\n\n已激活 Manifest 专用工具: {}。后续必须调用这些结构化工具，不要手写 Skill 脚本命令。",
                        activated.join(", ")
                    ));
                }
                Ok(_) => {}
                Err(error) => {
                    result.is_error = true;
                    result.content = format!(
                        "Skill Manifest 加载失败（{}）: {error}。已停止加载该 Skill，请修复 Manifest，不要退回手写 shell 命令。",
                        skill_path.display()
                    );
                    state.skill_scope_changed = true;
                }
            }
        }
        // 热经验层（EXPERIENCE.md）：技能目录携带的实战经验随入口读取自动注入，
        // 不改 SKILL.md 正文即生效；固化由后台 review fork 评估后执行（对齐
        // JiuwenSwarm evolutions.json 的「经验层与正文分离」思路）。仅顶层目录型
        // 技能（skills/<name>/SKILL.md）支持：skill_manage 的 write_file 只能按顶层
        // 目录定位，包内/平铺技能没有可写的经验归属位置（判定与索引标注同口径，
        // 用 top_level_skill_dir_name 而非记账归因的 top_level_skill_name——后者
        // 会把包内技能也算进来）。
        if !result.is_error
            && rclaw_store::skills::top_level_skill_dir_name(
                &skill_path.to_string_lossy().replace('\\', "/"),
            )
            .is_some()
        {
            if let Some(skill_dir) = skill_path.parent() {
                if let Ok(content) = std::fs::read_to_string(skill_dir.join("EXPERIENCE.md")) {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        let bounded: String = trimmed.chars().take(4000).collect();
                        result.content.push_str(&format!(
                            "\n\n[hot experience] 已自动加载该技能的 EXPERIENCE.md（未固化进 SKILL.md 的实战经验补丁，优先遵守）：\n{bounded}"
                        ));
                    }
                }
            }
        }
    }
}

pub(crate) async fn announce_skill_invocations(
    policy: &ToolExecutionPolicy<'_>,
    channels: &rclaw_core::RuntimeChannelInfo,
    tool_calls: &[rclaw_core::ToolCall],
    catalog: &SkillCatalog,
    announced_skill_paths: &mut HashSet<PathBuf>,
) -> Vec<SkillInvocation> {
    let invocations = catalog.detect_skill_invocations(tool_calls);
    for invocation in &invocations {
        let (path, name, package) = (&invocation.path, &invocation.name, &invocation.package);
        if !announced_skill_paths.insert(path.clone()) {
            continue;
        }
        tracing::info!(
            channel = policy.source_channel(),
            skill = ?name,
            skill_package = ?package,
            skill_path = ?path,
            "正在调用 Skill"
        );
        let ToolExecutionPolicy::Foreground {
            message,
            outbound,
            send_working_status,
        } = policy
        else {
            continue;
        };
        if message.source() != MessageSource::User
            || !*send_working_status
            || !should_emit_working_status(&message.channel, channels)
        {
            continue;
        }
        let display_name = package
            .as_ref()
            .map(|package| format!("{package} / {name}"))
            .unwrap_or_else(|| name.clone());
        if let Err(error) = outbound
            .send(BusMessage::new(
                message.channel.clone(),
                message.chat_id.clone(),
                format!("🧩 正在调用 Skill: {display_name}"),
            ))
            .await
        {
            tracing::warn!(skill = ?name, ?error, "Skill 调用状态推送失败");
        }
    }
    invocations
}

pub(crate) fn is_subagent_blocked_tool(name: &str) -> bool {
    matches!(
        name,
        "Agent"
            | "Task"
            | "AgentStatus"
            | "AgentWait"
            | "AgentCancel"
            | "AgentResume"
            | "TeamCreate"
            | "TeamDelete"
            | "PlanRun"
            | "message_agent"
            | "cron_add"
            | "cron_list"
            | "cron_remove"
            // Layer 3：应答与引擎入口都是编排者（主代理）专属；子代理只能 escalate 提问。
            | "ResolveWorkflowQuestion"
            | "WorkflowRun"
    )
}

pub(crate) fn is_candidate_patch_tool_allowed(name: &str) -> bool {
    matches!(
        name,
        "read"
            | "list"
            | "grep"
            | "web_search"
            | "zhipu_search"
            | "tavily_search"
            | "doubao_custom_search"
            | "doubao_global_search"
            | "tavily_extract"
            | "TaskUpdate"
            | "TaskList"
            | "TaskGet"
            | "SendMessage"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_write_flag_only_for_write_shaped_calls() {
        // list 是纯读：读成功不得置 memory_write_succeeded。
        let list = serde_json::json!({"action": "list", "target": "memory"});
        assert!(!is_memory_write_call(&list));
        for action in ["add", "replace", "remove"] {
            let w = serde_json::json!({"action": action, "target": "memory"});
            assert!(is_memory_write_call(&w), "{action} 应视为写");
        }
        let batch = serde_json::json!({"target": "memory", "operations": [{"action": "add"}]});
        assert!(is_memory_write_call(&batch));
        // 双形状入参（action=list 且带 operations）：工具按 list 优先分派为读。
        let malformed = serde_json::json!({
            "action": "list",
            "target": "memory",
            "operations": [{"action": "add"}]
        });
        assert!(!is_memory_write_call(&malformed));
        // 无 action/operations：工具内必报错，不会走到成功记账分支。
        assert!(!is_memory_write_call(
            &serde_json::json!({"target": "memory"})
        ));
    }
}
