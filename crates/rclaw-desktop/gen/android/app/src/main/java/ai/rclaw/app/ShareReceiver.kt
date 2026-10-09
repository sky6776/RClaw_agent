package ai.rclaw.app

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.provider.OpenableColumns
import java.io.File
import java.io.FileOutputStream
import org.json.JSONArray
import org.json.JSONObject

/** 系统分享接收（ACTION_SEND / SEND_MULTIPLE）：把分享的文件流拷入 agent 工作区
 *  的 `uploads/`（与 GUI 上传同一目录约定），并写 `files/pending_share.json`
 *  待 GUI 消费——消费方读到后开一个新会话并把附件引用预填进输入框。
 *  单文件与多文件、纯文本（无附件）均支持；单次分享整体写一份 pending。
 *  拷贝可能上百 MB：文件落盘全部在后台线程执行，UI 线程只做 intent 解析。 */
object ShareReceiver {

    private const val MAX_FILE_BYTES = 2L * 1024 * 1024 * 1024 // 2GB，拷贝中累计强制

    fun handle(context: Context, intent: Intent?) {
        if (intent == null) return
        val action = intent.action ?: return
        if (action != Intent.ACTION_SEND && action != Intent.ACTION_SEND_MULTIPLE) return
        val appContext = context.applicationContext
        Thread { handleInBackground(appContext, intent, action) }.start()
    }

    // 串行化：并发两次分享会同写 pending_share.json.tmp 交错损坏，且读旧
    // 合并的窗口互相丢附件；锁让后到的分享排队（拷贝期间等待是正确行为）。
    @Synchronized
    private fun handleInBackground(context: Context, intent: Intent, action: String) {
        val streams: List<Uri> = when (action) {
            Intent.ACTION_SEND ->
                listOfNotNull(
                    if (android.os.Build.VERSION.SDK_INT >= 33)
                        intent.getParcelableExtra(Intent.EXTRA_STREAM, Uri::class.java)
                    else
                        @Suppress("DEPRECATION") intent.getParcelableExtra(Intent.EXTRA_STREAM),
                )
            Intent.ACTION_SEND_MULTIPLE ->
                if (android.os.Build.VERSION.SDK_INT >= 33)
                    intent.getParcelableArrayListExtra(Intent.EXTRA_STREAM, Uri::class.java)
                        .orEmpty()
                else
                    @Suppress("DEPRECATION")
                    intent.getParcelableArrayListExtra<Uri>(Intent.EXTRA_STREAM).orEmpty()
            else -> return
        }

        val saved = ArrayList<String>()
        val dir = File(context.getFilesDir(), "agent/uploads")
        dir.mkdirs()
        for (uri in streams) {
            try {
                val target = File(dir, "${System.currentTimeMillis()}-${sanitize(displayName(context, uri))}")
                var oversized = false
                var copied = 0L
                context.contentResolver.openInputStream(uri)?.use { input ->
                    FileOutputStream(target).use { output ->
                        val buf = ByteArray(64 * 1024)
                        while (true) {
                            val n = input.read(buf)
                            if (n < 0) break
                            copied += n
                            if (copied > MAX_FILE_BYTES) {
                                oversized = true
                                break
                            }
                            output.write(buf, 0, n)
                        }
                    }
                } ?: continue
                if (oversized || copied == 0L) {
                    target.delete()
                    if (oversized) {
                        AppConfig.trace(context, "share-file-oversized ${target.name}")
                    }
                    continue
                }
                saved.add("uploads/" + target.name)
            } catch (e: Exception) {
                // 单个文件失败（授权回收/超大/不支持）跳过，不影响其余；留痕便于真机诊断。
                AppConfig.trace(context, "share-file-error ${e.javaClass.simpleName}: ${e.message}")
            }
        }
        val text = intent.getStringExtra(Intent.EXTRA_TEXT) ?: ""

        if (saved.isEmpty() && text.isBlank()) return
        writePending(context, saved, text)
        AppConfig.trace(context, "share-received files=${saved.size} hasText=${text.isNotBlank()}")
    }

    /** 原子落盘 pending_share.json（tmp + rename；消费端轮询读后即删，非原子写
     *  可能被读到半截后删除，整份分享丢失）。存在未消费的旧分享时合并而非
     *  覆盖：旧附件引用追加保留，新分享无文本时沿用旧文本。 */
    private fun writePending(context: Context, files: List<String>, text: String) {
        val target = File(context.filesDir, "pending_share.json")
        val mergedFiles = ArrayList<String>()
        var mergedText = text
        try {
            val old = JSONObject(target.readText())
            val oldFiles = old.optJSONArray("files")
            if (oldFiles != null) {
                for (i in 0 until oldFiles.length()) {
                    val name = oldFiles.optString(i)
                    if (name.isNotBlank()) mergedFiles.add(name)
                }
            }
            if (mergedText.isBlank()) mergedText = old.optString("text", "")
        } catch (_: Exception) {
            // 旧 pending 缺失/损坏：按全新分享处理。
        }
        mergedFiles.addAll(files)
        val payload = JSONObject().apply {
            put("files", JSONArray(mergedFiles))
            if (mergedText.isNotBlank()) put("text", mergedText)
        }
        try {
            val tmp = File(context.filesDir, "pending_share.json.tmp")
            tmp.writeText(payload.toString())
            if (!tmp.renameTo(target)) {
                target.writeText(payload.toString())
                tmp.delete()
            }
        } catch (e: Exception) {
            AppConfig.trace(context, "share-pending-write-error ${e.message}")
        }
    }

    private fun displayName(context: Context, uri: Uri): String {
        if ("content" == uri.scheme) {
            try {
                context.contentResolver.query(
                    uri,
                    arrayOf(OpenableColumns.DISPLAY_NAME),
                    null, null, null,
                )?.use { cursor ->
                    if (cursor.moveToFirst()) {
                        val name = cursor.getString(0)
                        if (!name.isNullOrBlank()) return name
                    }
                }
            } catch (ignored: Exception) {
            }
        }
        return uri.lastPathSegment ?: "file"
    }

    private fun sanitize(raw: String): String {
        val base = raw.substringAfterLast('/').substringAfterLast('\\')
        val cleaned = base.map { c ->
            val code = c.code
            // ASCII 可见区内的非法字符换下划线；非 ASCII（中文等）原样保留。
            if (code < 128 && !(code in 48..57 || code in 65..90 || code in 97..122 || c in "._- ()")) {
                '_'
            } else {
                c
            }
        }.joinToString("").trim()
        return if (cleaned.isEmpty()) "file" else cleaned
    }
}
