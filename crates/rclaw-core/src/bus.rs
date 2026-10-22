//! 通道间消息契约与通道名常量。
//!
//! 两条总线（inbound / outbound）用 `tokio::mpsc` 实现，内容为 `String`（所有权转移），
//! 无需 lease 借用模型，
//! 但 `channel` / `chat_id` 字段保留，便于 outbound dispatcher 不解析正文即可路由。

/// 通道名：CLI / 控制台（stdin ↔ stdout）。
pub const CHANNEL_CLI: &str = "cli";
/// 通道名：微信 / iLink Bot。
pub const CHANNEL_WEIXIN: &str = "weixin";
/// 通道名：NapCatQQ（OneBot 11 over WebSocket）。
pub const CHANNEL_NAPCAT: &str = "napcat";
/// 通道名：系统内部消息（heartbeat、cron 兜底目标等）。
pub const CHANNEL_SYSTEM: &str = "system";
/// 通道名：WebSocket（预留，暂不实现）。
pub const CHANNEL_WEBSOCKET: &str = "websocket";
/// 通道名：Telegram（预留，暂不实现）。
pub const CHANNEL_TELEGRAM: &str = "telegram";

/// cron 任务注入 inbound 时使用此前缀构建消息正文，便于在日志中识别。
/// 身份判定不再依赖正文前缀——现在通过 [`BusMessage::source`]（[`MessageSource`]）字段标识，
/// 外部通道（CLI/微信）构造的消息永远为 [`MessageSource::User`]，无法伪造 cron/heartbeat 身份。
pub const CRON_TASK_PREFIX: &str = "[CRON_TASK]";

/// 消息来源类型——区分「真实用户消息」「cron 定时任务」「heartbeat 周期自检」。
///
/// **修复 P2「heartbeat 被误判为 cron」**：旧实现只有一个 `is_internal: bool` 标志，
/// `is_cron_task()` 直接返回它。但 heartbeat 也通过 `new_internal` 入队（同样置
/// `is_internal = true`），于是 heartbeat 会错误地走进 cron 专属分支：跳过普通历史、
/// 注入「Cron Task Handling」系统提示。引入显式枚举后三类来源彻底分离，
/// [`BusMessage::is_cron_task`] 仅在 [`MessageSource::Cron`] 时为真，heartbeat 回归
/// 「带历史的普通内部消息」语义（不再误判为 cron）。
///
/// 安全属性：外部通道构造的消息永远是 [`MessageSource::User`]——用户无法通过发送特定正文
/// 伪装成 cron/heartbeat 来跳过历史持久化或套用内部专用提示（构造函数 `new_cron` /
/// `new_heartbeat` 仅由系统组件调用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MessageSource {
    /// 外部通道（CLI / 微信…）的真实用户消息。
    #[default]
    User,
    /// cron 调度器生成的定时任务消息（一次性提醒 / 周期任务）。
    /// agent loop 对其：不加载历史、不持久化 user、注入 Cron Task Handling 提示。
    Cron,
    /// heartbeat 周期自检消息（读 `HEARTBEAT.md` 后注入的固定提示）。
    /// 它是内部消息但【不是 cron 任务】：正常加载历史、正常持久化、不注入 cron 专用提示。
    Heartbeat,
    /// 工作流后台完成 / 阻塞通知（WorkflowDriver outbox 投递）。
    /// 语义同 Heartbeat 的「内部但非 cron」：正常加载/持久化历史；不触发自进化评审。
    Workflow,
    /// Bot Mode 交接消息（message_agent 工具投递到目标 bot 的收件箱会话）。
    /// 语义同 Heartbeat 的「内部但非 cron」：正常加载/持久化历史；但作为队友消息，
    /// 若目标 turn 正在进行则延迟为下一轮（不插话）；不触发自进化评审与 curator。
    /// 仅由 [`BusMessage::new_agent`]（BotRouter）构造，外部通道无法伪造。
    Agent,
}

/// 工具文件沙箱权限——决定 read/write/edit/list/grep/shell 是否被限制在 agent 目录内。
///
/// 默认 [`ToolAccess::Sandboxed`]：所有通道的消息都只能操作 `agent/` 目录（既有行为）。
/// napcat 通道对 `max_from` 命中的 QQ 私聊消息设为 [`ToolAccess::Unrestricted`]，
/// 放开 agent 目录沙箱。与 `source` 同为【私有字段 + 构造/访问器】范式：只有可信通道层
/// 能设置（用户无法从消息正文伪造——QQ 判定基于 napcat 上报的 user_id）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum ToolAccess {
    /// 仅 agent 目录（默认）。
    #[default]
    Sandboxed,
    /// 任意路径（max 权限 QQ）。
    Unrestricted,
}

/// 通道间流转的消息。
///
/// 生产方（通道、cron、heartbeat）填好 `channel`/`chat_id`/`content` 后投入 inbound；
/// agent loop 处理完产出回复投入 outbound；outbound dispatcher 按 `channel` 路由回通道。
#[derive(Debug, Clone)]
pub struct BusMessage {
    /// 来源/目标通道名（取上面的常量）。
    pub channel: String,
    /// 会话标识（如 weixin 用户 id、cli 的 "console" 等）。
    pub chat_id: String,
    /// 消息正文（已是自有 String，无需借用）。LLM 请求与工具可见文本以它为准；
    /// `/workflow` 展开后这里是展开文本，`display_content` 保留用户原文。
    pub content: String,
    /// 历史 / UI / 回放展示文本（默认等于 `content`）。
    pub display_content: String,
    /// LLM 请求与工具可见文本（默认等于 `content`）。
    pub model_content: String,
    /// 工作流 outbox 通知 id（接收端持久化后回执 consumed；普通消息为 None）。
    workflow_notification_id: Option<String>,
    /// 消息来源（[`MessageSource`]），【私有字段】。
    ///
    /// **修复 P2「source 字段公开破坏来源不可伪造约束」**：旧实现把 `source` 设为 `pub`，
    /// 任何外部 crate 都能用结构体字面量 `BusMessage { ..., source: MessageSource::Cron }`
    /// 绕过 [`BusMessage::new`] / [`BusMessage::new_cron`] / [`BusMessage::new_heartbeat`]
    /// 的构造约束，伪造 cron / heartbeat 身份——而 agent_loop 会信任 Cron 来源来跳过普通历史、
    /// 注入 cron 专用提示，这等于把「外部消息永远是 [`MessageSource::User`]」这条安全边界
    /// 打破（注释和 agent_loop 都把 source 当成安全边界）。
    ///
    /// 现在改为私有字段：外部只能通过构造函数设置来源（用户消息恒为 `User`，cron / heartbeat
    /// 仅由系统组件调用对应构造函数产生），读取则用只读访问器 [`BusMessage::source`]。
    /// 它替代旧的 `is_internal: bool` 标志，让 cron 与 heartbeat 不再混淆（详见 [`MessageSource`]）。
    source: MessageSource,
    /// Cron 调度任务 ID。只有由调度器创建的 Cron 消息才设置；普通用户消息与 heartbeat 为 `None`。
    ///
    /// Turn 路由用它区分「回复到同一 channel/chat_id 的不同定时任务」，从而允许这些任务
    /// 独立执行，而不是被误判为同一会话的补充消息。
    cron_job_id: Option<String>,
    /// Cron 任务名称。仅用于最终报告标识，不参与外部通道路由。
    cron_job_name: Option<String>,
    /// Bot Mode 交接链：这条消息到达当前 bot 之前依次经过的发送者身份
    /// （主 persona 记 "RClaw"，bot 记自己的名字）。
    ///
    /// 仅由 [`BusMessage::new_agent`] 设置，用于交接深度与环路判定；普通消息为空。
    agent_chain: Vec<String>,
    /// 交接关联 id（BotRouter 生成，回执按 id 精确配对）。仅 Agent 来源消息携带。
    handoff_id: Option<u64>,
    /// 工具文件沙箱权限（[`ToolAccess`]），默认 [`ToolAccess::Sandboxed`]。
    ///
    /// 与 `source` 同为私有字段：仅可信通道层通过 [`BusMessage::with_tool_access`] 设置；
    /// 用户消息恒为 `Sandboxed`，除非通道层根据配置（如 napcat `max_from`）显式提升。
    tool_access: ToolAccess,
    /// 中间过程消息标志：工具调用轮的 text / reasoning 流等「turn 未结束」的可见输出。
    ///
    /// 通道据此区分最终回复与过程输出——例如桌面端只在收到【非 intermediate】的
    /// 普通消息时解除会话忙碌态；工具轮之间的过程文本不会误判为 turn 收尾。
    /// 私有字段 + [`BusMessage::with_intermediate`] 构造，默认 false。
    intermediate: bool,
}

impl BusMessage {
    /// 构造【外部通道】用户消息（`source = User`）。
    /// 通道（CLI/微信）从外部输入解析出消息后调用此构造函数。
    pub fn new(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        BusMessage {
            channel: channel.into(),
            chat_id: chat_id.into(),
            content: content.into(),
            display_content: String::new(),
            model_content: String::new(),
            workflow_notification_id: None,
            source: MessageSource::User,
            cron_job_id: None,
            cron_job_name: None,
            agent_chain: Vec::new(),
            handoff_id: None,
            tool_access: ToolAccess::Sandboxed,
            intermediate: false,
        }
    }

    /// 构造【cron 调度器】定时任务消息（`source = Cron`）。
    /// 仅系统组件（`CronService`）调用；外部通道【不得】使用——这是
    /// 「用户无法伪造 cron 身份」的关键：用户消息永远是 `User`，无法走 cron 分支。
    pub fn new_cron(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        BusMessage {
            channel: channel.into(),
            chat_id: chat_id.into(),
            content: content.into(),
            display_content: String::new(),
            model_content: String::new(),
            workflow_notification_id: None,
            source: MessageSource::Cron,
            cron_job_id: None,
            cron_job_name: None,
            agent_chain: Vec::new(),
            handoff_id: None,
            tool_access: ToolAccess::Sandboxed,
            intermediate: false,
        }
    }

    /// 构造带任务身份的 Cron 消息。
    ///
    /// `job_id` 只作为内部执行键；`channel/chat_id` 仍是最终报告的外部回复目标。
    pub fn new_cron_job(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        job_id: impl Into<String>,
        job_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        BusMessage {
            channel: channel.into(),
            chat_id: chat_id.into(),
            content: content.into(),
            display_content: String::new(),
            model_content: String::new(),
            workflow_notification_id: None,
            source: MessageSource::Cron,
            cron_job_id: Some(job_id.into()),
            cron_job_name: Some(job_name.into()),
            agent_chain: Vec::new(),
            handoff_id: None,
            tool_access: ToolAccess::Sandboxed,
            intermediate: false,
        }
    }

    /// 构造【heartbeat 服务】自检消息（`source = Heartbeat`）。
    /// 仅系统组件（`Heartbeat` 服务）调用。heartbeat 是内部消息但【不是】cron 任务。
    pub fn new_heartbeat(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        BusMessage {
            channel: channel.into(),
            chat_id: chat_id.into(),
            content: content.into(),
            display_content: String::new(),
            model_content: String::new(),
            workflow_notification_id: None,
            source: MessageSource::Heartbeat,
            cron_job_id: None,
            cron_job_name: None,
            agent_chain: Vec::new(),
            handoff_id: None,
            tool_access: ToolAccess::Sandboxed,
            intermediate: false,
        }
    }

    /// 构造【Bot Mode 交接】消息（`source = Agent`），投递到目标 bot 的收件箱会话。
    ///
    /// `agent_chain` 为到达目标前的发送者链（防环与深度判定用）；`handoff_id` 为
    /// BotRouter 生成的关联 id（turn 收尾按它精确配对回执）。仅 BotRouter 调用；
    /// 外部通道无法构造——用户消息永远是 `User`，不可能冒充队友交接。
    pub fn new_agent(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        agent_chain: Vec<String>,
        handoff_id: u64,
        content: impl Into<String>,
    ) -> Self {
        BusMessage {
            channel: channel.into(),
            chat_id: chat_id.into(),
            content: content.into(),
            display_content: String::new(),
            model_content: String::new(),
            workflow_notification_id: None,
            source: MessageSource::Agent,
            cron_job_id: None,
            cron_job_name: None,
            agent_chain,
            handoff_id: Some(handoff_id),
            tool_access: ToolAccess::Sandboxed,
            intermediate: false,
        }
    }

    /// 构造【工作流后台通知】消息（`source = Workflow`）。
    /// 仅 [`rclaw_runtime`] 的 outbox dispatcher 调用；外部通道无法伪造。
    pub fn new_workflow(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        BusMessage {
            channel: channel.into(),
            chat_id: chat_id.into(),
            content: content.into(),
            display_content: String::new(),
            model_content: String::new(),
            workflow_notification_id: None,
            source: MessageSource::Workflow,
            cron_job_id: None,
            cron_job_name: None,
            agent_chain: Vec::new(),
            handoff_id: None,
            tool_access: ToolAccess::Sandboxed,
            intermediate: false,
        }
    }

    /// 工作流通知（携带 outbox id，接收端落库后回执）。
    pub fn new_workflow_with_id(
        channel: impl Into<String>,
        chat_id: impl Into<String>,
        content: impl Into<String>,
        notification_id: impl Into<String>,
    ) -> Self {
        let mut message = BusMessage::new_workflow(channel, chat_id, content);
        message.workflow_notification_id = Some(notification_id.into());
        message
    }

    /// 工作流 outbox 通知 id（普通消息 None）。
    pub fn workflow_notification_id(&self) -> Option<&str> {
        self.workflow_notification_id.as_deref()
    }

    /// 历史 / UI / 回放展示文本；未单独设置时回落正文。
    pub fn display_content(&self) -> &str {
        if self.display_content.is_empty() {
            &self.content
        } else {
            &self.display_content
        }
    }

    /// LLM 请求与工具可见文本；未单独设置时回落正文。
    pub fn model_content(&self) -> &str {
        if self.model_content.is_empty() {
            &self.content
        } else {
            &self.model_content
        }
    }

    /// `/workflow` 展开：保留原文到 display，把展开文本写进 content/model。
    pub fn expand_model_content(mut self, model: String) -> Self {
        self.display_content = self.content.clone();
        self.model_content = model.clone();
        self.content = model;
        self
    }

    /// 消息来源（User / Cron / Heartbeat）。
    pub fn source(&self) -> MessageSource {
        self.source
    }

    /// Cron 调度任务 ID；普通消息返回 `None`。
    pub fn cron_job_id(&self) -> Option<&str> {
        self.cron_job_id.as_deref()
    }

    /// Cron 任务名称；普通消息返回 `None`。
    pub fn cron_job_name(&self) -> Option<&str> {
        self.cron_job_name.as_deref()
    }

    /// Bot Mode 交接链（到达当前 bot 前的发送者身份序列）；普通消息为空切片。
    pub fn agent_chain(&self) -> &[String] {
        &self.agent_chain
    }

    /// Bot Mode 交接关联 id（回执配对用）；普通消息为 `None`。
    pub fn handoff_id(&self) -> Option<u64> {
        self.handoff_id
    }

    /// 工具文件沙箱权限（[`ToolAccess`]）。
    pub fn tool_access(&self) -> ToolAccess {
        self.tool_access
    }

    /// 链式设置工具沙箱权限（供可信通道层在构造后提升权限，如 napcat `max_from` 命中）。
    ///
    /// 仅由通道层在构造 inbound 消息时调用；外部用户无法经此提升权限——它不在消息正文里。
    pub fn with_tool_access(mut self, access: ToolAccess) -> Self {
        self.tool_access = access;
        self
    }

    /// 标记为中间过程消息（工具轮 text / reasoning 流等 turn 未结束的可见输出）。
    pub fn with_intermediate(mut self) -> Self {
        self.intermediate = true;
        self
    }

    /// 是否中间过程消息（false = turn 的最终回复 / 收尾确认）。
    pub fn is_intermediate(&self) -> bool {
        self.intermediate
    }

    /// 是否为 cron 任务消息（仅 [`MessageSource::Cron`] 为真）。
    ///
    /// **回归保护**：heartbeat（[`MessageSource::Heartbeat`]）不再被误判为 cron——
    /// 它是内部消息，但应正常加载历史、不注入 cron 专用提示。
    pub fn is_cron_task(&self) -> bool {
        matches!(self.source, MessageSource::Cron)
    }

    /// 是否为系统内部消息（非真实用户来源：Cron 或 Heartbeat）。
    ///
    /// 与 [`BusMessage::is_cron_task`] 的区别：本方法涵盖「任何非用户来源」（cron + heartbeat），
    /// 而 `is_cron_task` 仅指 cron 任务。外部通道构造的消息（`User`）此方法返回 false。
    pub fn is_internal(&self) -> bool {
        !matches!(self.source, MessageSource::User)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_is_neither_cron_nor_internal() {
        let m = BusMessage::new(CHANNEL_CLI, "console", "hi");
        assert_eq!(m.source(), MessageSource::User);
        assert!(!m.is_cron_task());
        assert!(!m.is_internal());
    }

    #[test]
    fn cron_message_is_cron_and_internal() {
        let m = BusMessage::new_cron(CHANNEL_SYSTEM, "c1", "[CRON_TASK] ...");
        assert_eq!(m.source(), MessageSource::Cron);
        assert!(m.is_cron_task());
        assert!(m.is_internal());
        assert_eq!(m.cron_job_id(), None);
        assert_eq!(m.cron_job_name(), None);
    }

    #[test]
    fn scheduled_cron_message_carries_job_identity() {
        let m = BusMessage::new_cron_job(
            CHANNEL_SYSTEM,
            "c1",
            "abcd1234",
            "股票分析",
            "[CRON_TASK] ...",
        );
        assert_eq!(m.source(), MessageSource::Cron);
        assert_eq!(m.cron_job_id(), Some("abcd1234"));
        assert_eq!(m.cron_job_name(), Some("股票分析"));
    }

    /// 回归 P2「heartbeat 被误判为 cron」：heartbeat 必须是内部消息，但【不是】cron 任务。
    #[test]
    fn heartbeat_message_is_internal_but_not_cron() {
        let m = BusMessage::new_heartbeat(CHANNEL_SYSTEM, "heartbeat", "Read HEARTBEAT.md ...");
        assert_eq!(m.source(), MessageSource::Heartbeat);
        assert!(m.is_internal());
        assert!(
            !m.is_cron_task(),
            "heartbeat 不得被判定为 cron 任务（否则会跳过历史并误注入 Cron Task Handling 提示）"
        );
    }

    /// Bot Mode 交接消息：内部来源、非 cron、携带交接链；普通消息链为空。
    #[test]
    fn agent_handoff_message_is_internal_not_cron_and_carries_chain() {
        let m = BusMessage::new_agent(
            "desktop",
            "bot-researcher",
            vec!["RClaw".to_string()],
            7,
            "Message from 🤖 主控 (@RClaw): 查一下",
        );
        assert_eq!(m.source(), MessageSource::Agent);
        assert!(m.is_internal());
        assert!(!m.is_cron_task());
        assert_eq!(m.agent_chain(), ["RClaw".to_string()].as_slice());
        assert_eq!(m.handoff_id(), Some(7));
        assert_eq!(
            BusMessage::new(CHANNEL_CLI, "console", "hi").handoff_id(),
            None
        );

        // 普通消息与 cron/heartbeat 交接链均为空。
        assert!(BusMessage::new(CHANNEL_CLI, "console", "hi")
            .agent_chain()
            .is_empty());
        assert!(BusMessage::new_cron(CHANNEL_SYSTEM, "c", "x")
            .agent_chain()
            .is_empty());
    }

    /// tool_access 默认 Sandboxed；可信通道层可用 with_tool_access 提升为 Unrestricted。
    #[test]
    fn tool_access_defaults_to_sandboxed_and_is_settable() {
        // 三种构造函数都默认受限。
        assert_eq!(
            BusMessage::new(CHANNEL_CLI, "console", "hi").tool_access(),
            ToolAccess::Sandboxed
        );
        assert_eq!(
            BusMessage::new_cron(CHANNEL_SYSTEM, "c", "x").tool_access(),
            ToolAccess::Sandboxed
        );
        assert_eq!(
            BusMessage::new_heartbeat(CHANNEL_SYSTEM, "h", "x").tool_access(),
            ToolAccess::Sandboxed
        );
        // 可信通道层可提升（如 napcat max_from 命中）。
        let maxed = BusMessage::new(CHANNEL_NAPCAT, "user:12345", "hi")
            .with_tool_access(ToolAccess::Unrestricted);
        assert_eq!(maxed.tool_access(), ToolAccess::Unrestricted);
    }
}

/// 单个通道的三项能力声明（C3：由适配器声明、CLI 接线时采集、runtime 消费）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelCaps {
    /// 规范化通道名（路由键，如 "cli"/"weixin"/"napcat"）。
    pub name: String,
    /// 是否可收发媒体（media marker 解析与出站附件）。
    pub media: bool,
    /// 是否具备「工作状态」推送（中间工具轮状态展示）。
    pub working_status: bool,
    /// 是否许可中间输出（工具轮模型 text 作为中间消息转发）。
    pub intermediate_output: bool,
    /// 是否许可推理（reasoning）增量流式出站（桌面「💭 思考过程」实时展示）。
    ///
    /// 仅桌面声明 true：按轮整段的 reasoning 快照（intermediate_output 许可的那条）
    /// 对 QQ 等通道足够；逐分片直播只有本地桌面 UI 消费得起。
    pub reasoning_stream: bool,
}

/// 接线期采集的通道能力集合 + 共享媒体沙箱目录（C3 替代 RCLAW_* 环境变量）。
#[derive(Debug, Clone, Default)]
pub struct RuntimeChannelInfo {
    /// 共享媒体沙箱目录（空 = 未启用媒体）。
    pub media_dir: Option<String>,
    /// 各通道能力（按规范化名索引）。
    pub channels: Vec<ChannelCaps>,
}

impl RuntimeChannelInfo {
    /// 按规范化名查通道能力。
    pub fn caps(&self, channel: &str) -> Option<&ChannelCaps> {
        let normalized = channel.trim().to_ascii_lowercase();
        self.channels.iter().find(|caps| caps.name == normalized)
    }
}
