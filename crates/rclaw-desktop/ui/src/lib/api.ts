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
  role: "user" | "assistant" | "status";
  content: string;
}

/** 行级快照：尾部条目 + 消费游标（之后用 chatHistorySince 按游标续读）。 */
export interface ChatHistorySnapshot {
  entries: HistoryMsg[];
  /** 读取时会话的最大消息行 id（含被过滤行——它们被游标消费、不再投递）。 */
  cursor: number;
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

export interface WizardInput {
  provider: string;
  baseUrl: string;
  model: string;
  apiKey: string;
  contextLength: number;
  autoEvolve: boolean;
}

/** 后台子 agent 执行快照（executions_payload 的 JSON）。 */
export interface AgentExecution {
  agent_id: string;
  state: string;
  progress: { completed_steps: number; total_steps: number; stage: string };
  goal: string;
  created_at: string;
  updated_at: string;
}

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
}

export interface UpdateInfo {
  current: string;
  latest: string | null;
  url: string | null;
  notes: string | null;
  error: string | null;
}

/** 应用内更新进度（update_progress 事件载荷）。 */
export interface UpdateProgress {
  phase: "download" | "install";
  received: number;
  total: number;
}

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

  /** 从任意消息锚点分叉出新会话：复制可见历史 + 自动回滚锚点之后的文件改动（原会话不动）。 */
  chatFork: (chatId: string, anchorMessageId: number) =>
    invoke<ChatForkResult>("chat_fork", { chatId, anchorMessageId }),

  /** 最近若干个 PlanRun 计划的状态（流程步骤 / todo 轮询）。 */
  planStatus: () => invoke<PlanStatus[]>("plan_status"),

  /** 暂停 / 继续一个计划（波次边界生效；当前在飞的步骤跑完后等待恢复）。 */
  planPause: (planId: string, paused: boolean) =>
    invoke<void>("plan_pause", { planId, paused }),

  chatArchive: (chatId: string) => invoke<void>("chat_archive", { chatId }),

  chatRestore: (chatId: string) => invoke<void>("chat_restore", { chatId }),

  /** 删除会话（state.sqlite 里的会话记录 + 自定义标题；前端先弹确认）。 */
  chatDelete: (chatId: string) => invoke<void>("chat_delete", { chatId }),

  /** 重命名会话；标题去空白后为空 = 恢复自动派生标题（首条用户消息截断）。 */
  chatRename: (chatId: string, title: string) =>
    invoke<void>("chat_rename", { chatId, title }),

  /** 导出完整聊天记录（Markdown 文本，不裁剪条数；前端经 Blob 触发下载）。 */
  chatExport: (chatId: string) => invoke<string>("chat_export", { chatId }),

  agentsList: () => invoke<{ executions: AgentExecution[]; total: number }>("agents_list"),

  usageSnapshot: (chatId: string) => invoke<UsageSnapshot>("usage_snapshot", { chatId }),

  toolsList: () => invoke<ToolEntry[]>("tools_list"),

  setToolEnabled: (name: string, enabled: boolean) =>
    invoke<void>("set_tool_enabled", { name, enabled }),

  skillsList: () => invoke<{ skills: SkillEntry[] }>("skills_list"),

  skillSetEnabled: (name: string, enabled: boolean) =>
    invoke<void>("skill_set_enabled", { name, enabled }),

  evolutionInfo: () => invoke<EvolutionInfo>("evolution_info"),

  configState: () => invoke<ConfigStateDto>("config_state"),

  configSave: (edits: ConfigEdit[]) =>
    invoke<void>("config_save", { edits }),

  configImportRaw: (content: string) =>
    invoke<void>("config_import_raw", { content }),

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

  /** 查询 GitHub 最新 release 并与当前版本比较。 */
  skillApprovalsPending: () => invoke<SkillApprovalItem[]>("skill_approvals_pending"),

  skillApprovalsApprove: (script: string) => invoke<void>("skill_approvals_approve", { script }),

  skillApprovalsRevoke: (script: string) => invoke<void>("skill_approvals_revoke", { script }),

  skillConfigList: () => invoke<SkillConfigEntry[]>("skill_config_list"),

  skillConfigRead: (path: string) => invoke<string>("skill_config_read", { path }),

  skillConfigWrite: (path: string, content: string) =>
    invoke<void>("skill_config_write", { path, content }),

  mediaBase64: (path: string) => invoke<MediaPayload>("media_base64", { path }),

  /** 媒体的屏幕级预览图（大图缩到长边 1440 缓存于 media/cache/preview）。
   *  卡片与查看器 1x 显示都用它，避免前端解码 12MP 原图。 */
  mediaPreview: (path: string) => invoke<MediaPayload>("media_preview", { path }),

  /** 视频/音频的本地流式 URL（127.0.0.1 + 随机 token，支持 Range 边下边播）。 */
  mediaStreamUrl: (path: string) => invoke<string>("media_stream_url", { path }),

  openMedia: (path: string) => invoke<void>("open_media", { path }),

  /** 浏览器快捷入口：桌面 = 无头 ↔ 可见浏览器窗口切换（与助手共用会话）；
   *  安卓 = 展开应用内浏览器面板（手动上网/过码）。返回可见模式状态。 */
  browserToggleVisible: () => invoke<boolean>("browser_toggle_visible"),

  /** 会话媒体保存到本地：图片/视频/音频进相册（桌面 Pictures/RClaw），
   *  文档进下载目录（安卓 MediaStore.Downloads / 桌面「下载/RClaw」）。 */
  saveMedia: (path: string) => invoke<string>("save_media", { path }),

  /** 转发工作区媒体到其它应用（安卓系统分享面板：微信/QQ 等）。 */
  shareMedia: (path: string) => invoke<void>("share_media", { path }),

  /** 把纯文本分享到其它应用（安卓 ACTION_SEND：微信/QQ 收为聊天文字）。 */
  shareText: (title: string, text: string) =>
    invoke<void>("share_text", { title, text }),

  /** 选中消息导出为文档（fmt: "md" 文本文档 / "docx" 内嵌图片文档）。
   *  返回 output/exports/ 下的工作区相对路径。 */
  exportMessages: (
    title: string,
    fmt: "md" | "docx",
    messages: Array<{
      role: "user" | "assistant" | "thinking" | "status";
      content: string;
    }>,
  ) => invoke<string>("export_messages", { title, fmt, messages }),

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

  onAgent: (cb: (payload: AgentEvent) => void): Promise<UnlistenFn> =>
    listen<AgentEvent>("agent", (event) => cb(event.payload)),

  /** Bot Mode：群聊房间更新事件（载荷为房间 id，前端据此刷新）。 */
  onGroup: (cb: (payload: { id: string }) => void): Promise<UnlistenFn> =>
    listen<{ id: string }>("group", (event) => cb(event.payload)),

  /** 工作区外访问授权请求（默认权限下工具触达工作区之外时弹出）。 */
  onPermissionRequest: (cb: (req: PermissionRequest) => void): Promise<UnlistenFn> =>
    listen<PermissionRequest>("permission_request", (event) => cb(event.payload)),
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
  const m = /^正在调用工具\((\d+)\):(.*)$/.exec(content);
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
