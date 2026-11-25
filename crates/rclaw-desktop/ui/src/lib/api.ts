/**
 * Tauri command / event 的类型化封装：前端与核心通信的唯一入口。
 *
 * 纪律：任何组件不得绕过本模块直接触碰核心——后端命令面刻意保持最小
 * （对话走 inbound 总线，配置复用 slash 管道），见 crates/rclaw-desktop/src/commands.rs。
 */
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export interface Status {
  wizard_needed: boolean;
  agent_ready: boolean;
  provider: string | null;
  model: string | null;
  home: string;
  /** 最近一次 agent 启动失败的错误文案（轮询兜底用；null = 无失败记录）。 */
  agent_error: string | null;
}

export interface BusPayload {
  channel: string;
  chat_id: string;
  content: string;
  /** 中间过程消息（工具轮 text / 思考流式增量）：不解除忙碌态，思考块流式合并。 */
  intermediate: boolean;
}

export interface AgentEvent {
  state: "starting" | "ready" | "error";
  error?: string;
}

export interface CronEntry {
  id: string;
  name: string;
  enabled: boolean;
  kind: string;
  schedule: string;
  message: string;
}

export interface ChatMeta {
  chat_id: string;
  title: string;
  updated_at: number;
  archived: boolean;
}

/** 关于页的模块版本信息（对应后端 app_versions 命令）。 */
export interface AppVersions {
  /** 应用版本（与安卓 versionName 同源）。 */
  app: string;
  agent_runtime: string;
  agent_core: string;
  agent_llm: string;
  agent_tools: string;
  agent_store: string;
  agent_cli: string;
  /** 通道接入（仅 APK 宿主链接时有值）。 */
  agent_channels?: string;
  tauri: string;
  rustc: string;
  git: string;
  /** 构建时间（UTC epoch 秒）。 */
  build_time: number;
}

export interface HistoryMsg {
  /** 消息行 id（state.sqlite 的 message.id；消息「分叉到新会话」的锚点）。 */
  id: number;
  /** thinking = assistant 行内 Reasoning 块的投影（重启后思考块恢复的通道）。 */
  role: "user" | "assistant" | "thinking" | "status";
  content: string;
  /** 消息落库时间（unix 秒，与 state.sqlite 的 message.ts 同单位）；
   *  映射到 Entry.ts（毫秒）时需 ×1000。 */
  ts: number;
  /** status 条目（工具调用轮）的结构化调用列表（名称 + 列表级入参 + 侧表事实）。 */
  tool_calls?: ToolCallSummary[];
}

/** 列表级工具调用（快照 / 增量 / 直播同形：入参按运行时规则裁剪，长字符串截断）。 */
export interface ToolCallSummary {
  /** ToolUse.id（同一批内唯一；跨批可重复）。 */
  id: string;
  name: string;
  input: unknown;
  input_truncated: boolean;
  /** Skill 归属（Manifest 专用工具所属 skill / read 激活的 skill）。 */
  skill?: string | null;
  duration_ms?: number | null;
  /** 结果成败；缺省 = 侧表无记录（旧行 / 侧表写入失败）。 */
  is_error?: boolean | null;
  /** turn 被取消、该调用没有执行完（链以 aborted 占位结果闭合）。 */
  aborted?: boolean;
}

/** 运行时 `tool_calls` 事件：一批工具调用开始 / 结束（直播通道；历史由库回放）。 */
export type ToolCallsEvent =
  | {
      phase: "begun";
      chat_id: string;
      batch: {
        /** 从 1 起的 ReAct 迭代号。 */
        iter: number;
        calls: Array<{
          id: string;
          name: string;
          input: unknown;
          input_truncated: boolean;
          skill: string | null;
        }>;
      };
    }
  | {
      phase: "finished";
      chat_id: string;
      batch: {
        iter: number;
        /** 已落库的 assistant(tool_calls) 行 id（cron 等不落库路径为 null）。 */
        assistant_msg_id: number | null;
        results: Array<{
          id: string;
          is_error: boolean;
          /** 被 /cancel 打断、没有结果（取消清理闭合中断链后补发的结束事件）。 */
          aborted: boolean;
          /** 结果正文预览（已截断时 truncated=true，全文走 chatToolDetail）。 */
          preview: string;
          truncated: boolean;
          duration_ms: number | null;
          skill: string | null;
        }>;
      };
    };

/** 展开面板详情：一条工具调用行的完整入参 + 结果 + 写入前快照。 */
export interface ToolChainDetail {
  msg_id: number;
  /** 行落库时间（unix 秒）。 */
  ts: number;
  calls: ToolCallDetail[];
}

export interface ToolCallDetail {
  id: string;
  name: string;
  input: unknown;
  skill?: string;
  started_ms?: number;
  duration_ms?: number;
  result?: { content: string; is_error: boolean };
  /** write / edit / delete 的写入前文件快照（未记录 checkpoint 时缺省）。 */
  checkpoint?: {
    path: string;
    /** 写入前内容；null = 写入前不存在（新建）。 */
    before: string | null;
    /** 写入后文件不存在（delete）。 */
    deleted: boolean;
  };
}

/** 行级快照：尾部条目 + 消费游标（之后用 chatHistorySince 按游标续读）。 */
export interface ChatHistorySnapshot {
  entries: HistoryMsg[];
  /** 读取时会话的最大消息行 id（含被过滤行——它们被游标消费、不再投递）。 */
  cursor: number;
  /** 首屏窗口（可能）裁掉了更早的行：entries 的首条不一定是会话开头。 */
  truncated?: boolean;
}

/** 行级增量：游标之后新落库的条目 + 新游标；more = 按条数截断，应立即续拉。 */
export interface ChatHistoryDelta {
  entries: HistoryMsg[];
  cursor: number;
  more: boolean;
}

/** chat_fork 结果：新会话 id、复制的历史条数与文件回滚结果。 */
export interface ChatForkResult {
  chat_id: string;
  copied: number;
  /** 撤销的文件写入条数（0 = 锚点之后没有 checkpointed 写入）。 */
  restored_files: number;
  /** 执行了文件回滚时 = 明细报告（每文件一行）；回滚失败时 = ⚠️ 警告文案；null = 未执行回滚。 */
  restore_report: string | null;
  /** 文件回滚是否失败（分叉本身已成功，历史不回退）。 */
  restore_failed: boolean;
}

/** PlanRun 步骤状态（与后端 PlanStepDto.status 同集合）。 */
export type PlanStepStatus =
  | "pending"
  | "running"
  | "verifying"
  | "succeeded"
  | "failed"
  | "skipped";

/** PlanRun 计划的一个步骤状态（plan_status 轮询）。 */
export interface PlanStep {
  id: string;
  /** 后端投影的状态；未知值（更新的后端）按 pending 渲染。`string & {}` 让联合不被
   *  坍缩成 string，已知字面量仍参与补全与穷举检查。 */
  status: PlanStepStatus | (string & {});
  task: string;
  summary: string;
}

/** PlanRun 计划的状态投影。 */
export interface PlanStatus {
  plan_id: string;
  name: string;
  /** 已整体收尾（plan_completed / plan_aborted，或执行者已不在）。 */
  completed: boolean;
  /** 执行器未正常收尾（取消 / 早退 / 进程重启）；completed 同时为 true。 */
  aborted: boolean;
  /** 用户暂停中（波次边界协作式暂停：当前波跑完后等待恢复）。 */
  paused: boolean;
  succeeded: number | null;
  updated_at: number;
  steps: PlanStep[];
}

/** 工作流 run 摘要（workflow_status 轮询）。 */
export interface WorkflowRunSummary {
  run_id: string;
  label: string;
  status: string;
  stop_reason?: string | null;
  resumed_from?: string | null;
  superseded_by?: string | null;
  owned_by_this_session?: boolean;
  possibly_interrupted?: boolean;
  legacy?: boolean;
  resumable?: boolean;
  created_at: number;
  updated_at: number;
  spent_tokens: number;
}

/** 工作流详情（workflow_detail / GetWorkflowRun 同形）。 */
export interface WorkflowDetail {
  run_id: string;
  label: string;
  status: string;
  stop_reason?: string | null;
  exec_generation?: number;
  progress?: { asks_done: number; asks_total: number; active_agents: number };
  log_tail?: Array<{ message: string }>;
  pending_questions?: Array<{ question_id: string; actor: string; question: string }>;
  holes?: Array<{ hole_id: string; name: string; prompt?: string; state: string; script_generation: number }>;
  phases?: Array<{
    name: string;
    status: string;
    started_at: number;
    finished_at: number;
    duration_ms?: number;
  }>;
  reports?: Array<{ seq: number; item_preview: unknown }>;
  artifacts?: Array<{ id: string; kind: string; title: string; version: number; primary: boolean; uri: string }>;
  effects?: Array<{ effect_key: string; op: string; is_write: boolean; status: string; args_preview?: string; decision?: string | null }>;
  result?: unknown;
  /** result 超过 48k 字符时只回句柄：全文落盘路径 + 头尾预览。 */
  result_handle?: { path?: string | null; bytes: number; chars: number; preview: string; hint: string } | null;
  /** 分页游标（null = 没有更多）。 */
  reports_cursor?: number | null;
  effects_cursor?: number | null;
  artifacts_cursor?: number | null;
  log_cursor?: number | null;
  error?: unknown;
  recovery?: string | null;
  notifications?: Array<{ id: string; state: string; attempts: number; last_error?: string | null }>;
  /** 当前 exec 的 world.run 进程树记录；`stopping` 下 running/unknown 的要人工确认已退出。 */
  processes?: Array<{ process_id: string; root_pid: number; state: string }>;
}

export interface WizardInput {
  provider: string;
  baseUrl: string;
  model: string;
  apiKey: string;
  contextLength: number;
  autoEvolve: boolean;
}

/** 子 agent 累计 token 用量（provider 上报的真实 usage 逐次模型调用累加）。 */
export interface AgentExecutionUsage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
  /** 已拿到结果的模型调用次数。 */
  model_calls: number;
  /** 其中 provider 没上报 usage 的调用次数（>0 时 token 数只是下限）。 */
  unreported_calls: number;
}

/** 后台子 agent 执行的列表级快照（executions_payload 的 `executions[]` 条目；
 *  `agent_exec` 的 snapshot 事件同形）。 */
export interface AgentExecution {
  agent_id: string;
  /** 子代理名字（Agent 工具的 name 入参；可为空）。 */
  name?: string;
  role?: string;
  state: string;
  progress: { completed_steps: number; total_steps: number; stage: string };
  /** 任务文本（截断到 240 字符；全文在详情 `execution.spec.task`）。 */
  goal: string;
  isolation?: string;
  model?: string | null;
  team_name?: string | null;
  /** 发起会话（通道 + chat_id）。 */
  source_channel?: string;
  origin_chat_id?: string;
  created_at: string;
  updated_at: string;
  /** 本次运行开跑时刻（RFC3339）；排队中 / 恢复待续为 null。 */
  started_at?: string | null;
  /** 终态时刻（RFC3339）；非终态为 null。 */
  finished_at?: string | null;
  usage?: AgentExecutionUsage;
  /** 进程重启后恢复的记录（没有执行者，需 AgentResume）。 */
  recovered?: boolean;
  /** 本进程正在跑（可取消 / 可插话）。 */
  live?: boolean;
  /** 失败 / 取消原因（截断到 400 字符）。 */
  error?: string | null;
  has_handoff?: boolean;
  /** 已发送、子代理尚未读到的插话条数。 */
  inbox_pending?: number;
}

/** 子 agent 结构化交接（handoff）。 */
export interface AgentHandoff {
  goal: string;
  summary: string;
  evidence: string[];
  artifacts: string[];
  open_risks: string[];
  validation: string[];
}

/** `agent_execution_detail` 的返回：完整快照 + 尾部事件 + 控制位。 */
export interface AgentExecutionDetail {
  execution: {
    agent_id: string;
    spec: {
      name: string;
      role: string;
      task: string;
      context: string;
      team_name?: string | null;
      subagent_type?: string | null;
      source_channel?: string;
      origin_chat_id?: string;
      model?: string | null;
      isolation: string;
      max_tool_iter: number;
      max_output_chars: number;
    };
    state: string;
    progress: { completed_steps: number; total_steps: number; stage: string };
    handoff: AgentHandoff | null;
    error: string | null;
    unresolved_disclosure?: string;
    created_at: string;
    updated_at: string;
    started_at?: string | null;
    finished_at?: string | null;
    usage?: AgentExecutionUsage;
    recovered: boolean;
  };
  events: Array<{
    event: string;
    timestamp: string;
    state: string;
    progress: { completed_steps: number; total_steps: number; stage: string };
  }>;
  cancellable: boolean;
  resumable: boolean;
  /** 已发送、子代理尚未读到的插话条数。 */
  inbox_pending: number;
  summary: AgentExecution;
}

/** 运行时 `agent_exec` 直播事件（执行登记表变更 / 在跑的工具批 / 活动记录新行）。
 *  `resync` 由宿主在广播滞后时发出：前端应重拉全量对齐。 */
export type AgentExecEvent =
  | { kind: "snapshot"; execution: AgentExecution }
  | { kind: "removed"; agent_ids: string[] }
  | {
      kind: "tools_begun";
      agent_id: string;
      batch: Extract<ToolCallsEvent, { phase: "begun" }>["batch"];
    }
  | { kind: "transcript"; agent_id: string; cursor: number }
  | { kind: "resync" };

export interface ToolEntry {
  name: string;
  description: string;
  enabled: boolean;
}

export interface SkillEntry {
  name: string;
  description?: string;
  state?: string;
  enabled?: boolean;
  pinned?: boolean;
  /** 这个技能是从哪台设备同步来的（设备的名字）；本机自己的技能没有这一项。 */
  sync_origin?: string;
  [key: string]: unknown;
}

export interface EvolutionInfo {
  auto_memory: boolean;
  auto_evolve: boolean;
  skill_curator: boolean;
  session_compression: boolean;
  memory_md: string | null;
  user_md: string | null;
  skills_dir: string;
}

export interface DayActivity {
  date: string;
  user_msgs: number;
  assistant_msgs: number;
}

export interface NameCount {
  name: string;
  count: number;
}

export interface SkillUsageDto {
  name: string;
  use_count: number;
  view_count: number;
  patch_count: number;
  state: string;
}

export interface LedgerLineDto {
  id: string;
  ts: string;
  action: string;
  skill: string;
  actor: string;
  undo_of?: string;
}

export interface Insights {
  total_chats: number;
  archived_chats: number;
  total_messages: number;
  days: DayActivity[];
  top_tools: NameCount[];
  top_skills: SkillUsageDto[];
  memory_used: number;
  memory_limit: number;
  recent_mutations: LedgerLineDto[];
}

export interface ConfigItem {
  key: string;
  path: string;
  doc: string[];
  default_raw: string;
  kind: "bool" | "int" | "float" | "str" | "str_array";
  commented: boolean;
  secret: boolean;
  /** 枚举型键的合法取值（非空 → 下拉框；空 → 自由输入）。 */
  options: string[];
  current: string | null;
}

export interface ConfigSection {
  header: string;
  path: string;
  title: string;
  doc: string[];
  example_only: boolean;
  items: ConfigItem[];
}

export interface ConfigDocDto {
  preamble: string[];
  sections: ConfigSection[];
}

export interface ConfigStateDto {
  path: string;
  raw: string;
  doc: ConfigDocDto;
}

export interface ConfigEdit {
  path: string;
  kind: string;
  value: string;
}

// ---- SOUL 人格库（配置页「人格（SOUL）」）----

export interface SoulPersonaDto {
  /** 内置人格 id；导入文件为文件名。 */
  id: string;
  label: string;
  file_name: string;
  builtin: boolean;
  /** 是否当前生效（后端按内容比对）。 */
  active: boolean;
  bytes: number;
}

export interface SoulStateDto {
  personas: SoulPersonaDto[];
  /** 生效人格是否被用户编辑过（不等于库内任何文件）。 */
  active_edited: boolean;
  active_path: string;
  souls_dir: string;
  /** [agent] soul_enabled：false 时人格不会被注入提示词（文件仍会写入）。 */
  soul_enabled: boolean;
  /** 生效人格有内容但库内多份同内容且无选择标记——无法唯一确定「当前」。 */
  active_ambiguous: boolean;
  /** 生效人格缺失/为空/仍是占位（还没播种或被清空）。 */
  active_empty: boolean;
}

export interface SoulApplyDto {
  status: "applied" | "needs_confirm" | "already_active" | "not_found";
  persona: string | null;
  /** 覆盖前的备份路径（仅旧人格被编辑过并覆盖时）。 */
  backup: string | null;
}

export interface SoulImportResultDto {
  file_name: string;
  saved_as: string | null;
  error: string | null;
}

// ---- wxbot 联系人查询（配置页「选择群 / 选择联系人」）----
// 字段 camelCase 与 wcx 域语言对齐（wxId 等），与配置面板其余 DTO 的 snake_case 无关。

export interface WxbotContact {
  wxId: string;
  nickname: string;
  customWxId: string;
  remarkName: string;
}

export interface WxbotRecentChat {
  /** 记录归属的微信账号；带账号过滤查询时不返回 legacy 空账号行，无过滤时空串=账号未知 */
  account: string;
  /** 纯会话 id：私聊 wxid / 群 xxx@chatroom */
  convId: string;
  isGroup: boolean;
  msgCount: number;
  /** 最近消息时间（Unix 秒；0 = 未知） */
  lastTs: number;
}

export interface WxbotContactsErrorDto {
  code: string;
  message: string;
}

/** invoke 拒绝值 → 结构化错误（后端约定 {code,message}；其余兜底 unknown）。 */
export function wxbotContactsError(e: unknown): WxbotContactsErrorDto {
  if (e && typeof e === "object" && "code" in e && "message" in e) {
    return e as WxbotContactsErrorDto;
  }
  return { code: "unknown", message: String(e) };
}

export interface SloganPrefs {
  slogans: string[];
  interval_secs: number;
}

/** 文件上传结果：rel_path 为 agent 工作区相对路径（对话里引用），abs_path 供展示。 */
export interface UploadResult {
  rel_path: string;
  abs_path: string;
  size: number;
}

/** 工作区外访问授权请求（permission_request 事件载荷）。 */
export interface PermissionRequest {
  id: string;
  /** 弹窗正文：操作 + 真正会被打开的物理落点（请求拼写与落点不同时一并带上）。 */
  path: string;
  /** 物理落点（正斜杠）。 */
  target: string;
  /** 工具请求时的拼写（正斜杠）。 */
  requested: string;
  /** 操作名（读取 / 写入 / 删除 / 列目录 / 搜索 / 作为 shell 工作目录）。 */
  op: string;
}

/** 工具权限模式：default = 工作区外需确认；full = 完全访问免确认。 */
export type PermissionMode = "default" | "full";

/** 上下文窗口 + 进程内存快照（usage_snapshot，右侧任务面板状态区）。 */
export interface UsageSnapshot {
  history_used: number;
  request_used: number;
  context_length: number;
  rss_bytes: number;
}

/** Bot Mode：bot 定义（花名册与编辑器共用）。 */
export interface BotEntry {
  name: string;
  title: string;
  description: string;
  persona: string;
  provider: string | null;
  denied_tools: string[];
}

/** Bot Mode：群聊房间一行消息。 */
export interface GroupLine {
  seq: number;
  who: string;
  text: string;
  ts: number;
}

/** Bot Mode：群聊活动流一条记录。 */
export interface GroupActivity {
  ts: number;
  kind: string;
  who: string;
  detail?: string | null;
}

/** Bot Mode：群聊房间（列表为轻量版，lines/activities 为空）。 */
export interface GroupRoom {
  id: string;
  name: string;
  members: string[];
  running: boolean;
  created_ts: number;
  lines: GroupLine[];
  activities: GroupActivity[];
}

/** 检查更新结果（update_check command）。 */
export interface PendingShare {
  files: string[];
  text?: string;
}

export interface SkillApprovalItem {
  script: string;
  has_baseline: boolean;
}

export interface SkillConfigEntry {
  path: string;
  size: number;
}

export interface MediaPayload {
  mime: string;
  base64: string;
  /** 数据是不是「按对象核对过的字节」。核不了时按老行为读路径出图，这里是 false：
   *  调用方不能把「读到了」当成「核对通过」（别撤掉「内容对不上」的标记）。 */
  verified: boolean;
}

export interface UpdateInfo {
  current: string;
  latest: string | null;
  url: string | null;
  notes: string | null;
  error: string | null;
}

/** 后台保活状态（keep_alive_status）。 */
export interface KeepAliveStatus {
  /** 是否安卓宿主；桌面恒 false（其余字段为占位值，卡片不渲染）。 */
  supported: boolean;
  /** 是否已加入电池优化白名单（Doze 期间网络/唤醒锁才真正生效）。 */
  battery_whitelisted: boolean;
  /** Build.MANUFACTURER 原值。 */
  manufacturer: string;
  /** 本机是否有可达的厂商自启动/后台管理页（否则按钮退化为「应用详情」）。 */
  oem_settings: boolean;
}

/** 应用内更新进度（update_progress 事件载荷）。 */
export interface UpdateProgress {
  phase: "download" | "install";
  received: number;
  total: number;
}

// ---------------------------------------------------------------------------
// 多端同步（docs/multi-device-sync.md）。字段名与后端一致（snake_case）。
//
// 配对码等同于全部同步数据的钥匙：只在「多端同步」面板里显示和输入，不写进控制台、
// 提示条、草稿和 localStorage。
// ---------------------------------------------------------------------------

/** `rclaw.toml` 的 `[sync]` 段。存储的密码不在这里（只存在本机，见 syncSetSecret）。 */
export interface SyncConfig {
  enabled: boolean;
  /** 展示用的设备名；留空取主机名。 */
  device_name: string;
  /** 存储类型：webdav / file（共享文件夹）/ s3。 */
  transport: string;
  /** 同步范围：sessions / memory / persona / attachments / skills / bots / cron。 */
  scopes: string[];
  channels: string[];
  /** 前台轮询间隔（秒，≥ 5）。 */
  poll_interval_sec: number;
  /** 后台轮询间隔（秒，≥ 60）。 */
  background_interval_sec: number;
  /** 发送前先拉取最多等多久（毫秒）；0 = 不等。 */
  presend_pull_wait_ms: number;
  /** 单个附件的大小上限（MB）。 */
  attachment_max_mb: number;
  /** 回收站保留天数；0 = 不留。 */
  trash_days: number;
  webdav: {
    url: string;
    username: string;
    /** 允许明文 HTTP（只对局域网地址有效）。 */
    allow_http: boolean;
    /** 信任的自签证书指纹（SHA-256）；空 = 信任系统与内置公共 CA。 */
    pinned_cert_sha256: string;
  };
  file: { dir: string };
  s3: {
    endpoint: string;
    region: string;
    bucket: string;
    prefix: string;
    access_key_id: string;
    path_style: boolean;
    /** 阿里云 OSS：用 x-oss-forbid-overwrite 代替 If-None-Match（OSS 会拒绝条件头）。
     * 老后端没有这个字段：读侧按 `?? false` 处理，写侧只在勾选时带上。 */
    oss_forbid_overwrite?: boolean;
    allow_http: boolean;
  };
}

/** 同步空间里的一台设备。 */
export interface SyncDevice {
  id: string;
  name: string;
  platform: string;
  version: string;
  /** 是不是本机。 */
  me: boolean;
  /** 最近一次看到它有新内容的时刻（Unix 毫秒；0 = 还没见过）。 */
  seen_ms: number;
  /** 超过 60 天没出现。 */
  stale: boolean;
  /** 被标成了「已丢失」（它的定时任务可以被强制接管）。 */
  lost: boolean;
  /** 它的日志里已经取回、还没处理完的条数。 */
  behind: number;
}

/** 一条待处理事项。 */
export interface SyncPendingItem {
  id: string;
  /** persona / skill / bot（等本机确认的改动）、quarantine、upgrade、peer-error、
   *  pub-halt、mem-hold、mem-wipe（记忆文件里原有的条目都不见了：accept = 是有意删的，
   *  把删除同步到所有设备；reject = 恢复到本机），其余是只读的提示。 */
  kind: string;
  title: string;
  detail: string;
  /** 来自哪台设备（展示名）。 */
  origin?: string;
  at_ms: number;
  /** 可以做的操作：accept / reject / retry / discard / dismiss。 */
  actions: string[];
  /** 类别自己的数据。等确认的改动（persona / skill / bot）带着 `hlc`：这是哪一版；
   *  处理时要原样交回（syncResolve 的 reviewed）。 */
  payload: unknown;
}

/** 同步的整体状态。 */
export interface SyncStatus {
  /** off（没开启）、setup（开启了，还没创建或加入空间）、idle、syncing、paused、
   *  offline（连不上存储）、error、confirm（计费网络下的首次同步在等确认）、
   *  standby（另一个 RClaw 进程正在用同一个数据目录同步）。 */
  state: string;
  message: string | null;
  transport: string;
  /** 本机存着当前这种存储的密码。 */
  secret_saved: boolean;
  space_id: string | null;
  device_id: string | null;
  device_name: string;
  /** 实际生效的同步范围。 */
  scopes: string[];
  last_sync_ms: number;
  /** 还没传出去的改动数。 */
  outbox: number;
  /** 取回了还没合入的改动数。 */
  inbox: number;
  /** 等着上传的附件数。 */
  uploads: number;
  /** 存量附件里还没上传的总大小（字节）；> 0 时要用户选上传范围。 */
  backlog_bytes: number;
  devices: SyncDevice[];
  pending: SyncPendingItem[];
}

/** 面板的全部状态：引擎状态 + 当前生效的设置。 */
export interface SyncState {
  status: SyncStatus;
  config: SyncConfig;
}

/** 「测试连接」里的一步。 */
export interface SyncSelfTestStep {
  name: string;
  ok: boolean;
  /** 没通过的原因；通过时为空。 */
  detail: string;
}

/** 配对码里带了什么（加入之前给用户核对）。不含密钥与密码。 */
export interface SyncPairingInfo {
  /** 配对码自带存储的地址与凭据：加入时不用再填。 */
  has_storage: boolean;
  transport: string | null;
  /** 自带的存储地址：数据会传到这里。 */
  endpoint: string | null;
  /** 加入之前要先填存储的密码。 */
  needs_secret: boolean;
  /** 自带的是阿里云 OSS 模式：加入页提示「桶不能开启过版本控制」。 */
  oss_forbid_overwrite: boolean;
}

/** 「添加设备」要显示的配对码。 */
export interface SyncPairingCode {
  code: string;
  /** 二维码（PNG 的 data URL）；配对码太长画不出来时为空。 */
  qr: string | null;
  qr_error: string | null;
}

/** 「同步已有附件」的范围。 */
export type SyncBacklog = "all" | "none" | { days: number };

/** 会话列表的角标。只列出至少有一项不是默认值的会话。 */
export interface SyncSessionMark {
  channel: string;
  chat_id: string;
  /** 标成了「仅本机」。 */
  local_only: boolean;
  /** 参与同步：别的设备上有它（或马上会有）。 */
  synced: boolean;
  /** 已经传出去过：在本机删掉它，删除会传到其他设备，各端先在回收站里留一份。还在排队、
   *  一次都没传出去过的不算——删它只是本机的事，也不进回收站。 */
  published: boolean;
  /** 并发续写时自动分出来的分支：造成分叉的那台设备的名字。 */
  branch_of: string | null;
  /** 会话键在本机被占用、换了一个键落地：来源设备的名字。 */
  landed_from: string | null;
  /** 正在这些设备上运行。 */
  running_on: string[];
}

/** 一条消息引用的一个附件在本机的情况。 */
export interface SyncAttachment {
  /** 消息行 id（= Entry.msgId）。 */
  message_id: number;
  /** file：被消息引用的工作区文件；artifact：工具结果的全文。 */
  kind: string;
  /** 消息里写的相对路径；artifact 是文件名。 */
  path: string;
  blob: string;
  size: number;
  /** local（本机有，在原位置）、cached（取回的文件快照）、remote（还在别的设备上）、
   *  missing（本机登记过，文件已经不在）、changed（落点上有个同名文件，内容已经不是
   *  这个对象）。去取过、同步空间里却没有的：asked（向别的设备要了，在等）、gone
   *  （保存着它的设备说拿不出来了）、lost（等了很久没有设备传上来）——后两种不再自动
   *  去取，点「重试」才再去看一次。 */
  state: string;
  /** 本机上这个文件的绝对路径。只给需要经引擎核对登记的后端入口用；界面打开文件
   *  要走 rel_path。 */
  local_path: string | null;
  /** local 状态下的工作区相对路径（正斜杠）。open_media 只收这种路径；cached 的
   *  副本在同步缓存里，只能经「存进工作区」取用。 */
  rel_path: string | null;
  /** 产生这条消息的设备名；本机产生的为空。 */
  origin: string | null;
  /** 消息里写的那个路径在本机工作区里是否真的原样存在（同一对象的别名补到原位置
   *  之后就是 true）。false 时界面按附件卡给取回入口。 */
  path_present: boolean;
  /** path_present / changed 是不是内容核对出来的。false：这次清单没轮到它（大文件、
   *  老消息），界面别当命中；点开前后端还有一次按需核对。 */
  checked: boolean;
}

/** 回收站里的一个会话。 */
export interface SyncTrashItem {
  id: string;
  channel: string;
  chat_id: string;
  title: string | null;
  deleted_at_ms: number;
  messages: number;
  size: number;
}

/** 回收站里的一份记忆条目：因为同步而从这台设备的记忆文件里删掉的（别的设备删的，或者
 *  /clear confirm all 清空的）。 */
export interface SyncTrashMemory {
  id: string;
  /** 一句话说明：哪个文件、因为什么。 */
  what: string;
  /** 条目原来在哪份记忆里（memory / user / bot:<名字>:memory……）。没记下来时为 null：
   *  只能查看，不能直接放回。 */
  target: string | null;
  deleted_at_ms: number;
  entries: string[];
}

/** 回收站里的全部内容。 */
export interface SyncTrashContents {
  sessions: SyncTrashItem[];
  memory: SyncTrashMemory[];
}

/** 把回收站里的一份记忆条目放回去的结果。 */
export interface SyncMemoryRestored {
  /** 放回了哪份记忆（给人看的称呼）。 */
  into: string;
  /** 现在在文件里的条数。 */
  restored: number;
  /** 没放回去的条目：[条目开头, 原因]。有的话副本还留在回收站里。 */
  rejected: Array<[string, string]>;
}

/** 导入 / 恢复出来的会话。 */
export interface SyncImported {
  channel: string;
  chat_id: string;
  messages: number;
}

/** 别的设备上的一个定时任务（只读）。 */
export interface SyncRemoteCron {
  owner: string;
  owner_name: string;
  id: string;
  name: string;
  enabled: boolean;
  /** 人读的调度描述；本机版本不认识这种调度方式时为空。 */
  schedule: string;
  message: string;
  /** 这份定义的版本。接管时原样交回（syncCronTakeover 的 reviewed）：看到的和要接管的
   *  得是同一版。 */
  version: string;
  /** 不能移到本机执行的原因；空 = 可以。 */
  blocked: string | null;
  /** 本机已经请求接手，在等那台设备确认。 */
  pending: boolean;
  /** 那台设备很久没出现、或被标为已丢失：可以不等它确认，强制接管。 */
  can_force: boolean;
}

/** 拍照扫码的进展。 */
export interface SyncScan {
  /** waiting（相机还没回来）、cancelled、done（见 code）、failed（见 error）。 */
  state: string;
  code: string | null;
  error: string | null;
  /** 照片没能删掉时的提醒。 */
  warning: string | null;
}

/** 开始一次拍照扫码：requestId 贯穿本轮的开始、轮询与取消。 */
export interface SyncScanStart {
  request_id: string;
  /** ok / permission_requested。 */
  status: string;
}

/** `sync` 事件：只是「该重新取数了」的提示，内容以重新查询为准。
 *  attachment：一个附件的取回有了着落。error 为空不一定是取到了——同步空间里没有这个
 *  文件时也是空的，它现在的情况看 SyncAttachment.state；不为空是这一次没取成的原因。 */
export type SyncEvent =
  | { kind: "status" }
  | { kind: "sessions"; chats: Array<{ channel: string; chat_id: string }>; list: boolean }
  | {
      kind: "attachment";
      blob: string;
      error: string | null;
      /** 这次真的把对象落到了本机（存储缺件/取不回来也是空 error，但没落盘）。 */
      placed: boolean;
      /** 实际落到的相对路径（`placed` 为真时）：界面只清这个路径的「内容对不上」标记。 */
      landed: string | null;
    }
  | { kind: "reload"; what: string };

export const api = {
  getStatus: () => invoke<Status>("get_status"),

  wizardSave: (input: WizardInput) =>
    invoke<Status>("wizard_save", {
      provider: input.provider,
      baseUrl: input.baseUrl,
      model: input.model,
      apiKey: input.apiKey,
      contextLength: input.contextLength,
      autoEvolve: input.autoEvolve,
    }),

  /** 发送对话消息到指定会话；以 `/` 开头即斜杠命令（/llmodel、/context、/cancel…）。 */
  send: (chatId: string, content: string) =>
    invoke<void>("send_message", { chatId, content }),

  cancel: (chatId: string) => invoke<void>("cancel", { chatId }),

  /** 状态条单击：复制用户数据目录路径到系统剪贴板。 */
  copyHomePath: () => invoke<void>("copy_home_path"),

  /** 状态条双击：在系统文件管理器中打开用户数据目录。 */
  openHomeDir: () => invoke<void>("open_home_dir"),

  chatList: () => invoke<ChatMeta[]>("chat_list"),

  /** 历史快照（尾部窗口条目 + 行级游标）：会话首开 / 重建视图用。 */
  chatHistory: (chatId: string) =>
    invoke<ChatHistorySnapshot>("chat_history", { chatId }),

  /** 行级增量：返回 last_seen_row_id 之后新落库的条目 + 新游标（turn 收尾 / 窗口
   *  恢复可见时续读，重连不重拉全量）。 */
  chatHistorySince: (chatId: string, after: number) =>
    invoke<ChatHistoryDelta>("chat_history_since", { chatId, after }),

  /** 工具调用行的展开详情（完整入参 + 结果正文 + 写入前快照）：按落库行 id 懒加载。 */
  chatToolDetail: (chatId: string, msgId: number) =>
    invoke<ToolChainDetail>("chat_tool_detail", { chatId, msgId }),

  /** 从任意消息锚点分叉出新会话：复制可见历史 + 自动回滚锚点之后的文件改动（原会话不动）。 */
  chatFork: (chatId: string, anchorMessageId: number) =>
    invoke<ChatForkResult>("chat_fork", { chatId, anchorMessageId }),

  /** 最近若干个 PlanRun 计划的状态（流程步骤 / todo 轮询）。 */
  planStatus: () => invoke<PlanStatus[]>("plan_status"),

  /** 回答 world.run 的逐命令确认弹窗。 */
  workflowAnswerCommandConfirm: (id: string, allowed: boolean) =>
    invoke<void>("workflow_answer_command_confirm", { id, allowed }),

  /** 工作流运行列表（卡片）。 */
  workflowStatus: () => invoke<{ runs: WorkflowRunSummary[]; truncated: boolean; filtered?: number }>("workflow_status"),

  /** 工作流运行详情。 */
  workflowDetail: (runId: string, includeEffects = false) =>
    invoke<WorkflowDetail>("workflow_detail", { runId, includeEffects }),

  /** 取消 running/queued 工作流。 */
  workflowCancel: (runId: string) => invoke<unknown>("workflow_cancel", { runId }),

  /** 删除一条已结束的工作流运行（从属记录与脚本快照一并删除，结果产物保留）。 */
  workflowDelete: (runId: string) => invoke<unknown>("workflow_delete", { runId }),

  /** 续跑 stopped 工作流（用户来源；用户取消过的 run 由 UI 代签一次性授权）。 */
  workflowResume: (runId: string) => invoke<unknown>("workflow_resume", { runId }),

  /** 未知副作用逐项裁决。 */
  workflowConfirmEffects: (
    runId: string,
    execGeneration: number,
    decisions: Array<{ effect_key: string; action: "skip" | "rerun"; substitute_result?: unknown }>,
  ) => invoke<unknown>("workflow_confirm_effects", { runId, execGeneration, decisions }),

  /** 进程树人工确认。 */
  workflowConfirmProcessGone: (runId: string, execGeneration: number, processIds: string[]) =>
    invoke<unknown>("workflow_confirm_process_gone", { runId, execGeneration, processIds }),

  /** dead 通知重投。 */
  workflowResendNotification: (notificationId: string) =>
    invoke<unknown>("workflow_resend_notification", { notificationId }),

  /** 确认 amend 可重建（前驱有写 effect 时）。 */
  workflowConfirmAmend: (runId: string) => invoke<unknown>("workflow_confirm_amend", { runId }),

  /** 填充等待中的 hole（函数体语句）。 */
  workflowFillHole: (runId: string, holeId: string, scriptGeneration: number, script: string) =>
    invoke<unknown>("workflow_fill_hole", { runId, holeId, scriptGeneration, script }),

  /** 回答运行中 actor 的问题。 */
  workflowResolveQuestion: (questionId: string, answer: string) =>
    invoke<unknown>("workflow_resolve_question", { questionId, answer }),

  /** 暂停 / 继续一个计划（波次边界生效；当前在飞的步骤跑完后等待恢复）。 */
  planPause: (planId: string, paused: boolean) =>
    invoke<void>("plan_pause", { planId, paused }),

  chatArchive: (chatId: string) => invoke<void>("chat_archive", { chatId }),

  chatRestore: (chatId: string) => invoke<void>("chat_restore", { chatId }),

  /** 删除会话（state.sqlite 里的会话记录 + 自定义标题；前端先弹确认）。参与同步的
   *  会话：删除会传播到其他设备，各端先留一份在回收站；purge = 彻底删除，都不留。 */
  chatDelete: (chatId: string, purge = false) =>
    invoke<void>("chat_delete", { chatId, purge }),

  /** 重命名会话；标题去空白后为空 = 恢复自动派生标题（首条用户消息截断）。 */
  chatRename: (chatId: string, title: string) =>
    invoke<void>("chat_rename", { chatId, title }),

  /** 导出完整聊天记录为文档（fmt: "md" / "docx" 含图片 / "html" 折叠块+轮次导航
   *  含图片）：后端读全量 transcript 落盘 output/exports/，返回工作区相对路径；
   *  保存/转发走 saveMedia / shareMedia（旧实现返回 Markdown 字符串经前端 Blob
   *  下载，安卓 WebView 静默 no-op 已废弃）。 */
  chatExportFull: (chatId: string, title: string, fmt: "md" | "docx" | "html") =>
    invoke<string>("chat_export_full", { chatId, title, fmt }),

  agentsList: () =>
    invoke<{ executions: AgentExecution[]; total: number; truncated?: boolean }>("agents_list"),

  /** 单条子 agent 执行的实时详情（完整快照 / 事件 / 控制位）。 */
  agentExecutionDetail: (agentId: string) =>
    invoke<AgentExecutionDetail>("agent_execution_detail", { agentId }),

  /** 子 agent 完整活动记录快照（与会话历史同形：user / assistant / thinking / status）。 */
  agentExecutionHistory: (agentId: string) =>
    invoke<ChatHistorySnapshot>("agent_execution_history", { agentId }),

  /** 子 agent 活动记录的行级增量（`agent_exec` 的 transcript 事件之后按游标续读）。 */
  agentExecutionHistorySince: (agentId: string, after: number) =>
    invoke<ChatHistoryDelta>("agent_execution_history_since", { agentId, after }),

  /** 子 agent 活动记录里一条工具调用行的展开详情。 */
  agentExecutionToolDetail: (agentId: string, msgId: number) =>
    invoke<ToolChainDetail>("agent_execution_tool_detail", { agentId, msgId }),

  /** 向正在运行的子 agent 发消息（它在下一个安全点读入）。 */
  agentExecutionSend: (agentId: string, text: string) =>
    invoke<void>("agent_execution_send", { agentId, text }),

  /** 取消正在运行的子 agent。 */
  agentExecutionCancel: (agentId: string) =>
    invoke<void>("agent_execution_cancel", { agentId }),

  /** 清空已终态的后台执行记录（只清任务历史，不动会话；活任务保留）。返回清除条数。 */
  agentsClearHistory: () => invoke<number>("agents_clear_history"),

  usageSnapshot: (chatId: string) => invoke<UsageSnapshot>("usage_snapshot", { chatId }),

  toolsList: () => invoke<ToolEntry[]>("tools_list"),

  setToolEnabled: (name: string, enabled: boolean) =>
    invoke<void>("set_tool_enabled", { name, enabled }),

  skillsList: () => invoke<{ skills: SkillEntry[] }>("skills_list"),

  skillSetEnabled: (name: string, enabled: boolean) =>
    invoke<void>("skill_set_enabled", { name, enabled }),

  evolutionInfo: () => invoke<EvolutionInfo>("evolution_info"),

  configState: () => invoke<ConfigStateDto>("config_state"),
  soulPersonas: () => invoke<SoulStateDto>("soul_personas"),
  soulApply: (personaId: string, force: boolean) =>
    invoke<SoulApplyDto>("soul_apply", { personaId, force }),
  soulImport: (items: { file_name: string; content_b64: string }[]) =>
    invoke<SoulImportResultDto[]>("soul_import", { items }),

  configSave: (edits: ConfigEdit[]) =>
    invoke<void>("config_save", { edits }),

  configImportRaw: (content: string) =>
    invoke<void>("config_import_raw", { content }),

  /** wxbot 联系人查询（连接参数用表单当前值：草稿 ?? 基线；reject 抛 {code,message}）。 */
  wxbotContactsList: (apiBase: string, apiToken: string, kind: "friends" | "groups") =>
    invoke<WxbotContact[]>("wxbot_contacts_list", { apiBase, apiToken, kind }),

  wxbotContactsLookup: (
    apiBase: string,
    apiToken: string,
    displayName: string,
    groupId: string | null,
  ) =>
    invoke<string>("wxbot_contacts_lookup", {
      apiBase,
      apiToken,
      displayName,
      groupId,
    }),

  /** WCX 当前登录账号；未登录/返回为空 → null（不当作绑定确认）。 */
  wxbotContactsSelf: (apiBase: string, apiToken: string) =>
    invoke<string | null>("wxbot_contacts_self", { apiBase, apiToken }),

  /** 本机 wxbot 消息索引只读聚合（无记录返回空数组，不算错误）。 */
  wxbotContactsRecent: (
    isGroup: boolean,
    account: string | null,
    limit: number | null,
  ) =>
    invoke<WxbotRecentChat[]>("wxbot_contacts_recent", {
      isGroup,
      account,
      limit,
    }),

  slogansGet: () => invoke<SloganPrefs>("slogans_get"),

  slogansSet: (slogans: string[], intervalSecs: number) =>
    invoke<void>("slogans_set", { slogans, intervalSecs }),

  setWindowTitle: (title: string) => invoke<void>("set_window_title", { title }),

  windowMinimize: () => invoke<void>("window_minimize"),

  windowToggleMaximize: () => invoke<boolean>("window_toggle_maximize"),

  windowMaximized: () => invoke<boolean>("window_maximized"),

  windowClose: () => invoke<void>("window_close"),

  windowStartDrag: () => invoke<void>("window_start_drag"),

  /** 退出应用 GUI（安卓返回键在侧边栏上的「退出」语义）：结束 GUI 进程；
   *  常驻运行时由 START_STICKY 服务稍后无头拉起（keep-alive 设计，不停 agent）。 */
  appExit: () => invoke<void>("app_exit"),

  /** 后台保活状态（安卓：电池白名单 / 厂商 / 厂商自启动页可达；桌面 supported=false）。 */
  keepAliveStatus: () => invoke<KeepAliveStatus>("keep_alive_status"),

  /** 拉起系统「忽略电池优化」确认弹窗；回到应用后应重查 keepAliveStatus。 */
  keepAliveRequestBatteryWhitelist: () =>
    invoke<void>("keep_alive_request_battery_whitelist"),

  /** 跳转厂商自启动/后台管理页（不可达时回退本应用的应用详情页）。 */
  keepAliveOpenOemSettings: () => invoke<void>("keep_alive_open_oem_settings"),

  /** 查询 GitHub 最新 release 并与当前版本比较。 */
  skillApprovalsPending: () => invoke<SkillApprovalItem[]>("skill_approvals_pending"),

  skillApprovalsApprove: (script: string) => invoke<void>("skill_approvals_approve", { script }),

  skillApprovalsRevoke: (script: string) => invoke<void>("skill_approvals_revoke", { script }),

  skillConfigList: () => invoke<SkillConfigEntry[]>("skill_config_list"),

  skillConfigRead: (path: string) => invoke<string>("skill_config_read", { path }),

  skillConfigWrite: (path: string, content: string) =>
    invoke<void>("skill_config_write", { path, content }),

  /** 原图（放大看）。`chatId`/`blob` 给定时先按对象核对，读到的才是核对过的那个版本。 */
  mediaBase64: (path: string, chatId?: string, blob?: string) =>
    invoke<MediaPayload>("media_base64", { path, chatId, blob }),

  /** 媒体的屏幕级预览图（大图缩到长边 1440 缓存于 media/cache/preview）。
   *  卡片与查看器 1x 显示都用它，避免前端解码 12MP 原图。 */
  /** 图片预览。`chatId`/`blob` 给定时先按对象核对这个文件（内联卡与画廊按路径读的
   *  最后一道核对）。 */
  mediaPreview: (path: string, chatId?: string, blob?: string) =>
    invoke<MediaPayload>("media_preview", { path, chatId, blob }),

  /** 视频/音频的本地流式 URL（127.0.0.1 + 随机 token，支持 Range 边下边播）。 */
  // `verified` = 地址上的字节是「这一次按对象核对过」的副本（点播暂存副本）；
  // 海报/核不了时的原路径都是 false，调用方不能把它当成核对通过。
  mediaStreamUrl: (path: string, chatId?: string, blob?: string, fresh?: boolean) =>
    invoke<{ url: string; verified: boolean }>("media_stream_url", {
      path,
      chatId,
      blob,
      fresh,
    }),

  openMedia: (path: string) => invoke<void>("open_media", { path }),

  /** 浏览器快捷入口：桌面 = 无头 ↔ 可见浏览器窗口切换（与助手共用会话）；
   *  安卓 = 展开应用内浏览器面板（手动上网/过码）。返回可见模式状态。 */
  browserToggleVisible: () => invoke<boolean>("browser_toggle_visible"),

  /** 会话媒体保存到本地：图片/视频/音频进相册（桌面 Pictures/RClaw），
   *  文档进下载目录（安卓 MediaStore.Downloads / 桌面「下载/RClaw」）。 */
  saveMedia: (path: string, chatId?: string, blob?: string) =>
    invoke<string>("save_media", { path, chatId, blob }),

  /** 转发工作区媒体到其它应用（安卓系统分享面板：微信/QQ 等）。 */
  shareMedia: (path: string, chatId?: string, blob?: string) =>
    invoke<void>("share_media", { path, chatId, blob }),

  /** 把纯文本分享到其它应用（安卓 ACTION_SEND：微信/QQ 收为聊天文字）。 */
  shareText: (title: string, text: string) =>
    invoke<void>("share_text", { title, text }),

  /** 选中消息导出为文档（fmt: "md" 文本文档 / "docx" 内嵌图片文档 /
   *  "html" 折叠块+轮次导航含图片+工具全量详情）。chatId 用于后端按行 id
   *  查工具详情；messages.ts 为落库 unix 秒（Entry.ts 是毫秒，调用方除
   *  1000）；msg_id 必须是落库行 id（Entry.msgId，不是合成 Entry.id），
   *  0=未落库；seg_complete = 所在会话段是否整段被选（不完整段平铺）。
   *  返回 output/exports/ 下的工作区相对路径。 */
  exportMessages: (
    chatId: string,
    title: string,
    fmt: "md" | "docx" | "html",
    messages: Array<{
      role: "user" | "assistant" | "thinking" | "status";
      content: string;
      ts: number;
      seg_complete: boolean;
      msg_id: number;
    }>,
  ) => invoke<string>("export_messages", { chatId, title, fmt, messages }),

  /** 关于页的模块版本信息（应用/Agent 各子模块/Tauri/编译器/构建提交）。 */
  appVersions: () => invoke<AppVersions>("app_versions"),

  // ---- 工作区文件面板（安卓私有存储的应用内浏览/编辑） ----

  /** 列出工作区目录（path 相对 agent/，空 = 根；目录在前按名排序；
   *  is_config = 主配置虚拟条目 rclaw.toml，实体在 home 根）。 */
  workspaceList: (path: string) =>
    invoke<
      Array<{
        name: string;
        is_dir: boolean;
        size: number;
        modified_ms: number;
        is_config?: boolean;
      }>
    >("workspace_list", { path }),

  /** 读取工作区文本文件（白名单扩展名且 ≤1MB）。 */
  workspaceRead: (path: string) => invoke<string>("workspace_read", { path }),

  /** 保存工作区文本文件（覆盖或新建）。 */
  workspaceWrite: (path: string, content: string) =>
    invoke<void>("workspace_write", { path, content }),

  /** 复制工作区文件为同目录副本（返回新文件相对路径）。 */
  workspaceCopyFile: (path: string) => invoke<string>("workspace_copy_file", { path }),

  /** 删除工作区文件或目录（目录递归删除）。 */
  workspaceDelete: (path: string) => invoke<void>("workspace_delete", { path }),

  /** 重命名工作区文件/目录（仅名字，不换目录）。 */
  workspaceRename: (path: string, newName: string) =>
    invoke<void>("workspace_rename", { path, newName }),

  /** 在工作区目录下新建文件夹（dir 为当前目录，空 = 根）。 */
  workspaceNewFolder: (dir: string, name: string) =>
    invoke<void>("workspace_new_folder", { dir, name }),

  /** 读系统剪贴板落盘到工作区目录（安卓；文本→.txt / 文件 URI→原文件）。 */
  workspacePasteFromClipboard: (dir: string) =>
    invoke<string>("workspace_paste_from_clipboard", { dir }),

  /** 把工作区文件以 FileProvider URI 写入系统剪贴板（安卓，文件管理器可粘贴）。 */
  workspaceCopyToClipboard: (path: string) =>
    invoke<void>("workspace_copy_to_clipboard", { path }),

  consumePendingShare: () =>
    invoke<PendingShare | null>("consume_pending_share"),

  updateCheck: () => invoke<UpdateInfo>("update_check"),

  /** 下载平台对应的最新版并触发安装（Windows 换文件后自动重启；安卓拉起系统
   *  安装器）。进度经 onUpdateProgress 推送。 */
  updateDownloadInstall: () => invoke<void>("update_download_install"),

  /** 应用内更新下载进度（字节）。 */
  onUpdateProgress: (cb: (p: UpdateProgress) => void): Promise<UnlistenFn> =>
    listen<UpdateProgress>("update_progress", (event) => cb(event.payload)),

  /** 用系统方式打开外部链接（Android 浏览器 / 桌面默认浏览器）。 */
  openUrl: (url: string) => invoke<void>("open_url", { url }),

  /** 聊天链接默认入口：Android 应用内浏览器面板导航，桌面系统浏览器。 */
  browserOpenUrl: (url: string) => invoke<void>("browser_open_url", { url }),

  themeGet: () => invoke<string>("theme_get"),

  themeSet: (theme: string) => invoke<void>("theme_set", { theme }),

  // ---- 会话采集（拍照/拍视频/录音）与 whisper 转写 ----

  /** 起系统相机拍照/录视频。返回 "ok" / "permission_requested"（授权后自动续跑）。 */
  captureStart: (kind: "photo" | "video") => invoke<string>("capture_start", { kind }),

  /** 轮询采集结果（单消费）；无结果返回 null，cancelled=true 表示用户放弃。 */
  consumeCaptureResult: () =>
    invoke<{ kind: "photo" | "video"; file: string | null } | null>(
      "consume_capture_result",
    ),

  /** 开始录音。返回 "ok" / "permission_requested"（见系统弹窗后重试）。 */
  recordStart: () => invoke<string>("record_start"),

  /** 停止录音，返回相对 agent/ 的文件路径（uploads/...），失败 null。 */
  recordStop: () => invoke<string | null>("record_stop"),

  /** 原生侧是否正在录音（授权后自动开录的感知）。 */
  recordActive: () => invoke<boolean>("record_active"),

  recordCancel: () => invoke<void>("record_cancel"),

  /** 保存工作区文件到系统相册（安卓 MediaStore；桌面拷到 Pictures/RClaw）。 */
  saveToGallery: (rel: string, kind: "photo" | "video" | "audio") =>
    invoke<string>("save_to_gallery", { rel, kind }),

  /** 放弃采集产物（只删 uploads/ 里的临时文件）。 */
  discardCapture: (rel: string) => invoke<void>("discard_capture", { rel }),

  /** 编辑后的照片落盘（画布导出的 JPEG base64），返回相对 agent/ 路径。 */
  saveEditedPhoto: (dataBase64: string) =>
    invoke<string>("save_edited_photo", { dataBase64 }),

  /** whisper 模型状态（已安装/下载进度）。 */
  whisperModelStatus: () =>
    invoke<{
      installed: boolean;
      size: number;
      downloading: boolean;
      received: number;
      total: number;
    }>("whisper_model_status"),

  /** 启动 whisper 模型后台下载（已装/已在下则幂等）。 */
  whisperModelDownload: () => invoke<void>("whisper_model_download"),

  /** whisper 转写工作区音频文件，返回识别文本。 */
  whisperTranscribe: (rel: string, language?: string) =>
    invoke<string>("whisper_transcribe", { rel, language: language ?? "auto" }),

  /** 上传文件到 agent 工作区（uploads/ 子目录），返回工作区相对路径。 */
  uploadFile: (name: string, dataBase64: string) =>
    invoke<UploadResult>("upload_file", { name, dataBase64 }),

  /** 回填工作区外访问的授权结果。 */
  permissionRespond: (id: string, allowed: boolean) =>
    invoke<void>("permission_respond", { id, allowed }),

  permissionModeGet: () => invoke<PermissionMode>("permission_mode_get"),

  permissionModeSet: (mode: PermissionMode) =>
    invoke<void>("permission_mode_set", { mode }),

  insights: () => invoke<Insights>("insights"),

  cronList: () => invoke<CronEntry[]>("cron_list"),

  cronRemove: (id: string) => invoke<boolean>("cron_remove", { id }),

  /** Bot Mode：bot 花名册（含 persona 全文）。 */
  botsList: () => invoke<BotEntry[]>("bots_list"),

  /** Bot Mode：可选 provider 名（bot 编辑器固定模型下拉）。 */
  botProviders: () => invoke<string[]>("bot_providers"),

  /** Bot Mode：创建 / 覆盖保存 bot 定义（下一 turn 生效）。 */
  botSave: (input: {
    name: string;
    title: string;
    description: string;
    persona: string;
    provider: string | null;
    deniedTools: string[];
  }) =>
    invoke<void>("bot_save", {
      name: input.name,
      title: input.title,
      description: input.description,
      persona: input.persona,
      provider: input.provider,
      deniedTools: input.deniedTools,
    }),

  /** Bot Mode：删除 bot 定义与数据目录。 */
  botDelete: (name: string) => invoke<void>("bot_delete", { name }),

  /** Bot Mode：群聊列表（轻量）。 */
  groupsList: () => invoke<GroupRoom[]>("groups_list"),

  /** Bot Mode：创建群聊（成员为已注册 bot 名，2..=6 人）。 */
  groupCreate: (name: string, members: string[]) =>
    invoke<GroupRoom>("group_create", { name, members }),

  /** Bot Mode：删除群聊（进行中先取消）。 */
  groupDelete: (id: string) => invoke<void>("group_delete", { id }),

  /** Bot Mode：群聊完整历史（转录 + 活动流）。 */
  groupHistory: (id: string) => invoke<GroupRoom>("group_history", { id }),

  /** Bot Mode：发送群聊消息并触发一轮编排（结果经 group 事件推送）。 */
  groupSend: (id: string, text: string) => invoke<void>("group_send", { id, text }),

  /** Bot Mode：取消进行中的群聊编排。 */
  groupCancel: (id: string) => invoke<void>("group_cancel", { id }),

  onBus: (cb: (payload: BusPayload) => void): Promise<UnlistenFn> =>
    listen<BusPayload>("bus", (event) => cb(event.payload)),

  /** 运行时工具调用直播事件（批次开始 / 结束；结束事件带落库行 id）。 */
  onToolCalls: (cb: (payload: ToolCallsEvent) => void): Promise<UnlistenFn> =>
    listen<ToolCallsEvent>("tool_calls", (event) => cb(event.payload)),

  onAgent: (cb: (payload: AgentEvent) => void): Promise<UnlistenFn> =>
    listen<AgentEvent>("agent", (event) => cb(event.payload)),

  /** 子 agent 执行登记表直播事件（侧边栏 Agent / 任务概览实时进度与详情页直播）。 */
  onAgentExec: (cb: (payload: AgentExecEvent) => void): Promise<UnlistenFn> =>
    listen<AgentExecEvent>("agent_exec", (event) => cb(event.payload)),

  /** Bot Mode：群聊房间更新事件（载荷为房间 id，前端据此刷新）。 */
  onGroup: (cb: (payload: { id: string }) => void): Promise<UnlistenFn> =>
    listen<{ id: string }>("group", (event) => cb(event.payload)),

  /** 工作区外访问授权请求（默认权限下工具触达工作区之外时弹出）。 */
  onPermissionRequest: (cb: (req: PermissionRequest) => void): Promise<UnlistenFn> =>
    listen<PermissionRequest>("permission_request", (event) => cb(event.payload)),

  // ---- 多端同步 ----

  /** 同步面板的全部状态（引擎状态 + 当前生效的设置）。 */
  syncStatus: () => invoke<SyncState>("sync_status"),

  /** 保存同步设置（开关、设备名、范围、存储地址……）并立即生效。 */
  syncSaveConfig: (config: SyncConfig) => invoke<void>("sync_save_config", { config }),

  /** 只换存储的密码（WebDAV 的密码 / 对象存储的 Secret Key）；它只存在本机。 */
  syncSetSecret: (secret: string) => invoke<void>("sync_set_secret", { secret }),

  /** 存储的设置（可带刚填的密码）一起保存并生效：密码按这份配置的存储账户绑定，
   *  中间不会被别的配置变更插进来。 */
  syncSaveStorage: (config: SyncConfig, secret: string | null) =>
    invoke<void>("sync_save_storage", { config, secret }),

  /** 「测试连接」：用表单上正在填的设置（不必先保存）把存储要用到的操作逐个走一遍。
   *  secret 不给就用本机存着的。 */
  syncTestConnection: (config: SyncConfig, secret: string | null) =>
    invoke<SyncSelfTestStep[]>("sync_test_connection", { config, secret }),

  /** 取一个 HTTPS 地址出示的证书指纹（自签证书的「信任这张证书」）。 */
  syncProbeCertificate: (url: string) => invoke<string>("sync_probe_certificate", { url }),

  /** 创建同步空间：保存设置（顺带开启同步）与密码，在存储上建出空间。
   *  `deviceNameBaseline` 是向导打开时的设备名：只有和它不同才算这次明确改名。 */
  syncCreateSpace: (
    config: SyncConfig,
    secret: string | null,
    deviceNameBaseline: string,
  ) => invoke<void>("sync_create_space", { config, secret, deviceNameBaseline }),

  /** 检查一段配对码，并说出它带了什么（不含密钥与密码）。 */
  syncPairingInfo: (code: string, config: SyncConfig | null) =>
    invoke<SyncPairingInfo>("sync_pairing_info", { code, config }),

  /** 用配对码加入同步空间。配对码带了存储设置的话以它为准。 */
  syncJoinSpace: (
    code: string,
    config: SyncConfig | null,
    secret: string | null,
    deviceNameBaseline: string,
  ) => invoke<void>("sync_join_space", { code, config, secret, deviceNameBaseline }),

  /** 退出同步空间：本机的密钥与存储密码清掉，会话、记忆等数据原样留着。 */
  syncLeaveSpace: () => invoke<void>("sync_leave_space"),

  /** 重置同步空间（设备丢失之后用）：换新空间、新密钥，其余设备要重新加入。 */
  syncResetSpace: () => invoke<void>("sync_reset_space"),

  /** 本机所在空间的配对码与二维码。includeStorage：带上存储的地址与凭据。 */
  syncPairingCode: (includeStorage: boolean) =>
    invoke<SyncPairingCode>("sync_pairing_code", { includeStorage }),

  /** 「立即同步」：完整跑一轮，这一轮没成就报错。 */
  syncNow: () => invoke<void>("sync_now"),

  syncSetPaused: (paused: boolean) => invoke<void>("sync_set_paused", { paused }),

  /** 一条待处理事项的详情（等确认的改动的新旧内容）。 */
  syncPendingDetail: (id: string) => invoke<unknown>("sync_pending_detail", { id }),

  /** 处理一条待处理事项：accept / reject / retry / discard / dismiss。
   *  reviewed：屏幕上显示的那一版（事项 payload.hlc / 详情的 hlc）。等确认的改动必须带着，
   *  这期间来了更新的一版的话后端不办；别的事项没有版本，给 null。 */
  syncResolve: (id: string, action: string, reviewed: string | null) =>
    invoke<void>("sync_resolve", { id, action, reviewed }),

  /** 把一台设备标成「已丢失」（它的定时任务可以被强制接管），或取消。 */
  syncMarkLost: (device: string, lost: boolean) =>
    invoke<void>("sync_mark_lost", { device, lost }),

  /** 同意在按流量计费的网络下做首次同步。 */
  syncConfirmMetered: () => invoke<void>("sync_confirm_metered"),

  /** 开启附件同步之后，已有的附件传哪些。 */
  syncSetBacklog: (choice: SyncBacklog) => invoke<void>("sync_set_backlog", { choice }),

  /** 告诉引擎界面上发生了什么（窗口获得焦点 / 打开了一个会话），由它决定要不要拉取。
   *  不阻塞；同步没开时什么都不做。 */
  syncNotify: (event: "focus" | "open_session", chatId?: string) =>
    invoke<void>("sync_notify", { event, chatId: chatId ?? null }),

  /** 会话列表的角标（仅本机、已同步、分支、正在别的设备上运行）。 */
  syncSessionMarks: () => invoke<SyncSessionMark[]>("sync_session_marks"),

  /** 标记 / 取消「仅本机」。 */
  syncSetLocalOnly: (chatId: string, on: boolean) =>
    invoke<void>("sync_set_local_only", { chatId, on }),

  /** 回收站里的会话和记忆条目。不需要开启同步，退出同步空间之后也读得出来；过了保留期
   *  的在这一次读取时清掉。 */
  syncTrashList: () => invoke<SyncTrashContents>("sync_trash_list"),

  /** 从回收站恢复一个会话：作为新会话回到本机。 */
  syncTrashRestore: (id: string) => invoke<SyncImported>("sync_trash_restore", { id }),

  syncTrashRemove: (id: string) => invoke<void>("sync_trash_remove", { id }),

  /** 把回收站里的一份记忆条目放回它原来所在的记忆文件。 */
  syncTrashMemoryRestore: (id: string) =>
    invoke<SyncMemoryRestored>("sync_trash_memory_restore", { id }),

  syncTrashMemoryRemove: (id: string) => invoke<void>("sync_trash_memory_remove", { id }),

  /** 别的设备上的定时任务（只读）。 */
  syncRemoteCron: () => invoke<SyncRemoteCron[]>("sync_remote_cron"),

  /** 把别的设备上的一个定时任务移到本机执行。force：那台设备已经丢失，不等它确认。
   *  reviewed：列表里显示的那份定义的版本（SyncRemoteCron.version）。 */
  syncCronTakeover: (owner: string, id: string, force: boolean, reviewed: string) =>
    invoke<void>("sync_cron_takeover", { owner, id, force, reviewed }),

  /** 一个会话里各条消息引用的附件在本机的情况。 */
  syncAttachments: (chatId: string) =>
    invoke<SyncAttachment[]>("sync_attachments", { chatId }),

  /** 下载一个还在别的设备上的附件。不等结果：结果经 sync 事件（kind = attachment）通知。
   *  `chatId` 让后端在登记被回收过时按消息里的引用补一条再取。 */
  syncFetchAttachment: (chatId: string, blob: string) =>
    invoke<void>("sync_fetch_attachment", { chatId, blob }),

  /** 同步缓存里一张图片快照的预览。`path` 是这条消息里写的路径：同一份内容被引用成
   *  不同名字时，按它的扩展名决定能不能预览。 */
  syncAttachmentPreview: (chatId: string, blob: string, path: string) =>
    invoke<MediaPayload>("sync_attachment_preview", { chatId, blob, path }),

  /** 点开同步会话里的一份附件：后端先按对象核对工作区里这个文件的内容，对不上就不
   *  打开（返回错误），由界面改出「重新下载」。返回值 = 打开的是不是核对过的副本
   *  （核不了时按老行为打开原路径，为 false）。 */
  syncOpenAttachment: (chatId: string, blob: string, path: string) =>
    invoke<boolean>("sync_open_attachment", { chatId, blob, path }),

  /** 把一份文件快照存进工作区（不覆盖已有的不同文件），返回落点与「写入的字节是不是
   *  核对过的」（核不了时按老行为复制，verified=false）。
   *  `path` 是消息里写的那个路径：同一份内容被几条消息引用时，存到点的那条的位置上。 */
  syncAttachmentSave: (chatId: string, blob: string, path: string) =>
    invoke<{ rel: string; verified: boolean }>("sync_attachment_save", {
      chatId,
      blob,
      path,
    }),

  /** 把一个会话导出成会话包（明文，含这个会话的全部内容）：落在 output/exports/，
   *  返回工作区相对路径；去向经 saveMedia / shareMedia 决定。不需要开启同步。 */
  sessionExportBundle: (chatId: string, title: string) =>
    invoke<string>("session_export_bundle", { chatId, title }),

  /** 导入一个会话包：总是新建一个会话，从不覆盖已有的。 */
  sessionImportBundle: (text: string) =>
    invoke<SyncImported>("session_import_bundle", { text }),

  /** 「从图片识别」：从一张二维码的图片里读出配对码（图片只在内存里过一遍）。 */
  syncScanQrImage: (dataBase64: string) =>
    invoke<string>("sync_scan_qr_image", { dataBase64 }),

  /** 拍照扫码（仅手机）：拉起系统相机。返回本轮的 requestId 与 ok / permission_requested；
   *  之后带着 requestId 轮询 syncScanQrPoll。 */
  syncScanQrStart: () => invoke<SyncScanStart>("sync_scan_qr_start"),

  syncScanQrPoll: (requestId: string) =>
    invoke<SyncScan>("sync_scan_qr_poll", { requestId }),

  /** 取消扫码：只删本轮 requestId 的标记、照片与结果。 */
  syncScanQrCancel: (requestId: string) =>
    invoke<void>("sync_scan_qr_cancel", { requestId }),

  /** 同步事件（状态变了 / 会话被同步改了 / 附件取回了 / 某类数据要重新加载）。 */
  onSync: (cb: (payload: SyncEvent) => void): Promise<UnlistenFn> =>
    listen<SyncEvent>("sync", (event) => cb(event.payload)),
};

/** 正式回复之外的状态消息（工具调用进度、取消确认等），渲染为弱化状态条。 */
export function isStatusLine(content: string): boolean {
  return (
    content.startsWith("🟢") ||
    content.startsWith("🧩") ||
    content.startsWith("🔧") ||
    content.startsWith("正在调用工具") ||
    content.startsWith("（已停止当前任务）")
  );
}

/** 工具调用进度行的结构化解析结果。 */
export interface ToolStatus {
  /** 迭代序号（同一轮多个调用共享）。 */
  iter: number;
  /** 工具名（read / edit / shell …）。 */
  tool: string;
  /** 摘要（路径 + 行数增减 / 命令预览 / 检索词）；无摘要为 null。 */
  detail: string | null;
}

/** 解析 `正在调用工具(N):tool → detail` 进度行；非该格式返回 null。 */
export function parseToolStatusLine(content: string): ToolStatus | null {
  const m = content.match(/^正在调用工具\((\d+)\):(.*)$/);
  if (!m) return null;
  const rest = m[2];
  const sep = rest.indexOf(" → ");
  if (sep === -1) {
    return { iter: Number(m[1]), tool: rest, detail: null };
  }
  return {
    iter: Number(m[1]),
    tool: rest.slice(0, sep),
    detail: rest.slice(sep + 3),
  };
}

/** 思考过程消息（模型 reasoning 经中间输出通道推送，💭 前缀）。 */
export function isThinkingLine(content: string): boolean {
  return content.startsWith("💭");
}

/** provider 预设：向导里一键填充 OpenAI 兼容端点与默认模型。 */
export const PROVIDER_PRESETS: Record<
  string,
  { label: string; baseUrl: string; model: string; contextLength: number }
> = {
  zhipu: {
    label: "智谱 GLM",
    baseUrl: "https://open.bigmodel.cn/api/paas/v4",
    model: "glm-5.3-flash",
    contextLength: 131072,
  },
  deepseek: {
    label: "DeepSeek",
    baseUrl: "https://api.deepseek.com/v1",
    model: "deepseek-chat",
    contextLength: 131072,
  },
  custom: {
    label: "自定义（OpenAI 兼容）",
    baseUrl: "",
    model: "",
    contextLength: 128000,
  },
};
