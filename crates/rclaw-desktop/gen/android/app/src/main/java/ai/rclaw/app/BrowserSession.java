package ai.rclaw.app;

/**
 * CDP 浏览器会话（browser_* 工具面的共享内核）。
 *
 * 专用长驻 WebView（不随单次调用销毁）：Rust 侧经 DevTools abstract socket
 * （webview_devtools_remote_<pid>）直连 CDP 驱动它——真实网络栈、真实 WebView
 * UA、Cookie 跨调用保持，配合 CDP Fetch 拦截摘掉网络栈强加的 X-Requested-With
 * （应用包名指纹），Google 等站点才放行。
 *
 * 可见性（悬浮窗接管，BrowserOverlay）：WebView 建于 applicationContext 离屏
 * 运行；attach(ViewGroup) 把它 reparent 进悬浮窗面板（DevTools socket 进程级，
 * reparent 不断 CDP 会话）——Google 弹风控码时用户可人工过码。attached 期间
 * 换硬件层（真窗口 GPU 合成），detach 回软件层（离屏 draw() 截图必须）。
 *
 * 视口：按真实 Display 的 CSS 尺寸布局（物理/density），不再是硬编码 1280×2400
 * ——此前 window.innerWidth(1280) 远大于 screen.width(360) 是真机不可能出现的
 * 指纹矛盾；start 的 JSON 带回 {width,height,dpr} 供 Rust 侧记录。
 *
 * 截图：离屏走 View.draw 到软件位图（无合成器帧）；attached 时由 Rust 侧直接
 * 走 CDP Page.captureScreenshot（真实合成器帧）。
 *
 * 线程约定：WebView 的创建/挂载/绘制/销毁与调试开关一律在【主线程】执行（公开
 * 方法从 Rust 阻塞工作线程调用，内部 post 到主线程 + CountDownLatch 等待）。
 * 实测 Android 10 + WebViewGoogle 154 上在子线程创建 WebView 会渲染进程崩溃、
 * 随后主进程 JNI CHECK abort（jni_android.cc(249)）；v2.5.4 时代的单发 WebView
 * 主线程创建则一直稳定。CDP 本身走套接字，与线程无关。
 */
final class BrowserSession {

    private BrowserSession() {}

    private static android.webkit.WebView webView;
    /** WebView 全部 UI 操作的主线程派发器。 */
    private static final android.os.Handler UI =
            new android.os.Handler(android.os.Looper.getMainLooper());
    /** 会话生命周期锁：ensure/attach/detach/stop/screenshot 互斥（状态只在持锁时改动）。 */
    private static final Object LOCK = new Object();

    /** 离屏布局视口（真实 Display 的 CSS 尺寸，start 时解析）。 */
    private static int viewW = 360;
    private static int viewH = 780;
    private static float viewDpr = 1f;
    /** WebView 当前是否挂载在悬浮窗容器里。 */
    private static boolean attached;

    /** 启动（或复用）会话。返回 JSON：
     *  {"ok":true,"pid":N,"width":W,"height":H,"dpr":D} / {"ok":false,"error":"..."}
     *  ——Rust 据此拼 DevTools socket 名 webview_devtools_remote_<pid> 连 CDP；
     *  尺寸为离屏布局的物理像素（CSS 视口 = width/dpr），供指纹一致性记录。 */
    static String start(android.content.Context context) {
        synchronized (LOCK) {
            if (webView != null) {
                return okJson();
            }
            if (context == null) {
                return errorJson("应用未就绪（无 context）");
            }
            final android.content.Context ctx = context.getApplicationContext();
            final java.util.concurrent.CountDownLatch ready =
                    new java.util.concurrent.CountDownLatch(1);
            final String[] out = {null};
            UI.post(() -> {
                try {
                    // 真实视口：WebView 的 measure/layout 以【物理像素】为单位，
                    // 渲染器再按 dpr 折算 CSS 视口（物理/density）——按物理尺寸
                    // 1080×2118 布局，CSS 视口自然等于 screen.width（360），
                    // 与真机 Chrome Mobile 完全同构。（此前按「CSS 尺寸」360
                    // 布局，渲染器再除一次 dpr 得到 120 CSS 宽的迷你视口，
                    // innerWidth 一系列非标准值的总根源。）
                    android.util.DisplayMetrics dm = ctx.getResources().getDisplayMetrics();
                    viewW = Math.max(320, dm.widthPixels);
                    viewH = Math.max(480, dm.heightPixels);
                    viewDpr = dm.density;
                    // 全局开关：本进程所有 WebView 暴露 DevTools socket。应用沙箱外
                    // （adb / root）才可能触达，普通应用无法连接它。新版 WebView 强制
                    // 此开关必须在主线程切换。
                    android.webkit.WebView.setWebContentsDebuggingEnabled(true);
                    android.webkit.WebView wv = new android.webkit.WebView(ctx);
                    android.webkit.WebSettings settings = wv.getSettings();
                    settings.setJavaScriptEnabled(true);
                    settings.setDomStorageEnabled(true);
                    // Chrome 移动版同款视口行为：无 viewport meta 的桌面页按
                    // 980 布局回退再缩小适配（真机 Chrome 的规范回退），有 meta
                    // 的页面按 device-width——否则 app-context WebView 的密度
                    // 换算会把 innerWidth 折成非标准值（实测 480：真机不可能
                    // 出现的布局宽度，是指纹矛盾）。
                    settings.setUseWideViewPort(true);
                    settings.setLoadWithOverviewMode(true);
                    // UA 基线保持 WebView 真实身份（真实 Chromium 版本）；
                    // Rust 侧会话建立时清洗覆写（摘「; wv」标记 + Client Hints
                    // 元数据），请求头由 CDP Fetch 拦截摘 X-Requested-With。
                    settings.setGeolocationEnabled(false);
                    settings.setSaveFormData(false);
                    // 软件层：离屏 draw() 截图必须（硬件层的离屏视图画不出内容）；
                    // attach 悬浮窗时切硬件层（真实 GPU 合成），detach 时换回。
                    wv.setLayerType(android.view.View.LAYER_TYPE_SOFTWARE, null);
                    // 不装 WebViewClient 时链接点击会甩给系统浏览器——CDP 会话
                    // 还停在旧页面，后续操作/截图全部错位。http(s)/about 一律
                    // 会话内导航；非 http(s)（tel:/mailto:/intent:…）复刻 WebView
                    // 默认派发器的安全语义后显式转交系统：
                    //   · 仅用户手势（点击）或重定向才派发——页面 JS 静默跳
                    //     tel:/mailto: 不得拉起拨号器/邮件；
                    //   · intent:// 先 parseUri 解出目标动作/组件（字面 intent
                    //     scheme 无法被 ACTION_VIEW 处理）并清掉 URL 携带的显式
                    //     component/selector（网页可借此调起非 BROWSABLE 的内部
                    //     组件）；
                    //   · 加 CATEGORY_BROWSABLE：只派发给声明接受浏览器来源的
                    //     目标（浏览器→应用边界）。
                    // 返回 true 表示本应用已处理（系统不会自动代发）。
                    wv.setWebViewClient(new android.webkit.WebViewClient() {
                        @Override
                        public void onPageStarted(
                                android.webkit.WebView view, String url, android.graphics.Bitmap favicon) {
                            // 地址栏实时跟随（含重定向/历史导航）；主线程回调直传。
                            notifyUrl(url);
                        }

                        @Override
                        public boolean shouldOverrideUrlLoading(
                                android.webkit.WebView view,
                                android.webkit.WebResourceRequest request) {
                            android.net.Uri uri = request.getUrl();
                            String scheme = uri.getScheme() == null ? "" : uri.getScheme();
                            if ("http".equals(scheme) || "https".equals(scheme)
                                    || "about".equals(scheme)) {
                                // about:（含 about:blank）是会话内部导航，外派会
                                // 取消导航且 CDP 停在旧文档。
                                return false;
                            }
                            if (request.hasGesture() || request.isRedirect()) {
                                try {
                                    android.content.Context c = view.getContext()
                                            .getApplicationContext();
                                    android.content.Intent intent;
                                    if ("intent".equals(scheme)) {
                                        intent = android.content.Intent.parseUri(
                                                uri.toString(),
                                                android.content.Intent.URI_INTENT_SCHEME);
                                        intent.setComponent(null);
                                        intent.setSelector(null);
                                    } else {
                                        intent = new android.content.Intent(
                                                android.content.Intent.ACTION_VIEW, uri);
                                    }
                                    intent.addCategory(
                                            android.content.Intent.CATEGORY_BROWSABLE);
                                    intent.addFlags(
                                            android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
                                    c.startActivity(intent);
                                } catch (Exception error) {
                                    // parseUri 抛受检 URISyntaxException；目标缺失等
                                    // 运行时异常一并按「派发失败」处理（页面留在原处）。
                                    android.util.Log.w("BrowserSession",
                                            "external scheme dispatch failed: " + uri, error);
                                }
                            }
                            return true;
                        }
                    });
                    // 进度回调（地址栏进度条消费）；顺带让 <input type=file>、
                    // 全屏视频等页面能力有接收方（无 WebChromeClient 时静默失效）。
                    wv.setWebChromeClient(new android.webkit.WebChromeClient() {
                        @Override
                        public void onProgressChanged(android.webkit.WebView view, int newProgress) {
                            // 后退可用性缓存（主线程维护，返回键无锁判定用）。
                            backAvailable = view.canGoBack();
                            notifyProgress(newProgress);
                        }
                    });
                    layoutOffscreen(wv);
                    wv.loadUrl("about:blank");
                    webView = wv;
                    out[0] = okJson();
                } catch (Throwable error) {
                    android.util.Log.e("BrowserSession", "webview create failed", error);
                    out[0] = errorJson("WebView 创建失败：" + error);
                } finally {
                    ready.countDown();
                }
            });
            try {
                if (!ready.await(15, java.util.concurrent.TimeUnit.SECONDS) || out[0] == null) {
                    return errorJson("WebView 创建超时");
                }
                return out[0];
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
                return errorJson("WebView 创建被中断");
            }
        }
    }

    /** 离屏布局：固定 CSS 视口尺寸（innerText/截图都依赖布局完成）。 */
    private static void layoutOffscreen(android.webkit.WebView wv) {
        wv.measure(
                android.view.View.MeasureSpec.makeMeasureSpec(viewW,
                        android.view.View.MeasureSpec.EXACTLY),
                android.view.View.MeasureSpec.makeMeasureSpec(viewH,
                        android.view.View.MeasureSpec.EXACTLY));
        wv.layout(0, 0, viewW, viewH);
    }

    /**
     * 把长驻 WebView 挂进悬浮窗容器（reparent；DevTools socket 进程级不断 CDP）。
     * attached 期间切硬件层：真窗口 GPU 合成、真实可见性/焦点。返回 JSON：
     * {"ok":true} / {"ok":false,"error":"..."}。
     */
    static String attach(android.view.ViewGroup container) {
        synchronized (LOCK) {
            if (webView == null) {
                return errorJson("会话未启动");
            }
            if (attached) {
                return okStateJson();
            }
            if (container == null) {
                return errorJson("悬浮窗容器不存在");
            }
            final android.view.ViewGroup host = container;
            final java.util.concurrent.CountDownLatch done =
                    new java.util.concurrent.CountDownLatch(1);
            final String[] out = {null};
            UI.post(() -> {
                try {
                    try {
                        webView.setLayerType(android.view.View.LAYER_TYPE_HARDWARE, null);
                    } catch (Throwable ignored) {
                    }
                    host.addView(webView, new android.view.ViewGroup.LayoutParams(
                            android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                            android.view.ViewGroup.LayoutParams.MATCH_PARENT));
                    attached = true;
                    out[0] = okStateJson();
                } catch (Throwable error) {
                    android.util.Log.e("BrowserSession", "attach failed", error);
                    out[0] = errorJson("WebView 挂载失败：" + error);
                } finally {
                    done.countDown();
                }
            });
            return awaitJson(done, out, "WebView 挂载超时");
        }
    }

    /** 从悬浮窗摘回离屏（布局尺寸保留；换回软件层）。幂等。返回 JSON 同 attach。 */
    static String detach() {
        synchronized (LOCK) {
            if (!attached) {
                return okStateJson();
            }
            final java.util.concurrent.CountDownLatch done =
                    new java.util.concurrent.CountDownLatch(1);
            final String[] out = {null};
            UI.post(() -> {
                try {
                    android.view.ViewParent parent = webView.getParent();
                    if (parent instanceof android.view.ViewGroup) {
                        ((android.view.ViewGroup) parent).removeView(webView);
                    }
                    attached = false;
                    try {
                        webView.setLayerType(android.view.View.LAYER_TYPE_SOFTWARE, null);
                    } catch (Throwable ignored) {
                    }
                    layoutOffscreen(webView);
                    out[0] = okStateJson();
                } catch (Throwable error) {
                    android.util.Log.e("BrowserSession", "detach failed", error);
                    out[0] = errorJson("WebView 摘离失败：" + error);
                } finally {
                    done.countDown();
                }
            });
            return awaitJson(done, out, "WebView 摘离超时");
        }
    }

    /** WebView 当前是否挂在悬浮窗里（Rust 截图分流：attached 走 CDP 截图）。 */
    static boolean isAttached() {
        synchronized (LOCK) {
            return attached;
        }
    }

    /** 会话是否已启动（面板展开前懒启动判断用）。 */
    static boolean isStarted() {
        synchronized (LOCK) {
            return webView != null;
        }
    }

    /** Activity 重建（新 decorView）：旧面板树作废——attached 标志复位、WebView
     *  从旧视图树摘回离屏布局。经 NAV 线程取引用（不在主线程抢 LOCK）。 */
    static void resetAttachmentForActivityChange() {
        NAV.execute(() -> {
            final android.webkit.WebView wv;
            synchronized (LOCK) {
                attached = false;
                wv = webView;
            }
            if (wv == null) {
                return;
            }
            UI.post(() -> {
                try {
                    android.view.ViewParent p = wv.getParent();
                    if (p instanceof android.view.ViewGroup) {
                        ((android.view.ViewGroup) p).removeView(wv);
                    }
                    wv.setLayerType(android.view.View.LAYER_TYPE_SOFTWARE, null);
                    layoutOffscreen(wv);
                } catch (Throwable ignored) {
                }
            });
        });
    }

    // ------------------------------------------------------------------
    // 手动上网（面板浏览器 chrome 的后端）：导航控制 + 地址栏回调。
    // 【锁纪律】chrome 按钮回调在主线程执行——主线程绝不抢 LOCK（后台
    // attach/detach/stop 持锁等主线程 latch 时，主线程抢锁即 ANR）：导航动作
    // 派发到 NAV 单线程执行器取引用再 post 回主线程；后退可用性由
    // onProgressChanged 在主线程维护的无锁缓存判定。
    // ------------------------------------------------------------------

    private static final java.util.concurrent.ExecutorService NAV =
            java.util.concurrent.Executors.newSingleThreadExecutor();

    /** 后退可用性缓存（onProgressChanged 主线程维护；返回键判定用，无锁）。 */
    private static volatile boolean backAvailable;

    /** 页面状态回调（WebViewClient/WebChromeClient 主线程回调转发；面板地址栏
     *  消费）。单方法函数式接口：url 非 null = 地址变化；progress 0-100，-1 =
     *  本拍无进度。 */
    interface PageListener {
        void onPageState(String url, int progress);
    }

    private static volatile PageListener pageListener;

    static void setPageListener(PageListener listener) {
        pageListener = listener;
    }

    /** 主线程转发页面状态（WebViewClient 回调本就在主线程，直接透传）。 */
    private static void notifyUrl(String url) {
        PageListener listener = pageListener;
        if (listener != null) {
            try {
                listener.onPageState(url == null ? "" : url, -1);
            } catch (Throwable ignored) {
            }
        }
    }

    private static void notifyProgress(int percent) {
        PageListener listener = pageListener;
        if (listener != null) {
            try {
                listener.onPageState(null, percent);
            } catch (Throwable ignored) {
            }
        }
    }

    /** 地址栏导航（面板地址栏回车；无 scheme 补 https://，搜索词兜底 Bing）。 */
    static void loadFromAddressBar(String input) {
        if (input == null || input.trim().isEmpty()) {
            return;
        }
        String trimmed = input.trim();
        final String url;
        if (trimmed.startsWith("http://") || trimmed.startsWith("https://")) {
            url = trimmed;
        } else if (!trimmed.contains(" ") && trimmed.contains(".")
                && !trimmed.endsWith(".")) {
            url = "https://" + trimmed;
        } else {
            // 非 URL 形态按搜索词处理（真实浏览器惯例）。
            url = "https://www.bing.com/search?q="
                    + android.net.Uri.encode(trimmed);
        }
        dispatchNav("address load", wv -> wv.loadUrl(url));
    }

    /** 导航动作统一通路：NAV 线程取引用（抢 LOCK 不在主线程），主线程执行。 */
    private static void dispatchNav(
            String what, java.util.function.Consumer<android.webkit.WebView> action) {
        NAV.execute(() -> {
            final android.webkit.WebView wv;
            synchronized (LOCK) {
                wv = webView;
            }
            if (wv == null) {
                return;
            }
            UI.post(() -> {
                try {
                    action.accept(wv);
                } catch (Throwable error) {
                    // 会话在排队期间被销毁等场景：WebView 方法抛 IllegalStateException。
                    android.util.Log.w("BrowserSession", what + " failed", error);
                }
            });
        });
    }

    /** 后退可用性（主线程读缓存，无锁）。 */
    static boolean canGoBack() {
        return backAvailable;
    }

    static void goBack() {
        dispatchNav("goBack", android.webkit.WebView::goBack);
    }

    static void goForward() {
        dispatchNav("goForward", android.webkit.WebView::goForward);
    }

    static void reload() {
        dispatchNav("reload", android.webkit.WebView::reload);
    }

    /** 销毁会话（browser_close 工具；下次使用自动重建）。 */
    static boolean stop() {
        synchronized (LOCK) {
            return teardownLocked();
        }
    }

    /** 离屏截图：主线程 View.draw 到 ARGB 位图（至多等 15s），PNG 压缩与写盘
     *  留在调用方工作线程——压缩可达秒级，上主线程会 ANR。 */
    static boolean screenshot(String absPath) {
        synchronized (LOCK) {
            if (webView == null || absPath == null || absPath.isEmpty()) {
                return false;
            }
            final java.util.concurrent.CountDownLatch done =
                    new java.util.concurrent.CountDownLatch(1);
            final android.graphics.Bitmap[] drawn = {null};
            UI.post(() -> {
                try {
                    android.graphics.Bitmap bmp = android.graphics.Bitmap.createBitmap(
                            webView.getWidth(), webView.getHeight(),
                            android.graphics.Bitmap.Config.ARGB_8888);
                    webView.draw(new android.graphics.Canvas(bmp));
                    drawn[0] = bmp;
                } catch (Throwable error) {
                    android.util.Log.e("BrowserSession", "screenshot draw failed", error);
                } finally {
                    done.countDown();
                }
            });
            try {
                if (!done.await(15, java.util.concurrent.TimeUnit.SECONDS)
                        || drawn[0] == null) {
                    return false;
                }
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
                return false;
            }
            try (java.io.FileOutputStream fos = new java.io.FileOutputStream(absPath)) {
                boolean ok = drawn[0].compress(
                        android.graphics.Bitmap.CompressFormat.PNG, 100, fos);
                return ok;
            } catch (Throwable error) {
                android.util.Log.e("BrowserSession", "screenshot save failed", error);
                return false;
            } finally {
                drawn[0].recycle();
            }
        }
    }

    /** 销毁 WebView（持 LOCK 调用；主线程执行，至多等 5s）。 */
    private static boolean teardownLocked() {
        if (webView == null) {
            return true;
        }
        attached = false;
        final android.webkit.WebView wv = webView;
        webView = null;
        final java.util.concurrent.CountDownLatch done =
                new java.util.concurrent.CountDownLatch(1);
        UI.post(() -> {
            try {
                wv.stopLoading();
                // destroy 前必须摘出视图树（可能还挂在悬浮窗容器里）。
                android.view.ViewParent parent = wv.getParent();
                if (parent instanceof android.view.ViewGroup) {
                    ((android.view.ViewGroup) parent).removeView(wv);
                }
                // Cookie 落盘：系统时机不定期，进程被杀即丢——风控信誉（已过码
                // 的 Google cookie）跟着丢。主动 flush 把内存 cookie 写进
                // SharedPreferences 持久化。
                try {
                    android.webkit.CookieManager.getInstance().flush();
                } catch (Throwable ignored) {
                }
                wv.destroy();
            } catch (RuntimeException ignored) {
                android.util.Log.w("BrowserSession", "webview destroy failed");
            } finally {
                done.countDown();
            }
        });
        try {
            done.await(5, java.util.concurrent.TimeUnit.SECONDS);
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
        }
        return true;
    }

    /** 主线程 post 结果的统一等待（attach/detach 共用；至多 10s）。 */
    private static String awaitJson(java.util.concurrent.CountDownLatch done,
                                    String[] out, String timeoutMessage) {
        try {
            if (!done.await(10, java.util.concurrent.TimeUnit.SECONDS) || out[0] == null) {
                return errorJson(timeoutMessage);
            }
            return out[0];
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            return errorJson("操作被中断");
        }
    }

    private static String okJson() {
        return "{\"ok\":true,\"pid\":" + android.os.Process.myPid()
                + ",\"width\":" + viewW + ",\"height\":" + viewH
                + ",\"dpr\":" + viewDpr + "}";
    }

    /** attach/detach 的状态应答（与 okJson 同构带 pid：Rust 侧单一解析器）。 */
    private static String okStateJson() {
        return okJson();
    }

    private static String errorJson(String message) {
        return "{\"ok\":false,\"error\":"
                + org.json.JSONObject.quote(message == null ? "" : message) + "}";
    }
}
