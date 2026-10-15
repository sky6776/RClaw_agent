package ai.rclaw.app

import android.Manifest
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.core.app.ActivityCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    AppConfig.trace(this, "activity-onCreate")
    // 不开 edge-to-edge：WebView 83 不支持 safe-area inset env，内容会延伸进
    // 状态栏/手势条区域造成重叠。默认布局让系统自动避让。
    // v2 入口接线（设计 §5 阶段 4）：先同步记录宿主上下文（Rust 侧 home 解析与
    // GUI 装配依赖 filesDir，必须在 tauri app setup 前就绪），再拉起前台服务——
    // 服务负责安装内置 skill、组装启动配置并 nativeStart（含无头共享运行时）。
    NativeBridge.setContext(application)
    NativeBridge.init(filesDir)
    CaptureBridge.activity = this
    // 浏览器面板（应用内原生视图，盖在聊天 UI 上）的宿主窗口：panel 加进本
    // Activity 的视图树，无需悬浮窗权限。setActivity 会复位 attached 并把
    // WebView 从旧窗口摘回离屏（面板展开期间 Activity 重建的场景）。
    BrowserOverlay.setActivity(this)
    ShareReceiver.handle(this, intent)
    openBrowserPanelIfRequested(intent)
    val service = Intent(this, RClawService::class.java).setAction(RClawService.ACTION_START)
    if (Build.VERSION.SDK_INT >= 26) {
      startForegroundService(service)
    } else {
      startService(service)
    }
    // Android 13+：前台服务常驻通知与「技能待批准」通知都需要 POST_NOTIFICATIONS。
    if (Build.VERSION.SDK_INT >= 33) {
      ActivityCompat.requestPermissions(
        this, arrayOf(Manifest.permission.POST_NOTIFICATIONS), 1001)
    }
    super.onCreate(savedInstanceState)
  }

  /** 返回键接管：后注册 = dispatcher 先调用，wry 内建的 canGoBack/goBack 回调
   *  不再执行（华为 WebView 的原生 goBack() 对 pushState 同文档历史条目会原生
   *  崩溃，crashpad 真机复现）。改为直接调前端 __rclawBack 决策：关最上层弹层
   *  → 呼出侧边栏 → 退出应用；前端未装载（向导/启动期）时按系统默认退出。 */
  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    onBackPressedDispatcher.addCallback(this, object : OnBackPressedCallback(true) {
      override fun handleOnBackPressed() {
        webView.evaluateJavascript(
          "if (typeof window.__rclawBack === 'function') { window.__rclawBack(); 'handled' } else { 'none' }"
        ) { result ->
          if (result?.contains("none") == true) {
            finish()
          }
        }
      }
    })
    // 通知点击进入（open_browser extra）：UI 已就绪，展开浏览器面板
    //（过码/手动上网入口）。
    if (pendingOpenBrowser) {
      pendingOpenBrowser = false
      NativeBridge.browserOverlayExpandAsync()
    }
  }

  /** 系统分享（应用已在前台时经 onNewIntent 送达，singleTask 不会重建 Activity）。 */
  override fun onNewIntent(intent: Intent) {
    AppConfig.trace(this, "activity-onNewIntent action=${intent.action}")
    ShareReceiver.handle(this, intent)
    // Activity 存活路径：webview 必已就绪，直接展开。
    if (intent?.getBooleanExtra("open_browser", false) == true) {
      NativeBridge.browserOverlayExpandAsync()
    }
    super.onNewIntent(intent)
  }

  /** 冷启动路径：记下请求，onWebViewCreate（UI 就绪）时展开。 */
  private fun openBrowserPanelIfRequested(intent: Intent?) {
    if (intent?.getBooleanExtra("open_browser", false) == true) {
      pendingOpenBrowser = true
      AppConfig.trace(this, "browser-panel open requested (pending)")
    }
  }

  private var pendingOpenBrowser = false

  /** 相机/录音权限结果：交给 CaptureBridge（录音授权后自动开录）。 */
  override fun onRequestPermissionsResult(
    requestCode: Int, permissions: Array<out String>, grantResults: IntArray
  ) {
    super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    val granted = grantResults.isNotEmpty() &&
      grantResults[0] == android.content.pm.PackageManager.PERMISSION_GRANTED
    CaptureBridge.onRequestPermissionsResult(requestCode, granted)
  }

  /** 系统相机（拍照/录视频）结果：CaptureBridge 落盘并写 capture_result.json。 */
  override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
    super.onActivityResult(requestCode, resultCode, data)
    CaptureBridge.onActivityResult(this, requestCode, resultCode, data)
  }

  /** GUI 前后台上报：turn 完成提醒只发给不在前台的用户（正看着聊天流时
   *  弹横幅只会打扰）。 */
  override fun onResume() {
    super.onResume()
    NativeBridge.setForeground(true)
  }

  override fun onPause() {
    NativeBridge.setForeground(false)
    super.onPause()
  }

  override fun onDestroy() {
    AppConfig.trace(this, "activity-onDestroy isFinishing=$isFinishing")
    // 配置变更重建的时序是「新实例 onCreate → 旧实例 onDestroy」：无条件清空会把
    // 新实例刚登记的引用抹掉，之后拍照/录音永远拿不到 Activity。只清自己的。
    if (CaptureBridge.activity === this) {
      CaptureBridge.activity = null
    }
    if (BrowserOverlay.activity === this) {
      BrowserOverlay.activity = null
    }
    super.onDestroy()
  }
}
