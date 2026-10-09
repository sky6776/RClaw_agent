//! Heartbeat 服务。
//!
//! 周期检查 `agent/HEARTBEAT.md`，若存在可执行任务（非空、非标题、非已完成勾选），
//! 则把固定提示注入 inbound，触发 agent 自检。

use std::fs;
use std::time::Duration;

use tokio::sync::mpsc;

use rclaw_core::{BusMessage, CHANNEL_SYSTEM};
use rclaw_store::fs::Layout;

/// heartbeat 提示文本。
const HEARTBEAT_PROMPT: &str = "Read agent/HEARTBEAT.md and follow any instructions or tasks listed there. If nothing needs attention, reply with just: HEARTBEAT_OK";

pub struct Heartbeat {
    layout: Layout,
    inbound_tx: mpsc::Sender<BusMessage>,
    interval_sec: u64,
}

impl Heartbeat {
    pub fn new(layout: Layout, inbound_tx: mpsc::Sender<BusMessage>, interval_sec: u64) -> Self {
        Heartbeat {
            layout,
            inbound_tx,
            interval_sec,
        }
    }

    /// 启动心跳循环。
    pub async fn run(self) {
        loop {
            tokio::time::sleep(Duration::from_secs(self.interval_sec.max(60))).await;
            if self.tick().await {
                tracing::debug!("heartbeat 已触发");
            }
        }
    }

    async fn tick(&self) -> bool {
        if !has_tasks(&self.layout.heartbeat_file) {
            return false;
        }
        // 用 `send().await` 而非 `try_send`：队列满时阻塞等待，对 agent 施加背压，
        // 而不是像 `try_send` 那样在队列满时静默丢弃。旧实现忽略了 `try_send` 的失败
        // 结果仍返回 `true`，假装已触发但心跳实际没进 agent——队列满时会漏掉这次自检。
        // 仅在 inbound 接收端全部关闭（通常进程退出）时 `send` 才返回 Err：此时返回
        // `false` 并记日志，不假装成功。
        //
        // 用 `new_heartbeat`（source = Heartbeat）而非旧的 `new_internal`：这样 agent loop
        // 的 `is_cron_task()` 对心跳返回 false，heartbeat 不再被误判为 cron——不会跳过
        // 普通历史、也不会注入「Cron Task Handling」提示（修复 P2「heartbeat 被误判为 cron」）。
        match self
            .inbound_tx
            .send(BusMessage::new_heartbeat(
                CHANNEL_SYSTEM,
                "heartbeat",
                HEARTBEAT_PROMPT.to_string(),
            ))
            .await
        {
            Ok(()) => true,
            Err(e) => {
                tracing::warn!("heartbeat 入队失败（inbound 已关闭，疑似进程退出）: {e:?}");
                false
            }
        }
    }
}

/// 检查 HEARTBEAT.md 是否含可执行任务行。
fn has_tasks(path: &std::path::Path) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        // 跳过已完成勾选：- [x] / * [x]
        if (t.starts_with("- ") || t.starts_with("* ")) && t[2..].starts_with('[') {
            let mark = t.as_bytes().get(3).copied();
            if matches!(mark, Some(b'x') | Some(b'X')) && t[4..].starts_with(']') {
                continue;
            }
        }
        return true; // 发现可执行行
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_actionable_line() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("HB.md");
        fs::write(&p, "# Heartbeat\n\n- [ ] todo A\n").unwrap();
        assert!(has_tasks(&p));
    }

    #[test]
    fn ignores_completed_only() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("HB.md");
        fs::write(&p, "# Heartbeat\n\n- [x] done\n").unwrap();
        assert!(!has_tasks(&p));
    }
}
