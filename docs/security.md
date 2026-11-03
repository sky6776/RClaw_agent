# 安全设计

[← 返回 README](../README.md)

RClaw 的安全边界围绕四件事：文件 / shell 的 fail-safe 沙箱、通道身份到工具权限的单向链、敏感信息脱敏、记忆与技能写入的内容防护。本文如实描述覆盖范围与边界，不夸大。实现位于 `crates/rclaw-tools/src/access.rs`、`files/`、`crates/rclaw-store/src/{redact,memory,path_util}.rs`、`crates/rclaw-core/src/bus.rs`。

## 文件沙箱

- 默认权限 `Sandboxed`：**文件工具**限制在数据目录（`agent/`）内。
- **shell 不是沙箱**：`Sandboxed` 下只校验工作目录 `cwd` 必须位于数据目录内，命令正文不受任何路径限制（`cat ~/.ssh/id_rsa`、删除任意目录都能执行），只有长度 8192 字节 / 超时 / 输出 64 KiB 的上限。把它当成「以当前用户身份执行任意命令」来评估风险。
- 路径解析到**已存在符号链接的物理落点**后再判定；拒绝 `..` 越界、盘符相对路径（Windows）、DOS 设备名（Windows）。
- 无论权限等级，始终拒绝：读写 `state.sqlite` 及 `-wal` / `-shm` / `-journal`；删除工作区根、其祖先、文件系统根 / 挂载点。
- 越界访问：确认钩子是**进程级**的——桌面 / 安卓宿主下任何通道（含同进程接入的 QQ / 微信）的越界请求都会向本机用户弹窗，按「操作 + 物理落点」授权并缓存给后续所有请求方（120 秒无应答按拒绝）；CLI 宿主没有钩子，越界一律拒绝。
- `Unrestricted`：可越出工作区，但上述数据库与危险删除保护仍有效。

## 权限单向链

```
通道可信身份 → BusMessage.tool_access（私有字段） → turn task-local → 文件 / shell gate
```

- 权限由通道适配器按**真实身份**设置，消息正文无法伪造：NapCat 按 QQ 号、桌面按本机权限模式；微信 iLink 与 wxbot 通道恒为 `Sandboxed`，没有提权机制。
- NapCat：`block_from` 优先于 `allow_from`；只有命中 `max_from` 的**私聊**提升为 `Unrestricted`，群聊永远 `Sandboxed`；`max_from` 空 = 无人提权。
- **子代理继承发起会话的权限**：前台子代理内联运行天然继承；后台子代理在派生时捕获发起 turn 的权限快照并重建，进程重启恢复也按快照还原；工作流 run 同样固化启动时的权限快照。因此 `Unrestricted` 会话派生的子代理同样 `Unrestricted`。只有读不到上下文的路径（如后台 review fork）回退 `Sandboxed`；cron turn 固定 `Sandboxed`。
- 桌面 / 安卓「配置 → 工具权限模式 → 完全访问」（`permission_mode = "full"`）把窗口消息提升为 `Unrestricted`——这是显式的本机选择，请知悉后再开。桌面权限钩子会在工作流后台 ask / 写操作前重查：`full` 切回默认后，以 `Unrestricted` 启动的 run 会转为 `permission_revoked`。工作流的 `world.run` 仅 `Unrestricted` 可用，可用 `[agent] workflow_command_confirm` 要求逐命令确认（60 秒超时按拒绝）。
- **命令特权门**：`/clear`、`/log`、`/rewind`、`/export all`、`/skills pin|unpin|adopt|restore|curate` 只对 CLI / 桌面会话或已提升为 `Unrestricted` 的会话开放，外部通道普通用户收到「权限不足」。

## 敏感信息脱敏

`redact` 模块匹配：`sk-*` 类 API Key、GitHub PAT、AWS `AKIA…`、`Bearer` / `Basic` 凭据、URL userinfo、敏感键值对，替换为 `[REDACTED]`；CLI 参数形式（`--token=…` 等）只在 shell 命令预览 / 日志脱敏中生效，通用文本脱敏不含该规则。

覆盖范围：

| 场景 | 是否脱敏 |
|---|---|
| 工具结果进入上下文 / 落盘 artifact | 是 |
| 压缩摘要的输入与输出 | 是 |
| 记忆（`MEMORY.md` / `USER.md`）与技能写入 | 是 |
| 进化信号详情 | 是 |
| 部分日志与 MCP 传输错误 | 是 |
| 模型最终回复正文 | **否**（没有统一的回复脱敏；完成闸门只要求副作用错误经脱敏披露） |
| 手机号等个人信息 | **否**（没有相应规则） |

## 写入内容防护

- **记忆**：写入前做提示注入与隐形字符威胁扫描；手工绕过写入门的内容在加载期复扫，危险条目只在注入视图中替换为 `[BLOCKED: threat scanner: <原因>]`，不改磁盘；结构检测到漂移时保存 `.drift.bak` 并拒绝覆盖。
- **技能**：拒绝隐形 / 双向控制字符，密钥自动脱敏；不会因教学文本含「ignore instructions」之类字样误杀。`skill_manage` 的 read-before-write、单 mutation、pinned / `.protected` 禁写、所有权门见 [技能系统](skills.md#查看与管理)。
- **技能脚本参数**：Manifest 动态工具以 argv 直达进程、不经 shell；`enum` 白名单与类型在调用前校验；`path` 类参数按工作区解析并拒绝越界（见 [技能系统](skills.md#设计动机)）。
- **安卓**：技能脚本按哈希审批后才可执行（配置页 SkillOpsCard）。

## 密钥存放

- 主 LLM、通道与搜索 Key 来自 `rclaw.toml`（已 gitignore）。技能 Key 文件（`skills/ark_key.toml`、`skills/happyhorse_key.toml`、`skills/glm-master-skill/config.toml`、`skills/*/config.toml`）以空占位**入库且未 gitignore**——它们就是真实文件而非模板，填入 Key 后 `git add -A` 会直接提交，请自行加入本地排除（`.git/info/exclude`）或提交前核对。清单见 [快速开始](getting-started.md#api-key-配置清单)。
- gbrain 的 Key 以 `GBRAIN_*` 环境变量形式写在 `[mcp.servers.gbrain.env]` 段：安卓 APK 由运行时注入进程内子系统，桌面 / CLI 作为 MCP 子进程的环境变量传入；都来自 TOML 而非宿主环境。
- MCP `headers` 以明文字符串配置；错误信息不回显 header 值，传输错误经脱敏；配置结构没有 Secret 包装类型。
- 首运行向导：provider 名只允许字母数字与 `-` `_`；API Key 与模型名拒绝引号、反斜杠与控制字符（防 TOML 注入）；`base_url` 只检查 `http(s)` 前缀。

## 网络侧限制

- `download_media` 仅接受公网 HTTP(S)：DNS 全解析校验并钉住地址，拒绝环回 / 私网 / 链路本地地址，忽略环境代理；单文件 200 MB、总时限 180 秒、同主机重定向最多 5 跳。
- wxbot 通道的 API 与 Webhook 强制 loopback 地址，Webhook 以 Bearer secret 校验、队列满返回 503，并禁用重定向与环境代理。loopback 不替代鉴权——同机其他进程仍可伪造 Webhook 事件。
- 出站媒体标记只接受规范化后仍位于 `agent/media/` 内的真实路径，并须经发布工具回执验证（媒体认领闸门）。
