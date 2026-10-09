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
            RuntimeCommand::Clear => manager.handle_clear(&msg.content),
            RuntimeCommand::Log => manager.handle_log(&msg.channel, &msg.content),
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
            "/llmodel glm-5.3",
        ] {
            assert_eq!(RuntimeCommand::parse(input), None, "input={input}");
        }
    }
}
