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

    /** MainActivity 生命周期持有；系统相机/麦克风授权都需要 Activity。 */
    @JvmStatic var activity: Activity? = null

    private var pendingPhotoFile: File? = null
    private var recorder: MediaRecorder? = null
    private var recordFile: File? = null
    private var startRecordWhenGranted = false
    /** 相机权限授权后自动续跑的采集类型（photo|video），null = 无待续跑。 */
    private var pendingCaptureKind: String? = null

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

    /** 起系统相机录视频；结果经 data URI 流拷贝（EXTRA_OUTPUT 在部分 OEM 相机上不可靠）。
     *  返回约定同 capturePhoto。 */
    @JvmStatic
    @Synchronized
    fun captureVideo(): String {
        val activity = activity ?: return "error:Activity 未就绪"
        if (ContextCompat.checkSelfPermission(activity, Manifest.permission.CAMERA)
                != PackageManager.PERMISSION_GRANTED) {
            pendingCaptureKind = "video"
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
        }
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
        // 临时文件 + rename 原子提交：Rust 轮询消费端读到半截 JSON 会把文件
        // 删掉（读后即删），非原子写会整份丢失采集结果。
        try {
            val target = File(activity.filesDir, "capture_result.json")
            val tmp = File(activity.filesDir, "capture_result.json.tmp")
            tmp.writeText(json)
            if (!tmp.renameTo(target)) {
                target.writeText(json)
                tmp.delete()
            }
        } catch (e: Exception) {
            AppConfig.trace(activity, "capture-result-write-error ${e.message}")
            return
        }
        AppConfig.trace(activity, "capture-done kind=$kind file=${file?.name ?: "cancelled"}")
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
            REQ_CAMERA -> if (granted) {
                val kind = pendingCaptureKind
                pendingCaptureKind = null
                when (kind) {
                    "photo" -> capturePhoto()
                    "video" -> captureVideo()
                }
            } else {
                pendingCaptureKind = null
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
