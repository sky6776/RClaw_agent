import { useCallback, useEffect, useRef, useState } from "react";
import { api, type WorkflowDetail, type WorkflowRunSummary } from "../lib/api";

const TERMINAL = ["completed", "errored", "stopped"];

/** 本机隐藏的已结束运行卡片（run_id 列表）。localStorage 损坏 / 不可用时按「未隐藏」处理。 */
const DISMISSED_KEY = "rclaw_workflow_dismissed";
/** 只留最近这么多条，防止偏好无限增长（内存里的集合同样按这个上限裁剪，保证两边一致）。 */
const DISMISSED_MAX = 100;

function loadDismissedRuns(): string[] {
  try {
    const raw = localStorage.getItem(DISMISSED_KEY);
    if (!raw) return [];
    const parsed: unknown = JSON.parse(raw);
    if (!Array.isArray(parsed)) return [];
    return parsed
      .filter((id): id is string => typeof id === "string")
      .slice(-DISMISSED_MAX);
  } catch {
    return [];
  }
}

function saveDismissedRuns(ids: string[]) {
  try {
    localStorage.setItem(DISMISSED_KEY, JSON.stringify(ids.slice(-DISMISSED_MAX)));
  } catch {
    // 隐私模式等存储不可用：本次会话内仍然隐藏（内存态），切页 / 重启后恢复显示。
  }
}

/** run_failed 的 error 是 {code,message,kind} 对象；直接 String() 会显示 [object Object]。 */
function formatError(error: unknown): string {
  if (error == null) return "";
  if (typeof error === "string") return error;
  if (typeof error === "object") {
    const record = error as Record<string, unknown>;
    const code = typeof record.code === "string" ? record.code : "";
    const message = typeof record.message === "string" ? record.message : "";
    if (code || message) return code && message ? `${code}: ${message}` : code || message;
  }
  try {
    return JSON.stringify(error);
  } catch {
    return String(error);
  }
}

/** 工作流运行卡片（最小展示面）：状态 / 进度 / 等待中的问题 / hole / 副作用裁决 /
 *  进程树确认 / 通知重投。轮询 workflow_status；展开时拉 workflow_detail，操作后刷新。 */
export default function WorkflowCard({ onError }: { onError?: (message: string) => void }) {
  const [runs, setRuns] = useState<WorkflowRunSummary[]>([]);
  const [openId, setOpenId] = useState<string | null>(null);
  const [detail, setDetail] = useState<WorkflowDetail | null>(null);
  const [answers, setAnswers] = useState<Record<string, string>>({});
  const [holeBodies, setHoleBodies] = useState<Record<string, string>>({});
  /** 本机隐藏掉的已结束运行卡片（界面偏好，存 localStorage；不影响其它设备与后端记录）。
   *  终态运行会作为摘要常驻一条，没有这个出口它就永远占着输入框上方。 */
  const dismissedRef = useRef<Set<string>>(new Set(loadDismissedRuns()));
  /** 本机隐藏了几条已结束运行：用于给出「恢复显示」入口（续跑 / 允许修订 / 重投都在这类卡片上）。
   *  初值直接取已读出的隐藏记录：首轮轮询前 / 轮询一直失败时，恢复入口也要可用。 */
  const [hiddenCount, setHiddenCount] = useState(() => dismissedRef.current.size);
  /** 请求序号（每次发出 +1）与已应用的响应序号：乱序返回的旧响应直接丢弃，
   *  但「响应比轮询间隔还慢」时不至于每次都被判过期（只要它仍是最新应用的响应）。 */
  /** 正在删除的 run（可同时有多条）：删除在途时禁用该卡片的隐藏 / 续跑 / 允许修订 / 删除，
   *  避免两套状态互相踩。 */
  const [deletingIds, setDeletingIds] = useState<Set<string>>(new Set());
  const pollSeq = useRef(0);
  const appliedSeq = useRef(0);
  /** 隐藏这张已结束的运行卡片（仅本机界面；运行记录本身不动，仍可用工具查询）。 */
  const dismissRun = useCallback((runId: string) => {
    // 内存集合与 localStorage 用同一份「最近 DISMISSED_MAX 条」清单，避免被淘汰的 id
    // 只活在内存里（切页重挂载后卡片又回来）。
    const ids = [...dismissedRef.current].filter((id) => id !== runId);
    ids.push(runId);
    const capped = ids.slice(-DISMISSED_MAX);
    dismissedRef.current = new Set(capped);
    saveDismissedRuns(capped);
    setRuns((prev) => prev.filter((run) => run.run_id !== runId));
    setOpenId((prev) => (prev === runId ? null : prev));
    // 隐藏数只有这一个口径（隐藏集合大小），与轮询里重算的值一致，避免「先显示 2 条、
    // 下一轮变 1 条」的跳变。
    setHiddenCount(dismissedRef.current.size);
  }, []);

  const loadDetail = useCallback(
    (runId: string) =>
      api
        .workflowDetail(runId, true)
        .then((value) => setDetail(value))
        .catch((error) => onError?.(String(error))),
    [onError],
  );

  /** 执行一个管理操作：成功提示返回值并刷新详情，失败提示错误。 */
  const runAction = useCallback(
    (runId: string, action: Promise<unknown>) =>
      action
        .then((value) => {
          onError?.(JSON.stringify(value));
          return loadDetail(runId);
        })
        .catch((error) => onError?.(String(error))),
    [loadDetail, onError],
  );

  /** 拉一次运行状态：未收尾的全显示，终态只保留最新 1 条当摘要（本机隐藏的不显示、
   *  也不拿更早的终态补位）。 */
  const poll = useCallback(async () => {
    const seq = ++pollSeq.current;
    let result;
    try {
      result = await api.workflowStatus();
    } catch {
      return; // agent 未启动时静默（首屏常见）。
    }
    // 已经有更新的响应被应用 / 已卸载（卸载时把 appliedSeq 抬到最新）：旧响应作废，
    // 否则刚隐藏完，旧响应会把同一张卡片连同「取消」带回来。慢响应只要仍是最新的
    // 一条被应用，就照常生效——不会因为「比轮询间隔还慢」被永久丢弃。
    if (seq <= appliedSeq.current) return;
    appliedSeq.current = seq;
    const active = result.runs.filter((run) => !TERMINAL.includes(run.status));
    // 后端按 updated_at 倒序返回：第一条终态就是最新的那条。视图只保留这一条当摘要，
    // 隐藏后也不拿更早的终态补位（更早的终态在改动前同样不在视图里）。
    const latestTerminal = result.runs.find((run) => TERMINAL.includes(run.status));
    const terminal =
      latestTerminal && !dismissedRef.current.has(latestTerminal.run_id) ? [latestTerminal] : [];
    setRuns([...active, ...terminal]);
    setHiddenCount(dismissedRef.current.size);
  }, []);

  /** 恢复显示本机隐藏掉的已结束运行：卡片上的「续跑 / 允许修订 / 重投通知」入口随之回来。 */
  const restoreDismissed = useCallback(() => {
    dismissedRef.current = new Set();
    saveDismissedRuns([]);
    setHiddenCount(0);
    void poll();
  }, [poll]);

  useEffect(() => {
    void poll();
    const timer = window.setInterval(() => void poll(), 3000);
    return () => {
      window.clearInterval(timer);
      // 卸载后作废在途响应（poll 里以 appliedSeq 判定新旧）。
      pollSeq.current += 1;
      appliedSeq.current = pollSeq.current;
    };
  }, [poll]);

  /** 彻底删除一条已结束的运行（不可恢复）：运行记录 + 从属数据一起删，脚本快照尽力
   *  清理，`workflow-artifacts/` 下的结果产物文件保留。 */
  const deleteRun = useCallback(
    (runId: string) => {
      if (
        !window.confirm(
          "删除这条运行记录？运行记录与副作用 / 进程 / 通知等从属数据会删除（脚本快照尽力清理），无法恢复；结果产物文件保留。",
        )
      ) {
        return;
      }
      setDeletingIds((prev) => new Set(prev).add(runId));
      api
        .workflowDelete(runId)
        .then(() => {
          // 删掉的 id 不必再留在隐藏集合里。
          if (dismissedRef.current.delete(runId)) {
            saveDismissedRuns([...dismissedRef.current]);
            setHiddenCount(dismissedRef.current.size);
          }
          setRuns((prev) => prev.filter((run) => run.run_id !== runId));
          setOpenId((prev) => (prev === runId ? null : prev));
          onError?.("已删除运行记录");
          // 作废删除前发出的状态请求：旧列表里有这条 run，晚到也会把卡片写回来。
          pollSeq.current += 1;
          appliedSeq.current = pollSeq.current;
          void poll();
        })
        .catch((error) => onError?.(String(error)))
        .finally(() =>
          setDeletingIds((prev) => {
            const next = new Set(prev);
            next.delete(runId);
            return next;
          }),
        );
    },
    [onError, poll],
  );

  useEffect(() => {
    if (!openId) {
      setDetail(null);
      return;
    }
    let alive = true;
    api
      .workflowDetail(openId, true)
      .then((value) => {
        if (alive) setDetail(value);
      })
      .catch((error) => onError?.(String(error)));
    return () => {
      alive = false;
    };
  }, [openId, onError]);

  // 没有可见卡片、也没有隐藏项才整体不渲染——只剩「恢复显示」入口时也必须渲染。
  if (runs.length === 0 && hiddenCount === 0) return null;

  return (
    <div className="mb-2 max-h-72 space-y-1.5 overflow-y-auto">
      {runs.map((run) => {
        const open = openId === run.run_id;
        const canCancel = ["running", "queued"].includes(run.status);
        // stopped 且可续跑：用户取消过的 run 只有 UI 能代签新鲜授权，模型工具面拿不到 token。
        const canResume =
          run.status === "stopped" && run.resumable !== false && !run.superseded_by && !run.legacy;
        // stopping 且仍有未确认的命令进程树：Windows 没有 OS 侧探测，只能由用户确认已退出。
        const unconfirmedProcesses =
          detail?.run_id === run.run_id && run.status === "stopping"
            ? (detail.processes ?? []).filter((process) =>
                ["running", "unknown"].includes(process.state),
              )
            : [];
        // 终态折叠掉的进度通知也是 dead，但没有重投意义；只列真正投递失败的。
        const deadNotifications =
          detail?.run_id === run.run_id
            ? (detail.notifications ?? []).filter(
                (note) =>
                  note.state === "dead" &&
                  note.last_error !== "superseded by terminal notification",
              )
            : [];
        const canConfirmAmend =
          detail?.run_id === run.run_id &&
          TERMINAL.includes(run.status) &&
          (detail.effects ?? []).some((effect) => effect.is_write);
        return (
          <div
            key={run.run_id}
            className="overflow-hidden rounded-xl border border-[#2a2f3a] bg-[#141821]"
          >
            <div className="flex items-center gap-2 px-3 py-2">
              <button
                onClick={() => setOpenId(open ? null : run.run_id)}
                className="min-w-0 flex-1 truncate text-left text-xs text-gray-200"
                title={run.run_id}
              >
                <span className="mr-1">🧩</span>
                {run.label}
                <span className="ml-2 text-[10px] text-gray-500">{run.status}</span>
                {run.possibly_interrupted && (
                  <span className="ml-2 text-[10px] text-amber-400">可能已中断</span>
                )}
              </button>
              {/* 已结束（终态）的卡片可以隐藏：终态摘要常驻一条，没有这个出口就一直占着
                  输入框上方。可续跑的 stopped 卡片不给 ✕——「续跑」是只能由桌面 UI 签发的
                  授权入口，隐藏后模型也代签不了。只清本机界面显示，后端运行记录不动。 */}
              {TERMINAL.includes(run.status) && !canResume && (
                <button
                  onClick={() => dismissRun(run.run_id)}
                  disabled={deletingIds.has(run.run_id)}
                  title="隐藏这张已结束的运行卡片（只影响本机界面，运行记录仍可用工具查询）"
                  aria-label="隐藏已结束的运行卡片"
                  className="shrink-0 rounded px-1.5 py-0.5 text-[11px] text-gray-500 transition hover:text-gray-200 disabled:opacity-40"
                >
                  ✕
                </button>
              )}
              {canCancel && (
                <button
                  onClick={() =>
                    api
                      .workflowCancel(run.run_id)
                      .then(() => {
                        onError?.("已发送取消；等待引擎确认退出");
                        if (open) void loadDetail(run.run_id);
                      })
                      .catch((error) => onError?.(String(error)))
                  }
                  className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-300 hover:border-red-500/50 hover:text-red-300"
                >
                  取消
                </button>
              )}
              {canResume && (
                <button
                  disabled={deletingIds.has(run.run_id)}
                  onClick={() => {
                    if (!window.confirm(`续跑工作流「${run.label}」？`)) return;
                    void runAction(run.run_id, api.workflowResume(run.run_id));
                  }}
                  className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-300 hover:border-emerald-500/50 hover:text-emerald-300 disabled:opacity-40"
                >
                  续跑
                </button>
              )}
            </div>
            {open && detail?.run_id === run.run_id && (
              <div className="space-y-2 border-t border-[#2a2f3a] px-3 py-2 text-[11px] text-gray-400">
                <div>
                  进度：{detail.progress?.asks_done ?? 0}/{detail.progress?.asks_total ?? 0}
                  {detail.stop_reason ? ` · 停止原因 ${detail.stop_reason}` : ""}
                </div>
                {detail.phases && detail.phases.length > 0 && (
                  <div className="flex flex-wrap gap-1">
                    {detail.phases.map((phase) => (
                      <span
                        key={`${phase.name}-${phase.started_at}`}
                        className="rounded bg-[#1d2433] px-1.5 py-0.5 text-[10px]"
                      >
                        {phase.name} · {phase.status}
                      </span>
                    ))}
                  </div>
                )}
                {detail.pending_questions?.map((question) => (
                  <div key={question.question_id} className="space-y-1">
                    <div className="text-amber-300">
                      {question.actor}: {question.question}
                    </div>
                    <div className="flex gap-1">
                      <input
                        value={answers[question.question_id] ?? ""}
                        onChange={(event) =>
                          setAnswers((prev) => ({
                            ...prev,
                            [question.question_id]: event.target.value,
                          }))
                        }
                        placeholder="回答…"
                        className="min-w-0 flex-1 rounded border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 text-[11px] text-gray-200"
                      />
                      <button
                        onClick={() =>
                          void runAction(
                            run.run_id,
                            api.workflowResolveQuestion(
                              question.question_id,
                              answers[question.question_id] ?? "",
                            ),
                          )
                        }
                        className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-200"
                      >
                        回答
                      </button>
                    </div>
                  </div>
                ))}
                {detail.holes
                  ?.filter((hole) => hole.state === "waiting")
                  .map((hole) => (
                    <div key={hole.hole_id} className="space-y-1 text-indigo-300">
                      <div>
                        hole {hole.name}（{hole.hole_id}）等待填充：{hole.prompt ?? ""}
                      </div>
                      <div className="flex gap-1">
                        <textarea
                          value={holeBodies[hole.hole_id] ?? ""}
                          onChange={(event) =>
                            setHoleBodies((prev) => ({
                              ...prev,
                              [hole.hole_id]: event.target.value,
                            }))
                          }
                          placeholder="函数体语句（可 return）"
                          rows={3}
                          className="min-w-0 flex-1 rounded border border-[#2a2f3a] bg-[#0f131b] px-2 py-1 text-[11px] text-gray-200"
                        />
                        <button
                          onClick={() =>
                            void runAction(
                              run.run_id,
                              api.workflowFillHole(
                                run.run_id,
                                hole.hole_id,
                                hole.script_generation,
                                holeBodies[hole.hole_id] ?? "",
                              ),
                            )
                          }
                          className="self-start rounded border border-[#3a4050] px-2 py-0.5 text-[10px]"
                        >
                          填充
                        </button>
                      </div>
                    </div>
                  ))}
                {detail.effects && detail.effects.length > 0 && (
                  <div className="space-y-1">
                    {detail.effects
                      .filter((effect) =>
                        ["failed", "started", "unknown"].includes(effect.status),
                      )
                      .map((effect) => (
                        <div key={effect.effect_key} className="flex items-center gap-1">
                          <span className="min-w-0 flex-1 truncate">
                            {effect.op} · {effect.status} · {effect.args_preview ?? ""}
                          </span>
                          <button
                            onClick={() => {
                              const blocking = (detail.effects ?? []).filter((item) =>
                                ["failed", "started", "unknown"].includes(item.status),
                              );
                              void runAction(
                                run.run_id,
                                api.workflowConfirmEffects(
                                  run.run_id,
                                  (detail.exec_generation ?? 0) + 1,
                                  blocking.map((item) => ({
                                    effect_key: item.effect_key,
                                    action: "rerun" as const,
                                  })),
                                ),
                              );
                            }}
                            className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px]"
                          >
                            全部重跑
                          </button>
                          <button
                            onClick={() => {
                              const blocking = (detail.effects ?? []).filter((item) =>
                                ["failed", "started", "unknown"].includes(item.status),
                              );
                              // skip 必须给脚本一个替代返回值（续跑时 world.run 等直接返回它）。
                              const raw = window.prompt(
                                "跳过这些副作用：请输入脚本将收到的替代结果（JSON）",
                                "null",
                              );
                              if (raw == null) return;
                              let substitute: unknown;
                              try {
                                substitute = JSON.parse(raw);
                              } catch {
                                onError?.("替代结果不是合法 JSON");
                                return;
                              }
                              void runAction(
                                run.run_id,
                                api.workflowConfirmEffects(
                                  run.run_id,
                                  (detail.exec_generation ?? 0) + 1,
                                  blocking.map((item) => ({
                                    effect_key: item.effect_key,
                                    action: "skip" as const,
                                    substitute_result: substitute,
                                  })),
                                ),
                              );
                            }}
                            className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px]"
                          >
                            全部跳过
                          </button>
                        </div>
                      ))}
                  </div>
                )}
                {detail.artifacts && detail.artifacts.length > 0 && (
                  <div className="space-y-0.5">
                    {detail.artifacts.map((artifact) => (
                      <div key={`${artifact.id}-${artifact.version}`}>
                        📎 {artifact.title || artifact.id}（{artifact.kind} v{artifact.version}）
                        <span className="ml-1 text-gray-600">{artifact.uri}</span>
                      </div>
                    ))}
                  </div>
                )}
                {unconfirmedProcesses.length > 0 && (
                  <div className="space-y-1 text-amber-300">
                    <div>
                      宿主重启时仍有 world.run 命令在飞；确认下列进程树已退出后才能收口：
                    </div>
                    {unconfirmedProcesses.map((process) => (
                      <div key={process.process_id} className="text-[10px] text-gray-400">
                        {process.process_id}（pid {process.root_pid}，{process.state}）
                      </div>
                    ))}
                    <button
                      onClick={() => {
                        if (
                          !window.confirm(
                            "确认这些命令进程（含子进程）都已经退出？确认后 run 转为 stopped，可 Resume。",
                          )
                        ) {
                          return;
                        }
                        void runAction(
                          run.run_id,
                          api.workflowConfirmProcessGone(
                            run.run_id,
                            detail.exec_generation ?? 0,
                            unconfirmedProcesses.map((process) => process.process_id),
                          ),
                        );
                      }}
                      className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-200"
                    >
                      确认进程已退出
                    </button>
                  </div>
                )}
                {deadNotifications.length > 0 && (
                  <div className="space-y-1">
                    {deadNotifications.map((note) => (
                      <div key={note.id} className="flex items-center gap-1">
                        <span className="min-w-0 flex-1 truncate text-red-300">
                          通知投递失败（{note.attempts} 次）：{note.last_error ?? note.id}
                        </span>
                        <button
                          onClick={() =>
                            void runAction(run.run_id, api.workflowResendNotification(note.id))
                          }
                          className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-200"
                        >
                          重投
                        </button>
                      </div>
                    ))}
                  </div>
                )}
                {canConfirmAmend && (
                  <div className="flex items-center gap-1">
                    <span className="min-w-0 flex-1 truncate">
                      该 run 有写副作用；模型的 AmendWorkflow 需要你先放行一次
                    </span>
                    <button
                      disabled={deletingIds.has(run.run_id)}
                      onClick={() =>
                        void runAction(run.run_id, api.workflowConfirmAmend(run.run_id))
                      }
                      className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-200"
                    >
                      允许修订
                    </button>
                  </div>
                )}
                {/* 彻底删除（只有终态、非 legacy 可选）：legacy 行没有项目归属，服务端会以
                    forbidden_project 拒绝；从属记录一并删，产物文件保留。 */}
                {TERMINAL.includes(run.status) && !run.legacy && (
                  <div className="flex items-center gap-1">
                    <span className="min-w-0 flex-1 truncate text-gray-500">
                      删除后运行记录与从属数据一并移除（结果产物文件保留）
                    </span>
                    <button
                      onClick={() => deleteRun(run.run_id)}
                      disabled={deletingIds.has(run.run_id)}
                      className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-red-300 hover:border-red-500/50 disabled:opacity-40"
                    >
                      {deletingIds.has(run.run_id) ? "删除中…" : "删除"}
                    </button>
                  </div>
                )}
                {detail.error != null && (
                  <div className="text-red-300">错误：{formatError(detail.error)}</div>
                )}
                {detail.recovery && <div>恢复：{detail.recovery}</div>}
              </div>
            )}
          </div>
        );
      })}
      {/* 恢复入口：隐藏只清本机显示，卡片上的续跑 / 允许修订 / 重投通知仍要能拿回来
          （这些授权只能由桌面 UI 签发，模型代签不了）。视图里本来就只显示最新一条
          终态摘要，清掉隐藏记录它就会回来。 */}
      {hiddenCount > 0 && (
        <button
          onClick={restoreDismissed}
          title={`恢复显示已隐藏的运行卡片（本机共隐藏 ${hiddenCount} 条）`}
          className="w-full rounded-xl px-3 py-1 text-left text-[11px] text-gray-500 transition hover:text-gray-300"
        >
          ↺ 恢复显示已结束的运行卡片（已隐藏 {hiddenCount} 条）
        </button>
      )}
    </div>
  );
}
