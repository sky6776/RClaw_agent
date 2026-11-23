import { useEffect, useState } from "react";
import { api, type SyncPendingItem } from "../lib/api";
import {
  SYNC_BTN_PRIMARY,
  SYNC_BTN_SMALL,
  SYNC_CARD,
  SYNC_ERROR_BOX,
  SYNC_HINT,
  SYNC_WARN_BOX,
  formatAgo,
  formatBytes,
} from "../lib/syncUi";
import { SyncCardHead, SyncConfirm } from "./SyncBits";

/** 要在本机看过差异、确认之后才写入的三类远端改动（§8.3–§8.5）。 */
const REVIEW_KINDS = ["persona", "skill", "bot"];

/** 记忆文件里原有的条目都不见了：「接受」= 是有意删的，把删除同步到所有设备。 */
const KIND_MEM_WIPE = "mem-wipe";

const ACTION_LABEL: Record<string, string> = {
  accept: "接受",
  reject: "拒绝",
  retry: "重试",
  discard: "丢弃",
  dismiss: "知道了",
};

type DiffLine = { kind: "same" | "add" | "del" | "skip"; text: string };

/** 逐行对比（最长公共子序列）。两边太长时返回 null，调用方改成分开显示。 */
function diffLines(before: string, after: string): DiffLine[] | null {
  const a = before.split("\n");
  const b = after.split("\n");
  const n = a.length;
  const m = b.length;
  if (n * m > 400000) return null;
  const width = m + 1;
  const table = new Uint16Array((n + 1) * width);
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      table[i * width + j] =
        a[i] === b[j]
          ? table[(i + 1) * width + j + 1] + 1
          : Math.max(table[(i + 1) * width + j], table[i * width + j + 1]);
    }
  }
  const out: DiffLine[] = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (a[i] === b[j]) {
      out.push({ kind: "same", text: a[i] });
      i++;
      j++;
    } else if (table[(i + 1) * width + j] >= table[i * width + j + 1]) {
      out.push({ kind: "del", text: a[i] });
      i++;
    } else {
      out.push({ kind: "add", text: b[j] });
      j++;
    }
  }
  while (i < n) out.push({ kind: "del", text: a[i++] });
  while (j < m) out.push({ kind: "add", text: b[j++] });
  return out;
}

/** 大段没变的内容只留改动附近的几行。 */
function collapseSame(lines: DiffLine[]): DiffLine[] {
  const keep = 3;
  const out: DiffLine[] = [];
  let start = 0;
  while (start < lines.length) {
    if (lines[start].kind !== "same") {
      out.push(lines[start]);
      start++;
      continue;
    }
    let end = start;
    while (end < lines.length && lines[end].kind === "same") end++;
    const run = end - start;
    const head = start === 0 ? 0 : keep;
    const tail = end === lines.length ? 0 : keep;
    if (run <= head + tail + 2) {
      for (let k = start; k < end; k++) out.push(lines[k]);
    } else {
      for (let k = start; k < start + head; k++) out.push(lines[k]);
      out.push({ kind: "skip", text: `… ${run - head - tail} 行没有变化 …` });
      for (let k = end - tail; k < end; k++) out.push(lines[k]);
    }
    start = end;
  }
  return out;
}

const LINE_STYLE: Record<DiffLine["kind"], string> = {
  same: "text-gray-400",
  add: "bg-emerald-500/10 text-emerald-300",
  del: "bg-red-500/10 text-red-300",
  skip: "text-gray-600",
};
const LINE_MARK: Record<DiffLine["kind"], string> = { same: " ", add: "+", del: "-", skip: " " };

function Lines({ lines }: { lines: DiffLine[] }) {
  return (
    <div className="max-h-72 overflow-auto rounded-md border border-[#2a2f3a] bg-[#0f131b] py-1 font-mono text-[11px] leading-relaxed">
      {lines.map((line, i) => (
        <div key={i} className={`whitespace-pre-wrap break-words px-2 ${LINE_STYLE[line.kind]}`}>
          <span className="select-none">{LINE_MARK[line.kind]} </span>
          {line.text}
        </div>
      ))}
    </div>
  );
}

function wholeText(text: string, kind: DiffLine["kind"]): DiffLine[] {
  return text.split("\n").map((line) => ({ kind, text: line }));
}

/** 一份文本在本机与对方之间的差异。`before` 为 null = 本机没有；`after` 为 null = 对方删了。 */
function TextDiff({ before, after }: { before: string | null; after: string | null }) {
  if (before === null && after === null) {
    return <div className={SYNC_HINT}>这个文件的内容没法在这里显示（不是文本，或者太大）。</div>;
  }
  if (before === null) {
    return (
      <div className="space-y-1">
        <div className={SYNC_HINT}>这台设备上没有它，接受后新建：</div>
        <Lines lines={wholeText(after || "", "add")} />
      </div>
    );
  }
  if (after === null) {
    return (
      <div className="space-y-1">
        <div className={SYNC_HINT}>接受后，这台设备上的这一份会被去掉：</div>
        <Lines lines={wholeText(before, "del")} />
      </div>
    );
  }
  if (before === after) {
    return <div className={SYNC_HINT}>内容与这台设备上的相同。</div>;
  }
  const lines = diffLines(before, after);
  if (!lines) {
    return (
      <div className="space-y-1">
        <div className={SYNC_HINT}>内容较长，分开显示。这台设备上现在的：</div>
        <Lines lines={wholeText(before, "del")} />
        <div className={SYNC_HINT}>对方的版本：</div>
        <Lines lines={wholeText(after, "add")} />
      </div>
    );
  }
  return (
    <div className="space-y-1">
      <div className={SYNC_HINT}>红色（-）是这台设备上现在的，绿色（+）是对方的版本。</div>
      <Lines lines={collapseSame(lines)} />
    </div>
  );
}

function asRecord(value: unknown): Record<string, unknown> {
  return value && typeof value === "object" ? (value as Record<string, unknown>) : {};
}

function asText(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

const SKILL_STATUS: Record<string, { text: string; tone: string }> = {
  added: { text: "新增", tone: "text-emerald-400" },
  modified: { text: "修改", tone: "text-amber-300" },
  removed: { text: "去掉", tone: "text-red-400" },
};

/** 技能版本的文件清单：哪些新增、修改、去掉，哪些是可执行的，能看文本的点开看。 */
function SkillDetail({ detail }: { detail: Record<string, unknown> }) {
  const [open, setOpen] = useState<string | null>(null);
  const files = Array.isArray(detail.files) ? detail.files.map(asRecord) : [];
  const unchanged = typeof detail.unchanged === "number" ? detail.unchanged : 0;
  const deleting = detail.op === "del";
  return (
    <div className="space-y-2">
      {deleting && <div className={SYNC_WARN_BOX}>对方删除了这个技能。接受后，这台设备上的也会删除（留有归档）。</div>}
      {files.length === 0 && <div className={SYNC_HINT}>没有文件改动。</div>}
      {files.map((file) => {
        const path = asText(file.path) || "";
        const status = SKILL_STATUS[asText(file.status) || ""] || { text: "改动", tone: "text-gray-400" };
        const local = asText(file.local);
        const remote = asText(file.remote);
        const viewable = local !== null || remote !== null;
        return (
          <div key={path} className="rounded-md border border-[#2a2f3a] px-2 py-1.5">
            <div className="flex items-center gap-2 text-[11px]">
              <span className={`shrink-0 ${status.tone}`}>{status.text}</span>
              <span className="min-w-0 flex-1 break-all font-mono text-gray-300">{path}</span>
              {file.exec === true && <span className="shrink-0 text-amber-300">可执行</span>}
              <span className="shrink-0 text-gray-600">
                {formatBytes(typeof file.size === "number" ? file.size : 0)}
              </span>
              {viewable && (
                <button
                  onClick={() => setOpen(open === path ? null : path)}
                  className="shrink-0 text-indigo-300 transition hover:text-indigo-200"
                >
                  {open === path ? "收起" : "查看"}
                </button>
              )}
            </div>
            {open === path && (
              <div className="mt-1.5">
                <TextDiff
                  before={file.status === "added" ? null : local}
                  after={file.status === "removed" ? null : remote}
                />
              </div>
            )}
          </div>
        );
      })}
      {unchanged > 0 && <div className={SYNC_HINT}>另有 {unchanged} 个文件没有变化。</div>}
    </div>
  );
}

function ReviewDetail({ kind, detail }: { kind: string; detail: unknown }) {
  const record = asRecord(detail);
  if (kind === "skill") return <SkillDetail detail={record} />;
  if (kind === "bot") {
    return (
      <div className="space-y-2">
        {Boolean(record.relaxed) && (
          <div className={SYNC_WARN_BOX}>
            {record.recreated === true
              ? "这个智能体之前被别的设备删掉了，现在又建了出来，而且比删掉之前少禁了一些工具：接受后，它在这台设备上能用的工具会比原来多。"
              : "这份定义放宽了工具限制：接受后，这个智能体在这台设备上能用的工具会变多。"}
          </div>
        )}
        <TextDiff before={asText(record.local)} after={asText(record.remote)} />
      </div>
    );
  }
  const remote = asText(record.remote);
  return (
    <div className="space-y-2">
      {remote === null && (
        <div className={SYNC_WARN_BOX}>对方删除了这个文件。接受后，这台设备上的也会删除（留有副本）。</div>
      )}
      <TextDiff before={asText(record.local)} after={remote} />
    </div>
  );
}

/** 接受之后要告诉用户的话（事项本身随即从列表里消失）。 */
const ACCEPTED_NOTE: Record<string, string> = {
  persona: "人格已经写入这台设备，下次启动 RClaw 时生效。",
  skill: "已经确认。这个技能版本会在下一轮同步时装上；有任务在跑的话，等它结束再装。",
  bot: "智能体定义已经写入这台设备。",
};

function PendingCard({
  item,
  onError,
  onNotice,
  onChanged,
}: {
  item: SyncPendingItem;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
  onChanged: () => void;
}) {
  const [busy, setBusy] = useState(false);
  const [detail, setDetail] = useState<unknown>(undefined);
  const [detailError, setDetailError] = useState<string | null>(null);
  const [loading, setLoading] = useState(false);
  const [confirmDiscard, setConfirmDiscard] = useState(false);
  const [confirmWipe, setConfirmWipe] = useState<{ version: string | null } | null>(null);
  // 展开着的详情被更新的一版顶掉了。
  const [outdated, setOutdated] = useState(false);
  const review = REVIEW_KINDS.indexOf(item.kind) >= 0 && item.actions.indexOf("accept") >= 0;
  const opened = detail !== undefined;
  // 等确认的改动是哪一版。事项的标识不随版本变：同一张卡片下面，内容可能已经换了一版。
  const itemVersion = asText(asRecord(item.payload).hlc);
  const shownVersion = opened ? asText(asRecord(detail).hlc) : null;

  useEffect(() => {
    // 换了一版：展开着的是旧内容，收起来，让用户重新查看。
    if (shownVersion !== null && shownVersion !== itemVersion) {
      setDetail(undefined);
      setOutdated(true);
    }
  }, [itemVersion, shownVersion]);

  const resolve = async (action: string, version = shownVersion ?? itemVersion) => {
    setBusy(true);
    try {
      // 带上屏幕上的那一版：展开了详情就是详情的版本（「接受」只在展开之后才点得到），
      // 没展开就是卡片上的。这期间又来了更新的一版的话，后端不办。
      await api.syncResolve(item.id, action, version);
      if (action === "accept" && ACCEPTED_NOTE[item.kind]) onNotice(ACCEPTED_NOTE[item.kind]);
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
      setConfirmDiscard(false);
      setConfirmWipe(null);
      onChanged();
    }
  };

  const view = async () => {
    setLoading(true);
    setDetailError(null);
    setOutdated(false);
    try {
      setDetail(await api.syncPendingDetail(item.id));
    } catch (e) {
      setDetailError(String(e));
      // 这项改动可能已经被更新的版本顶掉了：让面板刷新一次。
      onChanged();
    } finally {
      setLoading(false);
    }
  };

  const meta = [item.origin ? `来自「${item.origin}」` : "", item.at_ms > 0 ? formatAgo(item.at_ms) : ""]
    .filter(Boolean)
    .join(" · ");

  return (
    <div className="rounded-lg border border-[#2a2f3a] px-3 py-2.5">
      <div className="text-xs font-medium text-gray-200">{item.title}</div>
      {item.detail && (
        <div className="mt-1 whitespace-pre-wrap text-[11px] leading-relaxed text-gray-400">
          {item.detail}
        </div>
      )}
      {meta && <div className="mt-1 text-[10px] text-gray-600">{meta}</div>}
      {detailError && <div className={`mt-2 ${SYNC_ERROR_BOX}`}>{detailError}</div>}
      {outdated && !opened && (
        <div className={`mt-2 ${SYNC_WARN_BOX}`}>这项改动又有了更新的版本，请重新查看。</div>
      )}
      {review && opened && (
        <div className="mt-2">
          <ReviewDetail kind={item.kind} detail={detail} />
        </div>
      )}
      {(review || item.actions.length > 0) && (
        <div className="mt-2 flex items-center gap-2">
          {review ? (
            <>
              {opened ? (
                <button
                  disabled={busy}
                  onClick={() => void resolve("accept")}
                  className={SYNC_BTN_PRIMARY}
                >
                  {busy ? "请稍候…" : "接受这个版本"}
                </button>
              ) : (
                <button disabled={loading} onClick={() => void view()} className={SYNC_BTN_PRIMARY}>
                  {loading ? "读取中…" : "查看改动"}
                </button>
              )}
              {item.actions.indexOf("reject") >= 0 && (
                <button disabled={busy} onClick={() => void resolve("reject")} className={SYNC_BTN_SMALL}>
                  拒绝
                </button>
              )}
              {opened && (
                <button onClick={() => setDetail(undefined)} className={SYNC_BTN_SMALL}>
                  收起
                </button>
              )}
            </>
          ) : (
            item.actions.map((action) => (
              <button
                key={action}
                disabled={busy}
                onClick={() => {
                  if (action === "discard") setConfirmDiscard(true);
                  else if (action === "accept" && item.kind === KIND_MEM_WIPE) {
                    setConfirmWipe({ version: shownVersion ?? itemVersion });
                  }
                  else void resolve(action);
                }}
                className={SYNC_BTN_SMALL}
              >
                {ACTION_LABEL[action] || action}
              </button>
            ))
          )}
        </div>
      )}
      {confirmDiscard && (
        <SyncConfirm
          title="丢弃没合入的内容"
          confirmText="丢弃"
          danger
          busy={busy}
          onConfirm={() => void resolve("discard")}
          onCancel={() => setConfirmDiscard(false)}
        >
          <p>这个会话留着没有合入的那些同步数据会被丢掉，之后不会再同步过来。这台设备上已有的内容不受影响。</p>
        </SyncConfirm>
      )}
      {confirmWipe && (
        <SyncConfirm
          title="在所有设备上删除这些记忆"
          confirmText="删除"
          danger
          busy={busy}
          onConfirm={() => void resolve("accept", confirmWipe.version)}
          onCancel={() => setConfirmWipe(null)}
        >
          <p>
            这些记忆条目会从所有设备上删除。其他设备各自在回收站里留一份，过了保留期就找不回来了。
            不是有意删的话，请取消，改选「拒绝」把它们恢复到这台设备上。
          </p>
        </SyncConfirm>
      )}
    </div>
  );
}

/** 待处理事项：要在本机确认的远端改动、被拦下的内容、只读的提示。没有时不占地方。 */
export default function SyncPending({
  items,
  onError,
  onNotice,
  onChanged,
}: {
  items: SyncPendingItem[];
  onError: (message: string) => void;
  /** 要留在面板上的提示（不是一闪而过的那种）。 */
  onNotice: (message: string) => void;
  onChanged: () => void;
}) {
  if (items.length === 0) return null;
  return (
    <div className={SYNC_CARD}>
      <SyncCardHead title={`待处理事项（${items.length}）`} />
      <div className="space-y-2">
        {items.map((item) => (
          <PendingCard
            key={item.id}
            item={item}
            onError={onError}
            onNotice={onNotice}
            onChanged={onChanged}
          />
        ))}
      </div>
    </div>
  );
}
