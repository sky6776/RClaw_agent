import { useState } from "react";
import { api, type SyncConfig, type SyncSelfTestStep } from "../lib/api";
import {
  SYNC_BTN,
  SYNC_BTN_SMALL,
  SYNC_ERROR_BOX,
  SYNC_HINT,
  SYNC_INPUT,
  SYNC_LABEL,
  SYNC_WARN_BOX,
  secretOrNull,
  storageProblem,
} from "../lib/syncUi";
import { SyncCheck } from "./SyncBits";

const TRANSPORTS: Array<{ value: string; label: string }> = [
  { value: "webdav", label: "WebDAV（网盘、NAS 或自建文件服务器）" },
  { value: "file", label: "共享文件夹（本机能直接读写的目录）" },
  { value: "s3", label: "S3 兼容对象存储" },
];

function isPlainHttp(url: string): boolean {
  return /^http:\/\//i.test(url.trim());
}

function isHttps(url: string): boolean {
  return /^https:\/\//i.test(url.trim());
}

/**
 * 存储设置表单 +「测试连接」。创建空间、加入空间（配对码不带存储设置时）和修改存储共用。
 *
 * 密码不在 `config` 里：它只存在 `<数据根>/sync/secrets.json`，这里单独收，由调用方连同
 * 设置一起交给后端。留空 = 不动本机已经存着的那一个。
 */
export default function SyncStorageFields({
  config,
  onChange,
  secret,
  onSecret,
  secretSaved,
  disabled = false,
}: {
  config: SyncConfig;
  onChange: (next: SyncConfig) => void;
  secret: string;
  onSecret: (next: string) => void;
  /** 本机存着这种存储的密码。 */
  secretSaved: boolean;
  disabled?: boolean;
}) {
  const [testing, setTesting] = useState(false);
  const [steps, setSteps] = useState<SyncSelfTestStep[] | null>(null);
  const [testError, setTestError] = useState<string | null>(null);
  const [probing, setProbing] = useState(false);
  const [probed, setProbed] = useState<string | null>(null);
  const [probeError, setProbeError] = useState<string | null>(null);

  // 设置一改，上一次测试和上一次取到的证书指纹就不算数了。
  const stale = () => {
    setSteps(null);
    setTestError(null);
  };
  const change = (next: SyncConfig) => {
    stale();
    onChange(next);
  };
  const setWebdav = (patch: Partial<SyncConfig["webdav"]>) => {
    if (patch.url !== undefined) {
      setProbed(null);
      setProbeError(null);
    }
    change({ ...config, webdav: { ...config.webdav, ...patch } });
  };
  const setS3 = (patch: Partial<SyncConfig["s3"]>) =>
    change({ ...config, s3: { ...config.s3, ...patch } });

  const test = async () => {
    const problem = storageProblem(config);
    if (problem) {
      setSteps(null);
      setTestError(problem);
      return;
    }
    setTesting(true);
    stale();
    try {
      setSteps(await api.syncTestConnection(config, secretOrNull(secret)));
    } catch (e) {
      setTestError(String(e));
    } finally {
      setTesting(false);
    }
  };

  const probe = async () => {
    setProbing(true);
    setProbed(null);
    setProbeError(null);
    try {
      setProbed(await api.syncProbeCertificate(config.webdav.url));
    } catch (e) {
      setProbeError(String(e));
    } finally {
      setProbing(false);
    }
  };

  const secretPlaceholder = secretSaved ? "已经存在这台设备上；留空 = 不改" : "";
  const failed = steps ? steps.filter((step) => !step.ok) : [];

  return (
    <div className="space-y-3">
      <div>
        <label className={SYNC_LABEL}>存储类型</label>
        <select
          value={config.transport}
          disabled={disabled}
          onChange={(e) => change({ ...config, transport: e.target.value })}
          className={SYNC_INPUT}
        >
          {!TRANSPORTS.some((t) => t.value === config.transport) && (
            <option value={config.transport}>{config.transport || "（空）"}</option>
          )}
          {TRANSPORTS.map((t) => (
            <option key={t.value} value={t.value}>
              {t.label}
            </option>
          ))}
        </select>
      </div>

      {config.transport === "webdav" && (
        <>
          <div>
            <label className={SYNC_LABEL}>WebDAV 地址</label>
            <input
              type="text"
              value={config.webdav.url}
              disabled={disabled}
              placeholder="https://dav.example.com/dav/rclaw/"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              onChange={(e) => setWebdav({ url: e.target.value })}
              className={SYNC_INPUT}
            />
            <p className={`mt-1 ${SYNC_HINT}`}>
              填到一个专门给 RClaw 用的目录。各台设备要填同一个地址。公网地址必须是 https。
            </p>
          </div>
          <div>
            <label className={SYNC_LABEL}>用户名</label>
            <input
              type="text"
              value={config.webdav.username}
              disabled={disabled}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setWebdav({ username: e.target.value })}
              className={SYNC_INPUT}
            />
          </div>
          <div>
            <label className={SYNC_LABEL}>密码</label>
            <input
              type="password"
              value={secret}
              disabled={disabled}
              placeholder={secretPlaceholder}
              autoComplete="new-password"
              onChange={(e) => {
                stale();
                onSecret(e.target.value);
              }}
              className={SYNC_INPUT}
            />
            <p className={`mt-1 ${SYNC_HINT}`}>
              网盘一般要用「应用专用密码」，不是登录密码。密码只存在这台设备上，不写进
              rclaw.toml。
            </p>
          </div>
          {isPlainHttp(config.webdav.url) && (
            <div className={SYNC_WARN_BOX}>
              <SyncCheck
                checked={config.webdav.allow_http}
                disabled={disabled}
                onChange={(on) => setWebdav({ allow_http: on })}
                label="允许不加密的 http 连接"
                hint="只对局域网地址有效。账号和密码会明文经过网络（同步的内容本身仍是加密的）；公网地址请用 https。"
              />
            </div>
          )}
          {isHttps(config.webdav.url) && (
            <div className="rounded-lg border border-[#2a2f3a] px-3 py-2">
              {config.webdav.pinned_cert_sha256 ? (
                <div className="space-y-1.5">
                  <div className="text-xs text-gray-300">已信任这台服务器的自签证书</div>
                  <div className="break-all font-mono text-[10px] text-gray-500">
                    {config.webdav.pinned_cert_sha256}
                  </div>
                  <button
                    disabled={disabled}
                    onClick={() => setWebdav({ pinned_cert_sha256: "" })}
                    className={SYNC_BTN_SMALL}
                  >
                    取消信任
                  </button>
                </div>
              ) : (
                <div className="space-y-1.5">
                  <div className="flex items-center justify-between gap-2">
                    <span className={SYNC_HINT}>
                      服务器用的是自签证书（自建服务器常见）？取它的证书指纹，核对后信任。
                    </span>
                    <button
                      disabled={disabled || probing}
                      onClick={() => void probe()}
                      className={`shrink-0 ${SYNC_BTN_SMALL}`}
                    >
                      {probing ? "读取中…" : "读取证书"}
                    </button>
                  </div>
                  {probeError && <div className="text-[11px] text-red-300">{probeError}</div>}
                  {probed && (
                    <div className="space-y-1.5">
                      <div className="text-[11px] text-gray-400">
                        服务器出示的证书指纹（SHA-256）：
                      </div>
                      <div className="break-all font-mono text-[10px] text-gray-300">{probed}</div>
                      <div className={SYNC_HINT}>
                        请和服务器上那张证书的指纹核对，一致再信任。信任之后只认这一张证书，证书换了要重新信任。
                      </div>
                      <button
                        disabled={disabled}
                        onClick={() => {
                          setWebdav({ pinned_cert_sha256: probed });
                          setProbed(null);
                        }}
                        className={SYNC_BTN_SMALL}
                      >
                        核对过了，信任这张证书
                      </button>
                    </div>
                  )}
                </div>
              )}
            </div>
          )}
        </>
      )}

      {config.transport === "file" && (
        <div>
          <label className={SYNC_LABEL}>文件夹路径</label>
          <input
            type="text"
            value={config.file.dir}
            disabled={disabled}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            onChange={(e) => change({ ...config, file: { dir: e.target.value } })}
            className={SYNC_INPUT}
          />
          <p className={`mt-1 ${SYNC_HINT}`}>
            各台设备都能读写到的同一个目录，例如网盘客户端的同步文件夹、NAS
            的挂载目录。手机上一般用不了这种方式，请用 WebDAV。
          </p>
        </div>
      )}

      {config.transport === "s3" && (
        <>
          <div>
            <label className={SYNC_LABEL}>服务地址（Endpoint）</label>
            <input
              type="text"
              value={config.s3.endpoint}
              disabled={disabled}
              placeholder="https://oss-cn-hangzhou.aliyuncs.com"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              onChange={(e) => setS3({ endpoint: e.target.value })}
              className={SYNC_INPUT}
            />
          </div>
          <div className="grid grid-cols-2 gap-3 max-md:grid-cols-1">
            <div>
              <label className={SYNC_LABEL}>区域（Region）</label>
              <input
                type="text"
                value={config.s3.region}
                disabled={disabled}
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
                onChange={(e) => setS3({ region: e.target.value })}
                className={SYNC_INPUT}
              />
            </div>
            <div>
              <label className={SYNC_LABEL}>存储桶（Bucket）</label>
              <input
                type="text"
                value={config.s3.bucket}
                disabled={disabled}
                autoCapitalize="off"
                autoCorrect="off"
                spellCheck={false}
                onChange={(e) => setS3({ bucket: e.target.value })}
                className={SYNC_INPUT}
              />
            </div>
          </div>
          <div>
            <label className={SYNC_LABEL}>桶内目录（可不填）</label>
            <input
              type="text"
              value={config.s3.prefix}
              disabled={disabled}
              placeholder="rclaw/"
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              onChange={(e) => setS3({ prefix: e.target.value })}
              className={SYNC_INPUT}
            />
          </div>
          <div>
            <label className={SYNC_LABEL}>Access Key ID</label>
            <input
              type="text"
              value={config.s3.access_key_id}
              disabled={disabled}
              autoCapitalize="off"
              autoCorrect="off"
              spellCheck={false}
              autoComplete="off"
              onChange={(e) => setS3({ access_key_id: e.target.value })}
              className={SYNC_INPUT}
            />
          </div>
          <div>
            <label className={SYNC_LABEL}>Secret Access Key</label>
            <input
              type="password"
              value={secret}
              disabled={disabled}
              placeholder={secretPlaceholder}
              autoComplete="new-password"
              onChange={(e) => {
                stale();
                onSecret(e.target.value);
              }}
              className={SYNC_INPUT}
            />
            <p className={`mt-1 ${SYNC_HINT}`}>只存在这台设备上，不写进 rclaw.toml。</p>
          </div>
          <SyncCheck
            checked={config.s3.path_style}
            disabled={disabled}
            onChange={(on) => setS3({ path_style: on })}
            label="路径风格的地址（path-style）"
            hint="MinIO 等自建服务一般要勾上；云厂商的对象存储一般不勾。"
          />
          {isPlainHttp(config.s3.endpoint) && (
            <div className={SYNC_WARN_BOX}>
              <SyncCheck
                checked={config.s3.allow_http}
                disabled={disabled}
                onChange={(on) => setS3({ allow_http: on })}
                label="允许不加密的 http 连接"
                hint="只对局域网地址有效；公网地址请用 https。"
              />
            </div>
          )}
        </>
      )}

      <div>
        <button disabled={disabled || testing} onClick={() => void test()} className={SYNC_BTN}>
          {testing ? "正在测试…" : "测试连接"}
        </button>
        {testError && <div className={`mt-2 ${SYNC_ERROR_BOX}`}>{testError}</div>}
        {steps && (
          <div className="mt-2 space-y-1 rounded-lg border border-[#2a2f3a] px-3 py-2">
            {steps.map((step, i) => (
              <div key={`${i}-${step.name}`} className="text-[11px] leading-relaxed">
                <span className={step.ok ? "text-emerald-400" : "text-red-400"}>
                  {step.ok ? "✓" : "✗"} {step.name}
                </span>
                {step.detail && <span className="text-gray-400"> — {step.detail}</span>}
              </div>
            ))}
            <div className={`pt-1 text-[11px] ${failed.length ? "text-red-300" : "text-emerald-400"}`}>
              {steps.length === 0
                ? "没有可测试的项目"
                : failed.length
                  ? "这个存储现在还不能用于同步，请按上面的提示处理后再试。"
                  : "这个存储可以用于同步。"}
            </div>
          </div>
        )}
      </div>
    </div>
  );
}
