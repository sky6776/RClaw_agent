//! console (CLI) 通道。
//!
//! 控制台通道：
//! - stdin 通过 rustyline 行编辑读取（支持方向键 / 行内编辑 / 历史）
//!   → 投递 inbound（channel=cli, chat_id=console）；
//! - 消费 outbound → 打印 `agent[chat_id]:\n<content>`。
//!
//! 注意（与 agent loop 的协作）：CLI + 流式模式下，agent loop 会直接把
//! token 流式打到 stdout，并跳过对最终文本的 outbound 推送，避免重复打印；
//! 因此本通道的 outbound 消费端主要处理错误回复 / 系统消息等。
//!
//! 提示符（"> "）由 rustyline 在每次 readline 时自行渲染，故本通道不再
//! 手动打印/重绘提示符，避免与行编辑器的光标、重绘状态冲突。

use async_trait::async_trait;
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use tokio::sync::mpsc;

use rclaw_core::{BusMessage, CHANNEL_CLI};

use crate::{Channel, OutboundReceiver};

/// CLI 提示符（交由 rustyline 渲染）。
const PROMPT: &str = "> ";
/// CLI 下的固定 chat_id。
const CHAT_ID: &str = "console";

/// 控制台通道。
pub struct Console;

impl Console {
    pub fn new() -> Self {
        Console
    }
}

/// 提供 `Default` 实现，满足 clippy `new_without_default`：
/// `Console::new()` 是无参构造，约定上应能用 `Default::default()` 等价获得。
impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Channel for Console {
    fn name(&self) -> &str {
        CHANNEL_CLI
    }

    fn capabilities(&self) -> rclaw_core::ChannelCaps {
        // CLI 走终端与流式输出，无媒体、无工作状态推送、无中间输出转发。
        rclaw_core::ChannelCaps {
            name: CHANNEL_CLI.to_string(),
            ..Default::default()
        }
    }

    async fn run(self, inbound: mpsc::Sender<BusMessage>, mut outbound: OutboundReceiver) {
        // 1) outbound 消费端：收到回复就打印。
        //    提示符不再由此处重绘——交由 rustyline 在 readline 时统一渲染，
        //    避免与行编辑器的光标 / 重绘状态冲突。
        tokio::spawn(async move {
            while let Some(msg) = outbound.recv().await {
                let who = if msg.chat_id.is_empty() {
                    CHAT_ID
                } else {
                    &msg.chat_id
                };
                println!("\nagent[{who}]:\n{}", msg.content);
            }
        });

        // 2) stdin 读取（rustyline 是同步阻塞调用，放进 spawn_blocking，
        //    避免阻塞 runtime；行编辑器在该阻塞线程上构造与使用）。
        let stdin_handle = tokio::task::spawn_blocking(move || {
            let mut rl = match DefaultEditor::new() {
                Ok(rl) => rl,
                Err(err) => {
                    tracing::error!("初始化 rustyline 失败: {err:?}");
                    return;
                }
            };
            loop {
                let line = match rl.readline(PROMPT) {
                    Ok(line) => line,
                    Err(ReadlineError::Interrupted) => break, // Ctrl-C：退出（与改造前一致）
                    Err(ReadlineError::Eof) => break,         // Ctrl-D：退出
                    Err(err) => {
                        tracing::error!("readline 出错，控制台通道退出: {err:?}");
                        break;
                    }
                };

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                // 记入历史，供 上/下 方向键回溯。
                let _ = rl.add_history_entry(line.as_str());

                // blocking_send：inbound 通道满时阻塞直到 agent loop 消费。
                // 通道关闭（进程退出）时返回 Err，循环自然结束。
                if inbound
                    .blocking_send(BusMessage::new(CHANNEL_CLI, CHAT_ID, trimmed.to_string()))
                    .is_err()
                {
                    break;
                }
            }
        });

        // 等到 stdin EOF（或出错）时通道结束。
        let _ = stdin_handle.await;
        tracing::info!("Console 通道退出");
    }
}
