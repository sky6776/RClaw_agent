package ai.rclaw.app;

/**
 * CDP 浏览器会话（browser_* 工具面的共享内核）。
 *
 * 专用长驻 WebView（不随单次调用销毁）：Rust 侧经 DevTools abstract socket
 * （webview_devtools_remote_<pid>）直连 CDP 驱动它——真实网络栈、真实 WebView
 * UA、Cookie 跨调用保持，配合 CDP Fetch 拦截摘掉网络栈强加的 X-Requested-With
 * （应用包名指纹），Google 等站点才放行。
 *
 * 截图不走 CDP：离屏 WebView（不挂窗口）没有合成器帧，Page.captureScreenshot
 * 常出空白；用 View.draw 到软件位图（LAYER_TYPE_SOFTWARE）是确定性路径。
 *
 * 线程约定：WebView 的创建/绘制/销毁与调试开关一律在【主线程】执行（公开方法
 * 从 Rust 阻塞工作线程调用，内部 post 到主线程 + CountDownLatch 等待）。实测
 * Android 10 + WebViewGoogle 154 上在子线程创建 WebView 会渲染进程崩溃、随后
 * 主进程 JNI CHECK abort（jni_android.cc(249)）；v2.5.4 时代的单发 WebView 主
 * 线程创建则一直稳定。CDP 本身走套接字，与线程无关。
 */
final class BrowserSession {

    private BrowserSession() {}

    /** 视口：离屏强制布局尺寸（innerText/截图都依赖布局完成）。 */
    private static final int VIEW_W = 1280;
    private static final int VIEW_H = 2400;

    private static android.webkit.WebView webView;
    /** WebView 全部 UI 操作的主线程派发器。 */
    private static final android.os.Handler UI =
            new android.os.Handler(android.os.Looper.getMainLooper());
    /** 会话生命周期锁：ensure/stop/screenshot 互斥（状态只在持锁时改动）。 */
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
            final android.content.Context ctx = context.getApplicationContext();
            final java.util.concurrent.CountDownLatch ready =
                    new java.util.concurrent.CountDownLatch(1);
            final String[] out = {null};
            UI.post(() -> {
                try {
                    // 全局开关：本进程所有 WebView 暴露 DevTools socket。应用沙箱外
                    // （adb / root）才可能触达，普通应用无法连接它。新版 WebView 强制
                    // 此开关必须在主线程切换。
                    android.webkit.WebView.setWebContentsDebuggingEnabled(true);
                    android.webkit.WebView wv = new android.webkit.WebView(ctx);
                    android.webkit.WebSettings settings = wv.getSettings();
                    settings.setJavaScriptEnabled(true);
                    settings.setDomStorageEnabled(true);
                    // UA 基线保持 WebView 真实身份（真实 Chromium 版本）；
                    // Rust 侧会话建立时清洗覆写（摘「; wv」标记 + Client Hints
                    // 元数据），请求头由 CDP Fetch 拦截摘 X-Requested-With。
                    settings.setGeolocationEnabled(false);
                    settings.setSaveFormData(false);
                    // 软件层：离屏 draw() 截图必须（硬件层的离屏视图画不出内容）。
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
        final android.webkit.WebView wv = webView;
        webView = null;
        final java.util.concurrent.CountDownLatch done =
                new java.util.concurrent.CountDownLatch(1);
        UI.post(() -> {
            try {
                wv.stopLoading();
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

    private static String okJson() {
        return "{\"ok\":true,\"pid\":" + android.os.Process.myPid() + "}";
    }

    private static String errorJson(String message) {
        return "{\"ok\":false,\"error\":"
                + org.json.JSONObject.quote(message == null ? "" : message) + "}";
    }
}
