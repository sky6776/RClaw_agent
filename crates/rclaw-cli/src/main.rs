//! rclaw 顶层二进制（console agent + 装配启动）。
//!
//! 装配：config → tracing → 布局/存储 → 通道 → 核心组件（bootstrap）→
//! agent loop + dispatcher + cron + heartbeat，最后常驻。
//! 与通道无关的 provider / 工具 / opts / cron / slash 装配在 [`rclaw_cli::bootstrap`]
//! （与 rclaw-desktop 共用，避免两份装配漂移）。

use std::path::PathBuf;

use anyhow::Result;
use tokio::sync::mpsc;

use rclaw_channels::{Channel, Console, NapCat, OutboundDispatcher, Weixin};
use rclaw_core::{
    BusMessage, ChannelCaps, RuntimeChannelInfo, CHANNEL_CLI, CHANNEL_NAPCAT, CHANNEL_WEIXIN,
};
use rclaw_runtime::{AgentLoop, CronService, Heartbeat};

use rclaw_cli::bootstrap;
use rclaw_cli::config::Config;
use rclaw_cli::logging;

#[tokio::main]
async fn main() -> Result<()> {
    // daemon 后台模式：runtime 内部经 AgentOptions.daemon 消费（C3）；
    // 环境变量仅保留为 shell/skill 子进程契约（技能脚本经 os.environ 读取）。
    let daemon = std::env::args().any(|a| a == "--daemon");
    if daemon {
        std::env::set_var("RCLAW_DAEMON", "1");
    } else {
        std::env::remove_var("RCLAW_DAEMON");
    }

    // 1) 配置（先于日志：日志等级 / 输出目标由 rclaw.toml 的 [log] 段决定，
    //    必须先读到配置才能据此装配 tracing）。
    let cfg = Config::load()?;

    // 2) 存储布局（先于日志：日志默认写入 agent/logs，须先确定 layout 与 logs_dir）。
    let layout = bootstrap::prepare_storage(&cfg)?;

    // 3) 日志：默认写入 agent/logs（layout.logs_dir）；[log].dir 非空则尊重用户配置。
    logging::init_tracing(&cfg.log, &layout.logs_dir)?;

    tracing::info!("rclaw 启动");
    // 所有外部 channel 共用这一媒体沙箱（C3 起经 RuntimeChannelInfo 传入 runtime）。
    let media_dir = if layout.base.is_absolute() {
        layout.base.join("media")
    } else {
        std::env::current_dir()?.join(&layout.base).join("media")
    };
    let media_dir_str = media_dir.to_string_lossy().replace('\\', "/");

    // 通道对象先构造（能力声明在接线期采集），路由注册与 spawn 留在后段。
    // 只有真实构造成功的通道才声明媒体能力，避免配置存在但通道未启动时生成假 marker。
    let console_channel = Console::new();
    let mut wxcfg = cfg.weixin_channel_config();
    wxcfg.media_dir = media_dir.clone();
    let wx_enabled = wxcfg.enabled;
    let wx_channel = if wx_enabled { Weixin::new(wxcfg) } else { None };
    let mut ncfg = cfg.napcat_channel_config();
    ncfg.media_dir = media_dir.clone();
    let napcat_enabled = ncfg.enabled;
    let napcat_channel = if napcat_enabled {
        NapCat::new(ncfg)
    } else {
        None
    };
    let mut channel_caps: Vec<ChannelCaps> = Vec::new();
    channel_caps.push(console_channel.capabilities());
    if let Some(wx) = &wx_channel {
        channel_caps.push(wx.capabilities());
    }
    if let Some(nc) = &napcat_channel {
        channel_caps.push(nc.capabilities());
    }
    let channel_info = RuntimeChannelInfo {
        media_dir: Some(media_dir_str.clone()),
        channels: channel_caps,
    };
    // 子进程契约：只有真实构造成功的通道才声明能力（见上），避免生成假 marker。
    std::env::remove_var("RCLAW_WEIXIN_ENABLED");
    if wx_channel.is_some() {
        std::env::set_var("RCLAW_WEIXIN_ENABLED", "1");
    }
    std::env::remove_var("RCLAW_NAPCAT_ENABLED");
    if napcat_channel.is_some() {
        std::env::set_var("RCLAW_NAPCAT_ENABLED", "1");
    }

    // 4) 核心组件（provider / 工具 / opts / cron / slash）——与桌面版共用同一装配。
    let core = bootstrap::assemble_core(
        &cfg,
        &layout,
        &channel_info,
        daemon,
        PathBuf::from("rclaw.toml"),
    )
    .await?;

    // 5) 两条总线（tokio mpsc，承运 BusMessage，容量 64）。
    //    · inbound_tx：console / weixin / napcat / cron / heartbeat 各 clone 投入消息。
    //    · inbound_rx：唯一消费者 AgentLoop。
    //    · outbound_tx：agent loop 投入回复；outbound_rx：dispatcher 按 channel 二次路由。
    let (inbound_tx, inbound_rx) = mpsc::channel::<BusMessage>(64);
    let (outbound_tx, outbound_rx) = mpsc::channel::<BusMessage>(64);

    // 6) agent loop（路由模式：每会话一个活动 turn）。
    // Bot Mode：inbound 总线就绪后装配 BotRouter，回填给 message_agent 工具。
    let bot_router = std::sync::Arc::new(rclaw_runtime::BotRouter::new(
        inbound_tx.clone(),
        core.bots.clone(),
    ));
    let _ = core.bot_router_slot.set(bot_router.clone());
    // 后台完成通知的批量唤醒（ZCode 移植 · Layer 4）：inbound 总线就绪后接线。
    core.agent_runtime
        .background_wake()
        .attach(inbound_tx.clone());
    let agent = AgentLoop::new(
        core.provider,
        core.review_provider,
        core.session,
        core.memory,
        layout.clone(),
        core.tools,
        core.opts,
        inbound_rx,
        outbound_tx.clone(),
        Some(core.slash),
        channel_info,
        core.bots,
        Some(bot_router),
    );
    let agent_handle = tokio::spawn(agent.run());

    // 7) outbound 分发器 + 各通道路由与启动。
    let mut dispatcher = OutboundDispatcher::new(outbound_rx);

    let (cli_tx, cli_rx) = mpsc::channel::<BusMessage>(16);
    dispatcher.route_to(CHANNEL_CLI, cli_tx);
    let console_handle = tokio::spawn(console_channel.run(inbound_tx.clone(), cli_rx));

    if let Some(wx) = wx_channel {
        let (wx_tx, wx_rx) = mpsc::channel::<BusMessage>(16);
        dispatcher.route_to(CHANNEL_WEIXIN, wx_tx);
        tokio::spawn(wx.run(inbound_tx.clone(), wx_rx));
        tracing::info!("微信通道已启动");
    } else if wx_enabled {
        tracing::warn!("微信通道启用但 token 为空，已跳过");
    }

    if let Some(nc) = napcat_channel {
        let (nc_tx, nc_rx) = mpsc::channel::<BusMessage>(16);
        dispatcher.route_to(CHANNEL_NAPCAT, nc_tx);
        tokio::spawn(nc.run(inbound_tx.clone(), nc_rx));
        tracing::info!("NapCatQQ 通道已启动");
    } else if napcat_enabled {
        tracing::warn!("NapCatQQ 通道启用但 ws_url 为空，已跳过");
    }

    // 分发器自身常驻。
    tokio::spawn(dispatcher.run());

    // 8) cron + heartbeat 后台服务。
    tokio::spawn(
        CronService::new(core.cron, inbound_tx.clone(), cfg.cron.check_interval_sec).run(),
    );
    tokio::spawn(
        Heartbeat::new(
            layout.clone(),
            inbound_tx.clone(),
            cfg.heartbeat.interval_sec,
        )
        .run(),
    );

    // --daemon：后台常驻模式。nohup / termux-services / systemd 会把 stdin 重定向到
    // /dev/null，console 通道读到 EOF 会立刻结束；daemon 模式下不让 console EOF
    // 触发进程退出，只认 Ctrl+C / SIGTERM 或 agent loop 结束。
    tracing::info!(
        "rclaw 就绪（{}）—— Ctrl+C 退出",
        if daemon {
            "daemon 后台模式"
        } else {
            "前台模式，输入消息开始对话"
        }
    );

    // 9) 常驻：
    //   - 前台：console EOF（Ctrl+D / stdin 关闭）、Ctrl+C、agent 结束任一发生即退出。
    //   - daemon：忽略 console EOF，仅 Ctrl+C / agent 结束退出。
    tokio::select! {
        _ = console_handle, if !daemon => {
            tracing::info!("console 已结束，退出");
        }
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("收到 Ctrl+C，退出");
        }
        _ = agent_handle => {
            tracing::info!("agent loop 已结束");
        }
    }

    Ok(())
}
