use super::*;

// ---------------------------------------------------------------------------
// 安全点插队辅助
// ---------------------------------------------------------------------------

/// 安全点：排空所有已到达的插队消息，按到达顺序处理。
///
/// 注入位置永远是「当前 session tail 之后」（已闭合：要么 [当前 user]、要么 […完整 tool chain]），
/// 因此不会插进 `assistant(tool_calls)` 与 `tool_result` 中间，满足 OpenAI 兼容协议的邻接约束。
///
/// 返回值表示是否注入了至少一条真实用户消息。调用方在 tool_calls 执行前用它决定是否
/// 跳过旧 tool_calls 并重新请求 LLM。
pub(super) async fn drain_append_interjections(
    session: &SessionStore,
    messages: &mut Vec<Message>,
    event_rx: &mut mpsc::Receiver<Interjection>,
    deferred: &mut Vec<BusMessage>,
) -> bool {
    let mut appended_user = false;
    while let Ok(Interjection::AppendMessage(extra)) = event_rx.try_recv() {
        if extra.source() == MessageSource::User {
            persist_and_push_user(session, messages, &extra).await;
            appended_user = true;
        } else {
            deferred.push(extra);
        }
    }
    appended_user
}

/// 把一条补充文本作为 user 消息：先持久化到 session 安全位置（tail），再追加进内存 messages。
///
/// 即使持久化失败也仍注入内存（保证本轮 LLM 能看到），仅告警——本轮后续的 assistant
/// 若成功落盘，下轮 load_history 仍能拿到这条 user（持久化是尽力而为）。
pub(super) async fn persist_and_push_user(
    session: &SessionStore,
    messages: &mut Vec<Message>,
    msg: &BusMessage,
) {
    let m = Message::plain(Role::User, msg.content.clone());
    if let Err(e) = session.append(&msg.channel, &msg.chat_id, &m) {
        tracing::warn!("插队补充消息持久化失败（仍注入内存）: {e}");
    }
    messages.push(m);
}

/// 排空接收端，收集未处理消息（供上层保留原始 [`MessageSource`] 重新路由）。
pub(super) fn drain_leftovers(event_rx: &mut mpsc::Receiver<Interjection>) -> Vec<BusMessage> {
    let mut leftovers = Vec::new();
    while let Ok(Interjection::AppendMessage(msg)) = event_rx.try_recv() {
        leftovers.push(msg);
    }
    leftovers
}

// ---------------------------------------------------------------------------
// 由 LLM 一轮结果构造 assistant 消息等小工具
// ---------------------------------------------------------------------------

pub(crate) struct ToolExecutionMessages {
    pub(crate) tool_result: Message,
    /// 当前 turn 按需注入的包级 Skill 说明，不持久化到 session。
    pub(crate) skill_context_message: Option<Message>,
    /// 当前 ReAct 轮临时注入给模型的多模态内容，不持久化到 session。
    pub(crate) media_message: Option<Message>,
    /// 本批次新鲜验证的媒体 marker：由循环侧经 TurnState 的 seen 去重后认领。
    /// 粘性状态（skill_manage 成功、发布尝试、scope 标志）直接写在 TurnState 上，
    /// 不再经由出参往返。
    pub(crate) validated_media_markers: Vec<String>,
}

pub(super) fn multimodal_unsupported_message() -> Message {
    Message::plain(
        Role::User,
        "read 工具返回了媒体附件，但当前模型不支持多模态输入，附件没有发送给模型。不要声称已经看过附件内容；请说明需要切换到支持多模态的模型，或请用户提供文本描述/可读文本版本。",
    )
}

/// 是否向该通道推送「工作状态」（C3：按接线期采集的能力声明，替代按名排除）。
pub(super) fn should_emit_working_status(
    channel: &str,
    info: &rclaw_core::RuntimeChannelInfo,
) -> bool {
    info.caps(channel).is_some_and(|caps| caps.working_status)
}

/// 中间内容（工具调用轮的 text / reasoning，或最终回复轮的 reasoning）是用户可见输出，
/// 不能绕过完成门禁。含外部完成声明、未验证的媒体交付声明或内部状态帧时不外发——最终
/// 经过验证的结论仍由正式回复承载。
///
/// 工具调用轮还要额外拦截保留媒体控制行：通道出站层会解析其中的标记并投递真实文件，
/// 不能让新外发的工具轮内容绕过发布工具回执校验。最终回复轮继续沿用原有 reasoning
/// 安全规则，不扩大本次改动范围。
pub(super) fn intermediate_content_blocked(content: &str, tool_round: bool) -> bool {
    let lower = content.to_ascii_lowercase();
    !claimed_external_actions(content).is_empty()
        || claims_media_delivery(content)
        || lower.contains("<agent-status>")
        || lower.contains("<rclaw-runtime-context")
        || (tool_round && content.lines().any(is_media_control_line))
}

/// 向 napcat 通道推送一轮 LLM 的中间内容（工具调用轮或最终回复轮）。`tool_round`
/// 决定本轮 text 与 reasoning 的互斥选择：
/// - `true`（工具调用轮）：优先发送非空 `outcome.text`（普通过程消息格式，无「💭」前缀）；
///   `text` 为空或未通过安全检查时才 fallback 到 `reasoning`（「💭 思考中...」格式）。
/// - `false`（最终回复轮）：`text` 即将作为正式回复出站，此处只发 `reasoning`（保持原行为）。
///
/// 同一轮只发送一次：text 与 reasoning 互斥，多个并行 tool_calls 不重复发送。cron 任务
/// 与非 napcat 通道不发送。
pub(super) fn emit_napcat_intermediate_if_safe(
    outbound_tx: &mpsc::Sender<BusMessage>,
    msg: &BusMessage,
    outcome: &rclaw_core::ChatOutcome,
    is_cron: bool,
    tool_round: bool,
    info: &rclaw_core::RuntimeChannelInfo,
) {
    // C3：中间输出许可按接线期采集的能力声明（当前仅 NapCat 声明 true），
    // 替代按通道名硬编码。
    if is_cron
        || !info
            .caps(&msg.channel)
            .is_some_and(|caps| caps.intermediate_output)
    {
        return;
    }
    let text = outcome.text.trim();
    let reasoning = outcome
        .reasoning
        .as_deref()
        .map(str::trim)
        .filter(|reasoning| !reasoning.is_empty());

    // text 与 reasoning 互斥，同一轮只发送一次。
    let (content, as_text) = if tool_round && !text.is_empty() {
        if intermediate_content_blocked(text, true) {
            match reasoning {
                Some(r) if !intermediate_content_blocked(r, true) => {
                    tracing::warn!("已抑制未通过安全检查的 NapCat 工具轮 text，回退到 reasoning");
                    (r, false)
                }
                _ => {
                    tracing::warn!("NapCat 中间 text 被抑制且无可用 reasoning，本轮无中间输出");
                    return;
                }
            }
        } else {
            (text, true)
        }
    } else {
        match reasoning {
            Some(r) if !intermediate_content_blocked(r, tool_round) => (r, false),
            Some(_) => {
                tracing::warn!("已抑制未通过安全检查的 NapCat reasoning");
                return;
            }
            None => return,
        }
    };

    let payload = if as_text {
        // text 使用普通过程消息格式，不加「💭 思考中...」前缀。
        content.to_string()
    } else {
        format!("💭 思考中...\n{}", content)
    };
    // 标记 intermediate：通道据此区分过程输出与最终回复（桌面端忙碌态只在收到
    // 非 intermediate 的普通消息时解除，工具轮之间的过程文本不会误判为 turn 收尾）。
    let _ = outbound_tx.try_send(
        BusMessage::new(msg.channel.clone(), msg.chat_id.clone(), payload).with_intermediate(),
    );
}

/// 按文件工具的路径语义规范化模型传入的路径：绝对路径保持不变，普通相对路径基于 agent
/// 根目录，显式带 agent 根目录名的旧式相对路径基于其父目录。这样 Skill 识别与实际 `read`
/// 执行指向同一个文件。
pub(crate) fn normalize_agent_tool_path(path: &Path, base: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return normalize_absolute_lexical_path(path, None);
    }
    if base
        .file_name()
        .is_some_and(|base_name| path.starts_with(base_name))
    {
        return normalize_absolute_lexical_path(path, base.parent());
    }
    normalize_absolute_lexical_path(path, Some(base))
}

pub(crate) enum PackageSkillLoad {
    NotApplicable,
    AlreadyHandled,
    Loaded { path: PathBuf, content: String },
    Failed { path: PathBuf, error: String },
}

/// 包根说明文档登记表：读取登记包内的具体 Skill 时，自动注入的包根文档（逐包登记）。
/// 键为 `skills/` 下的包目录名，值为该包根目录下说明文档的文件名。
const PACKAGE_ROOT_DOCS: &[(&str, &str)] = &[
    ("ai-berkshire", "berkshire.md"),
    ("watch-positions", "watch-positions.md"),
    ("read-macro", "read-macro.md"),
    ("pick-funds", "pick-funds.md"),
];

/// 登记包（见 [`PACKAGE_ROOT_DOCS`]）的公共规则不常驻系统提示；首次读取包内具体 Skill 时，
/// 才把包根说明文档注入当前 turn，后续读取只复用已经进入上下文的内容。
///
/// 读取前解析符号链接/junction，确保文件仍落在 agent 根目录内；缺失、越界、空文件、
/// 非 UTF-8 或超限都显式返回失败，调用方会把本次 Skill read 标为错误，避免静默绕过规则。
pub(crate) fn load_package_skill(
    layout: &Layout,
    skill_path: &Path,
    loaded_package_skills: &mut HashMap<PathBuf, String>,
) -> PackageSkillLoad {
    let current_dir = std::env::current_dir().ok();
    let Some(skills_root) =
        normalize_absolute_lexical_path(&layout.skills_dir, current_dir.as_deref())
    else {
        return PackageSkillLoad::NotApplicable;
    };
    let Ok(relative) = skill_path.strip_prefix(&skills_root) else {
        return PackageSkillLoad::NotApplicable;
    };
    let mut parts = relative.components();
    let Some(package) = parts.next().map(|part| part.as_os_str()) else {
        return PackageSkillLoad::NotApplicable;
    };
    if parts.next().map(|part| part.as_os_str()) != Some(std::ffi::OsStr::new("skills")) {
        return PackageSkillLoad::NotApplicable;
    }
    let package_name = package.to_string_lossy();
    let Some(root_doc) = PACKAGE_ROOT_DOCS
        .iter()
        .find(|(name, _)| *name == package_name)
        .map(|(_, doc)| *doc)
    else {
        return PackageSkillLoad::NotApplicable;
    };

    let package_skill_path = skills_root.join(package).join(root_doc);
    if loaded_package_skills.contains_key(&package_skill_path) {
        return PackageSkillLoad::AlreadyHandled;
    }

    let Some(base) = normalize_absolute_lexical_path(&layout.base, current_dir.as_deref()) else {
        return package_skill_failure(package_skill_path, "agent 根目录无法规范化");
    };
    if !resolve_within_base(&base, &package_skill_path) {
        return package_skill_failure(
            package_skill_path,
            "文件经符号链接或 junction 解析后越出 agent 根目录",
        );
    }

    let file = match std::fs::File::open(&package_skill_path) {
        Ok(file) => file,
        Err(error) => {
            return package_skill_failure(package_skill_path, format!("读取失败：{error}"));
        }
    };
    let mut bytes = Vec::new();
    if let Err(error) = file
        .take((MAX_PACKAGE_SKILL_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
    {
        return package_skill_failure(package_skill_path, format!("读取失败：{error}"));
    }
    if bytes.len() > MAX_PACKAGE_SKILL_BYTES {
        return package_skill_failure(
            package_skill_path,
            format!(
                "文件过大（{} 字节 > 上限 {MAX_PACKAGE_SKILL_BYTES} 字节）",
                bytes.len()
            ),
        );
    }
    let content = match String::from_utf8(bytes) {
        Ok(content) => content,
        Err(error) => {
            return package_skill_failure(package_skill_path, format!("不是合法 UTF-8：{error}"));
        }
    };
    if content.trim().is_empty() {
        return package_skill_failure(package_skill_path, "文件为空");
    }
    let skills_dir = skills_root.to_string_lossy().replace('\\', "/");
    let content = content.replace("{skills_dir}", &skills_dir);
    loaded_package_skills.insert(package_skill_path.clone(), content.clone());
    tracing::info!(
        package_skill_path = ?package_skill_path,
        "按需加载包级 Skill 说明"
    );
    PackageSkillLoad::Loaded {
        path: package_skill_path,
        content,
    }
}

pub(super) fn package_skill_failure(path: PathBuf, error: impl Into<String>) -> PackageSkillLoad {
    let error = error.into();
    tracing::warn!(
        package_skill_path = ?path,
        error = %error,
        "包级 Skill 说明加载失败"
    );
    PackageSkillLoad::Failed { path, error }
}

/// 转成不访问文件系统、不展开符号链接的规范化绝对路径。工具沙箱本身拒绝 `..`，这里也拒绝，
/// 避免别名路径与系统提示中公布的 Skill 入口发生意外等价。
pub(crate) fn normalize_absolute_lexical_path(
    path: &Path,
    current_dir: Option<&Path>,
) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => return None,
            _ => normalized.push(component.as_os_str()),
        }
    }
    Some(normalized)
}

/// 在读取并解析完整 Skill 清单前，先按存储层支持的入口结构过滤目标路径，避免普通文件或
/// `references/scripts` 读取反复触发全目录扫描。
pub(crate) fn is_potential_skill_entry_path(skills_root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(skills_root) else {
        return false;
    };
    let parts = relative.iter().collect::<Vec<_>>();
    let is_skill_md =
        |name: &std::ffi::OsStr| name.to_string_lossy().eq_ignore_ascii_case("SKILL.md");
    let is_markdown = |name: &std::ffi::OsStr| {
        Path::new(name).extension().and_then(|ext| ext.to_str()) == Some("md")
    };
    let is_skills_dir =
        |name: &std::ffi::OsStr| name.to_string_lossy().eq_ignore_ascii_case("skills");

    match parts.as_slice() {
        [file] => is_markdown(file),
        [_skill_dir, file] => is_skill_md(file),
        [_package, skills_dir, file] => is_skills_dir(skills_dir) && is_markdown(file),
        [_package, skills_dir, _skill_dir, file] => is_skills_dir(skills_dir) && is_skill_md(file),
        _ => false,
    }
}

/// 由 LLM 一轮结果构造 assistant 消息（含 text / reasoning / tool_use 块）。
///
/// 任一 tool_use 的 input 不是合法 JSON 时返回 `Err`——调用方据此放弃持久化本轮
/// （绝不能把畸形入参静默当 {} 写进会话历史）。
pub(super) fn build_assistant_message(
    outcome: &rclaw_core::ChatOutcome,
) -> std::result::Result<Message, String> {
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
        role: Role::Assistant,
        content: blocks,
        tool_call_id: None,
        ts: 0,
    })
}

pub(crate) fn build_loop_circuit_result(
    outcome: &rclaw_core::ChatOutcome,
    fingerprints: &[String],
) -> Message {
    let detail = format!(
        "LOOP_CIRCUIT_OPEN: 同一规范化工具调用已重复达到 {} 次（{}）。本轮未执行工具。停止原样重试，改用不同参数或策略；若缺少必要信息，请明确请求。",
        REPEATED_TOOL_CALL_LIMIT,
        fingerprints.join(", ")
    );
    Message {
        role: Role::User,
        content: outcome
            .tool_calls
            .iter()
            .map(|call| ContentBlock::ToolResult {
                tool_use_id: call.id.clone(),
                content: detail.clone(),
                is_error: true,
            })
            .collect(),
        tool_call_id: None,
        ts: 0,
    }
}

/// 给带身份的 Cron 最终结果加任务标签。旧式无 job metadata 的内部 Cron 保持原文兼容。
pub(super) fn cron_report(msg: &BusMessage, body: &str, succeeded: bool) -> String {
    let Some(raw_name) = msg.cron_job_name() else {
        return body.to_string();
    };
    let name = raw_name.split_whitespace().collect::<Vec<_>>().join(" ");
    if name.is_empty() {
        return body.to_string();
    }
    let status = if succeeded { "✅" } else { "⚠️" };
    format!("{status}【定时任务：{name}】\n{body}")
}

/// 给 cron_add 入参补上当前 turn 的 channel/chat_id（缺失或非法时）。
pub(super) fn patch_cron_add_input(input: &mut Value, msg: &BusMessage) {
    // channel 缺失 → 当前通道。
    let need_channel = input
        .get("channel")
        .and_then(|v| v.as_str())
        .is_none_or(|s| s.is_empty());
    if need_channel && !msg.channel.is_empty() {
        input["channel"] = json!(msg.channel);
    }
    // chat_id 缺失或 "cron" 且 channel 匹配当前 → 当前 chat_id。
    let channel_matches = input
        .get("channel")
        .and_then(|v| v.as_str())
        .is_some_and(|s| s == msg.channel);
    if channel_matches && !msg.chat_id.is_empty() {
        let bad = input
            .get("chat_id")
            .and_then(|v| v.as_str())
            .is_none_or(|s| s.is_empty() || s == "cron");
        if bad {
            input["chat_id"] = json!(msg.chat_id);
        }
    }
}

/// 给 shell/Agent 工具注入仅供运行时使用的当前来源通道。
///
/// 所有外部 channel 共用媒体沙箱，不能靠目录反推出回执应属于哪个 channel。
/// 该字段不暴露在工具 schema 中；shell 执行时只将它映射为子进程环境变量
/// `RCLAW_SOURCE_CHANNEL`。这样无需修改进程级环境变量，并发 turn 也不会串线。
pub(super) fn patch_shell_turn_context(input: &mut Value, msg: &BusMessage) {
    patch_runtime_source_channel(input, &msg.channel);
}

pub(crate) fn patch_runtime_source_channel(input: &mut Value, source_channel: &str) {
    let Some(input) = input.as_object_mut() else {
        return;
    };
    // 即使当前来源为空也先移除该字段，避免模型输入或复用参数中的旧值冒充本轮来源。
    input.remove("_rclaw_source_channel");
    if !source_channel.is_empty() {
        input.insert("_rclaw_source_channel".to_string(), json!(source_channel));
    }
}

pub(super) fn is_parallel_agent_tool(name: &str) -> bool {
    matches!(name, "Agent" | "Task" | "PlanRun")
}

/// 把主任务本轮已经按需激活的包级 Skill 规则传给 Agent/Task 子任务。
///
/// 子 agent 使用独立的对话上下文，主任务临时注入的消息不会自然传播；因此在工具执行时
/// 追加到 `context`，既保留调用方原有共享上下文，也避免把包级规则常驻到所有子 agent。
pub(super) fn inject_package_skill_context(
    input: &mut Value,
    loaded_package_skills: &HashMap<PathBuf, String>,
) {
    if loaded_package_skills.is_empty() {
        return;
    }
    let Some(input) = input.as_object_mut() else {
        return;
    };

    let mut packages = loaded_package_skills.iter().collect::<Vec<_>>();
    packages.sort_by_key(|(path, _)| *path);
    let inherited = packages
        .into_iter()
        .map(|(path, content)| {
            format!(
                "### Package rules from {}\n\n{}",
                path.display(),
                content.trim()
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let inherited = format!("## Active Package Skill Rules (inherited)\n\n{inherited}");

    let context = input
        .get("context")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|context| !context.is_empty())
        .map(|context| format!("{context}\n\n{inherited}"))
        .unwrap_or(inherited);
    input.insert("context".into(), Value::String(context));
}
