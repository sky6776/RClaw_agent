use super::*;

pub(super) const DEFAULT_MAX_TOOL_ITER: usize = 256;
pub(super) const MAX_TOOL_ITER: usize = 256;
pub(super) const DEFAULT_MAX_OUTPUT_CHARS: usize = 48_000;
pub(super) const MAX_OUTPUT_CHARS: usize = 120_000;
pub(super) const EXECUTION_EVENT_TAIL: usize = 64;

pub(super) type RegistryLink = Arc<OnceLock<Arc<ToolRegistry>>>;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(super) enum ExecutionState {
    Queued,
    Running,
    Waiting,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
}

impl ExecutionState {
    pub(super) fn is_terminal(&self) -> bool {
        matches!(
            self,
            ExecutionState::Succeeded | ExecutionState::Failed | ExecutionState::Cancelled
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExecutionProgress {
    pub(super) completed_steps: usize,
    pub(super) total_steps: usize,
    pub(super) stage: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct StructuredHandoff {
    pub(super) goal: String,
    pub(super) summary: String,
    pub(super) evidence: Vec<String>,
    pub(super) artifacts: Vec<String>,
    pub(super) open_risks: Vec<String>,
    pub(super) validation: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExecutionSnapshot {
    pub(super) agent_id: String,
    pub(super) spec: AgentTaskSpec,
    pub(super) state: ExecutionState,
    pub(super) progress: ExecutionProgress,
    pub(super) handoff: Option<StructuredHandoff>,
    pub(super) error: Option<String>,
    /// 子 Agent 发布器产生并在共享媒体沙箱中验证过的 marker。
    #[serde(default)]
    pub(super) validated_media_markers: Vec<String>,
    /// 即使发布失败也要保留尝试状态，避免父 Agent 随后声称附件已经发送。
    #[serde(default)]
    pub(super) media_publish_attempted: bool,
    pub(super) created_at: String,
    pub(super) updated_at: String,
    pub(super) recovered: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ExecutionLogEntry {
    pub(super) event: String,
    pub(super) timestamp: String,
    pub(super) snapshot: ExecutionSnapshot,
}

pub(super) struct ExecutionRecord {
    pub(super) snapshot: ExecutionSnapshot,
    pub(super) events: Vec<ExecutionLogEntry>,
    pub(super) cancel: Option<CancellationToken>,
    pub(super) notify: Arc<Notify>,
}

#[derive(Clone)]
pub(super) struct ExecutionControl {
    pub(super) agent_id: String,
    pub(super) cancel: CancellationToken,
}

#[derive(Default)]
pub(super) struct ExecutionOutcomeUpdate {
    pub(super) handoff: Option<StructuredHandoff>,
    pub(super) error: Option<String>,
    pub(super) validated_media_markers: Vec<String>,
    pub(super) media_publish_attempted: bool,
}

pub(super) fn controlled_transition_failure(
    control: &ExecutionControl,
    error: String,
    completed_steps: usize,
    media_publish_attempted: bool,
) -> RunTaskFailure {
    if control.cancel.is_cancelled() {
        RunTaskFailure::Cancelled {
            reason: "任务在状态切换期间收到取消请求".to_string(),
            completed_steps,
            media_publish_attempted,
        }
    } else {
        RunTaskFailure::Failed {
            error,
            completed_steps,
            media_publish_attempted,
        }
    }
}

tokio::task_local! {
    pub(super) static CURRENT_AGENT: AgentIdentity;
    /// 当前计划步骤上下文 (plan_id, step_id)（Layer 3：escalate 定位 journal 用）。
    pub(super) static CURRENT_PLAN: Option<(String, String)>;
}

/// 当前计划步骤上下文；非计划步骤 / 作用域外返回 None。
pub(crate) fn current_plan_context() -> Option<(String, String)> {
    CURRENT_PLAN.try_with(std::clone::Clone::clone).ok().flatten()
}

#[derive(Clone, Debug)]
pub(super) struct AgentIdentity {
    pub(super) team_name: Option<String>,
    pub(super) name: String,
    pub(super) agent_id: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct TeamRuntimeState {
    pub(super) active_team: Option<String>,
    pub(super) teams: HashMap<String, TeamRecord>,
}

/// 每成员邮箱一个的显式唤醒句柄表（键 = (team, member)）。
type MailboxNotifyMap = HashMap<(String, String), Arc<Notify>>;

/// 协作域的唯一持久状态所有者。
///
/// 团队、任务、mailbox 与后台执行记录的锁和事件目录不再泄漏给运行时或工具适配器；
/// 所有写入都先追加事件日志，再提交内存快照，恢复也只在这里发生。
#[derive(Clone)]
pub(super) struct CollaborationState {
    teams: Arc<Mutex<TeamRuntimeState>>,
    executions: Arc<Mutex<HashMap<String, ExecutionRecord>>>,
    /// M2 显式唤醒：每 (team, member) 一个 Notify。SendMessage(wake=true) 唤醒
    /// 正在 `AgentWait` / `TaskList(wait_timeout_ms)` 上等待邮箱的成员，替代旧的
    /// 200ms 轮询。唤醒只作用于【自己选择等待中】的工具调用——没有任何机制能让
    /// SendMessage 中断或调度主循环（root 的邮箱只在它自然的轮次边界被读取）。
    mailbox_notifies: Arc<Mutex<MailboxNotifyMap>>,
    execution_dir: PathBuf,
}

impl CollaborationState {
    pub(super) fn open(execution_dir: PathBuf) -> Self {
        let teams = recover_team_state(&execution_dir);
        let executions = recover_execution_records(&execution_dir);
        Self {
            teams: Arc::new(Mutex::new(teams)),
            executions: Arc::new(Mutex::new(executions)),
            mailbox_notifies: Arc::new(Mutex::new(HashMap::new())),
            execution_dir,
        }
    }

    /// 取（或建）某成员邮箱的 Notify 句柄（等待方先注册 `notified()`，再复查邮箱）。
    pub(super) async fn mailbox_notify(&self, team_name: &str, name: &str) -> Arc<Notify> {
        self.mailbox_notifies
            .lock()
            .await
            .entry((team_name.to_string(), name.to_string()))
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }

    /// 唤醒某成员邮箱的全部等待者（无等待者时为 no-op）。
    pub(super) async fn wake_mailbox(&self, team_name: &str, name: &str) {
        let notify = self.mailbox_notify(team_name, name).await;
        notify.notify_waiters();
    }

    async fn mutate_team<R>(
        &self,
        event: &str,
        mutate: impl FnOnce(&mut TeamRuntimeState) -> Result<(R, bool), String>,
    ) -> Result<R, String> {
        let mut state = self.teams.lock().await;
        let mut next = state.clone();
        let (result, changed) = mutate(&mut next)?;
        if changed {
            append_team_state_log(&self.execution_dir, event, &next)?;
            *state = next;
        }
        Ok(result)
    }

    pub(super) async fn active_team_name(&self) -> Option<String> {
        self.teams.lock().await.active_team.clone()
    }

    pub(super) async fn team_snapshot(&self, team_name: &str) -> Option<TeamRecord> {
        self.teams.lock().await.teams.get(team_name).cloned()
    }

    pub(super) async fn team_exists(&self, team_name: &str) -> bool {
        self.teams.lock().await.teams.contains_key(team_name)
    }

    pub(super) async fn register_member(
        &self,
        team_name: String,
        member: TeamMember,
        identity: AgentIdentity,
    ) -> Result<Option<AgentIdentity>, String> {
        self.mutate_team("member_registered", move |state| {
            let team = state
                .teams
                .get_mut(&team_name)
                .ok_or_else(|| format!("Team not found: {team_name}"))?;
            if team
                .members
                .get(&member.name)
                .is_some_and(|existing| existing.is_active)
            {
                return Err(format!(
                    "Team member \"{}\" is already active; wait for it to finish or use a unique name",
                    member.name
                ));
            }
            if let Some(existing) = team.members.values().find(|existing| {
                existing.name != member.name && existing.agent_id == member.agent_id
            }) {
                return Err(format!(
                    "Team member \"{}\" conflicts with existing member \"{}\" after name normalization; use a distinct name",
                    member.name, existing.name
                ));
            }
            team.members.insert(member.name.clone(), member);
            Ok((Some(identity), true))
        })
        .await
    }

    pub(super) async fn mark_member_inactive(&self, team_name: String, name: String) {
        if let Err(error) = self
            .mutate_team("member_inactive", move |state| {
                let Some(member) = state
                    .teams
                    .get_mut(&team_name)
                    .and_then(|team| team.members.get_mut(&name))
                else {
                    return Ok(((), false));
                };
                let changed = member.is_active;
                member.is_active = false;
                Ok(((), changed))
            })
            .await
        {
            tracing::warn!("持久化成员 inactive 状态失败: {error}");
        }
    }

    pub(super) async fn drain_mailbox(
        &self,
        team_name: String,
        name: String,
        explicit_agent_id: Option<String>,
    ) -> Vec<TeamMessage> {
        match self
            .mutate_team("mailbox_drained", move |state| {
                let Some(team) = state.teams.get_mut(&team_name) else {
                    return Ok((Vec::new(), false));
                };
                let mut messages = team.mailboxes.remove(&name).unwrap_or_default();
                let agent_id = explicit_agent_id.or_else(|| {
                    team.members
                        .get(&name)
                        .map(|member| member.agent_id.clone())
                });
                if let Some(agent_id) = agent_id.filter(|agent_id| agent_id != &name) {
                    messages.extend(team.mailboxes.remove(&agent_id).unwrap_or_default());
                }
                let changed = !messages.is_empty();
                Ok((messages, changed))
            })
            .await
        {
            Ok(messages) => messages,
            Err(error) => {
                tracing::warn!("读取并持久化 mailbox 失败: {error}");
                Vec::new()
            }
        }
    }

    pub(super) async fn publish_background_completion(
        &self,
        team_name: String,
        member_name: String,
        message: TeamMessage,
    ) {
        let team_name_for_wake = team_name.clone();
        if let Err(error) = self
            .mutate_team("background_completion_published", move |state| {
                let Some(team) = state.teams.get_mut(&team_name) else {
                    return Ok(((), false));
                };
                if let Some(member) = team.members.get_mut(&member_name) {
                    member.is_active = false;
                }
                team.mailboxes
                    .entry("team-lead".to_string())
                    .or_default()
                    .push(message);
                Ok(((), true))
            })
            .await
        {
            tracing::warn!("持久化后台完成消息失败: {error}");
            return;
        }
        // 若 lead 正在自己的工具调用里等待邮箱（TaskList wait），立即送达；
        // 这不是唤醒主循环——主循环只能被用户 / cron 消息驱动。
        self.wake_mailbox(&team_name_for_wake, "team-lead").await;
    }

    pub(super) async fn create_team(
        &self,
        team_name: String,
        description: Option<String>,
        agent_type: Option<String>,
    ) -> Result<TeamRecord, String> {
        self.mutate_team("team_created", move |state| {
            if let Some(existing_team) = state.active_team.as_deref() {
                return Err(format!(
                    "Already leading team \"{existing_team}\". Use TeamDelete to end the current team before creating a new one."
                ));
            }
            state.active_team = Some(team_name.clone());
            let team = TeamRecord::new(team_name.clone(), description, agent_type);
            state.teams.insert(team_name, team.clone());
            Ok((team, true))
        })
        .await
    }

    pub(super) async fn delete_team(&self, team_name: String) -> Result<Value, String> {
        self.mutate_team("team_deleted", move |state| {
            if let Some(team) = state.teams.get(&team_name) {
                let active_members = team
                    .members
                    .values()
                    .filter(|member| member.name != "team-lead" && member.is_active)
                    .map(|member| {
                        member
                            .agent_type
                            .as_deref()
                            .map(|agent_type| format!("{} ({agent_type})", member.name))
                            .unwrap_or_else(|| member.name.clone())
                    })
                    .collect::<Vec<_>>();
                if !active_members.is_empty() {
                    return Ok((
                        json!({
                            "success": false,
                            "message": format!(
                                "Cannot cleanup team with {} active member(s): {}. Send shutdown_request first and wait for them to finish.",
                                active_members.len(),
                                active_members.join(", ")
                            ),
                            "team_name": team_name
                        }),
                        false,
                    ));
                }
            }
            let removed = state.teams.remove(&team_name).is_some();
            let cleared_active = state.active_team.as_deref() == Some(team_name.as_str());
            if cleared_active {
                state.active_team = None;
            }
            let success = removed || cleared_active;
            Ok((
                json!({
                    "success": success,
                    "message": if success {
                        format!("Cleaned up team \"{}\"", team_name)
                    } else {
                        format!("Team not found: {team_name}")
                    },
                    "team_name": team_name
                }),
                success,
            ))
        })
        .await
    }

    pub(super) async fn create_task(
        &self,
        team_name: String,
        subject: String,
        description: String,
        active_form: Option<String>,
        metadata: Option<Value>,
    ) -> Result<(String, String), String> {
        self.mutate_team("task_created", move |state| {
            let team = state
                .teams
                .get_mut(&team_name)
                .ok_or_else(|| format!("Team not found: {team_name}. Call TeamCreate first."))?;
            let id = team.next_task_id.to_string();
            team.next_task_id += 1;
            team.tasks.insert(
                id.clone(),
                TeamTask {
                    id: id.clone(),
                    subject: subject.clone(),
                    description,
                    active_form,
                    status: "queued".to_string(),
                    owner: None,
                    blocks: Vec::new(),
                    blocked_by: Vec::new(),
                    metadata,
                },
            );
            Ok(((id, subject), true))
        })
        .await
    }

    pub(super) async fn update_task(
        &self,
        team_name: String,
        task_id: String,
        input: Value,
    ) -> Result<Value, String> {
        self.mutate_team("task_updated", move |state| {
            let team = state
                .teams
                .get_mut(&team_name)
                .ok_or_else(|| format!("Team not found: {team_name}"))?;
            if get_str(&input, "status") == Some("deleted") {
                let removed = team.tasks.remove(&task_id).is_some();
                return Ok((
                    json!({
                        "success": removed,
                        "taskId": task_id,
                        "updatedFields": ["deleted"]
                    }),
                    removed,
                ));
            }
            let Some(task) = team.tasks.get_mut(&task_id) else {
                return Ok((
                    json!({
                        "success": false,
                        "taskId": task_id,
                        "updatedFields": [],
                        "error": "Task not found"
                    }),
                    false,
                ));
            };
            let mut updated = Vec::new();
            if let Some(subject) = get_str(&input, "subject") {
                task.subject = subject.to_string();
                updated.push("subject");
            }
            if let Some(description) = get_str(&input, "description") {
                task.description = description.to_string();
                updated.push("description");
            }
            if let Some(active_form) = get_str(&input, "activeForm") {
                task.active_form = Some(active_form.to_string());
                updated.push("activeForm");
            }
            if let Some(status) = get_str(&input, "status") {
                let normalized = match status {
                    "queued" | "running" | "waiting" | "cancelling" | "succeeded" | "failed"
                    | "cancelled" => Some(status),
                    "pending" => Some("queued"),
                    "in_progress" => Some("running"),
                    "completed" => Some("succeeded"),
                    _ => None,
                };
                let Some(normalized) = normalized else {
                    return Err(format!("invalid status for TaskUpdate: {status}"));
                };
                if !valid_task_state_transition(&task.status, normalized) {
                    return Err(format!(
                        "invalid task lifecycle transition: {} -> {}",
                        task.status, normalized
                    ));
                }
                task.status = normalized.to_string();
                updated.push("status");
                if normalized == "running" && task.owner.is_none() {
                    if let Ok(identity) = CURRENT_AGENT.try_with(Clone::clone) {
                        task.owner = Some(identity.name);
                        updated.push("owner");
                    }
                }
            }
            if let Some(owner) = get_str(&input, "owner") {
                let owner = owner.trim();
                if owner.is_empty() {
                    return Err("owner must not be empty".to_string());
                }
                task.owner = Some(owner.to_string());
                updated.push("owner");
            }
            if let Some(items) = input.get("addBlocks").and_then(Value::as_array) {
                for item in items.iter().filter_map(Value::as_str) {
                    if !task.blocks.iter().any(|existing| existing == item) {
                        task.blocks.push(item.to_string());
                    }
                }
                updated.push("blocks");
            }
            if let Some(items) = input.get("addBlockedBy").and_then(Value::as_array) {
                for item in items.iter().filter_map(Value::as_str) {
                    if !task.blocked_by.iter().any(|existing| existing == item) {
                        task.blocked_by.push(item.to_string());
                    }
                }
                updated.push("blockedBy");
            }
            if input.get("metadata").is_some() {
                task.metadata = input.get("metadata").cloned();
                updated.push("metadata");
            }
            let changed = !updated.is_empty();
            Ok((
                json!({
                    "success": changed,
                    "taskId": task_id,
                    "updatedFields": updated,
                    "statusChange": null,
                    "error": if changed {
                        Value::Null
                    } else {
                        Value::String("No update fields provided".to_string())
                    }
                }),
                changed,
            ))
        })
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn send_message(
        &self,
        team_name: String,
        sender: String,
        sender_agent_id: String,
        to: String,
        content: String,
        summary: Option<String>,
        timestamp: String,
        wake: bool,
    ) -> Result<Value, String> {
        let team_name_for_wake = team_name.clone();
        let response = self
            .mutate_team("message_sent", move |state| {            let team = state
                .teams
                .get_mut(&team_name)
                .ok_or_else(|| format!("Team not found: {team_name}"))?;
            let sender_inserted = if !team.members.contains_key(&sender) {
                team.members.insert(
                    sender.clone(),
                    TeamMember {
                        agent_id: sender_agent_id,
                        name: sender.clone(),
                        agent_type: None,
                        is_active: true,
                    },
                );
                true
            } else {
                false
            };
            let recipients = if to == "*" {
                team.members
                    .values()
                    .filter(|member| member.name != sender && member.is_active)
                    .map(|member| member.name.clone())
                    .collect::<Vec<_>>()
            } else if let Some(member_name) = team
                .members
                .values()
                .find(|member| member.name == to || member.agent_id == to)
                .map(|member| member.name.clone())
            {
                vec![member_name]
            } else {
                return Err(format!("Team member not found: {to}"));
            };
            if recipients.is_empty() {
                return Ok((
                    json!({
                        "success": true,
                        "skipped": true,
                        "message": "No active teammates to broadcast to",
                        "recipients": []
                    }),
                    sender_inserted,
                ));
            }
            for recipient in &recipients {
                team.mailboxes
                    .entry(recipient.clone())
                    .or_default()
                    .push(TeamMessage {
                        from: sender.clone(),
                        to: recipient.clone(),
                        summary: summary.clone(),
                        content: content.clone(),
                        ts: timestamp.clone(),
                        validated_media_markers: Vec::new(),
                        media_publish_attempted: false,
                    });
            }
            let observed_by_team_lead = sender != "team-lead"
                && !recipients.iter().any(|recipient| recipient == "team-lead");
            if observed_by_team_lead {
                let observed_target = if to == "*" {
                    "*".to_string()
                } else {
                    recipients.join(",")
                };
                team.mailboxes
                    .entry("team-lead".to_string())
                    .or_default()
                    .push(TeamMessage {
                        from: sender.clone(),
                        to: observed_target,
                        summary: summary.clone(),
                        content: content.clone(),
                        ts: timestamp,
                        validated_media_markers: Vec::new(),
                        media_publish_attempted: false,
                    });
            }
            Ok((
                json!({
                    "success": true,
                    "message": if to == "*" {
                        format!("Message broadcast to {} teammate(s): {}", recipients.len(), recipients.join(", "))
                    } else {
                        format!("Message sent to {}'s inbox", to)
                    },
                    "recipients": recipients,
                    "observed_by_team_lead": observed_by_team_lead,
                    "routing": {
                        "sender": sender,
                        "target": if to == "*" { "@team".to_string() } else { format!("@{to}") },
                        "summary": summary,
                        "content": content
                    }
                }),
                true,
            ))
        })
        .await?;
        // M2 显式唤醒：wake=true 时唤醒【收件人】的邮箱等待者（AgentWait /
        // TaskList wait 的等待）。只唤醒收件人，不唤醒 team-lead 的观察副本——
        // 硬规则：子 agent 不能唤醒主循环，root 的邮箱只在它自己自然的轮次边界
        // （或它主动调用 TaskList(wait) 时）被读取。
        if wake && response["success"].as_bool() == Some(true) {
            if let Some(recipients) = response["recipients"].as_array() {
                let recipients = recipients
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>();
                for recipient in recipients {
                    self.wake_mailbox(&team_name_for_wake, &recipient).await;
                }
            }
        }
        Ok(response)
    }

    pub(super) async fn transition_execution(
        &self,
        agent_id: &str,
        state: ExecutionState,
        completed_steps: usize,
        stage: impl Into<String>,
        outcome: ExecutionOutcomeUpdate,
        event: &str,
    ) -> Result<ExecutionSnapshot, String> {
        let (snapshot, notify) = {
            let mut executions = self.executions.lock().await;
            let record = executions
                .get_mut(agent_id)
                .ok_or_else(|| format!("unknown agent execution: {agent_id}"))?;
            if !valid_execution_state_transition(&record.snapshot.state, &state) {
                return Err(format!(
                    "invalid execution lifecycle transition: {:?} -> {:?}",
                    record.snapshot.state, state
                ));
            }
            let mut snapshot = record.snapshot.clone();
            snapshot.state = state;
            snapshot.progress.completed_steps = completed_steps.min(snapshot.progress.total_steps);
            snapshot.progress.stage = stage.into();
            snapshot.handoff = outcome.handoff;
            snapshot.error = outcome.error;
            snapshot.validated_media_markers = outcome.validated_media_markers;
            snapshot.media_publish_attempted = outcome.media_publish_attempted;
            snapshot.updated_at = chrono::Utc::now().to_rfc3339();
            let entry = ExecutionLogEntry {
                event: event.to_string(),
                timestamp: snapshot.updated_at.clone(),
                snapshot: snapshot.clone(),
            };
            append_execution_log(&self.execution_dir, &entry)?;
            record.snapshot = snapshot.clone();
            if record.snapshot.state.is_terminal() {
                record.cancel = None;
            }
            record.events.push(entry);
            trim_execution_events(&mut record.events);
            (snapshot, Arc::clone(&record.notify))
        };
        notify.notify_waiters();
        Ok(snapshot)
    }

    pub(super) async fn insert_queued_execution(
        &self,
        agent_id: String,
        spec: AgentTaskSpec,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        let snapshot = ExecutionSnapshot {
            agent_id: agent_id.clone(),
            progress: ExecutionProgress {
                completed_steps: 0,
                total_steps: spec.max_tool_iter,
                stage: "等待执行".to_string(),
            },
            spec,
            state: ExecutionState::Queued,
            handoff: None,
            error: None,
            validated_media_markers: Vec::new(),
            media_publish_attempted: false,
            created_at: now.clone(),
            updated_at: now.clone(),
            recovered: false,
        };
        let entry = ExecutionLogEntry {
            event: "queued".to_string(),
            timestamp: now,
            snapshot: snapshot.clone(),
        };
        append_execution_log(&self.execution_dir, &entry)?;
        self.executions.lock().await.insert(
            agent_id,
            ExecutionRecord {
                snapshot,
                events: vec![entry],
                cancel: Some(cancel),
                notify: Arc::new(Notify::new()),
            },
        );
        Ok(())
    }

    pub(super) async fn execution_payload(&self, agent_id: &str) -> Option<Value> {
        let executions = self.executions.lock().await;
        let record = executions.get(agent_id)?;
        let events = record
            .events
            .iter()
            .map(|entry| {
                json!({
                    "event": entry.event,
                    "timestamp": entry.timestamp,
                    "state": entry.snapshot.state,
                    "progress": entry.snapshot.progress,
                })
            })
            .collect::<Vec<_>>();
        Some(json!({
            "execution": record.snapshot,
            "events": events,
            "eventLog": execution_log_path(&self.execution_dir, agent_id),
            "cancellable": record.cancel.is_some() && !record.snapshot.state.is_terminal(),
            "resumable": record.cancel.is_none() && record.snapshot.state == ExecutionState::Waiting,
        }))
    }

    pub(super) async fn execution_list_payload(&self) -> Value {
        let executions = self.executions.lock().await;
        let mut snapshots = executions
            .values()
            .map(|record| record.snapshot.clone())
            .collect::<Vec<_>>();
        snapshots.sort_by(|left, right| right.created_at.cmp(&left.created_at));
        let total = snapshots.len();
        let executions = snapshots
            .into_iter()
            .take(100)
            .map(|snapshot| {
                json!({
                    "agent_id": snapshot.agent_id,
                    "state": snapshot.state,
                    "progress": snapshot.progress,
                    "goal": truncate_chars(&snapshot.spec.task, 240),
                    "isolation": snapshot.spec.isolation,
                    "created_at": snapshot.created_at,
                    "updated_at": snapshot.updated_at,
                    "recovered": snapshot.recovered,
                })
            })
            .collect::<Vec<_>>();
        json!({
            "executions": executions,
            "total": total,
            "truncated": total > 100,
        })
    }

    pub(super) async fn request_cancellation(
        &self,
        agent_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        let (cancel, notify) = {
            let mut executions = self.executions.lock().await;
            let record = executions
                .get_mut(agent_id)
                .ok_or_else(|| format!("unknown agent execution: {agent_id}"))?;
            if record.snapshot.state.is_terminal() {
                return Ok(());
            }
            let Some(cancel) = record.cancel.clone() else {
                return Err(format!(
                    "execution {agent_id} 没有活动取消句柄；若它是重启后恢复的 waiting 任务，请先 AgentResume"
                ));
            };
            let mut snapshot = record.snapshot.clone();
            snapshot.state = ExecutionState::Cancelling;
            snapshot.progress.stage = format!("正在取消：{}", reason.trim());
            snapshot.handoff = None;
            snapshot.error = None;
            snapshot.validated_media_markers.clear();
            snapshot.media_publish_attempted = false;
            snapshot.updated_at = chrono::Utc::now().to_rfc3339();
            let entry = ExecutionLogEntry {
                event: "cancelling".to_string(),
                timestamp: snapshot.updated_at.clone(),
                snapshot: snapshot.clone(),
            };
            if let Err(error) = append_execution_log(&self.execution_dir, &entry) {
                // 持久化异常不能阻止真实取消句柄触发；否则“取消失败”的恰恰是最需要停止的任务。
                cancel.cancel();
                return Err(error);
            }
            record.snapshot = snapshot;
            record.events.push(entry);
            trim_execution_events(&mut record.events);
            (cancel, Arc::clone(&record.notify))
        };
        cancel.cancel();
        notify.notify_waiters();
        Ok(())
    }

    pub(super) async fn wait_execution(
        &self,
        agent_id: &str,
        timeout_ms: u64,
    ) -> Result<Value, String> {
        let deadline =
            tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.min(300_000));
        loop {
            let (terminal, notify) = {
                let executions = self.executions.lock().await;
                let record = executions
                    .get(agent_id)
                    .ok_or_else(|| format!("unknown agent execution: {agent_id}"))?;
                (
                    record.snapshot.state.is_terminal(),
                    Arc::clone(&record.notify),
                )
            };
            if terminal {
                break;
            }
            let poll_deadline =
                deadline.min(tokio::time::Instant::now() + std::time::Duration::from_millis(200));
            if tokio::time::timeout_at(poll_deadline, notify.notified())
                .await
                .is_err()
                && tokio::time::Instant::now() >= deadline
            {
                break;
            }
        }
        self.execution_payload(agent_id)
            .await
            .ok_or_else(|| format!("unknown agent execution: {agent_id}"))
    }

    pub(super) async fn resumable_spec(&self, agent_id: &str) -> Result<AgentTaskSpec, String> {
        let executions = self.executions.lock().await;
        let record = executions
            .get(agent_id)
            .ok_or_else(|| format!("unknown agent execution: {agent_id}"))?;
        if record.snapshot.state != ExecutionState::Waiting || record.cancel.is_some() {
            return Err(format!(
                "execution {agent_id} 当前状态为 {:?}，不可恢复",
                record.snapshot.state
            ));
        }
        Ok(record.snapshot.spec.clone())
    }

    pub(super) async fn commit_resume(
        &self,
        agent_id: &str,
    ) -> Result<(CancellationToken, Arc<Notify>), String> {
        let mut executions = self.executions.lock().await;
        let record = executions
            .get_mut(agent_id)
            .ok_or_else(|| format!("unknown agent execution: {agent_id}"))?;
        if record.snapshot.state != ExecutionState::Waiting || record.cancel.is_some() {
            return Err(format!(
                "execution {agent_id} 当前状态为 {:?}，不可恢复",
                record.snapshot.state
            ));
        }
        let cancel = CancellationToken::new();
        let mut snapshot = record.snapshot.clone();
        snapshot.state = ExecutionState::Queued;
        snapshot.progress.completed_steps = 0;
        snapshot.progress.stage = "恢复任务已排队".to_string();
        snapshot.handoff = None;
        snapshot.error = None;
        snapshot.validated_media_markers.clear();
        snapshot.media_publish_attempted = false;
        snapshot.updated_at = chrono::Utc::now().to_rfc3339();
        snapshot.recovered = false;
        let entry = ExecutionLogEntry {
            event: "resumed_queued".to_string(),
            timestamp: snapshot.updated_at.clone(),
            snapshot: snapshot.clone(),
        };
        append_execution_log(&self.execution_dir, &entry)?;
        record.snapshot = snapshot;
        record.cancel = Some(cancel.clone());
        record.events.push(entry);
        trim_execution_events(&mut record.events);
        Ok((cancel, Arc::clone(&record.notify)))
    }

    pub(super) fn execution_log_path(&self, agent_id: &str) -> PathBuf {
        execution_log_path(&self.execution_dir, agent_id)
    }

    #[cfg(test)]
    pub(super) fn storage_dir(&self) -> &Path {
        &self.execution_dir
    }

    #[cfg(test)]
    pub(super) async fn insert_recovered_execution_for_test(
        &self,
        agent_id: String,
        snapshot: ExecutionSnapshot,
    ) {
        self.executions.lock().await.insert(
            agent_id,
            ExecutionRecord {
                snapshot,
                events: Vec::new(),
                cancel: None,
                notify: Arc::new(Notify::new()),
            },
        );
    }

    #[cfg(test)]
    pub(super) async fn execution_state_for_test(
        &self,
        agent_id: &str,
    ) -> Option<(ExecutionSnapshot, usize, bool)> {
        let executions = self.executions.lock().await;
        let record = executions.get(agent_id)?;
        Some((
            record.snapshot.clone(),
            record.events.len(),
            record.cancel.is_some(),
        ))
    }
}

fn trim_execution_events(events: &mut Vec<ExecutionLogEntry>) {
    if events.len() > EXECUTION_EVENT_TAIL {
        events.drain(..events.len() - EXECUTION_EVENT_TAIL);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct TeamRecord {
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) lead_agent_id: String,
    pub(super) members: BTreeMap<String, TeamMember>,
    pub(super) tasks: BTreeMap<String, TeamTask>,
    pub(super) mailboxes: HashMap<String, Vec<TeamMessage>>,
    pub(super) next_task_id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TeamMember {
    pub(super) agent_id: String,
    pub(super) name: String,
    pub(super) agent_type: Option<String>,
    pub(super) is_active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TeamTask {
    pub(super) id: String,
    pub(super) subject: String,
    pub(super) description: String,
    pub(super) active_form: Option<String>,
    pub(super) status: String,
    pub(super) owner: Option<String>,
    pub(super) blocks: Vec<String>,
    pub(super) blocked_by: Vec<String>,
    pub(super) metadata: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TeamMessage {
    pub(super) from: String,
    pub(super) to: String,
    pub(super) summary: Option<String>,
    pub(super) content: String,
    pub(super) ts: String,
    /// 仅后台 Agent 完成消息可设置；普通 SendMessage 永远保持为空。
    #[serde(default)]
    pub(super) validated_media_markers: Vec<String>,
    #[serde(default)]
    pub(super) media_publish_attempted: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct TeamStateLogEntry {
    pub(super) event: String,
    pub(super) timestamp: String,
    pub(super) state: TeamRuntimeState,
}

impl TeamRecord {
    pub(super) fn new(
        name: String,
        description: Option<String>,
        lead_agent_type: Option<String>,
    ) -> Self {
        let lead_agent_id = format_agent_id("team-lead", &name);
        let mut members = BTreeMap::new();
        members.insert(
            "team-lead".to_string(),
            TeamMember {
                agent_id: lead_agent_id.clone(),
                name: "team-lead".to_string(),
                agent_type: lead_agent_type,
                is_active: true,
            },
        );
        TeamRecord {
            name,
            description,
            lead_agent_id,
            members,
            tasks: BTreeMap::new(),
            mailboxes: HashMap::new(),
            next_task_id: 1,
        }
    }
}

pub(super) fn format_agent_id(name: &str, team_name: &str) -> String {
    format!(
        "{}@{}",
        sanitize_agent_name(name),
        sanitize_agent_name(team_name)
    )
}

pub(crate) fn sanitize_agent_name(s: &str) -> String {
    let mut out = String::new();
    for ch in s.trim().chars() {
        if ch.is_alphanumeric() || ch == '-' || ch == '_' {
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        } else if ch.is_whitespace() {
            out.push('-');
        }
    }
    if out.is_empty() {
        "agent".to_string()
    } else {
        out
    }
}

pub(super) fn valid_task_state_transition(from: &str, to: &str) -> bool {
    from == to
        || matches!(
            (from, to),
            (
                "queued",
                "running" | "waiting" | "cancelling" | "cancelled" | "succeeded" | "failed"
            ) | (
                "running",
                "waiting" | "cancelling" | "succeeded" | "failed" | "cancelled"
            ) | (
                "waiting",
                "running" | "cancelling" | "succeeded" | "failed" | "cancelled"
            ) | ("cancelling", "succeeded" | "cancelled" | "failed")
        )
}

pub(super) fn valid_execution_state_transition(from: &ExecutionState, to: &ExecutionState) -> bool {
    from == to
        || matches!(
            (from, to),
            (
                ExecutionState::Queued,
                ExecutionState::Running
                    | ExecutionState::Cancelling
                    | ExecutionState::Cancelled
                    | ExecutionState::Failed
            ) | (
                ExecutionState::Running,
                ExecutionState::Waiting
                    | ExecutionState::Cancelling
                    | ExecutionState::Succeeded
                    | ExecutionState::Failed
                    | ExecutionState::Cancelled
            ) | (
                ExecutionState::Waiting,
                ExecutionState::Queued
                    | ExecutionState::Running
                    | ExecutionState::Cancelling
                    | ExecutionState::Succeeded
                    | ExecutionState::Failed
                    | ExecutionState::Cancelled
            ) | (
                ExecutionState::Cancelling,
                // Cancelling 已是持久化成功的用户意图。若仍允许提交 Succeeded，
                // 后台结果携带的媒体 marker 会在用户取消后继续经邮箱向上游传播。
                ExecutionState::Cancelled | ExecutionState::Failed
            )
        )
}

pub(super) fn next_execution_id(name: &str) -> String {
    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    let stem = sanitize_agent_name(name)
        .chars()
        .take(48)
        .collect::<String>();
    format!(
        "{}-{}-{}",
        stem,
        chrono::Utc::now().timestamp_micros(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    )
}

pub(super) fn execution_log_path(dir: &Path, agent_id: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    agent_id.hash(&mut hasher);
    dir.join(format!(
        "{}-{:016x}.jsonl",
        sanitize_agent_name(agent_id),
        hasher.finish()
    ))
}

pub(super) fn append_execution_log(dir: &Path, entry: &ExecutionLogEntry) -> Result<(), String> {
    fs::create_dir_all(dir)
        .map_err(|error| format!("创建任务事件目录失败（{}）: {error}", dir.display()))?;
    let path = execution_log_path(dir, &entry.snapshot.agent_id);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("打开任务事件日志失败（{}）: {error}", path.display()))?;
    let line =
        serde_json::to_string(entry).map_err(|error| format!("序列化任务事件失败: {error}"))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .map_err(|error| format!("写入任务事件日志失败（{}）: {error}", path.display()))
}

pub(super) fn team_state_log_path(dir: &Path) -> PathBuf {
    dir.join("team-state.jsonl")
}

pub(super) fn append_team_state_log(
    dir: &Path,
    event: &str,
    state: &TeamRuntimeState,
) -> Result<(), String> {
    fs::create_dir_all(dir)
        .map_err(|error| format!("创建团队事件目录失败（{}）: {error}", dir.display()))?;
    let path = team_state_log_path(dir);
    let entry = TeamStateLogEntry {
        event: event.to_string(),
        timestamp: chrono::Utc::now().to_rfc3339(),
        state: state.clone(),
    };
    let line =
        serde_json::to_string(&entry).map_err(|error| format!("序列化团队事件失败: {error}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|error| format!("打开团队事件日志失败（{}）: {error}", path.display()))?;
    file.write_all(line.as_bytes())
        .and_then(|_| file.write_all(b"\n"))
        .and_then(|_| file.flush())
        .map_err(|error| format!("写入团队事件日志失败（{}）: {error}", path.display()))
}

pub(super) fn recover_team_state(dir: &Path) -> TeamRuntimeState {
    let path = team_state_log_path(dir);
    let Ok(file) = fs::File::open(&path) else {
        return TeamRuntimeState::default();
    };
    let Some(mut state) = BufReader::new(file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| serde_json::from_str::<TeamStateLogEntry>(&line).ok())
        .last()
        .map(|entry| entry.state)
    else {
        return TeamRuntimeState::default();
    };

    let mut changed = false;
    if state
        .active_team
        .as_ref()
        .is_some_and(|name| !state.teams.contains_key(name))
    {
        state.active_team = None;
        changed = true;
    }
    for team in state.teams.values_mut() {
        for member in team.members.values_mut() {
            // 子 agent 的执行任务不会跨进程存活；team-lead 则代表恢复后继续接管该
            // active team 的主 agent，必须保持可接收广播的活动状态。
            let should_be_active = member.name == "team-lead";
            if member.is_active != should_be_active {
                member.is_active = should_be_active;
                changed = true;
            }
        }
        for task in team.tasks.values_mut() {
            let recovered = match task.status.as_str() {
                "queued" | "running" | "waiting" => Some("waiting"),
                "cancelling" => Some("cancelled"),
                _ => None,
            };
            if let Some(recovered) = recovered {
                if task.status != recovered {
                    task.status = recovered.to_string();
                    changed = true;
                }
            }
        }
    }
    if changed {
        if let Err(error) = append_team_state_log(dir, "recovered_after_restart", &state) {
            tracing::warn!("{error}");
        }
    }
    state
}

pub(super) fn recover_execution_records(dir: &Path) -> HashMap<String, ExecutionRecord> {
    let mut records = HashMap::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return records;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = fs::File::open(&path) else {
            continue;
        };
        let mut events = BufReader::new(file)
            .lines()
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<ExecutionLogEntry>(&line).ok())
            .collect::<Vec<_>>();
        let Some(last) = events.last().cloned() else {
            continue;
        };
        let mut snapshot = last.snapshot;
        if snapshot.state == ExecutionState::Cancelling {
            snapshot.state = ExecutionState::Cancelled;
            snapshot.progress.stage = "进程重启后确认原取消请求已生效".to_string();
            snapshot.updated_at = chrono::Utc::now().to_rfc3339();
            snapshot.recovered = true;
            let recovered = ExecutionLogEntry {
                event: "recovered_cancelled".to_string(),
                timestamp: snapshot.updated_at.clone(),
                snapshot: snapshot.clone(),
            };
            if let Err(error) = append_execution_log(dir, &recovered) {
                tracing::warn!("{error}");
            }
            events.push(recovered);
        } else if !snapshot.state.is_terminal() {
            snapshot.state = ExecutionState::Waiting;
            snapshot.progress.stage =
                "进程重启后已恢复任务记录；调用 AgentResume 继续执行".to_string();
            snapshot.updated_at = chrono::Utc::now().to_rfc3339();
            snapshot.recovered = true;
            let recovered = ExecutionLogEntry {
                event: "recovered_waiting".to_string(),
                timestamp: snapshot.updated_at.clone(),
                snapshot: snapshot.clone(),
            };
            if let Err(error) = append_execution_log(dir, &recovered) {
                tracing::warn!("{error}");
            }
            events.push(recovered);
        }
        if events.len() > EXECUTION_EVENT_TAIL {
            events.drain(..events.len() - EXECUTION_EVENT_TAIL);
        }
        records.insert(
            snapshot.agent_id.clone(),
            ExecutionRecord {
                snapshot,
                events,
                cancel: None,
                notify: Arc::new(Notify::new()),
            },
        );
    }
    records
}

pub(super) fn render_inbound_team_messages(messages: &[TeamMessage], reader_name: &str) -> String {
    let mut out = String::from("## Incoming Team Messages\n");
    let mut has_observed_copy = false;
    for msg in messages {
        let observed_copy = reader_name == "team-lead" && msg.to != "team-lead";
        has_observed_copy |= observed_copy;
        out.push_str(&format!(
            "\n<team-message from=\"{}\" to=\"{}\" time=\"{}\"{}>\n",
            msg.from,
            msg.to,
            msg.ts,
            if observed_copy {
                " observed-copy=\"true\""
            } else {
                ""
            }
        ));
        if let Some(summary) = msg.summary.as_deref() {
            out.push_str(&format!(
                "summary: {}\n\n",
                strip_untrusted_media_control_lines(summary).trim()
            ));
        }
        out.push_str(strip_untrusted_media_control_lines(&msg.content).trim());
        append_validated_media_state(
            &mut out,
            &msg.validated_media_markers,
            msg.media_publish_attempted,
        );
        out.push_str("\n</team-message>\n");
    }
    out.push_str(
        "\nPlease incorporate these teammate messages. If a message addressed directly to you challenges your assumptions, answer the challenge and revise your conclusion when the evidence warrants it. If you receive a shutdown_request, stop taking new work, finish any necessary status update, and return your final message promptly.\n",
    );
    if has_observed_copy {
        out.push_str(
            "Messages marked observed-copy are peer-to-peer coordination copies. Audit their progress and evidence, but do not answer on the recipient's behalf.\n",
        );
    }
    out
}
