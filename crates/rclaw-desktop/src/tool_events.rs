//! 运行时工具调用观察事件 → Webview `tool_calls` 事件桥（会话面板「工具 / Skill 调用
//! 可展开观察」的直播通道；桌面与 APK 共用）。
//!
//! 直播只是加速：事实源在 state.sqlite（assistant(tool_calls) 行 + 结果载体 +
//! `tool_call_meta` 侧表），面板的快照 / 增量条目（`HistoryMsgDto::tool_calls`）与展开
//! 详情（`chat_tool_detail`）都从库里读——重启 / 重装（库文件仍在）后同样可观察。
//! finished 事件带落库行 id，前端据此把直播条目当场盖章成持久化条目。
//!
//! 事件不在观察点直接 `app.emit`，而是编成控制消息交回运行时、沿运行时同一条 outbound
//! 队列由 `outbound_bridge` 发出。思考流 / 过程文本走 outbound mpsc（APK 还要再过状态
//! 抽头与 dispatcher），观察点上的直接 emit 会抢在刚入队的本轮过程文本、末尾思考片段
//! 之前到达前端：工具行插到随附说明前面、思考快照落到工具行之后另起一块——与库回放
//! 的 thinking → text → status 顺序不符。同一条 FIFO 队列才是顺序保证。
//!
//! 入队由运行时在 turn 任务上 `send().await` 完成（见 `TurnObserver` 文档）：与最终回复
//! 同档，满载时等位而非丢弃——💭 突发灌满队列时丢的是可丢档的思考增量，不是这里的
//! 开始 / 结束事件。控制消息不标 intermediate：APK 路径上 dispatcher 的满载分流与滞留
//! 车道只丢 intermediate、按序保留其余（最终回复档），转发超时也用加长宽限。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::Emitter;

use rclaw_core::BusMessage;
use rclaw_runtime::{ToolBatchBegun, ToolBatchFinished, TurnObserver};

/// Webview 事件载荷（`tool_calls`）：`phase` 区分开始 / 结束。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum ToolCallsPayload<'a> {
    Begun {
        chat_id: &'a str,
        batch: &'a ToolBatchBegun,
    },
    Finished {
        chat_id: &'a str,
        batch: &'a ToolBatchFinished,
    },
}

/// outbound 队列里的控制消息标记。正文 = 前缀 + [`ToolCallsPayload`] 的 JSON（便于日志
/// 辨认）；但判定不看正文——模型在工具轮的过程文本原样出站，正文里的任何前缀都伪造
/// 得出来。带外标记放在 `display_content`：运行时构造的出站消息一律把它留空，模型文本
/// 只进 `content`，无法触及。只有 [`control_payload`] 认到带外标记的消息才按 `tool_calls`
/// 事件发出，其余一律是普通 `bus` 文本。
const CONTROL_PREFIX: &str = "[:rclaw-tool-calls]\n";

/// GUI 是否在场（桌面装配即在场；APK 无头服务先装配、GUI 后到）。不在场时 outbound
/// 桥没有运行，事件不交回运行时：没有人看直播，历史由库回放；也不让它们在 desktop
/// 通道的待接收队列里积压、等 GUI 接管时再回放一遍过时的开始 / 结束。
static GUI_ATTACHED: AtomicBool = AtomicBool::new(false);

/// 标记 GUI 出口就绪（幂等）。
pub fn attach_app() {
    GUI_ATTACHED.store(true, Ordering::Release);
}

/// 与 `outbound_bridge` 同过滤：只有桌面虚拟通道、且不是 Bot Mode 的 grp- 内部编排
/// 会话才推给前端。
fn visible(msg: &BusMessage) -> bool {
    msg.channel == crate::agent::CHANNEL_DESKTOP && !msg.chat_id.starts_with("grp-")
}

/// outbound 消息是否本模块产出的控制消息；是则返回 JSON 载荷。判定依据是带外标记
/// （`display_content` == 前缀），正文前缀只是形态核对——模型文本即使整段照抄前缀 +
/// JSON，也只是普通 `bus` 文本。不看 intermediate：控制消息按最终回复档出站。
pub fn control_payload(msg: &BusMessage) -> Option<&str> {
    if msg.display_content != CONTROL_PREFIX {
        return None;
    }
    msg.content.strip_prefix(CONTROL_PREFIX)
}

/// 控制消息载荷的形态校验：`phase` ∈ {begun, finished}、`chat_id` 与消息一致、`batch`
/// 为对象且 `iter` 为数字、按阶段带 `calls` / `results` 数组。前端在 React 状态更新器
/// 里消费它，形态不对会把整棵树卸掉——出口前兜底，不信任「反正是自己发的」。
fn valid_control_payload(payload: &serde_json::Value, chat_id: &str) -> bool {
    let Some(object) = payload.as_object() else {
        return false;
    };
    let phase = object.get("phase").and_then(serde_json::Value::as_str);
    if object.get("chat_id").and_then(serde_json::Value::as_str) != Some(chat_id) {
        return false;
    }
    let Some(batch) = object.get("batch").and_then(serde_json::Value::as_object) else {
        return false;
    };
    if !batch.get("iter").is_some_and(serde_json::Value::is_number) {
        return false;
    }
    let list = match phase {
        Some("begun") => "calls",
        Some("finished") => "results",
        _ => return false,
    };
    batch.get(list).is_some_and(serde_json::Value::is_array)
}

/// `outbound_bridge` 出口：把控制消息发成 `tool_calls` Webview 事件。形态不合法的载荷
/// 丢弃并告警（带外标记已证明它来自本模块，不会是用户正文）。
pub fn emit_control(app: &tauri::AppHandle, chat_id: &str, json: &str) {
    let payload = match serde_json::from_str::<serde_json::Value>(json) {
        Ok(payload) if valid_control_payload(&payload, chat_id) => payload,
        Ok(_) => {
            tracing::warn!("tool_calls 控制消息形态不合法，已丢弃");
            return;
        }
        Err(e) => {
            tracing::warn!("tool_calls 控制消息载荷非法: {e}");
            return;
        }
    };
    if let Err(e) = app.emit("tool_calls", payload) {
        tracing::debug!("tool_calls 事件推送失败: {e}");
    }
}

/// 运行时工具调用事件 → outbound 队列控制消息（由运行时投递）→ `tool_calls` Webview 事件。
#[derive(Default)]
pub struct ToolEventObserver;

impl ToolEventObserver {
    pub fn new() -> Self {
        Self
    }

    /// 编成控制消息交回运行时。不标 intermediate（见模块文档）；带外标记写在
    /// `display_content`——模型文本到不了这个字段。
    fn control(&self, msg: &BusMessage, payload: ToolCallsPayload<'_>) -> Option<BusMessage> {
        let json = match serde_json::to_string(&payload) {
            Ok(json) => json,
            Err(e) => {
                tracing::warn!("tool_calls 事件序列化失败: {e}");
                return None;
            }
        };
        let mut control = BusMessage::new(
            msg.channel.clone(),
            msg.chat_id.clone(),
            format!("{CONTROL_PREFIX}{json}"),
        );
        control.display_content = CONTROL_PREFIX.to_string();
        Some(control)
    }
}

impl TurnObserver for ToolEventObserver {
    fn turn_begun(&self, _msg: &BusMessage) {}

    fn turn_terminal(&self, _msg: &BusMessage, _reply: &str) {}

    fn turn_ended(&self, _msg: &BusMessage) {}

    /// 只看桌面会话：napcat / 微信等外部通道的 turn 不让运行时白组装载荷；GUI 不在场
    /// （APK 无头期）时同样不组装。
    fn wants_tool_events(&self, msg: &BusMessage) -> bool {
        visible(msg) && GUI_ATTACHED.load(Ordering::Acquire)
    }

    fn tool_calls_begun(&self, msg: &BusMessage, batch: &ToolBatchBegun) -> Option<BusMessage> {
        if !self.wants_tool_events(msg) {
            return None;
        }
        self.control(
            msg,
            ToolCallsPayload::Begun {
                chat_id: &msg.chat_id,
                batch,
            },
        )
    }

    fn tool_calls_finished(
        &self,
        msg: &BusMessage,
        batch: &ToolBatchFinished,
    ) -> Option<BusMessage> {
        if !self.wants_tool_events(msg) {
            return None;
        }
        self.control(
            msg,
            ToolCallsPayload::Finished {
                chat_id: &msg.chat_id,
                batch,
            },
        )
    }
}

/// 复合观察者：运行时每实例只接受一个观察者，宿主需要多个（APK：前台通知状态机 +
/// 工具事件桥）时在这里广播。各成员自行过滤，顺序 = 构造顺序。桌面只装一个观察者，
/// 这里在桌面构建下无使用方。
#[cfg_attr(
    not(all(target_os = "android", feature = "apk-host")),
    allow(dead_code)
)]
pub struct CompositeObserver(pub Vec<Arc<dyn TurnObserver>>);

/// 运行时每个事件只投递一条控制消息：各成员都调用（副作用照常），取第一条交回的；
/// 多个成员都交回时后面的丢弃并告警——装配上只有工具事件桥会交回。
#[cfg_attr(
    not(all(target_os = "android", feature = "apk-host")),
    allow(dead_code)
)]
fn first_control(
    observers: &[Arc<dyn TurnObserver>],
    what: &str,
    mut call: impl FnMut(&dyn TurnObserver) -> Option<BusMessage>,
) -> Option<BusMessage> {
    let mut picked = None;
    for observer in observers {
        match (call(observer.as_ref()), &picked) {
            (Some(control), None) => picked = Some(control),
            (Some(_), Some(_)) => {
                tracing::warn!("多个观察者都为 {what} 交回控制消息，只投递第一条");
            }
            (None, _) => {}
        }
    }
    picked
}

impl TurnObserver for CompositeObserver {
    fn turn_begun(&self, msg: &BusMessage) {
        for observer in &self.0 {
            observer.turn_begun(msg);
        }
    }

    fn turn_terminal(&self, msg: &BusMessage, reply: &str) {
        for observer in &self.0 {
            observer.turn_terminal(msg, reply);
        }
    }

    fn turn_ended(&self, msg: &BusMessage) {
        for observer in &self.0 {
            observer.turn_ended(msg);
        }
    }

    /// 任一成员关心即组装载荷（成员各自仍按自己的过滤决定是否上报）。
    fn wants_tool_events(&self, msg: &BusMessage) -> bool {
        self.0
            .iter()
            .any(|observer| observer.wants_tool_events(msg))
    }

    fn tool_calls_begun(&self, msg: &BusMessage, batch: &ToolBatchBegun) -> Option<BusMessage> {
        first_control(&self.0, "tool_calls_begun", |observer| {
            observer.tool_calls_begun(msg, batch)
        })
    }

    fn tool_calls_finished(
        &self,
        msg: &BusMessage,
        batch: &ToolBatchFinished,
    ) -> Option<BusMessage> {
        first_control(&self.0, "tool_calls_finished", |observer| {
            observer.tool_calls_finished(msg, batch)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_control_messages_are_recognised_but_forgeries_are_not() {
        // GUI_ATTACHED 是进程级单向门闩（生产上无「断开」）：本测试二进制内任何断言
        // 「未 attach 时不交回」的用例都会与本用例产生顺序依赖——需要时请新开测试
        // 二进制或引入 cfg(test) 的复位钩子，不要在这里依赖初值。
        attach_app();
        let observer = ToolEventObserver::new();
        let turn = BusMessage::new(crate::agent::CHANNEL_DESKTOP, "c1", "做点事");
        let batch = ToolBatchBegun {
            iter: 2,
            calls: vec![rclaw_runtime::ToolCallBegun {
                id: "call_0".into(),
                name: "read".into(),
                input: serde_json::json!({"path": "a.rs"}),
                input_truncated: false,
                skill: None,
            }],
        };
        assert!(observer.wants_tool_events(&turn));
        let control = observer
            .tool_calls_begun(&turn, &batch)
            .expect("应交回控制消息");
        // 最终回复档：不标 intermediate，dispatcher 满载时按序保留而不是丢弃。
        assert!(!control.is_intermediate());
        assert_eq!(control.channel, crate::agent::CHANNEL_DESKTOP);
        assert_eq!(control.chat_id, "c1");
        let json = control_payload(&control).expect("观察者产出的消息应被认作控制消息");
        let payload: serde_json::Value = serde_json::from_str(json).unwrap();
        assert!(valid_control_payload(&payload, "c1"));
        assert_eq!(payload["phase"], "begun");
        assert_eq!(payload["batch"]["iter"], 2);
        assert_eq!(payload["batch"]["calls"][0]["name"], "read");
        // 判定不看 intermediate 标志。
        assert!(control_payload(&control.clone().with_intermediate()).is_some());
        // 与消息 chat_id 不符 / 阶段未知 / 缺列表的载荷过不了形态校验。
        assert!(!valid_control_payload(&payload, "c2"));
        assert!(!valid_control_payload(
            &serde_json::json!({"phase": "x", "chat_id": "c1", "batch": {"iter": 1, "calls": []}}),
            "c1"
        ));
        assert!(!valid_control_payload(
            &serde_json::json!({"phase": "finished", "chat_id": "c1", "batch": {"iter": 1}}),
            "c1"
        ));

        // 模型文本整段照抄前缀 + 合法 JSON（工具轮过程文本原样出站，intermediate 与否
        // 都有）：没有带外标记，仍是普通 bus 文本。
        let forged = BusMessage::new(crate::agent::CHANNEL_DESKTOP, "c1", control.content.clone());
        assert_eq!(control_payload(&forged), None);
        assert_eq!(control_payload(&forged.clone().with_intermediate()), None);
        // 普通过程文本 / 思考流。
        let thinking =
            BusMessage::new(crate::agent::CHANNEL_DESKTOP, "c1", "💭\n想想").with_intermediate();
        assert_eq!(control_payload(&thinking), None);

        // 外部通道 / grp- 编排会话的 turn 不组装、不交回。
        let napcat = BusMessage::new(rclaw_core::CHANNEL_NAPCAT, "u1", "hi");
        assert!(!observer.wants_tool_events(&napcat));
        assert!(observer.tool_calls_begun(&napcat, &batch).is_none());
        let grp = BusMessage::new(crate::agent::CHANNEL_DESKTOP, "grp-1", "hi");
        assert!(observer.tool_calls_begun(&grp, &batch).is_none());
    }
}
