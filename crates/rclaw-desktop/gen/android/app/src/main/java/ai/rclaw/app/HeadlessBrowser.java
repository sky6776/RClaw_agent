package ai.rclaw.app;

/**
 * 无头浏览器取页内核（web_browser 工具）。
 *
 * Rust jni_bridge::browse → NativeBridge.browse → 本类 browse()：
 * 在主线程新建一个离屏系统 WebView（应用 context，不挂任何窗口），
 * 加载完成静置 500ms 后用 JS 提取（引擎感知的搜索结果列表 + 正文），
 * 用完即销毁，不跨调用持有状态。
 *
 * 调用线程约定：Rust 的阻塞工作线程（禁止主线程——WebView 工作全在
 * 主线程，本方法靠 CountDownLatch 等待，主线程调用会自死锁）。
 */
final class HeadlessBrowser {

    private HeadlessBrowser() {}

    /** 桌面版 Chrome UA：拿稳定的服务端渲染 SERP 结构（移动版布局选择器随版本漂移）。 */
    private static final String DESKTOP_UA =
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
                    + "(KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

    /** 页面提取 JS：按域名分派（bing/baidu/google 结果列表选择器，过滤广告条目），
     *  正文按 Readability 式启发式取段落密度最高的主文区域；返回 JSON 字符串
     *  （evaluateJavascript 回调拿到的是它的 JSON 字符串字面量）。 */
    private static final String EXTRACT_JS = "(function () {\n"
            + "  var out = { title: document.title || \"\", url: location.href, text: \"\", results: [] };\n"
            + "  function clean(s) { return (s || \"\").replace(/\\s+/g, \" \").trim(); }\n"
            + "  function adMarked(el) {\n"
            + "    return !!el.querySelector(\"[class*='ec-'], [class*='ec_'], [class*='tuiguang'], [class*='b_ad']\");\n"
            + "  }\n"
            + "  var host = location.hostname || \"\";\n"
            + "  var seen = {};\n"
            + "  function safeDecode(s) { try { return decodeURIComponent(s); } catch (e) { return \"\"; } }\n"
            + "  function push(title, url, snippet) {\n"
            + "    if (!url || seen[url]) return;\n"
            + "    seen[url] = true;\n"
            + "    out.results.push({ title: clean(title).slice(0, 120), url: url, snippet: clean(snippet).slice(0, 240) });\n"
            + "  }\n"
            + "  if (host.indexOf(\"bing.com\") >= 0) {\n"
            + "    var items = document.querySelectorAll(\"#b_results li.b_algo\");\n"
            + "    for (var i = 0; i < items.length && out.results.length < 10; i++) {\n"
            + "      var li = items[i];\n"
            + "      if (adMarked(li)) continue;\n"
            + "      var a = li.querySelector(\"h2 a\");\n"
            + "      if (!a) continue;\n"
            + "      var cap = li.querySelector(\".b_caption p, .b_algoSlug, .b_caption\");\n"
            + "      push(a.textContent, a.href, cap ? cap.textContent : \"\");\n"
            + "    }\n"
            + "  } else if (host.indexOf(\"google.\") >= 0) {\n"
            + "    var links = document.querySelectorAll(\"a h3\");\n"
            + "    for (var g = 0; g < links.length && out.results.length < 10; g++) {\n"
            + "      var gh = links[g];\n"
            + "      var ga = gh.closest(\"a\");\n"
            + "      if (!ga || !ga.href) continue;\n"
            + "      var gurl = ga.href;\n"
            + "      if (gurl.indexOf(\"/aclk\") >= 0 || gurl.indexOf(\"googleadservices\") >= 0) continue;\n"
            + "      var gm = gurl.match(/^.*\\/url\\?q=([^&]+)/);\n"
            + "      if (gm) { gurl = safeDecode(gm[1]); if (!gurl) continue; }\n"
            + "      if (gurl.indexOf(\"http\") !== 0) continue;\n"
            + "      var gbox = gh.closest(\"div.g\") || gh.parentElement;\n"
            + "      var gsnip = gbox ? gbox.querySelector(\".VwiC3b\") : null;\n"
            + "      push(gh.textContent, gurl, gsnip ? gsnip.textContent : \"\");\n"
            + "    }\n"
            + "  } else if (host.indexOf(\"baidu.com\") >= 0) {\n"
            + "    var links = document.querySelectorAll(\"#content_left h3 a\");\n"
            + "    for (var j = 0; j < links.length && out.results.length < 10; j++) {\n"
            + "      var a2 = links[j];\n"
            + "      var box = a2.closest(\".result, .result-op, .c-container\");\n"
            + "      if (!box || adMarked(box)) continue;\n"
            + "      if (/e\\.baidu\\.com|promotion\\.baidu\\.com/.test(a2.href)) continue;\n"
            + "      var abs = box.querySelector(\".c-abstract, [class*=content-right]\");\n"
            + "      push(a2.textContent, a2.href, abs ? abs.textContent : \"\");\n"
            + "    }\n"
            + "  }\n"
            // --- 正文：Readability 式主文区域打分（段落文字量密度）---
            + "  function boilerplate(el) {\n"
            + "    var sig = ((el.className && el.className.toString()) || \"\") + \" \" + (el.id || \"\");\n"
            + "    return /(nav|menu|footer|sidebar|comment|advert|ads?-|promo|banner|share|breadcrumb|copyright|related|recommend)/i.test(sig);\n"
            + "  }\n"
            + "  var nodes = document.querySelectorAll(\"article, main, [role=main]\");\n"
            + "  if (!nodes.length) nodes = document.querySelectorAll(\"div, section\");\n"
            + "  var best = null, bestScore = 0;\n"
            + "  for (var k = 0; k < nodes.length && k < 800; k++) {\n"
            + "    var n = nodes[k];\n"
            + "    if (boilerplate(n)) continue;\n"
            + "    var ps = n.querySelectorAll(\"p\");\n"
            + "    var score = 0;\n"
            + "    for (var m = 0; m < ps.length && m < 200; m++) {\n"
            + "      score += Math.min(ps[m].textContent.length, 1000);\n"
            + "    }\n"
            + "    if (score > bestScore) { bestScore = score; best = n; }\n"
            + "  }\n"
            + "  var root = (best && bestScore > 200) ? best : document.body;\n"
            + "  var body = \"\";\n"
            + "  if (root) {\n"
            + "    // innerText 依赖渲染树；离屏未挂窗口的 WebView 在个别机型上\n"
            + "    // 返回空，退回 textContent（不去不可见节点，聊胜于无）。\n"
            + "    body = root.innerText || root.textContent || \"\";\n"
            + "  }\n"
            + "  out.text = body.replace(/[ \\t]+\\n/g, \"\\n\").replace(/\\n{3,}/g, \"\\n\\n\").trim().slice(0, 20000);\n"
            + "  return JSON.stringify(out);\n"
            + "})()";

    /**
     * 取一页。返回 JSON 文本：
     * 成功 {"ok":true,"page":{title,url,text,results[]}}，失败 {"ok":false,"error":"..."}。
     */
    static String browse(android.content.Context context, String url, int timeoutMs) {
        if (context == null) {
            return errorJson("应用未就绪（无 context）");
        }
        if (android.os.Looper.myLooper() == android.os.Looper.getMainLooper()) {
            return errorJson("browse 不允许在主线程调用（会自死锁）");
        }
        if (url == null || url.isEmpty()) {
            return errorJson("目标 URL 为空");
        }
        final android.os.Handler main = new android.os.Handler(android.os.Looper.getMainLooper());
        final java.util.concurrent.CountDownLatch latch = new java.util.concurrent.CountDownLatch(1);
        final String[] out = new String[1];
        final android.webkit.WebView[] holder = new android.webkit.WebView[1];
        // 静置计时任务（方法层持有：超时销毁时要移除，防对已销毁 WebView 再提取）。
        final Runnable[] settle = new Runnable[1];
        // 内容为空时的延时重提任务（同上，销毁时要移除）。
        final Runnable[] extract = new Runnable[1];

        main.post(() -> {
            try {
                android.webkit.WebView webView = new android.webkit.WebView(context);
                holder[0] = webView;
                android.webkit.WebSettings settings = webView.getSettings();
                settings.setJavaScriptEnabled(true);
                settings.setDomStorageEnabled(true);
                settings.setUserAgentString(DESKTOP_UA);
                // 只取文字：不拉图，省流量省内存。
                settings.setLoadsImagesAutomatically(false);
                settings.setBlockNetworkImage(true);
                settings.setGeolocationEnabled(false);
                settings.setSaveFormData(false);
                // 离屏强制布局：innerText 依赖渲染树。
                webView.measure(
                        android.view.View.MeasureSpec.makeMeasureSpec(1280,
                                android.view.View.MeasureSpec.EXACTLY),
                        android.view.View.MeasureSpec.makeMeasureSpec(2400,
                                android.view.View.MeasureSpec.EXACTLY));
                webView.layout(0, 0, 1280, 2400);

                final boolean[] done = {false};
                final int[] attempt = {0};
                webView.setWebViewClient(new android.webkit.WebViewClient() {
                    @Override
                    public void onPageFinished(android.webkit.WebView view, String finishedUrl) {
                        if (done[0]) {
                            return;
                        }
                        // 重定向链会多次触发 onPageFinished；每次重置静置计时。
                        if (settle[0] != null) {
                            main.removeCallbacks(settle[0]);
                        }
                        settle[0] = () -> {
                            if (done[0]) {
                                return;
                            }
                            done[0] = true;
                            // 开源自动化工具的「等待内容」实践：首提为空（JS 未就绪）
                            // 时延时重提，至多 2 次；有结果立即返回。
                            extract[0] = () -> {
                                try {
                                    view.evaluateJavascript(EXTRACT_JS, value -> {
                                        String wrapped = wrapResult(value);
                                        if (looksEmpty(wrapped) && attempt[0] < 2) {
                                            attempt[0]++;
                                            main.postDelayed(extract[0], 700);
                                            return;
                                        }
                                        out[0] = wrapped;
                                        latch.countDown();
                                    });
                                } catch (RuntimeException destroyed) {
                                    // 超时分支已销毁 WebView：如常收尾，不带崩主线程。
                                    out[0] = errorJson("页面已销毁（提取太晚）");
                                    latch.countDown();
                                }
                            };
                            extract[0].run();
                        };
                        main.postDelayed(settle[0], 500);
                    }

                    @Override
                    public void onReceivedError(android.webkit.WebView view,
                            android.webkit.WebResourceRequest request,
                            android.webkit.WebResourceError error) {
                        if (done[0] || !request.isForMainFrame()) {
                            return;
                        }
                        done[0] = true;
                        out[0] = errorJson("页面加载失败：" + error.getDescription());
                        latch.countDown();
                    }
                });
                webView.loadUrl(url);
            } catch (RuntimeException error) {
                android.util.Log.e("HeadlessBrowser", "browse failed", error);
                out[0] = errorJson("WebView 创建失败：" + error.getMessage());
                latch.countDown();
            }
        });

        boolean finished;
        try {
            finished = latch.await(timeoutMs, java.util.concurrent.TimeUnit.MILLISECONDS);
        } catch (InterruptedException interrupted) {
            Thread.currentThread().interrupt();
            finished = false;
        }
        // 成败与否都销毁（主线程异步执行；先移除挂起的静置/重提任务，防止对已
        // 销毁 WebView 再触发提取——任务内另有 try/catch 兜底）。
        main.post(() -> {
            if (settle[0] != null) {
                main.removeCallbacks(settle[0]);
            }
            if (extract[0] != null) {
                main.removeCallbacks(extract[0]);
            }
            android.webkit.WebView webView = holder[0];
            if (webView == null) {
                return;
            }
            try {
                webView.stopLoading();
                webView.destroy();
            } catch (RuntimeException ignored) {
                android.util.Log.w("HeadlessBrowser", "webView destroy failed");
            }
        });
        if (!finished) {
            return errorJson("加载超时（" + timeoutMs + "ms）");
        }
        return out[0] != null ? out[0] : errorJson("页面无返回");
    }

    /** 提取结果是否「内容为空」（结果列表与正文都空）——驱动延时重提。 */
    private static boolean looksEmpty(String wrapped) {
        if (wrapped == null) {
            return true;
        }
        return wrapped.contains("\"results\":[]") && wrapped.contains("\"text\":\"\"");
    }

    /** evaluateJavascript 回调值（JSON 字符串字面量）→ 外层 {"ok":true,"page":...}。 */
    private static String wrapResult(String raw) {
        if (raw == null) {
            return errorJson("页面脚本无返回");
        }
        try {
            Object unwrapped = new org.json.JSONTokener(raw).nextValue();
            String inner = unwrapped instanceof String ? (String) unwrapped : raw;
            return "{\"ok\":true,\"page\":" + inner + "}";
        } catch (org.json.JSONException error) {
            android.util.Log.e("HeadlessBrowser", "wrapResult failed: " + raw);
            return errorJson("页面提取结果异常");
        }
    }

    private static String errorJson(String message) {
        return "{\"ok\":false,\"error\":"
                + org.json.JSONObject.quote(message == null ? "" : message) + "}";
    }
}
