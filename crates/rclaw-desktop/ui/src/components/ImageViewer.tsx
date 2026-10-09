import { useEffect, useRef, useState } from "react";
import { api } from "../lib/api";
import { useBackClose } from "../lib/backNav";

/** 全屏图片查看器（WebView83 无手势库，touch 自算）：
 * - 单击：循环放大 1x → 2x → 3x → 1x（围绕中心，带过渡）；
 * - 双指捏合：1–8x 连续缩放（无过渡跟手）；
 * - 放大后单指拖动平移；缩放回到 1x 时自动归位；
 * - 画廊模式（onPrev/onNext 传入，经 MediaGallery 打开）：1x 时水平拖动
 *   跟手，松手过阈值/快速轻扫切上一张/下一张，否则弹回；
 * - 桌面：滚轮缩放、单击同样循环。
 * 关闭：右上 ✕ 或系统返回键（经 backNav 弹层栈接管）。
 *
 * 打开性能（真机实测的三处主线程/光栅阻塞，都是点图「卡」的来源）：
 * 1. 3MB 级 data URL → blob 的 base64 解码在主线程 ~0.6s，直接赋 img.src 的
 *    setAttribute 也要 ~374ms → 转换挪进 Web Worker，主线程只收 Blob 结果；
 * 2. 大图解码阻塞它所在那一帧的光栅，与遮罩同帧会让点击后画面静止 ~1.8s
 *    → 两阶段挂载：遮罩先独立成帧，再挂图；
 * 3. 12MP 全图解码本身 ~1-2s——原生相册类应用（Aves/Fossify 等）的通用解法
 *    是「缩略图优先」：先展示低清已解码位图再换入全图。这里用媒体卡 <img>
 *    已解码的位图画 canvas 即时预览（drawImage 复用解码缓存，毫秒级），
 *    全图解码完成后淡入替换。 */

/** 单击循环的档位。 */
const TAP_SCALES = [1, 2, 3];

// ---- 应用级常驻 data→blob Worker -------------------------------------
// Worker 冷启动（线程创建 + 脚本加载）在低端机主线程要数百 ms：每次开图
// 都新建会把这笔开销加回点击路径。懒创建一个常驻实例，请求按 id 配对，
// 失败/关闭时以 null 回结（调用方走 data URL 直挂兜底）。
let blobWorker: Worker | null = null;
const blobPending = new Map<number, (b: Blob | null) => void>();
let blobRequestId = 1;

function getBlobWorker(): Worker | null {
  if (blobWorker) return blobWorker;
  try {
    const code =
      "self.onmessage=(e)=>{const{id,data}=e.data;fetch(data).then(r=>r.blob()).then(b=>postMessage({id,b})).catch(()=>postMessage({id,b:null}))};";
    const w = new Worker(
      URL.createObjectURL(new Blob([code], { type: "text/javascript" })),
    );
    w.onmessage = (ev: MessageEvent) => {
      const { id, b } = ev.data as { id: number; b: Blob | null };
      const resolve = blobPending.get(id);
      blobPending.delete(id);
      resolve?.(b);
    };
    w.onerror = () => {
      // Worker 挂掉：全部请求以失败回结，并丢弃实例（下次重建），避免
      // 坏 Worker 常驻导致后续转换全部静默失败。
      blobWorker = null;
      blobPending.forEach((resolve) => resolve(null));
      blobPending.clear();
    };
    blobWorker = w;
    return w;
  } catch {
    return null;
  }
}

// 原图 blob 会话级缓存：同一张图再次打开直接是原图（微信「查看原图」后
// 的行为）。LRU 上限 4 张（12MP blob 各 ~48MB 解码位图由浏览器自行淘汰，
// 这里只限 blob 句柄数量）。
const fullBlobCache = new Map<string, string>();
const FULL_CACHE_MAX = 4;

function cacheFullBlob(path: string, url: string) {
  if (fullBlobCache.has(path)) fullBlobCache.delete(path);
  fullBlobCache.set(path, url);
  if (fullBlobCache.size > FULL_CACHE_MAX) {
    const oldest = fullBlobCache.keys().next().value as string | undefined;
    if (oldest !== undefined) {
      const oldUrl = fullBlobCache.get(oldest);
      if (oldUrl) URL.revokeObjectURL(oldUrl);
      fullBlobCache.delete(oldest);
    }
  }
}

function cachedFullBlob(path: string): string | null {
  return fullBlobCache.get(path) ?? null;
}

function dataUrlToBlob(src: string): Promise<Blob | null> {
  const w = getBlobWorker();
  if (!w) return Promise.resolve(null);
  return new Promise((resolve) => {
    const rid = blobRequestId++;
    blobPending.set(rid, resolve);
    w.postMessage({ id: rid, data: src });
    // Worker 若被系统终止（不触发 onerror），8s 无响应按失败回结，
    // 调用方走 data URL 直挂兜底，避免 spinner 永转。
    window.setTimeout(() => {
      if (blobPending.delete(rid)) resolve(null);
    }, 8000);
  });
}

interface Props {
  /** 预览图 data URL（后端 media_preview 缩到长边 1440，1x 显示用）。 */
  src: string;
  /** 工作区路径：「查看原图」/放大时按需加载原图（media_base64）。 */
  path: string;
  /** 展示名（顶栏文件名）。 */
  display: string;
  /** 会话里媒体卡的 <img> 元素（已按小尺寸解码）。给定时就地画 canvas 即时
   *  预览——drawImage 复用浏览器已解码位图，毫秒级出图，全图解码完再换入
   *  （缩略图优先，原生相册类应用的同款策略）。 */
  preview?: HTMLImageElement | null;
  /** 原图/预览加载失败提示（冒泡到 App toast）。 */
  onError?: (message: string) => void;
  onClose: () => void;
  /** 媒体画廊切换（微信行为）：1x 时向右滑 = onPrev（上一个）、向左滑 =
   *  onNext（下一个）。缺省 = 单图模式，1x 水平拖动无效果。 */
  onPrev?: () => void;
  onNext?: () => void;
}

export default function ImageViewer({
  src,
  path,
  display,
  preview,
  onError,
  onClose,
  onPrev,
  onNext,
}: Props) {
  const [scale, setScale] = useState(1);
  const [tx, setTx] = useState(0);
  const [ty, setTy] = useState(0);
  /** 动画过渡：单击档位切换用，手势跟手时关闭。 */
  const [smooth, setSmooth] = useState(true);
  /** data URL → blob URL（见顶部说明）；fetch 失败回退直挂 src。 */
  const [resolved, setResolved] = useState<string | null>(null);
  const [fallback, setFallback] = useState(false);
  /** 图片是否已解码上屏（决定 canvas 预览换入全图的时机）。 */
  const [imgLoaded, setImgLoaded] = useState(false);
  /** canvas 预览是否已就绪（卡片位图已画上）。 */
  const [previewReady, setPreviewReady] = useState(false);
  const canvasRef = useRef<HTMLCanvasElement | null>(null);
  /** 微信式「查看原图」：原图 blob（已加载）/加载中/已上屏。 */
  const [full, setFull] = useState<{ url: string } | null>(null);
  const [fullLoading, setFullLoading] = useState(false);
  const [fullVisible, setFullVisible] = useState(false);
  const wantFullRef = useRef(false);
  const previewMadeRef = useRef<string | null>(null);
  // 移动端 touchend 处理完轻点后浏览器还会派发合成 click：500ms 内的
  // click 一律吞掉，否则一次轻点会连跳两档。
  const lastTouchEnd = useRef(0);
  // 系统返回键关闭查看器（挂载即打开，常驻注册）。
  useBackClose(true, onClose);
  const gesture = useRef({
    mode: "none" as "none" | "pan" | "pinch",
    startX: 0,
    startY: 0,
    baseTx: 0,
    baseTy: 0,
    baseDist: 0,
    baseScale: 1,
    moved: false,
    startTs: 0,
  });

  useEffect(() => {
    let alive = true;
    setResolved(null);
    setFallback(false);
    setImgLoaded(false);
    setPreviewReady(false);
    setFull(null);
    setFullLoading(false);
    setFullVisible(false);
    wantFullRef.current = false;
    if (previewMadeRef.current) {
      URL.revokeObjectURL(previewMadeRef.current);
      previewMadeRef.current = null;
    }
    // 预览 data→blob 转换放常驻 Worker 线程（见 getBlobWorker）：主线程不碰
    // base64 解码，遮罩/canvas 首帧不被拖住。Worker 不可用时直接 data URL
    // 兜底（setAttribute ~374ms，仅限极端环境）。
    const cached = cachedFullBlob(path);
    if (cached) {
      wantFullRef.current = true;
      // 会话里已加载过原图：直接原图上屏（微信「再打开就直接是原图」）。
      setFull({ url: cached });
      setFullVisible(true);
      setImgLoaded(true);
      return;
    }
    let cancelled = false;
    dataUrlToBlob(src).then((b) => {
      if (cancelled || !alive) return;
      if (!b) {
        setFallback(true);
        onError?.("图片加载失败，已降级为原始加载方式");
        return;
      }
      const u = URL.createObjectURL(b);
      previewMadeRef.current = u;
      setResolved(u);
    });
    return () => {
      alive = false;
      cancelled = true;
    };
  }, [src, path]);

  // 微信式「查看原图」：加载原始尺寸图（media_base64 → Worker 转 blob）。
  // 加载完成后主图换 src（浏览器会保持旧画面直到新图解码就绪），按钮隐藏；
  // 同一会话内重复打开，内存缓存命中，直接就是原图（秒开）。
  const requestFull = () => {
    if (wantFullRef.current || full || fullLoading) return;
    wantFullRef.current = true;
    setFullLoading(true);
    api
      .mediaBase64(path)
      .then((r) => {
        const dataUrl = `data:${r.mime};base64,${r.base64}`;
        return dataUrlToBlob(dataUrl).then((b) => ({ b, dataUrl }));
      })
      .then(({ b, dataUrl }) => {
        // Worker 不可用（如 CSP 拦 blob:）时拿 data URL 直挂原图：慢一点
        // （setAttribute 阻塞主线程）但「查看原图」不至于整个失败。
        const u = b ? URL.createObjectURL(b) : dataUrl;
        cacheFullBlob(path, u);
        setFull({ url: u });
        setFullLoading(false);
      })
      .catch(() => {
        // 失败必须可见（按钮会弹回「查看原图」），不能静默装作加载完成。
        wantFullRef.current = false;
        setFullLoading(false);
        onError?.("原图加载失败，请稍后重试");
      });
  };

  // canvas 即时预览：等遮罩先提交一拍，再把卡片已解码位图画上（drawImage
  // 复用解码缓存，毫秒级）。没有卡片元素（菜单「放大查看」路径）则跳过。
  useEffect(() => {
    setPreviewReady(false);
    if (!preview) return;
    let cancelled = false;
    let raf2 = 0;
    const raf1 = requestAnimationFrame(() => {
      raf2 = requestAnimationFrame(() => {
        if (cancelled) return;
        const c = canvasRef.current;
        if (!c || !preview.naturalWidth || !preview.naturalHeight) return;
        // 尺寸防御上限（卡片来自后端预览时本就 <=1440，防其它路径塞入原图）。
        const cap = 1440;
        const k = Math.min(1, cap / Math.max(preview.naturalWidth, preview.naturalHeight));
        c.width = Math.max(1, Math.round(preview.naturalWidth * k));
        c.height = Math.max(1, Math.round(preview.naturalHeight * k));
        const ctx = c.getContext("2d");
        if (!ctx) return;
        ctx.drawImage(preview, 0, 0, c.width, c.height);
        setPreviewReady(true);
      });
    });
    return () => {
      cancelled = true;
      cancelAnimationFrame(raf1);
      cancelAnimationFrame(raf2);
    };
  }, [preview]);

  // 卸载：撤销预览 blob（原图 blob 在会话缓存里，随 LRU 淘汰，不在此撤销）。
  useEffect(
    () => () => {
      if (previewMadeRef.current) {
        URL.revokeObjectURL(previewMadeRef.current);
        previewMadeRef.current = null;
      }
    },
    [],
  );

  const pinchDist = (t: React.TouchList) =>
    Math.hypot(t[0].clientX - t[1].clientX, t[0].clientY - t[1].clientY);

  const onTouchStart = (e: React.TouchEvent) => {
    const g = gesture.current;
    setSmooth(false);
    if (e.touches.length >= 2) {
      g.mode = "pinch";
      g.baseDist = pinchDist(e.touches);
      g.baseScale = scale;
      requestFull(); // 放大即看原图（微信行为）
    } else if (e.touches.length === 1) {
      g.mode = "pan";
      g.startX = e.touches[0].clientX;
      g.startY = e.touches[0].clientY;
      g.baseTx = tx;
      g.baseTy = ty;
      g.moved = false;
      g.startTs = Date.now();
    }
  };

  const onTouchMove = (e: React.TouchEvent) => {
    const g = gesture.current;
    if (g.mode === "pinch" && e.touches.length >= 2) {
      const next = Math.min(8, Math.max(1, (g.baseScale * pinchDist(e.touches)) / g.baseDist));
      setScale(next);
      if (next === 1) {
        setTx(0);
        setTy(0);
      }
      g.moved = true;
    } else if (g.mode === "pan" && e.touches.length === 1) {
      const dx = e.touches[0].clientX - g.startX;
      const dy = e.touches[0].clientY - g.startY;
      if (Math.abs(dx) + Math.abs(dy) > 8) g.moved = true;
      if (scale > 1) {
        setTx(g.baseTx + dx);
        setTy(g.baseTy + dy);
      } else if (g.moved && (onPrev || onNext)) {
        // 画廊模式 1x：水平跟手（切换的拖拽反馈），松手过阈值切换/弹回；
        // 垂直方向不跟（竖图横滑的手感不受影响）。
        setTx(dx);
      }
    }
  };

  const onTouchEnd = (e: React.TouchEvent) => {
    const g = gesture.current;
    lastTouchEnd.current = Date.now();
    // 双指结束剩一指：以剩余手指为平移基准，不抬起不算 tap。
    if (g.mode === "pinch" && e.touches.length === 1) {
      g.mode = "pan";
      g.startX = e.touches[0].clientX;
      g.startY = e.touches[0].clientY;
      g.baseTx = tx;
      g.baseTy = ty;
      g.moved = true;
      return;
    }
    if (g.mode === "pan" && e.touches.length === 0) {
      // 短按且没移动 = 单击：循环放大档位。
      if (!g.moved && Date.now() - g.startTs < 350) {
        const idx = TAP_SCALES.indexOf(scale);
        const next = TAP_SCALES[(idx + 1) % TAP_SCALES.length];
        setSmooth(true);
        setScale(next);
        if (next === 1) {
          setTx(0);
          setTy(0);
        } else {
          requestFull(); // 放大即看原图（微信行为）
        }
      } else if (g.moved && scale === 1 && (onPrev || onNext)) {
        // 画廊切换：水平拖动过阈值（或快速轻扫）切上一张/下一张，否则弹回。
        const end = e.changedTouches[0];
        const dx = end ? end.clientX - g.startX : 0;
        const dy = end ? end.clientY - g.startY : 0;
        const quick = Date.now() - g.startTs < 250 && Math.abs(dx) > 40;
        // 同 swipeNav：屏宽 20%，60–200px 夹紧（触屏桌面不至于拖半只手）。
        const threshold = Math.min(200, Math.max(60, window.innerWidth * 0.2));
        // 主导方向须水平：纵向为主的拖动不触发切换。
        if (Math.abs(dx) > Math.abs(dy) * 1.2) {
          if ((dx <= -threshold || (quick && dx < 0)) && onNext) {
            onNext();
            // 切换后画廊按新条目重建查看器（key=path），跟手位移随旧实例丢弃。
            return;
          }
          if ((dx >= threshold || (quick && dx > 0)) && onPrev) {
            onPrev();
            return;
          }
        }
        setSmooth(true); // 未达阈值：带动画弹回
      }
      if (scale === 1) {
        setTx(0);
        setTy(0);
      }
    }
    if (e.touches.length === 0) g.mode = "none";
  };

  // 桌面：滚轮缩放（围绕中心，简单档位 ±）。
  const onWheel = (e: React.WheelEvent) => {
    if (e.deltaY === 0) return;
    setSmooth(false);
    const next = e.deltaY < 0 ? Math.min(8, scale + 0.25) : Math.max(1, scale - 0.25);
    setScale(next);
    if (next > scale) requestFull(); // 放大即看原图（微信行为）
    if (next === 1) {
      setTx(0);
      setTy(0);
    }
  };

  // 桌面单击与移动端 tap 同一循环；右键不弹系统菜单。
  const onClick = () => {
    if (Date.now() - lastTouchEnd.current < 500) return;
    const idx = TAP_SCALES.indexOf(scale);
    const next = TAP_SCALES[(idx + 1) % TAP_SCALES.length];
    setSmooth(true);
    setScale(next);
    if (next === 1) {
      setTx(0);
      setTy(0);
    } else {
      requestFull(); // 放大即看原图（微信行为）
    }
  };

  return (
    <div
      className="fixed inset-0 z-[60] flex touch-none select-none items-center justify-center bg-black/95"
      onTouchStart={onTouchStart}
      onTouchMove={onTouchMove}
      onTouchEnd={onTouchEnd}
      onTouchCancel={() => {
        gesture.current.mode = "none";
      }}
      onWheel={onWheel}
      onContextMenu={(e) => e.preventDefault()}
    >
      <div
        className="absolute left-1/2 top-0 z-10 flex -translate-x-1/2 items-center gap-2 px-3 py-1 text-[11px] text-gray-400"
        style={{ marginTop: "calc(env(safe-area-inset-top) + 0.5rem)" }}
      >
        <span className="max-w-[60vw] truncate font-mono" title={display}>
          {display}
        </span>
        <span className="text-gray-600">{scale.toFixed(scale % 1 === 0 ? 0 : 1)}x</span>
      </div>
      <button
        onClick={onClose}
        aria-label="关闭"
        className="absolute right-3 z-10 flex h-10 w-10 items-center justify-center rounded-full bg-white/10 text-xl text-white transition active:bg-white/20"
        style={{ top: "calc(env(safe-area-inset-top) + 0.5rem)" }}
      >
        ✕
      </button>
      {/* 阶段1 即时预览：卡片已解码位图画到 canvas（略软但毫秒级出图），
          与全图用同一套 object-contain / transform，捏合拖动由根层手势统一
          处理；全图 onLoad 后隐藏 canvas 换入全图。 */}
      {previewReady && !imgLoaded && (
        <canvas
          ref={canvasRef}
          onClick={onClick}
          className="max-h-full max-w-full touch-none object-contain"
          style={{
            transform: `translate(${tx}px, ${ty}px) scale(${scale})`,
            transition: smooth ? "transform 0.25s ease-out" : "none",
          }}
        />
      )}
      {/* 微信式「查看原图」：预览阶段常驻底部；点击后原位转加载态（spinner +
          「加载中…」），原图解码完成自动替换主图并隐藏。已加载后不再出现。 */}
      {!full && (
        <button
          onClick={requestFull}
          disabled={fullLoading}
          className="absolute bottom-10 left-1/2 z-30 -translate-x-1/2 rounded-full border border-white/30 bg-black/60 px-4 py-1.5 text-xs text-white/90 backdrop-blur transition active:bg-black/80"
          style={{ marginBottom: "env(safe-area-inset-bottom)" }}
        >
          {fullLoading ? (
            <span className="flex items-center gap-2">
              <span className="inline-block h-3 w-3 animate-spin rounded-full border-[1.5px] border-white/80 border-t-transparent" />
              加载中…
            </span>
          ) : (
            "查看原图"
          )}
        </button>
      )}
      {/* 原图隐藏预载：解码完成后切主图（浏览器保持旧画面，无空窗闪动）。 */}
      {full && !fullVisible && (
        <img
          src={full.url}
          alt=""
          aria-hidden
          decoding="async"
          onLoad={() => setFullVisible(true)}
          className="pointer-events-none absolute h-px w-px opacity-0"
        />
      )}
      {/* 阶段2 全图：blob 解码完成后淡入替换预览。挂载条件包含「缓存命中
          直接是原图」路径——那时 resolved 尚为 null，若只看 (fallback ||
          resolved) 会整屏空白。 */}
      {(fallback || resolved || (full && fullVisible)) && (
        <img
          {...(full && fullVisible
            ? { src: full.url }
            : fallback
              ? { src }
              : { src: resolved as string })}
          alt={display}
          draggable={false}
          // 解码不阻塞遮罩首帧（见上）；完成/失败都收起 spinner。
          decoding="async"
          onLoad={() => setImgLoaded(true)}
          onError={() => setImgLoaded(true)}
          onClick={onClick}
          className={`max-h-full max-w-full touch-none object-contain transition-opacity duration-200 ${
            imgLoaded ? "opacity-100" : "pointer-events-none absolute opacity-0"
          }`}
          style={{
            transform: `translate(${tx}px, ${ty}px) scale(${scale})`,
            transition: smooth ? "transform 0.25s ease-out" : "none",
          }}
        />
      )}
      {/* 解码中 spinner：仅当没有即时预览可用（菜单「放大查看」路径）时显示。 */}
      {!imgLoaded && !previewReady && (fallback || resolved) && (
        <div className="pointer-events-none absolute inset-0 z-20 flex items-center justify-center">
          <span className="block h-8 w-8 animate-spin rounded-full border-2 border-white/70 border-t-transparent" />
        </div>
      )}
    </div>
  );
}
