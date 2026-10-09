# RClaw 桌面版（Windows / Tauri 2 + React）

把 RClaw 打包成 Windows 桌面应用：托盘常驻 + 原生窗口对话界面。核心与
`rclaw-cli` 完全共用（`rclaw-cli` 以 lib 形式暴露 `config` / `bootstrap` /
`logging`），桌面壳只是一个「虚拟通道」：inbound 由窗口输入写入总线，
outbound 经 Tauri 事件推给 Webview。

## 构建

前端产物必须先于 Rust 编译生成（tauri 在 `generate_context!` 编译期内嵌
`ui/dist`）：

```bash
cd crates/rclaw-desktop/ui
npm install
npm run build
cd ../..
cargo build --release -p rclaw-desktop
```

产物：`target/release/rclaw-desktop.exe`（绿色单文件，数据写入
`%USERPROFILE%\.rclaw`，与工作目录无关；旧版本的 `%APPDATA%\rclaw` 会在
升级后首次启动时自动整体迁入）。可选：`RCLAW_HOME` 环境变量覆盖数据目录。

## 运行语义

- **首运行**：无 `rclaw.toml` 时进入配置向导（provider 预设 / API key / 模型 /
  上下文窗口），自进化（auto_memory / auto_evolve / skill_curator）默认开启，
  生成配置后立即启动 agent；
- **常驻**：点窗口关闭 = 隐藏到托盘；托盘菜单「退出」才结束进程；
- **安全语义与 CLI 一致**：模型流式片段在完成闸门通过前不外发，界面只显示
  outbound 已有的状态消息；agent 存储沙箱保持默认（无 Unrestricted）；
- **设置复用 slash 管道**：`/llmodel`、`/context`、`/cancel` 等直接在输入框输入，
  不另开配置命令面。

## 模块

| 文件 | 职责 |
| --- | --- |
| `src/home.rs` | 数据目录解析（`RCLAW_HOME` > `%USERPROFILE%\.rclaw` > XDG > `~/.config`）+ 旧 `%APPDATA%\rclaw` 一次性迁移 |
| `src/wizard.rs` | 首运行 `rclaw.toml` 生成（字段白名单校验，防 TOML 注入） |
| `src/agent.rs` | agent loop 装配 + outbound→事件桥（`bus` / `agent` 事件） |
| `src/commands.rs` | Tauri command 层（`get_status` / `wizard_save` / `send_message` / `cancel` / `cron_list` / `cron_remove`） |
| `src/state.rs` | managed state：数据目录 + 运行中 agent 句柄 |
| `ui/` | React + Vite + Tailwind 前端（向导 / 对话 / 定时任务 / 关于） |
