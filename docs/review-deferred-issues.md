# 审查遗留问题清单（未修复项）

来源：对 `0a841159..` 工作树的多轮只读审查（agent_tools 16 轮、hosts/tools 12 轮、
store/turn 12 轮），三个切片最终都已无 P1/P2。下面是审查员指出、经分析后**决定暂不修**的
项，全部为 P3（低频 / 需要本地攻击者卡时间窗 / 纯措辞），按切片分组；每条给出位置、机制、
影响与建议修法，日后回头处理时直接按条销号。行号为 2026-10-13 的工作树，可能漂移，以函数
名为准。

## A. agent_tools —— PlanRun 验证环（`crates/rclaw-runtime/src/agent_tools/plan.rs`）

### A6 简报与任务文本同时建不起引用集时归因为 `Brief`
- 位置：`VerifierInputs::reference_set_failure`（约 632 行起）。
- 机制：`(brief_unavailable, task_text_unavailable) == (true, true)` 时归因 `Brief`，诊断与
  报告前缀都承诺「resume 会重跑该步骤」；但重跑目标改变不了任务文本，下一轮才会报
  `TaskText`，白跑一次目标。
- 影响：现版本的 `verification_brief` 总是先经 `neutralize_unclosed_json` 中和，简报侧实际
  够不到上限，该分支基本不可达；只有旧版 journal 里未中和的简报配合残缺任务文本才会触发。
- 建议：先查任务文本（`(_, true) => TaskText`），或增加 `Both` 归因并把两条诊断合并。

### A7 「对抗环」报告行对预检失败也写「验证者不可用」
- 位置：报告拼装处 `format!("未进入 verdict 轮次（验证者不可用）{last_reasons}")`（约 2366 行）。
- 机制：派发前引用集预检失败（`step_unverified{reason: "reference set unavailable"}`）时验证者
  是「未派发」而非「不可用」，只靠后面追加的「最后原因」区分。
- 建议：改为「（验证者未派发 / 不可用）」，或按 `verdict_feedback` 的来源分支措辞。

### A8 `looks_like_json_key_start` 可再缩小自愈盲区
- 位置：`looks_like_json_key_start`（约 387 行起）。
- 机制：现在要求引号后的**原始**下一个字符不是空白且不在 `, } ] :` 之内，所以首键以空白开头
  的对象（`{" k": 1}`）在相位错位时不被自愈。合法 JSON 里收尾引号之后首个**非空白**字符只能是
  `, } ] :` 或文末，因此改成 `after_quote.trim_start().chars().next()` 再判标点，对合法 JSON
  零风险，盲区缩到「首键以 `, } ] :` 开头」。
- 影响：极低——模型几乎不会产出空白开头的键；代价是正文垃圾里多几次入栈（都在现有上限内）。
- 建议：若采纳，同步改 `ScanMode::PhaseRobust` / `all_json_values` 注释与设计文档里的盲区描述。

### A9 设计文档一处过时术语
- 位置：`docs/zcode-port-longhorizon.md` 第 648 行附近，测试清单条目
  `classify_verdict_sees_echo_sources_hidden_behind_a_truncated_dependency_summary` 仍把
  `all_json_values` 叫「重同步扫描」；其余处均已叫「相位自愈 / `PhaseRobust`」。纯措辞。

## B. store/turn（`crates/rclaw-store/src/session.rs`）

### S6 `first_user_messages` / `search_messages` 的 `json_each` 未加防护
- 位置：`first_user_messages`（约 418–419 行）、`search_messages`（约 596–598 行）。
- 机制：两处仍是裸的 `json_each(m.data, '$.content')` + `json_extract(b.value, '$.type')`，没有
  `load_transcript_tail` 那套 `CASE WHEN json_valid(...) ... CASE WHEN b.type = 'object' THEN
  json_extract(...) END` 保护。某行 `data` 不是合法 JSON（`{not json`）或 `content` 是 serde 能
  接受的裸标量数组（`["bare"]`）时，SQLite 抛 `malformed JSON`，整条语句失败。
- 影响：优雅降级——会话标题回退为空（`commands.rs` 约 543 行），`session_search` 对整个库返回
  Err。需要手工损坏的行才能触发。
- 建议：复用 `load_transcript_tail` 的 `CASE` 形状（`search_messages` 对应改为 `EXISTS`）。

## C. hosts/tools —— 文件沙箱与桌面命令

### H8 `is_dos_device_name` 没按 `:` 截主干
- 位置：`crates/rclaw-tools/src/files/helpers.rs` `is_dos_device_name`（约 130–141 行）。
- 机制：Win32 `RtlIsDosDeviceName_U` 在第一个 `.` **或 `:`** 处截断叶子名，现在只按 `.` 截，
  `agent/con:x`、`nul:stream` 能过 `join_workspace_path`。Windows 下 `write agent/con:x`：原子写
  先建临时文件 `.con:x.<nonce>.tmp`（= 文件 `.con` + 备用数据流），rename 到 CON 失败，清理只
  删了流，工作区里留下一个空的 `.con` 并报 OS 错误；读在 `metadata` 处失败，不会卡住。
- 建议：`segment.split(['.', ':']).next()`；或像桌面端 `validate_workspace_rel_path` 那样在
  Windows 上直接拒绝 `Normal` 分量里的 `:`。

### H10 悬空链接的弹窗说明指向条目自身
- 位置：`crates/rclaw-tools/src/access.rs` `affected_target`（约 211–216 行）。
- 机制：`followed = physical_normalize(resolved)`；悬空叶子 `exists()` 为假，回退成「父目录物理
  路径 + 名字」，说明文案变成「符号链接条目本身，指向 /outside/link；不碰目标」（即自己）。
  授权键（Entry）本身正确，只是文案误导。
- 建议：说明文案改用 `fs::read_link(resolved)` 的原始目标，或 `followed == 条目路径` 时写
  「目标不存在」。

### H11 Windows 下仅大小写 / 8.3 不同也显示「重定向」子句
- 位置：`access.rs` `PathConfirmRequest::display`（约 109 行起）。
- 机制：`requested != target` 按原字符串比较，`c:/users/v/x.txt` 与规范形 `C:/Users/v/x.txt`
  会显示「（请求拼写 … 指向它）」，其实没有经过链接 / `..`。
- 建议：子句按 `granted_key` 折叠后的字符串（Windows 小写）判断是否不同。

### H12 `open_regular_file` 加 `O_NOCTTY`，并注释租约语义
- 位置：`helpers.rs` `open_regular_file`（约 792–808 行）。
- 机制：Unrestricted 下从会话首进程 `read /dev/ttyN` 可能获得控制终端（`File::open` 时代就有）。
  另外 `O_NONBLOCK` 让持有写租约（如 Samba oplock 导出的工作区）的文件 `open()` 直接
  `EWOULDBLOCK` 而不是等 lease-break，表现为「读取失败: Resource temporarily unavailable」——
  可重试、可接受，但值得注释。
- 建议：unix 分支 `custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY)`，并在注释里写明租约行为。

### H14 两处拒绝文案仍写「不能包含 '..'」
- 位置：`helpers.rs` 约 56 行、`crates/rclaw-tools/src/shell.rs` 约 266 行。
- 机制：工作区内折叠后仍在内部的 `..`（`agent/sub/../x`）现在已经放行，这句只在真正的工作区
  外路径被拒时显示，措辞过时。纯文案。

### T1 授权门与删除工具之间的「条目 / 对象」类型 TOCTOU
- 位置：`access.rs` `ensure_path_allowed` / `affected_target`（按 `is_symlink_leaf` 判定
  `GrantKind::Entry`）；`crates/rclaw-tools/src/files.rs` `impl Tool for DeleteFile`（约 298 行起，
  `remove_dir_all` 在约 375 行；重新 stat 后按当时类型动手）。
- 机制：在 Entry 授权命中之后（或弹窗等待的 ≤120 s 内）把 `/outside/link` 换成真实目录，删除
  工具看到的已是目录，会在一条「只摘条目、不碰目标」的授权下执行 `remove_dir_all`。
- 影响：需要本地攻击者精确卡时间窗；H9 之后两种授权已分键，剩下的只是门与执行之间的窗口。
- 建议：让 `ensure_path_allowed` 返回批准的 `GrantKind`（或提供 `approved_delete_kind()`），
  `DeleteFile::execute` 发现 `symlink_metadata().is_symlink()` 与之不符即拒绝。

### T2 rewind 读文件的 lstat → open 窗口
- 位置：`crates/rclaw-runtime/src/turn/rewind.rs` `read_bounded` / `read_file_bytes` /
  `current_file_hash`；`helpers.rs` `open_regular_file`。
- 机制：三处先用 `symlink_metadata` 确认叶子是普通文件，再调 `open_regular_file`，而后者会
  跟随链接；窗口内把普通文件换成符号链接就会读到目标（沙箱下可能把未放行的外部内容抄进
  `state.sqlite`，或拿目标内容做 hash 比对）。H13 之前就存在，H13 没有放大。
- 建议：rewind 专用的打开变体——unix `O_NOFOLLOW`，Windows `FILE_FLAG_OPEN_REPARSE_POINT`
  + 句柄上发现 reparse 即失败。

## D. 更早轮次判定不改的设计取舍

- `FinishReason::Length` 不经共享 turn runner 上抛：验证者输出侧的两道截断闸（`OUTPUT_TRUNCATED_MARKER`
  与「未闭合 JSON 样式对象」判 `Malformed`）已覆盖同一场景，上抛需要改共享 runner 的签名。
- verdict 里的 `reason` / `reasons` 字符串不强转为数组：会放宽 verdict 形状，让更多文本被当成结论。
- 持久化的 sanitize 键与导出文件名的百分号编码（`_` 替代）：既定命名方案，没有历史数据兼容负担。
- `/compact` 在 `SummaryLoad::Completed` 之后、落库之前被取消的分支没有专门测试（逻辑简单，返回
  「压缩已取消。」）。
- `head_overflows` 用 `>=` 判断：被摘要区恰好填满预算时多一次 `warn!`，无功能影响。
- `error_indicates_context_exceeded` 的 `"token上限"` 匹配串可能误伤 `max_tokens` 参数校验错误：
  误伤的后果只是多做一次压缩重试。

## E. 范围外既有 clippy 警告（本次未动）

- `rclaw-llm`：`provider.rs` 等 3 处 `must_use`（`async_trait` 宏产物）。
- `rclaw-channels`：`chunks_exact_mut` ×2、`must_use` ×1。
- `rclaw-tools`：`mcp/transport/mod.rs:26`、`skill_entrypoint.rs:96`、`web_search/types.rs:189/196`、
  `lib.rs:113` 的 `must_use`；`web_browser.rs` 跨 `await` 持 `MutexGuard` ×7。
- `rclaw-runtime`：`turn/run.rs:479` 一处 `must_use`（`async_trait` 宏产物）。
- `rclaw-desktop`：`export.rs:133` `byte_char_slices`。

## F. 验证缺口

- 本轮只在 Windows 上验证。未在本机跑过的 unix-only 代码 / 测试：`./agent` 相对根测试、H3 的第二个
  tempdir 用例、`access.rs::deleting_a_symlink_is_scoped_to_the_link_entry`、
  `rewind.rs::capture_skips_symlink_entries_and_restore_refuses_them`、`open_regular_file` 的
  `O_NONBLOCK` 分支、`open_for_write_nofollow` 的 `O_NOFOLLOW | O_NONBLOCK` 分支、新增的
  `[target.'cfg(unix)'.dependencies] libc`。
- 仓库在基线上就不是 rustfmt 干净的（`rclaw-android`、`rclaw-gbrain` 等未改动文件也有差异），本次
  改动跟随周边风格，没有跑 `cargo fmt --all`。
