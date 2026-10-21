use super::*;

pub struct TeamCreateTool {
    runtime: AgentToolRuntime,
}

impl TeamCreateTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        TeamCreateTool { runtime }
    }
}

#[async_trait]
impl Tool for TeamCreateTool {
    fn name(&self) -> &str {
        "TeamCreate"
    }

    fn description(&self) -> &str {
        "Create a multi-agent team with a shared task list and mailbox."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "team_name": {"type": "string"},
                "description": {"type": "string"},
                "agent_type": {"type": "string"}
            },
            "required": ["team_name"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(team_name) = get_str(&input, "team_name").filter(|s| !s.trim().is_empty()) else {
            return ToolResult::err("team_name is required for TeamCreate");
        };
        // sanitize_agent_name 对无字母数字的输入会回退为 "agent"（供成员名兜底），
        // 但团队名必须至少含一个字母或数字，否则拒绝——避免静默创建名为 "agent" 的团队。
        if !team_name.chars().any(|c| c.is_alphanumeric()) {
            return ToolResult::err("team_name must contain a letter or number");
        }
        let team_name = sanitize_agent_name(team_name);
        let description = get_str(&input, "description").map(str::to_string);
        let agent_type = get_str(&input, "agent_type").map(str::to_string);
        let team = match self
            .runtime
            .collaboration
            .create_team(team_name, description, agent_type)
            .await
        {
            Ok(team) => team,
            Err(error) => return ToolResult::err(error),
        };
        ToolResult::ok(
            json!({
                "team_name": team.name,
                "description": team.description,
                // 团队状态不落独立文件：在 state.sqlite 的 team_state 事件流里（整表快照）。
                "team_state": format!("state.sqlite journal {JOURNAL_TEAM_STATE}"),
                "lead_agent_id": team.lead_agent_id
            })
            .to_string(),
        )
    }
}

pub struct TeamDeleteTool {
    runtime: AgentToolRuntime,
}

impl TeamDeleteTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        TeamDeleteTool { runtime }
    }
}

#[async_trait]
impl Tool for TeamDeleteTool {
    fn name(&self) -> &str {
        "TeamDelete"
    }

    fn description(&self) -> &str {
        "Clean up the current multi-agent team."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "team_name": {"type": "string"}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let requested = get_str(&input, "team_name");
        let team_name = match self.runtime.resolve_team_name(requested).await {
            Ok(Some(team_name)) => team_name,
            Ok(None) => {
                return ToolResult::ok(
                    json!({
                        "success": true,
                        "message": "No team name found, nothing to clean up"
                    })
                    .to_string(),
                );
            }
            Err(error) => return ToolResult::err(error),
        };
        match self.runtime.collaboration.delete_team(team_name).await {
            Ok(response) if response["success"].as_bool() == Some(false) => {
                ToolResult::err(response.to_string())
            }
            Ok(response) => ToolResult::ok(response.to_string()),
            Err(error) => ToolResult::err(error),
        }
    }
}

pub struct TaskCreateTool {
    runtime: AgentToolRuntime,
}

impl TaskCreateTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        TaskCreateTool { runtime }
    }
}

#[async_trait]
impl Tool for TaskCreateTool {
    fn name(&self) -> &str {
        "TaskCreate"
    }

    fn description(&self) -> &str {
        "Create a task in the current team's shared task list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subject": {"type": "string"},
                "description": {"type": "string"},
                "activeForm": {"type": "string"},
                "metadata": {"type": "object"},
                "team_name": {"type": "string"}
            },
            "required": ["subject", "description"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(subject) = get_str(&input, "subject").filter(|s| !s.trim().is_empty()) else {
            return ToolResult::err("subject is required for TaskCreate");
        };
        let Some(description) = get_str(&input, "description") else {
            return ToolResult::err("description is required for TaskCreate");
        };
        let team_name = match self
            .runtime
            .resolve_team_name(get_str(&input, "team_name"))
            .await
        {
            Ok(Some(team_name)) => team_name,
            Ok(None) => {
                return ToolResult::err(
                    "TaskCreate requires an active team. Call TeamCreate first.",
                );
            }
            Err(error) => return ToolResult::err(error),
        };
        let subject = subject.to_string();
        let description = description.to_string();
        let active_form = get_str(&input, "activeForm").map(str::to_string);
        let metadata = input.get("metadata").cloned();
        match self
            .runtime
            .collaboration
            .create_task(team_name, subject, description, active_form, metadata)
            .await
        {
            Ok((id, subject)) => {
                ToolResult::ok(format!("Task #{id} created successfully: {subject}"))
            }
            Err(error) => ToolResult::err(error),
        }
    }
}

pub struct TaskUpdateTool {
    runtime: AgentToolRuntime,
}

impl TaskUpdateTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        TaskUpdateTool { runtime }
    }
}

#[async_trait]
impl Tool for TaskUpdateTool {
    fn name(&self) -> &str {
        "TaskUpdate"
    }

    fn description(&self) -> &str {
        "Update a task in the current team's shared task list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "taskId": {"type": "string"},
                "subject": {"type": "string"},
                "description": {"type": "string"},
                "activeForm": {"type": "string"},
                "status": {"type": "string", "enum": ["queued", "running", "waiting", "cancelling", "succeeded", "failed", "cancelled", "deleted"]},
                "owner": {"type": "string"},
                "addBlocks": {"type": "array", "items": {"type": "string"}},
                "addBlockedBy": {"type": "array", "items": {"type": "string"}},
                "metadata": {"type": "object"},
                "team_name": {"type": "string"}
            },
            "required": ["taskId"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(task_id) = get_str(&input, "taskId").filter(|s| !s.trim().is_empty()) else {
            return ToolResult::err("taskId is required for TaskUpdate");
        };
        let team_name = match self
            .runtime
            .resolve_team_name(get_str(&input, "team_name"))
            .await
        {
            Ok(Some(team_name)) => team_name,
            Ok(None) => {
                return ToolResult::err(
                    "TaskUpdate requires an active team. Call TeamCreate first.",
                );
            }
            Err(error) => return ToolResult::err(error),
        };
        let task_id = task_id.trim().to_string();
        match self
            .runtime
            .collaboration
            .update_task(team_name, task_id, input)
            .await
        {
            Ok(response) if response["success"].as_bool() == Some(false) => {
                ToolResult::err(response.to_string())
            }
            Ok(response) => ToolResult::ok(response.to_string()),
            Err(error) => ToolResult::err(error),
        }
    }
}

pub struct TaskListTool {
    runtime: AgentToolRuntime,
}

impl TaskListTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        TaskListTool { runtime }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "List tasks in the current team's shared task list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "team_name": {"type": "string"},
                "wait_timeout_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 300000,
                    "description": "Optionally wait for this agent's team mailbox before listing. Returns early when a message arrives."
                }
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let team_name = match self
            .runtime
            .resolve_team_name(get_str(&input, "team_name"))
            .await
        {
            Ok(Some(team_name)) => team_name,
            Ok(None) => return ToolResult::ok("No tasks found"),
            Err(error) => return ToolResult::err(error),
        };
        let current_name = CURRENT_AGENT
            .try_with(|id| id.name.clone())
            .unwrap_or_else(|_| "team-lead".to_string());
        let wait_timeout_ms = match input.get("wait_timeout_ms") {
            None => 0,
            Some(value) => match value.as_u64() {
                Some(value) if value <= 300_000 => value,
                _ => return ToolResult::err("wait_timeout_ms must be an integer in 0..=300000"),
            },
        };
        if wait_timeout_ms > 0 {
            self.runtime
                .wait_for_mailbox_for_name(&team_name, &current_name, wait_timeout_ms)
                .await;
        }
        let mut lines = {
            let Some(team) = self.runtime.collaboration.team_snapshot(&team_name).await else {
                return ToolResult::ok("No tasks found");
            };
            let active_members = team
                .members
                .values()
                .filter(|member| member.name != "team-lead" && member.is_active)
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>();
            let mut lines = vec![format!(
                "Active teammates: {}",
                if active_members.is_empty() {
                    "none".to_string()
                } else {
                    active_members.join(", ")
                }
            )];
            for task in team.tasks.values() {
                let owner = task
                    .owner
                    .as_deref()
                    .map(|o| format!(" ({o})"))
                    .unwrap_or_default();
                let blocked = if task.blocked_by.is_empty() {
                    String::new()
                } else {
                    format!(" [blocked by #{}]", task.blocked_by.join(", #"))
                };
                lines.push(format!(
                    "#{} [{}] {}{}{}",
                    task.id, task.status, task.subject, owner, blocked
                ));
            }
            lines
        };
        let inbox = self
            .runtime
            .drain_mailbox_for_name(&team_name, &current_name)
            .await;
        if !inbox.is_empty() {
            // M1：team-lead（主循环）亲自消费邮箱 → 未读完成通知计数清零。
            // 子 agent 消费自己的邮箱不影响主循环的计数。
            if current_name == "team-lead" {
                super::reset_team_inbox_unread(&self.runtime.journal);
            }
            if !lines.is_empty() {
                lines.push(String::new());
            }
            lines.push(render_inbound_team_messages(&inbox, &current_name));
        }
        if lines.is_empty() {
            return ToolResult::ok("No tasks found");
        }
        ToolResult::ok(lines.join("\n"))
    }
}

pub struct TaskGetTool {
    runtime: AgentToolRuntime,
}

impl TaskGetTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        TaskGetTool { runtime }
    }
}

#[async_trait]
impl Tool for TaskGetTool {
    fn name(&self) -> &str {
        "TaskGet"
    }

    fn description(&self) -> &str {
        "Retrieve a task from the current team's shared task list."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "taskId": {"type": "string"},
                "team_name": {"type": "string"}
            },
            "required": ["taskId"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(task_id) = get_str(&input, "taskId").filter(|s| !s.trim().is_empty()) else {
            return ToolResult::err("taskId is required for TaskGet");
        };
        let task_id = task_id.trim();
        let team_name = match self
            .runtime
            .resolve_team_name(get_str(&input, "team_name"))
            .await
        {
            Ok(Some(team_name)) => team_name,
            Ok(None) => return ToolResult::ok("Task not found"),
            Err(error) => return ToolResult::err(error),
        };
        let Some(team) = self.runtime.collaboration.team_snapshot(&team_name).await else {
            return ToolResult::ok("Task not found");
        };
        let Some(task) = team.tasks.get(task_id) else {
            return ToolResult::ok("Task not found");
        };
        let mut lines = vec![
            format!("Task #{}: {}", task.id, task.subject),
            format!("Status: {}", task.status),
            format!("Description: {}", task.description),
        ];
        if let Some(owner) = task.owner.as_deref() {
            lines.push(format!("Owner: {owner}"));
        }
        if !task.blocked_by.is_empty() {
            lines.push(format!("Blocked by: #{}", task.blocked_by.join(", #")));
        }
        if !task.blocks.is_empty() {
            lines.push(format!("Blocks: #{}", task.blocks.join(", #")));
        }
        ToolResult::ok(lines.join("\n"))
    }
}

pub struct SendMessageTool {
    runtime: AgentToolRuntime,
}

impl SendMessageTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        SendMessageTool { runtime }
    }
}

#[async_trait]
impl Tool for SendMessageTool {
    fn name(&self) -> &str {
        "SendMessage"
    }

    fn description(&self) -> &str {
        "Send a message to another agent in the current team. Peer-to-peer messages are also visible to team-lead for coordination. By default the message is only delivered to the recipient's mailbox (read at their next natural turn or next mailbox wait). wake=true additionally wakes a teammate that is currently waiting on its mailbox; waking can never interrupt the main loop - the root/team-lead only reads its mailbox at its own natural turn boundaries."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "to": {"type": "string", "description": "Teammate name, or * to broadcast."},
                "message": {"description": "Plain text or structured JSON message."},
                "summary": {"type": "string"},
                "wake": {"type": "boolean", "description": "false (default): deliver only. true: also wake the recipient if it is currently waiting on its mailbox. The main loop (root) is never woken - hard rule."},
                "team_name": {"type": "string"}
            },
            "required": ["to", "message"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(to) = get_str(&input, "to").filter(|s| !s.trim().is_empty()) else {
            return ToolResult::err("to is required for SendMessage");
        };
        let Some(message_value) = input.get("message") else {
            return ToolResult::err("message is required for SendMessage");
        };
        let team_name = match self
            .runtime
            .resolve_team_name(get_str(&input, "team_name"))
            .await
        {
            Ok(Some(team_name)) => team_name,
            Ok(None) => {
                return ToolResult::err(
                    "SendMessage requires an active team. Call TeamCreate first.",
                );
            }
            Err(error) => return ToolResult::err(error),
        };
        let sender = CURRENT_AGENT
            .try_with(|id| id.name.clone())
            .unwrap_or_else(|_| "team-lead".to_string());
        let sender_agent_id = CURRENT_AGENT
            .try_with(|id| id.agent_id.clone())
            .unwrap_or_else(|_| format_agent_id(&sender, &team_name));
        let content = message_to_string(message_value);
        let summary = get_str(&input, "summary").map(str::to_string);
        let wake = get_bool(&input, "wake");
        let ts = chrono::Utc::now().to_rfc3339();
        let to = to.trim().trim_start_matches('@').to_string();
        match self
            .runtime
            .collaboration
            .send_message(
                team_name,
                sender,
                sender_agent_id,
                to,
                content,
                summary,
                ts,
                wake,
            )
            .await
        {
            Ok(response) => ToolResult::ok(response.to_string()),
            Err(error) => ToolResult::err(error),
        }
    }
}

pub struct ClaudeAgentTool {
    runtime: AgentToolRuntime,
    name: &'static str,
}

impl ClaudeAgentTool {
    pub fn new(runtime: AgentToolRuntime, name: &'static str) -> Self {
        ClaudeAgentTool { runtime, name }
    }
}

#[async_trait]
impl Tool for ClaudeAgentTool {
    fn name(&self) -> &str {
        self.name
    }

    fn description(&self) -> &str {
        "Run a named sub-agent. Compatible with Claude Code Agent and legacy Task tool names. \
         Do NOT delegate work that a single ordinary tool call can finish (reading a specific \
         file, a simple search, a quick lookup) — do it directly. The prompt must be \
         self-contained for an agent that cannot see this conversation: state the goal, whether \
         the work is research-only (produce findings, change nothing) or execution (may write \
         files / run commands), what evidence or artifacts to return, and a clear completion \
         criterion (when to stop). In team mode with an explicit name it runs as a persistent \
         background teammate."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "prompt": {"type": "string"},
                "subagent_type": {"type": "string"},
                "model": {"type": "string"},
                "run_in_background": {"type": "boolean"},
                "name": {"type": "string"},
                "team_name": {"type": "string"},
                "split_reason": {
                    "type": "string",
                    "enum": ["new_feedback", "independent_evidence", "parallel_external_execution"],
                    "description": "Required delegation justification. Do not split work for any other reason."
                },
                "max_tool_iter": {"type": "integer", "minimum": 1, "maximum": MAX_TOOL_ITER},
                "max_output_chars": {"type": "integer", "minimum": 1000, "maximum": MAX_OUTPUT_CHARS}
            },
            "required": ["description", "prompt", "split_reason"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let spec = match parse_agent_task(&input, 0) {
            Ok(spec) => spec,
            Err(e) => return ToolResult::err(e),
        };
        // Layer 2b：model 名在入参校验期把关（运行期解析失败回落主 provider，不静默换模型）。
        if let Some(model) = spec
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            if !self.runtime.has_model_provider(model) {
                return ToolResult::err(format!(
                    "unknown model '{model}': 未在 [[llm.providers]] 中配置同名 provider；可用名称见 /llmodel 或配置文件。"
                ));
            }
        }
        let has_explicit_name = get_str(&input, "name")
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let resolved_team = match self.runtime.resolve_agent_team_name(&spec).await {
            Ok(team) => team,
            Err(error) => return ToolResult::err(error),
        };
        if let Some(team_name) = resolved_team.as_deref() {
            if !self.runtime.team_exists(team_name).await {
                return ToolResult::err(format!(
                    "Team not found: {team_name}. Call TeamCreate first."
                ));
            }
        }
        if !matches!(
            spec.split_reason.as_deref(),
            Some("new_feedback" | "independent_evidence" | "parallel_external_execution")
        ) {
            return ToolResult::err(
                "split_reason is required and must be one of: new_feedback, independent_evidence, parallel_external_execution. Do not split the task otherwise.",
            );
        }
        let should_spawn_teammate = resolved_team.is_some() && has_explicit_name;
        let should_run_background = should_spawn_teammate || get_bool(&input, "run_in_background");
        let description = get_str(&input, "description")
            .unwrap_or_default()
            .to_string();
        if should_run_background {
            let output = match self.runtime.launch_background_task(spec, description).await {
                Ok(output) => output,
                Err(error) => return ToolResult::err(error),
            };
            return ToolResult::ok(output.to_string());
        }

        // Layer 2a（ZCode 移植）：前台自动转后台。任务从 t=0 起就是登记过的后台执行
        // 快照（AgentStatus 立即可见），父工具调用与它竞速：预算内完成 → 按旧行为
        // 返回完整结果；预算到点 → detach（解除父取消联动，父 /cancel 不再波及），
        // 立即返回 backgrounded 结果，完成通知经团队邮箱 + 合批唤醒接续。
        let auto_background = self.runtime.opts.subagent_auto_background;
        if auto_background.is_zero() {
            let result = self.runtime.run_task(spec, None, None).await;
            return Self::render_foreground(result);
        }

        let (result_tx, mut result_rx) = tokio::sync::mpsc::channel::<AgentTaskResult>(1);
        // 父取消联动（attach）：done 触发前父 /cancel 会终止子任务（保持前台
        // 「cancel 连子代理一起停」语义）；done 触发即 detach，之后不再波及。
        // drop_guard：本工具 future 被整体丢弃（工具批被放弃等）时同样触发 done，
        // watcher 不会一直挂到父 token 取消才退出。
        let done = tokio_util::sync::CancellationToken::new();
        let _done_guard = done.clone().drop_guard();
        let linkage = rclaw_tools::turn_state::turn_cancel().map(|parent| (parent, done.clone()));
        let launched = match self
            .runtime
            .launch_background_task_impl(spec, description, Some(result_tx), linkage)
            .await
        {
            Ok(output) => output,
            Err(error) => return ToolResult::err(error),
        };
        let mut raced = tokio::select! {
            biased;
            result = result_rx.recv() => Some(result),
            _ = tokio::time::sleep(auto_background) => None,
        };
        if raced.is_none() {
            // detach：先关闭接收端（此后子任务的 send 必然失败，run_background_execution
            // 据此判定「未被前台消费」并入箱 + 合批唤醒），再把关闭前恰好送达的结果
            // 捞出来——否则这条结果既被子任务视为「父已拿到」又被父丢弃，凭空消失。
            result_rx.close();
            if let Ok(result) = result_rx.try_recv() {
                raced = Some(Some(result));
            }
        }
        // 竞速结束（完成或转后台）：解除父取消联动，watcher 退出（不泄漏）。
        done.cancel();
        match raced {
            Some(Some(result)) => Self::render_foreground(result),
            Some(None) => {
                ToolResult::err("子代理任务异常终止（执行通道关闭，无结果返回）".to_string())
            }
            None => {
                let agent_id = launched
                    .get("agentId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                ToolResult::ok(
                    json!({
                        "status": "backgrounded",
                        "agentId": agent_id,
                        "note": "前台执行超过自动转后台预算，任务已在后台继续运行（父取消不再波及；完成通知将入团队邮箱并合批唤醒收集）。不要重复它的工作。",
                        "next": ["AgentStatus", "AgentWait", "AgentCancel"]
                    })
                    .to_string(),
                )
            }
        }
    }
}

impl ClaudeAgentTool {
    /// 前台结果的统一渲染（竞速正常完成与 auto_background=0 旧路径共用）。
    fn render_foreground(result: AgentTaskResult) -> ToolResult {
        let rendered = format!(
            "status: {}\n\n{}",
            if result.is_error {
                "failed"
            } else {
                "completed"
            },
            render_foreground_result(&result)
        );
        if result.is_error {
            ToolResult::err(rendered)
        } else {
            ToolResult::ok(rendered)
        }
    }
}

pub struct AgentStatusTool {
    runtime: AgentToolRuntime,
}

impl AgentStatusTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for AgentStatusTool {
    fn name(&self) -> &str {
        "AgentStatus"
    }

    fn description(&self) -> &str {
        "Inspect one async agent execution, or list all executions when agent_id is omitted."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"}
            }
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        if let Some(agent_id) = get_str(&input, "agent_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return match self.runtime.execution_payload(agent_id).await {
                Some(payload) => ToolResult::ok(render_execution_media_payload(payload)),
                None => ToolResult::err(format!("unknown agent execution: {agent_id}")),
            };
        }
        ToolResult::ok(self.runtime.execution_list_payload().await.to_string())
    }
}

pub struct AgentCancelTool {
    runtime: AgentToolRuntime,
}

impl AgentCancelTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for AgentCancelTool {
    fn name(&self) -> &str {
        "AgentCancel"
    }

    fn description(&self) -> &str {
        "Request real cancellation of a queued/running/waiting async agent execution."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"},
                "reason": {"type": "string"}
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(agent_id) = get_str(&input, "agent_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("agent_id is required for AgentCancel");
        };
        match self
            .runtime
            .cancel_execution(
                agent_id,
                get_str(&input, "reason").unwrap_or("requested by caller"),
            )
            .await
        {
            Ok(payload) => ToolResult::ok(payload.to_string()),
            Err(error) => ToolResult::err(error),
        }
    }
}

pub struct AgentWaitTool {
    runtime: AgentToolRuntime,
}

impl AgentWaitTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for AgentWaitTool {
    fn name(&self) -> &str {
        "AgentWait"
    }

    fn description(&self) -> &str {
        "Wait for an async agent terminal handoff, with a bounded timeout."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"},
                "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 300000}
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(agent_id) = get_str(&input, "agent_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("agent_id is required for AgentWait");
        };
        let timeout_ms = match input.get("timeout_ms") {
            None => 30_000,
            Some(value) => match value.as_u64() {
                Some(value) if value <= 300_000 => value,
                _ => {
                    return ToolResult::err("timeout_ms must be an integer in 0..=300000");
                }
            },
        };
        match self.runtime.wait_execution(agent_id, timeout_ms).await {
            Ok(payload) => ToolResult::ok(render_execution_media_payload(payload)),
            Err(error) => ToolResult::err(error),
        }
    }
}

pub struct AgentResumeTool {
    runtime: AgentToolRuntime,
}

impl AgentResumeTool {
    pub fn new(runtime: AgentToolRuntime) -> Self {
        Self { runtime }
    }
}

#[async_trait]
impl Tool for AgentResumeTool {
    fn name(&self) -> &str {
        "AgentResume"
    }

    fn description(&self) -> &str {
        "Resume a waiting execution recovered from its persistent event log after process restart."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "agent_id": {"type": "string"}
            },
            "required": ["agent_id"]
        })
    }

    async fn execute(&self, input: Value) -> ToolResult {
        let Some(agent_id) = get_str(&input, "agent_id")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err("agent_id is required for AgentResume");
        };
        match self.runtime.resume_execution(agent_id).await {
            Ok(payload) => ToolResult::ok(render_execution_media_payload(payload)),
            Err(error) => ToolResult::err(error),
        }
    }
}
