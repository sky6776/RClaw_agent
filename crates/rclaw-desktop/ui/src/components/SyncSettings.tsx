import { useState } from "react";
import { api, type SyncConfig, type SyncStatus } from "../lib/api";
import {
  SYNC_BTN,
  SYNC_BTN_PRIMARY,
  SYNC_BTN_SMALL,
  SYNC_CARD,
  SYNC_ERROR_BOX,
  SYNC_HINT,
  SYNC_INPUT,
  SYNC_LABEL,
  SYNC_SCOPES,
  storageProblem,
  storageSummary,
} from "../lib/syncUi";
import { SyncCardHead, SyncCheck } from "./SyncBits";
import SyncStorageFields from "./SyncStorageFields";

type CardProps = {
  config: SyncConfig;
  status: SyncStatus;
  onError: (message: string) => void;
  onChanged: () => void;
};

/** 同步范围：勾一下立即保存。附件跟着会话走。 */
function ScopesCard({ config, onError, onChanged }: CardProps) {
  const [busy, setBusy] = useState(false);

  const toggle = async (name: string, on: boolean) => {
    const wanted = config.scopes.filter((scope) => scope !== name);
    if (on) wanted.push(name);
    if (name === "attachments" && on && wanted.indexOf("sessions") < 0) wanted.push("sessions");
    const pruned =
      name === "sessions" && !on ? wanted.filter((scope) => scope !== "attachments") : wanted;
    const scopes = SYNC_SCOPES.map((scope) => scope.name).filter(
      (scope) => pruned.indexOf(scope) >= 0,
    );
    setBusy(true);
    try {
      await api.syncSaveConfig({ ...config, scopes });
    } catch (e) {
      onError(String(e));
    } finally {
      setBusy(false);
      onChanged();
    }
  };

  return (
    <div className={SYNC_CARD}>
      <SyncCardHead title="同步哪些内容" />
      <div className="space-y-2.5">
        {SYNC_SCOPES.map((scope) => (
          <SyncCheck
            key={scope.name}
            checked={config.scopes.indexOf(scope.name) >= 0}
            disabled={busy}
            onChange={(on) => void toggle(scope.name, on)}
            label={scope.label}
            hint={scope.hint}
          />
        ))}
      </div>
      <p className={`mt-3 ${SYNC_HINT}`}>
        每台设备各自选。关掉某一项，这台设备就不再收发它；已经在这台设备上的内容不会被删除。API
        Key、权限设置、rclaw.toml 永远不同步。
      </p>
    </div>
  );
}

/** 存储：平时只显示在用哪个；点「修改」才展开表单。 */
function StorageCard({ config, status, onChanged }: CardProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState<SyncConfig>(config);
  const [secret, setSecret] = useState("");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const needsSecret = config.transport !== "file";

  const open = () => {
    setDraft(config);
    setSecret("");
    setError(null);
    setEditing(true);
  };

  const save = async () => {
    const problem = storageProblem(draft);
    if (problem) {
      setError(problem);
      return;
    }
    setBusy(true);
    setError(null);
    try {
      // 只带走存储这几段：别处（范围、设备名）在这期间改过的不被这份草稿盖回去。
      await api.syncSaveConfig({
        ...config,
        transport: draft.transport,
        webdav: draft.webdav,
        file: draft.file,
        s3: draft.s3,
      });
      if (secret && draft.transport !== "file") await api.syncSetSecret(secret);
      setSecret("");
      setEditing(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
      onChanged();
    }
  };

  return (
    <div className={SYNC_CARD}>
      <SyncCardHead title="存储">
        {!editing && (
          <button onClick={open} className={SYNC_BTN_SMALL}>
            修改
          </button>
        )}
      </SyncCardHead>
      {!editing && (
        <div className="space-y-1">
          <div className="break-all text-xs text-gray-300">{storageSummary(config)}</div>
          {needsSecret && (
            <div className={status.secret_saved ? SYNC_HINT : "text-[11px] text-amber-300"}>
              {status.secret_saved
                ? "密码已经存在这台设备上。"
                : "这台设备上还没有存这个存储的密码：点「修改」填上。"}
            </div>
          )}
          <div className={SYNC_HINT}>
            数据在这台设备上加密之后才传出去，存储方只能看到密文的数量、大小和时间。
          </div>
        </div>
      )}
      {editing && (
        <div className="space-y-3">
          <p className={SYNC_HINT}>
            改地址不会搬数据：新地址上要有这个同步空间原来的数据（比如服务器换了域名、目录挪了位置）。要换到一个全新的存储，请先「退出同步空间」，再在新存储上创建。只换密码的话，填上新密码保存即可。
          </p>
          <SyncStorageFields
            config={draft}
            onChange={setDraft}
            secret={secret}
            onSecret={setSecret}
            secretSaved={status.secret_saved && status.transport === draft.transport}
            disabled={busy}
          />
          {error && <div className={SYNC_ERROR_BOX}>{error}</div>}
          <div className="flex items-center gap-2">
            <button disabled={busy} onClick={() => void save()} className={SYNC_BTN_PRIMARY}>
              {busy ? "正在保存…" : "保存"}
            </button>
            <button disabled={busy} onClick={() => setEditing(false)} className={SYNC_BTN}>
              取消
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

type NumberField = {
  key: "poll_interval_sec" | "background_interval_sec" | "presend_pull_wait_ms" | "attachment_max_mb" | "trash_days";
  label: string;
  hint: string;
  min: number;
};

const NUMBER_FIELDS: NumberField[] = [
  {
    key: "poll_interval_sec",
    label: "检查间隔（秒）",
    hint: "应用开着时，每隔这么久看一次别的设备有没有新内容。这台设备上有改动时会马上同步，不等它。最小 5。",
    min: 5,
  },
  {
    key: "background_interval_sec",
    label: "后台检查间隔（秒）",
    hint: "手机上应用退到后台、并且连的是不计流量的网络时，每隔这么久同步一次。最小 60。",
    min: 60,
  },
  {
    key: "presend_pull_wait_ms",
    label: "发消息前最多等多久（毫秒）",
    hint: "发消息前先看一眼别的设备有没有续写这个会话，最多等这么久；距上次检查不到 5 秒时不等。填 0 关闭。",
    min: 0,
  },
  {
    key: "attachment_max_mb",
    label: "附件大小上限（MB）",
    hint: "比这更大的附件不上传。",
    min: 1,
  },
  {
    key: "trash_days",
    label: "回收站保留天数",
    hint: "被同步删掉的会话和记忆条目，在这台设备的回收站里留这么多天。",
    min: 1,
  },
];

/** 设备名与几个数值设置。默认收起。 */
function AdvancedCard({ config, status, onChanged }: CardProps) {
  const [open, setOpen] = useState(false);
  const [name, setName] = useState(config.device_name);
  const [values, setValues] = useState<Record<string, string>>({});
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const expand = () => {
    const initial: Record<string, string> = {};
    for (const field of NUMBER_FIELDS) initial[field.key] = String(config[field.key]);
    setValues(initial);
    setName(config.device_name);
    setError(null);
    setOpen(true);
  };

  const save = async () => {
    const next: SyncConfig = { ...config, device_name: name.trim() };
    for (const field of NUMBER_FIELDS) {
      const raw = (values[field.key] || "").trim();
      const parsed = Number(raw);
      if (!raw || !Number.isInteger(parsed) || parsed < field.min) {
        setError(`「${field.label}」要填不小于 ${field.min} 的整数。`);
        return;
      }
      next[field.key] = parsed;
    }
    setBusy(true);
    setError(null);
    try {
      await api.syncSaveConfig(next);
      setOpen(false);
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
      onChanged();
    }
  };

  return (
    <div className={SYNC_CARD}>
      <SyncCardHead title="设备名与高级设置">
        {!open && (
          <button onClick={expand} className={SYNC_BTN_SMALL}>
            修改
          </button>
        )}
      </SyncCardHead>
      {!open && (
        <div className={SYNC_HINT}>
          这台设备叫「{status.device_name}」。每 {config.poll_interval_sec} 秒检查一次；发消息前
          {config.presend_pull_wait_ms > 0
            ? `最多等 ${config.presend_pull_wait_ms} 毫秒`
            : "不等别的设备"}
          ；附件上限 {config.attachment_max_mb} MB；回收站保留 {config.trash_days} 天。
        </div>
      )}
      {open && (
        <div className="space-y-3">
          <div>
            <label className={SYNC_LABEL}>这台设备的名字</label>
            <input
              type="text"
              value={name}
              disabled={busy}
              maxLength={40}
              placeholder={status.device_name}
              onChange={(e) => setName(e.target.value)}
              className={SYNC_INPUT}
            />
          </div>
          {NUMBER_FIELDS.map((field) => (
            <div key={field.key}>
              <label className={SYNC_LABEL}>{field.label}</label>
              <input
                type="number"
                inputMode="numeric"
                min={field.min}
                step={1}
                value={values[field.key] || ""}
                disabled={busy}
                onChange={(e) => setValues({ ...values, [field.key]: e.target.value })}
                className={SYNC_INPUT}
              />
              <p className={`mt-1 ${SYNC_HINT}`}>{field.hint}</p>
            </div>
          ))}
          {error && <div className={SYNC_ERROR_BOX}>{error}</div>}
          <div className="flex items-center gap-2">
            <button disabled={busy} onClick={() => void save()} className={SYNC_BTN_PRIMARY}>
              {busy ? "正在保存…" : "保存"}
            </button>
            <button disabled={busy} onClick={() => setOpen(false)} className={SYNC_BTN}>
              取消
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

/** 「多端同步」面板里的设置部分：范围、存储、设备名与数值。 */
export default function SyncSettings(props: CardProps) {
  return (
    <>
      <ScopesCard {...props} />
      <StorageCard {...props} />
      <AdvancedCard {...props} />
    </>
  );
}
