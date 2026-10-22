import { useCallback, useEffect, useState } from "react";
import { api, type WorkflowDetail, type WorkflowRunSummary } from "../lib/api";

const TERMINAL = ["completed", "errored", "stopped"];

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

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      try {
        const result = await api.workflowStatus();
        if (!alive) return;
        // 未收尾的全显示；终态保留最近 1 条当摘要。
        const active = result.runs.filter(
          (run) => !["completed", "errored", "stopped"].includes(run.status),
        );
        const terminal = result.runs.filter(
          (run) => ["completed", "errored", "stopped"].includes(run.status),
        );
        setRuns([...active, ...terminal.slice(0, 1)]);
      } catch {
        // agent 未启动时静默（首屏常见）。
      }
    };
    void tick();
    const timer = window.setInterval(tick, 3000);
    return () => {
      alive = false;
      window.clearInterval(timer);
    };
  }, []);

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

  if (runs.length === 0) return null;

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
                  onClick={() => {
                    if (!window.confirm(`续跑工作流「${run.label}」？`)) return;
                    void runAction(run.run_id, api.workflowResume(run.run_id));
                  }}
                  className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-300 hover:border-emerald-500/50 hover:text-emerald-300"
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
                      onClick={() =>
                        void runAction(run.run_id, api.workflowConfirmAmend(run.run_id))
                      }
                      className="rounded border border-[#3a4050] px-2 py-0.5 text-[10px] text-gray-200"
                    >
                      允许修订
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
    </div>
  );
}
