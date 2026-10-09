import { useRef, useState } from "react";
import { api, PROVIDER_PRESETS, type WizardInput } from "../lib/api";
import { isAndroidDevice } from "../lib/useIsMobile";

/** 首运行配置向导：生成 rclaw.toml 并启动 agent。 */
export default function Wizard({ home, onDone }: { home: string; onDone: () => void }) {
  const [preset, setPreset] = useState<string>("zhipu");
  const [provider, setProvider] = useState("zhipu");
  const [baseUrl, setBaseUrl] = useState(PROVIDER_PRESETS.zhipu.baseUrl);
  const [model, setModel] = useState(PROVIDER_PRESETS.zhipu.model);
  const [contextLength, setContextLength] = useState(PROVIDER_PRESETS.zhipu.contextLength);
  const [apiKey, setApiKey] = useState("");
  const [autoEvolve, setAutoEvolve] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [submitting, setSubmitting] = useState(false);
  // 安卓 App 专属：从手机存储导入现成的 rclaw.toml（桌面无此入口）。
  const android = isAndroidDevice();
  const fileRef = useRef<HTMLInputElement>(null);
  const [importing, setImporting] = useState(false);

  const importFile = async (file: File) => {
    setError(null);
    setImporting(true);
    try {
      await api.configImportRaw(await file.text());
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setImporting(false);
    }
  };

  const applyPreset = (key: string) => {
    setPreset(key);
    const p = PROVIDER_PRESETS[key];
    if (!p) return;
    setProvider(key === "custom" ? "custom" : key);
    setBaseUrl(p.baseUrl);
    setModel(p.model);
    setContextLength(p.contextLength);
  };

  const submit = async () => {
    setError(null);
    setSubmitting(true);
    try {
      const input: WizardInput = {
        provider,
        baseUrl,
        model,
        apiKey,
        contextLength,
        autoEvolve,
      };
      await api.wizardSave(input);
      onDone();
    } catch (e) {
      setError(String(e));
    } finally {
      setSubmitting(false);
    }
  };

  const field =
    "w-full rounded-lg border border-[#2a2f3a] bg-[#161a22] px-3 py-2 text-sm text-gray-200 outline-none focus:border-indigo-500";

  return (
    <div className="flex h-full items-center justify-center overflow-y-auto p-6 max-md:p-4">
      <div className="w-full max-w-xl rounded-2xl border border-[#2a2f3a] bg-[#141821] p-8 shadow-2xl">
        <div className="mb-1 flex items-center gap-3">
          <div className="flex h-9 w-9 items-center justify-center rounded-lg bg-indigo-600 text-sm font-bold text-white">
            R
          </div>
          <h1 className="text-lg font-semibold text-gray-100">欢迎使用 RClaw</h1>
        </div>
        <p className="mb-6 text-sm text-gray-400">
          首次使用需要配置一个 OpenAI 兼容的大模型服务。数据将保存在：
          <span className="ml-1 font-mono text-xs text-gray-500">{home}</span>
        </p>

        <div className="space-y-4">
          <div>
            <label className="mb-1 block text-xs font-medium text-gray-400">服务商预设</label>
            <div className="flex gap-2">
              {Object.entries(PROVIDER_PRESETS).map(([key, p]) => (
                <button
                  key={key}
                  onClick={() => applyPreset(key)}
                  className={`flex-1 rounded-lg border px-3 py-2 text-sm transition ${
                    preset === key
                      ? "border-indigo-500 bg-indigo-500/10 text-indigo-300"
                      : "border-[#2a2f3a] bg-[#161a22] text-gray-400 hover:border-[#3a4150]"
                  }`}
                >
                  {p.label}
                </button>
              ))}
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-gray-400">API 端点</label>
              <input
                className={field}
                value={baseUrl}
                onChange={(e) => setBaseUrl(e.target.value)}
                placeholder="https://..."
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-gray-400">模型</label>
              <input
                className={field}
                value={model}
                onChange={(e) => setModel(e.target.value)}
                placeholder="glm-5.3-flash"
              />
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div>
              <label className="mb-1 block text-xs font-medium text-gray-400">API Key</label>
              <input
                className={field}
                type="password"
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                placeholder="sk-..."
              />
            </div>
            <div>
              <label className="mb-1 block text-xs font-medium text-gray-400">
                上下文窗口（token）
              </label>
              <input
                className={field}
                type="number"
                value={contextLength}
                onChange={(e) => setContextLength(Number(e.target.value) || 0)}
              />
            </div>
          </div>

          <label className="flex cursor-pointer items-start gap-3 rounded-lg border border-[#2a2f3a] bg-[#161a22] p-3">
            <input
              type="checkbox"
              checked={autoEvolve}
              onChange={(e) => setAutoEvolve(e.target.checked)}
              className="mt-0.5 h-4 w-4 accent-indigo-500"
            />
            <span className="text-sm text-gray-300">
              开启自进化（推荐）
              <span className="mt-0.5 block text-xs text-gray-500">
                RClaw 会把你的偏好沉淀进长期记忆、把重复工作固化成可复用技能，并定期整理技能库
                —— 它会随使用越来越懂你。
              </span>
              <span className="mt-1 block text-xs text-amber-500/80">
                注意：自进化的后台整理会额外调用模型，消耗更多 API 额度；可随时在
                rclaw.toml 中关闭。
              </span>
            </span>
          </label>

          {error && (
            <div className="rounded-lg border border-red-500/40 bg-red-500/10 px-3 py-2 text-sm text-red-300">
              {error}
            </div>
          )}

          <button
            onClick={submit}
            disabled={submitting || importing}
            className="w-full rounded-lg bg-indigo-600 py-2.5 text-sm font-medium text-white transition hover:bg-indigo-500 disabled:opacity-50"
          >
            {submitting ? "正在启动…" : "完成配置并启动"}
          </button>

          {android && (
            <div className="flex items-center justify-center gap-2 text-xs text-gray-500">
              <span>已有完整配置？</span>
              <button
                onClick={() => fileRef.current?.click()}
                disabled={importing}
                className="text-indigo-400 underline-offset-2 transition hover:text-indigo-300 hover:underline disabled:opacity-50"
              >
                {importing ? "导入中…" : "从文件导入 rclaw.toml"}
              </button>
              <input
                ref={fileRef}
                type="file"
                accept=".toml,text/plain,application/toml"
                className="hidden"
                onChange={(e) => {
                  const f = e.target.files?.[0];
                  e.target.value = "";
                  if (f) void importFile(f);
                }}
              />
            </div>
          )}
        </div>
      </div>
    </div>
  );
}
