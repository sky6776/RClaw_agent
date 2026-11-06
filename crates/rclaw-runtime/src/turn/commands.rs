use super::*;

use crate::slash::CommandOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeCommand {
    Cancel,
    Context,
    Skills,
    Tools,
    Clear,
    Log,
    Compact,
    Rewind,
    Fork,
    Goal,
    Export,
}

impl RuntimeCommand {
    fn parse(content: &str) -> Option<Self> {
        let rest = content.trim().strip_prefix('/')?.trim_start();
        let name = rest.split_whitespace().next().unwrap_or("");
        match name.to_ascii_lowercase().as_str() {
            "cancel" => Some(Self::Cancel),
            "context" => Some(Self::Context),
            "skills" => Some(Self::Skills),
            "tools" => Some(Self::Tools),
            "clear" => Some(Self::Clear),
            "log" => Some(Self::Log),
            "compact" => Some(Self::Compact),
            "rewind" => Some(Self::Rewind),
            "fork" => Some(Self::Fork),
            "goal" => Some(Self::Goal),
            "export" => Some(Self::Export),
            _ => None,
        }
    }

    /// 是否属于「影响全局 / 外泄数据」的命令：清空全部会话与长期记忆、读取或删除日志、
    /// 回滚工作区文件、Skill 生命周期变更、导出全部会话。这些命令只认 [`is_privileged`]
    /// 的会话。`/cancel`、`/context`、`/compact`、`/goal`、`/tools`、`/fork`、单会话
    /// `/export`、只读 `/skills list|status|help` 仍对所有用户开放。
    fn requires_privilege(self, content: &str) -> bool {
        let second = content
            .split_whitespace()
            .nth(1)
            .map(str::to_ascii_lowercase)
            .unwrap_or_default();
        match self {
            Self::Clear | Self::Log | Self::Rewind => true,
            Self::Skills => matches!(
                second.as_str(),
                "pin" | "unpin" | "adopt" | "restore" | "curate"
            ),
            Self::Export => second == "all",
            Self::Cancel
            | Self::Context
            | Self::Tools
            | Self::Compact
            | Self::Fork
            | Self::Goal => false,
        }
    }
}

/// 本机通道（cli / desktop 虚拟通道）或通道层已提升为 [`ToolAccess::Unrestricted`] 的消息
/// （如 napcat `max_from` 私聊）才算特权会话。外部通道（QQ 群成员、allow 名单里的普通
/// 用户、微信等）默认 `Sandboxed`：允许聊天不等于允许一句 `/clear confirm` 抹掉所有人的
/// 会话与记忆、用 `/log get` 把含他人输入的日志拉到自己聊天里。
fn is_privileged(msg: &BusMessage) -> bool {
    // 桌面虚拟通道名由 rclaw-desktop 定义（`CHANNEL_DESKTOP = "desktop"`），runtime 不依赖
    // 该 crate，这里按字面量比对（与 rclaw-channels 的路由一致）。
    msg.tool_access() == rclaw_core::ToolAccess::Unrestricted
        || msg.channel == CHANNEL_CLI
        || msg.channel == "desktop"
}

/// 路由层命令分发结果。
pub(super) enum DispatchOutcome {
    /// 未知斜杠 / 普通消息：继续走插队或新 turn。
    NotHandled,
    /// 命令已执行并完成结果分发：`route` 直接返回。
    Handled,
    /// 命令已产生状态副作用，但这条消息还要作为 turn 输入继续路由（`/goal <文本>`：
    /// 目标已落库，消息已展开为「立即开工」的模型输入）。
    StartTurn(BusMessage),
}

/// 在进入活动 turn 路由前处理所有即时命令。
///
/// [`DispatchOutcome::Handled`] 表示命令已执行并完成结果分发；[`NotHandled`] 表示未知
/// 斜杠或普通消息，继续走插队或新 turn；[`StartTurn`] 表示命令的副作用已完成，但当前
/// 消息要作为 turn 输入继续交给 `route` 的既有 active/spawn 逻辑。状态依赖、优先级与
/// outbound 语义全部封装在这里。
///
/// [`NotHandled`]: DispatchOutcome::NotHandled
/// [`StartTurn`]: DispatchOutcome::StartTurn
pub(super) async fn dispatch(manager: &Arc<TurnManager>, msg: &BusMessage) -> DispatchOutcome {
    if msg.source() != MessageSource::User {
        return DispatchOutcome::NotHandled;
    }

    if let Some(command) = RuntimeCommand::parse(&msg.content) {
        if command.requires_privilege(&msg.content) && !is_privileged(msg) {
            tracing::warn!(
                "拒绝非特权会话 {}:{} 的运行时命令 {}",
                msg.channel,
                msg.chat_id,
                msg.content.trim()
            );
            manager
                .send_command_reply(
                    &msg.channel,
                    &msg.chat_id,
                    "权限不足：该命令会影响全部会话或工作区，只能在本机（cli / desktop）或已授权的管理员会话中执行。"
                        .to_string(),
                )
                .await;
            return DispatchOutcome::Handled;
        }
        let response = match command {
            RuntimeCommand::Cancel => {
                manager.route_cancel(TurnKey::from_msg(msg)).await;
                return DispatchOutcome::Handled;
            }
            RuntimeCommand::Context => manager.handle_context(&msg.channel, &msg.chat_id),
            RuntimeCommand::Skills => manager.handle_skills_command(&msg.content),
            RuntimeCommand::Tools => render_tools(manager.tools.schemas()),
            RuntimeCommand::Clear => manager.handle_clear(&msg.content).await,
            RuntimeCommand::Log => manager.handle_log(&msg.channel, &msg.content),
            RuntimeCommand::Compact => {
                // 手动压缩要调一次摘要 LLM（十几秒到一分钟）：不能在路由任务上 await，
                // 否则期间所有会话的入站消息都排队。放到独立任务，结果经 outbound 回送。
                let manager = manager.clone();
                let channel = msg.channel.clone();
                let chat_id = msg.chat_id.clone();
                tokio::spawn(async move {
                    let response = manager.handle_compact(&channel, &chat_id).await;
                    manager
                        .send_command_reply(&channel, &chat_id, response)
                        .await;
                });
                return DispatchOutcome::Handled;
            }
            RuntimeCommand::Rewind => {
                manager
                    .handle_rewind(&msg.channel, &msg.chat_id, &msg.content)
                    .await
            }
            RuntimeCommand::Fork => {
                // fork 只读源会话 + 新建会话（不动源历史），运行中的 turn 不受影响；
                // 但复制要读活动视图快照，放 blocking 线程，不在路由任务上等。
                let manager = manager.clone();
                let channel = msg.channel.clone();
                let chat_id = msg.chat_id.clone();
                let content = msg.content.clone();
                tokio::spawn(async move {
                    let response = manager.handle_fork(&channel, &chat_id, &content).await;
                    manager
                        .send_command_reply(&channel, &chat_id, response)
                        .await;
                });
                return DispatchOutcome::Handled;
            }
            RuntimeCommand::Goal => {
                match manager
                    .handle_goal(&msg.channel, &msg.chat_id, &msg.content)
                    .await
                {
                    // 查询 / 清除 / 超长 / 写库失败：只回执，不进入 turn。
                    GoalCommandOutcome::Reply(text) => {
                        manager
                            .send_command_reply(&msg.channel, &msg.chat_id, text)
                            .await;
                        return DispatchOutcome::Handled;
                    }
                    // 目标已落库：先发强制 intermediate 的回执（turn 还没启动，非
                    // intermediate 会提前解除桌面忙碌态、放行排队消息），再把同一条
                    // 消息展开成交给 turn 的指令继续路由。原文保留在 display_content。
                    GoalCommandOutcome::Set { receipt, objective } => {
                        manager
                            .send_goal_receipt(&msg.channel, &msg.chat_id, receipt)
                            .await;
                        tracing::info!(
                            channel = ?msg.channel,
                            chat_id = ?msg.chat_id,
                            objective_chars = objective.chars().count(),
                            "/goal 已写入，作为 turn 输入继续路由"
                        );
                        return DispatchOutcome::StartTurn(msg.clone().expand_model_content(
                            TurnManager::goal_command_expansion(&objective),
                        ));
                    }
                }
            }
            RuntimeCommand::Export => {
                // 导出要把整段 transcript（可能上千条）读出来并渲染、落盘：虽然走的是
                // spawn_blocking，但在路由任务上 await 它同样会让别的会话排队。与 /compact
                // 一样放独立任务，结果经 outbound 回送。
                let manager = manager.clone();
                let channel = msg.channel.clone();
                let chat_id = msg.chat_id.clone();
                let content = msg.content.clone();
                tokio::spawn(async move {
                    let response = manager.handle_export(&channel, &chat_id, &content).await;
                    manager
                        .send_command_reply(&channel, &chat_id, response)
                        .await;
                });
                return DispatchOutcome::Handled;
            }
        };
        manager
            .send_command_reply(&msg.channel, &msg.chat_id, response)
            .await;
        return DispatchOutcome::Handled;
    }

    let Some(registry) = &manager.slash else {
        return DispatchOutcome::NotHandled;
    };
    let CommandOutcome::Handled(response) = registry.try_handle(&msg.content) else {
        return DispatchOutcome::NotHandled;
    };
    tracing::info!(
        "斜杠命令命中 {}:{}，路由层跳过 LLM",
        msg.channel,
        msg.chat_id
    );
    manager
        .send_command_reply(&msg.channel, &msg.chat_id, response)
        .await;
    DispatchOutcome::Handled
}

#[cfg(test)]
mod tests {
    use super::RuntimeCommand;

    #[test]
    fn runtime_command_parser_matches_only_complete_names() {
        for (input, expected) in [
            ("/cancel", RuntimeCommand::Cancel),
            (" /CANCEL now ", RuntimeCommand::Cancel),
            ("/context", RuntimeCommand::Context),
            ("/skills list", RuntimeCommand::Skills),
            ("/tools", RuntimeCommand::Tools),
            ("/clear confirm", RuntimeCommand::Clear),
            ("/LOG list", RuntimeCommand::Log),
            ("/compact", RuntimeCommand::Compact),
            ("/rewind 1791753207350478000", RuntimeCommand::Rewind),
            ("/goal", RuntimeCommand::Goal),
            ("/ GOAL 完成迁移", RuntimeCommand::Goal),
            ("/export", RuntimeCommand::Export),
            ("/export all", RuntimeCommand::Export),
        ] {
            assert_eq!(RuntimeCommand::parse(input), Some(expected));
        }
        for input in [
            "hello",
            "/etc/hosts",
            "/contexts",
            "/skills-list",
            "/tool",
            "/login",
            "/clearing",
            "/exports",
            "/goalx",
            "/llmodel glm-5.3",
        ] {
            assert_eq!(RuntimeCommand::parse(input), None, "input={input}");
        }
    }
}
