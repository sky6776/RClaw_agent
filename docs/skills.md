# 技能系统

[← 返回 README](../README.md)

技能（skill）是一份 `SKILL.md` 加可选的 `skill.toml` Manifest 与脚本。模型按需读取 `SKILL.md` 后，该技能的入口以结构化动态工具的形式变为可调用——不是让模型在 shell 里拼命令。内置技能清单见 [内置技能目录](skills-catalog.md)；技能的自进化治理见 [记忆与自进化](self-evolution.md#技能自进化)。实现位于 `crates/rclaw-tools/src/skill_entrypoint.rs`、`crates/rclaw-runtime/src/dynamic_tools.rs`、`crates/rclaw-store/src/skills.rs`。

## 技能发现与渐进披露

- 只扫描数据目录的 `agent/skills/`（仓库根 `skills/` 是安装源，见 [快速开始](getting-started.md#数据目录与技能目录)）。支持四种布局：平铺 `*.md`、`<skill>/SKILL.md`、技能包 `<包>/skills/*.md`、`<包>/skills/*/SKILL.md`。
- 索引只识别 frontmatter 的 `name`、`description`；无有效 frontmatter 时取首个标题与首段。
- **渐进披露**：系统提示每轮只注入技能索引——标题、读取路径、最多 60 字符的描述（超长为 59 字符 + 省略号）；至少 3 次结果样本后附「近 N 次：X 成 Y 败」。模型按需 `read` 对应 `SKILL.md` 才拿到完整说明并激活其工具。
- 顶层目录型技能（`skills/<name>/SKILL.md`）下若有 `EXPERIENCE.md`（热经验层，最多 4000 字符），读取 `SKILL.md` 时自动注入；包内 / 平铺技能不注入。
- **技能包**：包根的 `skill.toml` 对包内全部子技能生效（Manifest 向上查找到包根）；首次读取内置 4 个包（`ai-berkshire` / `pick-funds` / `read-macro` / `watch-positions`）的子技能时自动注入包根说明文档。

## Manifest（`skill.toml`）

```toml
manifest_version = 1

[skill]
name = "ai-berkshire"

[[entrypoints]]
name = "ashare-quote"
description = "查询 A 股实时行情。"
runtime = "python"              # python / node / platform_shell
script = "tools/ashare_data.py"
fixed_args = ["quote"]          # 固定子命令，模型不可见不可改
# windows_script = "x.ps1"      # 仅 runtime = "platform_shell"：script 为 .sh，此处必填 .ps1；python / node 入口配置它会被拒绝
# timeout_ms = 300000           # 1–7200000
# publishes_media = true        # 媒体发布入口（仅对运行时白名单内的脚本生效，见下文）
# python_venv = ".venv"         # 指定 venv

[[entrypoints.arguments]]
name = "code"
type = "string"
description = "股票代码，例如 600519。"
required = true
position = 0                    # 位置参数；或 flag = "--type" 声明具名参数

# [entrypoints.constraints]
# at_least_one = [["a", "b"]]   # 每项是一组参数名（二维数组）；还有 exactly_one / at_most_one
```

- **参数类型**（14 种）：`string` `path` `directory` `path_or_directory` `url_or_path` `string_array` `path_array` `url_or_path_array` `integer` `integer_array` `number` `number_array` `boolean` `enum`。
- **参数属性**：`name` `type` `description` `required` `position` `flag` `values`（enum 白名单）`minimum` / `maximum` `min_items` / `max_items` `repeat_flag` `must_exist`。
- **运行时**：`python` 优先用 Manifest 指定的 venv（Windows 查 `Scripts/python.exe` → `bin/python`，其它平台反序），否则从 PATH 查找；`node`；`platform_shell`。安卓 APK 注入 Chaquopy 桥，**只注册 Python 入口**，Node / shell 入口被过滤。
- **执行**：运行时直接构造 argv 启动进程，不经过 shell；默认超时 300000 ms；stdout / stderr 各默认 64 KiB 截断（`[skills] max_output_bytes` 可调），结果明确携带两路「输出截断」布尔标志。
- **中间态约定**：查询类入口（如 `check-task`）在任务仍在排队/运行时应返回退出码 0 + `{"ok": true, "pending": true, ...}`，表示「查询成功、结果尚未产出」；非零退出码用于硬失败（配置/鉴权/下载失败）、未确认完成的副作用（如 `POLL_TIMEOUT`）或中断（`0xC000013A`）。查询类入口观察到任务 `failed` 且未请求下载时仍返回 0——失败状态本身是查询结果。runtime 只按退出码判定工具成败、不解析 `pending` 字段，因此 pending 结果不会进入完成闸门的未解决错误账本。

## 动态工具与激活生命周期

- 工具名 `skill__<skill>__<entrypoint>`（连字符转下划线）。
- 每个 turn 开始时动态工具集为空；模型成功读取已登记入口的 `SKILL.md` 后，该技能的入口激活为可调用工具。
- 失效：`skill_manage` 修改任何技能会使本 turn 全部动态工具失效；Manifest / 脚本重校验失败或名称冲突也会全量失效；重读时某技能的 `skill.toml` 消失只失效该技能的旧工具；下一 turn 重新置空。
- **legacy shell 拦截**：技能的 Manifest 工具激活后，在 `shell` 里直接运行该技能脚本会被拒绝并回 `SKILL_ENTRYPOINT_REQUIRED`。
- 这套生命周期由 `DynamicToolSet` 单一所有者管理（术语见 [`CONTEXT.md`](../CONTEXT.md)）。安卓 APK 另注入脚本变更钩子：技能 Python 脚本任何改动后需在配置页 SkillOpsCard 重新审批哈希才能执行。

## 设计动机

这套设计针对 LLM 工具调用的真实失败模式：

1. **防幻觉传错参数**——`enum` 参数只能取清单内的值、类型不符在调用前直接拒绝，错误信息自描述（缺什么、合法值是什么），模型能在同一轮自行纠正。脚本路径与子命令由 `script` + `fixed_args` 固定，模型接触不到「调哪个脚本」这个自由度。
2. **防命令注入**——参数以 argv 数组直达进程，不存在 shell 解释层。
3. **防路径逃逸**——`path` 类参数按工作区解析相对路径，拒绝越界绝对路径与 `../`；`must_exist` 在启动前校验。
4. **防 Key 泄漏**——Key 集中在共享 TOML 配置（见 [API Key 清单](getting-started.md#api-key-配置清单)），动态工具不开放配置路径参数；Key 不出现在 `SKILL.md`、命令行、输出与日志里。
5. **输出有界且自描述**——64 KiB 截断 + 截断标志；规范要求大产物落文件、stdout 只回摘要 + 路径，模型经 `read` 分页取。
6. **媒体交付可信**——只有运行时硬编码白名单内的脚本（`glm-master-skill/scripts/publish_media.py`，以及 `evaluate-fund` 与 `glmv-stock-analyst` 的 `md2html.py` / `export_report.py`）被视为可信媒体发布入口；其他技能即使声明 `publishes_media = true` 也不会被认领。发布回执经媒体认领闸门验证，未验证的标记让 turn 无法宣称完成。
7. **工具面按需出现**——未激活的技能工具不在 schema 里，几十个技能不会撑爆系统提示与工具 schema，也降低误调用。

## 查看与管理

- `/skills [list]`：列出全部可发现入口及所属包 / 功能；子命令 `help`、`status [name]`、`pin` / `unpin`（钉住保护）、`adopt`（把技能认领为 agent-owned）、`restore <name>`（从 `.archive/` 恢复）、`curate`（手动触发策展）。变更类子命令仅限本机 / 已授权会话，且有 turn 或 review 运行时拒绝。
- `skills_list` 工具：只返回可由 `skill_manage` 寻址的顶层 `skills/<name>/SKILL.md`——名称、标题、完整描述、路径、owner / state / pinned 与可选可靠率；不列包内子技能。
- `skill_manage` 工具：`create` `patch` `edit` `delete` `write_file` `remove_file` `archive` `restore` `rollback`（后三者仅前台可用）。约束：名称 ≤ 64 字符、小写字母 / 数字开头、仅 `a-z 0-9 - _ .`；`SKILL.md` 与支撑文件均 ≤ 100000 字符（支撑文件另有 1 MiB 字节上限，实际先触发前者）；根目录只允许 `SKILL.md` / `skill.toml` / `EXPERIENCE.md`，其余放 `references/` `templates/` `scripts/` `assets/`；每次模型响应只允许一个 mutation，且同一批次不得与 `read` 并存。写入拒绝隐形 / 双向控制字符并自动脱敏密钥。
  - **前台**（用户在环）与**后台 review** 的约束不同：read-before-write（覆盖前必须本轮先读且摘要未变化）只在后台生效；`.protected` / pinned 技能后台一切写皆拒，前台仅禁止 `delete`；前台 `delete` 永久删除，后台 `delete` 必须带 `absorbed_into` 且只归档；后台只能改 agent-owned 技能。完整治理见 [记忆与自进化](self-evolution.md#技能自进化)。
- 每次改动写 `.evolution.log` 与 `.<文件>.patch.bak`，`rollback` 依赖 `skills/.ledger/ledger.jsonl` 的 before 快照；`.usage.json` 记录 use / view / patch 与成败遥测，派生可靠率与 `SkillState`。
- 图形界面「技能」页可搜索、启停（禁用 = 归档）；`skills/*/config.toml` 与 `skills/*_key.toml` 在安卓「配置」页的 SkillOpsCard 编辑。

## 所有权

`bundled_skills.json`（内嵌于二进制，判定以内嵌副本为准；运行时另把它落盘为 `agent/skills/.bundled_manifest.json` 供检视）列出全部 40 个仓库技能 / 技能包名（包按包目录名登记），命中者为 repository-owned，不可被 `/skills adopt`。用户放入的为 user-owned；后台 review 自建或经 `/skills adopt` 认领的为 agent-owned（对话中由模型调用 `skill_manage create` 创建的仍是 user-owned）。只有 agent-owned 技能会被后台 review 修改与策展器归档。

## 动态工作流技能

`dynamic-workflows` 是运行时安装的特殊技能：模型完整读取后按会话登记文件 SHA-256 与契约版本，文件变化即失效。它以确定性 QuickJS 脚本封装子代理、文件、只读 Git、命令、artifact 与人工填洞，见 [多智能体协作](multi-agent.md#动态工作流)。
