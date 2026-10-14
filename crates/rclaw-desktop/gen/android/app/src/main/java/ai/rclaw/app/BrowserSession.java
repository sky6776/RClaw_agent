package ai.rclaw.app;

/**
 * CDP 浏览器会话（web_browser / browser_* 工具的共享内核）。
 *
 * 专用 HandlerThread 上持有一个开启远程调试的系统 WebView（长驻，不随单次调用
 * 销毁）：Rust 侧经 DevTools abstract socket（webview_devtools_remote_<pid>）直连
 * CDP 驱动它——真实网络栈、真实 WebView UA、Cookie 跨调用保持，配合 CDP Fetch
 * 拦截摘掉网络栈强加的 X-Requested-With（应用包名指纹），Google 等站点才放行。
 *
 * 截图不走 CDP：离屏 WebView（不挂窗口）没有合成器帧，Page.captureScreenshot
 * 常出空白；用 View.draw 到软件位图（LAYER_TYPE_SOFTWARE）是确定性路径。
 *
 * 线程约定：WebView 的创建/销毁/绘制只能在会话 Looper 线程；公开方法从
 * Rust 阻塞工作线程调用（内部 CountDownLatch 等待，禁止在主线程调用）。
 */
final class BrowserSession {

    private BrowserSession() {}

    /** 视口：离屏强制布局尺寸（innerText/截图都依赖布局完成）。 */
    private static final int VIEW_W = 1280;
    private static final int VIEW_H = 2400;

    private static android.os.HandlerThread thread;
    private static android.os.Handler handler;
    private static android.webkit.WebView webView;
    /** 会话生命周期锁：ensure/stop/screenshot 互斥（WebView 只在会话线程动）。 */
    private static final Object LOCK = new Object();

    /** 启动（或复用）会话。返回 JSON：
     *  {"ok":true,"pid":<进程 pid>} / {"ok":false,"error":"..."}——Rust 据此拼
     *  DevTools socket 名 webview_devtools_remote_<pid> 连 CDP。 */
    static String start(android.content.Context context) {
        synchronized (LOCK) {
            if (webView != null) {
                return okJson();
            }
            if (context == null) {
                return errorJson("应用未就绪（无 context）");
            }
            try {
                // 全局开关：本进程所有 WebView 暴露 DevTools socket。应用沙箱外
                // （adb / root）才可能触达，普通应用无法连接它。
                android.webkit.WebView.setWebContentsDebuggingEnabled(true);
                thread = new android.os.HandlerThread("rclaw-cdp-browser");
                thread.start();
                handler = new android.os.Handler(thread.getLooper());
                final android.content.Context ctx = context.getApplicationContext();
                final java.util.concurrent.CountDownLatch ready = new java.util.concurrent.CountDownLatch(1);
                final String[] out = {null};
                handler.post(() -> {
                    try {
                        android.webkit.WebView wv = new android.webkit.WebView(ctx);
                        android.webkit.WebSettings settings = wv.getSettings();
                        settings.setJavaScriptEnabled(true);
                        settings.setDomStorageEnabled(true);
                        // UA 保持 WebView 真实身份（「; wv」+ 真实 Chromium 版本）——
                        // 假桌面 UA 与引擎/网络栈指纹不一致，对 Google 是更糟的签名；
                        // 干净的请求头由 Rust 侧 CDP Fetch 拦截负责。
                        settings.setGeolocationEnabled(false);
                        settings.setSaveFormData(false);
                        // 软件层：离屏 draw() 截图必须（硬件层的离屏视图画不出内容）。
                        wv.setLayerType(android.view.View.LAYER_TYPE_SOFTWARE, null);
                        // 不装 WebViewClient 时链接点击会甩给系统浏览器——CDP 会话
                        // 还停在旧页面，后续操作/截图全部错位。http(s) 一律会话内
                        // 导航；非 http(s)（tel:/mailto:/intent:…）复刻 WebView 默认
                        // 派发器的安全语义后显式转交系统：
                        //   · 仅用户手势（点击）或重定向才派发——页面 JS 静默跳
                        //     tel:/mailto: 不得拉起拨号器/邮件；
                        //   · intent:// 先 parseUri 解出目标动作/组件（字面 intent
                        //     scheme 无法被 ACTION_VIEW 处理）；
                        //   · 加 CATEGORY_BROWSABLE：只派发给声明接受浏览器来源的
                        //     目标（浏览器→应用边界）。
                        // 返回 true 表示本应用已处理（系统不会自动代发）。
                        wv.setWebViewClient(new android.webkit.WebViewClient() {
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
                                        android.content.Context ctx = view.getContext()
                                                .getApplicationContext();
                                        android.content.Intent intent;
                                        if ("intent".equals(scheme)) {
                                            intent = android.content.Intent.parseUri(
                                                    uri.toString(),
                                                    android.content.Intent.URI_INTENT_SCHEME);
                                            // 复刻 WebView 默认派发器的净化：parseUri
                                            // 会保留 URL 里的显式 component/selector，
                                            // 网页可借此调起未对浏览器开放的本应用内部
                                            // 组件（如非 BROWSABLE 的 MainActivity）——
                                            // 一律清掉，只按动作+数据派发。
                                            intent.setComponent(null);
                                            intent.setSelector(null);
                                        } else {
                                            intent = new android.content.Intent(
                                                    android.content.Intent.ACTION_VIEW, uri);
                                        }
                                        intent.addCategory(
                                                android.content.Intent.CATEGORY_BROWSABLE);
                                        intent.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK);
                                        ctx.startActivity(intent);
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
                        wv.measure(
                                android.view.View.MeasureSpec.makeMeasureSpec(VIEW_W,
                                        android.view.View.MeasureSpec.EXACTLY),
                                android.view.View.MeasureSpec.makeMeasureSpec(VIEW_H,
                                        android.view.View.MeasureSpec.EXACTLY));
                        wv.layout(0, 0, VIEW_W, VIEW_H);
                        wv.loadUrl("about:blank");
                        webView = wv;
                        out[0] = okJson();
                    } catch (Throwable error) {
                        android.util.Log.e("BrowserSession", "webview create failed", error);
                        out[0] = errorJson("WebView 创建失败：" + error.getMessage());
                    }
                    ready.countDown();
                });
                if (!ready.await(10, java.util.concurrent.TimeUnit.SECONDS) || out[0] == null) {
                    return errorJson("WebView 创建超时");
                }
                if (!out[0].contains("\"ok\":true")) {
                    // 创建失败：回收线程，下次调用重新起。
                    teardownLocked();
                }
                return out[0];
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
                return errorJson("WebView 创建被中断");
            }
        }
    }

    /** 销毁会话（browser_close 工具；下次使用自动重建）。 */
    static boolean stop() {
        synchronized (LOCK) {
            return teardownLocked();
        }
    }

    /** 离屏截图：View.draw 到 ARGB 位图 → PNG。阻塞至绘制完成（至多 20s）。 */
    static boolean screenshot(String absPath) {
        synchronized (LOCK) {
            if (webView == null || absPath == null || absPath.isEmpty()) {
                return false;
            }
            final java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
            final boolean[] ok = {false};
            handler.post(() -> {
                try {
                    android.graphics.Bitmap bmp = android.graphics.Bitmap.createBitmap(
                            webView.getWidth(), webView.getHeight(),
                            android.graphics.Bitmap.Config.ARGB_8888);
                    webView.draw(new android.graphics.Canvas(bmp));
                    try (java.io.FileOutputStream fos = new java.io.FileOutputStream(absPath)) {
                        bmp.compress(android.graphics.Bitmap.CompressFormat.PNG, 100, fos);
                    }
                    bmp.recycle();
                    ok[0] = true;
                } catch (Throwable error) {
                    android.util.Log.e("BrowserSession", "screenshot failed", error);
                } finally {
                    done.countDown();
                }
            });
            try {
                return done.await(20, java.util.concurrent.TimeUnit.SECONDS) && ok[0];
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
                return false;
            }
        }
    }

    /** 销毁 WebView + 停会话线程（持 LOCK 调用）。 */
    private static boolean teardownLocked() {
        if (webView == null) {
            return true;
        }
        final android.os.Handler h = handler;
        final android.webkit.WebView wv = webView;
        final java.util.concurrent.CountDownLatch done = new java.util.concurrent.CountDownLatch(1);
        if (h != null) {
            h.post(() -> {
                try {
                    wv.stopLoading();
                    wv.destroy();
                } catch (RuntimeException ignored) {
                    android.util.Log.w("BrowserSession", "webview destroy failed");
                }
                done.countDown();
            });
            try {
                done.await(5, java.util.concurrent.TimeUnit.SECONDS);
            } catch (InterruptedException error) {
                Thread.currentThread().interrupt();
            }
            if (thread != null) {
                thread.quitSafely();
            }
        }
        webView = null;
        handler = null;
        thread = null;
        return true;
    }

    private static String okJson() {
        return "{\"ok\":true,\"pid\":" + android.os.Process.myPid() + "}";
    }

    private static String errorJson(String message) {
        return "{\"ok\":false,\"error\":"
                + org.json.JSONObject.quote(message == null ? "" : message) + "}";
    }
}
