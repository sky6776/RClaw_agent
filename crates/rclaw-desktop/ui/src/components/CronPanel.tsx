import { useCallback, useEffect, useState } from "react";
import { api, type CronEntry, type SyncRemoteCron } from "../lib/api";
import { SyncConfirm } from "./SyncBits";

const remoteKey = (job: SyncRemoteCron) => `${job.owner}/${job.id}`;

/** 定时任务面板：列表 + 删除；新增任务直接在对话里让 RClaw 自己加（cron_add 工具）。
 *
 *  开了多端同步、并且在同步定时任务时，下面多一段「其他设备上的定时任务」：只读，
 *  可以把其中一个移到本机执行（docs/multi-device-sync.md §8.6）。 */
export default function CronPanel({ agentReady }: { agentReady: boolean }) {
  const [entries, setEntries] = useState<CronEntry[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [loaded, setLoaded] = useState(false);
  // 别的设备上的任务。没开同步、没在同步定时任务时后端给的是空列表。
  const [remote, setRemote] = useState<SyncRemoteCron[]>([]);
  // 正在发接手请求的那一个（`归属设备/任务 id`）；期间其余按钮都先停用。
  const [takingKey, setTakingKey] = useState<string | null>(null);
  const [forceTarget, setForceTarget] = useState<SyncRemoteCron | null>(null);
  // 接手没办成的原因。和 `error` 分开放：接手之后紧跟着要刷新列表，列表读成功会把 `error`
  // 清掉，用户就看不到为什么没办成了。它显示在「其他设备上的定时任务」那一段里，留到用户
  // 关掉或者下一次接手。
  const [takeError, setTakeError] = useState<string | null>(null);

  const refresh = useCallback(async () => {
    try {
      setEntries(await api.cronList());
      setError(null);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoaded(true);
    }
  }, []);

  const refreshRemote = useCallback(async () => {
    try {
      setRemote(await api.syncRemoteCron());
    } catch {
      // 这一段是附带的：读不到就当作没有，不挡本机任务的列表，也不报错打扰。
      setRemote([]);
    }
  }, []);

  useEffect(() => {
    if (!agentReady) return;
    void refresh();
    void refreshRemote();
  }, [agentReady, refresh, refreshRemote]);

  // 同步把任务移进 / 移出了本机（reload cron），或者同步状态变了（别的设备的任务有变化、
  // 接手请求得到了确认、设备被标为已丢失）：重新取。
  useEffect(() => {
    if (!agentReady) return;
    let timer: number | undefined;
    const un = api.onSync((event) => {
      if (event.kind === "reload" && event.what === "cron") {
        void refresh();
        void refreshRemote();
        return;
      }
      if (event.kind !== "status") return;
      // 一轮同步里状态事件会连着来几条：合并成一次查询。
      window.clearTimeout(timer);
      timer = window.setTimeout(() => void refreshRemote(), 300);
    });
    return () => {
      window.clearTimeout(timer);
      void un.then((fn) => fn());
    };
  }, [agentReady, refresh, refreshRemote]);

  const remove = async (id: string) => {
    try {
      await api.cronRemove(id);
      await refresh();
    } catch (e) {
      setError(String(e));
    }
  };

  const takeover = async (job: SyncRemoteCron, force: boolean) => {
    if (takingKey !== null) return;
    setTakingKey(remoteKey(job));
    setTakeError(null);
    try {
      // `job` 是用户点下去时屏幕上的那一份（强制接管是确认框里的那一份）：带上它的版本，
      // 那台设备在这之后改过任务的话后端不办，列表刷新后用户看到新的再决定。
      await api.syncCronTakeover(job.owner, job.id, force, job.version);
    } catch (e) {
      setTakeError(`「${job.name || job.id}」没有移到本机：${String(e)}`);
    } finally {
      setTakingKey(null);
      void refreshRemote();
      void refresh();
    }
  };

  return (
    <div className="h-full overflow-y-auto p-6 max-md:p-3">
      <div className="mb-4 flex items-center justify-between">
        <h1 className="text-base font-semibold text-gray-100">定时任务</h1>
        <button
          onClick={() => {
            void refresh();
            void refreshRemote();
          }}
          className="rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-400 transition hover:border-[#3a4150] hover:text-gray-200"
        >
          刷新
        </button>
      </div>

      <p className="mb-4 rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-xs text-gray-500">
        新增任务无需表单：直接在对话里说「每天早上 8 点提醒我…」，RClaw 会自己调用 cron
        工具登记。这里查看与删除已登记的任务。
      </p>

      {error && (
        <div className="mb-4 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
          {error}
        </div>
      )}
      {!agentReady && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          agent 启动后可用。
        </div>
      )}
      {agentReady && loaded && entries.length === 0 && (
        <div className="rounded-lg border border-[#2a2f3a] bg-[#141821] px-4 py-3 text-sm text-gray-500">
          暂无定时任务。
        </div>
      )}
      <div className="space-y-2">
        {entries.map((e) => (
          <div
            key={e.id}
            className="flex items-center justify-between rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
          >
            <div className="min-w-0">
              <div className="flex items-center gap-2">
                <span className="truncate text-sm font-medium text-gray-200">
                  {e.name || e.id}
                </span>
                <span
                  className={`rounded-full px-2 py-0.5 text-[10px] ${
                    e.enabled
                      ? "bg-emerald-500/10 text-emerald-400"
                      : "bg-gray-500/10 text-gray-500"
                  }`}
                >
                  {e.enabled ? "启用中" : "已停用"}
                </span>
              </div>
              <div className="mt-0.5 truncate text-xs text-gray-500">
                {e.schedule} · {e.kind}
              </div>
              <div className="mt-1 truncate text-xs text-gray-400">{e.message}</div>
            </div>
            <button
              onClick={() => void remove(e.id)}
              className="ml-3 shrink-0 rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-400 transition hover:border-red-500/50 hover:text-red-300"
            >
              删除
            </button>
          </div>
        ))}
      </div>

      {/* 多端同步：其他设备上的定时任务（只读镜像）。本机的调度器不加载它们。
          接手没办成的原因也放在这一段：刷新之后那个任务可能已经不在列表里了，原因照样要看得到。 */}
      {agentReady && (remote.length > 0 || takeError) && (
        <div className="mt-6">
          <h2 className="mb-1 text-sm font-medium text-gray-200">其他设备上的定时任务</h2>
          <p className="mb-3 text-xs leading-relaxed text-gray-500">
            这些任务只在各自的设备上触发，这里只能查看。点「移到本机执行」之后，要等那台设备联网同步一次、
            把任务从它那边移除，本机才开始执行；在那之前任务仍然只在那台设备上触发。
          </p>
          {takeError && (
            <div className="mb-3 flex items-start justify-between gap-3 rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-xs leading-relaxed text-red-300">
              <span className="min-w-0 break-words">{takeError}</span>
              <button
                onClick={() => setTakeError(null)}
                className="shrink-0 rounded-md border border-red-500/40 px-2 py-0.5 text-[11px] text-red-300 transition hover:border-red-500/70"
              >
                知道了
              </button>
            </div>
          )}
          <div className="space-y-2">
            {remote.map((job) => {
              const key = remoteKey(job);
              return (
                <div
                  key={key}
                  className="rounded-xl border border-[#2a2f3a] bg-[#141821] px-4 py-3"
                >
                  <div className="flex items-center gap-2">
                    <span className="min-w-0 truncate text-sm font-medium text-gray-200">
                      {job.name || job.id}
                    </span>
                    <span
                      title={`这个任务在「${job.owner_name}」上执行`}
                      className="max-w-[10rem] shrink-0 truncate rounded-full bg-indigo-500/10 px-2 py-0.5 text-[10px] text-indigo-300"
                    >
                      {job.owner_name}
                    </span>
                    <span
                      className={`shrink-0 rounded-full px-2 py-0.5 text-[10px] ${
                        job.enabled
                          ? "bg-emerald-500/10 text-emerald-400"
                          : "bg-gray-500/10 text-gray-500"
                      }`}
                    >
                      {job.enabled ? "启用中" : "已停用"}
                    </span>
                  </div>
                  <div className="mt-0.5 truncate text-xs text-gray-500">
                    {job.schedule || "这台设备的版本还不认识它的调度方式"}
                  </div>
                  {job.message && (
                    <div className="mt-1 truncate text-xs text-gray-400">{job.message}</div>
                  )}
                  {job.pending && (
                    <div className="mt-2 text-xs leading-relaxed text-amber-300">
                      已经请求移到本机，在等「{job.owner_name}」确认。
                      {job.can_force
                        ? "它很久没出现了（或已被标为丢失）；确定它不会再用的话，可以强制接管。"
                        : "它联网同步一次之后就会完成。"}
                    </div>
                  )}
                  {!job.pending && job.blocked && (
                    <div className="mt-2 text-xs leading-relaxed text-gray-500">
                      不能移到本机：{job.blocked}
                    </div>
                  )}
                  {!job.blocked && (!job.pending || job.can_force) && (
                    <div className="mt-2 flex items-center gap-2">
                      {!job.pending && (
                        <button
                          onClick={() => void takeover(job, false)}
                          disabled={takingKey !== null}
                          className="rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-300 transition hover:border-indigo-500/50 hover:text-indigo-300 disabled:opacity-40"
                        >
                          {takingKey === key ? "请稍候…" : "移到本机执行"}
                        </button>
                      )}
                      {job.can_force && (
                        <button
                          onClick={() => setForceTarget(job)}
                          disabled={takingKey !== null}
                          title="那台设备很久没出现、或已被标为丢失：不等它确认"
                          className="rounded-lg border border-[#2a2f3a] px-3 py-1 text-xs text-gray-400 transition hover:border-red-500/50 hover:text-red-300 disabled:opacity-40"
                        >
                          强制接管
                        </button>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        </div>
      )}

      {/* 强制接管：那台设备其实还在跑的话，任务会两边各触发一次——先说清楚再做。 */}
      {forceTarget && (
        <SyncConfirm
          title="强制接管定时任务"
          confirmText="强制接管"
          danger
          onConfirm={() => {
            const job = forceTarget;
            setForceTarget(null);
            void takeover(job, true);
          }}
          onCancel={() => setForceTarget(null)}
        >
          <p>
            把「{forceTarget.name || forceTarget.id}」从「{forceTarget.owner_name}
            」移到这台设备执行，不等那台设备确认。
          </p>
          <p>
            只在那台设备已经丢失、或者确定不会再用时这样做。
            如果它其实还在离线运行，这个任务会在两台设备上各触发一次，直到它重新联网、看到接管记录为止。
          </p>
        </SyncConfirm>
      )}
    </div>
  );
}
