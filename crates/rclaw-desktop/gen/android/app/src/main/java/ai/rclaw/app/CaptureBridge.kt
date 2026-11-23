package ai.rclaw.app

import android.Manifest
import android.app.Activity
import android.content.ContentValues
import android.content.Intent
import android.content.pm.PackageManager
import android.media.MediaCodec
import android.media.MediaExtractor
import android.media.MediaFormat
import android.media.MediaRecorder
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.content.FileProvider
import java.io.File
import java.io.FileOutputStream

/** 相机/麦克风采集：拍照（EXTRA_OUTPUT 直落工作区）、录视频（结果流拷贝）、
 *  录音（MediaRecorder AAC）与保存到相册（MediaStore）。结果统一写
 *  files/capture_result.json（单消费），GUI 经 consume_capture_result 轮询。 */
object CaptureBridge {
    private const val REQ_PHOTO = 2001
    private const val REQ_VIDEO = 2002
    private const val REQ_MIC = 2003
    private const val REQ_WRITE = 2004
    private const val REQ_CAMERA = 2005
    /** 扫码相机每次拉起用不同的 requestCode（间隔一段区间，避开上面几个）：旧请求迟到的
     *  回调据此认出自己的 requestId，不会冒充新一轮。 */
    private const val REQ_SCAN_BASE = 2100
    private const val REQ_SCAN_SPAN = 900

    /** MainActivity 生命周期持有；系统相机/麦克风授权都需要 Activity。 */
    @JvmStatic var activity: Activity? = null

    private var pendingPhotoFile: File? = null
    /** 为哪一轮、什么时候拉起过扫码相机（elapsedRealtime 毫秒），只用来挡同一轮的连点。 */
    private var scanLaunchedFor: String? = null
    private var scanLaunchedAt = -10_000L
    private var recorder: MediaRecorder? = null
    private var recordFile: File? = null
    private var startRecordWhenGranted = false
    /** 相机权限授权后自动续跑的采集类型（photo|video|scan），null = 无待续跑。 */
    private var pendingCaptureKind: String? = null
    /** 正在等相机权限的扫码请求；取消后标记会被删掉或换成下一轮，授权回调据此作废。 */
    private var pendingScanRequestId: String? = null
    /** 已拉起相机的 requestCode → requestId：相机回来时认出是哪一轮，旧回调不冒充新请求。 */
    private val scanLaunches = HashMap<Int, String>()
    private var nextScanCode = REQ_SCAN_BASE

    // ------------------------------------------------------------------
    // 拍照 / 拍视频
    // ------------------------------------------------------------------

    /** 起 System Camera 拍照；EXTRA_OUTPUT 直写 agent/uploads/，全分辨率。
     *  返回 "ok" / "permission_requested"（授权后自动续跑）/ "error:<原因>"。
     *  注意：应用声明了 CAMERA 权限就必须先运行时授权，否则系统直接拒
     *  （SecurityException: revoked permission android.permission.CAMERA）。 */
    @JvmStatic
    @Synchronized
    fun capturePhoto(): String {
        val activity = activity ?: return "error:Activity 未就绪"
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.CAMERA)
                != PackageManager.PERMISSION_GRANTED) {
            pendingCaptureKind = "photo"
            pendingScanRequestId = null
            ActivityCompat.requestPermissions(
                activity, arrayOf(Manifest.permission.CAMERA), REQ_CAMERA)
            return "permission_requested"
        }
        return try {
            val file = captureFile(activity, "capture-photo", "jpg")
            pendingPhotoFile = file
            val uri = FileProvider.getUriForFile(
                activity, activity.packageName + ".fileprovider", file)
            val intent = Intent(MediaStore.ACTION_IMAGE_CAPTURE)
                .putExtra(MediaStore.EXTRA_OUTPUT, uri)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or
                          Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            activity.startActivityForResult(intent, REQ_PHOTO)
            "ok"
        } catch (e: Exception) {
            AppConfig.trace(activity, "capture-photo-error ${e.message}")
            "error:${e.message}"
        }
    }

    /** 扫配对码时拍下的照片。它等同于配对码本身，所以不进工作区：落在
     *  files/sync/scan/qr-<requestId>.jpg（同步目录是助手文件工具的禁区，文件面板也看不
     *  到）。文件名带本轮的请求标识：旧请求迟到的回调只会删自己那张，不会碰新一轮。 */
    private fun scanFile(activity: Activity, requestId: String): File {
        val dir = File(activity.filesDir, "sync/scan").apply { mkdirs() }
        return File(dir, "qr-$requestId.jpg")
    }

    /** 删掉扫码目录里的全部照片：认不出是哪一轮的迟到回调、旧版本固定名都走这里。 */
    private fun removeAllScanPhotos(activity: Activity) {
        val dir = File(activity.filesDir, "sync/scan")
        dir.listFiles()?.forEach { if (it.isFile) it.delete() }
    }

    /** requestId 由 Rust 侧生成、会拼进文件名，只接受十六进制与 `-`。 */
    private fun validScanRequestId(requestId: String): Boolean =
        requestId.isNotEmpty() && requestId.length <= 64 &&
            requestId.all { it in '0'..'9' || it in 'a'..'f' || it in 'A'..'F' || it == '-' }

    /** 标记文件里记的是不是这一轮：取消、新一轮扫码都会让它变得对不上。 */
    private fun scanRequested(activity: Activity, requestId: String): Boolean {
        val marker = File(activity.filesDir, "capture_scan.pending")
        val raw = try {
            marker.readText()
        } catch (e: Exception) {
            return false
        }
        return raw.trim() == requestId
    }

    /** 标记还是这一轮时删掉它（结果写不成时用来交付终态）。 */
    private fun removeScanMarkerIf(activity: Activity, requestId: String) {
        if (!scanRequested(activity, requestId)) return
        File(activity.filesDir, "capture_scan.pending").delete()
    }

    /** 每次拉起相机换一个 requestCode：相机回来时才能分清是哪一轮的。 */
    private fun nextScanRequestCode(): Int {
        repeat(REQ_SCAN_SPAN) {
            val code = nextScanCode
            nextScanCode =
                if (code + 1 >= REQ_SCAN_BASE + REQ_SCAN_SPAN) REQ_SCAN_BASE else code + 1
            if (!scanLaunches.containsKey(code)) return code
        }
        // 极不可能：相机一直没把结果送回来，区间里的 code 都占着。丢掉最老的一条。
        scanLaunches.keys.firstOrNull()?.let { scanLaunches.remove(it) }
        return nextScanRequestCode()
    }

    /** 真正拉起扫码相机。返回 "ok" / "error:<原因>"；标记已经不是这一轮时拒绝启动。 */
    private fun launchScanLocked(activity: Activity, requestId: String): String {
        // 连点两下只起一次相机。按「同一轮 + 时间」判断而不是记一个「进行中」的标志：相机
        // 没有把结果送回来的时候（个别机型上会发生），标志会一直挂着，之后就再也扫不了了；
        // 取消后马上重扫是新一轮，不能受上一轮的时间差影响。
        val now = android.os.SystemClock.elapsedRealtime()
        if (requestId == scanLaunchedFor && now - scanLaunchedAt < 1500) return "ok"
        // 取消之后（标记没了）或已经开始新一轮：迟到的授权 / 续跑不能再拉相机。
        if (!scanRequested(activity, requestId)) return "error:扫码请求已失效"
        val code = nextScanRequestCode()
        return try {
            val file = scanFile(activity, requestId)
            file.delete()
            val uri = FileProvider.getUriForFile(
                activity, activity.packageName + ".fileprovider", file)
            val intent = Intent(MediaStore.ACTION_IMAGE_CAPTURE)
                .putExtra(MediaStore.EXTRA_OUTPUT, uri)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION or
                          Intent.FLAG_GRANT_WRITE_URI_PERMISSION)
            scanLaunches[code] = requestId
            activity.startActivityForResult(intent, code)
            scanLaunchedFor = requestId
            scanLaunchedAt = now
            "ok"
        } catch (e: Exception) {
            scanLaunches.remove(code)
            AppConfig.trace(activity, "capture-scan-error ${e.message}")
            "error:${e.message}"
        }
    }

    /** 起系统相机拍配对码的二维码（多端同步的「拍照扫码」）。requestId 是这一轮的标识，
     *  Rust 侧生成并贯穿轮询/取消。返回约定同 capturePhoto。结果写 capture_result.json：
     *  {"kind":"scan","request_id":...}，用户放弃时再带 "cancelled":true。 */
    @JvmStatic
    @Synchronized
    fun captureScan(requestId: String): String {
        val activity = activity ?: return "error:Activity 未就绪"
        if (!validScanRequestId(requestId)) return "error:请求标识不合法"
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.CAMERA)
                != PackageManager.PERMISSION_GRANTED) {
            pendingCaptureKind = "scan"
            pendingScanRequestId = requestId
            ActivityCompat.requestPermissions(
                activity, arrayOf(Manifest.permission.CAMERA), REQ_CAMERA)
            return "permission_requested"
        }
        return launchScanLocked(activity, requestId)
    }

    /** 起系统相机录视频；结果经 data URI 流拷贝（EXTRA_OUTPUT 在部分 OEM 相机上不可靠）。
     *  返回约定同 capturePhoto。 */
    @JvmStatic
    @Synchronized
    fun captureVideo(): String {
        val activity = activity ?: return "error:Activity 未就绪"
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.CAMERA)
                != PackageManager.PERMISSION_GRANTED) {
            pendingCaptureKind = "video"
            pendingScanRequestId = null
            ActivityCompat.requestPermissions(
                activity, arrayOf(Manifest.permission.CAMERA), REQ_CAMERA)
            return "permission_requested"
        }
        return try {
            val intent = Intent(MediaStore.ACTION_VIDEO_CAPTURE)
                .putExtra(MediaStore.EXTRA_VIDEO_QUALITY, 1)
            activity.startActivityForResult(intent, REQ_VIDEO)
            "ok"
        } catch (e: Exception) {
            AppConfig.trace(activity, "capture-video-error ${e.message}")
            "error:${e.message}"
        }
    }

    /** Activity 结果：拍照直落文件已在位；视频从结果 URI 拷入工作区。
     *  统一写 capture_result.json（cancelled=true 表示用户放弃）。 */
    @JvmStatic
    @Synchronized
    fun onActivityResult(activity: Activity, requestCode: Int, resultCode: Int, data: Intent?) {
        when (requestCode) {
            REQ_PHOTO -> {
                val file = pendingPhotoFile
                pendingPhotoFile = null
                // 空文件视为失败：部分相机 RESULT_OK 但写盘不完整（视频分支同判）。
                if (resultCode == Activity.RESULT_OK && file != null
                        && file.isFile && file.length() > 0) {
                    finishCapture("photo", file)
                } else {
                    file?.delete()
                    finishCapture("photo", null)
                }
            }
            REQ_VIDEO -> {
                if (resultCode == Activity.RESULT_OK) {
                    val uri: Uri? = data?.data
                    val target = captureFile(activity, "capture-video", "mp4")
                    var copied = false
                    if (uri != null) {
                        try {
                            activity.contentResolver.openInputStream(uri)?.use { input ->
                                FileOutputStream(target).use { output ->
                                    input.copyTo(output, 64 * 1024)
                                }
                            }
                            copied = target.isFile && target.length() > 0
                        } catch (e: Exception) {
                            AppConfig.trace(activity, "capture-video-copy-error ${e.message}")
                        }
                    }
                    if (copied) {
                        finishCapture("video", target)
                    } else {
                        target.delete()
                        finishCapture("video", null)
                    }
                } else {
                    finishCapture("video", null)
                }
            }
            else -> {
                // 扫码相机：按 requestCode 找回拉起时的 requestId，旧请求的回调不冒充新请求。
                val requestId = scanLaunches.remove(requestCode)
                if (requestId == null) {
                    // 旧版本的固定 requestCode、或进程重建后才回来的回调：认不出是谁的，
                    // 没有别的扫码在等结果时就把照片全删掉（等同于配对码，不能留）。
                    if (scanLaunches.isEmpty()) removeAllScanPhotos(activity)
                    return
                }
                onScanResult(activity, requestId, resultCode)
            }
        }
    }

    /** 扫码相机回来了。标记已经被取消、或者换成了下一轮时，这声回调作废：照片当场删掉，
     *  不写结果。 */
    private fun onScanResult(activity: Activity, requestId: String, resultCode: Int) {
        // 相机回来了：下一次扫码不用再等连点的间隔。
        scanLaunchedFor = null
        scanLaunchedAt = -10_000L
        val file = scanFile(activity, requestId)
        if (!scanRequested(activity, requestId)) {
            file.delete()
            return
        }
        val taken = resultCode == Activity.RESULT_OK && file.isFile && file.length() > 0
        if (!taken) file.delete()
        finishScan(activity, requestId, taken)
    }

    private fun captureFile(activity: Activity, prefix: String, ext: String): File {
        val dir = File(activity.filesDir, "agent/uploads").apply { mkdirs() }
        return File(dir, "$prefix-${System.currentTimeMillis()}.$ext")
    }

    private fun finishCapture(kind: String, file: File?) {
        val activity = activity ?: return
        val json = if (file != null) {
            "{\"kind\":\"$kind\",\"file\":\"uploads/${file.name}\"}"
        } else {
            "{\"kind\":\"$kind\",\"cancelled\":true}"
        }
        if (!writeCaptureResult(activity, json)) return
        AppConfig.trace(activity, "capture-done kind=$kind file=${file?.name ?: "cancelled"}")
    }

    /** 扫码的结果。照片按 requestId 分开存放，结果里不带路径、只带这一轮的标识。结果写不
     *  成的话把照片删掉、标记清掉：没有结果，也不能让 Rust 一直等。 */
    private fun finishScan(activity: Activity, requestId: String, taken: Boolean) {
        // 写结果之前再认一次标记：取消可能刚好在这声回调进来之后发生。
        if (!scanRequested(activity, requestId)) {
            scanFile(activity, requestId).delete()
            return
        }
        val cancelled = if (taken) "" else ",\"cancelled\":true"
        val json = "{\"kind\":\"scan\",\"request_id\":\"$requestId\"$cancelled}"
        if (!writeCaptureResult(activity, json)) {
            scanFile(activity, requestId).delete()
            removeScanMarkerIf(activity, requestId)
            return
        }
        AppConfig.trace(activity, "capture-done kind=scan taken=$taken")
    }

    /** 用户拒绝相机权限：立刻清掉本轮的标记，Rust 轮询看到标记没了就交付「取消」终态，
     *  不再一直等相机。不写共享的结果文件：新一轮可能已经开始，两边不能互相覆盖。 */
    private fun finishScanDenied(activity: Activity, requestId: String) {
        removeScanMarkerIf(activity, requestId)
        AppConfig.trace(activity, "capture-done kind=scan denied")
    }

    /** 相机权限授权后拉起相机失败（没有能处理拍照 Intent 的应用、FileProvider 出错）：
     *  这条续跑路径没有调用方能拿到返回值，只能就地交付终态——删掉本轮的照片、清掉仍是
     *  这一轮的标记，Rust 轮询看到标记没了就返回「取消」。不写共享的结果文件：新一轮
     *  可能已经开始，两边不能互相覆盖。 */
    private fun finishScanLaunchFailed(activity: Activity, requestId: String) {
        scanFile(activity, requestId).delete()
        removeScanMarkerIf(activity, requestId)
        AppConfig.trace(activity, "capture-done kind=scan launch-failed")
    }

    /** 临时文件 + rename 原子提交：Rust 轮询消费端读到半截 JSON 会把文件
     *  删掉（读后即删），非原子写会整份丢失采集结果。 */
    private fun writeCaptureResult(activity: Activity, json: String): Boolean {
        return try {
            val target = File(activity.filesDir, "capture_result.json")
            val tmp = File(activity.filesDir, "capture_result.json.tmp")
            tmp.writeText(json)
            if (!tmp.renameTo(target)) {
                target.writeText(json)
                tmp.delete()
            }
            true
        } catch (e: Exception) {
            AppConfig.trace(activity, "capture-result-write-error ${e.message}")
            false
        }
    }

    // ------------------------------------------------------------------
    // 录音（MediaRecorder AAC/m4a）
    // ------------------------------------------------------------------

    /** 开始录音。返回 "ok" / "permission_requested" / "error:<原因>"。 */
    @JvmStatic
    @Synchronized
    fun recordStart(): String {
        val activity = activity ?: return "error:Activity 未就绪"
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.RECORD_AUDIO)
                != PackageManager.PERMISSION_GRANTED) {
            startRecordWhenGranted = true
            ActivityCompat.requestPermissions(
                activity, arrayOf(Manifest.permission.RECORD_AUDIO), REQ_MIC)
            return "permission_requested"
        }
        return startRecordingLocked()
    }

    private fun startRecordingLocked(): String {
        val activity = activity ?: return "error:Activity 未就绪"
        val target = captureFile(activity, "record", "m4a")
        return try {
            val rec = if (Build.VERSION.SDK_INT >= 31) {
                MediaRecorder(activity)
            } else {
                @Suppress("DEPRECATION") MediaRecorder()
            }
            rec.setAudioSource(MediaRecorder.AudioSource.MIC)
            rec.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
            rec.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
            rec.setAudioEncodingBitRate(96_000)
            rec.setAudioSamplingRate(44_100)
            rec.setOutputFile(target.absolutePath)
            rec.prepare()
            rec.start()
            recorder = rec
            recordFile = target
            AppConfig.trace(activity, "record-start ${target.name}")
            "ok"
        } catch (e: Exception) {
            target.delete()
            AppConfig.trace(activity, "record-start-error ${e.message}")
            "error:${e.message}"
        }
    }

    /** 停止录音，返回相对 agent 路径（uploads/...）；失败返回空串。 */
    @JvmStatic
    @Synchronized
    fun recordStop(): String {
        val activity = activity ?: return ""
        val rec = recorder ?: return ""
        val file = recordFile
        return try {
            rec.stop()
            AppConfig.trace(activity, "record-stop ${file?.name}")
            if (file != null && file.isFile && file.length() > 0) "uploads/${file.name}" else ""
        } catch (e: Exception) {
            AppConfig.trace(activity, "record-stop-error ${e.message}")
            file?.delete()
            ""
        } finally {
            try { rec.release() } catch (_: Exception) {}
            recorder = null
            recordFile = null
        }
    }

    /** 当前是否在录音（GUI 在授权轮询中用它感知原生侧自动开录）。 */
    @JvmStatic
    @Synchronized
    fun recordActive(): Boolean = recorder != null

    /** 取消录音：停止并丢弃文件。 */
    @JvmStatic
    @Synchronized
    fun recordCancel() {
        val file = recordFile
        recordStop()
        file?.delete()
    }

    @JvmStatic
    @Synchronized
    fun onRequestPermissionsResult(requestCode: Int, granted: Boolean) {
        when (requestCode) {
            REQ_MIC -> if (granted && startRecordWhenGranted) {
                startRecordWhenGranted = false
                startRecordingLocked()
            } else {
                startRecordWhenGranted = false
            }
            REQ_CAMERA -> {
                val kind = pendingCaptureKind
                val scanId = pendingScanRequestId
                pendingCaptureKind = null
                pendingScanRequestId = null
                val activity = activity ?: return
                when (kind) {
                    "photo" -> if (granted) capturePhoto()
                    "video" -> if (granted) captureVideo()
                    "scan" -> if (scanId != null) {
                        if (!granted) {
                            // 用户拒绝：立刻交付终态，别让 Rust 一直等相机。
                            finishScanDenied(activity, scanId)
                        } else if (scanRequested(activity, scanId)) {
                            // 标记还是这一轮才拉相机；取消之后标记已经没了、或者开始了
                            // 新一轮的话，这声迟到的授权作废。
                            val status = launchScanLocked(activity, scanId)
                            // 授权之后才失败的话没有调用方接住这个 error：就地清掉本轮
                            // 的照片与标记，别让轮询一直等到超时。
                            if (status != "ok" && status != "permission_requested") {
                                finishScanLaunchFailed(activity, scanId)
                            }
                        }
                    }
                }
            }
        }
    }

    // ------------------------------------------------------------------
    // 保存到系统下载目录（文档类：pdf/md/xlsx…；MediaStore.Downloads）
    // ------------------------------------------------------------------

    /** 返回值约定同 saveToGallery："ok" / "permission_requested" / "error:<原因>"。 */
    @JvmStatic
    fun saveToDownloads(absPath: String): String {
        val activity = activity ?: return "error:Activity 未就绪"
        val src = File(absPath)
        if (!src.isFile) return "error:源文件不存在"
        val mime = android.webkit.MimeTypeMap.getSingleton()
            .getMimeTypeFromExtension(src.extension.lowercase())
            ?: "application/octet-stream"
        return try {
            if (Build.VERSION.SDK_INT >= 29) {
                val values = ContentValues().apply {
                    put(MediaStore.MediaColumns.DISPLAY_NAME, src.name)
                    put(MediaStore.MediaColumns.MIME_TYPE, mime)
                    put(MediaStore.MediaColumns.RELATIVE_PATH, Environment.DIRECTORY_DOWNLOADS + "/RClaw")
                    put(MediaStore.MediaColumns.IS_PENDING, 1)
                }
                val uri = activity.contentResolver.insert(
                    MediaStore.Downloads.EXTERNAL_CONTENT_URI, values)
                    ?: return "error:MediaStore 插入失败"
                activity.contentResolver.openOutputStream(uri)?.use { out ->
                    src.inputStream().use { it.copyTo(out, 64 * 1024) }
                } ?: return "error:输出流打开失败"
                values.clear()
                values.put(MediaStore.MediaColumns.IS_PENDING, 0)
                activity.contentResolver.update(uri, values, null, null)
                "ok"
            } else {
                if (ContextCompat.checkSelfPermission(activity, Manifest.permission.WRITE_EXTERNAL_STORAGE)
                        != PackageManager.PERMISSION_GRANTED) {
                    ActivityCompat.requestPermissions(
                        activity, arrayOf(Manifest.permission.WRITE_EXTERNAL_STORAGE), REQ_WRITE)
                    return "permission_requested"
                }
                val pub = File(Environment.getExternalStoragePublicDirectory(
                    Environment.DIRECTORY_DOWNLOADS), "RClaw")
                pub.mkdirs()
                src.copyTo(File(pub, src.name), overwrite = true)
                "ok"
            }
        } catch (e: Exception) {
            AppConfig.trace(activity, "save-downloads-error ${e.message}")
            "error:${e.message}"
        }
    }

    // ------------------------------------------------------------------
    // 保存到相册（MediaStore；API<29 走公共目录 + 老权限）
    // ------------------------------------------------------------------

    /** 返回 "ok" / "permission_requested" / "error:<原因>"。kind: photo|video|audio */
    @JvmStatic
    fun saveToGallery(absPath: String, kind: String): String {
        val activity = activity ?: return "error:Activity 未就绪"
        val src = File(absPath)
        if (!src.isFile) return "error:源文件不存在"
        val (dir, mime) = when (kind) {
            "photo" -> Environment.DIRECTORY_PICTURES to "image/jpeg"
            "video" -> Environment.DIRECTORY_MOVIES to "video/mp4"
            else -> Environment.DIRECTORY_MUSIC to "audio/mp4"
        }
        return try {
            if (Build.VERSION.SDK_INT >= 29) {
                val values = ContentValues().apply {
                    put(MediaStore.MediaColumns.DISPLAY_NAME, src.name)
                    put(MediaStore.MediaColumns.MIME_TYPE, mime)
                    put(MediaStore.MediaColumns.RELATIVE_PATH, "$dir/RClaw")
                    put(MediaStore.MediaColumns.IS_PENDING, 1)
                }
                val collection = when (kind) {
                    "photo" -> MediaStore.Images.Media.EXTERNAL_CONTENT_URI
                    "video" -> MediaStore.Video.Media.EXTERNAL_CONTENT_URI
                    else -> MediaStore.Audio.Media.EXTERNAL_CONTENT_URI
                }
                val uri = activity.contentResolver.insert(collection, values)
                    ?: return "error:MediaStore 插入失败"
                activity.contentResolver.openOutputStream(uri)?.use { out ->
                    src.inputStream().use { it.copyTo(out, 64 * 1024) }
                } ?: return "error:输出流打开失败"
                values.clear()
                values.put(MediaStore.MediaColumns.IS_PENDING, 0)
                activity.contentResolver.update(uri, values, null, null)
                "ok"
            } else {
                if (ContextCompat.checkSelfPermission(activity, Manifest.permission.WRITE_EXTERNAL_STORAGE)
                        != PackageManager.PERMISSION_GRANTED) {
                    ActivityCompat.requestPermissions(
                        activity, arrayOf(Manifest.permission.WRITE_EXTERNAL_STORAGE), REQ_WRITE)
                    return "permission_requested"
                }
                val pub = File(Environment.getExternalStoragePublicDirectory(dir), "RClaw")
                pub.mkdirs()
                src.copyTo(File(pub, src.name), overwrite = true)
                "ok"
            }
        } catch (e: Exception) {
            AppConfig.trace(activity, "save-gallery-error ${e.message}")
            "error:${e.message}"
        }
    }

    // ------------------------------------------------------------------
    // 音频解码 → 16kHz 单声道 WAV（whisper 输入要求）
    // ------------------------------------------------------------------

    /** 解码任意受支持音频（m4a/aac/mp3…）为 16kHz 单声道 16-bit PCM WAV。
     *  返回 "ok" 或 "error:<原因>"（JNI 跨层异常会丢消息，用字符串回传）。 */
    @JvmStatic
    fun decodeToWav(audioPath: String, wavPath: String): String {
        val extractor = MediaExtractor()
        return try {
            extractor.setDataSource(audioPath)
            var trackIndex = -1
            var format: MediaFormat? = null
            for (i in 0 until extractor.trackCount) {
                val fmt = extractor.getTrackFormat(i)
                if (fmt.getString(MediaFormat.KEY_MIME)?.startsWith("audio/") == true) {
                    trackIndex = i
                    format = fmt
                    break
                }
            }
            val audioFormat = format ?: throw RuntimeException("文件里没有音频轨")
            extractor.selectTrack(trackIndex)
            val mime = audioFormat.getString(MediaFormat.KEY_MIME)!!
            val codec = MediaCodec.createDecoderByType(mime)
            codec.configure(audioFormat, null, null, 0)
            codec.start()

            var srcRate = audioFormat.getInteger(MediaFormat.KEY_SAMPLE_RATE)
            var srcChannels = audioFormat.getInteger(MediaFormat.KEY_CHANNEL_COUNT)
            // 分块累积解码输出：直接持有每块 ShortArray（装箱 ArrayList 会让
            // 数分钟音频堆内存膨胀一个数量级），合并后峰值约 2 倍数据量。
            val chunks = ArrayList<ShortArray>()
            var totalSamples = 0L

            val info = MediaCodec.BufferInfo()
            var sawInputEos = false
            var sawOutputEos = false
            while (!sawOutputEos) {
                if (!sawInputEos) {
                    val inIdx = codec.dequeueInputBuffer(10_000)
                    if (inIdx >= 0) {
                        val buf = codec.getInputBuffer(inIdx)!!
                        val size = extractor.readSampleData(buf, 0)
                        if (size < 0) {
                            codec.queueInputBuffer(
                                inIdx, 0, 0, 0, MediaCodec.BUFFER_FLAG_END_OF_STREAM)
                            sawInputEos = true
                        } else {
                            codec.queueInputBuffer(inIdx, 0, size, extractor.sampleTime, 0)
                            extractor.advance()
                        }
                    }
                }
                val outIdx = codec.dequeueOutputBuffer(info, 10_000)
                if (outIdx >= 0) {
                    val out = codec.getOutputBuffer(outIdx)!!
                    val shorts = ShortArray(info.size / 2)
                    out.position(info.offset)
                    out.order(java.nio.ByteOrder.LITTLE_ENDIAN).asShortBuffer().get(shorts)
                    chunks.add(shorts)
                    totalSamples += shorts.size
                    codec.releaseOutputBuffer(outIdx, false)
                    if (info.flags and MediaCodec.BUFFER_FLAG_END_OF_STREAM != 0) sawOutputEos = true
                } else if (outIdx == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED) {
                    val outFmt = codec.outputFormat
                    srcRate = outFmt.getInteger(MediaFormat.KEY_SAMPLE_RATE)
                    srcChannels = outFmt.getInteger(MediaFormat.KEY_CHANNEL_COUNT)
                }
            }
            try { codec.stop() } catch (_: Exception) {}
            try { codec.release() } catch (_: Exception) {}

            val pcm = ShortArray(totalSamples.toInt())
            var offset = 0
            for (chunk in chunks) {
                chunk.copyInto(pcm, offset)
                offset += chunk.size
            }

            // 折单声道（就地写回 pcm 前段，避免再分配一个浮点副本）+ 线性重采样到 16k。
            // 插值在 [-32768,32767] 原值域内进行，结果饱和截断防回绕。
            val monoLen = pcm.size / srcChannels
            for (i in 0 until monoLen) {
                var acc = 0
                for (c in 0 until srcChannels) acc += pcm[i * srcChannels + c]
                pcm[i] = (acc / srcChannels).toShort()
            }
            val outLen = (monoLen.toLong() * 16_000L / srcRate).toInt()
            val resampled = ShortArray(outLen)
            for (i in 0 until outLen) {
                val pos = i.toDouble() * srcRate / 16_000.0
                val i0 = pos.toInt().coerceAtMost(monoLen - 1)
                val i1 = (i0 + 1).coerceAtMost(monoLen - 1)
                val frac = (pos - i0).toFloat()
                val value = pcm[i0] * (1f - frac) + pcm[i1] * frac
                resampled[i] = value.toInt().coerceIn(-32768, 32767).toShort()
            }
            writeWav16k(File(wavPath), resampled)
            "ok"
        } catch (e: Exception) {
            "error:音频解码失败: ${e.message}"
        } finally {
            try { extractor.release() } catch (_: Exception) {}
        }
    }

    private fun writeWav16k(target: File, pcm: ShortArray) {
        val dataLen = pcm.size * 2
        FileOutputStream(target).use { out ->
            val header = ByteArray(44)
            fun put(offset: Int, s: String) =
                s.toByteArray(Charsets.US_ASCII).copyInto(header, offset)
            fun putInt(offset: Int, v: Int) {
                header[offset] = (v and 0xff).toByte()
                header[offset + 1] = ((v shr 8) and 0xff).toByte()
                header[offset + 2] = ((v shr 16) and 0xff).toByte()
                header[offset + 3] = ((v shr 24) and 0xff).toByte()
            }
            put(0, "RIFF"); putInt(4, 36 + dataLen); put(8, "WAVE")
            put(12, "fmt "); putInt(16, 16); putInt(20, 1); putInt(22, 1)
            putInt(24, 16000); putInt(28, 32000); putInt(32, 2); putInt(34, 16)
            put(36, "data"); putInt(40, dataLen)
            out.write(header)
            val bytes = ByteArray(dataLen)
            for (i in pcm.indices) {
                bytes[i * 2] = (pcm[i].toInt() and 0xff).toByte()
                bytes[i * 2 + 1] = ((pcm[i].toInt() shr 8) and 0xff).toByte()
            }
            out.write(bytes)
        }
    }

    // ------------------------------------------------------------------
    // whisper 模型下载（后台线程 + 进度状态文件）
    // ------------------------------------------------------------------

    /** 依次尝试 urls 下载到 filesDir/models/<destName>（.part 临时 + 原子改名）。
     *  进度写 models/download_status.json；立即返回（线程后台跑）。 */
    @JvmStatic
    fun downloadModel(urls: Array<String>, destName: String): Boolean {
        val activity = activity ?: return false
        Thread {
            val dir = File(activity.filesDir, "models").apply { mkdirs() }
            val dest = File(dir, destName)
            val status = File(dir, "download_status.json")
            fun writeStatus(state: String, received: Long, total: Long, error: String? = null) {
                val errField = error?.let { ",\"error\":\"${it.replace("\"", "'")}\"" } ?: ""
                try {
                    status.writeText(
                        "{\"state\":\"$state\",\"received\":$received,\"total\":$total$errField}")
                } catch (_: Exception) {}
            }
            // 快速路径：模型已内置在 APK assets（离线集成），直接复制免联网。
            try {
                activity.assets.open("models/$destName").use { input ->
                    writeStatus("copying", 0, 0)
                    val part = File(dest.absolutePath + ".part")
                    FileOutputStream(part).use { out ->
                        input.copyTo(out, 256 * 1024)
                    }
                    if (!part.renameTo(dest)) {
                        part.copyTo(dest, overwrite = true)
                        part.delete()
                    }
                    writeStatus("done", dest.length(), dest.length())
                    AppConfig.trace(activity, "whisper-model-from-assets ${dest.name} ${dest.length()}")
                    return@Thread
                }
            } catch (e: Exception) {
                // assets 里没有模型（未集成或密度裁剪）：落回网络下载。
                AppConfig.trace(activity, "whisper-asset-miss ${e.message}")
            }

            writeStatus("downloading", 0, 0)
            for (url in urls) {
                var conn: java.net.HttpURLConnection? = null
                try {
                    conn = (java.net.URL(url).openConnection()
                            as java.net.HttpURLConnection).apply {
                        instanceFollowRedirects = true
                        connectTimeout = 15_000
                        readTimeout = 30_000
                    }
                    val code = conn.responseCode
                    if (code != 200) continue
                    val total = conn.contentLengthLong
                    val part = File(dest.absolutePath + ".part")
                    var received = 0L
                    var lastWrite = 0L
                    conn.inputStream.use { input ->
                        FileOutputStream(part).use { out ->
                            val buf = ByteArray(128 * 1024)
                            while (true) {
                                val n = input.read(buf)
                                if (n < 0) break
                                out.write(buf, 0, n)
                                received += n
                                if (received - lastWrite > 1_000_000) {
                                    lastWrite = received
                                    writeStatus("downloading", received, total)
                                }
                            }
                        }
                    }
                    if (received > 0) {
                        if (!part.renameTo(dest)) {
                            part.copyTo(dest, overwrite = true)
                            part.delete()
                        }
                        writeStatus("done", received, if (total > 0) total else received)
                        AppConfig.trace(activity, "whisper-model-done ${dest.name} $received bytes")
                        return@Thread
                    }
                } catch (e: Exception) {
                    AppConfig.trace(activity, "whisper-model-error ${e.message}")
                } finally {
                    conn?.disconnect()
                }
            }
            writeStatus("error", 0, 0, "全部镜像下载失败")
        }.start()
        return true
    }
}
