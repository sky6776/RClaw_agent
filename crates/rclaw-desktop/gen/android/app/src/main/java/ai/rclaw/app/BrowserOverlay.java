package ai.rclaw.app;

import androidx.activity.OnBackPressedCallback;

/**
 * CDP 浏览器面板（应用内原生视图，盖在聊天 UI 上；宿主 = MainActivity 视图树，
 * 无悬浮窗权限需求）。
 *
 * 存在的理由：长驻 WebView 建于 applicationContext 从不挂窗，agent 离屏驱动——
 * Google 弹风控码（CAPTCHA）时用户既看不到页面也无法人工过码，任务死局。
 * 面板把同一 WebView reparent 进可见容器（DevTools socket 进程级，reparent
 * 不断 CDP 会话），agent 与用户接管同一实例。
 *
 * 入口与形态：
 *  - 入口：侧边栏顶部的「浏览器」按钮（Tauri 命令 → NativeBridge）与过码通知
 *    （open_browser extra → MainActivity）；
 *  - FULLSCREEN：占满应用窗口 = 浏览器 chrome（地址栏/前进/后退/刷新/全屏切换/
 *    收起/关闭会话）+ WebView。人工过码与手动上网都在这态；
 *  - WINDOWED：应用窗口内的可拖拽调大小面板（右下角手柄），chrome 的 ⛶ 往返。
 * 收起 = 面板移出视图树（WebView 摘回离屏，agent 继续干活），聊天 UI 与侧边
 * 栏入口重新可见。
 *
 * 手动上网：地址栏输入 URL 或搜索词回车导航，后退/前进/刷新齐备，系统返回键
 * = 网页后退（无历史则收起）——与 Firefox 等浏览器的日常操作同构；用户浏览
 * 与 agent 驱动共用同一会话（同一 cookie 档案，用户登录同时养 agent 的风控
 * 信誉）。用户离开应用 = 页面转后台（visibilityState 如实隐藏，与真人最小化
 * 浏览器一致，不构成指纹矛盾）。
 *
 * 线程约定：视图操作一律主线程（公开静态方法从工作线程调用，内部 runOnUiThread
 * + CountDownLatch 等待）；形态切换调用方持有 NativeBridge.OVERLAY_SERIAL（与
 * attach/detach 全链路互斥）。
 */
final class BrowserOverlay {

    private BrowserOverlay() {}

    private static final Object LOCK = new Object();
    private static final android.os.Handler UI =
            new android.os.Handler(android.os.Looper.getMainLooper());

    /** 面板形态。 */
    private static final int MODE_FULLSCREEN = 1;
    private static final int MODE_WINDOWED = 2;

    /** 宿主 Activity（MainActivity onCreate 登记；面板加进其内容视图）。
     *  ComponentActivity：返回键 dispatcher 在 androidx 上。 */
    public static volatile androidx.activity.ComponentActivity activity;

    private static android.view.ViewGroup panel;
    /** WebView 挂载容器（面板主体）。 */
    private static android.widget.FrameLayout webviewHost;
    /** 面板地址栏 / 进度条（PageListener 回调更新）。 */
    private static android.widget.EditText addressBar;
    private static android.widget.ProgressBar progressBar;
    /** 返回键接管（面板可见时启用，优先级高于聊天返回逻辑）。 */
    private static OnBackPressedCallback backCallback;
    private static volatile boolean panelShown;
    /** 窗口态尺寸（物理 px；全屏切换与拖拽记忆）。 */
    private static int windowedW;
    private static int windowedH;
    /** 窗口态左上角绝对位置；-1 = 未拖动过（默认贴底居中）。 */
    private static int winX = -1;
    private static int winY = -1;
    private static int currentMode = MODE_FULLSCREEN;

    /** WebView 挂载容器（BrowserSession.attach 的目标；面板可见时有效）。 */
    static android.view.ViewGroup webviewHost() {
        return webviewHost;
    }

    /** 面板是否可见（WebView 应处于 attached；截图分流据此走 CDP 轨）。 */
    static boolean isExpanded() {
        return panelShown;
    }

    /** Activity 登记（onCreate；重建时刷新并丢弃旧视图树上的面板引用）。 */
    static void setActivity(androidx.activity.ComponentActivity host) {
        // 先作废旧面板引用（webviewHost=null 等），【再】投递复位：复位任务在
        // NAV 线程异步执行，若在引用作废前运行，会看到「attached=true 且父容器
        // == 旧 webviewHost」而错误跳过，WebView 滞留旧 Activity 的视图树。
        // 作废后投递则复位必然执行（摘旧树 + 恢复软件层 + 清 attached）。
        synchronized (BrowserOverlay.class) {
            activity = host;
            // 旧 Activity 的视图树不可再用：面板/回调引用作废，下次展开重建。
            panel = null;
            webviewHost = null;
            addressBar = null;
            progressBar = null;
            backCallback = null;
            panelShown = false;
        }
        BrowserSession.resetAttachmentForActivityChange();
    }

    // ------------------------------------------------------------------
    // 公开入口（工作线程调用；调用方持 NativeBridge.OVERLAY_SERIAL）
    // ------------------------------------------------------------------

    /**
     * 显示面板（幂等；恢复上次形态，首次全屏）。不含 attach：WebView 挂载由
     * 调用方在同一串行段完成（会话懒启动在 NativeBridge.browserOverlayExpand）。
     * 无宿主 Activity（极端时序）返回 false。
     */
    static boolean ensureExpanded(android.content.Context context) {
        androidx.activity.ComponentActivity host = activity;
        if (host == null || host.isFinishing()) {
            android.util.Log.w("BrowserOverlay", "expand without activity host");
            return false;
        }
        synchronized (BrowserOverlay.class) {
            currentMode = currentMode == MODE_WINDOWED ? MODE_WINDOWED : MODE_FULLSCREEN;
        }
        final androidx.activity.ComponentActivity uiHost = host;
        final java.util.concurrent.CountDownLatch done =
                new java.util.concurrent.CountDownLatch(1);
        final boolean[] ok = {false};
        UI.post(() -> {
            try {
                showPanelLocked(uiHost);
                ok[0] = true;
            } catch (Throwable error) {
                android.util.Log.e("BrowserOverlay", "expand failed", error);
            } finally {
                done.countDown();
            }
        });
        try {
            if (!done.await(5, java.util.concurrent.TimeUnit.SECONDS)) {
                return false;
            }
        } catch (InterruptedException error) {
            Thread.currentThread().interrupt();
            return false;
        }
        return ok[0];
    }

    /** 全屏展开（park 过码 / 通知自动展开用：过码要最大可视面积）。 */
    static boolean ensureExpandedFullscreen(android.content.Context context) {
        synchronized (BrowserOverlay.class) {
            currentMode = MODE_FULLSCREEN;
        }
        return ensureExpanded(context);
    }

    /** 收起：面板移出视图树（WebView 由调用方 detach；agent 不受影响）。幂等。 */
    static void collapse(android.content.Context context) {
        androidx.activity.ComponentActivity host = activity;
        if (host == null) {
            return;
        }
        UI.post(() -> {
            try {
                removePanelLocked(host);
            } catch (Throwable error) {
                android.util.Log.e("BrowserOverlay", "collapse failed", error);
            }
        });
    }

    /** 关闭面板（关闭会话路径；WebView 销毁由调用方负责）。幂等。 */
    static void dismiss(android.content.Context context) {
        collapse(context);
    }

    // ------------------------------------------------------------------
    // 主线程实现（*Locked 均在 UI 线程执行）
    // ------------------------------------------------------------------

    private static void showPanelLocked(androidx.activity.ComponentActivity host) {
        android.view.ViewGroup content = host.findViewById(android.R.id.content);
        if (content == null) {
            throw new IllegalStateException("content view 不可用");
        }
        int mode;
        synchronized (BrowserOverlay.class) {
            mode = currentMode;
        }
        if (panel == null) {
            panel = buildPanel(host, content);
        }
        if (panel.getParent() == null) {
            content.addView(panel, panelParams(host, mode));
        } else {
            panel.setLayoutParams(panelParams(host, mode));
        }
        applyMode(host, mode);
        BrowserSession.setPageListener(BrowserOverlay::onPageState);
        if (backCallback != null) {
            backCallback.setEnabled(true);
        }
        panelShown = true;
        panel.requestFocus();
    }

    private static void removePanelLocked(androidx.activity.ComponentActivity host) {
        if (panel != null && panel.getParent() instanceof android.view.ViewGroup) {
            ((android.view.ViewGroup) panel.getParent()).removeView(panel);
        }
        panelShown = false;
        if (backCallback != null) {
            backCallback.setEnabled(false);
        }
    }

    /** 全屏 ↔ 窗口切换（chrome 按钮触发；UI 线程）。 */
    private static void toggleMode(androidx.activity.ComponentActivity host) {
        int mode;
        synchronized (BrowserOverlay.class) {
            currentMode = currentMode == MODE_FULLSCREEN ? MODE_WINDOWED : MODE_FULLSCREEN;
            mode = currentMode;
        }
        if (panel != null && panel.getParent() != null) {
            panel.setLayoutParams(panelParams(host, mode));
            applyMode(host, mode);
        }
    }

    private static void applyMode(androidx.activity.ComponentActivity host, int mode) {
        if (panel == null) {
            return;
        }
        float density = host.getResources().getDisplayMetrics().density;
        android.graphics.drawable.GradientDrawable bg =
                new android.graphics.drawable.GradientDrawable();
        bg.setColor(0xF214161C);
        bg.setCornerRadius(mode == MODE_WINDOWED ? 14 * density : 0f);
        panel.setBackground(bg);
        if (panel instanceof android.widget.FrameLayout) {
            ((android.widget.FrameLayout) panel).setPadding(
                    0, 0, 0, mode == MODE_WINDOWED ? (int) (2 * density) : 0);
        }
        android.view.View grip = panel.findViewWithTag("resize_grip");
        if (grip != null) {
            grip.setVisibility(mode == MODE_WINDOWED
                    ? android.view.View.VISIBLE : android.view.View.GONE);
        }
        android.view.View dragStrip = panel.findViewWithTag("drag_strip");
        if (dragStrip != null) {
            dragStrip.setVisibility(mode == MODE_WINDOWED
                    ? android.view.View.VISIBLE : android.view.View.GONE);
        }
    }

    private static android.view.ViewGroup.LayoutParams panelParams(
            androidx.activity.ComponentActivity host, int mode) {
        android.content.res.Resources dmRes = host.getResources();
        if (mode == MODE_FULLSCREEN) {
            // 必须是 MarginLayoutParams 族（ContentFrameLayout 测量子视图时强转，
            // 裸 ViewGroup.LayoutParams 会 ClassCastException 崩溃）。
            return new android.widget.FrameLayout.LayoutParams(
                    android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                    android.view.ViewGroup.LayoutParams.MATCH_PARENT);
        }
        ensureWindowSize(host);
        android.widget.FrameLayout.LayoutParams params =
                new android.widget.FrameLayout.LayoutParams(windowedW, windowedH);
        if (winX >= 0 && winY >= 0) {
            // 拖动过：绝对定位（拖拽条实时更新 winX/winY）。
            params.gravity = android.view.Gravity.TOP | android.view.Gravity.START;
            params.leftMargin = Math.max(0, winX);
            params.topMargin = Math.max(0, winY);
        } else {
            // 未拖动：默认贴底居中。
            params.gravity = android.view.Gravity.BOTTOM | android.view.Gravity.CENTER_HORIZONTAL;
            params.bottomMargin = Math.round(24 * host.getResources().getDisplayMetrics().density);
        }
        return params;
    }

    private static void ensureWindowSize(androidx.activity.ComponentActivity host) {
        if (windowedW <= 0 || windowedH <= 0) {
            android.util.DisplayMetrics dm = host.getResources().getDisplayMetrics();
            windowedW = dm.widthPixels * 92 / 100;
            windowedH = dm.heightPixels * 72 / 100;
        }
    }

    // ------------------------------------------------------------------
    // 视图构建（UI 线程）
    // ------------------------------------------------------------------

    /** 面板：chrome（后退/前进/刷新/地址栏/全屏切换/收起/关闭）+ 容器 + 手柄。 */
    private static android.view.ViewGroup buildPanel(
            androidx.activity.ComponentActivity host, android.view.ViewGroup content) {
        float density = host.getResources().getDisplayMetrics().density;
        android.content.Context ctx = host;

        // 返回键接管：面板可见时优先于聊天返回逻辑（后注册 = dispatcher 先调）。
        backCallback = new OnBackPressedCallback(false) {
            @Override
            public void handleOnBackPressed() {
                if (BrowserSession.canGoBack()) {
                    BrowserSession.goBack();
                    return;
                }
                java.util.concurrent.Executors.newSingleThreadExecutor().execute(() -> {
                    synchronized (NativeBridge.OVERLAY_SERIAL) {
                        try {
                            BrowserSession.detach();
                        } catch (Throwable ignored) {
                        }
                        collapse(ctx);
                    }
                });
            }
        };
        host.getOnBackPressedDispatcher().addCallback(backCallback);

        android.widget.FrameLayout root = new android.widget.FrameLayout(ctx);
        root.setFocusableInTouchMode(true);
        root.setClickable(true);

        android.widget.LinearLayout column = new android.widget.LinearLayout(ctx);
        column.setOrientation(android.widget.LinearLayout.VERTICAL);

        // ---- 窗口态拖拽条（按住移动整个面板；全屏态隐藏）----
        android.widget.TextView dragStrip = new android.widget.TextView(ctx);
        dragStrip.setTag("drag_strip");
        dragStrip.setText("⋯⋯");
        dragStrip.setTextColor(0xFF5B6472);
        dragStrip.setTextSize(11);
        dragStrip.setGravity(android.view.Gravity.CENTER);
        dragStrip.setVisibility(android.view.View.GONE);
        dragStrip.setOnTouchListener(new android.view.View.OnTouchListener() {
            private float downX, downY;
            private float startPanelX, startPanelY;

            @Override
            public boolean onTouch(android.view.View v, android.view.MotionEvent event) {
                android.view.ViewGroup content = (android.view.ViewGroup) v.getRootView()
                        .findViewById(android.R.id.content);
                switch (event.getActionMasked()) {
                    case android.view.MotionEvent.ACTION_DOWN:
                        downX = event.getRawX();
                        downY = event.getRawY();
                        startPanelX = panel != null ? panel.getX() : 0;
                        startPanelY = panel != null ? panel.getY() : 0;
                        return true;
                    case android.view.MotionEvent.ACTION_MOVE: {
                        synchronized (BrowserOverlay.class) {
                            if (panel == null || panel.getParent() == null) {
                                return true;
                            }
                            int maxX = Math.max(0, content.getWidth() - panel.getWidth());
                            int maxY = Math.max(0, content.getHeight() - panel.getHeight());
                            winX = Math.max(0, Math.min(maxX,
                                    (int) (startPanelX + event.getRawX() - downX)));
                            winY = Math.max(0, Math.min(maxY,
                                    (int) (startPanelY + event.getRawY() - downY)));
                            currentMode = MODE_WINDOWED;
                            panel.setLayoutParams(panelParams(host, MODE_WINDOWED));
                        }
                        return true;
                    }
                    default:
                        return false;
                }
            }
        });
        column.addView(dragStrip, new android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT, dp(ctx, 16)));

        // ---- chrome 条 ----
        android.widget.LinearLayout bar = new android.widget.LinearLayout(ctx);
        bar.setOrientation(android.widget.LinearLayout.HORIZONTAL);
        bar.setGravity(android.view.Gravity.CENTER_VERTICAL);
        bar.setPadding(dp(ctx, 4), dp(ctx, 4), dp(ctx, 4), dp(ctx, 4));

        bar.addView(chromeButton(ctx, "‹", BrowserSession::goBack));
        bar.addView(chromeButton(ctx, "›", BrowserSession::goForward));
        bar.addView(chromeButton(ctx, "↻", BrowserSession::reload));

        addressBar = new android.widget.EditText(ctx);
        addressBar.setSingleLine(true);
        addressBar.setTextSize(13);
        addressBar.setTextColor(0xFFE6E6E6);
        addressBar.setHintTextColor(0xFF6B7280);
        addressBar.setHint("搜索或输入网址");
        addressBar.setImeOptions(android.view.inputmethod.EditorInfo.IME_ACTION_GO);
        addressBar.setInputType(android.text.InputType.TYPE_CLASS_TEXT
                | android.text.InputType.TYPE_TEXT_VARIATION_URI);
        android.graphics.drawable.GradientDrawable addrBg =
                new android.graphics.drawable.GradientDrawable();
        addrBg.setColor(0xFF232833);
        addrBg.setCornerRadius(dp(ctx, 20));
        addressBar.setBackground(addrBg);
        addressBar.setPadding(dp(ctx, 12), 0, dp(ctx, 12), 0);
        android.widget.LinearLayout.LayoutParams addrParams =
                new android.widget.LinearLayout.LayoutParams(0, dp(ctx, 40), 1f);
        addrParams.setMargins(dp(ctx, 4), 0, dp(ctx, 4), 0);
        addressBar.setLayoutParams(addrParams);
        addressBar.setOnEditorActionListener((v, actionId, event) -> {
            BrowserSession.loadFromAddressBar(addressBar.getText().toString());
            addressBar.clearFocus();
            return true;
        });
        bar.addView(addressBar);

        bar.addView(chromeButton(ctx, "⛶", () -> toggleMode(host)));
        bar.addView(chromeButton(ctx, "⌄", () -> {
            // 收起：面板移出视图树（agent 不受影响；页面保持加载；与 Rust 无
            // 信号通路，过码恢复由 park 轮询自行发现）。
            java.util.concurrent.Executors.newSingleThreadExecutor().execute(() -> {
                synchronized (NativeBridge.OVERLAY_SERIAL) {
                    try {
                        String detach = BrowserSession.detach();
                        if (detach != null && !detach.contains("\"ok\":true")) {
                            android.util.Log.w("BrowserOverlay", "detach on dock: " + detach);
                        }
                        collapse(ctx);
                    } catch (Throwable error) {
                        android.util.Log.e("BrowserOverlay", "dock action failed", error);
                    }
                }
            });
        }));
        bar.addView(chromeButton(ctx, "✕", () -> {
            // 关闭会话：销毁 WebView → DevTools socket 失效 → Rust park 的取消臂
            // 报「用户取消了过码」。工作线程执行（主线程 post + latch 会死锁）。
            java.util.concurrent.Executors.newSingleThreadExecutor().execute(() -> {
                synchronized (NativeBridge.OVERLAY_SERIAL) {
                    try {
                        BrowserSession.detach();
                    } catch (Throwable ignored) {
                    }
                    try {
                        BrowserSession.stop();
                    } catch (Throwable ignored) {
                    }
                    try {
                        dismiss(ctx);
                    } catch (Throwable ignored) {
                    }
                }
            });
        }));
        column.addView(bar, new android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                android.view.ViewGroup.LayoutParams.WRAP_CONTENT));

        // ---- 进度条 ----
        progressBar = new android.widget.ProgressBar(ctx, null,
                android.R.attr.progressBarStyleHorizontal);
        progressBar.setMax(100);
        progressBar.setProgress(0);
        progressBar.setVisibility(android.view.View.GONE);
        progressBar.getProgressDrawable().setColorFilter(
                0xFF6C7BFF, android.graphics.PorterDuff.Mode.SRC_IN);
        column.addView(progressBar, new android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT, dp(ctx, 3)));

        // ---- WebView 容器 ----
        webviewHost = new android.widget.FrameLayout(ctx);
        webviewHost.setLayoutParams(new android.widget.LinearLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT, 0, 1f));
        column.addView(webviewHost);

        root.addView(column, new android.widget.FrameLayout.LayoutParams(
                android.view.ViewGroup.LayoutParams.MATCH_PARENT,
                android.view.ViewGroup.LayoutParams.MATCH_PARENT));

        // ---- 窗口态右下角拖拽手柄 ----
        android.widget.TextView grip = new android.widget.TextView(ctx);
        grip.setTag("resize_grip");
        grip.setText("◢");
        grip.setTextColor(0xFF9AA3B2);
        grip.setTextSize(14);
        grip.setGravity(android.view.Gravity.CENTER);
        int gripSize = dp(ctx, 30);
        android.widget.FrameLayout.LayoutParams gripParams =
                new android.widget.FrameLayout.LayoutParams(gripSize, gripSize,
                        android.view.Gravity.BOTTOM | android.view.Gravity.END);
        grip.setLayoutParams(gripParams);
        grip.setOnTouchListener(new android.view.View.OnTouchListener() {
            private float downX, downY;
            private int startW, startH;

            @Override
            public boolean onTouch(android.view.View v, android.view.MotionEvent event) {
                android.util.DisplayMetrics dm = host.getResources().getDisplayMetrics();
                switch (event.getActionMasked()) {
                    case android.view.MotionEvent.ACTION_DOWN:
                        downX = event.getRawX();
                        downY = event.getRawY();
                        ensureWindowSize(host);
                        startW = windowedW;
                        startH = windowedH;
                        return true;
                    case android.view.MotionEvent.ACTION_MOVE: {
                        synchronized (BrowserOverlay.class) {
                            windowedW = Math.max(dm.widthPixels * 40 / 100, Math.min(
                                    dm.widthPixels, (int) (startW + event.getRawX() - downX)));
                            windowedH = Math.max(dm.heightPixels * 40 / 100, Math.min(
                                    dm.heightPixels, (int) (startH + event.getRawY() - downY)));
                            currentMode = MODE_WINDOWED;
                            if (panel != null && panel.getParent() != null) {
                                panel.setLayoutParams(panelParams(host, MODE_WINDOWED));
                            }
                        }
                        return true;
                    }
                    default:
                        return false;
                }
            }
        });
        root.addView(grip);

        return root;
    }

    /** chrome 胶囊钮（字符图标）：与地址栏同高的胶囊底、按压变亮，触达 44dp。 */
    private static android.widget.TextView chromeButton(
            android.content.Context ctx, String glyph, Runnable action) {
        android.widget.TextView button = new android.widget.TextView(ctx);
        button.setText(glyph);
        button.setTextColor(0xFFD5DAE3);
        button.setTextSize(19);
        button.setGravity(android.view.Gravity.CENTER);
        android.graphics.drawable.GradientDrawable bg =
                new android.graphics.drawable.GradientDrawable();
        bg.setColor(0xFF232833);
        bg.setCornerRadius(dp(ctx, 20));
        button.setBackground(bg);
        android.widget.LinearLayout.LayoutParams params =
                new android.widget.LinearLayout.LayoutParams(dp(ctx, 40), dp(ctx, 40));
        params.setMargins(dp(ctx, 2), 0, dp(ctx, 2), 0);
        button.setLayoutParams(params);
        // 按压反馈：半透明高亮（无 ripple 资源，用 selector 语义的最简实现）。
        button.setOnTouchListener((v, event) -> {
            if (event.getActionMasked() == android.view.MotionEvent.ACTION_DOWN) {
                bg.setColor(0xFF39414F);
            } else if (event.getActionMasked() == android.view.MotionEvent.ACTION_UP
                    || event.getActionMasked() == android.view.MotionEvent.ACTION_CANCEL) {
                bg.setColor(0xFF232833);
            }
            return false; // 不消费：onClick 仍触发
        });
        button.setOnClickListener(v -> action.run());
        return button;
    }

    /** PageListener 转发（WebViewClient 主线程回调）：地址栏 + 进度条。 */
    private static void onPageState(String url, int progress) {
        UI.post(() -> {
            try {
                if (addressBar != null && url != null
                        && !url.equals("about:blank") && !addressBar.hasFocus()) {
                    addressBar.setText(url);
                }
                if (progressBar != null) {
                    if (progress < 100) {
                        progressBar.setVisibility(android.view.View.VISIBLE);
                        progressBar.setProgress(progress);
                    } else {
                        progressBar.setProgress(100);
                        progressBar.setVisibility(android.view.View.GONE);
                    }
                }
            } catch (Throwable ignored) {
            }
        });
    }

    private static int dp(android.content.Context ctx, int value) {
        return Math.round(value * ctx.getResources().getDisplayMetrics().density);
    }

    // ------------------------------------------------------------------
    // 过码提醒通知（点开进应用并展开面板）
    // ------------------------------------------------------------------

    private static void ensureChannel(android.content.Context ctx) {
        android.app.NotificationManager manager =
                ctx.getSystemService(android.app.NotificationManager.class);
        if (manager != null) {
            manager.createNotificationChannel(new android.app.NotificationChannel(
                    CHANNEL_ID,
                    ctx.getString(R.string.browser_channel),
                    android.app.NotificationManager.IMPORTANCE_HIGH));
        }
    }

    private static final String CHANNEL_ID = "rclaw_browser";
    private static final int CAPTCHA_NOTIFICATION_ID = 1003;

    /** 风控页过码提醒（高优横幅；点击拉起 MainActivity 并展开面板）。 */
    static void notifyCaptcha(android.content.Context context, String url) {
        try {
            android.content.Context ctx = context.getApplicationContext();
            ensureChannel(ctx);
            android.content.Intent open = new android.content.Intent(ctx, MainActivity.class);
            open.putExtra("open_browser", true);
            open.addFlags(android.content.Intent.FLAG_ACTIVITY_NEW_TASK
                    | android.content.Intent.FLAG_ACTIVITY_SINGLE_TOP);
            android.app.PendingIntent pending = android.app.PendingIntent.getActivity(
                    ctx, 32, open,
                    android.app.PendingIntent.FLAG_UPDATE_CURRENT
                            | android.app.PendingIntent.FLAG_IMMUTABLE);
            String text = ctx.getString(R.string.browser_captcha_hint);
            if (url != null && !url.isEmpty()) {
                text = url + "\n" + text;
            }
            android.app.Notification notification =
                    new android.app.Notification.Builder(ctx, CHANNEL_ID)
                            .setSmallIcon(R.drawable.ic_bagua)
                            .setContentTitle(ctx.getString(R.string.browser_captcha_title))
                            .setContentText(text)
                            .setStyle(new android.app.Notification.BigTextStyle().bigText(text))
                            .setContentIntent(pending)
                            .setAutoCancel(true)
                            .build();
            android.app.NotificationManager manager =
                    ctx.getSystemService(android.app.NotificationManager.class);
            if (manager != null) {
                manager.notify(CAPTCHA_NOTIFICATION_ID, notification);
            }
        } catch (Throwable error) {
            android.util.Log.e("BrowserOverlay", "captcha notify failed", error);
        }
    }
}
