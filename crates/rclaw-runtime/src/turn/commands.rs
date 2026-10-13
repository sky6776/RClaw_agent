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
            "export" => Some(Self::Export),
            _ => None,
        }
    }
}

/// 在进入活动 turn 路由前处理所有即时命令。
///
/// 返回 `true` 表示命令已执行并完成结果分发；未知斜杠和普通消息返回 `false`，继续走
/// 插队或新 turn。状态依赖、优先级与 outbound 语义全部封装在这里。
pub(super) async fn dispatch(manager: &Arc<TurnManager>, msg: &BusMessage) -> bool {
    if msg.source() != MessageSource::User {
        return false;
    }

    if let Some(command) = RuntimeCommand::parse(&msg.content) {
        let response = match command {
            RuntimeCommand::Cancel => {
                manager.route_cancel(TurnKey::from_msg(msg)).await;
                return true;
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
                    let _ = manager
                        .outbound_tx
                        .send(BusMessage::new(channel, chat_id, response))
                        .await;
                });
                return true;
            }
            RuntimeCommand::Rewind => {
                manager
                    .handle_rewind(&msg.channel, &msg.chat_id, &msg.content)
                    .await
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
                    let _ = manager
                        .outbound_tx
                        .send(BusMessage::new(channel, chat_id, response))
                        .await;
                });
                return true;
            }
        };
        let _ = manager
            .outbound_tx
            .send(BusMessage::new(
                msg.channel.clone(),
                msg.chat_id.clone(),
                response,
            ))
            .await;
        return true;
    }

    let Some(registry) = &manager.slash else {
        return false;
    };
    let CommandOutcome::Handled(response) = registry.try_handle(&msg.content) else {
        return false;
    };
    tracing::info!(
        "斜杠命令命中 {}:{}，路由层跳过 LLM",
        msg.channel,
        msg.chat_id
    );
    let _ = manager
        .outbound_tx
        .send(BusMessage::new(
            msg.channel.clone(),
            msg.chat_id.clone(),
            response,
        ))
        .await;
    true
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
            "/llmodel glm-5.3",
        ] {
            assert_eq!(RuntimeCommand::parse(input), None, "input={input}");
        }
    }
}
